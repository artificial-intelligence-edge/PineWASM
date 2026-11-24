//! Rendering regression tests for the VM.
use pinewasm_compiler::{compile, run_program, CompileOptions, MarketData, RunOptions};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/ohlc_body_cases.json")).unwrap()
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
    // RGBT transparency is a numeric wire field: 70 and 70.0 denote the same
    // f64 value. Deserialize both records through the strict DTO so required
    // fields, unknown fields, booleans and RGB bytes are still checked exactly.
    let expected: pinewasm_compiler::OhlcBodyStyle =
        serde_json::from_value(case["expected"].clone()).unwrap();
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
                serde_json::from_value::<pinewasm_compiler::OhlcBodyStyle>(
                    style["ohlcBodyStyle"].clone()
                )
                .unwrap(),
                expected,
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
fixture_case!(ohlc_body_00, 0);
fixture_case!(ohlc_body_01, 1);
fixture_case!(ohlc_body_02, 2);
fixture_case!(ohlc_body_03, 3);
fixture_case!(ohlc_body_04, 4);
fixture_case!(ohlc_body_05, 5);
fixture_case!(ohlc_body_06, 6);
fixture_case!(ohlc_body_07, 7);
fixture_case!(ohlc_body_08, 8);
fixture_case!(ohlc_body_09, 9);
fixture_case!(ohlc_body_10, 10);
fixture_case!(ohlc_body_11, 11);
fixture_case!(ohlc_body_12, 12);
fixture_case!(ohlc_body_13, 13);
fixture_case!(ohlc_body_14, 14);
fixture_case!(ohlc_body_15, 15);
fixture_case!(ohlc_body_16, 16);
fixture_case!(ohlc_body_17, 17);
fixture_case!(ohlc_body_18, 18);
fixture_case!(ohlc_body_19, 19);
fixture_case!(ohlc_body_20, 20);
fixture_case!(ohlc_body_21, 21);
fixture_case!(ohlc_body_22, 22);
fixture_case!(ohlc_body_23, 23);
fixture_case!(ohlc_body_24, 24);
fixture_case!(ohlc_body_25, 25);
fixture_case!(ohlc_body_26, 26);
fixture_case!(ohlc_body_27, 27);
fixture_case!(ohlc_body_28, 28);
fixture_case!(ohlc_body_29, 29);
fixture_case!(ohlc_body_30, 30);
fixture_case!(ohlc_body_31, 31);
fixture_case!(ohlc_body_32, 32);
fixture_case!(ohlc_body_33, 33);
fixture_case!(ohlc_body_34, 34);
fixture_case!(ohlc_body_35, 35);
fixture_case!(ohlc_body_36, 36);
fixture_case!(ohlc_body_37, 37);
fixture_case!(ohlc_body_38, 38);
fixture_case!(ohlc_body_39, 39);
fixture_case!(ohlc_body_40, 40);
fixture_case!(ohlc_body_41, 41);
fixture_case!(ohlc_body_42, 42);
fixture_case!(ohlc_body_43, 43);
fixture_case!(ohlc_body_44, 44);
fixture_case!(ohlc_body_45, 45);
fixture_case!(ohlc_body_46, 46);
fixture_case!(ohlc_body_47, 47);
fixture_case!(ohlc_body_48, 48);
fixture_case!(ohlc_body_49, 49);
fixture_case!(ohlc_body_50, 50);
fixture_case!(ohlc_body_51, 51);
fixture_case!(ohlc_body_52, 52);
fixture_case!(ohlc_body_53, 53);
fixture_case!(ohlc_body_54, 54);
fixture_case!(ohlc_body_55, 55);
fixture_case!(ohlc_body_56, 56);
fixture_case!(ohlc_body_57, 57);
fixture_case!(ohlc_body_58, 58);
fixture_case!(ohlc_body_59, 59);
fixture_case!(ohlc_body_60, 60);
fixture_case!(ohlc_body_61, 61);
fixture_case!(ohlc_body_62, 62);
fixture_case!(ohlc_body_63, 63);
fixture_case!(ohlc_body_64, 64);
fixture_case!(ohlc_body_65, 65);
fixture_case!(ohlc_body_66, 66);
fixture_case!(ohlc_body_67, 67);
fixture_case!(ohlc_body_68, 68);
fixture_case!(ohlc_body_69, 69);
fixture_case!(ohlc_body_70, 70);
fixture_case!(ohlc_body_71, 71);
fixture_case!(ohlc_body_72, 72);
fixture_case!(ohlc_body_73, 73);
fixture_case!(ohlc_body_74, 74);
fixture_case!(ohlc_body_75, 75);
fixture_case!(ohlc_body_76, 76);
fixture_case!(ohlc_body_77, 77);
fixture_case!(ohlc_body_78, 78);
fixture_case!(ohlc_body_79, 79);
