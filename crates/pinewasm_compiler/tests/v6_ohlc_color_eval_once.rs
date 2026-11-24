//! `plotbar`/`plotcandle` surface one call as four OHLC plots. PineWASM
//! evaluates each call argument once per bar, in parameter order (also when the
//! arguments are named and written in another order), so a color argument
//! with a side effect (an inline `array.shift`) runs once per bar and all four
//! OHLC channels carry the same color. Expectations assume one call with one
//! evaluation of each argument, in the parameter order of PineWASM's built-in
//! signatures `plotbar(open, high, low, close, title, color, ...)` and
//! `plotcandle(open, high, low, close, title, color, wickcolor,
//! editable, show_last, bordercolor, ...)`.
use pinewasm_compiler::{
    compile, compile_and_run, CompileOptions, MarketData, PlotColorRole, RunOptions, RunResult,
    Value,
};

const BARS: usize = 3;

fn run(body: &str) -> RunResult {
    run_on(body, &[10.0; BARS], &[11.0; BARS])
}

/// Run over `BARS` daily bars with the given open and close per bar (high 12,
/// low 9 on every bar).
fn run_on(body: &str, open: &[f64; BARS], close: &[f64; BARS]) -> RunResult {
    let market = MarketData {
        open: open.to_vec(),
        high: vec![12.0; BARS],
        low: vec![9.0; BARS],
        close: close.to_vec(),
        volume: vec![100.0; BARS],
        time: (0..BARS as i64)
            .map(|i| 1_700_000_000_000 + i * 86_400_000)
            .collect(),
        timeframe: "1D".into(),
        ..MarketData::default()
    };
    let result = compile_and_run(
        &format!("//@version=6\nindicator(\"OHLC color once\",overlay=true)\n{body}"),
        &market,
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
    assert_eq!(result.bars_executed as usize, BARS);
    result
}

/// An opaque `#0000bb` color as `(r, g, b, transparency)`.
type Rgba = (u8, u8, u8, f64);

fn blue(b: u8) -> Rgba {
    (0, 0, b, 0.0)
}

fn numbers(result: &RunResult, slot: usize) -> Vec<f64> {
    result.plots[slot]
        .values
        .iter()
        .map(|value| match value {
            Value::Int(n) => *n as f64,
            Value::Float(n) => *n,
            other => panic!("plot {slot}: {other:?} is not a number"),
        })
        .collect()
}

fn lane(result: &RunResult, slot: usize, role: PlotColorRole) -> Vec<Rgba> {
    result.plots[slot]
        .colors
        .iter()
        .find(|lane| lane.role == role)
        .unwrap_or_else(|| panic!("plot {slot} has no {role:?} lane"))
        .values
        .iter()
        .map(|value| match value {
            Value::Color { r, g, b, t, .. } => (*r, *g, *b, *t),
            other => panic!("plot {slot} {role:?}: {other:?} is not a color"),
        })
        .collect()
}

/// Twelve distinct colors `#000001..#00000c`; `array.shift` returns them in order.
const TWELVE: &str = "var a = array.from(#000001,#000002,#000003,#000004,#000005,#000006,#000007,#000008,#000009,#00000a,#00000b,#00000c)";

fn assert_shared_lane(result: &RunResult, role: PlotColorRole, expected: &[Rgba]) {
    for slot in 0..4 {
        assert_eq!(
            lane(result, slot, role),
            expected,
            "OHLC channel {slot} {role:?} lane"
        );
    }
}

#[test]
fn plotbar_inline_shift_color_runs_once_per_bar() {
    let r = run(&format!(
        "{TWELVE}\nplotbar(open,high,low,close,color=array.shift(a))\nplot(array.size(a))"
    ));
    assert_eq!(
        numbers(&r, 4),
        [11.0, 10.0, 9.0],
        "remaining after each bar"
    );
    assert_shared_lane(&r, PlotColorRole::Color, &[blue(1), blue(2), blue(3)]);
}

#[test]
fn plotcandle_inline_shift_colors_run_once_per_bar() {
    let r = run(&format!(
        "{TWELVE}\nvar w = array.copy(a)\nvar e = array.copy(a)\nplotcandle(open,high,low,close,color=array.shift(a),wickcolor=array.shift(w),bordercolor=array.shift(e))\nplot(array.size(a))\nplot(array.size(w))\nplot(array.size(e))"
    ));
    for slot in 4..7 {
        assert_eq!(numbers(&r, slot), [11.0, 10.0, 9.0], "plot {slot}");
    }
    let expected = [blue(1), blue(2), blue(3)];
    for role in [
        PlotColorRole::Color,
        PlotColorRole::Wick,
        PlotColorRole::Border,
    ] {
        assert_shared_lane(&r, role, &expected);
    }
}

/// PineWASM evaluates arguments in parameter order: the `open` argument shifts before
/// the color argument reads the size, so the color sees the shifted array.
#[test]
fn ohlc_data_arguments_evaluate_before_a_captured_color() {
    let r = run(
        "var a = array.from(1.0,2.0,3.0,4.0,5.0,6.0,7.0,8.0,9.0,10.0,11.0,12.0)\nplotbar(array.shift(a),12,9,11,color=color.rgb(0,0,array.size(a)))\nplot(array.size(a))",
    );
    assert_eq!(numbers(&r, 0), [1.0, 2.0, 3.0], "open channel");
    assert_eq!(
        numbers(&r, 4),
        [11.0, 10.0, 9.0],
        "remaining after each bar"
    );
    assert_shared_lane(&r, PlotColorRole::Color, &[blue(11), blue(10), blue(9)]);
}

/// PineWASM evaluates named arguments in parameter order, not in the order they
/// are written (the same rule as for `table.cell`; see
/// `table_cell_reordered_named_arguments_evaluate_in_parameter_order` in
/// `pinewasm_vm`). The color is written first, but its parameter comes after
/// `open`, so `open` shifts before the color reads the size: 11, 10, 9, the
/// same as the positional call above. Written order would give 12, 11, 10.
#[test]
fn named_ohlc_arguments_evaluate_in_parameter_order() {
    let r = run(
        "var a = array.from(1.0,2.0,3.0,4.0,5.0,6.0,7.0,8.0,9.0,10.0,11.0,12.0)\nplotbar(color=color.rgb(0,0,array.size(a)),open=array.shift(a),high=12,low=9,close=11)\nplot(array.size(a))",
    );
    assert_eq!(numbers(&r, 0), [1.0, 2.0, 3.0], "open channel");
    assert_eq!(
        numbers(&r, 4),
        [11.0, 10.0, 9.0],
        "remaining after each bar"
    );
    assert_shared_lane(&r, PlotColorRole::Color, &[blue(11), blue(10), blue(9)]);
}

/// A shared array of tokens 1, 2, 3, ...; every argument takes the next
/// token, so each token shows when its argument ran.
const TOKENS: &str =
    "var a = array.new_float()\nif bar_index == 0\n    for i = 1 to 30\n        array.push(a, i)";

/// The four OHLC number channels and each color lane (shared by all four
/// channels) of one `plotbar`/`plotcandle` call.
fn ohlc_series(result: &RunResult, roles: &[PlotColorRole]) -> (Vec<Vec<f64>>, Vec<Vec<Rgba>>) {
    let data = (0..4).map(|slot| numbers(result, slot)).collect();
    let colors = roles
        .iter()
        .map(|role| {
            let first = lane(result, 0, *role);
            assert_shared_lane(result, *role, &first);
            first
        })
        .collect();
    (data, colors)
}

/// A fully reordered named `plotbar` with a shared mutating array must
/// consume the array in parameter order (open, high, low, close, color),
/// exactly as the positional call does, not in written order (color,
/// close, low, high, open).
#[test]
fn reordered_named_plotbar_matches_the_positional_call() {
    let positional = run(&format!(
        "{TOKENS}\nplotbar(array.shift(a),array.shift(a),array.shift(a),array.shift(a),color=color.rgb(0,0,array.shift(a)))"
    ));
    let named = run(&format!(
        "{TOKENS}\nplotbar(color=color.rgb(0,0,array.shift(a)),close=array.shift(a),low=array.shift(a),high=array.shift(a),open=array.shift(a))"
    ));
    let roles = [PlotColorRole::Color];
    let expected = ohlc_series(&positional, &roles);
    // Parameter order, five tokens per bar.
    assert_eq!(
        expected.0,
        [
            vec![1.0, 6.0, 11.0],
            vec![2.0, 7.0, 12.0],
            vec![3.0, 8.0, 13.0],
            vec![4.0, 9.0, 14.0]
        ]
    );
    assert_eq!(expected.1, [vec![blue(5), blue(10), blue(15)]]);
    assert_eq!(ohlc_series(&named, &roles), expected);
}

/// The same for `plotcandle`, whose color parameters are `color`, `wickcolor`
/// and `bordercolor` in that order (with `editable` and `show_last` between
/// `wickcolor` and `bordercolor`): seven tokens per bar.
#[test]
fn reordered_named_plotcandle_matches_the_positional_call() {
    let positional = run(&format!(
        "{TOKENS}\nplotcandle(array.shift(a),array.shift(a),array.shift(a),array.shift(a),color=color.rgb(0,0,array.shift(a)),wickcolor=color.rgb(0,0,array.shift(a)),bordercolor=color.rgb(0,0,array.shift(a)))"
    ));
    let named = run(&format!(
        "{TOKENS}\nplotcandle(bordercolor=color.rgb(0,0,array.shift(a)),wickcolor=color.rgb(0,0,array.shift(a)),color=color.rgb(0,0,array.shift(a)),close=array.shift(a),low=array.shift(a),high=array.shift(a),open=array.shift(a))"
    ));
    let roles = [
        PlotColorRole::Color,
        PlotColorRole::Wick,
        PlotColorRole::Border,
    ];
    let expected = ohlc_series(&positional, &roles);
    assert_eq!(
        expected.0,
        [
            vec![1.0, 8.0, 15.0],
            vec![2.0, 9.0, 16.0],
            vec![3.0, 10.0, 17.0],
            vec![4.0, 11.0, 18.0]
        ]
    );
    assert_eq!(
        expected.1,
        [
            vec![blue(5), blue(12), blue(19)],
            vec![blue(6), blue(13), blue(20)],
            vec![blue(7), blue(14), blue(21)]
        ]
    );
    assert_eq!(ohlc_series(&named, &roles), expected);
}

/// A `table.cell`-style check for `plotcandle`: each argument is a helper that
/// pushes its parameter position (1..7) to `order`, and the call names them in
/// reverse. PineWASM evaluates them in parameter order, so the plotted number is
/// 1234567 (written order would give 7654321). User function calls run
/// where the arguments are bound, in parameter order, so this guards that
/// path. The `ta.cum` color still advances once per bar.
#[test]
fn reordered_named_plotcandle_helpers_run_in_parameter_order() {
    let r = run(r#"var int[] order = array.new_int()
f_open() =>
    array.push(order, 1)
    open
f_high() =>
    array.push(order, 2)
    high
f_low() =>
    array.push(order, 3)
    low
f_close() =>
    array.push(order, 4)
    close
f_color() =>
    array.push(order, 5)
    color.rgb(0, 0, ta.cum(1))
f_wick() =>
    array.push(order, 6)
    #000006
f_border() =>
    array.push(order, 7)
    #000007
array.clear(order)
plotcandle(bordercolor = f_border(), wickcolor = f_wick(), color = f_color(), close = f_close(), low = f_low(), high = f_high(), open = f_open())
float code = 0.0
for i = 0 to array.size(order) - 1
    code := code * 10 + array.get(order, i)
plot(code, "Evaluation order")"#);
    assert_eq!(numbers(&r, 4), [1234567.0; BARS], "evaluation order");
    assert_eq!(numbers(&r, 0), [10.0; BARS], "open channel");
    assert_eq!(numbers(&r, 3), [11.0; BARS], "close channel");
    assert_shared_lane(&r, PlotColorRole::Color, &[blue(1), blue(2), blue(3)]);
    assert_shared_lane(&r, PlotColorRole::Wick, &[blue(6); BARS]);
    assert_shared_lane(&r, PlotColorRole::Border, &[blue(7); BARS]);
}

/// An engine-backed `ta.*` call site keeps state, so it must advance once per
/// bar, not once per OHLC channel: `ta.cum(1)` is 1, 2, 3 on bars 0, 1, 2.
#[test]
fn plotcandle_ta_color_advances_once_per_bar() {
    let r = run("plotcandle(open,high,low,close,color=color.rgb(0,0,ta.cum(1)))");
    assert_shared_lane(&r, PlotColorRole::Color, &[blue(1), blue(2), blue(3)]);
}

/// A slot-based `ta.*` call site (`ta.change`) reads its own source history.
/// `ta.change(bar_index * 3)` is `na` on bar 0 (shown as 7 by `nz`), then 3.
/// Repeating such a site in one bar stores the same source value again, so
/// this passes without the capture too; it guards that the captured site
/// keeps its history.
#[test]
fn plotbar_slot_ta_color_keeps_its_history() {
    let r = run("plotbar(open,high,low,close,color=color.rgb(0,0,nz(ta.change(bar_index*3),7)))");
    assert_shared_lane(&r, PlotColorRole::Color, &[blue(7), blue(3), blue(3)]);
}

/// `math.random` draws a new number on every evaluation. The call draws one
/// per bar, so all four OHLC channels carry the same color on each bar. The exact numbers are not asserted: they depend on the generator.
#[test]
fn plotbar_random_color_is_one_draw_per_bar() {
    let r = run("plotbar(open,high,low,close,color=color.rgb(math.random(0,255,7),0,0))");
    let first = lane(&r, 0, PlotColorRole::Color);
    assert_eq!(first.len(), BARS);
    assert_shared_lane(&r, PlotColorRole::Color, &first);
}

/// Colors that are safe to repeat are not captured. A fixed literal stays
/// fixed, and a series ternary keeps its per-bar value: close > open on bars
/// 0 and 2 only, so the wick is #000001, #000002, #000001.
#[test]
fn fixed_and_series_colors_keep_their_per_bar_values() {
    let r = run_on(
        "plotcandle(open,high,low,close,color=#000007,wickcolor=close>open?#000001:#000002)",
        &[10.0, 11.0, 10.0],
        &[11.0, 10.0, 11.0],
    );
    assert_shared_lane(&r, PlotColorRole::Color, &[blue(7), blue(7), blue(7)]);
    assert_shared_lane(&r, PlotColorRole::Wick, &[blue(1), blue(2), blue(1)]);
}

/// An argument that already failed to type-check is not captured into a
/// hidden slot: the call reports only the original errors.
#[test]
fn error_typed_ohlc_arguments_report_only_their_own_errors() {
    for (source, count) in [
        ("plotbar(open,high,low,close,color=array.shift(missing))", 1),
        (
            "plotcandle(array.shift(missing),high,low,close,color=array.shift(missing))",
            2,
        ),
    ] {
        let result = compile(
            &format!("//@version=6\nindicator(\"OHLC color errors\")\n{source}"),
            CompileOptions::default(),
        );
        let errors: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.is_error())
            .map(|d| (d.code.clone(), d.message.clone()))
            .collect();
        assert_eq!(errors.len(), count, "{source}: {errors:?}");
        assert!(
            errors
                .iter()
                .all(|(code, message)| code.as_deref() == Some("E0201")
                    && message == "unknown identifier `missing`"),
            "{source}: {errors:?}"
        );
    }
}
