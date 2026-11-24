//! Kept Pine `int / int` quotients the typed pass cannot lower, carried as
//! float code in the standalone module (`src/fraction.rs`).
//!
//! `pine_int_div_kept.rs` covers the shapes the typed pass lowers on its own.
//! Every other kept quotient (integer arithmetic on it, a stored quotient
//! read by a comparison or a history offset, an arm of `?:`, a loop bound)
//! is refused by the typed pass as `UnsupportedOpcode("PineIntDiv")`, which
//! refuses the whole script. The standalone compile retries such a program
//! with the quotient and everything it reaches rewritten into float opcodes.
//! Each test here runs the interpreter and the module and compares the plots
//! bit for bit.
//!
//! The quotient shapes are compared over eight input values.

use pinewasm_aot::{
    audit_standalone_wasm, compile_standalone_wasm_program,
    compile_standalone_wasm_program_with_config, standalone_eligibility, AotError,
    StandaloneRefusalSite, StandaloneWasmBuildConfig, StandaloneWasmModuleType,
    StandaloneWasmOptions, StandaloneWasmSignalSemantics,
};
use pinewasm_compiler::{compile, CompileOptions};
use pinewasm_csil::{CsilInstr, CsilProgram};
use pinewasm_mir::Value;
use pinewasm_vm::{run, RunOptions, RunOutcome};

mod support;
use support::{standalone_wasm_wire, synthetic_data, StandaloneWasmHost};

#[path = "support/kept_quotients.rs"]
mod kept_quotients;
use kept_quotients::{
    KEPT_INT_DIVISION_BODIES, KEPT_INT_DIVISION_BODIES_ON_MAIN, LARGE_INTS, LARGE_INT_BODIES,
};

fn csil(source: &str) -> CsilProgram {
    let result = compile(source, CompileOptions::all());
    let errors: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| pinewasm_span::Diagnostic::is_error(d))
        .collect();
    assert!(errors.is_empty(), "{source}\n{errors:?}");
    let program = result.program.expect("the front end produced a program");
    pinewasm_csil::validate(&program).expect("front-end output validates");
    program
}

const INDICATOR: StandaloneWasmOptions = StandaloneWasmOptions {
    signal_plot: None,
    // An indicator module serializes no signal, so its plot lane may hold
    // any value.
    module_type: StandaloneWasmModuleType::Indicator,
};

fn plot_bits(outcome: &RunOutcome, plot: usize) -> Vec<u64> {
    outcome.plots[plot]
        .values
        .iter()
        .map(|value| match value {
            Value::Float(x) => x.to_bits(),
            Value::Int(x) => (*x as f64).to_bits(),
            Value::Na => f64::NAN.to_bits(),
            other => panic!("plot holds {other:?}"),
        })
        .collect()
}

fn kept_source(n: i64, body: &str) -> String {
    format!("//@version=6\nindicator(\"kept quotient\")\nn = input.int({n}, \"N\")\n{body}")
}

/// Two of the shapes are refused on this line for a reason that has nothing
/// to do with the quotient: the same script without the division is refused
/// the same way. They are kept in the list so the day either limit goes, the
/// shape is compared like the rest.
const REFUSED_WITHOUT_THE_QUOTIENT_TOO: &[(&str, &str, &str)] = &[
    // Integer arithmetic in unreachable code has no site for the run-time
    // integer-minimum guard.
    (
        "unreachable quotient",
        "UnsupportedIntegerNaEncoding",
        "z = 0\nfor i = 0 to 3\n    if i > 1\n        break\n    else\n        continue\n    z := z + n\nplot(z)\n",
    ),
    // A loop step that is not a literal checks itself with `runtime.error`,
    // which the importless tier does not run.
    (
        "float step",
        "StandaloneWasmNotStandalone",
        "s = 0\nfor i = 0 to 10 by n + 4\n    s += 1\nplot(s)\n",
    ),
];

/// Pine `int / int` keeps its fraction in a statically-int value. The
/// standalone module must agree with the interpreter bit for bit, na
/// included, for whole and uneven quotients alike, whether the typed pass
/// lowers the shape itself or the fraction carry rewrites it.
#[test]
fn kept_integer_division_matches_interpreter_in_the_standalone_module() {
    assert_eq!(KEPT_INT_DIVISION_BODIES.len(), 47);
    let data = synthetic_data(24);
    let wire = standalone_wasm_wire(&data, None);
    let mut fractional = 0;
    let mut failed = 0;
    let mut compared = 0;
    for n in [7_i64, 8, 9, 1, 0, -7, -8, 6] {
        for (name, body) in KEPT_INT_DIVISION_BODIES
            .iter()
            .chain(KEPT_INT_DIVISION_BODIES_ON_MAIN)
        {
            let context = format!("{name}, n = {n}");
            let program = csil(&kept_source(n, body));
            let reference = run(&program, &data, &RunOptions::default());
            let known = REFUSED_WITHOUT_THE_QUOTIENT_TOO
                .iter()
                .find(|(refused, ..)| refused == name);
            let wasm = match (compile_standalone_wasm_program(&program, &INDICATOR), known) {
                (Ok(wasm), None) => wasm,
                (Err(error), Some((_, kind, twin))) => {
                    assert_eq!(error.kind(), *kind, "{context}: {error}");
                    let twin =
                        compile_standalone_wasm_program(&csil(&kept_source(n, twin)), &INDICATOR)
                            .expect_err("the script without the division is refused too");
                    assert_eq!(twin.kind(), *kind, "{context}: {twin}");
                    continue;
                }
                (Ok(_), Some(_)) => panic!("{context}: accepted now; compare it"),
                (Err(error), None) => panic!("{context}: standalone emit refused: {error}"),
            };
            audit_standalone_wasm(&wasm).unwrap_or_else(|e| panic!("{context}: audit: {e}"));
            let mut host = StandaloneWasmHost::new(&wasm);
            let evaluated = host.evaluate(&wire);
            if let Some(error) = reference.errors.first() {
                // A zero or wrong-way loop step: both engines must stop.
                assert!(
                    evaluated.is_err(),
                    "{context}: interpreter failed ({}) but the module succeeded",
                    error.message
                );
                failed += 1;
                continue;
            }
            evaluated.unwrap_or_else(|e| panic!("{context}: evaluate failed: {e}"));
            let lane: Vec<u64> = host.plot_lane(0).iter().map(|x| x.to_bits()).collect();
            let expected = plot_bits(&reference, 0);
            assert_eq!(lane.len(), expected.len(), "{context}: lane length");
            for (bar, (aot, interp)) in lane.iter().zip(&expected).enumerate() {
                let (aot, interp) = (f64::from_bits(*aot), f64::from_bits(*interp));
                fractional += usize::from(interp.is_finite() && interp.fract() != 0.0);
                assert!(
                    aot.to_bits() == interp.to_bits(),
                    "{context}: bar {bar}: module {aot:?} vs interpreter {interp:?}"
                );
            }
            compared += 1;
        }
    }
    // Compares every accepted kept-division shape, including the added ones, at 8 input values.
    assert_eq!(compared + failed, (45 + 13) * 8);
    // The comparison is only meaningful if uneven quotients reached the plot.
    assert!(
        fractional > 400,
        "only {fractional} fractional samples compared"
    );
    assert!(failed <= 4, "{failed} scripts failed in the interpreter");
    eprintln!(
        "{compared} scripts compared, {fractional} fractional samples, {failed} failed in both"
    );
}

/// A designated strategy-type module compiles the signal slice first, so the
/// carry runs on sliced code and inherits its free instructions.
#[test]
fn a_carried_quotient_feeds_the_designated_signal_of_a_sliced_module() {
    let data = synthetic_data(24);
    let wire = standalone_wasm_wire(&data, None);
    for n in [7_i64, 8, 3] {
        for body in [
            "half = n / 2\nplot(close, \"display\")\nplot(bar_index % 4 > half - 2 ? 1 : -1, \"signal\")\n",
            "var int acc = 0\nacc := acc + bar_index / n\nbgcolor(acc > 2 ? color.red : na)\nplot(acc % 2 == 0 ? 1 : acc % 3 == 0 ? 0 : -1, \"signal\")\n",
        ] {
            let context = format!("designated signal, n = {n}: {body}");
            let program = csil(&kept_source(n, body));
            let signal = program.plots.len() - 1;
            let reference = run(&program, &data, &RunOptions::default());
            assert!(reference.errors.is_empty(), "{context}");
            for signal_slice in [true, false] {
                let wasm = compile_standalone_wasm_program_with_config(
                    &program,
                    &StandaloneWasmOptions {
                        signal_plot: Some(signal),
                        module_type: StandaloneWasmModuleType::Strategy,
                    },
                    &StandaloneWasmBuildConfig {
                        signal_slice,
                        signal_semantics: StandaloneWasmSignalSemantics::PostureEvents,
                        ..StandaloneWasmBuildConfig::default()
                    },
                )
                .unwrap_or_else(|e| panic!("{context}: standalone emit refused: {e}"));
                let mut host = StandaloneWasmHost::new(&wasm);
                host.evaluate(&wire)
                    .unwrap_or_else(|e| panic!("{context}: evaluate failed: {e}"));
                let lane = host.plot_lane(signal);
                for (bar, (aot, interp)) in
                    lane.iter().zip(&reference.plots[signal].values).enumerate()
                {
                    assert_eq!(Value::Float(*aot), *interp, "{context}: bar {bar}");
                }
            }
        }
    }
}

/// The fraction carry holds whole ints in f64. Past 2^53 that is exact only
/// for some values, so the module checks each widening and each operation
/// on two whole operands: it either agrees with the interpreter bit for bit
/// or stops the run with its own error. It never returns a different value.
#[test]
fn large_ints_meeting_a_kept_quotient_stay_exact_or_stop_the_module() {
    const STOP: &str = "Indicator failure: a whole number too large to hold exactly";
    let data = synthetic_data(4);
    let wire = standalone_wasm_wire(&data, None);
    // `true` when the module ran (and agreed), `false` when it stopped.
    let outcome = |big: i64, body: &str| {
        let source = format!(
            "//@version=6\nindicator(\"range\")\nn = input.int(7, \"N\")\nbig = {big} + bar_index * 1024\n{body}"
        );
        let context = format!("big = {big}: {body}");
        let program = csil(&source);
        let reference = run(&program, &data, &RunOptions::default());
        assert!(
            reference.errors.is_empty(),
            "{context}: {:?}",
            reference.errors
        );
        let wasm = compile_standalone_wasm_program(&program, &INDICATOR)
            .unwrap_or_else(|e| panic!("{context}: {e}"));
        let mut host = StandaloneWasmHost::new(&wasm);
        match host.evaluate(&wire) {
            Ok(_) => {
                let lane: Vec<u64> = host.plot_lane(0).iter().map(|x| x.to_bits()).collect();
                let expected = plot_bits(&reference, 0);
                assert_eq!(lane.len(), expected.len(), "{context}");
                for (bar, (aot, interp)) in lane.iter().zip(&expected).enumerate() {
                    let (aot, interp) = (f64::from_bits(*aot), f64::from_bits(*interp));
                    assert!(
                        aot.to_bits() == interp.to_bits(),
                        "{context}: bar {bar}: module {aot:?} vs interpreter {interp:?}"
                    );
                }
                true
            }
            Err(error) => {
                assert!(error.starts_with(STOP), "{context}: {error}");
                false
            }
        }
    };

    // An even quotient of a large int that f64 holds exactly runs, and
    // agrees: 7 * 3000000000000000 / 4.
    assert!(outcome(21_000_000_000_000_000, "x = big / 4\nplot(x)\n"));
    assert!(outcome(9_007_199_254_740_000, "k = big / 2 + 1\nplot(k)\n"));
    // An odd int past 2^53 has no exact f64 image.
    assert!(!outcome(
        9_007_199_254_740_993,
        "k = big / 2 + 1\nplot(k)\n"
    ));
    // `i64::MAX` rounds up to 2^63 in f64, and the saturating conversion
    // brings 2^63 back to `i64::MAX`, so a plain round trip would call it
    // exact. The interpreter's `i64::MAX % 1000` is 807; 2^63 % 1000 is 808.
    assert!(!outcome(
        i64::MAX,
        "x = n / 2 > 3 ? big : n / 2\nplot(x % 1000)\n"
    ));

    // A quotient that is exactly `i64::MAX` or its neighbour, and a product
    // that is exactly `i64::MIN`.
    assert!(!outcome(i64::MAX, "x = big / 1\nplot(x % 1000)\n"));
    assert!(!outcome(i64::MAX - 1, "x = big / 1\nplot(x % 1000)\n"));
    assert!(!outcome(
        -4_611_686_018_427_387_904,
        "k = big / 1\nplot(-(k * 2))\n"
    ));

    let mut ran = 0;
    let mut stopped = 0;
    for &big in LARGE_INTS {
        for body in LARGE_INT_BODIES {
            if outcome(big, body) {
                ran += 1;
            } else {
                stopped += 1;
            }
        }
    }
    // Most of these run; the guard is narrow.
    assert!(ran > stopped, "ran {ran}, stopped {stopped}");
    eprintln!("{ran} ran and agreed, {stopped} stopped");
}

/// The shape of a midpoint of two
/// int settings, `(a - b) / 2 + b`, an int sum of a kept quotient that float
/// code then reads. The typed pass refuses it (the sum is integer arithmetic
/// on a quotient), and the signal slice does not help, because the midpoint
/// reaches the signal. A second quotient is used as an integer afterwards
/// (compared, counted, used as a history offset).
#[test]
fn a_midpoint_of_two_int_settings_builds_as_a_standalone_signal_module() {
    let source = "//@version=6\nindicator(\"midpoint\")\n\
        slow = input.int(21, \"Slow Length\", minval = 2)\n\
        fast = input.int(6, \"Fast Length\", minval = 1)\n\
        mid = (slow - fast) / 2 + fast\n\
        half = slow / 2\n\
        src = hlc3\n\
        osc = ta.ema(src, 5) - ta.ema(src, 21)\n\
        scaled = osc * mid / slow\n\
        var int streak = 0\n\
        streak := scaled > scaled[half] ? streak + 1 : 0\n\
        ready = bar_index >= half * 2\n\
        signal = ready and streak > half - 8 ? 1 : ready and streak == 0 ? -1 : 0\n\
        plot(scaled, \"Oscillator\")\n\
        bgcolor(signal > 0 ? color.new(color.green, 90) : na)\n\
        plot(signal, \"Signal\")\n";
    let program = csil(source);
    // As written, the typed pass refuses it.
    assert_eq!(
        pinewasm_aot::analysis::analyze(&program).err(),
        Some(AotError::UnsupportedOpcode("PineIntDiv".to_string()))
    );
    let opts = StandaloneWasmOptions {
        signal_plot: Some(1),
        module_type: StandaloneWasmModuleType::Strategy,
    };
    let config = StandaloneWasmBuildConfig {
        signal_slice: true,
        signal_semantics: StandaloneWasmSignalSemantics::PostureEvents,
        ..StandaloneWasmBuildConfig::default()
    };
    standalone_eligibility(&program, &opts, &config).expect("eligible");
    let wasm = compile_standalone_wasm_program_with_config(&program, &opts, &config)
        .expect("builds as a standalone module");
    audit_standalone_wasm(&wasm).expect("audit");

    let data = synthetic_data(120);
    let reference = run(&program, &data, &RunOptions::default());
    assert!(reference.errors.is_empty(), "{:?}", reference.errors);
    let mut host = StandaloneWasmHost::new(&wasm);
    host.evaluate(&standalone_wasm_wire(&data, None))
        .expect("evaluate");
    let lane: Vec<u64> = host.plot_lane(1).iter().map(|x| x.to_bits()).collect();
    assert_eq!(lane, plot_bits(&reference, 1));
    // The signal is not constant, so the comparison says something.
    let distinct: std::collections::BTreeSet<u64> = lane.iter().copied().collect();
    assert!(distinct.len() >= 2, "{distinct:?}");

    // The whole program as an indicator module: both plots, every bar.
    let wasm = compile_standalone_wasm_program(&program, &INDICATOR).expect("indicator module");
    let mut host = StandaloneWasmHost::new(&wasm);
    host.evaluate(&standalone_wasm_wire(&data, None))
        .expect("evaluate");
    for plot in 0..2 {
        let lane: Vec<u64> = host.plot_lane(plot).iter().map(|x| x.to_bits()).collect();
        let expected = plot_bits(&reference, plot);
        for (bar, (aot, interp)) in lane.iter().zip(&expected).enumerate() {
            let (aot, interp) = (f64::from_bits(*aot), f64::from_bits(*interp));
            assert!(
                aot.to_bits() == interp.to_bits(),
                "plot {plot} bar {bar}: module {aot:?} vs interpreter {interp:?}"
            );
        }
    }
}

/// What the carry does not take on. `ta.valuewhen` on an int source returns
/// it through an integer lane, which would drop the fraction the interpreter
/// keeps, so the program keeps the refusal it was written with.
#[test]
fn a_carried_value_returned_by_an_int_ta_engine_is_still_refused_by_name() {
    let program = csil(&kept_source(
        7,
        "k = n / 2 + 1\nint v = ta.valuewhen(close > open, k, 0)\nplot(v)\n",
    ));
    let reference = run(&program, &synthetic_data(8), &RunOptions::default());
    assert!(reference.errors.is_empty(), "{:?}", reference.errors);
    assert!(reference.plots[0].values.contains(&Value::Float(4.5)));
    let error = compile_standalone_wasm_program(&program, &INDICATOR).expect_err("refused");
    assert_eq!(error, AotError::UnsupportedOpcode("PineIntDiv".to_string()));
}

/// When the carried program is refused for another reason, that reason is
/// reported, at the instruction of the program as written.
#[test]
fn a_refusal_behind_the_carry_names_the_instruction_as_written() {
    let program = csil(&kept_source(
        7,
        "k = n / 2 + 1\nlabel.new(bar_index, close, str.tostring(close))\nplot(k)\n",
    ));
    let refusal =
        standalone_eligibility(&program, &INDICATOR, &StandaloneWasmBuildConfig::default())
            .expect_err("a label keeps the script on the host");
    assert_eq!(refusal.kind(), "StandaloneWasmNotStandalone", "{refusal}");
    let Some(StandaloneRefusalSite::HostSite { pc }) = refusal.site() else {
        panic!("no host site: {refusal:?}");
    };
    let code = &program.main().code;
    assert!(pc < code.len(), "pc {pc} is outside the written program");
    assert!(
        !matches!(
            code[pc],
            CsilInstr::IntToF64 | CsilInstr::PushF64(_) | CsilInstr::F64Add
        ),
        "pc {pc} is {:?}",
        code[pc]
    );
}

/// Fixtures with a kept quotient, as indicator modules. Every plot
/// of each must match the interpreter on every bar. The first needs the
/// carry; in the other two every quotient is read at once by `int()`, which
/// the typed pass lowers as written.
#[test]
fn the_fixtures_the_carry_newly_accepts_match_the_interpreter() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests");
    let data = synthetic_data(40);
    let wire = standalone_wasm_wire(&data, None);
    for (fixture, carried) in [
        ("compile-pass/arithmetic_precedence.pine", true),
        ("compile-pass/int_arithmetic.pine", false),
        ("regression/migration-parity-runtime-audit-v6.pine", false),
    ] {
        let source = std::fs::read_to_string(root.join(fixture)).expect(fixture);
        let program = csil(&source);
        let typed = if carried {
            Err(AotError::UnsupportedOpcode("PineIntDiv".to_string()))
        } else {
            Ok(())
        };
        assert_eq!(
            pinewasm_aot::analysis::analyze(&program).map(drop),
            typed,
            "{fixture}: the typed pass on the program as written"
        );
        let reference = run(&program, &data, &RunOptions::default());
        assert!(
            reference.errors.is_empty(),
            "{fixture}: {:?}",
            reference.errors
        );
        let wasm = compile_standalone_wasm_program(&program, &INDICATOR)
            .unwrap_or_else(|e| panic!("{fixture}: refused: {e}"));
        let mut host = StandaloneWasmHost::new(&wasm);
        host.evaluate(&wire)
            .unwrap_or_else(|e| panic!("{fixture}: evaluate failed: {e}"));
        assert!(!reference.plots.is_empty(), "{fixture}");
        for plot in 0..reference.plots.len() {
            let lane: Vec<u64> = host.plot_lane(plot).iter().map(|x| x.to_bits()).collect();
            let expected = plot_bits(&reference, plot);
            for (bar, (aot, interp)) in lane.iter().zip(&expected).enumerate() {
                let (aot, interp) = (f64::from_bits(*aot), f64::from_bits(*interp));
                assert!(
                    aot.to_bits() == interp.to_bits(),
                    "{fixture}: plot {plot} bar {bar}: module {aot:?} vs interpreter {interp:?}"
                );
            }
        }
    }
}

/// `na` is one bit pattern in the module's f64 lanes: the canonical NaN. A
/// float negation flips the sign bit of a NaN, which the interpreter's `na`
/// does not have, so a carried negation of `na` must come out canonical
/// wherever it can be seen: a plot lane, a slot read back through history,
/// and a value forwarded through `?:`. `big` overflows to `na` from bar 1.
#[test]
fn a_carried_negation_of_na_is_the_canonical_na() {
    let data = synthetic_data(4);
    let wire = standalone_wasm_wire(&data, None);
    let big = "big = 9223372036854774784 + bar_index * 1024\n";
    for body in [
        "k = -(big / 2)\nplot(k)\n",
        "k = -(big / 2)\nplot(k[1])\n",
        "k = -(big / 2)\nj = bar_index > 100 ? 1 : k\nplot(j)\n",
        "var int acc = 0\nacc := -(big / 2)\nplot(acc)\n",
        "k = -(big / 2)\nplot(na(k) ? 1 : 0)\n",
        "k = -(big / 2)\nplot(k == k ? 1 : k < 0 ? 2 : 3)\n",
    ] {
        let source = format!("//@version=6\nindicator(\"neg na\")\n{big}{body}");
        let program = csil(&source);
        let reference = run(&program, &data, &RunOptions::default());
        assert!(
            reference.errors.is_empty(),
            "{body}: {:?}",
            reference.errors
        );
        let wasm = compile_standalone_wasm_program(&program, &INDICATOR)
            .unwrap_or_else(|e| panic!("{body}: {e}"));
        let mut host = StandaloneWasmHost::new(&wasm);
        host.evaluate(&wire)
            .unwrap_or_else(|e| panic!("{body}: {e}"));
        let lane: Vec<u64> = host.plot_lane(0).iter().map(|x| x.to_bits()).collect();
        // Exact bits: `plot_bits` writes `na` as the canonical NaN.
        assert_eq!(
            lane.iter().map(|b| format!("{b:016x}")).collect::<Vec<_>>(),
            plot_bits(&reference, 0)
                .iter()
                .map(|b| format!("{b:016x}"))
                .collect::<Vec<_>>(),
            "{body}"
        );
    }
}

/// Every carried operation with an `na` operand on the left and on the
/// right. `x` is `na` on every third bar (a zero divisor) and fractional or
/// whole on the others; `y` is never `na`. The result must be the canonical
/// NaN wherever it is `na`: in the plot lane, through history, through a
/// `ta.*` window, and through the forwarding calls. A float subtraction
/// emitted as `a + (-b)` once left a NaN with its sign bit set here.
#[test]
fn every_carried_operation_keeps_an_na_operand_canonical() {
    let data = synthetic_data(12);
    let wire = standalone_wasm_wire(&data, None);
    let header = "//@version=6\nindicator(\"na operands\")\nn = input.int(7, \"N\")\n\
                  x = n / (bar_index % 3)\ny = n / 2\n";
    let mut na_samples = 0;
    let mut bodies: Vec<String> = Vec::new();
    for op in ["+", "-", "*", "/", "%"] {
        for (left, right) in [
            ("x", "1"),
            ("1", "x"),
            ("0", "x"),
            ("x", "y"),
            ("y", "x"),
            ("x", "x[1]"),
            ("x[1]", "x"),
            ("x", "x"),
            ("x", "bar_index"),
            ("bar_index", "x"),
        ] {
            bodies.push(format!("plot({left} {op} {right})\n"));
        }
    }
    for body in [
        "plot(-x)\n",
        "plot(-(1 - x))\n",
        "k = 1 - x\nplot(k[1])\n",
        "k = 1 - x\nplot(k - k[2])\n",
        "plot(ta.sma(1 - x, 3))\n",
        "plot(ta.sma(0 - x, 2))\n",
        "plot(math.max(1 - x, 2))\n",
        "plot(math.min(x - 1, 2))\n",
        "plot(math.abs(1 - x))\n",
        "plot(math.sign(1 - x))\n",
        "plot(nz(1 - x, 5) - x)\n",
        "plot(bar_index % 2 == 0 ? 1 - x : x - 1)\n",
        "var int acc = 0\nacc := 1 - x\nplot(acc)\n",
        "plot(int(1 - x))\n",
        "plot(math.round(1 - x))\n",
        "plot(close[nz(1 - x, 0) > 0 ? 1 : 0] - (1 - x))\n",
    ] {
        bodies.push(body.to_string());
    }
    for body in &bodies {
        let source = format!("{header}{body}");
        let program = csil(&source);
        let reference = run(&program, &data, &RunOptions::default());
        assert!(
            reference.errors.is_empty(),
            "{body}: {:?}",
            reference.errors
        );
        let wasm = compile_standalone_wasm_program(&program, &INDICATOR)
            .unwrap_or_else(|e| panic!("{body}: {e}"));
        let mut host = StandaloneWasmHost::new(&wasm);
        host.evaluate(&wire)
            .unwrap_or_else(|e| panic!("{body}: {e}"));
        let lane: Vec<String> = host
            .plot_lane(0)
            .iter()
            .map(|x| format!("{:016x}", x.to_bits()))
            .collect();
        let expected: Vec<String> = plot_bits(&reference, 0)
            .iter()
            .map(|b| format!("{b:016x}"))
            .collect();
        assert_eq!(lane, expected, "{body}");
        na_samples += expected.iter().filter(|b| *b == "7ff8000000000000").count();
    }
    assert!(na_samples > 200, "only {na_samples} na samples compared");
    eprintln!("{} scripts, {na_samples} na samples", bodies.len());
}

/// A carried quotient read by `float(x)` and by `log.info`. Whatever class
/// `pinewasm_csil::fraction::classify` gives the two consumers, the module
/// either agrees with the interpreter on every plot or the program is
/// refused for a declared reason. Prints which, so a change of class shows.
#[test]
fn a_carried_quotient_read_by_a_float_cast_or_a_log_call_is_correct_or_refused() {
    let data = synthetic_data(12);
    let wire = standalone_wasm_wire(&data, None);
    for n in [7_i64, 8, -7, 0] {
        for (name, body) in [
            ("float cast", "k = n / 2 + 1\nplot(float(k))\n"),
            (
                "float cast in arithmetic",
                "k = n / 2 + 1\nplot(float(k) * 2 + k)\n",
            ),
            (
                "float cast of a quotient by a series",
                "k = n / (bar_index % 3) - 1\nplot(float(k))\n",
            ),
            ("log.info", "k = n / 2 + 1\nlog.info(\"{0}\", k)\nplot(k)\n"),
            (
                "log.info of a sum",
                "k = n / 2\nlog.info(\"{0} {1}\", k + 1, k)\nplot(k + 1)\n",
            ),
        ] {
            let context = format!("{name}, n = {n}");
            let program = csil(&kept_source(n, body));
            let reference = run(&program, &data, &RunOptions::default());
            assert!(reference.errors.is_empty(), "{context}");
            let wasm = match compile_standalone_wasm_program(&program, &INDICATOR) {
                Ok(wasm) => wasm,
                Err(error) => {
                    assert_eq!(
                        error.class(),
                        pinewasm_aot::RefusalClass::Capability,
                        "{context}: {error}"
                    );
                    // Only the log call may be refused, and as the host site
                    // it is, not for the quotient.
                    assert!(name.starts_with("log.info"), "{context}: {error}");
                    assert_eq!(error.kind(), "StandaloneWasmNotStandalone", "{context}");
                    eprintln!("{context}: refused ({})", error.kind());
                    continue;
                }
            };
            let mut host = StandaloneWasmHost::new(&wasm);
            host.evaluate(&wire)
                .unwrap_or_else(|e| panic!("{context}: {e}"));
            let lane: Vec<u64> = host.plot_lane(0).iter().map(|x| x.to_bits()).collect();
            assert_eq!(lane, plot_bits(&reference, 0), "{context}");
            // `float(k)` keeps the fraction: 7 / 2 + 1 is 4.5, not 4.
            if name == "float cast" && n == 7 {
                assert!(
                    lane.iter().all(|bits| *bits == 4.5_f64.to_bits()),
                    "{lane:?}"
                );
            }
            assert!(
                name.starts_with("float cast"),
                "{context}: a log call is a host site in the standalone tier"
            );
            eprintln!("{context}: accepted, agrees");
        }
    }
}

/// The exactness check cannot tell a whole float that came from a fraction
/// (a float in the interpreter, which goes on rounding) from a whole int, so
/// it stops both. A known over-eager stop, never a different value: the bars
/// the module wrote before stopping agree with the interpreter.
#[test]
fn a_whole_float_from_a_fraction_stops_where_an_int_would() {
    let data = synthetic_data(4);
    let program = csil(&kept_source(
        7,
        "var int a = 0\na := bar_index == 0 ? n / 2 * 1000000 : a * a\nplot(a)\n",
    ));
    let reference = run(&program, &data, &RunOptions::default());
    assert!(reference.errors.is_empty(), "{:?}", reference.errors);
    assert_eq!(reference.plots[0].values[2], Value::Float(1.500_625e26));
    let wasm = compile_standalone_wasm_program(&program, &INDICATOR).expect("builds");
    let mut host = StandaloneWasmHost::new(&wasm);
    let error = host
        .evaluate(&standalone_wasm_wire(&data, None))
        .expect_err("stops at bar 2");
    assert!(
        error.starts_with("Indicator failure: a whole number too large to hold exactly"),
        "{error}"
    );
    let lane = host.plot_lane(0);
    assert_eq!(lane[0], 3_500_000.0);
    assert_eq!(lane[1], 1.225e13);
}
