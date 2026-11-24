//! Targeted overload coverage for TA windows that take series
//! integer lengths and type-preserving integer results.
use pinewasm_compiler::{compile, compile_and_run, CompileOptions, MarketData, RunOptions};

fn accepts(body: &str) {
    let source = format!("//@version=6\nindicator(\"TA overloads\")\n{body}\n");
    let result = compile(&source, CompileOptions::default());
    assert!(result.success, "{body}: {:?}", result.diagnostics);
}

fn rejects(body: &str) {
    let source = format!("//@version=6\nindicator(\"TA overloads\")\n{body}\n");
    let result = compile(&source, CompileOptions::default());
    assert!(!result.success, "unexpectedly accepted {body}");
}

#[test]
fn median_mode_and_range_accept_the_integer_result_overloads() {
    accepts("int median = ta.median(bar_index, 1)\nplot(median)");
    accepts("int mode = ta.mode(bar_index, 1)\nplot(mode)");
    accepts("int span = ta.range(bar_index, 1)\nplot(span)");
}

#[test]
fn median_mode_and_range_accept_series_int_lengths() {
    for call in [
        "ta.median(close, bar_index % 3 + 1)",
        "ta.mode(close, bar_index % 3 + 1)",
        "ta.range(close, bar_index % 3 + 1)",
    ] {
        accepts(&format!("plot({call})"));
    }
}

#[test]
fn dynamic_median_mode_and_range_match_multibar_windows() {
    let source = r#"//@version=6
indicator("dynamic statistic windows")
int mode_value = ta.mode(bar_index, bar_index % 3 + 1)
int range_value = ta.range(bar_index, bar_index % 3 + 1)
float median_value = ta.median(close, bar_index % 3 + 1)
plot(mode_value)
plot(range_value)
plot(median_value)
"#;
    let market = MarketData {
        open: vec![2.0, 4.0, 2.0, 3.0],
        high: vec![3.0, 5.0, 3.0, 4.0],
        low: vec![1.0, 3.0, 1.0, 2.0],
        close: vec![2.0, 4.0, 1.0, 3.0],
        volume: vec![10.0; 4],
        ..MarketData::default()
    };
    let result = compile_and_run(
        source,
        &market,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result
            .compile
            .as_ref()
            .is_some_and(|compile| compile.success),
        "{:?}",
        result.compile.as_ref().map(|c| &c.diagnostics)
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    let values: Vec<_> = result
        .plots
        .iter()
        .map(|plot| serde_json::to_value(&plot.values).unwrap())
        .collect();
    assert_eq!(
        values,
        vec![
            serde_json::json!([0.0, 0.0, 0.0, 3.0]),
            serde_json::json!([0.0, 1.0, 2.0, 0.0]),
            serde_json::json!([2.0, 3.0, 2.0, 3.0]),
        ]
    );
}

#[test]
fn dynamic_statistic_lengths_enforce_the_window_bounds_at_runtime() {
    let source =
        "//@version=6\nindicator(\"invalid dynamic length\")\nplot(ta.range(close, bar_index))\n";
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
    assert_eq!(result.runtime_errors.len(), 1);
    assert!(result.runtime_errors[0]
        .message
        .contains("int from 1 to 100000"));
}

#[test]
fn window_length_overloads_still_reject_non_integer_lengths() {
    rejects("plot(ta.median(close, 2.5))");
    rejects("plot(ta.mode(close, true))");
    rejects("plot(ta.range(close, \"3\"))");
    rejects("plot(ta.median(close, close))");
}

#[test]
fn valuewhen_accepts_bool_and_color_source_overloads() {
    accepts(
        "bool remembered = ta.valuewhen(close > open, close > open, 0)\nplot(remembered ? 1 : 0)",
    );
    accepts(
        "color remembered = ta.valuewhen(close > open, close > open ? color.red : color.blue, 0)\nplot(close, color=remembered)",
    );
}

#[test]
fn valuewhen_bool_and_color_overloads_preserve_runtime_values() {
    let source = r#"//@version=6
indicator("valuewhen bool and color")
bool remembered_bool = ta.valuewhen(close > open, close > 2, 0)
color remembered_color = ta.valuewhen(close > open, close > 2 ? color.red : color.blue, 0)
plot(remembered_bool ? 1 : 0)
plot(close, color=remembered_color)
"#;
    let market = MarketData {
        open: vec![0.0, 4.0, 4.0, 3.0],
        high: vec![2.0, 5.0, 6.0, 4.0],
        low: vec![0.0, 2.0, 3.0, 1.0],
        close: vec![1.0, 3.0, 5.0, 2.0],
        volume: vec![1.0; 4],
        ..MarketData::default()
    };
    let result = compile_and_run(
        source,
        &market,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result
            .compile
            .as_ref()
            .is_some_and(|compile| compile.success),
        "{:?}",
        result.compile.as_ref().map(|compile| &compile.diagnostics)
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    let bool_values: Vec<_> = result.plots[0]
        .values
        .iter()
        .map(|value| serde_json::to_value(value).unwrap())
        .collect();
    assert_eq!(bool_values, [0.0, 0.0, 1.0, 1.0]);
    assert_eq!(
        result.plots[1].colors[0].values,
        [
            pinewasm_compiler::Value::Color {
                r: 41,
                g: 98,
                b: 255,
                t: 0.0,
                input_origin: None
            },
            pinewasm_compiler::Value::Color {
                r: 41,
                g: 98,
                b: 255,
                t: 0.0,
                input_origin: None
            },
            pinewasm_compiler::Value::Color {
                r: 242,
                g: 54,
                b: 69,
                t: 0.0,
                input_origin: None
            },
            pinewasm_compiler::Value::Color {
                r: 242,
                g: 54,
                b: 69,
                t: 0.0,
                input_origin: None
            },
        ]
    );
}
