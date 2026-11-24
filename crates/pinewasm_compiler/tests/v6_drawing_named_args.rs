//! Drawing calls bind named arguments by their parameter names in the
//! compiler's built-in signature table. Accepted forms are checked for
//! compilation and for agreement with the positional forms.
use pinewasm_compiler::{compile, compile_and_run, CompileOptions, MarketData, RunOptions, Value};

/// One object of every drawing type for the generated calls to address.
const OBJECTS: &str = "\
var line ln = line.new(0, 1.0, 1, 2.0)
var line ln2 = line.new(0, 2.0, 1, 3.0)
var label lb = label.new(0, 1.0, \"x\")
var box bx = box.new(0, 2.0, 1, 1.0)
var table tb = table.new(position.top_right, 3, 3)
var linefill lf = linefill.new(ln, ln2, color.red)
pt = chart.point.from_index(0, 1.0)
pts = array.from(pt, pt)
var polyline pl = polyline.new(pts)
";

fn script(body: &str) -> String {
    format!("//@version=6\nindicator(\"drawing named arguments\")\n{OBJECTS}{body}\nplot(close)\n")
}

fn error_codes(body: &str) -> Vec<String> {
    let result = compile(&script(body), CompileOptions::default());
    assert!(!result.success, "unexpectedly accepted: {body}");
    result
        .diagnostics
        .iter()
        .filter_map(|diagnostic| diagnostic.code.clone())
        .collect()
}

#[test]
fn unknown_drawing_parameter_names_are_rejected() {
    for body in [
        "x = line.get_x1(line=ln)",
        "x = line.get_x1(table_id=ln)",
        "x = linefill.get_line1(linefill=lf)",
        "x = label.get_text(label=lb)",
        "y = line.get_price(id=ln, index=1)",
        "box.set_lefttop(id=bx, x=5, y=2.0)",
        "box.set_rightbottom(id=bx, left=5, top=2.0)",
        "line.set_xloc(id=ln, left=3, right=4, xloc=xloc.bar_index)",
        "label.set_xloc(id=lb, x1=3, xloc=xloc.bar_index)",
        "box.set_xloc(id=bx, x1=3, x2=4, xloc=xloc.bar_index)",
        "line.delete(table_id=ln)",
        "table.delete(id=tb)",
        "table.set_position(id=tb, position=position.bottom_right)",
        "table.clear(id=tb, start_column=0, start_row=0)",
        "table.clear(table_id=tb, column=0, row=0)",
        "table.merge_cells(id=tb, start_column=0, start_row=0, end_column=1, end_row=1)",
    ] {
        let codes = error_codes(body);
        assert!(
            codes.iter().any(|code| code == "E0228"),
            "{body}: expected E0228, got {codes:?}"
        );
    }
}

#[test]
fn malformed_named_drawing_calls_are_still_rejected() {
    let mut wrong = Vec::new();
    for (body, expected) in [
        // Bound twice. The getter's argument count is checked first.
        ("x = line.get_x1(ln, id=ln)", "E0238"),
        ("box.set_lefttop(bx, 5, left=5)", "E0228"),
        ("table.clear(tb, 0, start_column=0)", "E0228"),
        // Required parameter left out.
        ("x = line.get_x1()", "E0238"),
        ("y = line.get_price(id=ln)", "E0238"),
        ("box.set_lefttop(id=bx, top=2.0)", "E0238"),
        ("table.clear(table_id=tb, start_row=0)", "E0238"),
        (
            "table.clear(table_id=tb, start_column=0, end_column=1)",
            "E0204",
        ),
        (
            "table.clear(table_id=tb, start_row=0, end_row=1)",
            "E0204",
        ),
        (
            "table.merge_cells(table_id=tb, start_column=0, start_row=0, end_column=1)",
            "E0238",
        ),
        // Surplus argument.
        ("x = line.get_x1(ln, ln2)", "E0238"),
        ("table.clear(tb, 0, 0, 1, 1, 2)", "E0238"),
        // Wrong receiver or value type.
        ("x = line.get_x1(id=bx)", "E0238"),
        ("x = linefill.get_line1(id=ln)", "E0238"),
        ("y = line.get_price(id=ln, x=1.5)", "E0237"),
        ("box.set_lefttop(id=ln, left=5, top=2.0)", "E0238"),
        ("box.set_lefttop(id=bx, left=5.5, top=2.0)", "E0237"),
        (
            "table.clear(table_id=bx, start_column=0, start_row=0)",
            "E0238",
        ),
        (
            "table.merge_cells(table_id=tb, start_column=0.5, start_row=0, end_column=1, end_row=1)",
            "E0237",
        ),
    ] {
        let codes = error_codes(body);
        if codes != [expected] {
            wrong.push(format!("{body}: expected {expected}, got {codes:?}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn positional_and_method_forms_are_unchanged() {
    for body in [
        "x = line.get_x1(ln)",
        "x = ln.get_x1()",
        "x = lf.get_line1()",
        "y = line.get_price(ln, 1)",
        "y = ln.get_price(x=1)",
        "box.set_lefttop(bx, 5, 2.0)",
        "bx.set_lefttop(left=5, top=2.0)",
        "bx.set_rightbottom(bottom=2.0, right=5)",
        "box.set_xloc(bx, 3, 4, xloc.bar_index)",
        "ln.set_xloc(x1=3, x2=4, xloc=xloc.bar_index)",
        "line.set_xy1(ln, x=3, y=4.0)",
        "table.delete(tb)",
        "tb.delete()",
        "table.set_position(tb, position.bottom_right)",
        "tb.set_position(position=position.bottom_right)",
        "table.clear(tb, 0, 0)",
        "table.clear(tb, 0, 0, 1, 1)",
        "tb.clear(start_column=0, start_row=0)",
        "table.clear(tb, 0, 0, end_row=1)",
        "table.merge_cells(tb, 0, 0, 1, 1)",
        "tb.merge_cells(start_column=0, start_row=0, end_column=1, end_row=1)",
    ] {
        let result = compile(&script(body), CompileOptions::default());
        assert!(result.success, "{body}: {:?}", result.diagnostics);
    }
}

fn run(source: &str) -> pinewasm_compiler::RunResult {
    let bars = 3;
    let result = compile_and_run(
        source,
        &MarketData {
            open: vec![10.0; bars],
            high: vec![12.0; bars],
            low: vec![9.0; bars],
            close: vec![11.0; bars],
            volume: vec![0.0; bars],
            ..MarketData::default()
        },
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

fn last_number(values: &[Value]) -> f64 {
    match values.last().expect("plot has values") {
        Value::Float(value) => *value,
        #[allow(clippy::cast_precision_loss)]
        Value::Int(value) => *value as f64,
        other => panic!("expected a number, found {other:?}"),
    }
}

const READS: &str = "\
plot(line.get_x1(ln))
plot(line.get_x2(ln))
plot(line.get_price(ln, 7))
plot(box.get_left(bx))
plot(box.get_top(bx))
plot(box.get_right(bx))
plot(box.get_bottom(bx))
plot(label.get_x(lb))
";

#[test]
fn named_compound_setters_and_getters_bind_by_name_not_by_position() {
    // Every named call lists its arguments out of signature order, so a
    // positional reading of the same list would write different fields.
    let named = run(&script(
        "\
line.set_xy1(y=2.0, id=ln, x=1)
line.set_xy2(y=4.0, x=3, id=ln)
line.set_xloc(xloc=xloc.bar_index, x2=8, id=ln, x1=6)
box.set_lefttop(top=7.5, id=bx, left=5)
box.set_rightbottom(bottom=2.5, right=9, id=bx)
box.set_xloc(xloc=xloc.bar_index, right=9, left=5, id=bx)
label.set_xloc(xloc=xloc.bar_index, x=4, id=lb)
table.cell(tb, 0, 0, \"a\")
table.cell(tb, 1, 1, \"b\")
table.cell(tb, 2, 2, \"c\")
table.set_position(position=position.bottom_right, table_id=tb)
table.clear(start_row=1, start_column=1, table_id=tb)
plot(line.get_x1(id=ln))
plot(line.get_x2(id=ln))
plot(line.get_price(x=7, id=ln))
plot(box.get_left(id=bx))
plot(box.get_top(id=bx))
plot(box.get_right(id=bx))
plot(box.get_bottom(id=bx))
plot(label.get_x(id=lb))
",
    ));
    let positional = run(&script(&format!(
        "\
line.set_xy1(ln, 1, 2.0)
line.set_xy2(ln, 3, 4.0)
line.set_xloc(ln, 6, 8, xloc.bar_index)
box.set_lefttop(bx, 5, 7.5)
box.set_rightbottom(bx, 9, 2.5)
box.set_xloc(bx, 5, 9, xloc.bar_index)
label.set_xloc(lb, 4, xloc.bar_index)
table.cell(tb, 0, 0, \"a\")
table.cell(tb, 1, 1, \"b\")
table.cell(tb, 2, 2, \"c\")
table.set_position(tb, position.bottom_right)
table.clear(tb, 1, 1)
{READS}"
    )));

    let numbers: Vec<_> = named
        .plots
        .iter()
        .map(|plot| last_number(&plot.values))
        .collect();
    assert_eq!(numbers[..8], [6.0, 8.0, 3.0, 5.0, 7.5, 9.0, 2.5, 4.0]);
    for (named, positional) in named.plots.iter().zip(&positional.plots) {
        assert_eq!(named.values, positional.values);
    }
    assert_eq!(named.drawings, positional.drawings);
}

#[test]
fn named_table_ranges_match_their_positional_forms() {
    // Table deletion makes every alias to that table `na`. Keep the operation
    // on the final bar so later bars do not try to write cells through the
    // deleted handle while this test compares the final drawing snapshots.
    let cells = "\
table.cell(tb, 0, 0, \"a\")
table.cell(tb, 1, 0, \"b\")
table.cell(tb, 0, 1, \"c\")
table.cell(tb, 1, 1, \"d\")
table.cell(tb, 2, 2, \"e\")
";
    for (named, positional) in [
        (
            "table.clear(end_row=1, end_column=0, start_row=0, start_column=0, table_id=tb)",
            "table.clear(tb, 0, 0, 0, 1)",
        ),
        (
            "table.clear(tb, 1, 0, end_row=1)",
            "table.clear(tb, 1, 0, 1, 1)",
        ),
        (
            "table.merge_cells(end_row=1, end_column=1, start_row=0, start_column=0, table_id=tb)",
            "table.merge_cells(tb, 0, 0, 1, 1)",
        ),
        ("table.delete(table_id=tb)", "table.delete(tb)"),
        ("linefill.delete(id=lf)", "linefill.delete(lf)"),
    ] {
        let named_run = run(&script(&format!(
            "{cells}if barstate.islast\n    {named}\n"
        )));
        let positional_run = run(&script(&format!(
            "{cells}if barstate.islast\n    {positional}\n"
        )));
        assert_eq!(named_run.drawings, positional_run.drawings, "{named}");
        let untouched = run(&script(cells));
        assert_ne!(
            named_run.drawings, untouched.drawings,
            "{named} had no effect"
        );
    }
}
