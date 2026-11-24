//! Rendering contract regression tests for drawing string constants and values.
use pinewasm_compiler::{compile_and_run, CompileOptions, MarketData, RunOptions, RunResult};
use serde_json::{json, Value};

fn string_samples() -> Vec<(String, String)> {
    let reference: Value =
        serde_json::from_str(include_str!("fixtures/drawing-constants.json")).unwrap();
    reference["constants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["name"].as_str().unwrap().to_string(),
                c["value"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn run(body: &str) -> RunResult {
    let market = MarketData {
        open: vec![10.0; 8],
        high: vec![12.0; 8],
        low: vec![9.0; 8],
        close: vec![11.0; 8],
        volume: vec![100.0; 8],
        timeframe: "1D".into(),
        ..MarketData::default()
    };
    let source = format!("//@version=6\nindicator(\"Drawing strings\",overlay=true)\n{body}");
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
    result
}

#[test]
fn sixty_sampled_values_survive_inputs_comparisons_and_concatenation() {
    let mut source = String::new();
    for (i, (name, value)) in string_samples().iter().enumerate() {
        source.push_str(&format!("s{i}=input.string({name},{name:?})\nplot(s{i} == {value:?} and \"[\"+{name}+\"]\" == {:?} ? 1 : 0)\n",format!("[{value}]")));
    }
    let result = run(&source);
    let program = result.compile.as_ref().unwrap().program.as_ref().unwrap();
    assert_eq!(program.inputs.len(), 60);
    assert_eq!(result.plots.len(), 60);
    for (i, (name, value)) in string_samples().iter().enumerate() {
        assert_eq!(
            serde_json::to_value(&program.inputs[i].defval).unwrap(),
            json!({"String":value}),
            "{name}"
        );
        assert!(
            result.plots[i].values.iter().all(|v| matches!(
                v,
                pinewasm_compiler::Value::Int(1) | pinewasm_compiler::Value::Float(1.0)
            )),
            "{name} comparison or concatenation"
        );
    }
}

fn constructors(name: &str, expr: &str) -> Vec<(&'static str, &'static str, String)> {
    let mut cases = Vec::new();
    let namespace = name.split('.').next().unwrap();
    match namespace {
        "position" => cases.push(("tables", "position", format!("table.new({expr},1,1)"))),
        "xloc" => {
            cases.push(("lines", "xloc", format!("line.new(0,12,1,9,xloc={expr})")));
            cases.push((
                "labels",
                "xloc",
                format!("label.new(0,12,\"Text\",xloc={expr})"),
            ));
            cases.push(("boxes", "xloc", format!("box.new(0,12,1,9,xloc={expr})")));
            cases.push(("polylines","xloc",format!("polyline.new(array.from(chart.point.from_index(0,12),chart.point.from_index(1,9)),xloc={expr})")));
        }
        "yloc" => cases.push((
            "labels",
            "yloc",
            format!("label.new(0,12,\"Text\",yloc={expr})"),
        )),
        "extend" => {
            cases.push((
                "lines",
                "extend",
                format!("line.new(0,12,1,9,extend={expr})"),
            ));
            cases.push((
                "boxes",
                "extend",
                format!("box.new(0,12,1,9,extend={expr})"),
            ));
        }
        "line" => {
            cases.push(("lines", "style", format!("line.new(0,12,1,9,style={expr})")));
            cases.push((
                "boxes",
                "border_style",
                format!("box.new(0,12,1,9,border_style={expr})"),
            ));
            cases.push(("polylines","line_style",format!("polyline.new(array.from(chart.point.from_index(0,12),chart.point.from_index(1,9)),line_style={expr})")));
        }
        "label" => cases.push((
            "labels",
            "style",
            format!("label.new(0,12,\"Text\",style={expr})"),
        )),
        "size" => {
            cases.push((
                "labels",
                "size",
                format!("label.new(0,12,\"Text\",size={expr})"),
            ));
            cases.push((
                "boxes",
                "text_size",
                format!("box.new(0,12,1,9,text_size={expr})"),
            ));
            cases.push((
                "cell",
                "text_size",
                format!("table.cell(t,0,0,\"Text\",text_size={expr})"),
            ));
        }
        "text" if name.starts_with("text.align_") => {
            if !matches!(name, "text.align_top" | "text.align_bottom") {
                cases.push((
                    "labels",
                    "textalign",
                    format!("label.new(0,12,\"Text\",textalign={expr})"),
                ));
                cases.push((
                    "boxes",
                    "text_halign",
                    format!("box.new(0,12,1,9,text_halign={expr})"),
                ));
                cases.push((
                    "cell",
                    "text_halign",
                    format!("table.cell(t,0,0,\"Text\",text_halign={expr})"),
                ));
            }
            if !matches!(name, "text.align_left" | "text.align_right") {
                cases.push((
                    "boxes",
                    "text_valign",
                    format!("box.new(0,12,1,9,text_valign={expr})"),
                ));
                cases.push((
                    "cell",
                    "text_valign",
                    format!("table.cell(t,0,0,\"Text\",text_valign={expr})"),
                ));
            }
        }
        "text" => cases.push((
            "boxes",
            "text_wrap",
            format!("box.new(0,12,1,9,text_wrap={expr})"),
        )),
        "font" => {
            cases.push((
                "labels",
                "text_font_family",
                format!("label.new(0,12,\"Text\",text_font_family={expr})"),
            ));
            cases.push((
                "boxes",
                "text_font_family",
                format!("box.new(0,12,1,9,text_font_family={expr})"),
            ));
            cases.push((
                "cell",
                "text_font_family",
                format!("table.cell(t,0,0,\"Text\",text_font_family={expr})"),
            ));
        }
        _ => panic!("unhandled fixture constant {name}"),
    }
    cases
}

fn distinct_table_position(name: &str) -> &'static str {
    [
        "top_left",
        "top_center",
        "top_right",
        "middle_left",
        "middle_center",
        "middle_right",
        "bottom_left",
        "bottom_center",
        "bottom_right",
    ]
    .into_iter()
    .find(|position| format!("position.{position}") != name)
    .expect("the fixture defines multiple table positions")
}

#[test]
fn sampled_constants_and_literal_values_emit_canonical_tokens_in_each_field_domain() {
    for (name, value) in string_samples() {
        for expr in [&name, &format!("{value:?}")] {
            for (group, field, constructor) in constructors(&name, expr) {
                let base_position = if group == "tables" {
                    distinct_table_position(&name)
                } else {
                    "bottom_left"
                };
                let d=serde_json::to_value(run(&format!("if barstate.islast\n    t=table.new(position.{base_position},1,1)\n    {constructor}\n")).drawings).unwrap();
                let actual = if group == "cell" {
                    &d["tables"][0]["cells"][0]["attrs"][field]
                } else if group == "tables" {
                    &d[group][1]["fields"][field]
                } else {
                    &d[group][0]["fields"][field]
                };
                assert_eq!(actual, &json!(name), "{expr} in {group}.{field}");
            }
        }
    }
}

#[test]
fn arbitrary_text_and_colliding_option_values_are_never_globally_rewritten() {
    let d = serde_json::to_value(
        run(r#"if barstate.islast
    line.new(0,12,1,9,style="ab",extend="n")
    label.new(0,12,"ab",style="n",yloc="ab",tooltip="bi")
    label.new(0,12,"n",style="ab")
    box.new(0,12,1,9,text="bt",text_wrap="auto",text_font_family="monospace")
    t=table.new("top_right",1,1)
    table.cell(t,0,0,"normal",text_size="normal",tooltip="default")
"#)
        .drawings,
    )
    .unwrap();
    assert_eq!(d["lines"][0]["fields"]["style"], "line.style_arrow_both");
    assert_eq!(d["lines"][0]["fields"]["extend"], "extend.none");
    assert_eq!(d["labels"][0]["fields"]["style"], "label.style_none");
    assert_eq!(d["labels"][0]["fields"]["yloc"], "yloc.abovebar");
    assert_eq!(d["labels"][0]["fields"]["text"], "ab");
    assert_eq!(d["labels"][0]["fields"]["tooltip"], "bi");
    assert_eq!(
        d["labels"][1]["fields"]["style"], "ab",
        "not a valid label style: the embedding application must reject it"
    );
    assert_eq!(d["boxes"][0]["fields"]["text"], "bt");
    assert_eq!(d["boxes"][0]["fields"]["text_wrap"], "text.wrap_auto");
    assert_eq!(d["tables"][0]["cells"][0]["attrs"]["text"], "normal");
    assert_eq!(d["tables"][0]["cells"][0]["attrs"]["tooltip"], "default");
    assert_eq!(
        d["tables"][0]["cells"][0]["attrs"]["text_size"],
        "size.normal"
    );
}

#[test]
fn sampled_size_values_also_bind_to_plotshape_metadata() {
    use pinewasm_hir::MarkerSize;
    for (name, value, expected) in [
        ("size.auto", "auto", MarkerSize::Auto),
        ("size.tiny", "tiny", MarkerSize::Tiny),
        ("size.small", "small", MarkerSize::Small),
        ("size.normal", "normal", MarkerSize::Normal),
        ("size.large", "large", MarkerSize::Large),
        ("size.huge", "huge", MarkerSize::Huge),
    ] {
        for expr in [name.to_string(), format!("{value:?}")] {
            let result = run(&format!(
                "plotshape(true,size={expr})\nplotchar(true,size={expr})\n"
            ));
            for plot in result.plots {
                assert_eq!(plot.style.marker_size, Some(expected), "{expr}");
                assert!(
                    plot.style.unsupported.is_empty(),
                    "{expr}: {:?}",
                    plot.style.unsupported
                );
            }
        }
    }
}

#[test]
fn generated_metadata_distinguishes_pine_values_from_canonical_dto_option_domains() {
    let contract = pinewasm_compiler::rendering_contract();
    for (name, value) in string_samples() {
        assert_eq!(contract["constants"][&name], value, "{name}");
    }
    assert_eq!(
        contract["drawingOptionDomains"]["line"]["style"]["ab"],
        "line.style_arrow_both"
    );
    assert!(contract["drawingOptionDomains"]["label"]["style"]
        .get("ab")
        .is_none());
    assert_eq!(
        contract["drawingOptionDomains"]["label"]["yloc"]["ab"],
        "yloc.abovebar"
    );
    assert_eq!(
        contract["cellOptionDomains"]["text_size"]["normal"],
        "size.normal"
    );
    assert_eq!(
        contract["drawingFields"]["label"]["size"]["default"],
        "size.normal"
    );
}
