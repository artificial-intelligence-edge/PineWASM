//! Cases for color-comparison qualifiers and input-only hline colors.
use pinewasm_compiler::{compile, compile_and_run, CompileOptions, MarketData, RunOptions};

fn rejects_hline_color(prelude: &str, expression: &str) {
    for call in [
        format!("hline(1, color={expression})"),
        format!("hline(1, \"Guide\", {expression})"),
    ] {
        let source = format!("//@version=6\nindicator(\"Color boundary\")\n{prelude}\n{call}\n");
        let result = compile(&source, CompileOptions::default());
        assert!(!result.success, "unexpectedly accepted {source}");
        assert!(
            result.diagnostics.iter().any(|diagnostic| {
                diagnostic.code.as_deref() == Some("E0206")
                    && diagnostic.message.contains("color")
                    && diagnostic.message.contains("series")
            }),
            "{source}\n{:?}",
            result.diagnostics
        );
    }
}

#[test]
fn hline_rejects_theme_color_selected_by_color_equality() {
    let prelude = r"base = input.color(color.blue)
light = (color.r(chart.bg_color) + color.g(chart.bg_color) + color.b(chart.bg_color)) / 3 > 128
theme = light and base == color.blue ? color.navy : base";
    rejects_hline_color(prelude, "color.new(theme, 55)");
}

#[test]
fn hline_rejects_color_comparison_conditions_for_every_operand_qualifier() {
    for operator in ["==", "!="] {
        for (prelude, left, right) in [
            ("", "color.blue", "color.red"),
            ("", "#112233", "#223344"),
            ("base = input.color(color.blue)", "base", "color.red"),
            ("", "chart.bg_color", "color.black"),
            ("simple color base = color.blue", "base", "color.red"),
            (
                "base = close > open ? color.blue : color.red",
                "base",
                "color.red",
            ),
        ] {
            let selected = format!("{left} {operator} {right} ? color.navy : color.blue");
            rejects_hline_color(prelude, &selected);
            let alias = format!("{prelude}\nsame = {left} {operator} {right}\nselected = same ? color.navy : color.blue");
            rejects_hline_color(&alias, "color.new(selected, 55)");
        }
    }
}

#[test]
fn hline_accepts_input_colors_and_noncolor_comparison_conditions() {
    let source = r#"//@version=6
indicator("Valid input colors")
base = input.color(color.blue)
choose = input.bool(true)
light = color.r(chart.bg_color) > 128
hline(0, color=color.new(base, 55))
hline(1, color=color.new(chart.bg_color, 55))
hline(2, color=light ? color.navy : base)
hline(3, color=choose ? color.navy : base)
hline(4, color=1 == 2 ? color.navy : base)
hline(5, color="left" != "right" ? color.navy : base)
hline(6, color=1.0 != 2.0 ? color.navy : base)
"#;
    let result = compile(source, CompileOptions::default());
    assert!(result.success, "{:?}", result.diagnostics);
}

#[test]
fn color_comparison_cannot_initialize_const_or_simple_bool() {
    for qualifier in ["const", "simple"] {
        for comparison in ["color.blue == color.red", "#112233 != #223344"] {
            let source = format!("//@version=6\nindicator(\"Boolean boundary\")\n{qualifier} bool same = {comparison}\nplot(same ? 1 : 0)\n");
            let result = compile(&source, CompileOptions::default());
            assert!(!result.success, "unexpectedly accepted {source}");
            assert!(
                result
                    .diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.message.contains("series")),
                "{:?}",
                result.diagnostics
            );
        }
    }
}

#[test]
fn color_comparison_through_function_stays_series_qualified() {
    let prelude =
        "same(a, b) => a == b\nselected = same(color.blue, color.red) ? color.navy : color.blue";
    rejects_hline_color(prelude, "selected");
}

#[test]
fn color_comparison_series_qualification_preserves_runtime_values() {
    let source = r#"//@version=6
indicator("Color comparison values")
base = close > open ? color.blue : color.red
plot(base == color.blue ? 1 : 0)
plot(base != color.blue ? 1 : 0)
plot(color.blue == color.blue ? 1 : 0)
plot(#112233 != #223344 ? 1 : 0)
"#;
    let market = MarketData {
        open: vec![1.0, 2.0],
        high: vec![2.0, 2.0],
        low: vec![1.0, 1.0],
        close: vec![2.0, 1.0],
        volume: vec![1.0; 2],
        ..MarketData::default()
    };
    let result = compile_and_run(
        source,
        &market,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result
            .compile
            .as_ref()
            .is_some_and(|compiled| compiled.success),
        "{:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    for (plot, expected) in
        result
            .plots
            .iter()
            .zip(["[1.0,0.0]", "[0.0,1.0]", "[1.0,1.0]", "[1.0,1.0]"])
    {
        assert_eq!(serde_json::to_string(&plot.values).unwrap(), expected);
    }
    assert_eq!(result.plots.len(), 4);
}
