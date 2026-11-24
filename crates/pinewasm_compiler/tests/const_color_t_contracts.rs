//! Local rendering contract regression tests.
use pinewasm_compiler::{compile, CompileOptions, InputValue};
use serde::Deserialize;

#[derive(Deserialize)]
struct Fixture {
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    source: String,
    #[serde(rename = "expectedCompileSuccess")]
    success: bool,
    inputs: Vec<CaseInput>,
}

#[derive(Deserialize)]
struct CaseInput {
    name: String,
    defval: f64,
}

#[test]
fn successful_current_numeric_defaults_fold_direct_and_aliased_constant_accessors() {
    let fixture: Fixture =
        serde_json::from_str(include_str!("fixtures/const_color_t_numeric_cases.json")).unwrap();
    let mut failures = Vec::new();
    for case in fixture.cases {
        assert!(case.success);
        let result = compile(&case.source, CompileOptions::default());
        assert!(result.success, "{:?}", result.diagnostics);
        let program = result.program.unwrap();
        assert_eq!(program.inputs.len(), case.inputs.len());
        for (input, expected) in program.inputs.iter().zip(case.inputs) {
            assert_eq!(input.title.as_deref(), Some(expected.name.as_str()));
            if input.defval != InputValue::Float(expected.defval) {
                failures.push(format!(
                    "{}: expected {}, actual {:?}",
                    expected.name, expected.defval, input.defval
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn run(body: &str) -> pinewasm_compiler::RunResult {
    use pinewasm_compiler::{compile_and_run, MarketData, RunOptions};
    let data = MarketData {
        open: vec![10.0; 3],
        high: vec![12.0; 3],
        low: vec![9.0; 3],
        close: vec![11.0; 3],
        volume: vec![0.0; 3],
        ..MarketData::default()
    };
    let result = compile_and_run(
        &format!("//@version=6\nindicator(\"Const accessor boundary\")\n{body}"),
        &data,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.diagnostics.iter().all(|d| !d.is_error()),
        "{:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    result
}

#[test]
fn const_accessor_changes_numeric_result_without_rewriting_raw_color_storage() {
    use pinewasm_compiler::Value;
    let result = run("c=color.new(#336699,70.9)\nplot(close,color=c)\nplot(color.t(c))");
    assert_eq!(result.plots[1].values, vec![Value::Float(70.0); 3]);
    for value in &result.plots[0].colors[0].values {
        let Value::Color { t, .. } = value else {
            panic!("expected color")
        };
        assert_eq!(*t, 70.9);
    }
}

#[test]
fn internal_nonconst_and_persistent_runtime_accessors_keep_fractional_storage_semantics() {
    use pinewasm_compiler::Value;
    // These check that fractional values are kept as stored in the VM for input,
    // simple, series and persistent sources.
    for body in [
        "t=input.float(70.9)\nc=color.new(#336699,t)\nplot(color.t(c))",
        "simple float t=70.9\nc=color.new(#336699,t)\nplot(color.t(c))",
        "series float t=70.9\nc=color.new(#336699,t)\nplot(color.t(c))",
        "var color c=color.new(#336699,70.9)\nplot(color.t(c))\nc:=color.new(#336699,70.9)",
    ] {
        let result = run(body);
        assert_eq!(
            result.plots[0].values,
            vec![Value::Float(70.9); 3],
            "{body}"
        );
    }
}

#[test]
fn constant_numeric_branches_reach_matching_runtime_colors() {
    use pinewasm_compiler::Value;
    // This source is the numeric-default case selected by id from the fixture
    // below. Numeric input defaults fold to the same const accessor values; the
    // VM must execute those folded producers.
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/arrow_static_role_cases.json")).unwrap();
    let case = fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "arrow-static-role-alpha-exact-numeric")
        .unwrap();
    let source = case["source"].as_str().unwrap();
    // Use the declaration and all six expressions as written.
    use pinewasm_compiler::{compile_and_run, MarketData, RunOptions};
    let data = MarketData {
        open: vec![10.0; 3],
        high: vec![12.0; 3],
        low: vec![9.0; 3],
        close: vec![11.0; 3],
        volume: vec![0.0; 3],
        ..MarketData::default()
    };
    let result = compile_and_run(
        source,
        &data,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.diagnostics.iter().all(|d| !d.is_error()),
        "{:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    assert_eq!(result.plots.len(), 6);
    for plot in result.plots {
        let up = plot
            .colors
            .iter()
            .find(|c| c.role == pinewasm_compiler::PlotColorRole::Up)
            .unwrap();
        for color in &up.values {
            let Value::Color { r, g, b, t, .. } = color else {
                panic!("expected color: {color:?}")
            };
            assert_eq!((*r, *g, *b, *t), (17, 34, 51, 0.0));
        }
    }
}

#[test]
fn internal_color_cast_preserves_known_constant_accessor_provenance() {
    let result=compile("//@version=6\nindicator(\"Cast boundary\")\nx=input.float(color.t(color(color.new(#123456,47.6))))\nplot(x)",CompileOptions::default());
    assert!(result.success, "{:?}", result.diagnostics);
    assert_eq!(
        result.program.unwrap().inputs[0].defval,
        InputValue::Float(47.0)
    );
}

#[test]
fn internal_nested_persistent_producers_and_aliases_are_not_frozen_at_first_bar() {
    use pinewasm_compiler::Value;
    for body in [
        "var float t=70.9\nc=color.new(#336699,t)\nplot(color.t(c))\nt:=20.5",
        "var float t=70.9\nalias=t\nc=color.new(#336699,alias)\nplot(color.t(c))\nt:=20.5",
        "var color base=#336699\nc=color.new(base,70.9)\nplot(color.t(c))\nbase:=na",
    ] {
        let result = run(body);
        let expected = if body.contains("base:=na") {
            vec![Value::Float(70.9), Value::Na, Value::Na]
        } else {
            vec![Value::Float(70.9), Value::Float(20.5), Value::Float(20.5)]
        };
        assert_eq!(result.plots[0].values, expected, "{body}");
    }
}
