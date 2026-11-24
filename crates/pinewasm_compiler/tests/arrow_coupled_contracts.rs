//! Local rendering contract regression tests.
use pinewasm_compiler::{compile, run_program, CompileOptions, MarketData, RunOptions};
use serde_json::Value;

#[test]
fn successful_arrow_styles_and_colorer_targets_remain_coupled() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/arrow_coupled_cases.json")).unwrap();
    let market = MarketData {
        open: vec![10.0; 3],
        high: vec![12.0; 3],
        low: vec![9.0; 3],
        close: vec![11.0, 10.0, 12.0],
        volume: vec![0.0; 3],
        ..MarketData::default()
    };
    let mut failures = Vec::new();
    for case in fixture["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();

        let result = compile(case["source"].as_str().unwrap(), CompileOptions::default());
        assert!(result.success, "{id}: {:?}", result.diagnostics);
        let program = result.program.unwrap();
        let run = run_program(&program, &market, &RunOptions::default());
        assert!(
            run.runtime_errors.is_empty(),
            "{id}: {:?}",
            run.runtime_errors
        );
        let expected = case["expected"].as_array().unwrap();
        assert_eq!(program.plots.len(), expected.len(), "{id}");
        assert_eq!(run.plots.len(), expected.len(), "{id}");
        for (index, expected) in expected.iter().enumerate() {
            for (owner, style) in [
                (
                    "declaration",
                    serde_json::to_value(&program.plots[index].style).unwrap(),
                ),
                (
                    "snapshot",
                    serde_json::to_value(&run.plots[index].style).unwrap(),
                ),
            ] {
                if style["arrowRuntimeColorers"] != expected["runtimeColorers"] {
                    failures.push(format!(
                        "{id} arrow {index} {owner}: runtime binding {:?} != {:?}",
                        style["arrowRuntimeColorers"], expected["runtimeColorers"]
                    ));
                }
                for direction in ["up", "down"] {
                    let actual = &style["arrowStaticColors"][direction];
                    let want = &expected["staticColors"][direction];
                    for channel in ["r", "g", "b", "t"] {
                        let equal = match (actual[channel].as_f64(), want[channel].as_f64()) {
                            (Some(actual), Some(want)) if channel == "t" => {
                                (actual - want).abs() < 1e-10
                            }
                            (Some(actual), Some(want)) => actual == want,
                            _ => false,
                        };
                        if !equal {
                            failures.push(format!("{id} arrow {index} {owner} {direction}.{channel}: {:?} != {:?} (expected {:?})",
                                actual[channel], want[channel], expected["staticCss"][direction]));
                        }
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
