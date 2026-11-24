//! Rendering regression tests for the VM.
//!
//! The expected hex values come from PineWASM's named-color constant table in
//! tests/fixtures/rendering/named-colors.json.
use pinewasm_compiler::{compile_and_run, CompileOptions, MarketData, RunOptions, Value};

fn check_named_color(name: &str) {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/rendering/named-colors.json"
    ))
    .unwrap();
    let colors = fixture["colors"].as_array().unwrap();
    assert_eq!(colors.len(), 17);
    let entry = colors.iter().find(|entry| entry["title"] == name).unwrap();
    let hex = entry["color"].as_str().unwrap();
    let channel = |offset| u8::from_str_radix(&hex[offset..offset + 2], 16).unwrap();
    let (r, g, b) = (channel(1), channel(3), channel(5));
    let source = format!(
        "//@version=6\nindicator(\"Named color constants\")\nc=color.{name}\nplot(close,title=\"Paint\",color=c)\nplot(color.r(c))\nplot(color.g(c))\nplot(color.b(c))\nplot(color.t(c))\nplot(c == {hex} ? 1.0 : 0.0)\n"
    );
    let result = compile_and_run(
        &source,
        &MarketData {
            open: vec![10.0],
            high: vec![12.0],
            low: vec![9.0],
            close: vec![11.0],
            volume: vec![0.0],
            ..MarketData::default()
        },
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.diagnostics.iter().all(|d| !d.is_error()),
        "{name}: {:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{name}: {:?}",
        result.runtime_errors
    );
    assert_eq!(result.bars_executed, 1);
    assert_eq!(result.plots.len(), 6);
    assert_eq!(result.plots[0].colors.len(), 1);
    assert_eq!(
        result.plots[0].colors[0].role,
        pinewasm_hir::PlotColorRole::Color
    );
    assert_eq!(
        result.plots[0].colors[0].values,
        vec![Value::Color {
            r,
            g,
            b,
            t: 0.0,
            input_origin: None
        }],
        "color.{name}: explicit runtime paint value differs from the expected hex value"
    );
    assert_eq!(result.plots[0].style.color_palette.len(), 1);
    assert_eq!(
        result.plots[0].style.color_palette[0]
            .resolved_color
            .as_deref(),
        Some(hex.to_uppercase().as_str())
    );
    for (plot, expected) in result.plots[1..]
        .iter()
        .zip([r as f64, g as f64, b as f64, 0.0, 1.0])
    {
        assert_eq!(
            plot.values,
            vec![Value::Float(expected)],
            "color.{name}: channel/literal equality"
        );
    }
}

macro_rules! named_color_tests {
    ($($name:ident),+ $(,)?) => { $(
        #[test]
        fn $name() { check_named_color(stringify!($name)); }
    )+ };
}
named_color_tests!(
    aqua, black, fuchsia, gray, green, lime, maroon, navy, olive, orange, purple, red, silver,
    teal, white, yellow
);

// Declared as a standalone test rather than through the macro list, so that
// every named-color test has a visible declaration in the source.
#[test]
fn blue() {
    check_named_color("blue");
}
