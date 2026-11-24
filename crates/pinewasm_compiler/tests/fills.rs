//! End-to-end tests for plot-id capture and `fill(...)` bands.
//!
//! PineWASM's `plot()`/`hline()` return a plot id; capturing one (`p = plot(close)`)
//! and passing two ids to `fill(id1, id2, color)` shades the band between them.
//! These tests assert the *runtime values* a host renders with: which plots a
//! band spans, and its per-bar color — not merely that the script compiles.

use pinewasm_compiler::{compile, compile_and_run, CompileOptions, MarketData, RunOptions, Value};

fn data() -> MarketData {
    MarketData {
        open: vec![9.0, 11.0, 12.0, 10.5],
        high: vec![10.5, 12.5, 12.0, 11.0],
        low: vec![8.5, 10.5, 10.0, 9.5],
        close: vec![10.0, 12.0, 11.0, 10.75],
        volume: vec![1000.0, 1500.0, 800.0, 1200.0],
        ..MarketData::default()
    }
}

fn run_ok(source: &str) -> pinewasm_compiler::RunResult {
    let run = compile_and_run(
        source,
        &data(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        run.diagnostics.iter().all(|d| !d.is_error()),
        "unexpected compile errors: {:?}",
        run.diagnostics
    );
    assert!(
        run.runtime_errors.is_empty(),
        "unexpected runtime errors: {:?}",
        run.runtime_errors
    );
    run
}

fn rgba(value: &Value) -> (u8, u8, u8, f64) {
    match value {
        Value::Color { r, g, b, t, .. } => (*r, *g, *b, *t),
        other => panic!("expected a color, found {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Plot-id capture
// ---------------------------------------------------------------------------

#[test]
fn plot_result_can_be_captured_and_still_plots() {
    // Capturing the id must not suppress the plot itself.
    let run = run_ok(
        r"//@version=6
indicator('capture')
p = plot(close, 'Close')
",
    );
    assert_eq!(run.plots.len(), 1, "the captured plot is still emitted");
    assert_eq!(run.plots[0].title.as_deref(), Some("Close"));
    assert_eq!(run.plots[0].values[0], Value::Float(10.0));
    assert_eq!(run.plots[0].values[3], Value::Float(10.75));
}

#[test]
fn captured_plot_keeps_source_order() {
    // The plot statement is emitted before the declaration that captures it,
    // so slot order matches source order even when captures and bare plots mix.
    let run = run_ok(
        r"//@version=6
indicator('order')
plot(open, 'A')
b = plot(high, 'B')
plot(low, 'C')
",
    );
    let titles: Vec<_> = run
        .plots
        .iter()
        .map(|p| p.title.clone().unwrap_or_default())
        .collect();
    assert_eq!(titles, vec!["A", "B", "C"]);
}

#[test]
fn plot_handle_is_not_a_number() {
    // A plot id is its own type: arithmetic on it must not silently work.
    let result = compile(
        r"//@version=6
indicator('bad')
p = plot(close)
plot(p + 1)
",
        CompileOptions::default(),
    );
    assert!(!result.success, "arithmetic on a plot id must be rejected");
}

#[test]
fn plot_in_local_scope_is_still_rejected_when_captured() {
    let result = compile(
        r"//@version=6
indicator('bad')
if close > 0
    p = plot(close)
",
        CompileOptions::default(),
    );
    assert!(
        !result.success,
        "plot is global-scope only, captured or not"
    );
}

// ---------------------------------------------------------------------------
// fill(...) bands
// ---------------------------------------------------------------------------

#[test]
fn fill_records_endpoints_and_per_bar_color() {
    let run = run_ok(
        r"//@version=6
indicator('band')
top = plot(high, 'Top')
bot = plot(low, 'Bot')
fill(top, bot, color = color.new(#FF0000, 70), title = 'Band')
",
    );
    assert_eq!(run.fills.len(), 1, "one band");
    let band = &run.fills[0];
    assert_eq!(band.title.as_deref(), Some("Band"));
    // The band spans the two captured plots, in argument order.
    assert_eq!(band.plot1, 0);
    assert_eq!(band.plot2, 1);
    assert_eq!(run.plots[band.plot1 as usize].title.as_deref(), Some("Top"));
    assert_eq!(run.plots[band.plot2 as usize].title.as_deref(), Some("Bot"));
    // The color channel carries one sample per bar.
    assert_eq!(band.colors.len(), 1);
    let values = &band.colors[0].values;
    assert_eq!(values.len(), 4, "one color per bar");
    for value in values {
        assert_eq!(rgba(value), (255, 0, 0, 70.0));
    }
}

#[test]
fn fill_color_varies_per_bar() {
    // A gradient fill has a color that varies per bar: the color is a
    // series, so each bar must record its own sample rather than one constant.
    let run = run_ok(
        r"//@version=6
indicator('gradient')
top = plot(high, 'Top')
bot = plot(low, 'Bot')
fill(top, bot, color = close > 10.5 ? color.new(#00FF00, 50) : color.new(#FF0000, 50))
",
    );
    let values = &run.fills[0].colors[0].values;
    // close = [10.0, 12.0, 11.0, 10.75] → red, green, green, green.
    assert_eq!(rgba(&values[0]), (255, 0, 0, 50.0));
    assert_eq!(rgba(&values[1]), (0, 255, 0, 50.0));
    assert_eq!(rgba(&values[2]), (0, 255, 0, 50.0));
    assert_eq!(rgba(&values[3]), (0, 255, 0, 50.0));
}

#[test]
fn fill_between_hlines() {
    // `hline` ids are fill endpoints too, and each hline is a flat data plot.
    let run = run_ok(
        r"//@version=6
indicator('zone')
h1 = hline(1.0, 'Upper')
h2 = hline(-1.0, 'Lower')
fill(h1, h2, color = color.new(#0000FF, 80))
",
    );
    assert_eq!(run.fills.len(), 1);
    let band = &run.fills[0];
    assert_eq!(run.plots[band.plot1 as usize].values[0], Value::Float(1.0));
    assert_eq!(run.plots[band.plot2 as usize].values[0], Value::Float(-1.0));
    assert_eq!(rgba(&band.colors[0].values[0]), (0, 0, 255, 80.0));
}

#[test]
fn multiple_fills_get_distinct_slots() {
    // Three bands: each needs its own slot and color stream.
    let run = run_ok(
        r"//@version=6
indicator('layers')
base = plot(0.0, 'Base')
a = plot(close, 'A')
b = plot(open, 'B')
fill(base, a, color = color.new(#111111, 10))
fill(base, b, color = color.new(#222222, 20))
",
    );
    assert_eq!(run.fills.len(), 2);
    assert_eq!(run.fills[0].slot, 0);
    assert_eq!(run.fills[1].slot, 1);
    assert_eq!((run.fills[0].plot1, run.fills[0].plot2), (0, 1));
    assert_eq!((run.fills[1].plot1, run.fills[1].plot2), (0, 2));
    assert_eq!(rgba(&run.fills[0].colors[0].values[0]), (17, 17, 17, 10.0));
    assert_eq!(rgba(&run.fills[1].colors[0].values[0]), (34, 34, 34, 20.0));
}

#[test]
fn fill_with_na_color_records_na() {
    // The `cond ? color : na` idiom hides the band on some bars; `na` must
    // survive to the host rather than becoming an error or a default color.
    let run = run_ok(
        r"//@version=6
indicator('gated')
top = plot(high)
bot = plot(low)
fill(top, bot, color = close > 10.5 ? color.new(#00FF00, 50) : na)
",
    );
    let values = &run.fills[0].colors[0].values;
    assert_eq!(values[0], Value::Na);
    assert_eq!(rgba(&values[1]), (0, 255, 0, 50.0));
}

#[test]
fn fill_rejects_a_non_plot_endpoint() {
    let result = compile(
        r"//@version=6
indicator('bad')
top = plot(high)
fill(top, close, color = color.red)
",
        CompileOptions::default(),
    );
    assert!(
        !result.success,
        "a series endpoint is not a plot id and must be rejected"
    );
}

// A gradient fill takes two hlines as its endpoints, positionally or under
// the `hline1`/`hline2` names, with bounds and colors that come from inputs.
// Keep runtime endpoint and paint checks independent.
#[test]
fn hline_gradient_with_input_bounds_preserves_runtime_boundaries_and_paints() {
    let source = r#"//@version=6
indicator("hline gradient with input bounds")
upperBound = input.int(75)
lowerBound = input.int(25)
upperPaint = input.color(color.teal)
lowerPaint = input.color(color.orange)
upperLine = hline(75, color = upperPaint, linestyle = hline.style_dotted)
lowerLine = hline(25, color = lowerPaint, linestyle = hline.style_dotted)
fill(upperLine, lowerLine, upperBound, lowerBound, upperPaint, lowerPaint)"#;
    for source in [
        source.to_string(),
        source.replace(
            "fill(upperLine, lowerLine, upperBound, lowerBound, upperPaint, lowerPaint)",
            "fill(hline2=lowerLine, hline1=upperLine, top_value=upperBound, bottom_value=lowerBound, top_color=upperPaint, bottom_color=lowerPaint)",
        ),
    ] {
        let run = run_ok(&source);
        assert_eq!(run.plots.len(), 2);
        assert_eq!(run.fills.len(), 1);
        let fill = &run.fills[0];
        assert_eq!((fill.plot1, fill.plot2), (0, 1));
        assert!(run.plots.iter().all(|plot| plot.style.kind == pinewasm_hir::DisplayKind::Hline));
        assert_eq!(run.plots[0].values, vec![Value::Float(75.0); 4]);
        assert_eq!(run.plots[1].values, vec![Value::Float(25.0); 4]);
        assert_eq!(fill.colors.len(), 4);
        let lane = |role| &fill.colors.iter().find(|lane| lane.role == role).unwrap().values;
        use pinewasm_hir::PlotColorRole;
        assert_eq!(*lane(PlotColorRole::TopValue), vec![Value::Float(75.0); 4]);
        assert_eq!(*lane(PlotColorRole::BottomValue), vec![Value::Float(25.0); 4]);
        for value in lane(PlotColorRole::TopColor) {
            assert_eq!(rgba(value), (8, 153, 129, 0.0));
        }
        for value in lane(PlotColorRole::BottomColor) {
            assert_eq!(rgba(value), (255, 152, 0, 0.0));
        }
    }
}

#[test]
fn hline_gradient_inline_handles_execute_once_and_keep_dynamic_paints() {
    let run = run_ok(
        r#"//@version=6
indicator("Dynamic hline gradient")
fill(hline(100), hline(0), close, low, close > 10.5 ? #00FF00 : #FF0000, color.new(#0000FF, 70))
"#,
    );
    assert_eq!(run.plots.len(), 2, "each inline hline executes once");
    assert_eq!(run.fills.len(), 1);
    let fill = &run.fills[0];
    assert_eq!((fill.plot1, fill.plot2), (0, 1));
    assert!(run
        .plots
        .iter()
        .all(|plot| plot.style.kind == pinewasm_hir::DisplayKind::Hline));
    use pinewasm_hir::PlotColorRole;
    let lane = |role| {
        &fill
            .colors
            .iter()
            .find(|lane| lane.role == role)
            .unwrap()
            .values
    };
    assert_eq!(
        *lane(PlotColorRole::TopValue),
        vec![
            Value::Float(10.0),
            Value::Float(12.0),
            Value::Float(11.0),
            Value::Float(10.75)
        ]
    );
    assert_eq!(
        *lane(PlotColorRole::BottomValue),
        vec![
            Value::Float(8.5),
            Value::Float(10.5),
            Value::Float(10.0),
            Value::Float(9.5)
        ]
    );
    assert_eq!(rgba(&lane(PlotColorRole::TopColor)[0]), (255, 0, 0, 0.0));
    for value in &lane(PlotColorRole::TopColor)[1..] {
        assert_eq!(rgba(value), (0, 255, 0, 0.0));
    }
    for value in lane(PlotColorRole::BottomColor) {
        assert_eq!(rgba(value), (0, 0, 255, 70.0));
    }
}
