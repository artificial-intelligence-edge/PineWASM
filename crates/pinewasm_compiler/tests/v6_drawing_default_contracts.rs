//! Drawing constructor defaults and cell replacement semantics.
use pinewasm_compiler::{compile_and_run, CompileOptions, MarketData, RunOptions};
use serde_json::{json, Value};

fn drawings(body: &str) -> Value {
    let market = MarketData {
        open: vec![10.0; 8],
        high: vec![12.0; 8],
        low: vec![9.0; 8],
        close: vec![11.0; 8],
        volume: vec![100.0; 8],
        timeframe: "1D".into(),
        ..MarketData::default()
    };
    let source = format!(
        "//@version=6\nindicator(\"Drawing defaults\",overlay=true)\nif barstate.islast\n{}",
        body.lines()
            .map(|line| format!("    {line}\n"))
            .collect::<String>()
    );
    let result = compile_and_run(
        &source,
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
    assert_eq!(result.bars_executed, 8);
    serde_json::to_value(result.drawings).unwrap()
}

#[test]
fn omitted_constructor_colors_use_v6_defaults_in_both_coordinate_overloads() {
    let d = drawings(
        r#"p = chart.point.from_index(0, 12)
q = chart.point.from_index(1, 9)
line.new(0,12,1,9)
line.new(p,q)
box.new(0,12,1,9,text="Box")
box.new(p,q,text="Point box")
label.new(0,12,"Label")
label.new(p,"Point label")
polyline.new(array.from(p,q))"#,
    );
    for line in d["lines"].as_array().unwrap() {
        assert_eq!(line["fields"]["color"], "#2962FF");
    }
    for b in d["boxes"].as_array().unwrap() {
        assert_eq!(b["fields"]["border_color"], "#2962FF");
        assert_eq!(b["fields"]["bgcolor"], "#2962FF");
        assert_eq!(b["fields"]["text_color"], "#363A45");
    }
    for label in d["labels"].as_array().unwrap() {
        assert_eq!(label["fields"]["color"], "#2962FF");
        assert_eq!(label["fields"]["textcolor"], "#FFFFFF");
    }
    assert_eq!(d["polylines"][0]["fields"]["line_color"], "#2962FF");
    assert_eq!(d["polylines"][0]["fields"]["fill_color"], Value::Null);
}

#[test]
fn explicit_na_constructor_colors_remain_absent() {
    let d = drawings(
        r#"line.new(0,12,1,9,color=na)
box.new(0,12,1,9,text="Box",border_color=na,bgcolor=na,text_color=na)
label.new(0,12,"Label",color=na,textcolor=na)
polyline.new(array.from(chart.point.from_index(0,12),chart.point.from_index(1,9)),line_color=na,fill_color=na)"#,
    );
    for (kind, fields) in [
        ("lines", vec!["color"]),
        ("boxes", vec!["border_color", "bgcolor", "text_color"]),
        ("labels", vec!["color", "textcolor"]),
        ("polylines", vec!["line_color", "fill_color"]),
    ] {
        for field in fields {
            assert_eq!(d[kind][0]["fields"][field], Value::Null, "{kind}.{field}");
        }
    }
}

#[test]
fn color_setters_and_copies_preserve_defaults_and_explicit_absence() {
    let d = drawings(
        r#"l=line.new(0,12,1,9)
lc=line.copy(l)
line.set_color(l,na)
b=box.new(0,12,1,9,text="Box")
bc=box.copy(b)
box.set_bgcolor(b,na)
box.set_border_color(b,na)
box.set_text_color(b,na)
v=label.new(0,12,"Label")
vc=label.copy(v)
label.set_color(v,na)
label.set_textcolor(v,na)"#,
    );
    assert_eq!(d["lines"][0]["fields"]["color"], Value::Null);
    assert_eq!(d["lines"][1]["fields"]["color"], "#2962FF");
    for f in ["bgcolor", "border_color", "text_color"] {
        assert_eq!(d["boxes"][0]["fields"][f], Value::Null);
    }
    assert_eq!(d["boxes"][1]["fields"]["bgcolor"], "#2962FF");
    assert_eq!(d["boxes"][1]["fields"]["text_color"], "#363A45");
    assert_eq!(d["labels"][0]["fields"]["color"], Value::Null);
    assert_eq!(d["labels"][0]["fields"]["textcolor"], Value::Null);
    assert_eq!(d["labels"][1]["fields"]["textcolor"], "#FFFFFF");
}

fn default_cell(text: &str) -> Value {
    json!({"text":text,"width":0.0,"height":0.0,"text_color":"#363A45","text_halign":"text.align_center","text_valign":"text.align_center","text_size":"size.normal","bgcolor":null,"tooltip":"","text_font_family":"font.family_default","text_formatting":"text.format_none"})
}

#[test]
fn initial_cell_and_table_defaults_are_engine_authored() {
    let d = drawings("t=table.new(position.top_right,1,1)\ntable.cell(t,0,0,\"Initial\")");
    assert_eq!(d["tables"][0]["cells"][0]["attrs"], default_cell("Initial"));
    for f in ["bgcolor", "frame_color", "border_color"] {
        assert_eq!(d["tables"][0]["fields"][f], Value::Null);
    }
    assert_eq!(d["tables"][0]["fields"]["frame_width"], 0);
    assert_eq!(d["tables"][0]["fields"]["border_width"], 0);
}

#[test]
fn cell_replacement_resets_all_omitted_attributes() {
    let d = drawings(
        r#"t=table.new(position.top_right,1,1)
table.cell(t,0,0,"Old",width=25,height=15,text_color=color.red,text_halign=text.align_left,text_valign=text.align_top,text_size=20,bgcolor=color.blue,tooltip="Tip",text_font_family=font.family_monospace,text_formatting=text.format_bold)
table.cell(t,0,0,"New")"#,
    );
    assert_eq!(d["tables"][0]["cells"][0]["attrs"], default_cell("New"));
}

#[test]
fn cell_setters_change_only_the_selected_property() {
    let d = drawings(
        r#"t=table.new(position.top_right,1,1)
table.cell(t,0,0,"Old",width=25,height=15,text_color=color.red,bgcolor=color.blue)
table.cell_set_text(t,0,0,"New")"#,
    );
    let mut expected = default_cell("New");
    expected["width"] = json!(25.0);
    expected["height"] = json!(15.0);
    expected["text_color"] = json!("#F23645");
    expected["bgcolor"] = json!("#2962FF");
    assert_eq!(d["tables"][0]["cells"][0]["attrs"], expected);
}

#[test]
fn explicit_absent_cell_colors_are_not_replaced_by_defaults() {
    let d=drawings("t=table.new(position.top_right,1,1)\ntable.cell(t,0,0,\"Hidden\",text_color=na,bgcolor=na)");
    let attrs = &d["tables"][0]["cells"][0]["attrs"];
    assert_eq!(attrs["text_color"], Value::Null);
    assert_eq!(attrs["bgcolor"], Value::Null);
    assert_eq!(attrs["text"], "Hidden");
}

#[test]
fn generated_contract_defaults_match_engine_emission() {
    let c = pinewasm_compiler::rendering_contract();
    for (kind, field, expected) in [
        ("line", "color", "#2962FF"),
        ("box", "border_color", "#2962FF"),
        ("box", "bgcolor", "#2962FF"),
        ("box", "text_color", "#363A45"),
        ("label", "color", "#2962FF"),
        ("label", "textcolor", "#FFFFFF"),
        ("polyline", "line_color", "#2962FF"),
    ] {
        assert_eq!(
            c["drawingFields"][kind][field]["default"], expected,
            "{kind}.{field}"
        );
    }
    for (field, expected) in default_cell("").as_object().unwrap() {
        assert_eq!(&c["cellFields"][field]["default"], expected, "cell.{field}");
    }
}
