//! Configured background and foreground colors must affect Pine expressions, not only paint.
use pinewasm_compiler::{compile_and_run, CompileOptions, RunOptions};
use pinewasm_mir::Value;
use pinewasm_vm::MarketData;

#[test]
fn chart_theme_controls_colors_and_script_branches_without_leaking() {
    let source = "indicator(\"Theme\")\nplot(color.r(chart.bg_color))\nplot(color.g(chart.fg_color))\nplot(color.r(chart.bg_color) > 128 ? 1 : 0)\n";
    let market = MarketData {
        open: vec![1.0],
        high: vec![1.0],
        low: vec![1.0],
        close: vec![1.0],
        volume: vec![1.0],
        time: vec![0],
        ..Default::default()
    };
    for (background, foreground, expected) in [
        (
            Some([16, 16, 16]),
            Some([210, 211, 212]),
            [16.0, 211.0, 0.0],
        ),
        (
            Some([240, 241, 242]),
            Some([20, 21, 22]),
            [240.0, 21.0, 1.0],
        ),
        (None, None, [255.0, 0.0, 1.0]),
    ] {
        let result = compile_and_run(
            source,
            &market,
            CompileOptions {
                chart_background: background,
                chart_foreground: foreground,
                ..Default::default()
            },
            &RunOptions::default(),
        );
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(
            result.runtime_errors.is_empty(),
            "{:?}",
            result.runtime_errors
        );
        let actual: Vec<_> = result
            .plots
            .iter()
            .map(|plot| match plot.values.first() {
                Some(Value::Float(v)) => *v,
                Some(Value::Int(v)) => *v as f64,
                Some(other) => panic!("unexpected {other:?}"),
                None => panic!("plot has no values"),
            })
            .collect();
        assert_eq!(actual, expected);
    }
}
