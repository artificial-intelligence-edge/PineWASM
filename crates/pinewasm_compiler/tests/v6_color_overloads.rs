//! Color overload calls: accessor type inference, serialized integer accessors, and rejected base types.
use pinewasm_compiler::{compile, CompileOptions};
use pinewasm_span::Severity;

#[test]
fn color_accessors_infer_reference_float_lanes_without_an_explicit_annotation() {
    for accessor in ["color.r", "color.g", "color.b", "color.t"] {
        let result = compile(
            &format!("//@version=6\nindicator(\"Color result type\")\nc = input.color(color.red)\nvalue = {accessor}(c)\nplot(value)\n"),
            CompileOptions::default(),
        );
        assert!(result.success, "{accessor}: {:?}", result.diagnostics);
        let program = result.program.unwrap();
        let function = program
            .main()
            .code
            .iter()
            .find_map(|instruction| match instruction {
                pinewasm_csil::CsilInstr::CallPure { func, .. } if func.name() == accessor => {
                    Some(*func)
                }
                _ => None,
            })
            .expect("compiled accessor call");
        assert_eq!(
            pinewasm_csil::pure_result_ty(function, 1, &[pinewasm_csil::AbstractTy::Color]),
            pinewasm_csil::AbstractTy::F64,
            "{accessor}"
        );
        let rejected = compile(
            &format!("//@version=6\nindicator(\"Wrong color result type\")\nc = input.color(color.red)\nint value = {accessor}(c)\nplot(value)\n"),
            CompileOptions::default(),
        );
        assert!(!rejected.success, "{accessor} returned an implicit integer");
    }
}

#[test]
fn legacy_serialized_color_accessors_keep_their_integer_contract() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/legacy-color-accessors.json")).unwrap();
    assert_eq!(fixture["program"]["header"]["il_version"], 2);
    let program: pinewasm_compiler::Program =
        serde_json::from_value(fixture["program"].clone()).unwrap();
    pinewasm_csil::validate(&program).unwrap();
    for instruction in &program.main().code {
        if let pinewasm_csil::CsilInstr::CallPure { func, .. } = instruction {
            assert!(!func.color_channel_is_float());
            assert_eq!(
                pinewasm_csil::pure_result_ty(*func, 1, &[pinewasm_csil::AbstractTy::Color]),
                pinewasm_csil::AbstractTy::Int
            );
        }
    }
    let data = pinewasm_compiler::MarketData {
        close: vec![10.0; 3],
        open: vec![9.0; 3],
        high: vec![11.0; 3],
        low: vec![8.0; 3],
        volume: vec![100.0; 3],
        ..Default::default()
    };
    let result =
        pinewasm_compiler::run_program(&program, &data, &pinewasm_compiler::RunOptions::default());
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    assert_eq!(result.bars_executed, 3);
    for (plot, expected) in result.plots.iter().zip([18.0, 52.0, 86.0, 0.0]) {
        assert_eq!(
            plot.values,
            vec![pinewasm_compiler::Value::Float(expected); 3]
        );
    }
}

#[test]
fn color_overloads_reject_wrong_base_types_and_missing_required_arguments() {
    for body in [
        "color.new(1, 20)",
        "color.rgb(color.red, 0, 0)",
        "color.r(\"red\")",
        "color.from_gradient(1.0, 0.0, 2.0, 0.0, color.blue)",
        "color.new(color.red)",
        "color.rgb(255, 0)",
        "color.from_gradient(1.0, 0.0, 2.0, color.red)",
    ] {
        let result = compile(
            &format!("//@version=6\nindicator(\"Color negative case\")\n{body}\nplot(close)\n"),
            CompileOptions::default(),
        );
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.severity == Severity::Error),
            "unexpectedly accepted `{body}`"
        );
    }
}
