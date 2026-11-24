//! Compiler-side regression tests for rendering behaviors: outline-only
//! candles, per-object `force_overlay`, nested future cone boxes and a subject
//! `switch` over `input.bool` used as a marker offset.
//!
//! The expectations follow PineWASM's rendering contract for `plotcandle`,
//! the drawing constructors, `box.new` and `plotshape`.
use pinewasm_compiler::{
    compile_and_run, CompileOptions, InputValue, MarketData, RunOptions, RunResult, Value,
};
use pinewasm_hir::{OhlcField, PlotColorRole};
use serde_json::{json, Value as Json};
use std::collections::HashMap;

const FIRST: i64 = 1_700_000_000_000;
const HOUR: i64 = 3_600_000;
const LAST_BAR: i64 = 7;

fn run_with(source: &str, options: &RunOptions) -> RunResult {
    let market = MarketData {
        open: vec![10.0; 8],
        high: vec![12.0; 8],
        low: vec![9.0; 8],
        close: vec![11.0; 8],
        volume: vec![100.0; 8],
        time: (0..8).map(|i| FIRST + i * HOUR).collect(),
        timeframe: "60".into(),
        ..MarketData::default()
    };
    let result = compile_and_run(source, &market, CompileOptions::default(), options);
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
    assert_eq!(result.bars_executed, 8);
    result
}

fn run(source: &str) -> RunResult {
    run_with(source, &RunOptions::default())
}

fn drawings(source: &str) -> Json {
    serde_json::to_value(run(source).drawings).unwrap()
}

fn color(r: u8, g: u8, b: u8, t: f64) -> Value {
    Value::Color {
        r,
        g,
        b,
        t,
        input_origin: None,
    }
}

#[test]
fn plotcandle_with_missing_body_and_wick_keeps_its_border_lane() {
    let result = run(r#"//@version=6
indicator("Outline candles", overlay=true)
plotcandle(open, high, low, close, "Outline", color=na, wickcolor=na, bordercolor=#0088ff)
plotcandle(open, high, low, close, "Transparent", color=color.new(color.red, 100), wickcolor=color.new(color.red, 100), bordercolor=#0088ff)
plotbar(open, high, low, close, "Bar", color=na)
"#);
    assert_eq!(result.plots.len(), 12);
    let fields = [
        OhlcField::Open,
        OhlcField::High,
        OhlcField::Low,
        OhlcField::Close,
    ];
    let border = color(0, 136, 255, 0.0);

    // `na` body and wick are absent, not painted with a fallback; the border
    // is still the caller's color on every OHLC lane.
    for (plot, field) in result.plots[..4].iter().zip(fields) {
        assert_eq!(plot.style.ohlc_field, Some(field));
        let roles: Vec<_> = plot.colors.iter().map(|lane| lane.role).collect();
        assert_eq!(
            roles,
            [
                PlotColorRole::Color,
                PlotColorRole::Wick,
                PlotColorRole::Border
            ]
        );
        for lane in &plot.colors[..2] {
            assert_eq!(lane.values.len(), 8);
            assert!(lane.values.iter().all(Value::is_na), "{:?}", lane.role);
            assert!(lane.palette_indices.iter().all(Option::is_none));
        }
        assert_eq!(plot.colors[2].values, vec![border.clone(); 8]);
        let palette: Vec<_> = plot
            .style
            .color_palette
            .iter()
            .map(|entry| (entry.role, entry.resolved_color.as_deref()))
            .collect();
        assert_eq!(palette, [(PlotColorRole::Border, Some("#0088FF"))]);
    }

    // A fully transparent color is a present color, distinct from `na`.
    let transparent_red = color(242, 54, 69, 100.0);
    for plot in &result.plots[4..8] {
        assert_eq!(plot.colors[0].values, vec![transparent_red.clone(); 8]);
        assert_eq!(plot.colors[1].values, vec![transparent_red.clone(); 8]);
        assert_eq!(plot.colors[2].values, vec![border.clone(); 8]);
        assert_eq!(plot.style.color_palette.len(), 3);
    }

    // `plotbar` has a single color channel, so a missing color leaves nothing
    // to retain.
    for plot in &result.plots[8..] {
        assert_eq!(plot.colors.len(), 1);
        assert_eq!(plot.colors[0].role, PlotColorRole::Color);
        assert!(plot.colors[0].values.iter().all(Value::is_na));
    }
}

#[test]
fn drawing_constructors_serialize_their_own_force_overlay() {
    // In a separate pane, `force_overlay` is the setting that sends an object
    // to the main chart. Each constructor is called once with the flag and
    // once without it.
    let d = drawings(
        r#"//@version=6
indicator("Per-object overlay", overlay=false)
var forced = table.new(position.top_right, 1, 1, force_overlay=true)
var plain = table.new(position.top_left, 1, 1)
if barstate.islast
    p = chart.point.from_index(bar_index - 1, low)
    q = chart.point.from_index(bar_index, high)
    line.new(bar_index - 1, close, bar_index, close, force_overlay=true)
    line.new(bar_index - 1, open, bar_index, open)
    label.new(bar_index, high, "forced", force_overlay=true)
    label.new(bar_index, low, "plain")
    box.new(bar_index - 1, high, bar_index, low, force_overlay=true)
    box.new(bar_index - 1, close, bar_index, open)
    polyline.new(array.from(p, q), force_overlay=true)
    polyline.new(array.from(p, q))
"#,
    );
    for kind in ["lines", "labels", "boxes", "tables", "polylines"] {
        let flags: Vec<_> = d[kind]
            .as_array()
            .unwrap()
            .iter()
            .map(|object| object["fields"]["force_overlay"].clone())
            .collect();
        assert_eq!(flags, [json!(true), json!(false)], "{kind}");
    }
}

/// Expected cone geometry: `steps` columns of three nested boxes, outermost
/// layer first. Returns `(step, alpha, half_height)` in creation order.
fn cone_layers(steps: i64) -> Vec<(i64, &'static str, f64)> {
    let mut layers = Vec::new();
    for step in 0..steps {
        for (layer, alpha) in [(3, "66"), (2, "4D"), (1, "33")] {
            let half_height = (f64::from(layer) / 3.0) * (step + 1) as f64 * 0.5;
            layers.push((step, alpha, half_height));
        }
    }
    layers
}

fn assert_cone(boxes: &Json, xloc: &str, origin: i64, step_width: i64) {
    let boxes = boxes.as_array().unwrap();
    let expected = cone_layers(4);
    assert_eq!(boxes.len(), expected.len());
    for (index, (object, (step, alpha, half_height))) in boxes.iter().zip(expected).enumerate() {
        let fields = &object["fields"];
        // Objects are drawn in id order, so creation order is the layer order.
        assert_eq!(object["id"], json!(index), "{object}");
        assert_eq!(fields["xloc"], xloc);
        assert_eq!(fields["left"], json!(origin + step * step_width));
        assert_eq!(fields["right"], json!(origin + (step + 1) * step_width));
        assert_eq!(fields["bgcolor"], format!("#2962FF{alpha}"));
        assert_eq!(fields["border_color"], Json::Null);
        for (edge, expected) in [("top", 11.0 + half_height), ("bottom", 11.0 - half_height)] {
            let actual = fields[edge].as_f64().unwrap();
            assert!((actual - expected).abs() < 1e-9, "{edge}: {object}");
        }
    }
}

#[test]
fn nested_future_cone_boxes_keep_creation_order_alpha_and_bar_index_anchors() {
    let d = drawings(
        r#"//@version=6
indicator("Nested box layout", overlay=true, max_boxes_count=500)
steps = input.int(4, "Steps")
layers = input.int(3, "Layers")
var array<box> cone = array.new<box>()
if barstate.islast
    while array.size(cone) > 0
        box.delete(array.pop(cone))
    for s = 0 to steps - 1
        for j = layers to 1
            float w = (j / layers) * (s + 1) * 0.5
            array.push(cone, box.new(bar_index + s * 3, close + w, bar_index + (s + 1) * 3, close - w, border_color=na, bgcolor=color.new(color.blue, 90 - j * 10)))
"#,
    );
    // Every anchor is at or beyond the last bar: the cone projects forward.
    assert_cone(&d["boxes"], "xloc.bar_index", LAST_BAR, 3);
}

#[test]
fn nested_future_cone_boxes_keep_future_bar_time_anchors() {
    let d = drawings(
        r#"//@version=6
indicator("Nested box layout by time", overlay=true)
layers = input.int(3, "Layers")
var array<box> cone = array.new<box>()
ms = timeframe.in_seconds() * 1000
if barstate.islast
    while array.size(cone) > 0
        box.delete(array.pop(cone))
    for s = 0 to 3
        for j = layers to 1
            float w = (j / layers) * (s + 1) * 0.5
            array.push(cone, box.new(time + s * 3 * ms, close + w, time + (s + 1) * 3 * ms, close - w, xloc=xloc.bar_time, border_color=na, bgcolor=color.new(color.blue, 90 - j * 10)))
"#,
    );
    assert_cone(
        &d["boxes"],
        "xloc.bar_time",
        FIRST + LAST_BAR * HOUR,
        3 * HOUR,
    );
}

#[test]
fn subject_switch_over_input_bool_binds_the_marker_offset_for_both_settings() {
    let source = r#"//@version=6
indicator("Subject switch offset", overlay=true)
useShift = input.bool(true, "Shift")
off = switch useShift
    true => -1
    => 0
plotshape(bar_index % 2 == 0, style=shape.triangleup, location=location.belowbar, offset=off)
plotshape(bar_index % 2 == 1, style=shape.triangledown, location=location.abovebar, offset=off)
plot(off, "Selected offset")
"#;
    let disabled = RunOptions {
        inputs: HashMap::from([(0, InputValue::Bool(false))]),
        ..RunOptions::default()
    };
    for (options, selected) in [(RunOptions::default(), -1.0), (disabled, 0.0)] {
        let result = run_with(source, &options);
        // The declared offset stays an input expression in both runs: the
        // embedding application resolves it from the same overrides it passed to
        // the run.
        for marker in &result.plots[..2] {
            assert!(
                marker.style.unsupported.is_empty(),
                "{:?}",
                marker.style.unsupported
            );
            assert_eq!(
                serde_json::to_value(&marker.style.bound_scalars).unwrap(),
                json!([{
                    "field": "offset",
                    "expression": {
                        "kind": "ternary",
                        "condition": {
                            "kind": "binary",
                            "op": "eq",
                            "lhs": {"kind": "input", "input": 0},
                            "rhs": {"kind": "value", "value": {"Bool": true}}
                        },
                        "when_true": {
                            "kind": "unary",
                            "op": "neg",
                            "operand": {"kind": "value", "value": {"Int": 1}}
                        },
                        "when_false": {"kind": "value", "value": {"Int": 0}}
                    }
                }])
            );
        }
        // The engine's own evaluation of the same switch is the value that
        // expression must resolve to.
        assert_eq!(result.plots[2].values, vec![Value::Float(selected); 8]);
    }
}
