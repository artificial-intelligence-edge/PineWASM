//! Local rendering contract regression tests.
use pinewasm_compiler::{compile, run_program, CompileOptions, MarketData, RunOptions};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/arrow_cosine_guard_cases.json")).unwrap()
}
fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
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
fn arrow(index: usize) {
    let fixture = fixture();
    let case = &fixture["arrows"][index];
    let source = case["source"].as_str().unwrap();
    assert_eq!(hash(source.as_bytes()), case["sourceSha256"]);

    let compiled = compile(
        source,
        CompileOptions {
            emit_csil: true,
            ..CompileOptions::default()
        },
    );
    assert!(
        compiled.success,
        "{}: {:?}",
        case["id"], compiled.diagnostics
    );
    let listing_hash = hash(compiled.csil_text.as_ref().unwrap().as_bytes());
    let program = compiled.program.unwrap();
    assert_eq!(program.plots.len(), 1);
    let run = run_program(&program, &market(), &RunOptions::default());
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    // Metadata preparation must keep the complete execution listing and the raw
    // values and color roles unchanged, including their palette identity.
    eprintln!(
        "PRESERVATION {}",
        json!({
            "id":case["id"],
            "sourceSha256":case["sourceSha256"],
            "expectedCsilSha256":listing_hash,
            "rawSha256":hash(&serde_json::to_vec(&(run.plots[0].values.clone(),run.plots[0].colors.clone())).unwrap()),
            "actualStyle":program.plots[0].style
        })
    );
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
        assert_eq!(
            style["arrowRuntimeColorers"], case["expectedBindings"],
            "{} {owner} expected colorer binding",
            case["id"]
        );
        for role in ["up", "down"] {
            for field in ["r", "g", "b", "t"] {
                assert_eq!(
                    style["arrowStaticColors"][role][field].as_f64(),
                    case["expectedStatic"][role][field].as_f64(),
                    "{} {owner} {role}.{field} expected static color",
                    case["id"]
                );
            }
        }
    }
}
macro_rules! arrow_case {
    ($name:ident, $index:expr) => {
        #[test]
        fn $name() {
            arrow($index);
        }
    };
}
arrow_case!(unknown_const_left_and_series, 0);
arrow_case!(series_left_and_unknown_const, 1);
arrow_case!(unknown_const_left_or_series, 2);
arrow_case!(series_left_or_unknown_const, 3);
arrow_case!(cosine_false_left_or_series, 4);
arrow_case!(series_left_or_cosine_false, 5);
arrow_case!(cosine_false_left_and_series, 6);
arrow_case!(series_left_and_cosine_false, 7);
arrow_case!(direct_cosine_true, 8);
arrow_case!(direct_cosine_false, 9);
