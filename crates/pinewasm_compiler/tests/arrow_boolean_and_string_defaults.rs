//! Local rendering contract regression tests.
use pinewasm_compiler::{compile, run_program, CompileOptions, InputValue, MarketData, RunOptions};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "fixtures/arrow_boolean_and_string_default_cases.json"
    ))
    .unwrap()
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
arrow_case!(left_false_and_series, 0);
arrow_case!(left_true_or_series, 1);
arrow_case!(nonabsorbing_true_and_series, 2);
arrow_case!(nonabsorbing_false_or_series, 3);
arrow_case!(left_false_and_unsupported_const, 4);
arrow_case!(left_true_or_unsupported_const, 5);
arrow_case!(direct_false_control, 6);
arrow_case!(direct_true_control, 7);
arrow_case!(right_false_and_series, 8);
arrow_case!(right_true_or_series, 9);
arrow_case!(right_false_and_unsupported_series, 10);
arrow_case!(right_true_or_unsupported_series, 11);
arrow_case!(nested_false_control, 12);
arrow_case!(nested_true_control, 13);
arrow_case!(left_false_and_input, 14);
arrow_case!(left_true_or_input, 15);
arrow_case!(down_left_false_and_series, 16);
arrow_case!(down_left_true_or_series, 17);

fn literal_default(index: usize) {
    let fixture = fixture();
    let case = &fixture["literalStringDefaults"][index];
    let source = case["source"].as_str().unwrap();
    assert_eq!(hash(source.as_bytes()), case["sourceSha256"]);

    let result = compile(source, CompileOptions::default());
    assert!(result.success, "{}: {:?}", case["id"], result.diagnostics);
    let program = result.program.unwrap();
    let listed_inputs = case["expectedInputs"].as_array().unwrap();
    assert_eq!(program.inputs.len(), listed_inputs.len());
    for (input, expected) in program.inputs.iter().zip(listed_inputs) {
        assert_eq!(input.title.as_deref(), expected["name"].as_str());
        match expected["type"].as_str().unwrap() {
            "text" => assert_eq!(
                input.defval,
                InputValue::String(expected["defval"].as_str().unwrap().to_owned())
            ),
            "bool" => assert_eq!(
                input.defval,
                InputValue::Bool(expected["defval"].as_bool().unwrap())
            ),
            other => panic!("unexpected input type {other}"),
        }
    }
}
#[test]
fn literal_missing_string_cast_has_empty_input_default() {
    literal_default(0);
}
#[test]
fn literal_missing_string_cast_has_true_empty_equality_default() {
    literal_default(1);
}
