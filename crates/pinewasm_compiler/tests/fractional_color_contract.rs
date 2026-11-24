//! Local rendering contract regressions.
use pinewasm_compiler::{compile_and_run, CompileOptions, MarketData, RunOptions, Value};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Sample {
    transparency: f64,
    alpha_byte: u8,
    hex: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExpectedColors {
    whole_percentages: Vec<Sample>,
    alpha_bytes: Vec<Sample>,
    fractional_percentages: Vec<Sample>,
}

fn run(body: &str, options: &RunOptions) -> pinewasm_compiler::RunResult {
    let data = MarketData {
        open: vec![9.0; 3],
        high: vec![11.0; 3],
        low: vec![8.0; 3],
        close: vec![10.0; 3],
        volume: vec![100.0; 3],
        ..MarketData::default()
    };
    let source = format!("//@version=6\nindicator(\"Fractional alpha\")\n{body}\nplot(close,color=c)\nplot(color.t(c))\n");
    let result = compile_and_run(&source, &data, CompileOptions::default(), options);
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
    assert_eq!(result.bars_executed, 3);
    result
}

fn assert_color(body: &str, sample: &Sample, options: &RunOptions) -> pinewasm_compiler::RunResult {
    let result = run(body, options);
    for value in &result.plots[0].colors[0].values {
        assert_eq!(
            value.render(),
            sample.hex,
            "{body}: alpha {}",
            sample.alpha_byte
        );
    }
    for value in &result.plots[0].colors[0].values {
        let Value::Color { t, .. } = value else {
            panic!("expected a raw color: {value:?}")
        };
        assert!(
            (t - sample.transparency).abs() <= 1e-12,
            "{body}: raw transparency {t} != {}",
            sample.transparency
        );
    }
    result
}

fn assert_runtime_accessor(result: &pinewasm_compiler::RunResult, transparency: f64) {
    for value in &result.plots[1].values {
        let Value::Float(actual) = value else {
            panic!("runtime color.t must return a float: {value:?}")
        };
        assert!((actual - transparency).abs() <= 1e-12);
    }
}

fn expected_colors() -> ExpectedColors {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/rendering/frozen-color-alpha.json"
    ))
    .unwrap()
}

#[test]
fn serialized_whole_color_percentages_remain_backward_readable_and_fractions_are_numbers() {
    for (text, t) in [("40", 40.0), ("47.6", 47.6)] {
        let input = format!("{{\"Color\":{{\"r\":18,\"g\":52,\"b\":86,\"t\":{text}}}}}");
        let value: pinewasm_hir::InputValue = serde_json::from_str(&input).unwrap();
        assert_eq!(
            value,
            pinewasm_hir::InputValue::Color {
                r: 18,
                g: 52,
                b: 86,
                t
            }
        );
        assert_eq!(serde_json::to_string(&value).unwrap(), input);
        let instruction = format!("{{\"PushColor\":{{\"r\":18,\"g\":52,\"b\":86,\"t\":{text}}}}}");
        let decoded: pinewasm_csil::CsilInstr = serde_json::from_str(&instruction).unwrap();
        assert_eq!(
            decoded,
            pinewasm_csil::CsilInstr::PushColor {
                r: 18,
                g: 52,
                b: 86,
                t: pinewasm_csil::F64Bits::new(t)
            }
        );
        assert_eq!(serde_json::to_string(&decoded).unwrap(), instruction);
    }
}

#[test]
fn every_literal_alpha_byte_survives_parse_and_wire_serialization() {
    let samples = expected_colors().alpha_bytes;
    assert_eq!(samples.len(), 256);
    for sample in samples {
        assert_color(
            &format!("c = {}", sample.hex),
            &sample,
            &RunOptions::default(),
        );
    }
}

#[test]
fn constructors_preserve_fractional_percentages_and_frozen_alpha_rounding() {
    let expected = expected_colors();
    assert_eq!(expected.whole_percentages.len(), 101);
    assert_eq!(expected.fractional_percentages.len(), 11);
    for sample in expected
        .whole_percentages
        .into_iter()
        .chain(expected.fractional_percentages)
    {
        for expression in [
            format!("color.new(#123456, {})", sample.transparency),
            format!("color.rgb(18, 52, 86, {})", sample.transparency),
            format!("color.new(#123456, input.float({}))", sample.transparency),
        ] {
            let result = assert_color(
                &format!("c = {expression}"),
                &sample,
                &RunOptions::default(),
            );
            if expression.contains("input.float") {
                assert_runtime_accessor(&result, sample.transparency);
            }
        }
    }
}

#[test]
fn input_color_defaults_and_overrides_keep_fractional_transparency() {
    let samples = expected_colors().fractional_percentages;
    for sample in samples {
        let result = assert_color(
            &format!(
                "c = input.color(color.new(#123456, {}))",
                sample.transparency
            ),
            &sample,
            &RunOptions::default(),
        );
        assert_runtime_accessor(&result, sample.transparency);
        let mut options = RunOptions::default();
        options.inputs.insert(
            0,
            pinewasm_hir::InputValue::Color {
                r: 18,
                g: 52,
                b: 86,
                t: sample.transparency,
            },
        );
        let result = assert_color("c = input.color(#123456)", &sample, &options);
        assert_runtime_accessor(&result, sample.transparency);
    }
}

#[test]
fn internal_simple_and_series_accessors_retain_fractional_runtime_boundary() {
    for sample in expected_colors().fractional_percentages {
        for qualifier in ["simple", "series"] {
            let result = assert_color(
                &format!(
                    "{qualifier} float alpha={}\nc=color.new(#123456,alpha)",
                    sample.transparency
                ),
                &sample,
                &RunOptions::default(),
            );
            assert_runtime_accessor(&result, sample.transparency);
        }
    }
}

#[test]
fn constant_float_accessors_are_valid_input_defaults() {
    for (accessor, expected) in [("r", 18.0), ("g", 52.0), ("b", 86.0), ("t", 47.0)] {
        let result = run(
            &format!(
                "c = color.new(#123456, input.float(color.{accessor}(color.new(#123456, 47.6))))"
            ),
            &RunOptions::default(),
        );
        assert_eq!(result.plots[1].values, vec![Value::Float(expected); 3]);
    }
}

#[test]
fn valuewhen_keeps_exact_fractional_colors_na_and_bounded_capture_order() {
    let bars = 1500;
    let data = MarketData {
        open: vec![9.0; bars],
        high: vec![11.0; bars],
        low: vec![8.0; bars],
        close: vec![10.0; bars],
        volume: vec![100.0; bars],
        ..MarketData::default()
    };
    let source = "//@version=6\nindicator(\"Captured alpha\")\nshade = bar_index % 17 == 0 ? na : color.rgb(18,52,86,(bar_index % 256) * 100.0 / 255)\nc = ta.valuewhen(bar_index % 3 == 0, shade, 4)\nplot(color.t(c))\nplot(close,color=c)\n";
    let result = compile_and_run(
        source,
        &data,
        CompileOptions::default(),
        &RunOptions::default(),
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
    assert_eq!(result.bars_executed as usize, bars);
    let mut captures = Vec::new();
    for bar in 0..bars {
        if bar % 3 == 0 {
            captures.push((bar % 17 != 0).then_some((bar % 256) as f64 * 100.0 / 255.0));
        }
        let expected = captures
            .len()
            .checked_sub(5)
            .and_then(|index| captures[index]);
        match (&result.plots[0].values[bar], expected) {
            (Value::Na, None) => {}
            (Value::Float(actual), Some(expected)) => {
                assert!((actual - expected).abs() < 1e-12, "bar {bar}");
            }
            (actual, expected) => panic!("bar {bar}: {actual:?} != {expected:?}"),
        }
        match (&result.plots[1].colors[0].values[bar], expected) {
            (Value::Na, None) => {}
            (
                Value::Color {
                    r: 18,
                    g: 52,
                    b: 86,
                    t,
                    ..
                },
                Some(expected),
            ) => assert!((t - expected).abs() < 1e-12, "bar {bar}"),
            (actual, expected) => panic!("color bar {bar}: {actual:?} != {expected:?}"),
        }
    }
}

#[test]
fn gradient_alpha_is_interpolated_without_rounding_the_percentage() {
    let result = run(
        "c = color.from_gradient(0.25,0.0,1.0,color.new(#123456,20.5),color.new(#123456,47.6))",
        &RunOptions::default(),
    );
    for value in &result.plots[1].values {
        let Value::Float(value) = value else {
            panic!("{value:?}")
        };
        assert!((value - 27.275).abs() < 1e-12);
    }
    assert!(result.plots[0].colors[0]
        .values
        .iter()
        .all(|value| value.render() == "#123456B9"));
}
