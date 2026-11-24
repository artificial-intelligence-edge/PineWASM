//! Local rendering contract regression tests.
use pinewasm_compiler::{compile, run_program, CompileOptions, MarketData, RunOptions};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct Fixture {
    cases: Vec<Case>,
    #[serde(rename = "rejectedCases")]
    rejected_cases: Vec<Case>,
}
#[derive(Deserialize)]
struct Case {
    id: String,
    source: String,
    #[serde(default)]
    expected: Vec<Value>,
    #[serde(rename = "expectedCompileSuccess")]
    success: bool,
}
fn fixture() -> Fixture {
    serde_json::from_str(include_str!("fixtures/arrow_runtime_colorer_cases.json")).unwrap()
}
fn data() -> MarketData {
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
fn successful_arrow_colorer_targets_bind_declarations_and_snapshots() {
    let mut failures = Vec::new();
    for case in fixture().cases {
        assert!(case.success);
        let result = compile(&case.source, CompileOptions::default());
        assert!(result.success, "{}: {:?}", case.id, result.diagnostics);
        let program = result.program.unwrap();
        let run = run_program(&program, &data(), &RunOptions::default());
        assert!(
            run.runtime_errors.is_empty(),
            "{}: {:?}",
            case.id,
            run.runtime_errors
        );
        assert_eq!(program.plots.len(), case.expected.len(), "{}", case.id);
        assert_eq!(run.plots.len(), case.expected.len(), "{}", case.id);
        for (i, expected) in case.expected.iter().enumerate() {
            let declaration = serde_json::to_value(&program.plots[i].style).unwrap();
            let snapshot = serde_json::to_value(&run.plots[i].style).unwrap();
            for (owner, style) in [("declaration", declaration), ("snapshot", snapshot)] {
                if style.get("arrowRuntimeColorers") != Some(expected) {
                    failures.push(format!(
                        "{} arrow {i} {owner}: expected {expected}, actual {:?}",
                        case.id,
                        style.get("arrowRuntimeColorers")
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
#[test]
fn rejected_programs_never_supply_colorer_expectations() {
    for case in fixture().rejected_cases {
        assert!(!case.success);
        assert!(case.expected.is_empty());
    }
}

fn program(source: &str) -> pinewasm_compiler::Program {
    let result = compile(source, CompileOptions::default());
    assert!(result.success, "{:?}", result.diagnostics);
    result.program.unwrap()
}
#[test]
fn fixed_role_preserves_its_raw_runtime_lane() {
    let p = program("//@version=6\nindicator(\"fixed role raw separation\")\nplotarrow(close,colorup=color.new(#336699,70),colordown=#993366)");
    assert_eq!(
        p.plots[0].style.arrow_runtime_colorers,
        Some(pinewasm_compiler::ArrowRuntimeColorers {
            up: false,
            down: false
        })
    );
    assert!(p.plots[0]
        .color_roles
        .contains(&pinewasm_compiler::PlotColorRole::Up));
    let run = run_program(&p, &data(), &RunOptions::default());
    let up = run.plots[0]
        .colors
        .iter()
        .find(|lane| lane.role == pinewasm_compiler::PlotColorRole::Up)
        .unwrap();
    assert_eq!(up.values.len(), 3);
    assert!(up.values.iter().all(|value| matches!(
        value,
        pinewasm_compiler::Value::Color {
            r: 51,
            g: 102,
            b: 153,
            t: 70.0,
            ..
        }
    )));
    let mut legacy = p.clone();
    legacy.plots[0].style.arrow_runtime_colorers = None;
    let old = run_program(&legacy, &data(), &RunOptions::default());
    assert_eq!(old.plots[0].values, run.plots[0].values);
    assert_eq!(old.plots[0].colors, run.plots[0].colors);
}
#[test]
fn bindings_are_optional_complete_boolean_records_on_arrows_only() {
    let p=program("//@version=6\nindicator(\"binding wire\")\nplotarrow(close,colorup=bar_index%2==0?#336699:#669933)");
    let contract = pinewasm_compiler::rendering_contract();
    assert_eq!(
        contract["dtoFields"]["ArrowRuntimeColorers"],
        serde_json::json!(["up", "down"])
    );
    assert!(contract["optionalStyleFields"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("arrowRuntimeColorers")));
    for invalid in [
        serde_json::json!({"up":true}),
        serde_json::json!({"up":true,"down":false,"extra":true}),
        serde_json::json!({"up":"true","down":false}),
    ] {
        let mut json = serde_json::to_value(&p).unwrap();
        json["plots"][0]["style"]["arrowRuntimeColorers"] = invalid;
        assert!(serde_json::from_value::<pinewasm_compiler::Program>(json).is_err());
    }
    let other = program("//@version=6\nindicator(\"other family\")\nplot(close,color=#336699)");
    assert!(other.plots[0].style.arrow_runtime_colorers.is_none());
    let mut wrong = other;
    wrong.plots[0].style.arrow_runtime_colorers = p.plots[0].style.arrow_runtime_colorers;
    wrong.plots[0].style.arrow_static_colors = p.plots[0].style.arrow_static_colors.clone();
    assert!(pinewasm_csil::validate(&wrong).is_err());
}
#[test]
fn true_bindings_require_a_declared_role_and_static_record() {
    let p=program("//@version=6\nindicator(\"binding admission\")\nplotarrow(close,colorup=bar_index%2==0?#336699:#669933)");
    assert!(p.plots[0].style.arrow_runtime_colorers.unwrap().up);
    let mut missing_role = p.clone();
    missing_role.plots[0]
        .color_roles
        .retain(|role| *role != pinewasm_compiler::PlotColorRole::Up);
    let errors = pinewasm_csil::validate(&missing_role).unwrap_err();
    assert!(errors
        .iter()
        .any(|error| format!("{error:?}").contains("requires its declared color role")));
    let mut missing_static = p;
    missing_static.plots[0].style.arrow_static_colors = None;
    let errors = pinewasm_csil::validate(&missing_static).unwrap_err();
    assert!(errors
        .iter()
        .any(|error| format!("{error:?}").contains("require complete plotarrow static colors")));
}
#[test]
fn unresolved_pure_fold_does_not_guess_a_binding() {
    let p=program("//@version=6\nindicator(\"unknown binding\")\nplotarrow(close,colorup=color.new(#336699,math.tan(-70)))");
    assert!(p.plots[0].style.arrow_runtime_colorers.is_none());
    assert!(p.plots[0].style.unsupported.iter().any(|option| option
        .reason
        .contains("arrow runtime colorer cannot be proven")));
}
