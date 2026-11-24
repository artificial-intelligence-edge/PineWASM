//! Differential tests for the `csil-validate` stack-height preflight
//! (`stack_heights` + `preflight_stack_heights`) and the TA-input kernels
//! (`ta_target_bounds`, `resolve_ta_int`) against expected-result copies of
//! the validator code, including contextual stack-effect resolution in the
//! preflight copy.

use super::{
    height_error, instruction_offset, preflight_stack_heights, stack_heights, HeightErr,
    HeightErrKind, ValidationError, MAX_DECLARED_STACK, MAX_VALIDATION_ABSTRACT_CELLS,
};
use crate::{
    CsilFunction, CsilInstr, CsilProgram, InputValue, StackEffect, StackEffectError, TaFunc,
    MAX_WINDOW_LENGTH,
};
use pinewasm_hir::{TaInputBinding, TaInputTarget, TaParamRule};
use std::sync::atomic::{AtomicUsize, Ordering};

/// The stack effect as the expected-result copy of the validator resolves it.
type Resolve<'a> = &'a dyn Fn(&CsilInstr) -> Result<StackEffect, StackEffectError>;

/// `CsilInstr::resolve_stack_effect` with the TA state table replaced by the
/// arities `ta_arity` (`CALL_TA` is the only instruction that consults it).
fn arity_effect(
    ta_arity: &[(u32, u32)],
    instr: &CsilInstr,
) -> Result<StackEffect, StackEffectError> {
    match instr {
        CsilInstr::CallTa { state } => match ta_arity.get(*state as usize) {
            Some(&(pops, pushes)) => Ok(StackEffect { pops, pushes }),
            None => Err(StackEffectError::InvalidTaState {
                state: *state,
                state_count: ta_arity.len(),
            }),
        },
        _ => instr.resolve_stack_effect(&[]),
    }
}

/// An expected-result copy of the preflight block of `validate_function`. It
/// differs from the production block only in that errors are returned as
/// `Err(e)` instead of being pushed, the stack-effect resolution is the
/// `resolve` parameter, and the heights are returned.
fn baseline_heights(
    code: &[CsilInstr],
    name: &str,
    resolve: Resolve<'_>,
) -> Result<Vec<Option<u32>>, ValidationError> {
    let code_len = code.len();
    let mut heights = vec![None; code_len];
    let mut height_worklist = vec![0usize];
    heights[0] = Some(0u32);
    let mut abstract_cells = 0usize;
    while let Some(pc) = height_worklist.pop() {
        let height = heights[pc].expect("height worklist entries always have state");
        let instr = &code[pc];
        let effect = match resolve(instr) {
            Ok(effect) => effect,
            Err(error) => {
                return Err(ValidationError::AtInstruction {
                    function: name.to_string(),
                    offset: instruction_offset(pc),
                    message: format!("cannot resolve stack effect: {error}"),
                });
            }
        };
        let (pops, pushes) = (effect.pops, effect.pushes);
        let Some(after_pops) = height.checked_sub(pops) else {
            return Err(ValidationError::AtInstruction {
                function: name.to_string(),
                offset: instruction_offset(pc),
                message: format!("stack underflow: needs {pops} operand(s), stack has {height}"),
            });
        };
        let Some(next_height) = after_pops.checked_add(pushes) else {
            return Err(ValidationError::AtInstruction {
                function: name.to_string(),
                offset: instruction_offset(pc),
                message: "operand stack height overflows u32".to_string(),
            });
        };
        if next_height > MAX_DECLARED_STACK {
            return Err(ValidationError::AtInstruction {
                function: name.to_string(),
                offset: instruction_offset(pc),
                message: format!("actual operand stack exceeds the limit of {MAX_DECLARED_STACK}"),
            });
        }

        if matches!(instr, CsilInstr::Return) {
            if next_height != 0 {
                return Err(ValidationError::AtInstruction {
                    function: name.to_string(),
                    offset: instruction_offset(pc),
                    message: format!(
                        "operand stack must be empty at RETURN (has {next_height} values)"
                    ),
                });
            }
            continue;
        }

        let mut successors = [None, None];
        match instr {
            CsilInstr::Jump { target } => successors[0] = Some(*target as usize),
            CsilInstr::JumpIfFalse { target }
            | CsilInstr::JumpIfExact { target, .. }
            | CsilInstr::JumpIfSlotWritten { target, .. } => {
                successors = [Some(*target as usize), Some(pc + 1)];
            }
            _ => successors[0] = Some(pc + 1),
        }
        for successor in successors.into_iter().flatten() {
            let Some(successor_height) = heights.get_mut(successor) else {
                return Err(ValidationError::AtInstruction {
                    function: name.to_string(),
                    offset: instruction_offset(pc),
                    message: "execution falls off the end of the function".to_string(),
                });
            };
            match *successor_height {
                None => {
                    abstract_cells = match abstract_cells.checked_add(next_height as usize) {
                        Some(total) if total <= MAX_VALIDATION_ABSTRACT_CELLS => total,
                        _ => {
                            return Err(ValidationError::Structure {
                                function: name.to_string(),
                                message: format!(
                                    "abstract stack analysis exceeds the {MAX_VALIDATION_ABSTRACT_CELLS}-cell complexity limit"
                                ),
                            });
                        }
                    };
                    *successor_height = Some(next_height);
                    height_worklist.push(successor);
                }
                Some(existing) if existing != next_height => {
                    return Err(ValidationError::AtInstruction {
                        function: name.to_string(),
                        offset: instruction_offset(successor),
                        message: format!(
                            "inconsistent stack heights at merge point ({existing} vs {next_height})"
                        ),
                    });
                }
                Some(_) => {}
            }
        }
    }
    Ok(heights)
}

/// Number of validator runs whose preflight was checked against the
/// expected-result copy by [`assert_preflight_matches_baseline`].
static HOOKED_PREFLIGHTS: AtomicUsize = AtomicUsize::new(0);

/// Called by `preflight_stack_heights` in test builds, so every validator run
/// in this crate's tests (all test programs, valid and rejected) is also a
/// differential check against the expected-result copy, messages included.
pub(super) fn assert_preflight_matches_baseline(
    program: &CsilProgram,
    func: &CsilFunction,
    name: &str,
    new: &Result<(), ValidationError>,
) {
    let legacy = baseline_heights(&func.code, name, &|instr| {
        instr.resolve_stack_effect(&program.ta_states)
    })
    .map(|_| ());
    assert_eq!(
        *new, legacy,
        "preflight diverged from the expected-result copy for `{name}`"
    );
    HOOKED_PREFLIGHTS.fetch_add(1, Ordering::Relaxed);
}

/// The new kernel plus the new message rendering, heights included.
fn new_heights(
    code: &[CsilInstr],
    name: &str,
    ta_arity: &[(u32, u32)],
) -> (
    Result<Vec<Option<u32>>, ValidationError>,
    Option<HeightErrKind>,
) {
    match stack_heights(code, ta_arity) {
        Ok(heights) => (Ok(heights), None),
        Err(error) => (
            Err(height_error(name, error, ta_arity.len())),
            Some(error.kind),
        ),
    }
}

fn program_arity(program: &CsilProgram) -> Vec<(u32, u32)> {
    (0..program.ta_states.len())
        .map(|state| {
            let effect = CsilInstr::CallTa {
                state: u32::try_from(state).unwrap(),
            }
            .resolve_stack_effect(&program.ta_states)
            .expect("declared TA state resolves");
            (effect.pops, effect.pushes)
        })
        .collect()
}

/// Differential check on one code vector; returns the new rejection kind.
fn check(code: &[CsilInstr], ta_arity: &[(u32, u32)]) -> Option<HeightErrKind> {
    let (new, kind) = new_heights(code, "f", ta_arity);
    let legacy = baseline_heights(code, "f", &|instr| arity_effect(ta_arity, instr));
    assert_eq!(new, legacy, "diverged on {code:?} with arity {ta_arity:?}");
    kind
}

#[derive(Default)]
struct KindsSeen {
    ok: usize,
    underflow: usize,
    overflow: usize,
    exceeds: usize,
    non_empty_return: usize,
    falls_off: usize,
    complexity: usize,
    merge: usize,
    unresolved: usize,
}

impl KindsSeen {
    fn record(&mut self, kind: Option<HeightErrKind>) {
        match kind {
            None => self.ok += 1,
            Some(HeightErrKind::Underflow { .. }) => self.underflow += 1,
            Some(HeightErrKind::Overflow) => self.overflow += 1,
            Some(HeightErrKind::ExceedsLimit) => self.exceeds += 1,
            Some(HeightErrKind::NonEmptyReturn { .. }) => self.non_empty_return += 1,
            Some(HeightErrKind::FallsOffEnd) => self.falls_off += 1,
            Some(HeightErrKind::ComplexityLimit) => self.complexity += 1,
            Some(HeightErrKind::MergeMismatch { .. }) => self.merge += 1,
            Some(HeightErrKind::InvalidTaState { .. } | HeightErrKind::CountExceedsU32) => {
                self.unresolved += 1;
            }
        }
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n).unwrap()).unwrap()
    }
}

fn corpus_programs() -> Vec<(String, CsilProgram)> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut paths = Vec::new();
    for dir in [
        "tests/compile-pass",
        "tests/regression",
        "benchmarks/workloads",
    ] {
        for entry in std::fs::read_dir(root.join(dir)).expect("corpus directory") {
            let path = entry.expect("corpus entry").path();
            if path.extension().is_some_and(|ext| ext == "pine") {
                paths.push(path);
            }
        }
    }
    paths.sort();
    let mut programs = Vec::new();
    for path in paths {
        let source = std::fs::read_to_string(&path).expect("corpus source");
        let parsed = pinewasm_parse::parse(&source);
        if pinewasm_span::has_errors(&parsed.diagnostics) {
            continue;
        }
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        if pinewasm_span::has_errors(&analysis.diagnostics) {
            continue;
        }
        let mir = pinewasm_mir::lower(&analysis.hir);
        if let Ok(program) = crate::lower_mir(&mir) {
            programs.push((path.display().to_string(), program));
        }
    }
    programs
}

fn is_jump(instr: &CsilInstr) -> bool {
    matches!(
        instr,
        CsilInstr::Jump { .. }
            | CsilInstr::JumpIfFalse { .. }
            | CsilInstr::JumpIfExact { .. }
            | CsilInstr::JumpIfSlotWritten { .. }
    )
}

fn retarget(instr: &CsilInstr, new_target: u32) -> CsilInstr {
    match *instr {
        CsilInstr::Jump { .. } => CsilInstr::Jump { target: new_target },
        CsilInstr::JumpIfFalse { .. } => CsilInstr::JumpIfFalse { target: new_target },
        CsilInstr::JumpIfExact { on, .. } => CsilInstr::JumpIfExact {
            on,
            target: new_target,
        },
        CsilInstr::JumpIfSlotWritten { slot, .. } => CsilInstr::JumpIfSlotWritten {
            slot,
            target: new_target,
        },
        ref other => other.clone(),
    }
}

/// Every Pine test program validates through the hooked preflight, and every
/// function is also checked under truncation, deletion, replacement and
/// retargeting mutations that drive each rejection path.
#[test]
fn corpus_preflight_matches_legacy_including_mutations() {
    let programs = corpus_programs();
    assert!(
        programs.len() >= 60,
        "only {} Pine test programs",
        programs.len()
    );
    let before = HOOKED_PREFLIGHTS.load(Ordering::Relaxed);
    let mut seen = KindsSeen::default();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for (path, program) in &programs {
        crate::validate(program).unwrap_or_else(|e| panic!("{path}: {e:?}"));
        let arity = program_arity(program);
        for func in &program.functions {
            let code = &func.code;
            let len = code.len();
            // Unmodified code, through the kernel and through the wrapper.
            seen.record(check(code, &arity));
            assert_eq!(
                preflight_stack_heights(program, func, &func.name),
                baseline_heights(code, &func.name, &|instr| {
                    instr.resolve_stack_effect(&program.ta_states)
                })
                .map(|_| ()),
            );
            let step = len.div_ceil(64).max(1);
            for k in (1..len).step_by(step) {
                seen.record(check(&code[..k], &arity));
                let mut deleted = code.clone();
                deleted.remove(k);
                seen.record(check(&deleted, &arity));
                for replacement in [
                    CsilInstr::Return,
                    CsilInstr::Pop,
                    CsilInstr::PushNa,
                    CsilInstr::Dup,
                ] {
                    let mut replaced = code.clone();
                    replaced[k] = replacement;
                    seen.record(check(&replaced, &arity));
                }
                let mut jumped = code.clone();
                jumped[k] = CsilInstr::Jump {
                    target: u32::try_from(rng.below(len + 1)).unwrap(),
                };
                seen.record(check(&jumped, &arity));
                if is_jump(&code[k]) {
                    let mut moved = code.clone();
                    moved[k] = retarget(&code[k], u32::try_from(rng.below(len + 1)).unwrap());
                    seen.record(check(&moved, &arity));
                }
            }
        }
    }
    assert!(HOOKED_PREFLIGHTS.load(Ordering::Relaxed) > before);
    assert!(seen.ok > 0, "no accepted cases");
    assert!(seen.underflow > 0, "no underflow cases");
    assert!(seen.non_empty_return > 0, "no non-empty RETURN cases");
    assert!(seen.falls_off > 0, "no fall-off cases");
    assert!(seen.merge > 0, "no merge-mismatch cases");
}

/// Random code over the instructions used by the Pine test programs, plus
/// random jumps and `CALL_TA`s with real and extreme arities.
#[test]
fn random_code_preflight_matches_baseline() {
    let programs = corpus_programs();
    let mut palette: Vec<CsilInstr> = Vec::new();
    for (_, program) in &programs {
        for func in &program.functions {
            for instr in &func.code {
                if !is_jump(instr)
                    && !matches!(instr, CsilInstr::CallTa { .. })
                    && !palette.contains(instr)
                {
                    palette.push(instr.clone());
                }
            }
        }
    }
    assert!(palette.len() > 20);
    let mut arity: Vec<(u32, u32)> = TaFunc::ALL
        .iter()
        .map(|func| {
            (
                u32::try_from(func.engine_operands().len()).unwrap(),
                func.engine_result_arity(),
            )
        })
        .collect();
    let real = arity.len();
    arity.extend([
        (0, u32::MAX),
        (0, MAX_DECLARED_STACK + 1),
        (0, MAX_DECLARED_STACK),
        (u32::MAX, 0),
        (0, 0),
        (5, 7),
    ]);
    let mut seen = KindsSeen::default();
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    for case in 0..40_000 {
        let len = 1 + rng.below(if case % 10 == 0 { 200 } else { 24 });
        let mut code = Vec::with_capacity(len);
        for _ in 0..len {
            let roll = rng.below(100);
            let instr = if roll < 10 {
                retarget(
                    &[
                        CsilInstr::Jump { target: 0 },
                        CsilInstr::JumpIfFalse { target: 0 },
                        CsilInstr::JumpIfExact {
                            on: true,
                            target: 0,
                        },
                        CsilInstr::JumpIfSlotWritten { slot: 0, target: 0 },
                    ][rng.below(4)],
                    u32::try_from(rng.below(len + 1)).unwrap(),
                )
            } else if roll < 16 {
                let state = if rng.below(4) == 0 {
                    rng.below(arity.len())
                } else {
                    rng.below(real)
                };
                CsilInstr::CallTa {
                    state: u32::try_from(state).unwrap(),
                }
            } else if roll < 22 {
                CsilInstr::Return
            } else {
                palette[rng.below(palette.len())].clone()
            };
            code.push(instr);
        }
        seen.record(check(&code, &arity));
    }
    assert!(seen.ok > 0 && seen.underflow > 0 && seen.merge > 0);
    assert!(seen.overflow > 0 && seen.exceeds > 0);
    assert!(seen.non_empty_return > 0 && seen.falls_off > 0);
}

/// One hand-built program per rejection kind; offsets and messages match
/// the expected-result copy exactly.
#[test]
fn preflight_rejection_kinds_match_baseline() {
    let push = CsilInstr::PushI64(1);
    type Case = (Vec<CsilInstr>, Vec<(u32, u32)>, Option<HeightErr>);
    let cases: Vec<Case> = vec![
        (
            vec![push.clone(), CsilInstr::Pop, CsilInstr::Return],
            vec![],
            None,
        ),
        (
            vec![CsilInstr::Pop, CsilInstr::Return],
            vec![],
            Some(HeightErr {
                pc: 0,
                kind: HeightErrKind::Underflow { pops: 1, height: 0 },
            }),
        ),
        (
            vec![push.clone(), CsilInstr::CallTa { state: 0 }],
            vec![(0, u32::MAX)],
            Some(HeightErr {
                pc: 1,
                kind: HeightErrKind::Overflow,
            }),
        ),
        (
            vec![CsilInstr::CallTa { state: 0 }],
            vec![(0, MAX_DECLARED_STACK + 1)],
            Some(HeightErr {
                pc: 0,
                kind: HeightErrKind::ExceedsLimit,
            }),
        ),
        (
            vec![push.clone(), CsilInstr::Return],
            vec![],
            Some(HeightErr {
                pc: 1,
                kind: HeightErrKind::NonEmptyReturn { height: 1 },
            }),
        ),
        (
            vec![push.clone()],
            vec![],
            Some(HeightErr {
                pc: 0,
                kind: HeightErrKind::FallsOffEnd,
            }),
        ),
        (
            vec![
                CsilInstr::PushBool(true),
                CsilInstr::JumpIfFalse { target: 3 },
                push.clone(),
                CsilInstr::Return,
            ],
            vec![],
            // The fall-through is visited first (LIFO), so the merge at the
            // jump target is reported before its non-empty RETURN.
            Some(HeightErr {
                pc: 3,
                kind: HeightErrKind::MergeMismatch {
                    existing: 0,
                    incoming: 1,
                },
            }),
        ),
        (
            vec![
                CsilInstr::PushBool(true),
                CsilInstr::JumpIfFalse { target: 4 },
                push.clone(),
                CsilInstr::Jump { target: 4 },
                CsilInstr::Return,
            ],
            vec![],
            Some(HeightErr {
                pc: 4,
                kind: HeightErrKind::MergeMismatch {
                    existing: 0,
                    incoming: 1,
                },
            }),
        ),
    ];
    for (code, arity, expected) in cases {
        assert_eq!(stack_heights(&code, &arity).err(), expected, "{code:?}");
        check(&code, &arity);
    }
    // The cell budget is inclusive: 125 first visits at height 64000 sum to
    // exactly the limit and pass; one more visit rejects.
    let arity = [(0, 64_000), (64_000, 64_000), (64_000, 0)];
    for (repeats, expected) in [(124, None), (125, Some(HeightErrKind::ComplexityLimit))] {
        let mut code = vec![CsilInstr::CallTa { state: 0 }];
        code.extend(std::iter::repeat_n(CsilInstr::CallTa { state: 1 }, repeats));
        code.extend([CsilInstr::CallTa { state: 2 }, CsilInstr::Return]);
        assert_eq!(check(&code, &arity), expected, "{repeats} repeats");
    }
    // A long push triangle trips the cell budget before any height limit.
    let triangle = vec![push; 5_000];
    let error = stack_heights(&triangle, &[]).unwrap_err();
    assert_eq!(error.kind, HeightErrKind::ComplexityLimit);
    assert_eq!(check(&triangle, &[]), Some(HeightErrKind::ComplexityLimit));
    assert_eq!(
        height_error("f", error, 0),
        ValidationError::Structure {
            function: "f".to_string(),
            message: format!(
                "abstract stack analysis exceeds the {MAX_VALIDATION_ABSTRACT_CELLS}-cell complexity limit"
            ),
        }
    );
    assert_eq!(
        height_error(
            "g",
            HeightErr {
                pc: 7,
                kind: HeightErrKind::FallsOffEnd
            },
            0
        ),
        ValidationError::AtInstruction {
            function: "g".to_string(),
            offset: instruction_offset(7),
            message: "execution falls off the end of the function".to_string(),
        }
    );
}

/// Expected-result copy of `CsilProgram::ta_input_bounds`.
fn baseline_ta_input_bounds(program: &CsilProgram, binding: &TaInputBinding) -> Option<(i64, i64)> {
    match binding.target {
        TaInputTarget::Sma { .. } | TaInputTarget::Change { .. } => {
            Some((1, i64::from(MAX_WINDOW_LENGTH)))
        }
        TaInputTarget::State { state, param } => {
            match program
                .ta_states
                .get(state as usize)?
                .func
                .engine_param_rules()
                .get(param as usize)?
            {
                TaParamRule::Length { min } => Some((*min, i64::from(MAX_WINDOW_LENGTH))),
                TaParamRule::IntRange { min, max } => Some((*min, *max)),
                _ => None,
            }
        }
    }
}

/// Expected-result copy of input resolution, including Pine's implicit
/// integer-to-float conversion for float TA parameters.
fn baseline_resolve_ta_input(
    program: &CsilProgram,
    binding: &TaInputBinding,
    value: Option<&InputValue>,
) -> Option<i64> {
    let default = &program.inputs.get(binding.input as usize)?.defval;
    match program.ta_input_rule(binding) {
        Some(TaParamRule::FloatBits { positive }) => {
            let value = match (default, value) {
                (InputValue::Int(_), Some(InputValue::Int(value))) => *value as f64,
                (InputValue::Float(_), Some(InputValue::Float(value))) => *value,
                (InputValue::Int(value), _) => *value as f64,
                (InputValue::Float(value), _) => *value,
                _ => return None,
            };
            let value = if binding.negated { -value } else { value };
            return (value.is_finite() && (!positive || value > 0.0))
                .then_some(i64::from_ne_bytes(value.to_bits().to_ne_bytes()));
        }
        Some(TaParamRule::Percent) => {
            let value = match (default, value) {
                (InputValue::Int(_), Some(InputValue::Int(value))) => *value as f64,
                (InputValue::Float(_), Some(InputValue::Float(value))) => *value,
                (InputValue::Int(value), _) => *value as f64,
                (InputValue::Float(value), _) => *value,
                _ => return None,
            };
            let value = if binding.negated { -value } else { value };
            return (value.is_finite() && (0.0..=100.0).contains(&value))
                .then_some(i64::from_ne_bytes(value.to_bits().to_ne_bytes()));
        }
        _ => {}
    }
    let InputValue::Int(value) = value
        .filter(|v| matches!(v, InputValue::Int(_)))
        .unwrap_or(default)
    else {
        return None;
    };
    let value = if binding.negated {
        value.checked_neg()?
    } else {
        *value
    };
    let (min, max) = baseline_ta_input_bounds(program, binding)?;
    (min..=max).contains(&value).then_some(value)
}

/// Integer samples around every bound the TA tables and window limit use.
fn ta_int_samples() -> Vec<i64> {
    let mut samples = vec![
        i64::MIN,
        i64::MIN + 1,
        -1,
        0,
        1,
        2,
        14,
        i64::from(MAX_WINDOW_LENGTH),
        i64::MAX - 1,
        i64::MAX,
    ];
    for func in TaFunc::ALL {
        for rule in func.engine_param_rules() {
            let (lo, hi) = match *rule {
                TaParamRule::Length { min } => (min, i64::from(MAX_WINDOW_LENGTH)),
                TaParamRule::IntRange { min, max } => (min, max),
                _ => continue,
            };
            for bound in [lo, hi] {
                for delta in [-1, 0, 1] {
                    samples.push(bound.saturating_add(delta));
                }
            }
        }
    }
    let negated: Vec<i64> = samples.iter().filter_map(|v| v.checked_neg()).collect();
    samples.extend(negated);
    samples.sort_unstable();
    samples.dedup();
    samples
}

/// Every TA function, target shape, negation, input index, default and
/// override kind: both public methods agree with the expected-result copy.
#[test]
fn ta_input_resolution_matches_baseline() {
    let (_, mut program) = corpus_programs()
        .into_iter()
        .find(|(_, p)| !p.ta_states.is_empty() && !p.inputs.is_empty())
        .expect("a Pine test program with TA state and an input");
    let samples = ta_int_samples();
    let n_states = u32::try_from(program.ta_states.len()).unwrap();
    let mut targets = vec![
        TaInputTarget::Sma { slot: 0 },
        TaInputTarget::Change { slot: 3 },
        TaInputTarget::State {
            state: n_states,
            param: 0,
        },
        TaInputTarget::State {
            state: u32::MAX,
            param: 0,
        },
    ];
    for param in 0..8 {
        targets.push(TaInputTarget::State { state: 0, param });
    }
    targets.push(TaInputTarget::State {
        state: 0,
        param: u32::MAX,
    });
    let overrides: Vec<Option<InputValue>> = std::iter::once(None)
        .chain(samples.iter().map(|v| Some(InputValue::Int(*v))))
        .chain([Some(InputValue::Float(3.0)), Some(InputValue::Bool(true))])
        .collect();
    let defaults: Vec<InputValue> = [0, 1, 14, -14, i64::MIN, i64::from(MAX_WINDOW_LENGTH) + 1]
        .into_iter()
        .map(InputValue::Int)
        .chain([InputValue::Float(2.0)])
        .collect();
    let mut accepted = 0usize;
    let mut rejected = 0usize;
    for func in TaFunc::ALL {
        program.ta_states[0].func = func;
        for target in &targets {
            for negated in [false, true] {
                for input in [0, u32::try_from(program.inputs.len()).unwrap()] {
                    let binding = TaInputBinding {
                        input,
                        negated,
                        target: *target,
                    };
                    assert_eq!(
                        program.ta_input_bounds(&binding),
                        baseline_ta_input_bounds(&program, &binding),
                        "{func:?} {binding:?}"
                    );
                    for default in &defaults {
                        program.inputs[0].defval = default.clone();
                        for value in &overrides {
                            let new = program.resolve_ta_input(&binding, value.as_ref());
                            let old = baseline_resolve_ta_input(&program, &binding, value.as_ref());
                            assert_eq!(new, old, "{func:?} {binding:?} {default:?} {value:?}");
                            if new.is_some() {
                                accepted += 1;
                            } else {
                                rejected += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(accepted > 1_000 && rejected > 1_000);
}

/// The rule kernel on every rule shape, and the resolution kernel on the
/// negation-overflow and missing-bounds rejections.
#[test]
fn ta_integer_kernels_cover_every_rule_shape() {
    use crate::ta_inputs::{resolve_ta_int, ta_param_rule_bounds, ta_target_bounds};
    let max = i64::from(MAX_WINDOW_LENGTH);
    assert_eq!(
        ta_param_rule_bounds(&TaParamRule::Length { min: 2 }),
        Some((2, max))
    );
    assert_eq!(
        ta_param_rule_bounds(&TaParamRule::IntRange { min: -3, max: 9 }),
        Some((-3, 9))
    );
    assert_eq!(
        ta_param_rule_bounds(&TaParamRule::FloatBits { positive: true }),
        None
    );
    assert_eq!(ta_param_rule_bounds(&TaParamRule::Percent), None);
    assert_eq!(ta_param_rule_bounds(&TaParamRule::Bool01), None);
    let rules = [TaParamRule::Length { min: 1 }];
    assert_eq!(
        ta_target_bounds(TaInputTarget::Sma { slot: 9 }, None),
        Some((1, max))
    );
    assert_eq!(
        ta_target_bounds(TaInputTarget::State { state: 0, param: 0 }, Some(&rules)),
        Some((1, max))
    );
    assert_eq!(
        ta_target_bounds(TaInputTarget::State { state: 0, param: 1 }, Some(&rules)),
        None
    );
    assert_eq!(
        ta_target_bounds(TaInputTarget::State { state: 0, param: 0 }, None),
        None
    );
    assert_eq!(
        resolve_ta_int(i64::MIN, true, Some((i64::MIN, i64::MAX))),
        None
    );
    assert_eq!(resolve_ta_int(5, false, None), None);
    assert_eq!(resolve_ta_int(-5, true, Some((1, 5))), Some(5));
    assert_eq!(resolve_ta_int(6, false, Some((1, 5))), None);
    assert_eq!(resolve_ta_int(0, false, Some((1, 5))), None);
}

/// A reachable `CALL_TA` whose state is outside the table is rejected with
/// the resolver's message, like the contextual resolver in the expected-result copy.
#[test]
fn unresolved_call_ta_state_matches_baseline() {
    let code = vec![CsilInstr::CallTa { state: 5 }, CsilInstr::Return];
    assert_eq!(
        check(&code, &[(1, 1), (0, 1)]),
        Some(HeightErrKind::InvalidTaState { state: 5 })
    );
    // Unreachable code is not resolved.
    let skipped = vec![
        CsilInstr::Jump { target: 2 },
        CsilInstr::CallTa { state: 5 },
        CsilInstr::Return,
    ];
    assert_eq!(check(&skipped, &[]), None);
}
