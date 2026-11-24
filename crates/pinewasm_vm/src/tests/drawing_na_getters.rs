//! Drawing getters on an `na` drawing id return `na`.
//!
//! A script may declare `var line l = na`, create the line on a later bar and
//! call `line.get_price(l, bar_index)` on every bar. On the bars before the
//! line exists the getter reads an `na` line, and the script must still run.
//! "Na in, na out" is therefore the required behaviour for every `line.get_*`,
//! `label.get_*`, `box.get_*` and `linefill.get_*` getter: each one returns
//! `na` and does not stop the run with `na_field_access`. Field reads of a
//! user-defined `na` object keep their structured error
//! (`udt_na_object_field_read_is_structured_error`).
use super::{compile, data_from_close, floats};
use crate::{run, RunOptions};

fn plots(source: &str, close: &[f64]) -> Vec<Vec<Option<f64>>> {
    let outcome = run(
        &compile(source),
        &data_from_close(close),
        &RunOptions::default(),
    );
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    outcome
        .plots
        .iter()
        .map(|plot| floats(&plot.values))
        .collect()
}

#[test]
fn line_get_price_plots_na_until_the_line_exists() {
    const SOURCE: &str = "//@version=6\n\
        indicator(\"GetPrice\", overlay=true)\n\
        var line l = na\n\
        if bar_index == 10\n    \
            l := line.new(0, high[5], bar_index, high)\n\
        plot(line.get_price(l, bar_index), color=color.green)\n";
    let close: Vec<f64> = (0..16).map(|bar| 100.0 + f64::from(bar * bar)).collect();
    let plot = &plots(SOURCE, &close)[0];
    // Hand-computed: the line runs from (0, high[5] on bar 10) = (0, close[5])
    // to (10, close[10]); `line.get_price` extends it both ways.
    let (y1, y2) = (close[5], close[10]);
    for (bar, value) in plot.iter().enumerate() {
        if bar < 10 {
            assert_eq!(*value, None, "bar {bar}: the line is still na");
        } else {
            let expected = y1 + (bar as f64 - 0.0) * (y2 - y1) / 10.0;
            assert_eq!(*value, Some(expected), "bar {bar}");
        }
    }
}

#[test]
fn every_line_getter_on_an_na_id_returns_na() {
    const SOURCE: &str = "//@version=6\n\
        indicator(\"T\")\n\
        var line l = na\n\
        plot(line.get_x1(l))\n\
        plot(line.get_x2(l))\n\
        plot(line.get_y1(l))\n\
        plot(line.get_y2(l))\n\
        plot(line.get_price(l, bar_index))\n\
        plot(line.get_y2(na))\n\
        plot(line.get_price(na, bar_index))\n";
    for (index, plot) in plots(SOURCE, &[1.0, 2.0]).iter().enumerate() {
        assert_eq!(plot, &vec![None, None], "plot {index}");
    }
}

#[test]
fn every_label_box_and_linefill_getter_on_an_na_id_returns_na() {
    const SOURCE: &str = "//@version=6\n\
        indicator(\"T\")\n\
        var label lb = na\n\
        var box bx = na\n\
        var linefill lf = na\n\
        plot(label.get_x(lb))\n\
        plot(label.get_y(lb))\n\
        plot(na(label.get_text(lb)) ? 1 : 0)\n\
        plot(box.get_left(bx))\n\
        plot(box.get_top(bx))\n\
        plot(box.get_right(bx))\n\
        plot(box.get_bottom(bx))\n\
        plot(na(linefill.get_line1(lf)) ? 1 : 0)\n\
        plot(na(linefill.get_line2(lf)) ? 1 : 0)\n";
    let plots = plots(SOURCE, &[1.0, 2.0]);
    for (index, plot) in plots.iter().enumerate() {
        let expected = if matches!(index, 2 | 7 | 8) {
            Some(1.0)
        } else {
            None
        };
        assert_eq!(plot, &vec![expected; 2], "plot {index}");
    }
}

#[test]
fn getters_on_a_live_id_still_read_its_fields() {
    const SOURCE: &str = "//@version=6\n\
        indicator(\"T\")\n\
        l = line.new(bar_index, close, bar_index + 2, close + 4)\n\
        lb = label.new(bar_index, close + 1, \"x\")\n\
        bx = box.new(bar_index, close + 3, bar_index + 1, close - 3)\n\
        lf = linefill.new(l, l, color.red)\n\
        plot(line.get_x2(l))\n\
        plot(line.get_price(l, bar_index + 1))\n\
        plot(label.get_y(lb))\n\
        plot(label.get_text(lb) == \"x\" ? 1 : 0)\n\
        plot(box.get_bottom(bx))\n\
        plot(line.get_y2(linefill.get_line1(lf)))\n";
    let plots = plots(SOURCE, &[10.0, 20.0]);
    assert_eq!(plots[0], vec![Some(2.0), Some(3.0)]);
    assert_eq!(plots[1], vec![Some(12.0), Some(22.0)]);
    assert_eq!(plots[2], vec![Some(11.0), Some(21.0)]);
    assert_eq!(plots[3], vec![Some(1.0), Some(1.0)]);
    assert_eq!(plots[4], vec![Some(7.0), Some(17.0)]);
    assert_eq!(plots[5], vec![Some(14.0), Some(24.0)]);
}

#[test]
fn line_get_price_on_an_na_line_still_evaluates_x() {
    // The receiver and then `x` are evaluated once each, as for an existing line,
    // before the getter returns `na`.
    const SOURCE: &str = "//@version=6\n\
        indicator(\"T\")\n\
        var line l = na\n\
        var a = array.new_int(0)\n\
        f() =>\n    \
            array.push(a, 1)\n    \
            bar_index\n\
        plot(line.get_price(l, f()))\n\
        plot(array.size(a))\n";
    let plots = plots(SOURCE, &[1.0, 2.0, 3.0]);
    assert_eq!(plots[0], vec![None, None, None]);
    assert_eq!(plots[1], vec![Some(1.0), Some(2.0), Some(3.0)]);
}
