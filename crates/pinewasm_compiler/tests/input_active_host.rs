//! Host-reactive metadata for the `input.*(active = ...)` surface of Pine Script® v6.

use pinewasm_compiler::{
    compile, CompileOptions, InputActiveBinaryOp, InputActiveExpr, InputActiveUnaryOp, InputValue,
    Program,
};

fn compile_ok(source: &str) -> Program {
    let result = compile(source, CompileOptions::default());
    assert!(
        result.success,
        "expected success, got diagnostics: {:?}",
        result.diagnostics
    );
    result.program.expect("program on successful compile")
}

#[test]
fn compound_active_alias_serializes_complete_reactive_tree() {
    let program = compile_ok(
        r#"//@version=6
indicator("Reactive active")
mode = input.string("Off", "Mode", options = ["Off", "On"])
limit = input.int(3, "Limit")
derived = mode == "On" and limit + 1 >= 4
value = input.int(5, "Value", active = derived)
plot(value)
"#,
    );

    let active = &program.inputs[2].active;
    assert!(!active.default);
    assert_eq!(active.dependencies, vec![0, 1]);
    assert_eq!(active.direct, None);
    let Some(InputActiveExpr::Binary {
        op: InputActiveBinaryOp::And,
        ..
    }) = active.expression.as_ref()
    else {
        panic!(
            "expected complete logical expression, got {:?}",
            active.expression
        );
    };

    let json = serde_json::to_string(&program).expect("serialize program");
    assert!(json.contains("\"expression\""));
    assert!(json.contains("\"kind\":\"binary\""));
    assert!(json.contains("\"op\":\"and\""));
    let round_trip: Program = serde_json::from_str(&json).expect("deserialize program");
    assert_eq!(round_trip.inputs[2].active, *active);
}

#[test]
fn ternary_active_expression_uses_declared_defaults() {
    let program = compile_ok(
        r#"//@version=6
indicator("Ternary active")
enabled = input.bool(false, "Enabled")
gate = enabled ? true : false
value = input.float(1.0, "Value", active = gate)
plot(value)
"#,
    );

    let active = &program.inputs[1].active;
    assert!(!active.default);
    assert_eq!(active.dependencies, vec![0]);
    assert!(matches!(
        active.expression,
        Some(InputActiveExpr::Ternary { .. })
    ));
}

#[test]
fn validator_rejects_tampered_active_expression_references() {
    let mut program = compile_ok(
        r#"//@version=6
indicator("Validated active")
enabled = input.bool(true, "Enabled")
value = input.int(1, "Value", active = enabled)
plot(value)
"#,
    );
    // Input 1 may only reference prior input 0. A cold/tampered artifact must
    // not smuggle the current declaration into host-facing metadata.
    program.inputs[1].active.expression = Some(InputActiveExpr::Input { input: 1 });
    let errors = pinewasm_csil::validate(&program).expect_err("tampered active tree must fail");
    assert!(errors.iter().any(|error| {
        error
            .to_string()
            .contains("active expression input 1 must reference a prior input")
    }));
}

#[test]
fn non_allowlisted_pure_calls_use_the_safe_enabled_fallback() {
    let program = compile_ok(
        r#"//@version=6
indicator("Pure active")
level = input.int(0, "Level")
mode = input.string("manual", "Mode")
gate = bool(level) and str.tonumber(mode) > 0
value = input.float(1.0, "Value", active = gate)
plot(value)
"#,
    );
    let active = &program.inputs[2].active;
    assert!(active.default);
    assert!(active.dependencies.is_empty());
    assert_eq!(active.expression, None);
}

#[test]
fn absurd_active_repeat_is_bounded_without_rejecting_the_script() {
    let program = compile_ok(
        r#"//@version=6
indicator("Bounded active")
repeat = input.int(4294967296, "Repeat")
value = input.int(1, "Value", active = str.repeat("", repeat) == "")
plot(value)
"#,
    );

    let active = &program.inputs[1].active;
    assert!(
        active.default,
        "an opaque bounded host call uses the safe fallback"
    );
    assert_eq!(active.dependencies, vec![0]);
    assert_eq!(active.expression, None);
}

#[test]
fn math_sign_collapses_negative_zero() {
    let program = compile_ok(
        r#"//@version=6
indicator("Signed zero")
zero = input.float(-0.0, "Zero")
value = input.int(1, "Value", active = math.sign(zero) == 0)
plot(value)
"#,
    );

    let active = &program.inputs[1].active;
    assert!(active.default);
    assert_eq!(active.dependencies, vec![0]);
    assert!(matches!(
        active.expression,
        Some(InputActiveExpr::Binary { .. })
    ));
}

#[test]
fn nz_results_cannot_drive_input_active() {
    for expression in ["nz(1.0 / denominator, 0.0) == 0.0", "nz(-minimum, 0) > 1"] {
        let source = format!("//@version=6\nindicator(\"NZ qualifier\")\ndenominator = input.float(1.0)\nminimum = input.int(-9223372036854775807 - 1)\nvalue = input.int(1, active = {expression})\nplot(value)\n");
        let result = compile(&source, CompileOptions::default());
        assert!(!result.success, "must reject simple result: {expression}");
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code.as_deref() == Some("E0230")),
            "{:?}",
            result.diagnostics
        );
    }
}

#[test]
fn validator_rejects_unbounded_host_calls_and_expression_shapes() {
    let mut program = compile_ok(
        r#"//@version=6
indicator("Validated host expression")
value = input.int(1, "Value", active = true)
plot(value)
"#,
    );

    for function in [
        "str.match",
        "str.pos",
        "str.tonumber",
        "str.upper",
        "timestamp",
    ] {
        program.inputs[0].active.expression = Some(InputActiveExpr::Call {
            function: function.to_string(),
            args: vec![InputActiveExpr::Value {
                value: InputValue::String("host input".to_string()),
            }],
        });
        let errors = pinewasm_csil::validate(&program)
            .expect_err("host call outside the bounded allowlist must fail");
        assert!(errors.iter().any(|error| {
            error
                .to_string()
                .contains(&format!("`{function}` is not supported by bounded hosts"))
        }));
    }

    let mut expression = InputActiveExpr::Value {
        value: InputValue::Bool(true),
    };
    for _ in 0..65 {
        expression = InputActiveExpr::Unary {
            op: InputActiveUnaryOp::Not,
            operand: Box::new(expression),
        };
    }
    program.inputs[0].active.expression = Some(expression);
    let errors = pinewasm_csil::validate(&program).expect_err("deep host tree must fail");
    assert!(errors.iter().any(|error| {
        error
            .to_string()
            .contains("active expression exceeds the 64-level nesting limit")
    }));
}

#[test]
fn simple_string_overload_results_cannot_control_input_active() {
    for gate in ["str.contains(mode, \"auto\")", "str.length(mode) == 0"] {
        let source = format!("//@version=6\nindicator(\"Active qualifier\")\nmode = input.string(\"manual\")\nx = input.int(1, active = {gate})\nplot(x)\n");
        let result = compile(&source, CompileOptions::default());
        assert!(!result.success);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code.as_deref() == Some("E0230")),
            "{:?}",
            result.diagnostics
        );
    }
}

#[test]
fn na_result_cannot_control_input_active() {
    let result = compile("indicator(\"Invalid active\")\nminimum = input.int(1)\nn = input.int(1, active=na(minimum))\nplot(n)\n", CompileOptions::default());
    assert!(!result.success);
}
