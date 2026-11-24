//! Data-independent language regression fixtures.
//!
//! Every expected bit pattern below is produced by running the checked-in
//! scripts over [`synthetic_data`]. They pin PineWASM's behavior.

use pinewasm_compiler::{
    compile_and_run, CompileOptions, MarketData, PlotSeries, RunOptions, Value,
};

const BARS: usize = 60;

#[derive(Clone, Copy)]
struct ExpectedPlot {
    title: &'static str,
    finite: usize,
    final_bits: u64,
}

const fn plot(title: &'static str, finite: usize, final_bits: u64) -> ExpectedPlot {
    ExpectedPlot {
        title,
        finite,
        final_bits,
    }
}

const INDICATOR_EXPECTATIONS: &[ExpectedPlot] = &[
    plot("lang_core_checks", 60, 0x40be_3400_0000_0000),
    plot("lang_core_acc", 60, 0x410d_d6d0_0000_0000),
    plot("mc shape", 60, 0x3ff0_0000_0000_0000),
    plot("mc shape num", 60, 0x3fd0_0000_0000_0000),
    plot("mc char", 60, 0x3ff0_0000_0000_0000),
    plot("mc char num", 60, 0x3fd0_0000_0000_0000),
    plot("mc arrow", 60, 0x3fd0_0000_0000_0000),
    plot("mc candle.open", 60, 0x4060_3000_0000_0000),
    plot("mc candle.high", 60, 0x4060_5000_0000_0000),
    plot("mc candle.low", 60, 0x4060_1000_0000_0000),
    plot("mc candle.close", 60, 0x4060_3800_0000_0000),
    plot("mc bar.open", 60, 0x4060_3000_0000_0000),
    plot("mc bar.high", 60, 0x4060_5000_0000_0000),
    plot("mc bar.low", 60, 0x4060_1000_0000_0000),
    plot("mc bar.close", 60, 0x4060_3800_0000_0000),
    plot("mc hline", 60, 0x0000_0000_0000_0000),
    plot("mc fill ceiling", 60, 0x3ff0_0000_0000_0000),
    plot("mc acc", 60, 0x40ed_34c0_0000_0007),
    plot("mc checks", 60, 0x40b1_1c00_0000_0000),
    // The two ta.kcw contributions are ratios, not percentages. On this dense
    // synthetic feed each finite value is 8 / EMA20(close), ending at 0.064.
    // Scaling only those contributions by 100 yields the accumulator lane
    // bit for bit; every other fixture lane is unchanged.
    plot("ta accumulator", 60, 0x4144_487c_d6d8_4e7d),
    plot("ta checks passed", 60, 0x40a3_3800_0000_0000),
    plot("math_str_color_acc", 60, 0x40fe_1187_430f_b32e),
    // This script checks color.blue against #2196F3, but PineWASM's built-in
    // color.blue is #2962FF, so that condition is false. Two fractional const
    // color.t conditions are also false: numeric defaults truncate RGB 0.4 to 0
    // and color.new 49.6 to 49. The variant scripts below check these three
    // conditions separately and together, leaving every other plotted result
    // unchanged.
    plot("math_str_color_checks", 60, 0x4057_8000_0000_0000),
    plot("array-map accumulator", 60, 0x40c7_4943_9a01_d1a4),
    plot("array-map checks passed", 60, 0x40b2_fc00_0000_0000),
    plot("matrix accumulator", 60, 0x40d8_d800_0000_0000),
    plot("matrix checks passed", 60, 0x40b4_6400_0000_0000),
    plot("drawings_acc", 60, 0x4051_4000_0000_0000),
    plot("drawings_checks", 60, 0x4051_4000_0000_0000),
    // Drawing strings shorten this sum by 304 per bar; formatting checks
    // replace 50 string characters with three assertions per bar.
    // The same terminal flag contributes 16 here; its truth-table check remains true.
    plot("context_constants_acc", 60, 0x40f9_1a39_9999_9993),
    plot("context_constants_checks", 60, 0x40b2_fc00_0000_0000),
    // A boolean marker keeps a complete zero/one numeric lane. Only its
    // final true value draws a marker; false does not become numeric na.
    plot("context_constants_last", 60, 0x3ff0_0000_0000_0000),
    plot("gap 2-05 accumulator", 60, 0x40b8_42a9_a7e0_7898),
    plot("gap 2-05 checks", 60, 0x402e_0000_0000_0000),
    plot("gap acc", 60, 0x40c2_fc00_0000_0000),
    plot("gap checks", 60, 0x4014_0000_0000_0000),
    plot("gap_s1_s3_na_series", 30, 0x4060_3800_0000_0000),
    plot("gap_s1_s3_acc", 60, 0x40b0_5c00_0000_0000),
    plot("gap_s1_s3_checks", 60, 0x407a_5000_0000_0000),
    plot("gap checks", 60, 0x40a5_1800_0000_0000),
    plot("gap acc", 60, 0x40b3_5600_0000_0000),
];

const STRATEGY_EXPECTATIONS: &[ExpectedPlot] = &[
    plot("strategy_request_input_acc", 60, 0x42d9_40aa_9869_4d3c),
    plot("strategy_request_input_checks", 60, 0x40ad_1000_0000_0000),
    plot("gap strategy acc", 60, 0x40ca_0100_0000_0000),
    plot("gap strategy checks", 60, 0x4024_0000_0000_0000),
    plot("gap_s1_s3_strategy_acc", 60, 0xc03c_0000_0000_0000),
    plot("gap_s1_s3_strategy_checks", 60, 0x4066_6000_0000_0000),
];

struct Script<'a> {
    name: &'static str,
    source: &'a str,
    expected_plots: &'static [ExpectedPlot],
}

const SCRIPTS: [Script<'static>; 3] = [
    Script {
        name: "language-indicator.pine",
        source: include_str!("../../../tests/regression/language-indicator.pine"),
        expected_plots: INDICATOR_EXPECTATIONS,
    },
    Script {
        name: "language-library.pine",
        source: include_str!("../../../tests/regression/language-library.pine"),
        expected_plots: &[],
    },
    Script {
        name: "language-strategy.pine",
        source: include_str!("../../../tests/regression/language-strategy.pine"),
        expected_plots: STRATEGY_EXPECTATIONS,
    },
];

const NUMERIC_SOURCE: &str = include_str!("../../../tests/regression/numeric-runtime.pine");

fn synthetic_data() -> MarketData {
    let mut data = MarketData::default();
    for index in 0..BARS {
        let base = 100.0 + index as f64 * 0.5;
        data.open.push(base);
        data.high.push(base + 1.0);
        data.low.push(base - 1.0);
        data.close.push(base + 0.25);
        data.volume.push(1_000.0 + index as f64);
        data.time
            .push(1_000_000_000_000 + index as i64 * 86_400_000);
    }
    data
}

fn is_finite_plot_value(value: &Value) -> bool {
    match value {
        Value::Na => false,
        Value::Int(_) => true,
        Value::Float(value) => {
            assert!(
                value.is_finite(),
                "the VM must normalize non-finite values to na"
            );
            true
        }
        other => panic!("a numeric plot produced a non-numeric value: {other:?}"),
    }
}

fn assert_plot_expectations(script: &str, actual: &[PlotSeries], expected: &[ExpectedPlot]) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "{script} emitted a different plot inventory"
    );

    for (slot, (actual, expected)) in actual.iter().zip(expected).enumerate() {
        assert_eq!(
            actual.slot,
            u32::try_from(slot).expect("regression plot slot fits u32"),
            "{script} plot {slot} has the wrong slot"
        );
        assert_eq!(
            actual.title.as_deref(),
            Some(expected.title),
            "{script} plot {slot} has the wrong title"
        );
        assert_eq!(
            actual.values.len(),
            BARS,
            "{script} plot {slot} ({}) has the wrong length",
            expected.title
        );

        let finite = actual
            .values
            .iter()
            .filter(|value| is_finite_plot_value(value))
            .count();
        assert_eq!(
            finite, expected.finite,
            "{script} plot {slot} ({}) has the wrong finite-value count",
            expected.title
        );
        if expected.title == "context_constants_last" {
            let mut values = vec![Value::Float(0.0); BARS];
            values[BARS - 1] = Value::Float(1.0);
            assert_eq!(
                actual.values, values,
                "the last-bar marker must preserve every false and true value"
            );
        }

        let final_value = actual
            .values
            .last()
            .unwrap_or_else(|| panic!("{script} plot {slot} ({}) is empty", expected.title));
        let Value::Float(final_value) = final_value else {
            panic!(
                "{script} plot {slot} ({}) has a non-float final value: {final_value:?}",
                expected.title
            );
        };
        assert_eq!(
            final_value.to_bits(),
            expected.final_bits,
            "{script} plot {slot} ({}) final value changed: expected bits {:#018x}, found {:#018x} ({final_value:?})",
            expected.title,
            expected.final_bits,
            final_value.to_bits(),
        );
    }
}

#[test]
fn language_fixtures_match_local_expected_outputs() {
    let data = synthetic_data();
    let options = RunOptions {
        max_collection_elements: 1_000_000,
        ..RunOptions::default()
    };

    for script in SCRIPTS {
        let result = compile_and_run(script.source, &data, CompileOptions::default(), &options);
        let compile_errors: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.is_error())
            .collect();
        assert!(
            compile_errors.is_empty(),
            "{} failed to compile: {compile_errors:#?}",
            script.name
        );
        assert!(
            result.runtime_errors.is_empty(),
            "{} failed at runtime: {:#?}",
            script.name,
            result.runtime_errors
        );
        assert_eq!(
            result.bars_executed,
            u32::try_from(BARS).expect("regression bar count fits u32"),
            "{} executed the wrong feed",
            script.name
        );
        assert_plot_expectations(script.name, &result.plots, script.expected_plots);
    }
}

#[test]
fn current_blue_condition_restores_the_named_color_fixture_check() {
    let source = SCRIPTS[0].source;
    let legacy = "math_str_color_channels(color.blue) == 2201331";
    assert_eq!(source.matches(legacy).count(), 1);
    let current = source.replacen(legacy, "math_str_color_channels(color.blue) == 2712319", 1);
    let options = RunOptions {
        max_collection_elements: 1_000_000,
        ..RunOptions::default()
    };
    let result = compile_and_run(
        &current,
        &synthetic_data(),
        CompileOptions::default(),
        &options,
    );
    assert!(
        result.diagnostics.iter().all(|d| !d.is_error()),
        "{:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    let mut expected = INDICATOR_EXPECTATIONS.to_vec();
    let checks = expected
        .iter_mut()
        .find(|v| v.title == "math_str_color_checks")
        .unwrap();
    checks.final_bits = 95.0_f64.to_bits();
    assert_plot_expectations(
        "language-indicator current-blue variant",
        &result.plots,
        &expected,
    );
}

#[test]
fn current_constant_accessor_conditions_restore_only_the_two_numeric_checks() {
    // These constructor aliases are checked by the test below. This is a
    // variant of the indicator script with two constructor checks replaced.
    for repair_blue in [false, true] {
        let mut current = SCRIPTS[0].source.to_owned();
        for (legacy, observed) in [
            ("color.t(colRgbF) == 0.4", "color.t(colRgbF) == 0"),
            ("color.t(colNewF) == 49.6", "color.t(colNewF) == 49"),
        ] {
            assert_eq!(current.matches(legacy).count(), 1);
            current = current.replacen(legacy, observed, 1);
        }
        if repair_blue {
            let legacy = "math_str_color_channels(color.blue) == 2201331";
            assert_eq!(current.matches(legacy).count(), 1);
            current = current.replacen(legacy, "math_str_color_channels(color.blue) == 2712319", 1);
        }
        let result = compile_and_run(
            &current,
            &synthetic_data(),
            CompileOptions::default(),
            &RunOptions {
                max_collection_elements: 1_000_000,
                ..RunOptions::default()
            },
        );
        assert!(
            result.diagnostics.iter().all(|d| !d.is_error()),
            "{:?}",
            result.diagnostics
        );
        assert!(
            result.runtime_errors.is_empty(),
            "{:?}",
            result.runtime_errors
        );
        let mut expected = INDICATOR_EXPECTATIONS.to_vec();
        expected
            .iter_mut()
            .find(|v| v.title == "math_str_color_checks")
            .unwrap()
            .final_bits = if repair_blue { 97.0_f64 } else { 96.0_f64 }.to_bits();
        assert_plot_expectations(
            "language-indicator current accessor variant",
            &result.plots,
            &expected,
        );
    }
}

#[test]
fn numeric_fixture_runs_locally_without_non_finite_outputs() {
    let result = compile_and_run(
        NUMERIC_SOURCE,
        &synthetic_data(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    let compile_errors: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.is_error())
        .collect();
    assert!(
        compile_errors.is_empty(),
        "numeric-runtime.pine failed to compile: {compile_errors:#?}"
    );
    assert!(
        result.runtime_errors.is_empty(),
        "numeric-runtime.pine failed at runtime: {:#?}",
        result.runtime_errors
    );
    assert_eq!(result.bars_executed, BARS as u32);
    assert_eq!(result.plots.len(), 38, "numeric plot inventory changed");
    for plot in &result.plots {
        for value in &plot.values {
            let _ = is_finite_plot_value(value);
        }
    }
}
