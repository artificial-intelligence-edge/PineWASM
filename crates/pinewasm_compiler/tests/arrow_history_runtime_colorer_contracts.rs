//! Rendering contract regression tests for arrow history colorers.
use pinewasm_compiler::{compile, run_program, CompileOptions, MarketData, RunOptions};

#[test]
fn history_targets_bind_declarations_and_snapshots() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/arrow_history_cases.json")).unwrap();
    let market = MarketData {
        open: vec![10.0; 3],
        high: vec![12.0; 3],
        low: vec![9.0; 3],
        close: vec![11.0, 10.0, 12.0],
        volume: vec![0.0; 3],
        ..MarketData::default()
    };
    for case in fixture["cases"].as_array().unwrap() {
        let compiled = compile(case["source"].as_str().unwrap(), CompileOptions::default());
        assert!(compiled.success, "{:?}", compiled.diagnostics);
        let program = compiled.program.unwrap();
        let run = run_program(&program, &market, &RunOptions::default());
        assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
        for style in [
            serde_json::to_value(&program.plots[0].style).unwrap(),
            serde_json::to_value(&run.plots[0].style).unwrap(),
        ] {
            assert_eq!(
                style["arrowRuntimeColorers"]["up"], case["expectedUpColorer"],
                "{}",
                case["id"]
            );
            // Every source in this fixture has a fixed colordown.
            assert_eq!(style["arrowRuntimeColorers"]["down"], false);
            assert_eq!(style["arrowStaticColors"], case["expected"]);
        }
    }
}
