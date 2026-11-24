//! Local rendering contract regression tests.
use pinewasm_compiler::{compile, run_program, CompileOptions, MarketData, RunOptions};
use sha2::{Digest, Sha256};

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
fn successful_color_history_selects_dynamic_static_fallback() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/arrow_history_cases.json")).unwrap();
    let mut failures = Vec::new();
    for case in fixture["cases"].as_array().unwrap() {
        let compiled = compile(
            case["source"].as_str().unwrap(),
            CompileOptions {
                emit_csil: true,
                ..CompileOptions::default()
            },
        );
        assert!(compiled.success, "{:?}", compiled.diagnostics);
        // Checks the canonical execution listing, including code, source maps,
        // stack, slots and inputs.
        // This is separate from the metadata checks above.
        assert_eq!(
            Sha256::digest(compiled.csil_text.as_ref().unwrap().as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            case["expectedCsilSha256"].as_str().unwrap(),
            "{} execution listing changed",
            case["id"]
        );
        let program = compiled.program.unwrap();
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
            if style["arrowStaticColors"] != case["expected"] {
                failures.push(format!(
                    "{} {owner}: {:?} != {:?}",
                    case["id"], style["arrowStaticColors"], case["expected"]
                ));
            }
        }
        // Actual runtime rows remain history-sensitive; the static fallback
        // must not replace the missing value on the first bar.
        let up = run.plots[0]
            .colors
            .iter()
            .find(|color| color.role == pinewasm_compiler::PlotColorRole::Up)
            .unwrap();
        if case["expectedUpColorer"] == true && case["historyOffset"] != 0 {
            assert!(up.values[0].is_na(), "{}", case["id"]);
        } else {
            assert_eq!(up.values[0].render(), "#336699");
        }
        assert_eq!(up.values[1].render(), "#336699");
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn dynamic_materialization_is_not_a_current_constant_style() {
    // The input-offset-cast-history case uses the dynamic-offset HIR variant.
    // Runtime row values below remain an internal invariant.
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/arrow_history_cases.json")).unwrap();
    let source = fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["id"] == "input-offset-cast-history")
        .unwrap()["source"]
        .as_str()
        .unwrap();
    let compiled = compile(source, CompileOptions::default());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    let program = compiled.program.unwrap();
    let style = serde_json::to_value(&program.plots[0].style).unwrap();
    assert_eq!(style["arrowStaticColors"]["up"]["origin"], "dynamic");
    let run = run_program(&program, &market(), &RunOptions::default());
    assert!(run.runtime_errors.is_empty());
    let up = run.plots[0]
        .colors
        .iter()
        .find(|color| color.role == pinewasm_compiler::PlotColorRole::Up)
        .unwrap();
    assert!(up.values[0].is_na());
    assert_eq!(up.values[1].render(), "#336699");
    assert_eq!(up.values[2].render(), "#336699");
}
