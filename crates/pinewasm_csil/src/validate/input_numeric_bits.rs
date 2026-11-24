//! Production numeric input classification with an explicit binary64 payload.
use pinewasm_hir::InputValue;

/// Keep the cast and classification of the numeric value; consumers decode before comparing.
/// A bit payload is not a numeric ordering key (notably for signed zero).
pub(super) fn numeric_input_value_bits(value: &InputValue) -> Option<u64> {
    match value {
        InputValue::Int(value) => Some((*value as f64).to_bits()),
        InputValue::Float(value) if value.is_finite() => Some(value.to_bits()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;

    // Expected-result copy of the declaration check. Its numeric paths use the
    // Option<f64>/domain logic, never the bit helper.
    fn baseline_validate_input_declaration(
        program: &CsilProgram,
        index: usize,
        errors: &mut Vec<ValidationError>,
    ) {
        let decl = &program.inputs[index];
        let mut bad = |message: String| errors.push(ValidationError::Input { index, message });
        if decl.id as usize != index {
            bad(format!("id {} does not match its index {index}", decl.id));
        }
        if !value_matches_kind(&decl.defval, decl.kind) {
            bad(format!(
                "defval does not match declared kind `input.{}`",
                decl.kind.describe()
            ));
        }
        if let Some(message) = invalid_input_value_domain(&decl.defval) {
            bad(format!("defval {message}"));
        }
        for (label, value) in [
            ("minval", decl.constraints.min.as_ref()),
            ("maxval", decl.constraints.max.as_ref()),
            ("step", decl.constraints.step.as_ref()),
        ] {
            if let Some(v) = value {
                if !value_matches_kind(v, decl.kind) {
                    bad(format!("{label} does not match declared kind"));
                }
                if let Some(message) = invalid_input_value_domain(v) {
                    bad(format!("{label} {message}"));
                }
            }
        }
        for opt in &decl.constraints.options {
            if !value_matches_kind(opt, decl.kind) {
                bad("an options entry does not match declared kind".to_string());
            }
            if let Some(message) = invalid_input_value_domain(opt) {
                bad(format!("an options entry {message}"));
            }
        }
        // min/max/step are numeric-only. Options additionally belong to the
        // string and enum dropdown forms.
        let has_numeric_bounds = decl.constraints.min.is_some()
            || decl.constraints.max.is_some()
            || decl.constraints.step.is_some();
        if !matches!(decl.kind, InputKind::Int | InputKind::Float) && has_numeric_bounds {
            bad(format!(
                "`input.{}` does not take numeric constraints",
                decl.kind.describe()
            ));
        }
        if !matches!(
            decl.kind,
            InputKind::Int | InputKind::Float | InputKind::String | InputKind::Enum
        ) && !decl.constraints.options.is_empty()
        {
            bad(format!(
                "`input.{}` does not take an options list",
                decl.kind.describe()
            ));
        }
        if has_numeric_bounds && !decl.constraints.options.is_empty() {
            bad("numeric bounds/step and options are mutually exclusive".to_string());
        }

        if let (Some(default), Some(minimum)) = (
            numeric_input_value(&decl.defval),
            decl.constraints.min.as_ref().and_then(numeric_input_value),
        ) {
            if default < minimum {
                bad("defval is below minval".to_string());
            }
        }
        if let (Some(default), Some(maximum)) = (
            numeric_input_value(&decl.defval),
            decl.constraints.max.as_ref().and_then(numeric_input_value),
        ) {
            if default > maximum {
                bad("defval is above maxval".to_string());
            }
        }
        if !decl.constraints.options.is_empty()
            && !decl.constraints.options.iter().any(|option| {
                baseline_input_values_share_domain_value(&decl.defval, option, decl.kind)
            })
        {
            bad("options do not contain the declared default".to_string());
        }
        if let InputValue::Enum {
            name: enum_name,
            index: default_index,
            ..
        } = &decl.defval
        {
            let mut seen_indexes = Vec::new();
            let mut seen_members: Vec<&str> = Vec::new();
            let mut contains_default = decl.constraints.options.is_empty();
            for option in &decl.constraints.options {
                let InputValue::Enum {
                    name,
                    index,
                    member,
                    ..
                } = option
                else {
                    continue;
                };
                if name != enum_name {
                    bad(format!(
                        "enum option `{name}` does not match declaring enum `{enum_name}`"
                    ));
                }
                if seen_indexes.contains(index) {
                    bad(format!("duplicate enum option index {index}"));
                }
                seen_indexes.push(*index);
                if !member.is_empty() {
                    if seen_members.contains(&member.as_str()) {
                        bad(format!("duplicate enum option member `{member}`"));
                    }
                    seen_members.push(member);
                }
                contains_default |= name == enum_name && index == default_index;
            }
            if !contains_default {
                bad("enum options do not contain the declared default member".to_string());
            }
        }
        let mut previous_dependency = None;
        for dependency in &decl.active.dependencies {
            if *dependency as usize >= index {
                bad(format!(
                    "active dependency {dependency} must reference a prior input"
                ));
            }
            if previous_dependency.is_some_and(|previous| previous >= *dependency) {
                bad("active dependencies must be sorted and unique".to_string());
            }
            previous_dependency = Some(*dependency);
        }
        if let Some(direct) = decl.active.direct {
            if direct.input as usize >= index {
                bad(format!(
                    "direct active dependency {} must reference a prior input",
                    direct.input
                ));
            } else if program.inputs[direct.input as usize].kind != InputKind::Bool {
                bad(format!(
                    "direct active dependency {} must reference a bool input",
                    direct.input
                ));
            }
            if !decl.active.dependencies.contains(&direct.input) {
                bad(format!(
                    "direct active dependency {} is missing from dependencies",
                    direct.input
                ));
            }
        }
        if let Some(expression) = &decl.active.expression {
            let mut expression_dependencies = Vec::new();
            let mut expression_nodes = 0;
            validate_active_expression(
                expression,
                index,
                1,
                &mut expression_nodes,
                &mut expression_dependencies,
                &mut bad,
            );
            expression_dependencies.sort_unstable();
            expression_dependencies.dedup();
            if expression_dependencies != decl.active.dependencies {
                bad(format!(
                    "active expression dependencies {:?} do not match declared dependencies {:?}",
                    expression_dependencies, decl.active.dependencies
                ));
            }
        }
    }

    fn baseline_input_values_share_domain_value(
        left: &InputValue,
        right: &InputValue,
        kind: InputKind,
    ) -> bool {
        match kind {
            InputKind::Float => numeric_input_value(left) == numeric_input_value(right),
            InputKind::Enum => matches!(
                (left, right),
                (
                    InputValue::Enum {
                        name: left_name,
                        index: left_index,
                        ..
                    },
                    InputValue::Enum {
                        name: right_name,
                        index: right_index,
                        ..
                    }
                ) if left_name == right_name && left_index == right_index
            ),
            _ => left == right,
        }
    }

    fn values() -> Vec<InputValue> {
        let mut values = [
            i64::MIN,
            i64::MAX,
            -9_007_199_254_740_993,
            -9_007_199_254_740_992,
            -1,
            0,
            1,
            9_007_199_254_740_991,
            9_007_199_254_740_992,
            9_007_199_254_740_993,
        ]
        .into_iter()
        .map(InputValue::Int)
        .collect::<Vec<_>>();
        values.extend(
            [
                0,
                0x8000_0000_0000_0000,
                1,
                0x8000_0000_0000_0001,
                0x0010_0000_0000_0000,
                0x8010_0000_0000_0000,
                0x3ff0_0000_0000_0000,
                0xbff0_0000_0000_0000,
                0x7fef_ffff_ffff_ffff,
                0xffef_ffff_ffff_ffff,
                0x7ff0_0000_0000_0000,
                0xfff0_0000_0000_0000,
                0x7ff8_0000_0000_0001,
                0xfff8_0000_0000_0001,
                0x7ff0_0000_0000_0001,
                0xfff0_0000_0000_0001,
            ]
            .into_iter()
            .map(|bits| InputValue::Float(f64::from_bits(bits))),
        );
        values.extend([
            InputValue::Bool(false),
            InputValue::Bool(true),
            InputValue::String(String::new()),
            InputValue::String("χ".into()),
            InputValue::Color {
                r: 1,
                g: 2,
                b: 3,
                t: 100.0,
            },
            InputValue::Color {
                r: 255,
                g: 0,
                b: 7,
                t: 101.0,
            },
            InputValue::Source(pinewasm_hir::BuiltinSeries::Close),
            InputValue::ExternalSource(pinewasm_hir::ExternalSourceRef {
                script_id: "source χ".into(),
                plot_slot: 9,
                plot_label: "label".into(),
            }),
            InputValue::Enum {
                name: "E".into(),
                index: 1,
                member: "first".into(),
                title: None,
            },
            InputValue::Enum {
                name: "E".into(),
                index: 1,
                member: "alias".into(),
                title: Some("χ".into()),
            },
            InputValue::Enum {
                name: "Other".into(),
                index: 2,
                member: "other".into(),
                title: None,
            },
        ]);
        values
    }

    #[test]
    fn classification_matches_raw_bit_reference() {
        for value in values() {
            assert_eq!(
                numeric_input_value_bits(&value),
                numeric_input_value(&value).map(f64::to_bits),
                "{value:?}"
            );
        }
        assert_eq!(
            numeric_input_value_bits(&InputValue::Float(-0.0)),
            Some(1 << 63)
        );
        assert_eq!(
            numeric_input_value_bits(&InputValue::Int(9_007_199_254_740_992)),
            numeric_input_value_bits(&InputValue::Int(9_007_199_254_740_993))
        );
    }

    #[test]
    fn domain_matrix_preserves_signed_zero_rounding_and_none() {
        for kind in [
            InputKind::Int,
            InputKind::Float,
            InputKind::Bool,
            InputKind::String,
            InputKind::Color,
            InputKind::Source,
            InputKind::Price,
            InputKind::Time,
            InputKind::Enum,
        ] {
            for left in values() {
                for right in values() {
                    assert_eq!(
                        input_values_share_domain_value(&left, &right, kind),
                        baseline_input_values_share_domain_value(&left, &right, kind),
                        "{kind:?}: {left:?} vs {right:?}"
                    );
                }
            }
        }
        assert!(input_values_share_domain_value(
            &InputValue::Float(0.0),
            &InputValue::Float(-0.0),
            InputKind::Float
        ));
        assert!(input_values_share_domain_value(
            &InputValue::Bool(true),
            &InputValue::String("x".into()),
            InputKind::Float
        ));
        assert!(!input_values_share_domain_value(
            &InputValue::Int(0),
            &InputValue::Bool(false),
            InputKind::Float
        ));
    }

    fn program() -> CsilProgram {
        let parsed = pinewasm_parse::parse("//@version=6\nindicator(\"input bits χ\")\na = input.float(1.0)\nb = input.int(2)\nplot(a + b)");
        assert!(!pinewasm_span::has_errors(&parsed.diagnostics));
        let analyzed = pinewasm_semantics::analyze(&parsed.program);
        assert!(!pinewasm_span::has_errors(&analyzed.diagnostics));
        crate::lower_mir(&pinewasm_mir::lower(&analyzed.hir)).expect("lowers")
    }

    fn floating_payloads(program: &CsilProgram) -> Vec<u64> {
        program
            .inputs
            .iter()
            .flat_map(|input| {
                std::iter::once(&input.defval)
                    .chain(input.constraints.min.iter())
                    .chain(input.constraints.max.iter())
                    .chain(input.constraints.step.iter())
                    .chain(input.constraints.options.iter())
            })
            .filter_map(|value| match value {
                InputValue::Float(value) => Some(value.to_bits()),
                _ => None,
            })
            .collect()
    }

    fn compare_declarations(program: &CsilProgram) {
        let debug_before = format!("{program:?}");
        let bits_before = floating_payloads(program);
        let mut actual = vec![ValidationError::Metadata("retained χ".into())];
        let mut expected = vec![ValidationError::Metadata("retained χ".into())];
        validate_inputs(program, &mut actual);
        for index in 0..program.inputs.len() {
            baseline_validate_input_declaration(program, index, &mut expected);
        }
        assert_eq!(actual, expected);
        assert_eq!(
            actual.first(),
            Some(&ValidationError::Metadata("retained χ".into()))
        );
        assert_eq!(format!("{program:?}"), debug_before);
        assert_eq!(floating_payloads(program), bits_before);
    }

    #[test]
    fn complete_declaration_diagnostics_match_original_numeric_paths() {
        let base = program();
        let values = values();
        for kind in [
            InputKind::Int,
            InputKind::Float,
            InputKind::Bool,
            InputKind::String,
            InputKind::Color,
            InputKind::Source,
            InputKind::Price,
            InputKind::Time,
            InputKind::Enum,
        ] {
            for (i, default) in values.iter().enumerate() {
                for (j, constraint) in values.iter().enumerate() {
                    let mut program = base.clone();
                    let input = &mut program.inputs[0];
                    input.kind = kind;
                    input.defval = default.clone();
                    match (i + j) % 5 {
                        0 => {}
                        1 => input.constraints.min = Some(constraint.clone()),
                        2 => input.constraints.max = Some(constraint.clone()),
                        3 => input.constraints.options = vec![constraint.clone()],
                        _ => {
                            input.constraints.min = Some(constraint.clone());
                            input.constraints.max = Some(InputValue::Int(-1));
                            input.constraints.step = Some(InputValue::Float(-0.0));
                            input.constraints.options =
                                vec![constraint.clone(), constraint.clone()];
                        }
                    }
                    compare_declarations(&program);
                }
            }
        }
    }

    #[test]
    fn original_messages_order_and_complete_metadata_are_retained() {
        let mut program = program();
        let input = &mut program.inputs[0];
        input.id = 3;
        input.title = Some("title χ".into());
        input.tooltip = Some("tooltip".into());
        input.inline = Some("inline".into());
        input.group = Some("group".into());
        input.confirm = true;
        input.defval = InputValue::Float(-1.0);
        input.constraints.min = Some(InputValue::Float(1.0));
        input.constraints.max = Some(InputValue::Float(-2.0));
        input.constraints.step = Some(InputValue::Float(f64::INFINITY));
        input.constraints.options = vec![InputValue::Float(3.0)];
        input.active.dependencies = vec![1, 0, 0];
        input.active.direct = Some(pinewasm_hir::InputActiveDirect {
            input: 9,
            inverted: true,
        });
        input.active.expression = Some(InputActiveExpr::Input { input: 1 });
        compare_declarations(&program);
        let mut errors = Vec::new();
        validate_input_declaration(&program, 0, &mut errors);
        let messages: Vec<_> = errors
            .iter()
            .filter_map(|error| match error {
                ValidationError::Input { message, .. } => Some(message.as_str()),
                _ => None,
            })
            .collect();
        let expected_prefix = [
            "id 3 does not match its index 0",
            "step float must be finite",
            "numeric bounds/step and options are mutually exclusive",
            "defval is below minval",
            "defval is above maxval",
            "options do not contain the declared default",
        ];
        assert_eq!(&messages[..expected_prefix.len()], expected_prefix);
    }
}
