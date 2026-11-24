//! Local rendering contract regression tests.
use pinewasm_compiler::{compile, run_program, CompileOptions, MarketData, RunOptions};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/arrow_missing_guard_cases.json")).unwrap()
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
arrow_case!(missing_statement_00, 0);
arrow_case!(missing_statement_01, 1);
arrow_case!(missing_statement_02, 2);
arrow_case!(missing_statement_03, 3);
arrow_case!(missing_statement_04, 4);
arrow_case!(missing_statement_05, 5);
arrow_case!(missing_statement_06, 6);
arrow_case!(missing_statement_07, 7);
arrow_case!(missing_statement_08, 8);
arrow_case!(missing_statement_09, 9);
arrow_case!(missing_statement_10, 10);
arrow_case!(missing_statement_11, 11);
arrow_case!(missing_ternary_00, 12);
arrow_case!(missing_ternary_01, 13);
arrow_case!(missing_ternary_02, 14);
arrow_case!(missing_ternary_03, 15);
arrow_case!(missing_ternary_04, 16);
arrow_case!(missing_ternary_05, 17);
arrow_case!(missing_nested_00, 18);
arrow_case!(missing_nested_01, 19);
arrow_case!(missing_nested_02, 20);
arrow_case!(missing_nested_03, 21);
arrow_case!(missing_nested_04, 22);
arrow_case!(missing_nested_05, 23);
arrow_case!(missing_nested_06, 24);
arrow_case!(missing_nested_07, 25);
arrow_case!(missing_nested_08, 26);
arrow_case!(missing_nested_09, 27);
arrow_case!(missing_alias_00, 28);
arrow_case!(missing_alias_01, 29);
arrow_case!(missing_alias_02, 30);
arrow_case!(missing_alias_03, 31);
arrow_case!(missing_alias_04, 32);
arrow_case!(missing_alias_05, 33);
