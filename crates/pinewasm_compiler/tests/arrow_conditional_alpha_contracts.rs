//! Local rendering contract regression tests.
use pinewasm_compiler::{compile, run_program, CompileOptions, MarketData, RunOptions, Value};

fn check(index: usize) {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/arrow_conditional_alpha_cases.json")).unwrap();
    let case = &fixture["cases"][index];

    let result = compile(case["source"].as_str().unwrap(), CompileOptions::default());
    assert!(result.success, "{:?}", result.diagnostics);
    let program = result.program.unwrap();
    let run = run_program(
        &program,
        &MarketData {
            open: vec![10.0; 3],
            high: vec![12.0; 3],
            low: vec![9.0; 3],
            close: vec![11.0, 10.0, 12.0],
            volume: vec![0.0; 3],
            ..MarketData::default()
        },
        &RunOptions::default(),
    );
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    assert_eq!(program.plots.len(), 1);
    assert_eq!(run.plots.len(), 1);
    for style in [
        serde_json::to_value(&program.plots[0].style).unwrap(),
        serde_json::to_value(&run.plots[0].style).unwrap(),
    ] {
        assert_eq!(
            style["arrowRuntimeColorers"],
            case["expected"]["runtimeColorers"]
        );
        for direction in ["up", "down"] {
            for channel in ["r", "g", "b", "t"] {
                assert_eq!(
                    style["arrowStaticColors"][direction][channel].as_f64(),
                    case["expected"]["staticColors"][direction][channel].as_f64()
                );
            }
        }
    }
    let up = run.plots[0]
        .colors
        .iter()
        .find(|lane| lane.role == pinewasm_compiler::PlotColorRole::Up)
        .unwrap();
    assert_eq!(up.values.len(), 3);
    assert!(up.values.iter().all(|value| matches!(
        value,
        Value::Color {
            r: 51,
            g: 102,
            b: 153,
            t: 70.0,
            ..
        }
    )));
}
#[test]
fn same_value_nonconstant_write_has_opaque_fallback_without_rewriting_raw_alpha() {
    check(0);
}
#[test]
fn same_value_literal_true_write_preserves_fixed_alpha_without_rewriting_raw_alpha() {
    check(1);
}
