//! Positive and negative coverage for variadic numeric overloads.
//!
//! Covers the `math.min`, `math.max` and `math.avg` overloads. These compiler
//! tests cover call binding and qualifier/type acceptance. Runtime values are
//! asserted separately in `pinewasm_vm` tests.
use pinewasm_compiler::{compile, compile_and_run, CompileOptions, MarketData, RunOptions};

#[test]
fn min_max_match_all_numeric_type_and_qualifier_overloads() {
    let expressions = [
        ("int", "math.min(3, 1)"),
        ("float", "math.min(3.5, 1.5)"),
        ("int", "math.min(input.int(3), 1)"),
        ("float", "math.min(input.float(3.5), 1.5)"),
        ("int", "math.min(timeframe.in_seconds(), 1)"),
        ("float", "math.min(timeframe.in_seconds() / 60.0, 1.5)"),
        ("int", "math.min(bar_index, 1)"),
        ("float", "math.min(close, 1.5)"),
        ("int", "math.max(3, 1)"),
        ("float", "math.max(3.5, 1.5)"),
        ("int", "math.max(input.int(3), 1)"),
        ("float", "math.max(input.float(3.5), 1.5)"),
        ("int", "math.max(timeframe.in_seconds(), 1)"),
        ("float", "math.max(timeframe.in_seconds() / 60.0, 1.5)"),
        ("int", "math.max(bar_index, 1)"),
        ("float", "math.max(close, 1.5)"),
    ];
    let body = expressions
        .iter()
        .enumerate()
        .map(|(index, (ty, expression))| {
            format!("{ty} value_{index} = {expression}\nplot(value_{index})\n")
        })
        .collect::<String>();
    let source = format!("//@version=6\nindicator(\"min/max v6 overloads\")\n{body}");
    let result = compile(&source, CompileOptions::default());
    assert!(result.success, "{source}\n{:?}", result.diagnostics);
}

#[test]
fn avg_matches_the_simple_and_series_numeric_overloads() {
    let source = r#"//@version=6
indicator("avg v6 overloads")
float const_result = math.avg(2, 4)
float simple_result = math.avg(timeframe.in_seconds(), 4)
float series_result = math.avg(close, 4)
plot(const_result)
plot(simple_result)
plot(series_result)
"#;
    let result = compile(source, CompileOptions::default());
    assert!(result.success, "{:?}", result.diagnostics);
}

#[test]
fn variadic_result_qualifiers_match_simple_and_series_contexts() {
    for expression in [
        "math.min(timeframe.in_seconds(), 4)",
        "math.max(timeframe.in_seconds(), 4)",
        "math.avg(timeframe.in_seconds(), 4)",
    ] {
        let source = format!(
            "//@version=6\nindicator(\"simple qualifier\")\nf(simple float value) => value\nplot(f({expression}))"
        );
        let result = compile(&source, CompileOptions::default());
        assert!(result.success, "{expression}: {:?}", result.diagnostics);
    }

    for expression in [
        "math.min(close, 4)",
        "math.max(close, 4)",
        "math.avg(close, 4)",
    ] {
        let source = format!(
            "//@version=6\nindicator(\"series qualifier\")\nf(simple float value) => value\nplot(f({expression}))"
        );
        let result = compile(&source, CompileOptions::default());
        assert!(
            !result.success,
            "series result unexpectedly satisfied a simple parameter: {expression}"
        );
    }
}

#[test]
fn variadic_numeric_functions_reject_invalid_arity_and_argument_types() {
    for expression in [
        "math.min(1)",
        "math.min()",
        "math.max(1)",
        "math.max()",
        "math.avg(1)",
        "math.avg()",
        "math.min(\"1\", 2)",
        "math.max(\"1\", 2)",
        "math.avg(\"1\", 2)",
    ] {
        let source = format!("//@version=6\nindicator(\"invalid overload\")\nplot({expression})");
        let result = compile(&source, CompileOptions::default());
        assert!(!result.success, "unexpectedly accepted {expression}");
    }
}

#[test]
fn variadic_numeric_overloads_match_hand_computed_multibar_values() {
    let source = r#"//@version=6
indicator("variadic runtime values")
plot(math.min(3, 1, 2))
plot(math.max(3, 1, 2))
plot(math.min(3.5, 1.5, 2.5))
plot(math.max(3.5, 1.5, 2.5))
plot(math.avg(3, 1, 2))
plot(math.min(close, 4))
plot(math.max(bar_index, 1))
"#;
    let market = MarketData {
        open: vec![2.0, 5.0],
        high: vec![3.0, 6.0],
        low: vec![1.0, 4.0],
        close: vec![2.0, 5.0],
        volume: vec![10.0, 11.0],
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
            serde_json::json!([1.0, 1.0]),
            serde_json::json!([3.0, 3.0]),
            serde_json::json!([1.5, 1.5]),
            serde_json::json!([3.5, 3.5]),
            serde_json::json!([2.0, 2.0]),
            serde_json::json!([2.0, 4.0]),
            serde_json::json!([1.0, 1.0]),
        ]
    );
}
