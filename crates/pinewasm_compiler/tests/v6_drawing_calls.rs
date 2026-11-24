//! Drawing calls: required and optional parameters, handle and getter qualifiers, and numeric coordinate qualifiers.
use pinewasm_compiler::{compile, CompileOptions};
use pinewasm_span::Severity;

fn source_has_errors(body: &str) -> bool {
    compile(
        &format!("//@version=6\nindicator(\"drawing argument contract\")\n{body}\nplot(close)\n"),
        CompileOptions::default(),
    )
    .diagnostics
    .iter()
    .any(|diagnostic| diagnostic.severity == Severity::Error)
}

#[test]
fn drawing_required_and_optional_parameters_are_enforced() {
    for body in [
        "b = box.new()",
        "b = box.new(left=0, top=1.0, right=2)",
        "l = line.new()",
        "l = line.new(x1=0, y1=1.0, x2=2)",
        "t = table.new()",
        "t = table.new(position.top_right, 2)",
        "l = line.new(0, 1.0, 2, 3.0)\nline.set_x1(l)",
        "l = line.new(0, 1.0, 2, 3.0)\nline.set_y2(l)",
        "l = line.new(0, 1.0, 2, 3.0)\nplot(line.get_price(l, 1.0))",
        "overlay = close > open\nl = line.new(0, 1.0, 2, 3.0, force_overlay=overlay)",
        "overlay = close > open\nb = box.new(0, 1.0, 2, 0.0, force_overlay=overlay)",
        "overlay = close > open\nlabel.new(0, 1.0, \"x\", force_overlay=overlay)",
        "overlay = close > open\nt = table.new(position.top_right, 1, 1, force_overlay=overlay)",
        "overlay = close > open\npts = array.new<chart.point>()\npolyline.new(pts, force_overlay=overlay)",
        "overlay = close > open\np1 = chart.point.from_index(0, 1.0)\np2 = chart.point.from_index(1, 2.0)\nline.new(p1, p2, force_overlay=overlay)",
        "t = table.new(position.top_right, 1, 1)\ntable.cell_set_text_formatting(t, 0, 0)",
    ] {
        assert!(source_has_errors(body), "unexpectedly accepted: {body}");
    }

    for body in [
        "b = box.new(left=0, top=1.0, right=2, bottom=0.0)\nbox.set_border_color(b, color.red)",
        "b = box.new(0, 1.0, 2, 0.0)\nbox.set_border_width(b, 2)",
        "b = box.new(0, 1.0, 2, 0.0)\nbox.set_border_style(b, line.style_dashed)",
        "b = box.new(0, 1.0, 2, 0.0)\nbox.set_border_color(id=b, color=color.red)",
        "b = box.new(0, 1.0, 2, 0.0)\nbox.set_bgcolor(id=b, color=color.blue)",
        "b = box.new(0, 1.0, 2, 0.0)\nbox.set_border_width(id=b, width=2)",
        "b = box.new(0, 1.0, 2, 0.0)\nbox.set_border_style(id=b, style=line.style_dashed)",
        "l = line.new(0, 1.0, 2, 3.0)\nline.set_x1(id=l, x=1)",
        "l = line.new(0, 1.0, 2, 3.0)\nline.set_y2(id=l, y=4.0)",
        "t = table.new(position.top_right, 1, 1)\ntable.set_bgcolor(t)",
        "t = table.new(position.top_right, 1, 1)\ntable.set_bgcolor(table_id=t)",
        "t = table.new(position.top_right, 1, 1)\ntable.cell_set_text(t, 0, 0)",
        "t = table.new(position.top_right, 1, 1)\ntable.cell_set_text(table_id=t, column=0, row=0)",
        "line.new(0, 1.0, 2, 3.0, force_overlay=true)",
        "box.new(0, 1.0, 2, 0.0, force_overlay=true)",
        "label.new(0, 1.0, \"x\", force_overlay=true)",
        "table.new(position.top_right, 1, 1, force_overlay=true)",
        "pts = array.new<chart.point>()\npolyline.new(pts, force_overlay=true)",
    ] {
        assert!(!source_has_errors(body), "unexpectedly rejected: {body}");
    }
}

#[test]
fn drawing_handles_and_getters_have_series_qualifiers() {
    for body in [
        "simple line l = line.new(0, 1.0, 2, 3.0)",
        "simple float y = line.get_y1(line.new(0, 1.0, 2, 3.0))",
        "simple int x = line.get_x1(line.new(0, 1.0, 2, 3.0))",
        "simple float y = line.get_price(line.new(0, 1.0, 2, 3.0), 1)",
    ] {
        assert!(source_has_errors(body), "unexpectedly accepted: {body}");
    }
    for body in [
        "series line l = line.new(0, 1.0, 2, 3.0)",
        "series float y = line.get_y1(line.new(0, 1.0, 2, 3.0))",
        "series int x = line.get_x1(line.new(0, 1.0, 2, 3.0))",
        "series float y = line.get_price(line.new(0, 1.0, 2, 3.0), 1)",
    ] {
        assert!(!source_has_errors(body), "unexpectedly rejected: {body}");
    }
}

#[test]
fn drawing_coordinates_accept_each_documented_numeric_qualifier() {
    for body in [
        "line.new(x1=1, y1=1.0, x2=2, y2=2.0)",
        "simple int x = timeframe.multiplier\nline.new(x1=x, y1=close, x2=x + 1, y2=close)",
        "x = input.int(1)\nline.new(x1=x, y1=close, x2=x + 1, y2=close)",
        "line.new(x1=bar_index, y1=close, x2=bar_index + 1, y2=close)",
    ] {
        assert!(!source_has_errors(body), "unexpectedly rejected: {body}");
    }
}
