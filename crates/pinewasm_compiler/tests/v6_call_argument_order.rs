//! Arguments of a call are evaluated once, left to right, in bound
//! parameter order (also when they are named and written in another order;
//! see `table_cell_reordered_named_arguments_evaluate_in_parameter_order` in
//! `pinewasm_vm`). A user function in a later argument therefore runs after
//! an earlier argument that has a side effect.
//!
//! PineWASM inlines a user function by running its body statements before the
//! statement that holds the call. Without care that moves the body ahead of an
//! earlier argument's inline side effect: in `plot(array.pop(a), color=f())`
//! where `f` pushes onto `a`, the push ran first and the pop took the pushed
//! value. Every case below uses one shape:
//!
//! - `a` starts as `[1, 2, 3]`;
//! - the earlier argument pops `a` (an inline builtin side effect);
//! - the later argument calls a user function that pushes `100 + bar_index`.
//!
//! In parameter order bar 0 pops `3` and then pushes `100`, so the popped
//! values are `3, 100, 101` and the last element of `a` after the call is
//! `100, 101, 102`. The wrong order pushes first and pops the value it just
//! pushed: `100, 101, 102` popped and `3, 3, 3` left at the end.
use pinewasm_compiler::{
    compile_and_run, CompileOptions, MarketData, PlotColorRole, RealtimeSim, RunOptions, RunResult,
    Tick, Value,
};

const BARS: usize = 3;

/// The popped values in parameter order.
const POPPED: [f64; BARS] = [3.0, 100.0, 101.0];
/// The last element of `a` after the call, in parameter order.
const TAIL: [f64; BARS] = [100.0, 101.0, 102.0];

const PRELUDE: &str = "var a = array.from(1.0, 2.0, 3.0)
f_color() =>
    array.push(a, 100.0 + bar_index)
    #0000ff
f_num() =>
    array.push(a, 100.0 + bar_index)
    0.0
f_int() =>
    array.push(a, 100.0 + bar_index)
    0
f_text() =>
    array.push(a, 100.0 + bar_index)
    \"t\"
";

fn run(body: &str) -> RunResult {
    let market = MarketData {
        open: vec![10.0; BARS],
        high: vec![12.0; BARS],
        low: vec![9.0; BARS],
        close: vec![11.0; BARS],
        volume: vec![100.0; BARS],
        time: (0..BARS as i64)
            .map(|i| 1_700_000_000_000 + i * 86_400_000)
            .collect(),
        timeframe: "1D".into(),
        ..MarketData::default()
    };
    let result = compile_and_run(
        &format!(
            "//@version=6\nindicator(\"Argument order\", overlay=true, max_lines_count=500, max_labels_count=500, max_boxes_count=500)\n{PRELUDE}{body}\nplot(array.get(a, 2), \"tail\")\n"
        ),
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

fn number(value: &Value) -> f64 {
    match value {
        Value::Int(n) => *n as f64,
        Value::Float(n) => *n,
        other => panic!("{other:?} is not a number"),
    }
}

fn plot_values(result: &RunResult, title: &str) -> Vec<f64> {
    result
        .plots
        .iter()
        .find(|plot| plot.title.as_deref() == Some(title))
        .unwrap_or_else(|| panic!("no plot titled {title}"))
        .values
        .iter()
        .map(number)
        .collect()
}

fn slot_values(result: &RunResult, slot: usize) -> Vec<f64> {
    result.plots[slot].values.iter().map(number).collect()
}

fn blue_lane(result: &RunResult, slot: usize, role: PlotColorRole) -> Vec<f64> {
    result.plots[slot]
        .colors
        .iter()
        .find(|lane| lane.role == role)
        .unwrap_or_else(|| panic!("plot {slot} has no {role:?} lane"))
        .values
        .iter()
        .map(|value| match value {
            Value::Color { r: 0, g: 0, b, .. } => f64::from(*b),
            other => panic!("plot {slot} {role:?}: {other:?} is not a blue"),
        })
        .collect()
}

fn assert_tail(result: &RunResult) {
    assert_eq!(
        plot_values(result, "tail"),
        TAIL,
        "the earlier argument's pop must run before the later argument's user function"
    );
}

#[test]
fn plot_series_pop_runs_before_a_user_function_color() {
    let r = run("plot(array.pop(a), \"value\", color = f_color())");
    assert_eq!(plot_values(&r, "value"), POPPED);
    assert_tail(&r);
}

#[test]
fn plot_reordered_named_arguments_keep_parameter_order() {
    // Written color first; `series` is parameter 0, so the pop still runs first.
    let r = run("plot(color = f_color(), title = \"value\", series = array.pop(a))");
    assert_eq!(plot_values(&r, "value"), POPPED);
    assert_tail(&r);
}

#[test]
fn plot_color_pop_runs_before_a_later_user_function() {
    // `color` (parameter 2) pops inside a color builtin; `linewidth` comes
    // later, and so does the user function in it.
    let r = run(
        "plot(close, \"value\", color = color.rgb(0, 0, array.pop(a)), linewidth = f_int() + 1)",
    );
    assert_eq!(blue_lane(&r, 0, PlotColorRole::Color), POPPED);
    assert_tail(&r);
}

#[test]
fn binary_operand_pop_runs_before_a_user_function_operand() {
    let r = run("plot(array.pop(a) + f_num(), \"value\")");
    assert_eq!(plot_values(&r, "value"), POPPED);
    assert_tail(&r);
}

#[test]
fn builtin_call_argument_pop_runs_before_a_user_function_argument() {
    let r = run("plot(math.max(array.pop(a), f_num()), \"value\")");
    assert_eq!(plot_values(&r, "value"), POPPED);
    assert_tail(&r);
}

#[test]
fn plotshape_series_pop_runs_before_a_user_function_color() {
    let r = run("plotshape(array.pop(a) > 0, color = f_color())");
    assert_tail(&r);
}

#[test]
fn plotchar_series_pop_runs_before_a_user_function_color() {
    let r = run("plotchar(array.pop(a), \"value\", color = f_color())");
    assert_eq!(slot_values(&r, 0), POPPED);
    assert_tail(&r);
}

#[test]
fn plotcandle_open_pop_runs_before_a_user_function_color() {
    let r = run("plotcandle(array.pop(a), high, low, close, color = f_color())");
    assert_eq!(slot_values(&r, 0), POPPED, "open channel");
    assert_tail(&r);
}

#[test]
fn plotbar_open_pop_runs_before_a_user_function_color() {
    let r = run("plotbar(array.pop(a), high, low, close, color = f_color())");
    assert_eq!(slot_values(&r, 0), POPPED, "open channel");
    assert_tail(&r);
}

#[test]
fn plotcandle_color_pop_runs_before_a_user_function_wickcolor() {
    // Both colors are parameters after the OHLC values; `color` comes before
    // `wickcolor`. All four OHLC channels still share one color per bar.
    let r = run(
        "plotcandle(open, high, low, close, color = color.rgb(0, 0, array.pop(a)), wickcolor = f_color())",
    );
    for slot in 0..4 {
        assert_eq!(
            blue_lane(&r, slot, PlotColorRole::Color),
            POPPED,
            "channel {slot}"
        );
    }
    assert_tail(&r);
}

#[test]
fn fill_top_value_pop_runs_before_a_user_function_bottom_color() {
    let r = run(
        "p1 = plot(high)\np2 = plot(low)\nfill(p1, p2, array.pop(a), 0.0, color.red, f_color())",
    );
    assert_tail(&r);
}

#[test]
fn bgcolor_color_pop_runs_before_a_user_function_offset() {
    let r = run("bgcolor(color.rgb(0, 0, array.pop(a)), offset = f_int())");
    assert_tail(&r);
}

#[test]
fn barcolor_color_pop_runs_before_a_user_function_offset() {
    let r = run("barcolor(color.rgb(0, 0, array.pop(a)), offset = f_int())");
    assert_tail(&r);
}

#[test]
fn hline_user_functions_run_in_parameter_order() {
    // `hline` takes only input-qualified values, so no earlier argument can
    // pop; its user-function arguments still run in parameter order: the
    // price helper (parameter 0) before the color helper (parameter 2),
    // whichever is written first.
    let r = run(
        "var order = array.new_float()\nf_price() =>\n    array.push(order, 1)\n    1.0\nf_hcolor() =>\n    array.push(order, 2)\n    color.red\nhline(color = f_hcolor(), price = f_price())\nplot(array.get(order, 0) * 10 + array.get(order, 1), \"order\")\nplot(array.pop(a) + f_num(), \"value\")",
    );
    assert_eq!(plot_values(&r, "order"), [12.0; BARS]);
    assert_eq!(plot_values(&r, "value"), POPPED);
    assert_tail(&r);
}

#[test]
fn time_timeframe_pop_runs_before_a_user_function_session() {
    // `time(timeframe, session)` evaluates the timeframe first, also when
    // the arguments are named in another order. The optional operands used
    // to be analyzed first, so the session helper's body ran before the pop.
    let session = "f_session() =>\n    array.push(a, 100.0 + bar_index)\n    \"\"\n";
    for call in [
        "time(array.pop(a) > 0 ? \"D\" : \"W\", f_session())",
        "time(session = f_session(), timeframe = array.pop(a) > 0 ? \"D\" : \"W\")",
        "time_close(array.pop(a) > 0 ? \"D\" : \"W\", f_session())",
    ] {
        let r = run(&format!("{session}t = {call}\nplot(t, \"t\")"));
        assert_tail(&r);
    }
}

#[test]
fn alertcondition_condition_pop_runs_before_a_user_function_title() {
    let r = run("alertcondition(array.pop(a) > 0, title = f_text())");
    assert_tail(&r);
}

#[test]
fn destructured_ta_tuple_source_pop_runs_before_a_user_function_argument() {
    // A tuple `ta.*` call on the right of a destructuring declaration is
    // dispatched by the statement analyzer, not through an expression, so it
    // had no argument-order frame: the body of `f_anchor` in the later
    // `anchor` argument ran before the source's pop (positional, wrapped in
    // parentheses, and named in another order). The banded `ta.vwap` is the
    // tuple `ta.*` call with a series argument after its source; the other
    // tuple functions take literal or input settings there. An anchor that
    // is true on every bar restarts the VWAP each bar, so it equals the
    // popped source.
    for body in [
        // Runtime counterexample: `stdev_mult` from a user function. Without
        // the ordering, the VWAP is 100 (the pushed value) on bar 0 instead
        // of the popped 3.
        "f_mult() =>\n    array.push(a, 100.0 + bar_index)\n    1.0\n[v, up, lo] = ta.vwap(array.pop(a), true, f_mult())\nplot(v, \"value\")",
        "f_anchor() =>\n    array.push(a, 100.0 + bar_index)\n    true\n[v, up, lo] = ta.vwap(array.pop(a), f_anchor(), 1.0)\nplot(v, \"value\")",
        "f_anchor() =>\n    array.push(a, 100.0 + bar_index)\n    true\n[v, up, lo] = (ta.vwap(array.pop(a), f_anchor(), 1.0))\nplot(v, \"value\")",
        "f_anchor() =>\n    array.push(a, 100.0 + bar_index)\n    true\n[v, up, lo] = ta.vwap(source = array.pop(a), stdev_mult = 1.0, anchor = f_anchor())\nplot(v, \"value\")",
    ] {
        let r = run(body);
        assert_tail(&r);
        assert_eq!(plot_values(&r, "value"), POPPED, "{body}");
    }
}

#[test]
fn destructured_ta_bb_keeps_its_fixed_multiplier_guard() {
    // Negative case: `ta.bb`'s `mult` must be a number literal or an
    // unchanged numeric input (E0219), so `ta.bb(src, 1, f())` is refused
    // rather than ordered; the argument ordering does not weaken that guard. A
    // literal multiplier still compiles and runs.
    let market = MarketData {
        open: vec![10.0; BARS],
        high: vec![12.0; BARS],
        low: vec![9.0; BARS],
        close: vec![11.0; BARS],
        volume: vec![100.0; BARS],
        time: (0..BARS as i64)
            .map(|i| 1_700_000_000_000 + i * 86_400_000)
            .collect(),
        timeframe: "1D".into(),
        ..MarketData::default()
    };
    let bb = |mult: &str| {
        compile_and_run(
            &format!(
                "//@version=6\nindicator(\"bb\")\n{PRELUDE}[basis, upper, lower] = ta.bb(array.pop(a), 1, {mult})\nplot(basis)\n"
            ),
            &market,
            CompileOptions::default(),
            &RunOptions::default(),
        )
    };
    let refused = bb("f_num()");
    let codes: Vec<_> = refused
        .diagnostics
        .iter()
        .filter(|d| d.is_error())
        .map(|d| d.code.clone())
        .collect();
    assert_eq!(
        codes,
        vec![Some("E0219".to_string())],
        "{:?}",
        refused.diagnostics
    );
    let literal = bb("2.0");
    assert!(
        literal.diagnostics.iter().all(|d| !d.is_error()),
        "{:?}",
        literal.diagnostics
    );
    assert!(
        literal.runtime_errors.is_empty(),
        "{:?}",
        literal.runtime_errors
    );
    // With a length of 1 the basis is the popped value: 3, 2, 1.
    assert_eq!(slot_values(&literal, 0), [3.0, 2.0, 1.0]);
}

#[test]
fn table_cell_text_pop_runs_before_a_user_function_text_color() {
    let r = run(
        "var t = table.new(position.top_right, 1, 1)\ntable.cell(t, 0, 0, str.tostring(array.pop(a)), text_color = f_color())",
    );
    assert_tail(&r);
}

#[test]
fn label_new_y_pop_runs_before_a_user_function_color() {
    let r = run("label.new(bar_index, array.pop(a), color = f_color())");
    assert_tail(&r);
}

#[test]
fn line_new_y_pop_runs_before_a_user_function_color() {
    let r = run("line.new(bar_index, array.pop(a), bar_index + 1, 0.0, color = f_color())");
    assert_tail(&r);
}

#[test]
fn box_new_top_pop_runs_before_a_user_function_border_color() {
    let r = run("box.new(bar_index, array.pop(a), bar_index + 1, 0.0, border_color = f_color())");
    assert_tail(&r);
}

#[test]
fn linefill_new_line_argument_runs_before_a_user_function_color() {
    let r = run(
        "l2 = line.new(bar_index, 0.0, bar_index + 1, 0.0)\nlinefill.new(line.new(bar_index, array.pop(a), bar_index + 1, 1.0), l2, f_color())",
    );
    assert_tail(&r);
}

#[test]
fn collection_call_argument_pop_runs_before_a_user_function_argument() {
    let r = run("var b = array.new_float(3, 0.0)\narray.fill(b, array.pop(a), f_int())\nplot(array.get(b, 0), \"value\")");
    assert_eq!(plot_values(&r, "value"), POPPED);
    assert_tail(&r);
}

#[test]
fn ta_source_operand_pop_runs_before_a_user_function_operand() {
    // A one-bar `ta.sma` returns its source, so the plotted value is the
    // operand order inside the source expression.
    let r = run("plot(ta.sma(array.pop(a) + f_num(), 1), \"value\")");
    assert_eq!(plot_values(&r, "value"), POPPED);
    assert_tail(&r);
}

#[test]
fn ta_source_pop_runs_before_a_user_function_length() {
    // `source` is parameter 0 and `length` parameter 1. A one-bar window
    // returns the source, so the plotted value is the value popped first.
    for call in ["ta.sma", "ta.highest", "ta.lowest", "ta.ema"] {
        let r = run(&format!(
            "plot({call}(array.pop(a), f_int() + 1), \"value\")"
        ));
        assert_eq!(plot_values(&r, "value"), POPPED, "{call}");
        assert_tail(&r);
    }
}

#[test]
fn ta_second_series_user_function_runs_after_the_first_series_pop() {
    // Two series parameters: the pop in `source1` runs before the user
    // function in `source2`.
    for call in ["ta.crossover", "ta.crossunder", "ta.cross"] {
        let r = run(&format!(
            "x = {call}(array.pop(a), f_num())\nplot(x ? 1 : 0, \"value\")"
        ));
        assert_tail(&r);
    }
    let r = run("plot(ta.valuewhen(array.pop(a) > 0, close + f_num(), 0), \"value\")");
    assert_eq!(plot_values(&r, "value"), [11.0; BARS]);
    assert_tail(&r);
}

#[test]
fn realtime_ticks_capture_in_parameter_order_with_var_rollback_and_varip() {
    // On every realtime tick the `var` array rolls back to its committed state
    // and the `varip` array keeps each tick's change. In both, the pop runs
    // before the user function's push. Bar 0 is a closed bar; bar 1 has three
    // ticks.
    let source = "//@version=6\nindicator(\"Realtime order\")\nvar a = array.from(1.0, 2.0, 3.0)\nvarip b = array.from(1.0, 2.0, 3.0)\nfa() =>\n    array.push(a, 100.0 + bar_index)\n    0.0\nfb() =>\n    array.push(b, 200.0 + bar_index)\n    0.0\nplot(array.pop(a) + fa(), \"pa\")\nplot(array.pop(b) + fb(), \"pb\")\nplot(array.get(a, 2), \"ta\")\nplot(array.last(b), \"tb\")\n";
    let market = MarketData {
        open: vec![100.0],
        high: vec![101.0],
        low: vec![99.0],
        close: vec![100.0],
        volume: vec![1000.0],
        time: vec![1_700_000_000_000],
        ..MarketData::default()
    };
    let ticks = [101.0, 99.0, 103.0]
        .into_iter()
        .enumerate()
        .map(|(ordinal, close)| Tick {
            bar_index: 1,
            open: 100.0,
            high: 103.0,
            low: 99.0,
            close,
            volume: 1000.0,
            time: 1_700_000_060_000,
            trading_day: None,
            is_bar_close: ordinal == 2,
        })
        .collect();
    let r = compile_and_run(
        source,
        &market,
        CompileOptions::default(),
        &RunOptions {
            realtime: Some(RealtimeSim { ticks }),
            realtime_trace: true,
            ..RunOptions::default()
        },
    );
    assert!(
        r.diagnostics.iter().all(|d| !d.is_error()),
        "{:?}",
        r.diagnostics
    );
    assert!(r.runtime_errors.is_empty(), "{:?}", r.runtime_errors);
    // Bar 0 pops 3 from both and pushes 100 / 200. Each tick of bar 1 pops
    // the committed 100 from `a` (rolled back) and pushes 101. `b` keeps its
    // ticks: the first pops 200 and pushes 201, later ones pop 201 and push
    // 201 again.
    assert_eq!(plot_values(&r, "pa"), [3.0, 100.0]);
    assert_eq!(plot_values(&r, "pb"), [3.0, 201.0]);
    assert_eq!(plot_values(&r, "ta"), [100.0, 101.0]);
    assert_eq!(plot_values(&r, "tb"), [200.0, 201.0]);
    let trace: Vec<Vec<f64>> = r
        .realtime_trace
        .as_ref()
        .expect("realtime trace")
        .iter()
        .map(|sample| sample.values.iter().map(number).collect())
        .collect();
    assert_eq!(
        trace,
        [
            [100.0, 200.0, 101.0, 201.0],
            [100.0, 201.0, 101.0, 201.0],
            [100.0, 201.0, 101.0, 201.0],
        ]
    );
}

#[test]
fn user_function_arguments_keep_their_order() {
    // A user function's own arguments were already stored in order; keep it.
    let r = run("g(x, y) => x + y\nplot(g(array.pop(a), f_num()), \"value\")");
    assert_eq!(plot_values(&r, "value"), POPPED);
    assert_tail(&r);
}

#[test]
fn var_initializer_pop_still_runs_once() {
    // A `var` initializer runs on the first bar only. Moving its pop into a
    // statement before the declaration would pop on every bar.
    let r = run("var b = array.from(1.0, 2.0, 3.0)\nvar float first = array.pop(b) + f_num() * 0\nplot(first, \"value\")\nplot(array.size(b), \"size\")");
    assert_eq!(plot_values(&r, "value"), [3.0; BARS]);
    assert_eq!(plot_values(&r, "size"), [2.0; BARS]);
}

#[test]
fn while_condition_pop_still_runs_on_every_test() {
    // A `while` condition is tested before every iteration. The pop in it must
    // stay inside the condition, not move before the loop: `c` pops 3, 2 and
    // then 1, which ends the loop after two iterations.
    let r = run("c = array.from(1.0, 2.0, 3.0)\nn = 0\nwhile array.pop(c) + f_num() > 1\n    n += 1\nplot(n, \"value\")");
    assert_eq!(plot_values(&r, "value"), [2.0; BARS]);
}
