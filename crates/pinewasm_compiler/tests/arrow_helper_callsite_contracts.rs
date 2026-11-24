//! Local rendering contract regression tests.
use pinewasm_compiler::{compile, run_program, CompileOptions, MarketData, RunOptions};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/arrow_helper_callsite_cases.json")).unwrap()
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
    assert_eq!(
        program.plots.len(),
        case["expectedPlots"].as_array().unwrap().len()
    );
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
            "rawSha256":hash(&serde_json::to_vec(&run.plots.iter().map(|plot| (&plot.values,&plot.colors)).collect::<Vec<_>>()).unwrap()),
            "actualStyles":program.plots.iter().map(|plot| &plot.style).collect::<Vec<_>>()
        })
    );
    for (plot_index, expected) in case["expectedPlots"].as_array().unwrap().iter().enumerate() {
        for (owner, style) in [
            (
                "declaration",
                serde_json::to_value(&program.plots[plot_index].style).unwrap(),
            ),
            (
                "snapshot",
                serde_json::to_value(&run.plots[plot_index].style).unwrap(),
            ),
        ] {
            assert_eq!(
                style["arrowRuntimeColorers"], expected["bindings"],
                "{} {owner} expected colorer binding",
                case["id"]
            );
            for role in ["up", "down"] {
                for field in ["r", "g", "b", "t"] {
                    assert_eq!(
                        style["arrowStaticColors"][role][field].as_f64(),
                        expected["static"][role][field].as_f64(),
                        "{} {owner} {role}.{field} expected static color",
                        case["id"]
                    );
                }
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
arrow_case!(helper_00_false_and_persistent, 0);
arrow_case!(helper_01_true_or_persistent, 1);
arrow_case!(helper_02_persistent_and_false, 2);
arrow_case!(helper_03_persistent_or_true, 3);
arrow_case!(helper_04_true_and_persistent, 4);
arrow_case!(helper_05_false_or_persistent, 5);
arrow_case!(helper_06_literal_argument_with_unrelated_mutable_global, 6);
arrow_case!(helper_07_unchanged_persistent, 7);
arrow_case!(helper_08_zero_argument_global_false_and, 8);
arrow_case!(helper_09_false_and_persistent_down_role, 9);
arrow_case!(helper_10_absorbing_callsite_and, 10);
arrow_case!(helper_11_absorbing_callsite_or, 11);
arrow_case!(helper_12_formal_local_alias, 12);
arrow_case!(helper_13_unused_formal, 13);
arrow_case!(helper_14_formal_in_dead_ternary, 14);
arrow_case!(helper_15_nested_formal, 15);
arrow_case!(helper_16_boolean_result_formal, 16);
arrow_case!(helper_17_zeroarg_global_alias, 17);
arrow_case!(helper_18_same_helper_literal_then_mutable, 18);
arrow_case!(helper_19_same_helper_mutable_then_literal, 19);
arrow_case!(helper_20_same_helper_role_literal_mutable, 20);
arrow_case!(helper_21_same_helper_role_mutable_literal, 21);
arrow_case!(helper_22_nested_same_helper_literal_mutable, 22);
arrow_case!(helper_23_nested_same_helper_mutable_literal, 23);
arrow_case!(helper_24_same_helper_same_expression_pair, 24);
arrow_case!(helper_25_same_helper_captured_alias, 25);
