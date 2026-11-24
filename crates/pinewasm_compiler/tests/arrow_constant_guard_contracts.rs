//! Local rendering contract regression tests.
use pinewasm_compiler::{compile, run_program, CompileOptions, MarketData, RunOptions};

fn market() -> MarketData {
    MarketData {
        open: vec![10.0; 3],
        high: vec![12.0; 3],
        low: vec![9.0; 3],
        close: vec![11.0, 10.0, 12.0],
        volume: vec![0.0; 3],
        ..MarketData::default()
    }
}
#[test]
fn constant_and_input_guards_select_independent_static_and_runtime_styles() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/arrow_constant_guard_cases.json")).unwrap();
    let mut failures = Vec::new();
    for case in fixture["cases"].as_array().unwrap() {
        let result = compile(case["source"].as_str().unwrap(), CompileOptions::default());
        assert!(result.success, "{:?}", result.diagnostics);
        let program = result.program.unwrap();
        let run = run_program(&program, &market(), &RunOptions::default());
        assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
        for (owner, style) in [
            (
                "declaration",
                serde_json::to_value(&program.plots[0].style).unwrap(),
            ),
            (
                "snapshot",
                serde_json::to_value(&run.plots[0].style).unwrap(),
            ),
        ] {
            if style["arrowRuntimeColorers"] != case["expected"]["runtimeColorers"] {
                failures.push(format!(
                    "{} {owner}: binding {:?} != {:?}",
                    case["id"], style["arrowRuntimeColorers"], case["expected"]["runtimeColorers"]
                ));
            }
            for direction in ["up", "down"] {
                for channel in ["r", "g", "b", "t"] {
                    if style["arrowStaticColors"][direction][channel].as_f64()
                        != case["expected"]["staticColors"][direction][channel].as_f64()
                    {
                        failures.push(format!(
                            "{} {owner}: {direction}.{channel} differs",
                            case["id"]
                        ));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
#[test]
fn unproven_constant_guard_preserves_omission_and_diagnostics() {
    let result = compile("//@version=6\nindicator(\"unproven guard admission\")\ncolor UP=#336699\nif math.tan(0)==0\n    UP:=#336699\nplotarrow(close,colorup=UP,colordown=#993366)", CompileOptions::default());
    assert!(result.success, "{:?}", result.diagnostics);
    let program = result.program.unwrap();
    assert!(program.plots[0].style.arrow_static_colors.is_none());
    assert!(program.plots[0].style.arrow_runtime_colorers.is_none());
    assert!(program.plots[0]
        .style
        .unsupported
        .iter()
        .any(|option| option.reason.contains("cannot be proven")));
    let run = run_program(&program, &market(), &RunOptions::default());
    assert!(run.runtime_errors.is_empty());
    assert_eq!(run.plots[0].values.len(), 3);
}
