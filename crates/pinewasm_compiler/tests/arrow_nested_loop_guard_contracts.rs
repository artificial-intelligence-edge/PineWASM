//! Local rendering contract regression tests.
use pinewasm_compiler::{compile, run_program, CompileOptions, MarketData, RunOptions, Value};
fn check(index: usize) {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/arrow_nested_loop_guard_cases.json")).unwrap();
    let case = &fixture["cases"][index];

    let result = compile(case["source"].as_str().unwrap(), CompileOptions::default());
    assert!(result.success, "{:?}", result.diagnostics);
    let program = result.program.unwrap();
    let market = MarketData {
        open: vec![10.0; 3],
        high: vec![12.0; 3],
        low: vec![9.0; 3],
        close: vec![11.0, 10.0, 12.0],
        volume: vec![0.0; 3],
        ..MarketData::default()
    };
    let run = run_program(&program, &market, &RunOptions::default());
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    assert_eq!(program.plots.len(), 1);
    assert_eq!(run.plots.len(), 1);
    for style in [
        serde_json::to_value(&program.plots[0].style).unwrap(),
        serde_json::to_value(&run.plots[0].style).unwrap(),
    ] {
        assert_eq!(
            style["arrowRuntimeColorers"], case["expected"]["runtimeColorers"],
            "{}",
            case["id"]
        );
        for direction in ["up", "down"] {
            for channel in ["r", "g", "b", "t"] {
                assert_eq!(
                    style["arrowStaticColors"][direction][channel].as_f64(),
                    case["expected"]["staticColors"][direction][channel].as_f64(),
                    "{} {direction}.{channel}",
                    case["id"]
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
fn nested_constant_false_is_pruned() {
    check(0);
}
#[test]
fn nested_constant_true_keeps_outer_dynamic_write() {
    check(1);
}
#[test]
fn folded_false_while_keeps_loop_qualification() {
    check(2);
}
#[test]
fn nested_input_is_dynamic() {
    check(3);
}
#[test]
fn literal_false_while_keeps_loop_qualification() {
    check(4);
}
#[test]
fn alias_false_while_keeps_loop_qualification() {
    check(5);
}
#[test]
fn abs_false_while_keeps_loop_qualification() {
    check(6);
}
#[test]
fn literal_false_if_is_pruned() {
    check(7);
}
#[test]
fn descending_for_keeps_loop_qualification() {
    check(8);
}
