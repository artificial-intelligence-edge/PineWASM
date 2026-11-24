//! Coverage for the remaining `math.*` overloads.
//!
//! Covers `math.sign`, `math.round_to_mintick`, `math.random`, `math.sum`,
//! `math.todegrees` and `math.toradians`. The VM tests also check random seeding,
//! mintick rounding, angular conversion, and rolling sums.

use pinewasm_compiler::{compile, compile_and_run, CompileOptions, MarketData, RunOptions};

#[test]
fn sign_and_mintick_rounding_match_qualifier_overloads() {
    let source = r#"//@version=6
indicator("remaining math qualifiers")
float sign_const = math.sign(-3)
float sign_input = math.sign(input.int(2))
float sign_simple = math.sign(timeframe.in_seconds())
float sign_series = math.sign(close)
float tick_const = math.round_to_mintick(1.23)
float tick_input = math.round_to_mintick(input.float(1.24))
float tick_simple = math.round_to_mintick(timeframe.in_seconds() / 60.0)
float tick_series = math.round_to_mintick(close)
plot(sign_const)
plot(sign_input)
plot(sign_simple)
plot(sign_series)
plot(tick_const)
plot(tick_input)
plot(tick_simple)
plot(tick_series)
"#;
    let result = compile(source, CompileOptions::default());
    assert!(result.success, "{:?}", result.diagnostics);

    let invalid = compile(
        "//@version=6\nindicator(\"invalid math type\")\nplot(math.sign(\"1\"))",
        CompileOptions::default(),
    );
    assert!(!invalid.success);
}

#[test]
fn angle_conversion_and_rolling_sum_accept_numeric_types() {
    let source = r#"//@version=6
indicator("remaining math functions")
degrees = math.todegrees(math.pi)
radians = math.toradians(180.0)
total = math.sum(close, 2)
plot(degrees)
plot(radians)
plot(total)
"#;
    let result = compile(source, CompileOptions::default());
    assert!(result.success, "{:?}", result.diagnostics);
    let invalid = compile(
        "//@version=6\nindicator(\"invalid sum length\")\nplot(math.sum(close, 1.5))",
        CompileOptions::default(),
    );
    assert!(!invalid.success);
}

#[test]
fn random_optional_arguments_are_seeded_and_bounded() {
    let source = r#"//@version=6
indicator("random overloads")
plot(math.random())
plot(math.random(2.0, 5.0))
plot(math.random(0, 10, 7))
plot(math.random(max = 3.0))
plot(math.random(min = 0.2))
"#;
    let market = MarketData {
        open: vec![1.0; 8],
        high: vec![2.0; 8],
        low: vec![0.0; 8],
        close: vec![1.23, 1.24, 1.25, 1.26, 1.27, 1.28, 1.29, 1.30],
        volume: vec![1.0; 8],
        mintick: 0.05,
        ..MarketData::default()
    };
    let run = || {
        compile_and_run(
            source,
            &market,
            CompileOptions::default(),
            &RunOptions::default(),
        )
    };
    let first = run();
    let second = run();
    assert!(first
        .compile
        .as_ref()
        .is_some_and(|compile| compile.success));
    assert!(
        first.runtime_errors.is_empty(),
        "{:?}",
        first.runtime_errors
    );
    assert_eq!(first.plots[2].values, second.plots[2].values);
    assert!(first.plots[0]
        .values
        .iter()
        .all(|value| matches!(value, pinewasm_compiler::Value::Float(n) if *n > 0.0 && *n < 1.0)));
    assert!(first.plots[1]
        .values
        .iter()
        .all(|value| matches!(value, pinewasm_compiler::Value::Float(n) if *n > 2.0 && *n < 5.0)));
    assert!(first.plots[3]
        .values
        .iter()
        .all(|value| matches!(value, pinewasm_compiler::Value::Float(n) if *n > 0.0 && *n < 3.0)));
    assert!(first.plots[4]
        .values
        .iter()
        .all(|value| matches!(value, pinewasm_compiler::Value::Float(n) if *n > 0.2 && *n < 1.0)));
}

#[test]
fn random_accepts_named_arguments_and_series_bounds() {
    let source = r#"//@version=6
indicator("random named and series arguments")
float from_series = math.random(max = high, min = low, seed = bar_index)
float from_input = math.random(min = input.float(2.0), max = input.float(5.0))
plot(from_series)
plot(from_input)
"#;
    let market = MarketData {
        open: vec![10.0, 20.0, 30.0],
        high: vec![12.0, 22.0, 32.0],
        low: vec![8.0, 18.0, 28.0],
        close: vec![10.0, 20.0, 30.0],
        volume: vec![1.0; 3],
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
    for (value, (low, high)) in
        result.plots[0]
            .values
            .iter()
            .zip([(8.0, 12.0), (18.0, 22.0), (28.0, 32.0)])
    {
        let pinewasm_compiler::Value::Float(value) = value else {
            panic!("expected finite random value, got {value:?}");
        };
        assert!(
            *value > low && *value < high,
            "{value} not in ({low}, {high})"
        );
    }
    assert!(result.plots[1]
        .values
        .iter()
        .all(|value| matches!(value, pinewasm_compiler::Value::Float(n) if *n > 2.0 && *n < 5.0)));

    for (ty, expression) in [
        ("input float", "math.random(input.float(1.0), 2.0)"),
        ("simple float", "math.random(timeframe.in_seconds(), 2.0)"),
        ("series float", "math.random(close, 2.0)"),
    ] {
        let source = format!(
            "//@version=6\nindicator(\"random qualifier\")\nfloat value = {expression}\nplot(value)"
        );
        let result = compile(&source, CompileOptions::default());
        assert!(result.success, "{ty}: {:?}", result.diagnostics);
    }
    let simple_rejects_series = compile(
        "//@version=6\nindicator(\"random qualifier\")\nsimple float value = math.random(close, 2.0)\nplot(value)",
        CompileOptions::default(),
    );
    assert!(!simple_rejects_series.success);
}

#[test]
fn mintick_rounding_angles_and_rolling_sum_match_hand_computed_values() {
    let source = r#"//@version=6
indicator("remaining math values")
plot(math.round_to_mintick(close))
plot(math.todegrees(math.pi))
plot(math.toradians(180.0))
plot(math.sum(close, 2))
"#;
    let market = MarketData {
        open: vec![1.0; 3],
        high: vec![2.0; 3],
        low: vec![0.0; 3],
        close: vec![1.23, 1.26, 1.31],
        volume: vec![1.0; 3],
        mintick: 0.05,
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
    let values: Vec<Vec<_>> = result
        .plots
        .iter()
        .map(|plot| {
            plot.values
                .iter()
                .map(|value| match value {
                    pinewasm_compiler::Value::Float(value) => *value,
                    _ => f64::NAN,
                })
                .collect()
        })
        .collect();
    assert_eq!(values[0], [1.25, 1.25, 1.30]);
    assert_eq!(values[1], [180.0; 3]);
    assert_eq!(values[2], [std::f64::consts::PI; 3]);
    assert!(values[3][0].is_nan());
    assert!((values[3][1] - 2.49).abs() < 1e-12);
    assert!((values[3][2] - 2.57).abs() < 1e-12);
}
