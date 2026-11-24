//! Coverage for trigonometric and exponential math overloads.
//! Covers selected unary `math.*` functions.
use pinewasm_compiler::{compile, compile_and_run, CompileOptions, MarketData, RunOptions};

#[test]
fn unary_math_functions_accept_const_input_simple_and_series_qualifiers() {
    let expressions = [
        ("math.log10", "3.0"),
        ("math.exp", "3.0"),
        ("math.cos", "3.0"),
        ("math.tan", "3.0"),
        ("math.asin", "0.5"),
        ("math.acos", "0.5"),
        ("math.atan", "0.5"),
    ];
    let mut body = String::new();
    for (index, (name, value)) in expressions.into_iter().enumerate() {
        body.push_str(&format!("float c_{index} = {name}({value})\n"));
        body.push_str(&format!("float i_{index} = {name}(input.float({value}))\n"));
        body.push_str(&format!(
            "float s_{index} = {name}(timeframe.in_seconds() / 60.0)\n"
        ));
        body.push_str(&format!("float r_{index} = {name}(close)\n"));
    }
    let source = format!("//@version=6\nindicator(\"unary math overloads\")\n{body}");
    let result = compile(&source, CompileOptions::default());
    assert!(result.success, "{source}\n{:?}", result.diagnostics);
}

#[test]
fn unary_math_functions_reject_strings() {
    for name in ["log10", "exp", "cos", "tan", "asin", "acos", "atan"] {
        let source =
            format!("//@version=6\nindicator(\"invalid math\")\nplot(math.{name}(\"0.5\"))");
        let result = compile(&source, CompileOptions::default());
        assert!(
            !result.success,
            "unexpectedly accepted math.{name} string input"
        );
    }
}

#[test]
fn unary_math_functions_reject_missing_and_extra_arguments() {
    for name in [
        "abs", "ceil", "floor", "sqrt", "log", "log10", "exp", "sin", "cos", "tan", "asin", "acos",
        "atan",
    ] {
        for arguments in ["", "1, 2"] {
            let source = format!(
                "//@version=6\nindicator(\"invalid arity\")\nplot(math.{name}({arguments}))"
            );
            let result = compile(&source, CompileOptions::default());
            assert!(
                !result.success,
                "unexpectedly accepted math.{name}({arguments})"
            );
        }
    }
    for arguments in ["", "1, 2, 3"] {
        let source = format!(
            "//@version=6\nindicator(\"invalid round arity\")\nplot(math.round({arguments}))"
        );
        let result = compile(&source, CompileOptions::default());
        assert!(
            !result.success,
            "unexpectedly accepted math.round({arguments})"
        );
    }
}

#[test]
fn unary_math_runtime_values_match_formulas() {
    let source = r#"//@version=6
indicator("unary math runtime values")
plot(math.log10(100.0))
plot(math.exp(0.0))
plot(math.cos(0.0))
plot(math.tan(0.0))
plot(math.asin(0.0))
plot(math.acos(1.0))
plot(math.atan(0.0))
"#;
    let market = MarketData {
        open: vec![2.0; 2],
        high: vec![3.0; 2],
        low: vec![1.0; 2],
        close: vec![2.0; 2],
        volume: vec![10.0; 2],
        ..MarketData::default()
    };
    let result = compile_and_run(
        source,
        &market,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(result
        .compile
        .as_ref()
        .is_some_and(|compile| compile.success));
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    let actual: Vec<_> = result
        .plots
        .iter()
        .map(|plot| serde_json::to_value(&plot.values).unwrap())
        .collect();
    assert_eq!(
        actual,
        vec![
            serde_json::json!([2.0, 2.0]),
            serde_json::json!([1.0, 1.0]),
            serde_json::json!([1.0, 1.0]),
            serde_json::json!([0.0, 0.0]),
            serde_json::json!([0.0, 0.0]),
            serde_json::json!([0.0, 0.0]),
            serde_json::json!([0.0, 0.0]),
        ]
    );
}
