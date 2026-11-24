//! chart.point coordinates and expression evaluation regressions.
//! Expectations are scalar language semantics, independent of renderer geometry.
use pinewasm_compiler::{
    compile_and_run, CompileOptions, MarketData, RunOptions, RunResult, Value,
};
use serde_json::{json, Value as Json};

const FIRST: i64 = 1_700_000_000_000;
const STEP: i64 = 86_400_000;

fn run(body: &str) -> RunResult {
    let market = MarketData {
        open: vec![10.0; 8],
        high: vec![12.0; 8],
        low: vec![9.0; 8],
        close: vec![11.0; 8],
        volume: vec![100.0; 8],
        time: (0..8).map(|i| FIRST + i * STEP).collect(),
        timeframe: "1D".into(),
        ..MarketData::default()
    };
    let result = compile_and_run(
        &format!("//@version=6\nindicator(\"Point coordinates\",overlay=true)\n{body}"),
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
    result
}

fn drawings(result: &RunResult) -> Json {
    serde_json::to_value(&result.drawings).unwrap()
}

fn assert_plot(result: &RunResult, slot: usize, expected: &[i64]) {
    assert_eq!(result.plots[slot].values.len(), expected.len());
    for (value, expected) in result.plots[slot].values.iter().zip(expected) {
        assert!(
            matches!(value, Value::Int(n) if n==expected)
                || matches!(value, Value::Float(n) if *n == *expected as f64),
            "plot {slot}: {value:?} != {expected}"
        );
    }
}

#[test]
fn point_constructors_select_time_or_index_from_mixed_points() {
    for (mode, expected) in [("xloc.bar_index", 3), ("xloc.bar_time", FIRST)] {
        let r=run(&format!("if barstate.islast\n    p=chart.point.new({FIRST},3,12)\n    q=chart.point.new({},5,9)\n    line.new(p,q,xloc={mode})\n    box.new(p,q,xloc={mode})\n    label.new(p,\"P\",xloc={mode})",FIRST+2*STEP));
        let d = drawings(&r);
        for (family, field) in [("lines", "x1"), ("boxes", "left"), ("labels", "x")] {
            assert_eq!(
                d[family][0]["fields"][field],
                json!(expected),
                "{family}/{mode}"
            );
        }
        assert_eq!(d["lines"][0]["fields"]["y1"], json!(12.0));
        assert_eq!(d["boxes"][0]["fields"]["bottom"], json!(9.0));
    }
}

#[test]
fn from_time_points_supply_time_coordinates_to_every_point_constructor() {
    let r=run(&format!("if barstate.islast\n    p=chart.point.from_time({FIRST},12)\n    q=chart.point.from_time({},9)\n    line.new(p,q,xloc=xloc.bar_time)\n    box.new(p,q,xloc=xloc.bar_time)\n    label.new(p,\"P\",xloc=xloc.bar_time)",FIRST+STEP));
    let d = drawings(&r);
    assert_eq!(d["lines"][0]["fields"]["x1"], json!(FIRST));
    assert_eq!(d["lines"][0]["fields"]["x2"], json!(FIRST + STEP));
    assert_eq!(d["boxes"][0]["fields"]["left"], json!(FIRST));
    assert_eq!(d["boxes"][0]["fields"]["right"], json!(FIRST + STEP));
    assert_eq!(d["labels"][0]["fields"]["x"], json!(FIRST));
}

#[test]
fn missing_selected_point_coordinate_remains_absent_without_fallback() {
    for (point, mode) in [
        (
            format!("chart.point.from_time({FIRST},12)"),
            "xloc.bar_index",
        ),
        ("chart.point.from_index(3,12)".into(), "xloc.bar_time"),
    ] {
        let r=run(&format!("if barstate.islast\n    p={point}\n    line.new(p,p,xloc={mode})\n    box.new(p,p,xloc={mode})\n    label.new(p,\"P\",xloc={mode})"));
        let d = drawings(&r);
        for (family, field) in [
            ("lines", "x1"),
            ("lines", "x2"),
            ("boxes", "left"),
            ("boxes", "right"),
            ("labels", "x"),
        ] {
            assert!(
                d[family][0]["fields"][field].is_null(),
                "{family}/{mode}: {}",
                d[family][0]["fields"][field]
            );
        }
    }
}

#[test]
fn series_xloc_selects_each_bars_coordinate_without_changing_price() {
    let r=run("p=chart.point.new(time,bar_index,high)\nq=chart.point.new(time+86400000,bar_index+1,low)\nm=bar_index%2==0?xloc.bar_time:xloc.bar_index\nline.new(p,q,xloc=m)\nbox.new(p,q,xloc=m)\nlabel.new(p,\"P\",xloc=m)");
    let d = drawings(&r);
    for i in 0..8 {
        let x = if i % 2 == 0 {
            FIRST + (i as i64) * STEP
        } else {
            i as i64
        };
        for (family, field) in [("lines", "x1"), ("boxes", "left"), ("labels", "x")] {
            assert_eq!(d[family][i]["fields"][field], json!(x), "{family}/bar {i}");
        }
    }
}

#[test]
fn line_point_constructor_evaluates_each_direct_pop_once() {
    let r=run("a=array.from(100,101,102,103,104,105)\nline.new(chart.point.from_index(array.pop(a),12),chart.point.from_index(array.pop(a),9))\nplot(array.size(a))");
    assert_plot(&r, 0, &[4; 8]);
    let d = drawings(&r);
    assert_eq!(d["lines"][0]["fields"]["x1"], json!(105));
    assert_eq!(d["lines"][0]["fields"]["x2"], json!(104));
}

#[test]
fn box_point_constructor_evaluates_each_direct_pop_once() {
    let r=run("a=array.from(100,101,102,103,104,105)\nbox.new(chart.point.from_index(array.pop(a),12),chart.point.from_index(array.pop(a),9))\nplot(array.size(a))");
    assert_plot(&r, 0, &[4; 8]);
    let d = drawings(&r);
    assert_eq!(d["boxes"][0]["fields"]["left"], json!(105));
    assert_eq!(d["boxes"][0]["fields"]["right"], json!(104));
}

#[test]
fn label_point_constructor_evaluates_direct_pop_once() {
    let r=run("a=array.from(100,101,102,103)\nlabel.new(chart.point.from_index(array.pop(a),12),\"P\")\nplot(array.size(a))");
    assert_plot(&r, 0, &[3; 8]);
    assert_eq!(drawings(&r)["labels"][0]["fields"]["x"], json!(103));
}

#[test]
fn all_point_setters_select_the_receivers_current_xloc_and_copy_scalars() {
    for mode in ["xloc.bar_time", "xloc.bar_index"] {
        let r=run(&format!("if barstate.islast\n    p=chart.point.new({FIRST},3,12)\n    q=chart.point.new({},5,9)\n    l=line.new(0,0,1,1,xloc={mode})\n    b=box.new(0,0,1,1,xloc={mode})\n    v=label.new(0,0,\"P\",xloc={mode})\n    line.set_first_point(l,p)\n    l.set_second_point(q)\n    box.set_top_left_point(b,p)\n    b.set_bottom_right_point(q)\n    label.set_point(v,p)\n    p.index:=77\n    p.time:={}\n    p.price:=88\n    q.index:=99",FIRST+STEP,FIRST+7*STEP));
        let d = drawings(&r);
        let x1 = if mode == "xloc.bar_time" { FIRST } else { 3 };
        let x2 = if mode == "xloc.bar_time" {
            FIRST + STEP
        } else {
            5
        };
        for (family, field) in [("lines", "x1"), ("boxes", "left"), ("labels", "x")] {
            assert_eq!(d[family][0]["fields"][field], json!(x1), "{family}/{mode}");
        }
        assert_eq!(d["lines"][0]["fields"]["x2"], json!(x2));
        assert_eq!(d["boxes"][0]["fields"]["right"], json!(x2));
        assert_eq!(d["labels"][0]["fields"]["y"], json!(12.0));
    }
}

#[test]
fn point_setter_arguments_evaluate_once_and_a_direct_receiver_registers_once() {
    for statement in [
        "line.set_first_point(line.new(0,0,1,1),chart.point.from_index(array.pop(a),12))",
        "line.set_second_point(line.new(0,0,1,1),chart.point.from_index(array.pop(a),12))",
        "box.set_top_left_point(box.new(0,0,1,1),chart.point.from_index(array.pop(a),12))",
        "box.set_bottom_right_point(box.new(0,0,1,1),chart.point.from_index(array.pop(a),12))",
        "label.set_point(label.new(0,0),chart.point.from_index(array.pop(a),12))",
    ] {
        let r = run(&format!(
            "a=array.from(100,101,102,103)\n{statement}\nplot(array.size(a))"
        ));
        assert_plot(&r, 0, &[3; 8]);
        let d = drawings(&r);
        let family = if statement.starts_with("line") {
            "lines"
        } else if statement.starts_with("box") {
            "boxes"
        } else {
            "labels"
        };
        assert_eq!(d[family].as_array().unwrap().len(), 8);
    }
}

#[test]
fn named_point_setters_follow_the_public_id_and_point_signature() {
    let r=run(&format!("if barstate.islast\n    p=chart.point.new({FIRST},3,12)\n    l=line.new(0,0,1,1,xloc=xloc.bar_time)\n    b=box.new(0,0,1,1,xloc=xloc.bar_time)\n    v=label.new(0,0,xloc=xloc.bar_time)\n    line.set_first_point(point=p,id=l)\n    line.set_second_point(id=l,point=p)\n    box.set_top_left_point(point=p,id=b)\n    box.set_bottom_right_point(id=b,point=p)\n    label.set_point(point=p,id=v)"));
    let d = drawings(&r);
    for (family, field) in [
        ("lines", "x1"),
        ("lines", "x2"),
        ("boxes", "left"),
        ("boxes", "right"),
        ("labels", "x"),
    ] {
        assert_eq!(d[family][0]["fields"][field], json!(FIRST));
    }
}

#[test]
fn persistent_point_constructor_keeps_direct_effects_inside_first_initialization() {
    let r=run("var a=array.from(100,101,102,103)\nvar l=label.new(chart.point.from_index(array.pop(a),12),\"P\")\nplot(array.size(a))");
    assert_plot(&r, 0, &[3; 8]);
    assert_eq!(r.drawings.labels.len(), 1);
}

#[test]
fn skipped_conditional_and_lazy_boolean_calls_do_not_consume_point_arguments() {
    let r=run("a=array.from(100,101,102,103)\nif bar_index==0\n    label.new(chart.point.from_index(array.pop(a),12),\"P\")\nplot(array.size(a))\nb=array.from(100,101,102,103)\nok=bar_index==0 and not na(label.new(chart.point.from_index(array.pop(b),12),\"P\"))\nplot(array.size(b))");
    assert_plot(&r, 0, &[3, 4, 4, 4, 4, 4, 4, 4]);
    assert_plot(&r, 1, &[3, 4, 4, 4, 4, 4, 4, 4]);
    assert_eq!(r.drawings.labels.len(), 2);
}

#[test]
fn loop_calls_rebind_each_execution_and_each_argument_order() {
    let r=run("a=array.from(100,101,102,103,104,105)\nfor i=0 to 1\n    label.new(chart.point.from_index(array.pop(a),12),\"P\")\nplot(array.size(a))");
    assert_plot(&r, 0, &[4; 8]);
    let d = drawings(&r);
    assert_eq!(d["labels"][0]["fields"]["x"], json!(105));
    assert_eq!(d["labels"][1]["fields"]["x"], json!(104));
}

#[test]
fn now_carries_current_time_and_index_and_omitted_price_is_close() {
    let r=run("p=chart.point.now(high)\nq=chart.point.now()\nplot(p.time)\nplot(p.index)\nplot(p.price)\nplot(q.price)\nplot(q.time)\nplot(q.index)\nz=chart.point.now(na)\nplot(na(z.price)?1:0)");
    let times: Vec<_> = (0..8).map(|i| FIRST + i * STEP).collect();
    let indices: Vec<_> = (0..8).collect();
    assert_plot(&r, 0, &times);
    assert_plot(&r, 1, &indices);
    assert_plot(&r, 2, &[12; 8]);
    assert_plot(&r, 3, &[11; 8]);
    assert_plot(&r, 4, &times);
    assert_plot(&r, 5, &indices);
    assert_plot(&r, 6, &[1; 8]);
}

#[test]
fn parenthesized_points_select_the_point_overload() {
    let r=run(&format!("if barstate.islast\n    p=chart.point.new({FIRST},3,12)\n    line.new((p),(p),xloc=xloc.bar_time)\n    box.new((p),(p),xloc=xloc.bar_time)\n    label.new((p),\"P\",xloc=xloc.bar_time)"));
    for (family, field) in [("lines", "x1"), ("boxes", "left"), ("labels", "x")] {
        assert_eq!(drawings(&r)[family][0]["fields"][field], json!(FIRST));
    }
}

#[test]
fn point_fields_select_the_point_overload() {
    let r=run(&format!("type Holder\n    chart.point anchor\nif barstate.islast\n    h=Holder.new(chart.point.new({FIRST},3,12))\n    line.new(h.anchor,h.anchor,xloc=xloc.bar_time)\n    box.new(h.anchor,h.anchor,xloc=xloc.bar_time)\n    label.new(h.anchor,\"P\",xloc=xloc.bar_time)"));
    for (family, field) in [("lines", "x1"), ("boxes", "left"), ("labels", "x")] {
        assert_eq!(drawings(&r)[family][0]["fields"][field], json!(FIRST));
    }
}

#[test]
fn direct_point_collection_reads_select_the_overload_and_pop_once() {
    for statement in [
        "line.new(array.pop(a),array.pop(a),xloc=xloc.bar_time)",
        "box.new(array.pop(a),array.pop(a),xloc=xloc.bar_time)",
        "label.new(array.pop(a),\"P\",xloc=xloc.bar_time)",
    ] {
        let r=run(&format!("a=array.from(chart.point.new({FIRST},3,12),chart.point.new({},5,9),chart.point.new({},7,11))\n{statement}\nplot(array.size(a))",FIRST+STEP,FIRST+2*STEP));
        assert_plot(
            &r,
            0,
            &[if statement.starts_with("label") { 2 } else { 1 }; 8],
        );
        let (family, field) = if statement.starts_with("line") {
            ("lines", "x1")
        } else if statement.starts_with("box") {
            ("boxes", "left")
        } else {
            ("labels", "x")
        };
        assert_eq!(
            drawings(&r)[family][0]["fields"][field],
            json!(FIRST + 2 * STEP)
        );
    }
}

#[test]
fn side_effecting_point_price_and_dynamic_xloc_are_captured_once_in_order() {
    let r=run(&format!("a=array.from(100,101,102,103,104,105)\nline.new(chart.point.new({FIRST},array.pop(a),array.pop(a)),chart.point.new({},array.pop(a),array.pop(a)),xloc=array.pop(a)==101?xloc.bar_time:xloc.bar_index)\nplot(array.size(a))",FIRST+STEP));
    assert_plot(&r, 0, &[1; 8]);
    let d = drawings(&r);
    assert_eq!(d["lines"][0]["fields"]["x1"], json!(FIRST));
    assert_eq!(d["lines"][0]["fields"]["x2"], json!(FIRST + STEP));
    assert_eq!(d["lines"][0]["fields"]["y1"], json!(104.0));
    assert_eq!(d["lines"][0]["fields"]["y2"], json!(102.0));
}

#[test]
fn setters_use_xloc_after_reanchoring_and_keep_missing_selected_coordinates() {
    let r=run(&format!("if barstate.islast\n    p=chart.point.new({FIRST},3,12)\n    l=line.new(0,0,1,1)\n    b=box.new(0,0,1,1)\n    v=label.new(0,0)\n    line.set_xloc(l,{FIRST},{},xloc.bar_time)\n    box.set_xloc(b,{FIRST},{},xloc.bar_time)\n    label.set_xloc(v,{FIRST},xloc.bar_time)\n    l.set_first_point(p)\n    b.set_top_left_point(p)\n    v.set_point(p)\n    q=chart.point.from_index(7,9)\n    l.set_second_point(q)\n    b.set_bottom_right_point(q)",FIRST+STEP,FIRST+STEP));
    let d = drawings(&r);
    for (family, field) in [("lines", "x1"), ("boxes", "left"), ("labels", "x")] {
        assert_eq!(d[family][0]["fields"][field], json!(FIRST));
    }
    assert!(d["lines"][0]["fields"]["x2"].is_null());
    assert!(d["boxes"][0]["fields"]["right"].is_null());
}

#[test]
fn constructor_scalars_are_copied_before_later_point_mutation() {
    let r=run(&format!("if barstate.islast\n    p=chart.point.new({FIRST},3,12)\n    line.new(p,p,xloc=xloc.bar_time)\n    box.new(p,p,xloc=xloc.bar_time)\n    label.new(p,\"P\",xloc=xloc.bar_time)\n    p.index:=99\n    p.time:={}\n    p.price:=77",FIRST+STEP));
    let d = drawings(&r);
    for (family, x, y) in [
        ("lines", "x1", "y1"),
        ("boxes", "left", "top"),
        ("labels", "x", "y"),
    ] {
        assert_eq!(d[family][0]["fields"][x], json!(FIRST));
        assert_eq!(d[family][0]["fields"][y], json!(12.0));
    }
}

#[test]
fn canonical_legacy_time_xloc_keeps_its_existing_coordinate_meaning() {
    let r=run(&format!("if barstate.islast\n    p=chart.point.new({FIRST},3,12)\n    line.new(p,p,xloc=\"xloc.bar_time\")\n    box.new(p,p,xloc=\"xloc.bar_time\")\n    label.new(p,\"P\",xloc=\"xloc.bar_time\")"));
    for (family, field) in [("lines", "x1"), ("boxes", "left"), ("labels", "x")] {
        assert_eq!(drawings(&r)[family][0]["fields"][field], json!(FIRST));
    }
}

#[test]
fn local_dotted_line_point_pop_selects_point_overload_once() {
    let r = run("if barstate.islast\n    a=array.from(chart.point.from_index(3,12),chart.point.from_index(5,9))\n    q=chart.point.from_index(3,12)\n    line.new(a.pop(),q)\n    label.new(0,0,str.tostring(array.size(a)))");
    let d = drawings(&r);
    assert_eq!(d["lines"][0]["fields"]["x1"], json!(5));
    assert_eq!(d["lines"][0]["fields"]["y1"], json!(9.0));
    assert_eq!(d["labels"][0]["fields"]["text"], json!("1"));
}

#[test]
fn local_dotted_box_point_pop_selects_point_overload_once() {
    let r = run("if barstate.islast\n    a=array.from(chart.point.from_index(3,12),chart.point.from_index(5,9))\n    q=chart.point.from_index(3,12)\n    box.new(a.pop(),q)\n    label.new(0,0,str.tostring(array.size(a)))");
    let d = drawings(&r);
    assert_eq!(d["boxes"][0]["fields"]["left"], json!(5));
    assert_eq!(d["boxes"][0]["fields"]["top"], json!(9.0));
    assert_eq!(d["labels"][0]["fields"]["text"], json!("1"));
}

#[test]
fn local_dotted_label_point_pop_selects_point_overload_once() {
    let r = run("if barstate.islast\n    a=array.from(chart.point.from_index(3,12),chart.point.from_index(5,9))\n    label.new(a.pop(),\"P\")\n    label.new(0,0,str.tostring(array.size(a)))");
    let d = drawings(&r);
    assert_eq!(d["labels"][0]["fields"]["x"], json!(5));
    assert_eq!(d["labels"][0]["fields"]["y"], json!(9.0));
    assert_eq!(d["labels"][1]["fields"]["text"], json!("1"));
}

#[test]
fn dotted_array_point_read_methods_bind_without_speculative_evaluation() {
    for (method, index, remaining) in [
        ("get(0)", 3, 2),
        ("pop()", 5, 1),
        ("shift()", 3, 1),
        ("first()", 3, 2),
        ("last()", 5, 2),
    ] {
        for statement in [
            format!("line.new(a.{method},q)"),
            format!("box.new(a.{method},q)"),
            format!("label.new(a.{method},\"P\")"),
        ] {
            let r=run(&format!("if barstate.islast\n    a=array.from(chart.point.from_index(3,12),chart.point.from_index(5,9))\n    q=chart.point.from_index(3,12)\n    {statement}\n    label.new(0,0,str.tostring(array.size(a)))"));
            let d = drawings(&r);
            let (family, field) = if statement.starts_with("line") {
                ("lines", "x1")
            } else if statement.starts_with("box") {
                ("boxes", "left")
            } else {
                ("labels", "x")
            };
            assert_eq!(d[family][0]["fields"][field], json!(index), "{statement}");
            let witness = usize::from(family == "labels");
            assert_eq!(
                d["labels"][witness]["fields"]["text"],
                json!(remaining.to_string()),
                "{statement}"
            );
        }
    }
}

#[test]
fn dotted_map_and_matrix_point_reads_select_point_overloads() {
    for (setup, read) in [
        (
            "m=map.new<string,chart.point>()\nmap.put(m,\"k\",chart.point.from_index(3,12))",
            "m.get(\"k\")",
        ),
        (
            "m=matrix.new<chart.point>(1,1,chart.point.from_index(3,12))",
            "m.get(0,0)",
        ),
    ] {
        let r=run(&format!("{setup}\nq=chart.point.from_index(3,12)\nline.new({read},q)\nbox.new({read},q)\nlabel.new({read},\"P\")"));
        let d = drawings(&r);
        for (family, field) in [("lines", "x1"), ("boxes", "left"), ("labels", "x")] {
            assert_eq!(d[family][0]["fields"][field], json!(3), "{read}");
        }
    }
}
