//! User-defined type field reads are series values: paint modes derived from
//! them are per-bar, and they cannot satisfy a `simple` parameter.

use pinewasm_compiler::{compile, compile_and_run, CompileOptions, MarketData, RunOptions};
use pinewasm_hir::{FillColorMode, PlotColorMode};

fn run(source: &str) -> pinewasm_compiler::RunResult {
    // Bars alternate up/down so `close > open` changes during the run.
    let data = MarketData {
        open: vec![10.0, 12.0, 10.0, 12.0],
        high: vec![13.0; 4],
        low: vec![9.0; 4],
        close: vec![11.0; 4],
        volume: vec![100.0; 4],
        timeframe: "1D".into(),
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
    result
}

/// Distinct values in the first color lane of one output.
fn distinct_colors(colors: &[pinewasm_vm::PlotColorSeries]) -> usize {
    let mut seen: Vec<_> = colors[0]
        .values
        .iter()
        .map(|value| serde_json::to_string(value).unwrap())
        .collect();
    seen.sort();
    seen.dedup();
    seen.len()
}

#[test]
fn plot_colors_read_from_udt_fields_are_series() {
    let result = run(r#"//@version=6
indicator("UDT field plot colors")
type Sty
    color c
    bool up
method tint(Sty self) => self.up ? color.green : color.red
s = Sty.new(close > open ? color.red : color.blue, close > open)
var Sty kept = Sty.new(color.blue, false)
kept.c := close > open ? color.red : color.blue
hist = array.new<Sty>()
hist.push(s)
plot(close, color = s.c)
plot(close, color = s.tint())
plot(close, color = kept.c)
plot(close, color = hist.last().c)
plot(close, color = Sty.new(color.blue, true).c)
plot(close, color = close > open ? color.red : color.blue)
plot(close, color = color.blue)
"#);
    let modes: Vec<_> = result
        .plots
        .iter()
        .map(|plot| plot.style.plot_color_mode)
        .collect();
    use PlotColorMode::{Fixed, Series};
    assert_eq!(
        modes,
        vec![
            Some(Series),
            Some(Series),
            Some(Series),
            Some(Series),
            Some(Series),
            Some(Series),
            Some(Fixed)
        ]
    );
    // The UDT lanes carry the same per-bar colors as the plain series plot.
    for plot in &result.plots[..4] {
        assert_eq!(distinct_colors(&plot.colors), 2, "{:?}", plot.colors);
    }
    for udt in [0, 2, 3] {
        assert_eq!(
            result.plots[udt].colors[0].values,
            result.plots[5].colors[0].values
        );
    }
    // A field holding one color for the whole run is still a series read.
    assert_eq!(
        result.plots[4].colors[0].values,
        result.plots[6].colors[0].values
    );
    let program = result.compile.as_ref().unwrap().program.as_ref().unwrap();
    for (runtime, declared) in result.plots.iter().zip(&program.plots) {
        assert_eq!(
            runtime.style.plot_color_mode,
            declared.style.plot_color_mode
        );
    }
}

#[test]
fn fill_colors_and_gradient_bounds_read_from_udt_fields_are_series() {
    let result = run(r#"//@version=6
indicator("UDT field fill colors")
type Sty
    color c
type Band
    float top
    float bottom
s = Sty.new(close > open ? color.red : color.blue)
b = Band.new(high, close > open ? low : open)
p1 = plot(high, display = display.none)
p2 = plot(low, display = display.none)
fill(p1, p2, color = s.c)
fill(p1, p2, b.top, b.bottom, color.red, color.blue)
fill(p1, p2, color = color.blue)
fill(p1, p2, 13, 9, color.red, color.blue)
"#);
    let modes: Vec<_> = result
        .fills
        .iter()
        .map(|fill| fill.style.fill_color_mode)
        .collect();
    use FillColorMode::{Fixed, Series};
    assert_eq!(
        modes,
        vec![Some(Series), Some(Series), Some(Fixed), Some(Fixed)]
    );
    assert_eq!(distinct_colors(&result.fills[0].colors), 2);
}

#[test]
fn udt_fields_do_not_satisfy_simple_parameters() {
    for (body, code) in [
        ("plot(close, offset = s.w)", "E0206"),
        ("plot(ta.ema(close, s.w))", "E0219"),
        ("simple color k = s.c\nplot(close, color = k)", "E0226"),
        ("simple int n = s.w\nplot(n)", "E0226"),
    ] {
        let source = format!(
            "//@version=6\nindicator(\"UDT field simple\")\ntype Sty\n    color c\n    int w\ns = Sty.new(close > open ? color.red : color.blue, close > open ? 2 : 3)\n{body}\n"
        );
        let result = compile(&source, CompileOptions::default());
        assert!(!result.success, "unexpectedly accepted {body}");
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code.as_deref() == Some(code)),
            "{body}: {:?}",
            result.diagnostics
        );
    }
}

#[test]
fn udt_fields_keep_their_values_and_series_uses() {
    let result = run(r#"//@version=6
indicator("UDT field values")
type Cfg
    int w
    float level
length = input.int(2)
c = Cfg.new(length, close > open ? 1.5 : 2.5)
series int width = c.w
plot(ta.ema(close, length))
plot(c.level * width)
plot(math.max(c.level, 2.0), offset = length)
plot(ta.sma(close, c.w))
"#);
    assert_eq!(
        serde_json::to_string(&result.plots[1].values).unwrap(),
        "[3.0,5.0,3.0,5.0]"
    );
    assert_eq!(
        serde_json::to_string(&result.plots[3].values).unwrap(),
        "[null,11.0,11.0,11.0]"
    );
}

#[test]
fn udt_field_history_stays_rejected() {
    for body in [
        "plot(p.x[1])",
        "plot((p.x)[1])",
        "n = bar_index > 1 ? 1 : 0\nplot(p.x[n])",
        "plot(pts.get(0).x[1])",
        "plot((pts.get(0).x)[1])",
        "n = bar_index > 1 ? 1 : 0\nplot(pts.get(0).x[n])",
    ] {
        let source = format!(
            "//@version=6\nindicator(\"UDT field history\")\ntype P\n    float x\np = P.new(close)\npts = array.new<P>()\npts.push(p)\n{body}\n"
        );
        let result = compile(&source, CompileOptions::default());
        assert!(!result.success, "unexpectedly accepted {body}");
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code.as_deref() == Some("E0209")),
            "{body}: {:?}",
            result.diagnostics
        );
    }
    // A variable bound to the field read is an ordinary series with history.
    let result = run(r#"//@version=6
indicator("UDT field history")
type P
    float x
p = P.new(open)
q = p.x
plot(q[1])
"#);
    assert_eq!(
        serde_json::to_string(&result.plots[0].values).unwrap(),
        "[null,10.0,12.0,10.0]"
    );
}

#[test]
fn history_of_getter_and_call_results_stays_valid() {
    // Drawing getters and user functions or methods that return a field lower
    // to a field read, but `[]` applies to the call result, which has a
    // history. Only `[]` written directly on a field is `E0209`.
    let result = run(r#"//@version=6
indicator("Getter and call history", overlay = true)
type P
    float x
getX(P p) => p.x
method mx(P self) => self.x
p = P.new(open)
pts = array.new<P>()
pts.push(p)
var line l = line.new(0, open, 1, open)
line.set_xy1(l, bar_index, open)
var label lb = label.new(0, open, "probe")
label.set_y(lb, open)
var box bx = box.new(0, open, 1, low)
box.set_top(bx, open)
n = bar_index >= 0 ? 1 : 0
plot(line.get_x1(l)[1])
plot(line.get_y1(l)[1])
plot(l.get_y1()[1])
plot(label.get_y(lb)[1])
plot(box.get_top(bx)[1])
plot(getX(p)[1])
plot(p.mx()[1])
plot(pts.get(0).mx()[1])
plot(line.get_y1(l)[n])
plot(getX(p)[n])
plot(p.mx()[n])
plot(pts.get(0).mx()[n])
plot((p.x * 2)[1])
"#);
    let values: Vec<_> = result
        .plots
        .iter()
        .map(|plot| serde_json::to_string(&plot.values).unwrap())
        .collect();
    let (first, rest) = values.split_first().unwrap();
    let (last, middle) = rest.split_last().unwrap();
    // `x1` is set to `bar_index` on every bar.
    assert_eq!(first, "[null,0.0,1.0,2.0]");
    // Every other getter and call returns the bar's `open`.
    assert_eq!(middle.len(), 11);
    for (index, value) in middle.iter().enumerate() {
        assert_eq!(value, "[null,10.0,12.0,10.0]", "plot {}", index + 1);
    }
    assert_eq!(last, "[null,20.0,24.0,20.0]");
}
