//! End-to-end tests for named-argument calls.
//!
//! Named arguments are a front-end concern: they are resolved to positional
//! bindings during semantic analysis, so a named-argument call must produce
//! byte-identical runtime output to the equivalent positional call, and the
//! binding diagnostics (E0228) must surface through the public compile API.

use pinewasm_compiler::{compile, compile_and_run, CompileOptions, MarketData, RunOptions};

fn data(close: Vec<f64>) -> MarketData {
    let n = close.len();
    MarketData {
        open: close.clone(),
        high: close.clone(),
        low: close.clone(),
        close,
        volume: vec![0.0; n],
        ..MarketData::default()
    }
}

/// Compile and run, returning each plot's values as a JSON string. Panics on
/// any compile or runtime error so a regression is loud.
fn run(src: &str, close: Vec<f64>) -> Vec<String> {
    let r = compile_and_run(
        src,
        &data(close),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        r.runtime_errors.is_empty(),
        "runtime errors: {:?}",
        r.runtime_errors
    );
    assert!(
        r.compile.as_ref().is_some_and(|c| c.success),
        "compile failed: {:?}",
        r.diagnostics
    );
    r.plots
        .iter()
        .map(|p| serde_json::to_string(&p.values).unwrap())
        .collect()
}

fn error_codes(src: &str) -> Vec<String> {
    compile(src, CompileOptions::all())
        .diagnostics
        .iter()
        .filter(|d| d.is_error())
        .filter_map(|d| d.code.clone())
        .collect()
}

#[test]
fn timeframe_seconds_default_tracks_market_timeframe() {
    let source = r#"//@version=6
indicator("Chart duration")
plot(timeframe.in_seconds())
plot(timeframe.in_seconds(timeframe = timeframe.period))
plot(timeframe.in_seconds("30S"))
"#;
    for (timeframe, seconds) in [("5", 300), ("240", 14_400), ("1D", 86_400), ("1W", 604_800)] {
        let mut market = data(vec![10.0, 11.0]);
        market.timeframe = timeframe.to_string();
        let result = compile_and_run(
            source,
            &market,
            CompileOptions::default(),
            &RunOptions::default(),
        );
        assert!(
            result.compile.as_ref().is_some_and(|c| c.success),
            "{:?}",
            result.diagnostics
        );
        assert!(
            result.runtime_errors.is_empty(),
            "{:?}",
            result.runtime_errors
        );
        let values: Vec<_> = result
            .plots
            .iter()
            .map(|plot| serde_json::to_string(&plot.values).unwrap())
            .collect();
        assert_eq!(
            values,
            vec![
                format!("[{seconds}.0,{seconds}.0]"),
                format!("[{seconds}.0,{seconds}.0]"),
                "[30.0,30.0]".to_string()
            ]
        );
    }
}

#[test]
fn named_length_matches_positional_output() {
    // ta.sma(close, length = 3) must compute exactly ta.sma(close, 3).
    let named = run(
        "//@version=6\nindicator(\"x\")\nplot(ta.sma(close, length = 3))\n",
        vec![10.0, 20.0, 30.0, 40.0],
    );
    let positional = run(
        "//@version=6\nindicator(\"x\")\nplot(ta.sma(close, 3))\n",
        vec![10.0, 20.0, 30.0, 40.0],
    );
    assert_eq!(named[0], positional[0]);
    // sma3 of [10,20,30,40] = [na, na, 20, 30].
    assert_eq!(named[0], "[null,null,20.0,30.0]");
}

#[test]
fn named_matrix_new_matches_positional_output() {
    // matrix.new<T> binds rows/columns/initial_value by name, so the reordered
    // named constructor must build exactly the matrix of the positional call.
    let program = |ctor: &str| {
        format!(
            "//@version=6\nindicator(\"x\")\nm = {ctor}\n\
             plot(matrix.rows(m))\nplot(matrix.columns(m))\nplot(matrix.get(m, 1, 2))\n"
        )
    };
    for (named_ctor, positional_ctor, expected) in [
        (
            "matrix.new<float>(columns = 3, rows = 2, initial_value = 1.5)",
            "matrix.new<float>(2, 3, 1.5)",
            ["[2.0,2.0]", "[3.0,3.0]", "[1.5,1.5]"],
        ),
        (
            "matrix.new<int>(initial_value = 7, rows = 2, columns = 3)",
            "matrix.new<int>(2, 3, 7)",
            ["[2.0,2.0]", "[3.0,3.0]", "[7.0,7.0]"],
        ),
    ] {
        let named = run(&program(named_ctor), vec![1.0, 2.0]);
        let positional = run(&program(positional_ctor), vec![1.0, 2.0]);
        assert_eq!(named, positional, "{named_ctor} vs {positional_ctor}");
        assert_eq!(named, expected, "{named_ctor}");
    }
}

#[test]
fn all_named_and_reordered_matches_positional() {
    // Both parameters named, in reverse order.
    let reordered = run(
        "//@version=6\nindicator(\"x\")\nplot(ta.sma(length = 2, source = close))\n",
        vec![2.0, 4.0, 6.0, 8.0],
    );
    let positional = run(
        "//@version=6\nindicator(\"x\")\nplot(ta.sma(close, 2))\n",
        vec![2.0, 4.0, 6.0, 8.0],
    );
    assert_eq!(reordered[0], positional[0]);
    // sma2 of [2,4,6,8] = [na, 3, 5, 7].
    assert_eq!(reordered[0], "[null,3.0,5.0,7.0]");
}

#[test]
fn named_arguments_on_user_function_match_positional() {
    // A user function called with reversed named arguments matches the
    // positional call: f(a, b) => a - b, so f(b=close, a=open) == open - close.
    let named = run(
        "//@version=6\n\
         indicator(\"x\")\n\
         f(a, b) =>\n    \
             a - b\n\
         plot(f(b = close, a = high))\n",
        vec![1.0, 2.0, 3.0],
    );
    // open/high/low all equal close in `data`, so open - close == 0 everywhere.
    assert_eq!(named[0], "[0.0,0.0,0.0]");
}

#[test]
fn unknown_named_parameter_surfaces_e0228() {
    let codes = error_codes("//@version=6\nindicator(\"x\")\nplot(ta.sma(close, lenght = 3))\n");
    assert!(codes.contains(&"E0228".to_string()), "codes: {codes:?}");
}

#[test]
fn duplicate_named_binding_surfaces_e0228() {
    let codes = error_codes(
        "//@version=6\nindicator(\"x\")\nplot(ta.sma(close, source = open, length = 3))\n",
    );
    assert!(codes.contains(&"E0228".to_string()), "codes: {codes:?}");
}

#[test]
fn positional_after_named_surfaces_a_parse_error() {
    // A positional argument after a named one is rejected at parse time; the
    // compile must fail (some error is reported).
    let r = compile(
        "//@version=6\nindicator(\"x\")\nplot(ta.sma(source = close, 3))\n",
        CompileOptions::all(),
    );
    assert!(
        r.diagnostics
            .iter()
            .any(pinewasm_span::Diagnostic::is_error),
        "expected a parse error: {:?}",
        r.diagnostics
    );
}
