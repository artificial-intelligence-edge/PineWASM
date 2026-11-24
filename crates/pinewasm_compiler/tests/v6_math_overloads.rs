//! Coverage for numeric math overload types and qualifiers.
use pinewasm_compiler::{compile, compile_and_run, CompileOptions, MarketData, RunOptions};

#[test]
fn unary_math_overloads_accept_all_types_and_qualifiers() {
    let cases = [
        ("int", "math.abs(-3)"),
        ("int", "math.abs(input.int(-3))"),
        ("int", "math.abs(timeframe.in_seconds())"),
        ("int", "math.abs(bar_index - 3)"),
        ("float", "math.abs(-3.25)"),
        ("float", "math.abs(input.float(-3.25))"),
        ("float", "math.abs(timeframe.in_seconds() / 60.0)"),
        ("float", "math.abs(close)"),
        ("int", "math.ceil(-3.25)"),
        ("int", "math.ceil(input.float(-3.25))"),
        ("int", "math.ceil(timeframe.in_seconds() / 60.0)"),
        ("int", "math.ceil(close)"),
        ("int", "math.floor(-3.25)"),
        ("int", "math.floor(input.float(-3.25))"),
        ("int", "math.floor(timeframe.in_seconds() / 60.0)"),
        ("int", "math.floor(close)"),
        ("int", "math.round(-3.25)"),
        ("int", "math.round(input.float(-3.25))"),
        ("int", "math.round(timeframe.in_seconds() / 60.0)"),
        ("int", "math.round(close)"),
        ("float", "math.sqrt(9.0)"),
        ("float", "math.sqrt(input.float(9.0))"),
        ("float", "math.sqrt(timeframe.in_seconds() / 60.0)"),
        ("float", "math.sqrt(close)"),
        ("float", "math.log(9.0)"),
        ("float", "math.log(input.float(9.0))"),
        ("float", "math.log(timeframe.in_seconds() / 60.0)"),
        ("float", "math.log(close)"),
        ("float", "math.sin(3.0)"),
        ("float", "math.sin(input.float(3.0))"),
        ("float", "math.sin(timeframe.in_seconds() / 60.0)"),
        ("float", "math.sin(close)"),
    ];
    let body = cases
        .iter()
        .enumerate()
        .map(|(index, (ty, expression))| {
            format!("{ty} value_{index} = {expression}\nplot(value_{index})\n")
        })
        .collect::<String>();
    let source = format!("//@version=6\nindicator(\"unary math overloads\")\n{body}");
    let result = compile(&source, CompileOptions::default());
    assert!(result.success, "{source}\n{:?}", result.diagnostics);
}

#[test]
fn floor_and_ceil_preserve_each_result_qualifier() {
    // These eight cases distinguish result and argument qualifiers.
    // `const`, `simple`, and `series` are asserted in declarations; Pine's
    // input qualifier comes directly from input.float() and is checked by the
    // call's type binding.
    let source = r#"//@version=6
indicator("floor and ceil qualifiers")
const int floor_const = math.floor(-2.1)
int floor_input = math.floor(input.float(-2.1))
simple int floor_simple = math.floor(timeframe.in_seconds() / 60.0)
series int floor_series = math.floor(close)
const int ceil_const = math.ceil(-2.1)
int ceil_input = math.ceil(input.float(-2.1))
simple int ceil_simple = math.ceil(timeframe.in_seconds() / 60.0)
series int ceil_series = math.ceil(close)
plot(floor_const)
plot(floor_input)
plot(floor_simple)
plot(floor_series)
plot(ceil_const)
plot(ceil_input)
plot(ceil_simple)
plot(ceil_series)
"#;
    let result = compile(source, CompileOptions::default());
    assert!(result.success, "{:?}", result.diagnostics);
}

#[test]
fn precision_round_and_pow_match_the_overloads() {
    let source = r#"//@version=6
indicator("round and pow overloads")
int round_const = math.round(2.5)
int round_input = math.round(input.float(2.5))
int round_simple = math.round(timeframe.in_seconds() / 60.0)
int round_series = math.round(close)
float round_precision_const = math.round(2.567, 2)
float round_precision_input = math.round(input.float(2.567), 2)
float round_precision_simple = math.round(timeframe.in_seconds() / 60.0, 2)
float round_precision_series = math.round(close, bar_index)
float pow_const = math.pow(2.0, 3.0)
float pow_input = math.pow(input.float(2.0), input.float(3.0))
float pow_simple = math.pow(timeframe.in_seconds() / 60.0, timeframe.in_seconds() / 60.0)
float pow_series = math.pow(close, close)
plot(round_const)
plot(round_input)
plot(round_simple)
plot(round_series)
plot(round_precision_const)
plot(round_precision_input)
plot(round_precision_simple)
plot(round_precision_series)
plot(pow_const)
plot(pow_input)
plot(pow_simple)
plot(pow_series)
"#;
    let result = compile(source, CompileOptions::default());
    assert!(result.success, "{:?}", result.diagnostics);
}

#[test]
fn unary_math_and_power_reject_string_arguments() {
    for expression in [
        "math.abs(\"3\")",
        "math.ceil(\"3\")",
        "math.floor(\"3\")",
        "math.round(\"3\")",
        "math.sqrt(\"3\")",
        "math.log(\"3\")",
        "math.sin(\"3\")",
        "math.pow(\"3\", 2)",
        "math.pow(2)",
        "math.pow(2, 3, 4)",
    ] {
        let source = format!("//@version=6\nindicator(\"invalid math\")\nplot({expression})");
        let result = compile(&source, CompileOptions::default());
        assert!(!result.success, "unexpectedly accepted {expression}");
    }
}

#[test]
fn numeric_math_runtime_values_match_hand_computed_results() {
    let source = r#"//@version=6
indicator("math runtime values")
plot(math.abs(-3.25))
plot(math.ceil(-2.1))
plot(math.floor(-2.1))
plot(math.round(-2.5))
plot(math.round(2.567, 2))
plot(math.sqrt(9.0))
plot(math.log(1.0))
plot(math.sin(0.0))
plot(math.pow(2.0, 3.0))
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
            serde_json::json!([3.25, 3.25]),
            serde_json::json!([-2.0, -2.0]),
            serde_json::json!([-3.0, -3.0]),
            serde_json::json!([-3.0, -3.0]),
            serde_json::json!([2.57, 2.57]),
            serde_json::json!([3.0, 3.0]),
            serde_json::json!([0.0, 0.0]),
            serde_json::json!([0.0, 0.0]),
            serde_json::json!([8.0, 8.0]),
        ]
    );
}

#[test]
fn numeric_math_domain_errors_propagate_as_na() {
    let source = r#"//@version=6
indicator("math domain behavior")
plot(math.sqrt(-1.0))
plot(math.log(0.0))
plot(math.asin(2.0))
plot(math.acos(-2.0))
plot(math.pow(-2.0, 0.5))
"#;
    let market = MarketData {
        open: vec![1.0],
        high: vec![2.0],
        low: vec![0.0],
        close: vec![1.0],
        volume: vec![1.0],
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
    assert_eq!(result.plots.len(), 5);
    for plot in result.plots {
        assert_eq!(plot.values, [pinewasm_compiler::Value::Na]);
    }
}

#[test]
fn floor_and_ceil_propagate_missing_values_and_use_mathematical_direction() {
    let source = r#"//@version=6
indicator("floor and ceil na behavior")
plot(math.floor(close[1]))
plot(math.ceil(close[1]))
plot(math.floor(close))
plot(math.ceil(close))
"#;
    let market = MarketData {
        open: vec![-2.1, 2.1],
        high: vec![-2.0, 2.2],
        low: vec![-2.2, 2.0],
        close: vec![-2.1, 2.1],
        volume: vec![1.0; 2],
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
    let actual: Vec<_> = result.plots.iter().map(|plot| &plot.values).collect();
    assert_eq!(
        actual[0],
        &[
            pinewasm_compiler::Value::Na,
            pinewasm_compiler::Value::Float(-3.0)
        ]
    );
    assert_eq!(
        actual[1],
        &[
            pinewasm_compiler::Value::Na,
            pinewasm_compiler::Value::Float(-2.0)
        ]
    );
    assert_eq!(
        actual[2],
        &[
            pinewasm_compiler::Value::Float(-3.0),
            pinewasm_compiler::Value::Float(2.0)
        ]
    );
    assert_eq!(
        actual[3],
        &[
            pinewasm_compiler::Value::Float(-2.0),
            pinewasm_compiler::Value::Float(3.0)
        ]
    );
}
