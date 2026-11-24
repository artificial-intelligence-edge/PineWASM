//! Rendering regression tests for the VM.
use pinewasm_compiler::{compile, run_program, CompileOptions, MarketData, RunOptions};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/ohlc_body_missing_origin_cases.json")).unwrap()
}
fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn check_case(index: usize) {
    let fixture = fixture();
    let case = &fixture["cases"][index];
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
    let listing = hash(compiled.csil_text.as_ref().unwrap().as_bytes());
    let program = compiled.program.unwrap();
    let market = MarketData {
        open: vec![10.0; 3],
        high: vec![12.0; 3],
        low: vec![9.0; 3],
        close: vec![11.0, 10.0, 12.0],
        volume: vec![0.0; 3],
        ..MarketData::default()
    };
    let run = run_program(&program, &market, &RunOptions::default());
    assert!(
        run.runtime_errors.is_empty(),
        "{}: {:?}",
        case["id"],
        run.runtime_errors
    );
    assert_eq!(program.plots.len(), 4, "{}", case["id"]);
    assert_eq!(run.plots.len(), 4, "{}", case["id"]);
    let raw = run
        .plots
        .iter()
        .map(|p| (&p.values, &p.colors))
        .collect::<Vec<_>>();
    eprintln!(
        "OHLC_PRESERVATION {}",
        json!({
            "id":case["id"], "sourceSha256":case["sourceSha256"],
            "expectedCsilSha256":listing, "rawSha256":hash(&serde_json::to_vec(&raw).unwrap())
        })
    );
    for lane in 0..4 {
        for (owner, style) in [
            (
                "declaration",
                serde_json::to_value(&program.plots[lane].style).unwrap(),
            ),
            (
                "snapshot",
                serde_json::to_value(&run.plots[lane].style).unwrap(),
            ),
        ] {
            assert_eq!(
                style["ohlcBodyStyle"], case["expected"],
                "{} {owner} lane{lane} compiled style properties and binding",
                case["id"]
            );
        }
    }
}

macro_rules! fixture_case {
    ($name:ident, $index:expr) => {
        #[test]
        fn $name() {
            check_case($index);
        }
    };
}
fixture_case!(ohlc_missing_origin_00, 0);
fixture_case!(ohlc_missing_origin_01, 1);
fixture_case!(ohlc_missing_origin_02, 2);
fixture_case!(ohlc_missing_origin_03, 3);
fixture_case!(ohlc_missing_origin_04, 4);
fixture_case!(ohlc_missing_origin_05, 5);
fixture_case!(ohlc_missing_origin_06, 6);
fixture_case!(ohlc_missing_origin_07, 7);
fixture_case!(ohlc_missing_origin_08, 8);
fixture_case!(ohlc_missing_origin_09, 9);
fixture_case!(ohlc_missing_origin_10, 10);
fixture_case!(ohlc_missing_origin_11, 11);
fixture_case!(ohlc_missing_origin_12, 12);
fixture_case!(ohlc_missing_origin_13, 13);
fixture_case!(ohlc_missing_origin_14, 14);
fixture_case!(ohlc_missing_origin_15, 15);
fixture_case!(ohlc_missing_origin_16, 16);
fixture_case!(ohlc_missing_origin_17, 17);
fixture_case!(ohlc_missing_origin_18, 18);
fixture_case!(ohlc_missing_origin_19, 19);
fixture_case!(ohlc_missing_origin_20, 20);
fixture_case!(ohlc_missing_origin_21, 21);
fixture_case!(ohlc_missing_origin_22, 22);
fixture_case!(ohlc_missing_origin_23, 23);

#[test]
fn rejected_cases_supply_no_metadata_expectation() {
    for row in fixture()["rejectedCases"].as_array().unwrap() {
        assert!(row.get("expected").is_none());
        assert_eq!(
            hash(row["source"].as_str().unwrap().as_bytes()),
            row["sourceSha256"]
        );
    }
}
