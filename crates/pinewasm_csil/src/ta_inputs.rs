//! Run-constant TA input bindings shared by the interpreter and AOT emitter.

use crate::{CsilProgram, InputValue, MAX_WINDOW_LENGTH};
use pinewasm_hir::{InputDecl, TaInputBinding, TaInputTarget, TaParamRule};

impl CsilProgram {
    /// The engine rule for a TA input's declaration parameter. Slot-backed
    /// lengths have no engine declaration.
    pub(crate) fn ta_input_rule(&self, binding: &TaInputBinding) -> Option<TaParamRule> {
        let TaInputTarget::State { state, param } = binding.target else {
            return None;
        };
        self.ta_states
            .get(state as usize)?
            .func
            .engine_param_rule(param)
    }

    /// The engine's legal interval for a bound integer parameter. Input widget
    /// constraints do not widen this execution bound.
    pub fn ta_input_bounds(&self, binding: &TaInputBinding) -> Option<(i64, i64)> {
        match binding.target {
            TaInputTarget::Sma { .. } | TaInputTarget::Change { .. } => {
                Some((1, MAX_WINDOW_LENGTH as i64))
            }
            TaInputTarget::State { .. } => {
                let rule = self.ta_input_rule(binding)?;
                ta_param_rule_bounds(&rule)
            }
        }
    }

    /// Resolve an override with the same wrong-kind fallback as `READ_INPUT`.
    /// An override outside the engine parameter's rule is an error, never a
    /// clamp. Float parameters are returned as their exact `f64` bits.
    pub fn resolve_ta_input(
        &self,
        binding: &TaInputBinding,
        value: Option<&InputValue>,
    ) -> Option<i64> {
        let default = ta_declared_default(&self.inputs, binding.input)?;
        match self.ta_input_rule(binding) {
            Some(TaParamRule::FloatBits { positive }) => {
                ta_resolve_float_bits(default, value, binding.negated, positive)
            }
            Some(TaParamRule::Percent) => ta_resolve_percent(default, value, binding.negated),
            _ => self.resolve_ta_integer_branch(binding, default, value),
        }
    }

    /// The actual integer arm after declaration lookup and rule dispatch.
    /// Selection rejects before bounds are evaluated; the float arms do not
    /// call this helper. Declaration lookup remains in `resolve_ta_input`.
    pub(crate) fn resolve_ta_integer_branch(
        &self,
        binding: &TaInputBinding,
        default: &InputValue,
        value: Option<&InputValue>,
    ) -> Option<i64> {
        let value = ta_selected_integer(default, value)?;
        resolve_ta_int(value, binding.negated, self.ta_input_bounds(binding))
    }

    /// Specialize one clone in a single bytecode pass. Call only after program
    /// validation and input range checks; window bindings address hidden slots.
    pub fn set_ta_inputs(&mut self, values: &std::collections::HashMap<TaInputTarget, i64>) {
        for (target, value) in values {
            if let TaInputTarget::State { state, param } = *target {
                self.ta_states[state as usize].params[param as usize] = *value;
            }
        }
        crate::history_binding_walk::specialize_functions(&mut self.functions, values);
    }
}

/// Selected numeric payload, before the caller's integer-to-float conversion.
#[derive(Clone, Copy)]
pub(crate) enum TaSelectedNumeric {
    Int(i64),
    Float(f64),
}

/// Numeric overrides are default-kind-directed. A wrong-kind override falls
/// back; a nonnumeric default rejects even when the override is numeric.
pub(crate) fn ta_selected_numeric(
    default: &InputValue,
    override_value: Option<&InputValue>,
) -> Option<TaSelectedNumeric> {
    match (default, override_value) {
        (InputValue::Int(_), Some(InputValue::Int(value))) => Some(TaSelectedNumeric::Int(*value)),
        (InputValue::Float(_), Some(InputValue::Float(value))) => {
            Some(TaSelectedNumeric::Float(*value))
        }
        (InputValue::Int(value), _) => Some(TaSelectedNumeric::Int(*value)),
        (InputValue::Float(value), _) => Some(TaSelectedNumeric::Float(*value)),
        _ => None,
    }
}

/// Native `FloatBits` primitive boundary for the complete resolver. The
/// float conversion and comparison contract is separate from caller control flow.
pub(crate) fn ta_resolve_float_bits(
    default: &InputValue,
    override_value: Option<&InputValue>,
    negated: bool,
    positive: bool,
) -> Option<i64> {
    let selected = match ta_selected_numeric(default, override_value)? {
        TaSelectedNumeric::Int(value) => value as f64,
        TaSelectedNumeric::Float(value) => value,
    };
    let value = f64::from_bits(ta_conditional_sign_bits(selected.to_bits(), negated));
    (value.is_finite() && (!positive || value > 0.0)).then_some(value.to_bits() as i64)
}

/// Native percent primitive boundary. Inclusive endpoints and signed-zero
/// acceptance keep the existing resolver behavior.
pub(crate) fn ta_resolve_percent(
    default: &InputValue,
    override_value: Option<&InputValue>,
    negated: bool,
) -> Option<i64> {
    let selected = match ta_selected_numeric(default, override_value)? {
        TaSelectedNumeric::Int(value) => value as f64,
        TaSelectedNumeric::Float(value) => value,
    };
    let value = f64::from_bits(ta_conditional_sign_bits(selected.to_bits(), negated));
    (value.is_finite() && (0.0..=100.0).contains(&value)).then_some(value.to_bits() as i64)
}

/// Total binary64 encoding operation. Float primitive agreement is a separate
/// proof obligation; nonfinite resolver results are rejected by existing guards.
pub(crate) fn ta_conditional_sign_bits(bits: u64, negated: bool) -> u64 {
    if negated {
        bits ^ (1_u64 << 63)
    } else {
        bits
    }
}

/// Borrow the default at a declaration-vector index. Declaration metadata is
/// not searched; an absent index rejects even when an override is available.
pub(crate) fn ta_declared_default(inputs: &[InputDecl], input: u32) -> Option<&InputValue> {
    match inputs.get(input as usize) {
        Some(declaration) => Some(&declaration.defval),
        None => None,
    }
}

/// Select the integer override, falling back to the borrowed declaration default.
/// Bounds are deliberately not an argument: rejected kinds must return before
/// the caller evaluates its engine bounds.
pub(crate) fn ta_selected_integer(
    default: &InputValue,
    override_value: Option<&InputValue>,
) -> Option<i64> {
    match override_value {
        Some(InputValue::Int(value)) => Some(*value),
        _ => match default {
            InputValue::Int(value) => Some(*value),
            _ => None,
        },
    }
}

/// Integer kernel of [`CsilProgram::ta_input_bounds`] (Aeneas extraction
/// root). `state_rules` is the engine parameter rule table of the target's TA
/// state; it is consulted only for [`TaInputTarget::State`], where `None`
/// means the state index is out of range.
pub(crate) fn ta_target_bounds(
    target: TaInputTarget,
    state_rules: Option<&[TaParamRule]>,
) -> Option<(i64, i64)> {
    match target {
        TaInputTarget::Sma { .. } | TaInputTarget::Change { .. } => {
            Some((1, MAX_WINDOW_LENGTH as i64))
        }
        TaInputTarget::State { param, .. } => match state_rules {
            Some(rules) => {
                if (param as usize) < rules.len() {
                    ta_param_rule_bounds(&rules[param as usize])
                } else {
                    None
                }
            }
            None => None,
        },
    }
}

/// The integer interval of one engine parameter rule; `None` for rules that
/// are not integer ranges.
pub(crate) fn ta_param_rule_bounds(rule: &TaParamRule) -> Option<(i64, i64)> {
    match rule {
        TaParamRule::Length { min } => Some((*min, MAX_WINDOW_LENGTH as i64)),
        TaParamRule::IntRange { min, max } => Some((*min, *max)),
        _ => None,
    }
}

/// Integer kernel of [`CsilProgram::resolve_ta_input`] (Aeneas extraction
/// root): negate the selected integer when the binding asks for it, then
/// accept it only inside `bounds` (the binding's
/// [`CsilProgram::ta_input_bounds`]). Overflowing negation and missing
/// bounds both reject. Explicit matches stand in for `?`, which on `Option`
/// has no Aeneas library model.
pub(crate) fn resolve_ta_int(value: i64, negated: bool, bounds: Option<(i64, i64)>) -> Option<i64> {
    let value = if negated {
        value.checked_neg()
    } else {
        Some(value)
    };
    match (value, bounds) {
        (Some(value), Some((min, max))) => {
            if min <= value && value <= max {
                Some(value)
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod declared_default_tests {
    use super::*;

    fn program() -> CsilProgram {
        let parsed = pinewasm_parse::parse(
            "//@version=6\nindicator(\"test\")\na = input.int(3)\nb = input.int(7)\nplot(ta.sma(close, a))\n",
        );
        assert!(!pinewasm_span::has_errors(&parsed.diagnostics));
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        assert!(!pinewasm_span::has_errors(&analysis.diagnostics));
        crate::lower_mir(&pinewasm_mir::lower(&analysis.hir)).expect("lowering test fixture")
    }

    #[test]
    fn owned_program_rule_matches_previous_borrowed_lookup() {
        use pinewasm_hir::{TaFunc, TaStateDecl};

        let mut program = program();
        let mut binding = program.ta_input_bindings[0];
        for function in TaFunc::ALL {
            program.ta_states = vec![TaStateDecl {
                func: function,
                params: vec![],
            }];
            for state in [0, 1, u32::MAX] {
                for param in [0, 1, 2, 3, 4, u32::MAX] {
                    for negated in [false, true] {
                        binding.input = u32::MAX;
                        binding.negated = negated;
                        binding.target = TaInputTarget::State { state, param };
                        let previous =
                            program
                                .ta_states
                                .get(state as usize)
                                .and_then(|declaration| {
                                    declaration
                                        .func
                                        .engine_param_rules()
                                        .get(param as usize)
                                        .copied()
                                });
                        assert_eq!(program.ta_input_rule(&binding), previous);
                    }
                }
            }
        }
        for target in [
            TaInputTarget::Sma { slot: u32::MAX },
            TaInputTarget::Change { slot: u32::MAX },
        ] {
            binding.target = target;
            assert_eq!(program.ta_input_rule(&binding), None);
        }
        program.ta_states.clear();
        binding.target = TaInputTarget::State { state: 0, param: 0 };
        assert_eq!(program.ta_input_rule(&binding), None);
    }

    #[test]
    fn integer_branch_matches_previous_inline_arm() {
        use pinewasm_hir::{BuiltinSeries, TaFunc, TaStateDecl};
        let values = [
            InputValue::Int(i64::MIN),
            InputValue::Int(-100_000),
            InputValue::Int(-1),
            InputValue::Int(0),
            InputValue::Int(1),
            InputValue::Int(2),
            InputValue::Int(100_000),
            InputValue::Int(100_001),
            InputValue::Int(i64::MAX),
            InputValue::Float(-0.0),
            InputValue::Bool(false),
            InputValue::String("λ".into()),
            InputValue::Color {
                r: 1,
                g: 2,
                b: 3,
                t: 4.0,
            },
            InputValue::Source(BuiltinSeries::Close),
            InputValue::ExternalSource(pinewasm_hir::ExternalSourceRef {
                script_id: "other_script".into(),
                plot_slot: 17,
                plot_label: "λ".into(),
            }),
            InputValue::Enum {
                name: "Mode".into(),
                index: 1,
                member: "fast".into(),
                title: None,
            },
        ];
        let mut program = program();
        let mut binding = program.ta_input_bindings[0];
        for function in TaFunc::ALL {
            program.ta_states = vec![TaStateDecl {
                func: function,
                params: vec![],
            }];
            let targets = [
                TaInputTarget::Sma { slot: u32::MAX },
                TaInputTarget::Change { slot: u32::MAX },
                TaInputTarget::State { state: 0, param: 0 },
                TaInputTarget::State { state: 0, param: 1 },
                TaInputTarget::State { state: 0, param: 2 },
                TaInputTarget::State { state: 0, param: 3 },
                TaInputTarget::State {
                    state: 0,
                    param: u32::MAX,
                },
                TaInputTarget::State {
                    state: u32::MAX,
                    param: 0,
                },
            ];
            for target in targets {
                for negated in [false, true] {
                    binding.target = target;
                    binding.negated = negated;
                    for default in &values {
                        binding.input = 0;
                        program.inputs[0].defval = default.clone();
                        for candidate in std::iter::once(None).chain(values.iter().map(Some)) {
                            let previous =
                                ta_selected_integer(default, candidate).and_then(|value| {
                                    resolve_ta_int(
                                        value,
                                        binding.negated,
                                        program.ta_input_bounds(&binding),
                                    )
                                });
                            assert_eq!(
                                program.resolve_ta_integer_branch(&binding, default, candidate),
                                previous
                            );
                            if !matches!(
                                program.ta_input_rule(&binding),
                                Some(TaParamRule::FloatBits { .. } | TaParamRule::Percent)
                            ) {
                                assert_eq!(program.resolve_ta_input(&binding, candidate), previous);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn owned_program_bounds_match_previous_borrowed_lookup() {
        use pinewasm_hir::{TaFunc, TaStateDecl};

        fn previous(program: &CsilProgram, binding: &TaInputBinding) -> Option<(i64, i64)> {
            let rules = match binding.target {
                TaInputTarget::State { state, .. } => Some(
                    program
                        .ta_states
                        .get(state as usize)?
                        .func
                        .engine_param_rules(),
                ),
                TaInputTarget::Sma { .. } | TaInputTarget::Change { .. } => None,
            };
            ta_target_bounds(binding.target, rules)
        }

        let mut program = program();
        let mut binding = program.ta_input_bindings[0];
        for function in TaFunc::ALL {
            program.ta_states = vec![TaStateDecl {
                func: function,
                params: vec![],
            }];
            for state in [0, 1, u32::MAX] {
                for param in [0, 1, 2, 3, 4, u32::MAX] {
                    for negated in [false, true] {
                        binding.input = u32::MAX;
                        binding.negated = negated;
                        binding.target = TaInputTarget::State { state, param };
                        assert_eq!(
                            program.ta_input_bounds(&binding),
                            previous(&program, &binding)
                        );
                    }
                }
            }
        }
        program.ta_states.clear();
        for target in [
            TaInputTarget::Sma { slot: u32::MAX },
            TaInputTarget::Change { slot: u32::MAX },
            TaInputTarget::State { state: 0, param: 0 },
        ] {
            binding.target = target;
            assert_eq!(
                program.ta_input_bounds(&binding),
                previous(&program, &binding)
            );
        }
    }

    #[test]
    fn declared_default_distinct_indices_and_borrowed_payloads() {
        let mut program = program();
        assert_eq!(program.inputs.len(), 2);
        assert!(matches!(
            ta_declared_default(&program.inputs, 0),
            Some(InputValue::Int(3))
        ));
        assert!(matches!(
            ta_declared_default(&program.inputs, 1),
            Some(InputValue::Int(7))
        ));
        // The operation indexes the vector; it does not search declaration.id.
        program.inputs[0].id = 17;
        for payload in [
            InputValue::Int(i64::MIN),
            InputValue::Bool(false),
            InputValue::Float(-0.0),
            InputValue::String("borrowed λ".into()),
        ] {
            program.inputs[1].defval = payload;
            let actual = ta_declared_default(&program.inputs, 1).expect("existing declaration");
            assert!(std::ptr::eq(actual, &raw const program.inputs[1].defval));
            assert!(!std::ptr::eq(actual, &raw const program.inputs[0].defval));
        }
    }

    #[test]
    fn declared_default_absent_and_slice_boundaries() {
        let program = program();
        assert!(ta_declared_default(&[], 0).is_none());
        assert!(ta_declared_default(&program.inputs[..1], 0).is_some());
        assert!(ta_declared_default(&program.inputs[..1], 1).is_none());
        assert!(ta_declared_default(&program.inputs, 2).is_none());
        assert!(ta_declared_default(&program.inputs, u32::MAX).is_none());
    }

    #[test]
    fn declared_default_native_resolution_absence_and_wrong_kind() {
        let mut program = program();
        let mut binding = program.ta_input_bindings[0];
        let valid = InputValue::Int(4);
        let wrong = InputValue::Bool(false);
        assert_eq!(program.resolve_ta_input(&binding, Some(&wrong)), Some(3));
        binding.input = 1;
        assert_eq!(program.resolve_ta_input(&binding, Some(&wrong)), Some(7));
        assert_eq!(program.resolve_ta_input(&binding, Some(&valid)), Some(4));
        binding.input = 2;
        assert_eq!(program.resolve_ta_input(&binding, Some(&valid)), None);
        binding.input = u32::MAX;
        assert_eq!(program.resolve_ta_input(&binding, Some(&valid)), None);
        binding.input = 1;
        program.inputs[1].defval = wrong;
        assert_eq!(program.resolve_ta_input(&binding, None), None);
        assert_eq!(program.resolve_ta_input(&binding, Some(&valid)), Some(4));
    }
    #[test]
    fn selected_integer_precedence_and_wrong_kind_fallback() {
        let integer = InputValue::Int(7);
        let override_value = InputValue::Int(i64::MIN);
        let wrong = InputValue::Bool(false);
        assert_eq!(
            ta_selected_integer(&wrong, Some(&override_value)),
            Some(i64::MIN)
        );
        assert_eq!(ta_selected_integer(&integer, Some(&wrong)), Some(7));
        assert_eq!(ta_selected_integer(&integer, None), Some(7));
        assert_eq!(ta_selected_integer(&wrong, Some(&wrong)), None);
        assert_eq!(ta_selected_integer(&wrong, None), None);
    }

    #[test]
    fn selected_integer_matches_original_selection() {
        let values = [
            InputValue::Int(i64::MIN),
            InputValue::Int(0),
            InputValue::Int(i64::MAX),
            InputValue::Bool(false),
            InputValue::Float(-0.0),
            InputValue::String("noninteger λ".into()),
        ];
        for default in &values {
            for candidate in std::iter::once(None).chain(values.iter().map(Some)) {
                let original = match candidate
                    .filter(|v| matches!(v, InputValue::Int(_)))
                    .unwrap_or(default)
                {
                    InputValue::Int(value) => Some(*value),
                    _ => None,
                };
                assert_eq!(ta_selected_integer(default, candidate), original);
            }
        }
    }

    #[test]
    fn selected_integer_native_window_and_absent_declaration() {
        let mut program = program();
        let mut binding = program.ta_input_bindings[0];
        // Window targets have no engine rule, but still have integer bounds.
        assert_eq!(program.ta_input_rule(&binding), None);
        let lower = InputValue::Int(1);
        let upper = InputValue::Int(MAX_WINDOW_LENGTH as i64);
        assert_eq!(program.resolve_ta_input(&binding, Some(&lower)), Some(1));
        assert_eq!(
            program.resolve_ta_input(&binding, Some(&upper)),
            Some(MAX_WINDOW_LENGTH as i64)
        );
        let below = InputValue::Int(0);
        let above = InputValue::Int(MAX_WINDOW_LENGTH as i64 + 1);
        assert_eq!(program.resolve_ta_input(&binding, Some(&below)), None);
        assert_eq!(program.resolve_ta_input(&binding, Some(&above)), None);
        let original_target = binding.target;
        binding.target = TaInputTarget::Change { slot: 0 };
        assert_eq!(program.ta_input_rule(&binding), None);
        assert_eq!(program.resolve_ta_input(&binding, Some(&lower)), Some(1));
        assert_eq!(
            program.resolve_ta_input(&binding, Some(&upper)),
            Some(MAX_WINDOW_LENGTH as i64)
        );
        assert_eq!(program.resolve_ta_input(&binding, Some(&below)), None);
        assert_eq!(program.resolve_ta_input(&binding, Some(&above)), None);
        binding.target = TaInputTarget::State {
            state: u32::MAX,
            param: 0,
        };
        assert_eq!(program.resolve_ta_input(&binding, Some(&lower)), None);
        // A real State exists, but the requested parameter is absent.
        let parsed =
            pinewasm_parse::parse("//@version=6\nindicator(\"state\")\nplot(ta.ema(close, 3))\n");
        assert!(!pinewasm_span::has_errors(&parsed.diagnostics));
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        assert!(!pinewasm_span::has_errors(&analysis.diagnostics));
        let state_program =
            crate::lower_mir(&pinewasm_mir::lower(&analysis.hir)).expect("lowering state fixture");
        assert!(!state_program.ta_states.is_empty());
        program.ta_states = state_program.ta_states;
        binding.target = TaInputTarget::State {
            state: 0,
            param: u32::MAX,
        };
        assert_eq!(program.resolve_ta_input(&binding, Some(&lower)), None);
        binding.target = original_target;
        program.inputs[binding.input as usize].defval = InputValue::Bool(false);
        assert_eq!(program.resolve_ta_input(&binding, None), None);
        assert_eq!(program.resolve_ta_input(&binding, Some(&lower)), Some(1));
        binding.negated = true;
        let minimum = InputValue::Int(i64::MIN);
        assert_eq!(program.resolve_ta_input(&binding, Some(&minimum)), None);
        binding.input = u32::MAX;
        binding.negated = false;
        assert_eq!(program.resolve_ta_input(&binding, Some(&lower)), None);
    }
}

#[cfg(test)]
mod float_sign_kernel_tests {
    use super::*;

    #[test]
    fn sign_bits_total_and_finite_unary_reference() {
        let encodings = [
            0,
            1_u64 << 63,
            1,
            (1_u64 << 63) | 1,
            0x0010_0000_0000_0000,
            0x7fef_ffff_ffff_ffff,
            0xffef_ffff_ffff_ffff,
            0x7ff0_0000_0000_0000,
            0xfff0_0000_0000_0000,
            0x7ff8_0000_0000_0001,
            0xfff0_0000_0000_0001,
        ];
        for bits in encodings {
            assert_eq!(ta_conditional_sign_bits(bits, false), bits);
            assert_eq!(ta_conditional_sign_bits(bits, true), bits ^ (1_u64 << 63));
            let value = f64::from_bits(bits);
            let flipped = f64::from_bits(ta_conditional_sign_bits(bits, true));
            assert_eq!(flipped.is_finite(), value.is_finite());
            if value.is_finite() {
                assert_eq!(flipped.to_bits(), (-value).to_bits());
            }
            // No arithmetic NaN payload equality is asserted.
        }
    }

    fn float_program(expression: &str) -> CsilProgram {
        let source = format!(
            "//@version=6\nindicator(\"sign\")\nx = input.float(2.0)\nplot({expression})\n"
        );
        let parsed = pinewasm_parse::parse(&source);
        assert!(!pinewasm_span::has_errors(&parsed.diagnostics));
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        assert!(!pinewasm_span::has_errors(&analysis.diagnostics));
        crate::lower_mir(&pinewasm_mir::lower(&analysis.hir)).expect("real resolver fixture")
    }

    // Copy of the direct branch logic, kept to check selection and guards.
    fn reference(
        program: &CsilProgram,
        binding: &TaInputBinding,
        override_value: Option<&InputValue>,
    ) -> Option<i64> {
        let default = ta_declared_default(&program.inputs, binding.input)?;
        let selected = match (default, override_value) {
            (InputValue::Int(_), Some(InputValue::Int(v))) => *v as f64,
            (InputValue::Float(_), Some(InputValue::Float(v))) => *v,
            (InputValue::Int(v), _) => *v as f64,
            (InputValue::Float(v), _) => *v,
            _ => return None,
        };
        let v = if binding.negated { -selected } else { selected };
        match program.ta_input_rule(binding)? {
            TaParamRule::FloatBits { positive } => {
                (v.is_finite() && (!positive || v > 0.0)).then_some(v.to_bits() as i64)
            }
            TaParamRule::Percent => {
                (v.is_finite() && (0.0..=100.0).contains(&v)).then_some(v.to_bits() as i64)
            }
            _ => panic!("expected actual float rule"),
        }
    }

    // Each expected rule is listed with its function. The test finds the binding
    // that resolves to that rule, then checks resolve_ta_input against `reference`.
    #[test]
    fn real_float_and_percent_resolver_match_expected_values() {
        for (expression, expected_rule) in [
            (
                "ta.bbw(close, 3, x)",
                TaParamRule::FloatBits { positive: false },
            ),
            ("ta.sar(x, x, x)", TaParamRule::FloatBits { positive: true }),
            (
                "ta.percentile_nearest_rank(close, 3, x)",
                TaParamRule::Percent,
            ),
        ] {
            let mut program = float_program(expression);
            let binding = *program
                .ta_input_bindings
                .iter()
                .find(|b| program.ta_input_rule(b) == Some(expected_rule))
                .expect("actual expected float rule binding");
            assert_eq!(program.ta_input_rule(&binding), Some(expected_rule));
            for default in [
                InputValue::Float(-0.0),
                InputValue::Float(2.0),
                InputValue::Int(i64::MIN),
                InputValue::Int(i64::MAX),
                InputValue::Int((1_i64 << 53) + 1),
                InputValue::Bool(false),
                InputValue::String("wrong kind".into()),
                InputValue::Color {
                    r: 1,
                    g: 2,
                    b: 3,
                    t: 4.0,
                },
                InputValue::Source(pinewasm_hir::BuiltinSeries::Close),
                InputValue::ExternalSource(pinewasm_hir::ExternalSourceRef {
                    script_id: "other_script".into(),
                    plot_slot: 0,
                    plot_label: "value".into(),
                }),
                InputValue::Enum {
                    name: "E".into(),
                    index: 0,
                    member: "A".into(),
                    title: None,
                },
            ] {
                program.inputs[binding.input as usize].defval = default;
                for override_value in [
                    None,
                    Some(InputValue::Bool(true)),
                    Some(InputValue::Int(8)),
                    Some(InputValue::String("wrong override".into())),
                    Some(InputValue::Color {
                        r: 1,
                        g: 2,
                        b: 3,
                        t: 4.0,
                    }),
                    Some(InputValue::Source(pinewasm_hir::BuiltinSeries::Close)),
                    Some(InputValue::ExternalSource(
                        pinewasm_hir::ExternalSourceRef {
                            script_id: "other_script".into(),
                            plot_slot: 0,
                            plot_label: "value".into(),
                        },
                    )),
                    Some(InputValue::Enum {
                        name: "E".into(),
                        index: 0,
                        member: "A".into(),
                        title: None,
                    }),
                    Some(InputValue::Int(i64::MAX)),
                    Some(InputValue::Int((1_i64 << 53) + 1)),
                    Some(InputValue::Float(-2.0)),
                    Some(InputValue::Float(0.0)),
                    Some(InputValue::Float(100.0)),
                    Some(InputValue::Float(100.5)),
                    Some(InputValue::Float(-0.0)),
                    Some(InputValue::Float(f64::from_bits(1))),
                    Some(InputValue::Float(f64::MAX)),
                    Some(InputValue::Float(f64::INFINITY)),
                    Some(InputValue::Float(f64::from_bits(0x7ff8_0000_0000_0001))),
                ] {
                    for negated in [false, true] {
                        let mut binding = binding;
                        binding.negated = negated;
                        assert_eq!(
                            program.resolve_ta_input(&binding, override_value.as_ref()),
                            reference(&program, &binding, override_value.as_ref())
                        );
                    }
                }
            }
            let mut absent = binding;
            absent.input = u32::MAX;
            assert_eq!(
                program.resolve_ta_input(&absent, Some(&InputValue::Float(8.0))),
                None
            );
            assert_eq!(
                reference(&program, &absent, Some(&InputValue::Float(8.0))),
                None
            );
            // A missing state or invalid parameter keeps the integer fallback:
            // float defaults cannot be selected as integers, and no float rule is invented.
            program.inputs[binding.input as usize].defval = InputValue::Float(2.0);
            for target in [
                TaInputTarget::State {
                    state: u32::MAX,
                    param: 0,
                },
                TaInputTarget::State {
                    state: match binding.target {
                        TaInputTarget::State { state, .. } => state,
                        _ => panic!("state fixture"),
                    },
                    param: u32::MAX,
                },
            ] {
                let mut invalid = binding;
                invalid.target = target;
                assert_eq!(program.ta_input_rule(&invalid), None);
                assert_eq!(
                    program.resolve_ta_input(&invalid, Some(&InputValue::Float(8.0))),
                    None
                );
            }
        }
    }
}

#[cfg(test)]
mod numeric_selection_tests {
    use super::*;

    fn selected_bits(
        default: &InputValue,
        override_value: Option<&InputValue>,
    ) -> Option<(bool, u64)> {
        ta_selected_numeric(default, override_value).map(|selected| match selected {
            TaSelectedNumeric::Int(value) => (false, value as u64),
            TaSelectedNumeric::Float(value) => (true, value.to_bits()),
        })
    }

    #[test]
    fn numeric_selection_kind_directed_and_exact_payloads() {
        for integer in [i64::MIN, i64::MAX, (1_i64 << 53) + 1, 0, -1] {
            let default = InputValue::Int(integer);
            let wrong = InputValue::Float(7.0);
            let replacement = InputValue::Int(11);
            assert_eq!(selected_bits(&default, None), Some((false, integer as u64)));
            assert_eq!(
                selected_bits(&default, Some(&wrong)),
                Some((false, integer as u64))
            );
            assert_eq!(
                selected_bits(&default, Some(&replacement)),
                Some((false, 11))
            );
        }
        for bits in [
            0,
            1_u64 << 63,
            1,
            0x7fef_ffff_ffff_ffff,
            0x7ff0_0000_0000_0000,
            0xfff0_0000_0000_0000,
            0x7ff8_0000_0000_0001,
        ] {
            let default = InputValue::Float(f64::from_bits(bits));
            let wrong = InputValue::Int(11);
            let replacement = InputValue::Float(f64::from_bits(bits ^ (1_u64 << 63)));
            assert_eq!(selected_bits(&default, None), Some((true, bits)));
            assert_eq!(selected_bits(&default, Some(&wrong)), Some((true, bits)));
            assert_eq!(
                selected_bits(&default, Some(&replacement)),
                Some((true, bits ^ (1_u64 << 63)))
            );
        }
    }

    #[test]
    fn nonnumeric_defaults_reject_numeric_overrides() {
        let defaults = [
            InputValue::Bool(false),
            InputValue::String("value".into()),
            InputValue::Color {
                r: 1,
                g: 2,
                b: 3,
                t: 4.0,
            },
            InputValue::Source(pinewasm_hir::BuiltinSeries::Close),
            InputValue::ExternalSource(pinewasm_hir::ExternalSourceRef {
                script_id: "other_script".into(),
                plot_slot: 0,
                plot_label: "value".into(),
            }),
            InputValue::Enum {
                name: "E".into(),
                index: 0,
                member: "A".into(),
                title: None,
            },
        ];
        for default in defaults {
            assert_eq!(
                selected_bits(&InputValue::Int(5), Some(&default)),
                Some((false, 5))
            );
            assert_eq!(
                selected_bits(&InputValue::Float(-0.0), Some(&default)),
                Some((true, 1_u64 << 63))
            );
            assert_eq!(selected_bits(&default, None), None);
            assert_eq!(selected_bits(&default, Some(&InputValue::Int(7))), None);
            assert_eq!(selected_bits(&default, Some(&InputValue::Float(7.0))), None);
        }
    }
}
