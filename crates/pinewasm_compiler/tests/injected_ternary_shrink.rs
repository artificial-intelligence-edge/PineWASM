//! Shrinking of an explicitly injected source-level defect through the compile-and-run path.
//! This is a deterministic regression-tool test, not a production compiler bug
//! or a proof of the language semantics. The test writes no files.
use pinewasm_compiler::{compile, CompileOptions, MarketData, RunOptions, Value};
use pinewasm_csil::{validate, CsilInstr, CsilProgram};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

const POLICY: &str = "injected-ternary-v1";
const SEED: u64 = 20261006;
const BOUNDARIES: [i64; 7] = [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
struct Case {
    index: usize,
    a: i64,
    delta: i64,
    cut: i64,
}
impl Case {
    fn generate(index: usize) -> Self {
        let selector = (SEED * 257 + u64::try_from(index).expect("bounded index")) % 63;
        let k = usize::try_from(selector).expect("selector fits");
        Self {
            index,
            a: BOUNDARIES[k % 7],
            delta: [-1, 0, 1][(k / 7) % 3],
            cut: [-1, 0, 1][k / 21],
        }
    }
    fn source(self, mutant: bool) -> String {
        let literal = if self.a == i64::MIN {
            "(-9223372036854775807 - 1)".to_owned()
        } else {
            self.a.to_string()
        };
        let arms = if mutant { "x : y" } else { "y : x" };
        format!("//@version=6\nindicator(\"Injected ternary\")\nx = {literal}\ny = x + {}\nz = y > {} ? {arms}\nplot(y)\nplot(z)\n", self.delta, self.cut)
    }
    fn measure(self) -> (usize, u128) {
        (
            self.source(false).len(),
            u128::from(self.a.unsigned_abs())
                + u128::from(self.delta.unsigned_abs())
                + u128::from(self.cut.unsigned_abs()),
        )
    }
    fn feature(self) -> (u8, bool, bool) {
        let boundary = if self.a == i64::MIN {
            0
        } else if self.a == i64::MAX {
            1
        } else {
            2
        };
        let y = self.a.checked_add(self.delta);
        (boundary, y.is_some(), y.is_some_and(|v| v > self.cut))
    }
    fn expected_values(self, mutant: bool) -> [Option<u64>; 2] {
        let y = self.a.checked_add(self.delta);
        let branch = y.is_some_and(|v| v > self.cut);
        let z = if branch != mutant { y } else { Some(self.a) };
        [y.map(integer_bits), z.map(integer_bits)]
    }
}

/// Binary64 bit pattern of an i64, computed with integer arithmetic (round to nearest, ties to even).
fn integer_bits(value: i64) -> u64 {
    if value == 0 {
        return 0;
    }
    let sign = if value < 0 { 1_u64 << 63 } else { 0 };
    let n = value.unsigned_abs();
    let mut exponent = n.ilog2();
    let mut mantissa = if exponent <= 52 {
        n << (52 - exponent)
    } else {
        let shift = exponent - 52;
        let mut m = n >> shift;
        let remainder = n - (m << shift);
        let half = 1_u64 << (shift - 1);
        if remainder > half || (remainder == half && m % 2 == 1) {
            m += 1;
        }
        m
    };
    if mantissa == 1_u64 << 53 {
        mantissa >>= 1;
        exponent += 1;
    }
    sign | (u64::from(exponent + 1023) << 52) | (mantissa - (1_u64 << 52))
}
fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct RunRecord {
    source: String,
    source_sha256: String,
    artifact: String,
    artifact_sha256: String,
    // Whether the emitted code contains branch, read and store instructions, and two plots.
    branch_read_store_two_plots: bool,
    // A run creates fresh execution state.
    fresh_values: [Option<u64>; 2],
    // Same immutable prepared code, but fresh mutable state per evaluation.
    prepared_first_values: [Option<u64>; 2],
    prepared_second_values: [Option<u64>; 2],
}
fn run_source(source: String) -> Result<RunRecord, String> {
    let compiled = compile(&source, CompileOptions::default());
    if !compiled.success {
        return Err(format!("compile refusal: {:?}", compiled.diagnostics));
    }
    let program = compiled.program.ok_or("success without program")?;
    validate(&program).map_err(|e| format!("validation: {e:?}"))?;
    let artifact = program.to_json();
    let restored = CsilProgram::from_json(&artifact).map_err(|e| format!("redecode: {e}"))?;
    validate(&restored).map_err(|e| format!("revalidated: {e:?}"))?;
    let mut kinds = [false; 3];
    let mut plots = BTreeSet::new();
    for function in &restored.functions {
        for instruction in &function.code {
            match instruction {
                CsilInstr::JumpIfFalse { .. } => kinds[0] = true,
                CsilInstr::ReadUserSlot { .. } => kinds[1] = true,
                CsilInstr::StoreUserSlotCurrent { .. } => kinds[2] = true,
                CsilInstr::PlotF64 { plot_slot } | CsilInstr::PlotStyledF64 { plot_slot, .. } => {
                    plots.insert(*plot_slot);
                }
                _ => {}
            }
        }
    }
    let market = MarketData {
        open: vec![1.0],
        high: vec![1.0],
        low: vec![1.0],
        close: vec![1.0],
        volume: vec![1.0],
        ..MarketData::default()
    };
    let values = extract_outcome(pinewasm_vm::run(&restored, &market, &RunOptions::default()))?;
    let prepared = pinewasm_vm::PreparedProgram::new(restored.clone())
        .map_err(|error| format!("prepared admission: {error}"))?;
    let prepared_first_values = extract_outcome(prepared.run(&market, &RunOptions::default()))?;
    let prepared_second_values = extract_outcome(prepared.run(&market, &RunOptions::default()))?;
    if values != prepared_first_values || values != prepared_second_values {
        return Err("prepared code reuse differs from a fresh run".into());
    }
    Ok(RunRecord {
        source_sha256: sha(source.as_bytes()),
        source,
        artifact_sha256: sha(artifact.as_bytes()),
        artifact,
        branch_read_store_two_plots: kinds.into_iter().all(|x| x)
            && plots == BTreeSet::from([0, 1]),
        fresh_values: values,
        prepared_first_values,
        prepared_second_values,
    })
}

/// Shared checks for the fresh-state and prepared-code execution paths.
fn extract_outcome(outcome: pinewasm_vm::RunOutcome) -> Result<[Option<u64>; 2], String> {
    if !outcome.errors.is_empty() || outcome.bars_executed != 1 || outcome.plots.len() != 2 {
        return Err(format!("runtime prefix: {:?}", outcome.errors));
    }
    let mut values = [None; 2];
    for (index, lane) in outcome.plots.iter().enumerate() {
        if lane.slot != u32::try_from(index).expect("two lanes") || lane.values.len() != 1 {
            return Err("lane identity/length".into());
        }
        values[index] = match lane.values.first().ok_or("missing plot cell")? {
            Value::Na => None,
            Value::Float(value) => Some(value.to_bits()),
            _ => return Err("unexpected plot type".into()),
        };
    }
    Ok(values)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Attempt {
    run_id: usize,
    parent: Case,
    proposal: Case,
    reason: String,
    source: Option<RunRecord>,
    target: Option<RunRecord>,
    source_error: Option<String>,
    target_error: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Replay {
    policy: &'static str,
    schedule: &'static str,
    injected_mutation: &'static str,
    initial: Case,
    initial_source: RunRecord,
    initial_target: RunRecord,
    final_case: Case,
    accepted: Vec<Case>,
    attempts: Vec<Attempt>,
}
fn replay(initial: Case) -> Replay {
    let original = run_source(initial.source(false)).expect("initial source run");
    let mutant = run_source(initial.source(true)).expect("initial mutant run");
    assert!(original.branch_read_store_two_plots && mutant.branch_read_store_two_plots);
    assert_eq!(original.fresh_values, initial.expected_values(false));
    assert_eq!(mutant.fresh_values, initial.expected_values(true));
    assert_ne!(
        original.fresh_values, mutant.fresh_values,
        "the injected mutation must change the fresh values"
    );
    let mut current = initial;
    let mut accepted = vec![initial];
    let mut attempts = Vec::new();
    loop {
        let mut seen = BTreeSet::new();
        let mut next = None;
        for index in 0..63 {
            let proposal = Case::generate(index);
            if !seen.insert(sha(proposal.source(false).as_bytes()))
                || proposal.measure() >= current.measure()
                || proposal.feature() != current.feature()
            {
                continue;
            }
            let source = run_source(proposal.source(false));
            let target = run_source(proposal.source(true));
            let source_error = source.as_ref().err().cloned();
            let target_error = target.as_ref().err().cloned();
            let (source, target, reason) = match (source, target) {
                (Ok(source), Ok(target)) => {
                    assert_eq!(
                        source.fresh_values,
                        proposal.expected_values(false),
                        "source fresh values match the expected values"
                    );
                    assert_eq!(
                        target.fresh_values,
                        proposal.expected_values(true),
                        "mutant fresh values match the expected values"
                    );
                    let reason = if !source.branch_read_store_two_plots
                        || !target.branch_read_store_two_plots
                    {
                        "feature lost"
                    } else if source.fresh_values == target.fresh_values {
                        "equal values"
                    } else {
                        "value mismatch"
                    };
                    (Some(source), Some(target), reason.to_owned())
                }
                (source, target) => (
                    source.ok(),
                    target.ok(),
                    "compile or run refusal/failure".to_owned(),
                ),
            };
            let mismatch = reason == "value mismatch";
            attempts.push(Attempt {
                run_id: attempts.len(),
                parent: current,
                proposal,
                reason,
                source,
                target,
                source_error,
                target_error,
            });
            if mismatch {
                next = Some(proposal);
                break;
            }
        }
        if let Some(proposal) = next {
            assert!(proposal.measure() < current.measure());
            current = proposal;
            accepted.push(current);
        } else {
            break;
        }
    }
    Replay {
        policy: POLICY,
        schedule: "one bar, OHLCV=1, default RunOptions",
        injected_mutation: "source-level swapped ternary arms; not a compiler defect",
        initial,
        initial_source: original,
        initial_target: mutant,
        final_case: current,
        accepted,
        attempts,
    }
}

#[test]
fn injected_ternary_regression_is_replayed_and_minimized() {
    let initial = (0..63)
        .map(Case::generate)
        .find(|c| (c.a, c.delta, c.cut) == (-1, 1, 1))
        .expect("grid case");
    let first = replay(initial);
    let second = replay(initial);
    assert_eq!(
        first, second,
        "fresh complete replay, ordered values and minimal result"
    );
    assert!(first.accepted.len() > 1, "must actually shrink");
    assert!(first.final_case.measure() < initial.measure());
    assert_eq!(first.final_case.feature(), initial.feature());
    let final_attempts: Vec<_> = first
        .attempts
        .iter()
        .filter(|a| a.parent == first.final_case)
        .collect();
    let eligible: Vec<_> = (0..63)
        .map(Case::generate)
        .filter(|c| {
            c.measure() < first.final_case.measure() && c.feature() == first.final_case.feature()
        })
        .collect();
    assert_eq!(
        final_attempts
            .iter()
            .map(|a| a.proposal)
            .collect::<Vec<_>>(),
        eligible,
        "terminal sweep freshly evaluates every eligible grid candidate in order"
    );
    assert!(
        final_attempts.iter().all(|a| a.reason != "value mismatch"),
        "no eligible class-preserving candidate remains"
    );
    assert_eq!(first.policy, POLICY);
    println!(
        "{}",
        serde_json::to_string(&first).expect("serialize replay record")
    );
}

#[test]
fn integer_rounding_and_candidate_grid_boundaries() {
    assert_eq!(integer_bits(0), 0);
    assert_eq!(integer_bits(-1), 0xbff0000000000000);
    assert_eq!(integer_bits(i64::MIN), 0xc3e0000000000000);
    assert_eq!(integer_bits(i64::MAX), 0x43e0000000000000);
    assert_eq!(integer_bits((1_i64 << 53) + 1), 0x4340000000000000);
    assert_eq!(integer_bits((1_i64 << 53) + 3), 0x4340000000000002);
    let grid: BTreeSet<_> = (0..63)
        .map(Case::generate)
        .map(|c| (c.a, c.delta, c.cut))
        .collect();
    assert_eq!(grid.len(), 63);
}

#[test]
fn invalid_input_and_equal_case_checks() {
    assert!(run_source("//@version=6\nthis is invalid Pine\n".into()).is_err());
    let case = Case {
        index: 0,
        a: 0,
        delta: 0,
        cut: 0,
    };
    let source = run_source(case.source(false)).expect("equal-case source");
    let target = run_source(case.source(true)).expect("equal-case mutant");
    assert!(source.branch_read_store_two_plots && target.branch_read_store_two_plots);
    assert_eq!(
        source.fresh_values, target.fresh_values,
        "arm swap is not always a mismatch"
    );
    let no_branch =
        run_source("//@version=6\nindicator(\"Injected ternary\")\nplot(1)\nplot(2)\n".into())
            .expect("supported constant program");
    assert!(
        !no_branch.branch_read_store_two_plots,
        "source text alone cannot establish emitted features"
    );
}
