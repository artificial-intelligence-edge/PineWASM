use pinewasm_compiler::{
    compile, run_program_profiled, CompileOptions, InputValue, MarketData, Program, RunOptions,
};
use pinewasm_csil::CsilInstr;
use pinewasm_hir::PureFunc;

fn program(color: &str, declaration: &str) -> Program {
    let source = format!("//@version=6\nindicator(\"colors\")\n{declaration}\nplot(close, color={color})\nbarcolor({color})\n");
    let result = compile(&source, CompileOptions::default());
    assert!(result.success, "{:?}", result.diagnostics);
    result.program.unwrap()
}

fn has_constructor(program: &Program) -> bool {
    program.main().code.iter().any(|op| {
        matches!(
            op,
            CsilInstr::CallPure {
                func: PureFunc::ColorNew | PureFunc::ColorRgb,
                ..
            }
        )
    })
}

fn data() -> MarketData {
    MarketData {
        open: vec![1.0, 2.0, 3.0],
        high: vec![1.0, 2.0, 3.0],
        low: vec![1.0, 2.0, 3.0],
        close: vec![1.0, 2.0, 3.0],
        volume: vec![1.0; 3],
        ..MarketData::default()
    }
}

#[test]
fn literal_colors_match_runtime_rounding_clamping_and_visual_channels() {
    for template in [
        "color.new(#12345678, VALUE)",
        "color.rgb(VALUE, 2.5, 260.0, 99.5)",
    ] {
        let dynamic = program(
            &template.replace("VALUE", "channel"),
            "channel = input.float(0.0)",
        );
        assert!(has_constructor(&dynamic));
        for value in [0.0_f64, 0.49, 0.5, 0.51, 99.49, 99.5, 100.5, 255.5, 1e30] {
            let folded = program(&template.replace("VALUE", &format!("{value:.2}")), "");
            assert!(!has_constructor(&folded));
            let mut options = RunOptions::default();
            options
                .inputs
                .insert(dynamic.inputs[0].id, InputValue::Float(value));
            let expected = run_program_profiled(&dynamic, &data(), &options, None);
            let actual = run_program_profiled(&folded, &data(), &RunOptions::default(), None);
            assert!(expected.runtime_errors.is_empty());
            assert!(actual.runtime_errors.is_empty());
            assert_eq!(actual.plots.len(), expected.plots.len());
            for (folded, dynamic) in actual.plots.iter().zip(&expected.plots) {
                assert_eq!(folded.values, dynamic.values, "{template}: {value}");
                assert_eq!(folded.colors.len(), dynamic.colors.len());
                for (fixed_lane, live_lane) in folded.colors.iter().zip(&dynamic.colors) {
                    assert_eq!(fixed_lane.role, live_lane.role, "{template}: {value}");
                    assert_eq!(fixed_lane.values, live_lane.values, "{template}: {value}");
                    // Equal paint does not imply equal palette identity: the
                    // folded constant is editable; the float input is not.
                    assert_eq!(fixed_lane.palette_indices, [Some(0); 3]);
                    assert_eq!(live_lane.palette_indices, [None; 3]);
                }
                // A literal constructor exposes one editable constant paint.
                // A float input constructor cannot enumerate its palette.
                assert!(folded.style.color_editable);
                assert_eq!(folded.style.color_palette.len(), 1);
                assert!(!dynamic.style.color_editable);
                assert!(dynamic.style.color_palette.is_empty());
                let mut folded_style = folded.style.clone();
                folded_style.color_editable = false;
                folded_style.color_palette.clear();
                assert_eq!(folded_style, dynamic.style, "{template}: {value}");
            }
            assert_eq!(actual.visuals.len(), expected.visuals.len());
            for (folded, dynamic) in actual.visuals.iter().zip(&expected.visuals) {
                assert_eq!(folded.slot, dynamic.slot);
                assert_eq!(folded.title, dynamic.title);
                assert_eq!(folded.colors.len(), dynamic.colors.len());
                for (fixed_lane, live_lane) in folded.colors.iter().zip(&dynamic.colors) {
                    assert_eq!(fixed_lane.role, live_lane.role, "{template}: {value}");
                    assert_eq!(fixed_lane.values, live_lane.values, "{template}: {value}");
                    // Equal paint does not imply equal palette identity: the
                    // folded constant is editable; the float input is not.
                    assert_eq!(fixed_lane.palette_indices, [Some(0); 3]);
                    assert_eq!(live_lane.palette_indices, [None; 3]);
                }
                assert!(folded.style.color_editable);
                assert_eq!(folded.style.color_palette.len(), 1);
                assert!(!dynamic.style.color_editable);
                assert!(dynamic.style.color_palette.is_empty());
                let mut folded_style = folded.style.clone();
                folded_style.color_editable = false;
                folded_style.color_palette.clear();
                assert_eq!(folded_style, dynamic.style, "{template}: {value}");
            }
        }
    }
}

#[test]
fn dynamic_constructor_operands_are_evaluated_even_with_na_color() {
    let program = program("color.new(na, array.get(array.new<float>(0), 0))", "");
    assert!(has_constructor(&program));
    let actual = run_program_profiled(&program, &data(), &RunOptions::default(), None);
    assert!(
        !actual.runtime_errors.is_empty(),
        "must evaluate the invalid array read"
    );
}

#[test]
fn literal_na_propagates_and_dynamic_history_stays_live() {
    for color in [
        "color.new(na, 10)",
        "color.new(color.red, na)",
        "color.rgb(na, 2, 3, 4)",
    ] {
        let folded = program(color, "");
        assert!(!has_constructor(&folded));
        let result = run_program_profiled(&folded, &data(), &RunOptions::default(), None);
        assert!(result.runtime_errors.is_empty());
        assert!(result.plots[0]
            .colors
            .iter()
            .flat_map(|lane| &lane.values)
            .all(|v| matches!(v, pinewasm_compiler::Value::Na)));
    }
    let dynamic = program(
        "color.new(#123456, channel[1])",
        "channel = input.float(0.0)",
    );
    assert!(has_constructor(&dynamic));
    for value in [-100.0, -0.5, 10.5, 200.0] {
        let mut options = RunOptions::default();
        options
            .inputs
            .insert(dynamic.inputs[0].id, InputValue::Float(value));
        let result = run_program_profiled(&dynamic, &data(), &options, None);
        assert!(result.runtime_errors.is_empty());
        let colors = &result.plots[0].colors[0].values;
        assert_eq!(colors[0], pinewasm_compiler::Value::Na);
        assert_eq!(
            colors[1],
            pinewasm_compiler::Value::Color {
                r: 18,
                g: 52,
                b: 86,
                t: value.clamp(0.0, 100.0),
                input_origin: None
            }
        );
    }
}
