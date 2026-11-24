//! Self-contained numerical contract tests for the public compiler pipeline.
//!
//! The expected values are mathematical identities, exactly representable IEEE
//! 754 results, or explicit error-domain rules defined by PineWASM.
//!
//! Each expression still travels through the real compile-and-run path. Inputs
//! arrive as `close` and `open`, preventing the assertions from collapsing into
//! tests of a Rust helper or constant folder. Low-level VM/AOT bit-for-bit
//! agreement remains covered by the runtime-numerics tests in those crates.

use pinewasm_compiler::{compile_and_run, CompileOptions, RunOptions};
use pinewasm_mir::Value;
use pinewasm_span::Severity;
use pinewasm_vm::MarketData;

#[derive(Clone, Copy, Debug)]
enum Expected {
    Exact(f64),
    Approx { value: f64, tolerance: f64 },
    Na,
}

#[derive(Clone, Copy, Debug)]
struct Case {
    x: f64,
    y: f64,
    expected: Expected,
}

struct Column<'a> {
    name: &'static str,
    /// The expression under test. `x` is `close`; `y` is `open`.
    expr: &'static str,
    cases: &'a [Case],
}

const fn exact(x: f64, y: f64, expected: f64) -> Case {
    Case {
        x,
        y,
        expected: Expected::Exact(expected),
    }
}

const fn approx(x: f64, y: f64, expected: f64, tolerance: f64) -> Case {
    Case {
        x,
        y,
        expected: Expected::Approx {
            value: expected,
            tolerance,
        },
    }
}

const fn na(x: f64, y: f64) -> Case {
    Case {
        x,
        y,
        expected: Expected::Na,
    }
}

fn evaluate_column(column: &Column<'_>) -> Vec<Option<f64>> {
    let closes: Vec<f64> = column.cases.iter().map(|case| case.x).collect();
    let opens: Vec<f64> = column.cases.iter().map(|case| case.y).collect();
    let bars = closes.len();
    let source = format!(
        "//@version=6\nindicator(\"numeric contract\")\nx = close\ny = open\nplot({})\n",
        column.expr
    );
    let market = MarketData {
        open: opens,
        high: closes.clone(),
        low: closes.clone(),
        close: closes,
        volume: vec![0.0; bars],
        time: (0..bars).map(|bar| bar as i64 * 60_000).collect(),
        mintick: 0.01,
        ..Default::default()
    };
    let result = compile_and_run(
        &source,
        &market,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    let errors: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == Severity::Error)
        .collect();
    assert!(
        errors.is_empty(),
        "{}: expression failed to compile: {errors:?}",
        column.name
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{}: expression failed at runtime: {:?}",
        column.name,
        result.runtime_errors
    );
    assert_eq!(result.plots.len(), 1, "{}: expected one plot", column.name);

    result.plots[0]
        .values
        .iter()
        .map(|value| match value {
            Value::Float(value) => Some(*value),
            Value::Int(value) => Some(*value as f64),
            Value::Na => None,
            unexpected => panic!(
                "{}: numeric expression produced non-numeric value {unexpected:?}",
                column.name
            ),
        })
        .collect()
}

fn assert_column(column: &Column<'_>) {
    let actual = evaluate_column(column);
    assert_eq!(
        actual.len(),
        column.cases.len(),
        "{}: bar count",
        column.name
    );
    for (index, (case, found)) in column.cases.iter().zip(actual).enumerate() {
        match (case.expected, found) {
            (Expected::Na, None) => {}
            (Expected::Exact(expected), Some(found)) => assert_eq!(
                found.to_bits(),
                expected.to_bits(),
                "{} case {index}, x={:?}, y={:?}",
                column.name,
                case.x,
                case.y
            ),
            (Expected::Approx { value, tolerance }, Some(found)) => {
                let error = (found - value).abs();
                assert!(
                    error <= tolerance,
                    "{} case {index}, x={:?}, y={:?}: expected {value:?} +/- {tolerance:e}, got {found:?} (error {error:e})",
                    column.name,
                    case.x,
                    case.y
                );
            }
            (expected, found) => panic!(
                "{} case {index}, x={:?}, y={:?}: expected {expected:?}, got {found:?}",
                column.name, case.x, case.y
            ),
        }
    }
}

#[test]
fn transcendental_reference_points_follow_mathematical_identities() {
    let columns = [
        Column {
            name: "exp",
            expr: "math.exp(x)",
            cases: &[
                exact(0.0, 0.0, 1.0),
                approx(1.0, 0.0, core::f64::consts::E, 2.0e-15),
                approx(-1.0, 0.0, 1.0 / core::f64::consts::E, 2.0e-15),
            ],
        },
        Column {
            name: "log",
            expr: "math.log(x)",
            cases: &[
                exact(1.0, 0.0, 0.0),
                approx(core::f64::consts::E, 0.0, 1.0, 2.0e-15),
                na(0.0, 0.0),
                na(-1.0, 0.0),
            ],
        },
        Column {
            name: "log10",
            expr: "math.log10(x)",
            cases: &[
                exact(1.0, 0.0, 0.0),
                exact(10.0, 0.0, 1.0),
                approx(1_000.0, 0.0, 3.0, 1.0e-15),
            ],
        },
        Column {
            name: "sin",
            expr: "math.sin(x)",
            cases: &[
                exact(0.0, 0.0, 0.0),
                approx(core::f64::consts::FRAC_PI_2, 0.0, 1.0, 1.0e-15),
                approx(-core::f64::consts::FRAC_PI_2, 0.0, -1.0, 1.0e-15),
            ],
        },
        Column {
            name: "cos",
            expr: "math.cos(x)",
            cases: &[
                exact(0.0, 0.0, 1.0),
                approx(core::f64::consts::PI, 0.0, -1.0, 1.0e-15),
            ],
        },
        Column {
            name: "tan",
            expr: "math.tan(x)",
            cases: &[
                exact(0.0, 0.0, 0.0),
                approx(core::f64::consts::FRAC_PI_4, 0.0, 1.0, 1.0e-15),
            ],
        },
        Column {
            name: "asin",
            expr: "math.asin(x)",
            cases: &[
                exact(0.0, 0.0, 0.0),
                approx(1.0, 0.0, core::f64::consts::FRAC_PI_2, 1.0e-15),
                na(2.0, 0.0),
            ],
        },
        Column {
            name: "acos",
            expr: "math.acos(x)",
            cases: &[
                exact(1.0, 0.0, 0.0),
                approx(0.0, 0.0, core::f64::consts::FRAC_PI_2, 1.0e-15),
                na(2.0, 0.0),
            ],
        },
        Column {
            name: "atan",
            expr: "math.atan(x)",
            cases: &[
                exact(0.0, 0.0, 0.0),
                approx(1.0, 0.0, core::f64::consts::FRAC_PI_4, 1.0e-15),
                approx(-1.0, 0.0, -core::f64::consts::FRAC_PI_4, 1.0e-15),
            ],
        },
        Column {
            name: "pow",
            expr: "math.pow(x, y)",
            cases: &[
                exact(2.0, 0.0, 1.0),
                exact(2.0, 10.0, 1_024.0),
                approx(4.0, 0.5, 2.0, 1.0e-15),
                na(-1.0, 0.5),
            ],
        },
        Column {
            name: "sqrt",
            expr: "math.sqrt(x)",
            cases: &[
                exact(0.0, 0.0, 0.0),
                exact(4.0, 0.0, 2.0),
                approx(2.0, 0.0, core::f64::consts::SQRT_2, 1.0e-15),
                na(-1.0, 0.0),
            ],
        },
    ];

    for column in &columns {
        assert_column(column);
    }
}

#[test]
fn rounding_remainder_and_angle_conversions_follow_the_public_contract() {
    let columns = [
        Column {
            name: "round-half-away-from-zero",
            expr: "math.round(x)",
            cases: &[
                exact(2.5, 0.0, 3.0),
                exact(-2.5, 0.0, -3.0),
                exact(2.49, 0.0, 2.0),
                exact(-2.49, 0.0, -2.0),
            ],
        },
        Column {
            name: "round-two-decimals",
            expr: "math.round(x, 2)",
            cases: &[
                exact(1.234, 0.0, 1.23),
                exact(1.235, 0.0, 1.24),
                exact(-1.235, 0.0, -1.24),
            ],
        },
        Column {
            name: "runtime-negative-precision-clamps-to-zero",
            expr: "math.round(x, -int(bar_index) - 1)",
            cases: &[
                exact(12.4, 0.0, 12.0),
                exact(12.5, 0.0, 13.0),
                exact(-12.5, 0.0, -13.0),
            ],
        },
        Column {
            name: "remainder-sign",
            expr: "x % y",
            cases: &[
                exact(5.5, 2.0, 1.5),
                exact(-5.5, 2.0, -1.5),
                exact(5.5, -2.0, 1.5),
            ],
        },
        Column {
            name: "to-radians",
            expr: "math.toradians(x)",
            cases: &[
                exact(0.0, 0.0, 0.0),
                approx(180.0, 0.0, core::f64::consts::PI, 1.0e-15),
                approx(-90.0, 0.0, -core::f64::consts::FRAC_PI_2, 1.0e-15),
            ],
        },
        Column {
            name: "to-degrees",
            expr: "math.todegrees(x)",
            cases: &[
                exact(0.0, 0.0, 0.0),
                approx(core::f64::consts::PI, 0.0, 180.0, 1.0e-13),
                approx(-core::f64::consts::FRAC_PI_2, 0.0, -90.0, 1.0e-13),
            ],
        },
    ];

    for column in &columns {
        assert_column(column);
    }
}

#[test]
fn composed_identities_hold_over_representative_finite_inputs() {
    let columns = [
        Column {
            name: "exp-log-round-trip",
            expr: "math.exp(math.log(x))",
            cases: &[
                approx(0.125, 0.0, 0.125, 2.0e-15),
                exact(1.0, 0.0, 1.0),
                approx(10.0, 0.0, 10.0, 2.0e-14),
            ],
        },
        Column {
            name: "unit-circle",
            expr: "math.pow(math.sin(x), 2) + math.pow(math.cos(x), 2)",
            cases: &[
                approx(-10.0, 0.0, 1.0, 2.0e-15),
                approx(-1.0, 0.0, 1.0, 2.0e-15),
                exact(0.0, 0.0, 1.0),
                approx(1.0, 0.0, 1.0, 2.0e-15),
                approx(10.0, 0.0, 1.0, 2.0e-15),
            ],
        },
        Column {
            name: "degree-radian-round-trip",
            expr: "math.toradians(math.todegrees(x))",
            cases: &[
                approx(-core::f64::consts::PI, 0.0, -core::f64::consts::PI, 1.0e-15),
                exact(0.0, 0.0, 0.0),
                approx(0.25, 0.0, 0.25, 1.0e-15),
            ],
        },
    ];

    for column in &columns {
        assert_column(column);
    }
}

#[test]
fn non_finite_results_are_normalized_to_na() {
    let columns = [
        Column {
            name: "division-by-zero",
            expr: "x / y",
            cases: &[na(0.0, 0.0), na(1.0, 0.0), na(-1.0, 0.0)],
        },
        Column {
            name: "overflow",
            expr: "math.exp(x)",
            cases: &[na(800.0, 0.0)],
        },
        Column {
            name: "na-propagation",
            expr: "math.sin(x / y)",
            cases: &[na(1.0, 0.0)],
        },
    ];

    for column in &columns {
        assert_column(column);
    }
}

#[test]
fn integer_division_preserves_fractional_cone_layers() {
    let market = MarketData {
        open: vec![1.0],
        high: vec![1.0],
        low: vec![1.0],
        close: vec![1.0],
        volume: vec![0.0],
        ..Default::default()
    };
    let source = r#"//@version=6
indicator("fractional layers")
layers = input.int(4)
plot(3 / 4)
plot(input.float(3 / 4))
for j = layers to 1
    float width = (j / layers) * 2.0
    box.new(0, width, 1, -width)
"#;
    let result = compile_and_run(
        source,
        &market,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result
            .diagnostics
            .iter()
            .all(|d| d.severity != Severity::Error),
        "{:?}",
        result.diagnostics
    );
    assert!(result.runtime_errors.is_empty());
    assert_eq!(result.plots[0].values, vec![Value::Float(0.75)]);
    assert_eq!(result.plots[1].values, vec![Value::Float(0.75)]);
    let tops: Vec<_> = result
        .drawings
        .boxes
        .iter()
        .map(|b| b.fields["top"].clone())
        .collect();
    assert_eq!(
        tops,
        vec![
            Value::Float(2.0),
            Value::Float(1.5),
            Value::Float(1.0),
            Value::Float(0.5)
        ]
    );
}
