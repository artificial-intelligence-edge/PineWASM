//! Local rendering contract regression tests.
use pinewasm_compiler::{compile_and_run, CompileOptions, MarketData, RunOptions};
use serde::Deserialize;
use serde_json::Value;
#[derive(Deserialize)]
struct Fixture {
    cases: Vec<Case>,
}
#[derive(Deserialize)]
struct Case {
    id: String,
    source: String,
    expected: Vec<Value>,
    #[serde(rename = "expectedCompileSuccess")]
    success: bool,
}
#[test]
fn static_arrow_roles_compile_and_run() {
    let fixture: Fixture =
        serde_json::from_str(include_str!("fixtures/arrow_static_role_cases.json")).unwrap();
    let data = MarketData {
        open: vec![10.0; 3],
        high: vec![12.0; 3],
        low: vec![9.0; 3],
        close: vec![11.0, 10.0, 12.0],
        volume: vec![0.0; 3],
        ..MarketData::default()
    };
    let mut failures = Vec::new();
    for case in fixture.cases {
        assert!(case.success);
        let run = compile_and_run(
            &case.source,
            &data,
            CompileOptions::default(),
            &RunOptions::default(),
        );
        assert!(
            run.diagnostics.iter().all(|d| !d.is_error()),
            "{}: {:?}",
            case.id,
            run.diagnostics
        );
        assert!(
            run.runtime_errors.is_empty(),
            "{}: {:?}",
            case.id,
            run.runtime_errors
        );
        assert_eq!(run.plots.len(), case.expected.len(), "{}", case.id);
        for (plot, expected) in run.plots.iter().zip(case.expected) {
            let style = serde_json::to_value(&plot.style).unwrap();
            if style.get("arrowStaticColors") != Some(&expected) {
                failures.push(format!(
                    "{}: expected {}, actual {:?}",
                    case.id,
                    expected,
                    style.get("arrowStaticColors")
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn program(source: &str) -> pinewasm_compiler::Program {
    let result = pinewasm_compiler::compile(source, CompileOptions::default());
    assert!(result.success, "{:?}", result.diagnostics);
    result.program.unwrap()
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
fn input_override_changes_rows_but_never_static_direction_fallback() {
    use pinewasm_compiler::{run_program, InputValue};
    let p=program("//@version=6\nindicator(\"input separation\")\nup=input.color(color.new(#336699,70))\nplotarrow(close-close[1],colorup=up,colordown=#993366)");
    let before = run_program(&p, &data(), &RunOptions::default());
    let mut options = RunOptions::default();
    options.inputs.insert(
        p.inputs[0].id,
        InputValue::Color {
            r: 102,
            g: 153,
            b: 51,
            t: 20.0,
        },
    );
    let after = run_program(&p, &data(), &options);
    assert!(before.runtime_errors.is_empty());
    assert!(after.runtime_errors.is_empty());
    assert_eq!(
        before.plots[0].style.arrow_static_colors,
        after.plots[0].style.arrow_static_colors
    );
    assert_eq!(
        after.plots[0]
            .style
            .arrow_static_colors
            .as_ref()
            .unwrap()
            .up
            .origin,
        pinewasm_compiler::ArrowStaticColorOrigin::Dynamic
    );
    assert_ne!(
        before.plots[0].colors[0].values,
        after.plots[0].colors[0].values
    );
}
#[test]
fn missing_row_color_retains_numeric_sample_and_separate_static_style() {
    let p=program("//@version=6\nindicator(\"missing row\")\nplotarrow(close,colorup=bar_index==1?na:color.new(#336699,70),colordown=color.new(#993366,40))");
    let run = pinewasm_compiler::run_program(&p, &data(), &RunOptions::default());
    assert!(run.runtime_errors.is_empty());
    assert_eq!(
        run.plots[0].values[1],
        pinewasm_compiler::Value::Float(10.0)
    );
    let up = run.plots[0]
        .colors
        .iter()
        .find(|c| c.role == pinewasm_compiler::PlotColorRole::Up)
        .unwrap();
    assert!(up.values[1].is_na());
    let style = run.plots[0].style.arrow_static_colors.as_ref().unwrap();
    assert_eq!(style.up.t, 0.0);
    assert_eq!((style.up.r, style.up.g, style.up.b), (0, 255, 0));
    assert_eq!(style.down.t, 40.0);
}
#[test]
fn each_callsite_reads_its_own_nonpersistent_assignment() {
    let p=program("//@version=6\nindicator(\"callsite order\")\ncolor UP=color.new(#336699,70)\nplotarrow(close,colorup=UP)\nUP:=#669933\nplotarrow(close,colorup=UP)");
    let a = p.plots[0].style.arrow_static_colors.as_ref().unwrap();
    let b = p.plots[1].style.arrow_static_colors.as_ref().unwrap();
    assert_eq!((a.up.r, a.up.g, a.up.b, a.up.t), (51, 102, 153, 70.0));
    assert_eq!((b.up.r, b.up.g, b.up.b, b.up.t), (102, 153, 51, 0.0));
}
#[test]
fn unproven_constant_folding_has_an_explicit_diagnostic() {
    let p=program("//@version=6\nindicator(\"unproven pure folding\")\nplotarrow(close,colorup=color.new(#336699,math.tan(-70)))");
    assert!(p.plots[0].style.arrow_static_colors.is_none());
    assert!(p.plots[0]
        .style
        .unsupported
        .iter()
        .any(|v| v.field == "colorup" && v.reason.contains("cannot be proven")));
}
#[test]
fn generated_wire_domains_describe_every_static_field() {
    let contract = pinewasm_compiler::rendering_contract();
    assert_eq!(
        contract["dtoFields"]["ArrowStaticColors"],
        serde_json::json!(["up", "down"])
    );
    assert_eq!(
        contract["dtoFields"]["ArrowStaticRoleColor"],
        serde_json::json!(["origin", "r", "g", "b", "t"])
    );
    assert_eq!(
        contract["enums"]["ArrowStaticColorOrigin"],
        serde_json::json!(["omitted", "constant", "missing", "dynamic"])
    );
    assert!(contract["optionalStyleFields"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("arrowStaticColors")));
}
#[test]
fn serialized_metadata_is_strict_complete_and_semantically_validated() {
    use pinewasm_compiler::ArrowStaticColorOrigin as Origin;
    let original=program("//@version=6\nindicator(\"metadata validation\")\nplotarrow(close,colorup=color.new(#336699,70.25),colordown=na)");
    let roundtrip = pinewasm_compiler::Program::from_json(&original.to_json()).unwrap();
    assert_eq!(roundtrip.plots[0].style, original.plots[0].style);
    assert_eq!(
        roundtrip.plots[0]
            .style
            .arrow_static_colors
            .as_ref()
            .unwrap()
            .up
            .t,
        70.0
    );
    assert!(pinewasm_csil::validate(&roundtrip).is_ok());
    for t in [f64::NAN, f64::INFINITY, -0.01, 100.01] {
        let mut p = original.clone();
        p.plots[0].style.arrow_static_colors.as_mut().unwrap().up.t = t;
        assert!(pinewasm_csil::validate(&p).is_err());
    }
    for origin in [Origin::Missing, Origin::Omitted, Origin::Dynamic] {
        let mut p = original.clone();
        p.plots[0]
            .style
            .arrow_static_colors
            .as_mut()
            .unwrap()
            .up
            .origin = origin;
        assert!(pinewasm_csil::validate(&p).is_err());
    }
    let mut p = original.clone();
    p.plots[0].style.kind = pinewasm_compiler::DisplayKind::Plot;
    assert!(pinewasm_csil::validate(&p).is_err());
    let json = serde_json::to_value(&original).unwrap();
    for field in ["up", "down"] {
        let mut j = json.clone();
        j["plots"][0]["style"]["arrowStaticColors"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        assert!(serde_json::from_value::<pinewasm_compiler::Program>(j).is_err());
    }
    for field in ["origin", "r", "g", "b", "t"] {
        let mut j = json.clone();
        j["plots"][0]["style"]["arrowStaticColors"]["up"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        assert!(serde_json::from_value::<pinewasm_compiler::Program>(j).is_err());
    }
    let mut j = json.clone();
    j["plots"][0]["style"]["arrowStaticColors"]["up"]["extra"] = serde_json::json!(true);
    assert!(serde_json::from_value::<pinewasm_compiler::Program>(j).is_err());
    let mut j = json.clone();
    j["plots"][0]["style"]["arrowStaticColors"]["up"]["origin"] = serde_json::json!("guessed");
    assert!(serde_json::from_value::<pinewasm_compiler::Program>(j).is_err());
    let mut j = json;
    j["plots"][0]["style"]
        .as_object_mut()
        .unwrap()
        .remove("arrowStaticColors");
    // A record without either optional arrow extension still validates. A present
    // runtime binding deliberately requires the complete static fallback.
    j["plots"][0]["style"]
        .as_object_mut()
        .unwrap()
        .remove("arrowRuntimeColorers");
    let legacy: pinewasm_compiler::Program = serde_json::from_value(j).unwrap();
    assert!(pinewasm_csil::validate(&legacy).is_ok());
}

#[test]
fn persistent_condition_reachability_is_not_frozen_to_the_first_bar() {
    let p=program("//@version=6\nindicator(\"cross-bar reachability\")\nvar bool gate=false\nvar color UP=color.new(#336699,70)\nif gate\n    UP:=#669933\nplotarrow(close,colorup=UP)\ngate:=true");
    let run = pinewasm_compiler::run_program(&p, &data(), &RunOptions::default());
    assert!(run.runtime_errors.is_empty());
    let up = run.plots[0]
        .colors
        .iter()
        .find(|v| v.role == pinewasm_compiler::PlotColorRole::Up)
        .unwrap();
    assert_ne!(up.values[0], up.values[1]);
    assert_eq!(
        run.plots[0]
            .style
            .arrow_static_colors
            .as_ref()
            .unwrap()
            .up
            .origin,
        pinewasm_compiler::ArrowStaticColorOrigin::Dynamic
    );
}
