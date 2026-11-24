//! Expected-result copies of the input validation kernels, used in the
//! equivalence tests. The recursive helper copy is pub(super) so the cfg(test)
//! module can call it.
use super::*;
use pinewasm_hir::{InputActiveBinaryOp, InputActiveDirect, InputActiveUnaryOp};

fn copied_validate_input_declaration(
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
    if let Some(reason) = input_domain_reason::invalid_reason(&decl.defval) {
        let message = reason.message();
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
            if let Some(reason) = input_domain_reason::invalid_reason(v) {
                let message = reason.message();
                bad(format!("{label} {message}"));
            }
        }
    }
    for opt in &decl.constraints.options {
        if !value_matches_kind(opt, decl.kind) {
            bad("an options entry does not match declared kind".to_string());
        }
        if let Some(reason) = input_domain_reason::invalid_reason(opt) {
            let message = reason.message();
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
        numeric_input_value_bits(&decl.defval),
        match decl.constraints.min.as_ref() {
            Some(value) => numeric_input_value_bits(value),
            None => None,
        },
    ) {
        let default = f64::from_bits(default);
        let minimum = f64::from_bits(minimum);
        if default < minimum {
            bad("defval is below minval".to_string());
        }
    }
    if let (Some(default), Some(maximum)) = (
        numeric_input_value_bits(&decl.defval),
        match decl.constraints.max.as_ref() {
            Some(value) => numeric_input_value_bits(value),
            None => None,
        },
    ) {
        let default = f64::from_bits(default);
        let maximum = f64::from_bits(maximum);
        if default > maximum {
            bad("defval is above maxval".to_string());
        }
    }
    if !decl.constraints.options.is_empty()
        && !decl
            .constraints
            .options
            .iter()
            .any(|option| input_values_share_domain_value(&decl.defval, option, decl.kind))
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
        copied_validate_active_expression(
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

pub(super) fn copied_validate_active_expression(
    expression: &InputActiveExpr,
    input_index: usize,
    depth: usize,
    nodes: &mut usize,
    dependencies: &mut Vec<u32>,
    bad: &mut impl FnMut(String),
) {
    *nodes = nodes.saturating_add(1);
    if *nodes > MAX_INPUT_ACTIVE_EXPRESSION_NODES {
        if *nodes == MAX_INPUT_ACTIVE_EXPRESSION_NODES + 1 {
            bad(format!(
                "active expression exceeds the {MAX_INPUT_ACTIVE_EXPRESSION_NODES}-node limit"
            ));
        }
        return;
    }
    if depth > MAX_INPUT_ACTIVE_EXPRESSION_DEPTH {
        bad(format!(
            "active expression exceeds the {MAX_INPUT_ACTIVE_EXPRESSION_DEPTH}-level nesting limit"
        ));
        return;
    }
    match expression {
        InputActiveExpr::Value { value } => match value {
            InputValue::Source(_) | InputValue::ExternalSource(_) => {
                bad("active expression cannot contain a series source value".to_string());
            }
            InputValue::Float(value) if !value.is_finite() => {
                bad("active expression float value must be finite".to_string());
            }
            InputValue::Color { t, .. } if !t.is_finite() || !(0.0..=100.0).contains(t) => {
                bad(format!(
                    "active expression color transparency {t} out of range (0..=100)"
                ));
            }
            InputValue::String(value) if value.len() > MAX_INPUT_ACTIVE_STRING_BYTES => {
                bad(format!(
                    "active expression string exceeds the {MAX_INPUT_ACTIVE_STRING_BYTES}-byte limit"
                ));
            }
            _ => {}
        },
        InputActiveExpr::Na => {}
        InputActiveExpr::Input { input } => {
            if *input as usize >= input_index {
                bad(format!(
                    "active expression input {input} must reference a prior input"
                ));
            }
            dependencies.push(*input);
        }
        InputActiveExpr::Unary { operand, .. } => {
            copied_validate_active_expression(
                operand,
                input_index,
                depth + 1,
                nodes,
                dependencies,
                bad,
            );
        }
        InputActiveExpr::Binary { lhs, rhs, .. } => {
            copied_validate_active_expression(
                lhs,
                input_index,
                depth + 1,
                nodes,
                dependencies,
                bad,
            );
            copied_validate_active_expression(
                rhs,
                input_index,
                depth + 1,
                nodes,
                dependencies,
                bad,
            );
        }
        InputActiveExpr::Ternary {
            condition,
            when_true,
            when_false,
        } => {
            copied_validate_active_expression(
                condition,
                input_index,
                depth + 1,
                nodes,
                dependencies,
                bad,
            );
            copied_validate_active_expression(
                when_true,
                input_index,
                depth + 1,
                nodes,
                dependencies,
                bad,
            );
            copied_validate_active_expression(
                when_false,
                input_index,
                depth + 1,
                nodes,
                dependencies,
                bad,
            );
        }
        InputActiveExpr::Call { function, args } => {
            if function.is_empty() || function.len() > 64 {
                bad("active expression call has an invalid function name".to_string());
            } else if let Some((min, max)) = input_active_call_argc_bounds(function) {
                if !(min..=max).contains(&args.len()) {
                    bad(format!(
                        "active expression call `{function}` expects {min}..={max} arguments, got {}",
                        args.len()
                    ));
                }
            } else {
                bad(format!(
                    "active expression call `{function}` is not supported by bounded hosts"
                ));
            }
            if args.len() > MAX_INPUT_ACTIVE_CALL_ARGUMENTS {
                bad(format!(
                    "active expression call `{function}` has too many arguments"
                ));
            }
            for arg in args {
                copied_validate_active_expression(
                    arg,
                    input_index,
                    depth + 1,
                    nodes,
                    dependencies,
                    bad,
                );
            }
        }
    }
}

fn program() -> CsilProgram {
    let parsed = pinewasm_parse::parse(
        "//@version=6\nindicator(\"sink\")\na = input.bool(true, \"a\")\nb = input.int(3, \"b\")\nplot(close)",
    );
    assert!(!pinewasm_span::has_errors(&parsed.diagnostics));
    let analyzed = pinewasm_semantics::analyze(&parsed.program);
    assert!(!pinewasm_span::has_errors(&analyzed.diagnostics));
    crate::lower_mir(&pinewasm_mir::lower(&analyzed.hir)).expect("lowers")
}
fn prefix() -> Vec<ValidationError> {
    vec![ValidationError::Input {
        index: 91,
        message: "prior χ\0diagnostic".to_string(),
    }]
}
fn compare(program: &CsilProgram) -> Vec<ValidationError> {
    let mut old = prefix();
    let mut new = prefix();
    for index in 0..program.inputs.len() {
        copied_validate_input_declaration(program, index, &mut old);
    }
    validate_inputs(program, &mut new);
    assert_eq!(new, old, "full ordered errors on {:?}", program.inputs);
    assert_eq!(new.first(), prefix().first());
    new
}
fn enum_value(name: &str, index: u32, member: &str) -> InputValue {
    InputValue::Enum {
        name: name.to_string(),
        index,
        member: member.to_string(),
        title: None,
    }
}
#[test]
fn immediate_append_preserves_exact_index_message_and_empty_inventory() {
    let mut errors = prefix();
    input_diagnostics::emit(&mut errors, 17, "new Ω\0message".to_string());
    assert_eq!(
        errors,
        vec![
            prefix().remove(0),
            ValidationError::Input {
                index: 17,
                message: "new Ω\0message".to_string()
            }
        ]
    );
    let mut p = program();
    p.inputs.clear();
    assert_eq!(compare(&p), prefix());
}
#[test]
fn ordered_multifault_enum_and_dependency_diagnostics() {
    let mut p = program();
    p.inputs[1].id = 99;
    p.inputs[1].kind = InputKind::Enum;
    p.inputs[1].defval = enum_value("Ω\0decl", 7, "default");
    p.inputs[1].constraints.min = Some(InputValue::Float(f64::INFINITY));
    p.inputs[1].constraints.max = Some(InputValue::Bool(false));
    p.inputs[1].constraints.step = Some(InputValue::Color {
        r: 1,
        g: 19,
        b: 241,
        t: 255.0,
    });
    p.inputs[1].constraints.options = vec![
        enum_value("other χ\0", 2, "member"),
        enum_value("other χ\0", 2, "member"),
        InputValue::Bool(true),
    ];
    p.inputs[1].active.dependencies = vec![2, 0, 0];
    p.inputs[1].active.direct = Some(InputActiveDirect {
        input: 1,
        inverted: true,
    });
    p.inputs[1].active.expression = Some(InputActiveExpr::Input { input: 3 });
    let errors = compare(&p);
    assert!(errors.len() > 15);
    assert_eq!(
        errors[1],
        ValidationError::Input {
            index: 1,
            message: "id 99 does not match its index 1".to_string()
        }
    );
    p.inputs[1].active.direct = Some(InputActiveDirect {
        input: 0,
        inverted: false,
    });
    p.inputs[0].kind = InputKind::Int;
    compare(&p);
    p.inputs[1].constraints.options.clear();
    compare(&p);
}
#[test]
fn numeric_domains_options_and_every_transparency_byte() {
    let mut p = program();
    let bits = [
        0,
        0x8000_0000_0000_0000,
        1,
        0x0010_0000_0000_0000,
        0x7fef_ffff_ffff_ffff,
        0xffef_ffff_ffff_ffff,
        0x7ff0_0000_0000_0000,
        0xfff0_0000_0000_0000,
        0x7ff8_0000_0000_0001,
        0x7ff0_0000_0000_0001,
    ];
    for kind in [
        InputKind::Int,
        InputKind::Float,
        InputKind::Price,
        InputKind::Time,
        InputKind::Bool,
        InputKind::String,
        InputKind::Color,
        InputKind::Source,
        InputKind::Enum,
    ] {
        for value in bits
            .into_iter()
            .map(|b| InputValue::Float(f64::from_bits(b)))
            .chain([
                InputValue::Int(i64::MIN),
                InputValue::Int(i64::MAX),
                InputValue::String("χ\0".to_string()),
                InputValue::Bool(false),
            ])
        {
            p.inputs[1].kind = kind;
            p.inputs[1].defval = value.clone();
            p.inputs[1].constraints.min = Some(InputValue::Int(5));
            p.inputs[1].constraints.max = Some(InputValue::Int(-5));
            p.inputs[1].constraints.step = Some(value.clone());
            p.inputs[1].constraints.options = vec![value, InputValue::Int(3)];
            compare(&p);
        }
    }
    p.inputs[1].kind = InputKind::Color;
    for t in 0..=255 {
        let value = InputValue::Color {
            r: 1,
            g: 19,
            b: 241,
            t: f64::from(t),
        };
        p.inputs[1].defval = value.clone();
        p.inputs[1].constraints.min = Some(value.clone());
        p.inputs[1].constraints.max = Some(value.clone());
        p.inputs[1].constraints.step = Some(value.clone());
        p.inputs[1].constraints.options = vec![value];
        compare(&p);
    }
}
fn compare_expression(
    expression: &InputActiveExpr,
    depth: usize,
    initial_nodes: usize,
) -> (usize, Vec<u32>, Vec<ValidationError>) {
    let (mut old_nodes, mut new_nodes) = (initial_nodes, initial_nodes);
    let (mut old_deps, mut new_deps) = (vec![77], vec![77]);
    let (mut old, mut new) = (prefix(), prefix());
    copied_validate_active_expression(
        expression,
        1,
        depth,
        &mut old_nodes,
        &mut old_deps,
        &mut |message| old.push(ValidationError::Input { index: 1, message }),
    );
    validate_active_expression_with_errors(
        expression,
        1,
        depth,
        &mut new_nodes,
        &mut new_deps,
        &mut new,
    );
    assert_eq!(
        (new_nodes, &new_deps, &new),
        (old_nodes, &old_deps, &old),
        "{expression:?} depth={depth} nodes={initial_nodes}"
    );
    (new_nodes, new_deps, new)
}
#[test]
fn recursive_order_state_and_exact_guard_thresholds() {
    let expression = InputActiveExpr::Ternary {
        condition: Box::new(InputActiveExpr::Input { input: 9 }),
        when_true: Box::new(InputActiveExpr::Binary {
            op: InputActiveBinaryOp::Add,
            lhs: Box::new(InputActiveExpr::Input { input: 4 }),
            rhs: Box::new(InputActiveExpr::Value {
                value: InputValue::Color {
                    r: 1,
                    g: 19,
                    b: 241,
                    t: 101.0,
                },
            }),
        }),
        when_false: Box::new(InputActiveExpr::Unary {
            op: InputActiveUnaryOp::Not,
            operand: Box::new(InputActiveExpr::Input { input: 8 }),
        }),
    };
    let (_, deps, errors) = compare_expression(&expression, 1, 0);
    assert_eq!(deps, [77, 9, 4, 8]);
    assert!(
        matches!(&errors[1], ValidationError::Input { message, .. } if message.contains("input 9"))
    );
    for depth in [
        MAX_INPUT_ACTIVE_EXPRESSION_DEPTH,
        MAX_INPUT_ACTIVE_EXPRESSION_DEPTH + 1,
    ] {
        for nodes in [
            MAX_INPUT_ACTIVE_EXPRESSION_NODES - 1,
            MAX_INPUT_ACTIVE_EXPRESSION_NODES,
            MAX_INPUT_ACTIVE_EXPRESSION_NODES + 1,
            usize::MAX,
        ] {
            compare_expression(&expression, depth, nodes);
        }
    }
    for function in [
        "",
        "χ\0unknown",
        "math.abs",
        "na",
        "math.sign",
        "nz",
        "math.min",
        "math.max",
        &"a".repeat(65),
    ] {
        for count in [
            0,
            1,
            2,
            MAX_INPUT_ACTIVE_CALL_ARGUMENTS,
            MAX_INPUT_ACTIVE_CALL_ARGUMENTS + 1,
            MAX_INPUT_ACTIVE_EXPRESSION_NODES + 2,
        ] {
            let call = InputActiveExpr::Call {
                function: function.to_string(),
                args: (0..count)
                    .map(|i| InputActiveExpr::Input {
                        input: u32::try_from(i).unwrap(),
                    })
                    .collect(),
            };
            compare_expression(&call, 1, 0);
        }
    }
    for value in [
        InputValue::Float(f64::INFINITY),
        InputValue::Color {
            r: 1,
            g: 19,
            b: 241,
            t: 255.0,
        },
        InputValue::String("a".repeat(MAX_INPUT_ACTIVE_STRING_BYTES - 1)),
        InputValue::String("a".repeat(MAX_INPUT_ACTIVE_STRING_BYTES)),
        InputValue::String("a".repeat(MAX_INPUT_ACTIVE_STRING_BYTES + 1)),
        InputValue::String("χ".repeat(MAX_INPUT_ACTIVE_STRING_BYTES)),
        InputValue::ExternalSource(pinewasm_hir::ExternalSourceRef {
            script_id: "χ\0".to_string(),
            plot_slot: 7,
            plot_label: "external".to_string(),
        }),
        InputValue::Source(pinewasm_hir::BuiltinSeries::Close),
    ] {
        compare_expression(&InputActiveExpr::Value { value }, 1, 0);
    }
    compare_expression(&InputActiveExpr::Na, 1, 0);
    let mut p = program();
    p.inputs[1].active.expression = Some(expression);
    p.inputs[1].active.dependencies = vec![9, 4, 8];
    compare(&p);
}
