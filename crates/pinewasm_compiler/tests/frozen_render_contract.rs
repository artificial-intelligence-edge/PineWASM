//! Regressions for PineWASM's rendering contract.
use pinewasm_compiler::{compile_and_run, CompileOptions, MarketData, RunOptions};

fn run(source: &str) -> pinewasm_compiler::RunResult {
    let mut data = MarketData {
        open: vec![10.0; 8],
        high: vec![12.0; 8],
        low: vec![9.0; 8],
        close: vec![11.0; 8],
        volume: vec![100.0; 8],
        timeframe: "1D".into(),
        ..MarketData::default()
    };
    data.syminfo.ticker = "EURUSD".into();
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

#[test]
fn omitted_plot_colors_are_engine_authored_blue_independent_of_slot_order() {
    let result = run("//@version=6\nindicator(\"Default plot colors\")\nplot(close,\"First\")\nplot(open,\"Second\")\nplot(high,\"Explicit missing\",color=na)\n");
    for plot in &result.plots[..2] {
        assert_eq!(plot.colors.len(), 1);
        assert_eq!(plot.colors[0].role, pinewasm_hir::PlotColorRole::Color);
        assert!(plot.colors[0].values.iter().all(|color| color
            == &pinewasm_compiler::Value::Color {
                r: 33,
                g: 150,
                b: 243,
                t: 0.0,
                input_origin: None
            }));
        assert_eq!(plot.style.color_palette.len(), 1);
        assert_eq!(
            plot.style.color_palette[0].resolved_color.as_deref(),
            Some("#2196F3")
        );
    }
    assert!(result.plots[2].colors[0]
        .values
        .iter()
        .all(pinewasm_compiler::Value::is_na));
    assert!(!result.plots[2].style.color_editable);
}

#[test]
fn omitted_marker_arguments_emit_the_documented_shape_location_and_size() {
    use pinewasm_hir::{MarkerLocation, MarkerShape, MarkerSize};
    let result = run("//@version=6\nindicator(\"Marker defaults\")\nplotshape(7,color=color.blue)\nplotshape(true,style=shape.circle,location=location.belowbar,size=size.normal,color=color.blue)\nplotchar(7,char=\"X\",color=color.red)\n");
    assert_eq!(
        result.plots[0].style.marker_shape,
        Some(MarkerShape::XCross)
    );
    assert_eq!(
        result.plots[0].style.marker_location,
        Some(MarkerLocation::AboveBar)
    );
    assert_eq!(result.plots[0].style.marker_size, Some(MarkerSize::Auto));
    assert_eq!(
        result.plots[1].style.marker_shape,
        Some(MarkerShape::Circle)
    );
    assert_eq!(
        result.plots[1].style.marker_location,
        Some(MarkerLocation::BelowBar)
    );
    assert_eq!(result.plots[1].style.marker_size, Some(MarkerSize::Normal));
    assert_eq!(
        result.plots[2].style.marker_location,
        Some(MarkerLocation::AboveBar)
    );
    assert_eq!(result.plots[2].style.marker_size, Some(MarkerSize::Auto));
    assert_eq!(result.plots[2].style.character.as_deref(), Some("X"));
}

#[test]
fn automatic_color_choices_follow_provenance_not_the_output_qualifier() {
    use pinewasm_hir::{PlotColorRole, StyleColorSource};
    let result = run(r#"//@version=6
indicator("Automatic color choices")
bull = input.color(color.teal)
c = close > open ? color.new(color.teal, 50) : color.new(color.red, 50)
plot(close, "Constants", color=c)
plot(close, "Input", color=close > open ? bull : color.red)
choice = switch
    close > open => color.blue
    close < open => color.red
    => color.yellow
plot(close, "Switch", color=choice)
tupleColors() => [close, color.purple]
[value, shade] = tupleColors()
plot(value, "Tuple", color=shade)
"#);
    assert!(result.plots.iter().all(|plot| plot.style.color_editable));
    assert_eq!(result.plots[0].style.color_palette.len(), 2);
    assert!(result.plots[0]
        .style
        .color_palette
        .iter()
        .all(|entry| matches!(entry.source, StyleColorSource::Constant { t: 50.0, .. })));
    assert_eq!(
        result.plots[1].style.color_palette[0].source,
        StyleColorSource::Input { input: 0 }
    );
    assert!(result.plots[1]
        .style
        .color_palette
        .iter()
        .all(|entry| entry.role == PlotColorRole::Color));
    assert_eq!(result.plots[2].style.color_palette.len(), 3);
    assert_eq!(result.plots[3].style.color_palette.len(), 1);
    assert_eq!(
        result.plots[0].style.color_palette[0]
            .resolved_color
            .as_deref(),
        Some("#08998180")
    );
    let hidden = run("//@version=6\nindicator(\"Missing color\")\nplot(close,color=na)\nplot(open,color=color.blue)\n");
    assert!(!hidden.plots[0].style.color_editable);
    assert!(hidden.plots[1].style.color_editable);
}

#[test]
fn a_runtime_color_calculation_disables_automatic_selectors_script_wide() {
    for calculation in [
        "color.new(close > open ? color.teal : color.red, 50)",
        "color.new(input.color(color.teal), 50)",
        "color.rgb(10, 20, input.int(30), 50)",
        "color.from_gradient(1, 0, 2, color.red, color.green)",
    ] {
        let result = run(&format!(
            r#"//@version=6
indicator("Runtime colors")
plot(close, "Fixed", color=color.blue)
shade = {calculation}
plot(close, "Calculated", color=shade)
"#
        ));
        assert!(
            result.plots.iter().all(|plot| !plot.style.color_editable),
            "{calculation}"
        );
    }
    let refused = pinewasm_compiler::compile(
        r#"//@version=6
indicator("Gradient qualifier")
const color c = color.from_gradient(1,0,2,color.red,color.green)
plot(close,color=c)
"#,
        CompileOptions::default(),
    );
    assert!(!refused.success);
    assert!(
        refused
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0226")),
        "{:?}",
        refused.diagnostics
    );
}

#[test]
fn automatic_palette_roles_and_input_identities_are_validated_before_execution() {
    use pinewasm_hir::{PlotColorRole, StyleColorSource};
    let result = run(r#"//@version=6
indicator("Palette admission")
c = input.color(color.blue)
plot(close,color=c)
"#);
    let program = result.compile.unwrap().program.unwrap();
    let mut malformed = program.clone();
    malformed.plots[0].style.color_palette[0].source = StyleColorSource::Input { input: 99 };
    assert!(pinewasm_csil::validate(&malformed).is_err());
    malformed = program.clone();
    malformed.plots[0].style.color_palette[0].role = PlotColorRole::Text;
    assert!(pinewasm_csil::validate(&malformed).is_err());
    malformed = program.clone();
    let duplicate = malformed.plots[0].style.color_palette[0].clone();
    malformed.plots[0].style.color_palette.push(duplicate);
    assert!(pinewasm_csil::validate(&malformed).is_err());
    malformed = program;
    malformed.plots[0].style.color_palette[0].source = StyleColorSource::Constant {
        r: 0,
        g: 0,
        b: 0,
        t: 101.0,
    };
    assert!(pinewasm_csil::validate(&malformed).is_err());
}

#[test]
fn fixed_functions_and_simple_context_styles_are_resolved_by_the_engine() {
    let result = run(r#"//@version=6
indicator("Engine styles")
n = input.int(7)
width = int(math.pow(n, 2))
plot(close, linewidth=width, offset=timeframe.isdaily ? -2 : 1)
plotshape(true, text="ENTER", offset=str.length(syminfo.ticker))
hline(20, linewidth=width)
"#);
    let program = result.compile.as_ref().unwrap().program.as_ref().unwrap();
    assert_eq!(program.plots[0].style.runtime_scalars.len(), 2);
    assert_eq!(program.plots[1].style.runtime_scalars.len(), 1);
    assert_eq!(program.plots[2].style.runtime_scalars.len(), 1);
    for plot in &result.plots {
        assert!(plot.style.unsupported.is_empty(), "{:?}", plot.style);
        assert!(plot.style.runtime_scalars.is_empty());
    }
    let fixed = |plot: usize, field: &str| {
        result.plots[plot]
            .style
            .bound_scalars
            .iter()
            .find(|entry| entry.field == field)
            .unwrap()
            .expression
            .clone()
    };
    use pinewasm_hir::{InputActiveExpr, InputValue};
    assert_eq!(
        fixed(0, "linewidth"),
        InputActiveExpr::Value {
            value: InputValue::Int(49)
        }
    );
    assert_eq!(
        fixed(0, "offset"),
        InputActiveExpr::Value {
            value: InputValue::Int(-2)
        }
    );
    assert_eq!(
        fixed(1, "offset"),
        InputActiveExpr::Value {
            value: InputValue::Int(6)
        }
    );
    assert_eq!(
        fixed(2, "linewidth"),
        InputActiveExpr::Value {
            value: InputValue::Int(49)
        }
    );
    let mut corrupted = program.clone();
    corrupted.plots[0].style.runtime_scalars[0].slot.0 = u32::MAX;
    assert!(pinewasm_csil::validate(&corrupted).is_err());
    let mut corrupted = program.clone();
    corrupted.plots[0].style.runtime_scalars[0].qualifier = pinewasm_hir::Qualifier::Series;
    assert!(pinewasm_csil::validate(&corrupted).is_err());
}

#[test]
fn input_linewidth_snapshots_preserve_na_and_signed_values() {
    use pinewasm_hir::{InputActiveExpr, InputValue, Qualifier};
    let result = run(
        "//@version=6\nindicator(\"Width snapshots\")\nmirror=input.bool(false)\nwidth=input.int(0)\nplot(close,linewidth=mirror ? 2 : na)\nhline(40,linewidth=width)\nhline(20,linewidth=width - 2)",
    );
    let program = result.compile.as_ref().unwrap().program.as_ref().unwrap();
    for plot in &program.plots {
        assert!(plot.style.bound_scalars.is_empty());
        let [binding] = plot.style.runtime_scalars.as_slice() else {
            panic!("expected one runtime linewidth slot");
        };
        assert_eq!(binding.field, "linewidth");
        assert_eq!(binding.qualifier, Qualifier::Input);
    }
    for (plot, expected) in result.plots.iter().zip([
        InputActiveExpr::Na,
        InputActiveExpr::Value {
            value: InputValue::Int(0),
        },
        InputActiveExpr::Value {
            value: InputValue::Int(-2),
        },
    ]) {
        assert!(plot.style.runtime_scalars.is_empty());
        let [binding] = plot.style.bound_scalars.as_slice() else {
            panic!("expected one captured linewidth");
        };
        assert_eq!(binding.field, "linewidth");
        assert_eq!(binding.expression, expected);
    }
}

#[test]
fn v6_rejects_series_qualified_display_offsets_at_the_compiler_boundary() {
    let result = compile_and_run(
        r#"//@version=6
indicator("Series offset rejection")
plot(close, offset=bar_index)
plotshape(true, offset=bar_index)
plotchar(true, offset=bar_index)
plotarrow(1, offset=bar_index)
bgcolor(color.red, offset=bar_index)
barcolor(color.red, offset=bar_index)
"#,
        &MarketData::default(),
        CompileOptions::default(),
        &RunOptions::default(),
    );

    for function in [
        "plot",
        "plotshape",
        "plotchar",
        "plotarrow",
        "bgcolor",
        "barcolor",
    ] {
        assert!(
            result.diagnostics.iter().any(|diagnostic| {
                diagnostic.code.as_deref() == Some("E0206")
                    && diagnostic.message.contains(&format!("`{function}`"))
                    && diagnostic.message.contains("`offset`")
                    && diagnostic.message.contains("found series")
            }),
            "{function} must reject a series offset with E0206: {:?}",
            result.diagnostics
        );
    }
    assert_eq!(
        result
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code.as_deref() == Some("E0206"))
            .count(),
        6,
        "each offset consumer should report one qualifier error"
    );
}

#[test]
fn input_fixed_enum_and_switch_styles_are_bound_without_fallback_warnings() {
    let result = run(r#"//@version=6
indicator("Input contracts")
enabled = input.bool(false)
width = input.int(7)
kind = input.string("Histogram", options=["Line", "Histogram"])
mode = switch kind
    "Histogram" => plot.style_histogram
    => plot.style_line
offset = switch
    enabled => 0
    => -1
plot(close, linewidth=width, style=mode, display=enabled ? display.all : display.none,
     linestyle=enabled ? plot.linestyle_dashed : plot.linestyle_dotted)
plotshape(true, offset=offset)
"#);
    assert!(result.plots.iter().all(|p| p.style.unsupported.is_empty()));
    let fields: Vec<_> = result.plots[0]
        .style
        .bound_scalars
        .iter()
        .map(|b| b.field.as_str())
        .collect();
    for required in ["linewidth", "style", "display", "linestyle"] {
        assert!(fields.contains(&required));
    }
    assert_eq!(result.plots[1].style.bound_scalars[0].field, "offset");
}

#[test]
fn declaration_and_output_order_survive_every_lowering_boundary() {
    let result = run(r#"//@version=6
indicator("Order", shorttitle="Short", format=format.percent, precision=3,
          explicit_plot_zorder=true, behind_chart=false, scale=scale.left)
a = plot(20, display=display.none)
b = plot(0, display=display.none)
hline(10)
fill(a, b, color.blue)
plot(15)
"#);
    let program = result.compile.as_ref().unwrap().program.as_ref().unwrap();
    let declaration = program.metadata.indicator.as_ref().unwrap();
    assert!(declaration.explicit_plot_zorder);
    assert!(!declaration.behind_chart);
    assert_eq!(declaration.shorttitle.as_deref(), Some("Short"));
    assert_eq!(declaration.format.as_deref(), Some("format.percent"));
    assert_eq!(declaration.precision, Some(3));
    assert_eq!(declaration.scale.as_deref(), Some("scale.left"));
    assert_eq!(result.plots[2].style.source_order, Some(2));
    assert_eq!(result.fills[0].style.source_order, Some(3));
    assert_eq!(result.plots[3].style.source_order, Some(4));
}

#[test]
fn cone_linefill_references_are_serialized_as_live_drawing_ids() {
    let result = run(r#"//@version=6
indicator("Cone", overlay=true)
if barstate.islast
    upper = line.new(bar_index, close, bar_index+12, close+4)
    lower = line.new(bar_index, close, bar_index+12, close-4)
    linefill.new(upper, lower, color.new(color.blue, 65))
    line.set_y2(upper, close+6)
"#);
    let drawings = &result.drawings;
    let fill = &drawings.linefills[0];
    assert_eq!(
        fill.line_refs,
        Some([Some(drawings.lines[0].id), Some(drawings.lines[1].id)])
    );
    assert!(serde_json::to_value(fill)
        .unwrap()
        .get("lineRefs")
        .is_some());
}

#[test]
fn adjacent_table_merges_retain_separate_geometry() {
    let result = run(r#"//@version=6
indicator("Table regions")
if barstate.islast
    t = table.new(position.top_right, 4, 2)
    table.cell(t, 0, 0, "First", width=10, height=12, text_valign=text.align_bottom)
    table.cell(t, 2, 0, "Second", width=10)
    table.merge_cells(t, 0, 0, 1, 1)
    table.merge_cells(t, 2, 0, 3, 1)
"#);
    let cells = &result.drawings.tables[0].cells;
    assert_eq!(
        cells
            .iter()
            .find(|c| c.column == 0 && c.row == 0)
            .unwrap()
            .merge,
        Some([0, 0, 1, 1])
    );
    assert_eq!(
        cells
            .iter()
            .find(|c| c.column == 2 && c.row == 0)
            .unwrap()
            .merge,
        Some([2, 0, 3, 1])
    );
    assert_eq!(cells.len(), 8);
}

#[test]
fn repeated_linefills_emit_only_the_latest_pair_with_fresh_ordered_references() {
    let result = run(r#"//@version=6
indicator("Latest fill", overlay=true)
var upper = line.new(0, 2, 1, 3)
var lower = line.new(0, 0, 1, 1)
linefill.new(upper, lower, color.red)
linefill.new(lower, upper, color.blue)
plot(array.size(linefill.all))
"#);
    assert_eq!(result.drawings.linefills.len(), 1);
    let fill = &result.drawings.linefills[0];
    assert_eq!(fill.id, 17);
    assert_eq!(fill.line_refs, Some([Some(1), Some(0)]));
    assert_eq!(
        fill.fields["color"],
        pinewasm_compiler::Value::Color {
            r: 41,
            g: 98,
            b: 255,
            t: 0.0,
            input_origin: None
        }
    );
    assert_eq!(
        result.plots[0].values,
        vec![pinewasm_compiler::Value::Float(1.0); 8]
    );
}

#[test]
fn deleting_a_boundary_removes_linefills_from_both_live_arrays_and_wire_snapshots() {
    let result = run(r#"//@version=6
indicator("Deleted fill", overlay=true)
if barstate.islast
    upper = line.new(0, 2, 1, 3)
    lower = line.new(0, 0, 1, 1)
    linefill.new(upper, lower, color.blue)
    line.delete(lower)
plot(array.size(linefill.all))
"#);
    assert_eq!(result.drawings.lines.len(), 1);
    assert!(result.drawings.linefills.is_empty());
    assert_eq!(
        result.plots[0].values,
        vec![pinewasm_compiler::Value::Float(0.0); 8]
    );
}

#[test]
fn width_is_not_clipped_to_chart_library_or_byte_limits() {
    let result = run("//@version=6\nindicator(\"Wide\")\nplot(close, linewidth=300)\nhline(20, linewidth=300)\nhline(10, linewidth=7)\n");
    assert_eq!(result.plots.len(), 3);
    for (plot, width) in result.plots.iter().zip([300, 300, 7]) {
        assert_eq!(plot.style.line_width, width);
        assert!(plot.style.unsupported.is_empty());
    }
}

#[test]
fn fill_color_modes_come_from_expression_qualifiers_not_observed_colors() {
    let result = run(r#"//@version=6
indicator("Fill color modes")
a=plot(20,display=display.none)
b=plot(0,display=display.none)
c=input.color(color.blue)
select=input.bool(false)
fill(a,b,color.blue)
fill(a,b,c)
fill(a,b,select ? color.blue : color.red)
fill(a,b,bar_index>=0 ? color.blue : color.blue)
fill(a,b,top_value=20,bottom_value=0,top_color=color.blue,bottom_color=na)
fill(a,b,top_value=bar_index,bottom_value=0,top_color=color.blue,bottom_color=na)
"#);
    let modes: Vec<_> = result
        .fills
        .iter()
        .map(|fill| {
            serde_json::to_value(&fill.style)
                .unwrap()
                .get("fillColorMode")
                .cloned()
        })
        .collect();
    assert_eq!(
        modes,
        ["fixed", "fixed", "fixed", "series", "fixed", "series"]
            .map(|value| Some(serde_json::Value::String(value.into())))
    );
    // One identical palette entry and one identical runtime lane can still
    // be series-qualified. Neither proves a fixed fill color source.
    assert_eq!(result.fills[0].colors, result.fills[3].colors);
    assert_eq!(
        result.fills[0].style.color_palette,
        result.fills[3].style.color_palette
    );
    // Old serialized programs remain admissible without invented mode metadata.
    let mut old = serde_json::to_value(&result.fills[0].style).unwrap();
    old.as_object_mut().unwrap().remove("fillColorMode");
    let style: pinewasm_hir::DisplayStyle = serde_json::from_value(old).unwrap();
    assert_eq!(style.fill_color_mode, None);
    assert!(
        serde_json::from_value::<pinewasm_hir::FillColorMode>(serde_json::json!("sampled"))
            .is_err()
    );
}

#[test]
fn color_channels_and_ohlc_routing_are_owned_by_their_declared_output() {
    let result = run(r#"//@version=6
indicator("Owned colors")
plotcandle(open,high,low,close,color=color.red,wickcolor=color.blue,bordercolor=color.green)
plotarrow(close-open, colorup=color.blue,colordown=color.red)
plotshape(true,color=color.red,textcolor=color.blue)
bgcolor(color.new(color.blue,80))
a=plot(20)
b=plot(0)
fill(a,b,20,0,color.blue,color.red)
"#);
    let program = result.compile.as_ref().unwrap().program.as_ref().unwrap();
    assert!(pinewasm_csil::validate(program).is_ok());
    use pinewasm_hir::PlotColorRole;
    let mut malformed = program.clone();
    malformed.plots[0].color_roles.push(PlotColorRole::Text);
    assert!(pinewasm_csil::validate(&malformed).is_err());
    let mut malformed = program.clone();
    malformed.plots[1].style.force_overlay = true;
    assert!(pinewasm_csil::validate(&malformed).is_err());
    let mut malformed = program.clone();
    malformed.plots[1].style.ohlc_field = Some(pinewasm_hir::OhlcField::Open);
    assert!(pinewasm_csil::validate(&malformed).is_err());
    let mut malformed = program.clone();
    malformed.fills[0].color_roles.swap(0, 1);
    assert!(pinewasm_csil::validate(&malformed).is_err());
    let mut malformed = program.clone();
    malformed.visuals[0].style.source_order = program.plots[0].style.source_order;
    assert!(pinewasm_csil::validate(&malformed).is_err());
    let mut malformed = program.clone();
    malformed.plots[0].style.fill_color_mode = Some(pinewasm_hir::FillColorMode::Fixed);
    assert!(pinewasm_csil::validate(&malformed)
        .unwrap_err()
        .iter()
        .any(|error| error.to_string().contains("fill color mode belongs only")));
}

#[test]
fn plot_color_modes_preserve_qualifiers_through_runtime_and_serialization() {
    use pinewasm_hir::PlotColorMode::{Fixed, Series};
    let result = run(r#"//@version=6
indicator("Plot color modes")
c=input.color(color.blue)
select=input.bool(false)
plot(close,color=color.blue)
plot(close,color=c)
plot(close,color=select ? color.blue : color.red)
plot(close,color=timeframe.isintraday ? color.blue : color.blue)
plot(close,color=bar_index>=0 ? color.blue : color.blue)
plot(close,color=na)
hline(10,color=color.blue)
plotshape(true,color=color.blue)
a=plot(20)
b=plot(0)
fill(a,b,color.blue)
"#);
    let modes: Vec<_> = result
        .plots
        .iter()
        .map(|plot| plot.style.plot_color_mode)
        .collect();
    assert_eq!(
        modes,
        vec![
            Some(Fixed),
            Some(Fixed),
            Some(Fixed),
            Some(Fixed),
            Some(Series),
            Some(Fixed),
            None,
            None,
            Some(Fixed),
            Some(Fixed)
        ]
    );
    assert_eq!(result.plots[0].colors, result.plots[4].colors);
    assert_eq!(
        result.plots[0].style.color_palette,
        result.plots[4].style.color_palette
    );
    let program = result.compile.as_ref().unwrap().program.as_ref().unwrap();
    let compiled_series_style = serde_json::to_value(&program.plots[4].style).unwrap();
    let runtime_series_style = serde_json::to_value(&result.plots[4].style).unwrap();
    assert_eq!(compiled_series_style["plotColorMode"], "series");
    assert_eq!(runtime_series_style["plotColorMode"], "series");
    assert!(
        runtime_series_style.get("runtimeScalars").is_none(),
        "runtime style snapshot omits compiler-only runtimeScalars: {runtime_series_style}"
    );
    for (runtime, declared) in result.plots.iter().zip(&program.plots) {
        assert_eq!(
            runtime.style.plot_color_mode,
            declared.style.plot_color_mode
        );
    }
    assert_eq!(result.fills[0].style.plot_color_mode, None);
    let mut old = serde_json::to_value(&result.plots[0].style).unwrap();
    old.as_object_mut().unwrap().remove("plotColorMode");
    let old_style: pinewasm_hir::DisplayStyle = serde_json::from_value(old.clone()).unwrap();
    assert_eq!(old_style.plot_color_mode, None);
    assert_eq!(serde_json::to_value(old_style).unwrap(), old);
    assert!(
        serde_json::from_value::<pinewasm_hir::PlotColorMode>(serde_json::json!("sampled"))
            .is_err()
    );
    for owner in ["fill", "hline", "shape"] {
        let mut malformed = program.clone();
        let style = match owner {
            "fill" => &mut malformed.fills[0].style,
            "hline" => &mut malformed.plots[6].style,
            _ => &mut malformed.plots[7].style,
        };
        style.plot_color_mode = Some(Fixed);
        assert!(pinewasm_csil::validate(&malformed)
            .unwrap_err()
            .iter()
            .any(|error| error.to_string().contains("plot color mode belongs only")));
    }
}

#[test]
fn declaration_input_values_are_rejected_instead_of_silently_defaulted() {
    let source =
        "//@version=6\nv = input.bool(false)\nindicator(\"Invalid\", behind_chart=v)\nplot(close)";
    let result = pinewasm_compiler::compile(source, CompileOptions::default());
    assert!(!result.success);
    // `behind_chart` is `const bool`; an input bool is a qualifier error.
    assert!(result.diagnostics.iter().any(|d| {
        d.code.as_deref() == Some("E0206")
            && d.message
                == "`indicator` argument `behind_chart` must be const or weaker, found input"
    }));
}

#[test]
fn serialized_style_bindings_cannot_raise_the_fields_qualifier_ceiling() {
    use pinewasm_hir::Qualifier;
    let result = run(
        "//@version=6\nindicator(\"Qualifier admission\")\nn=input.int(2)\nplot(close, offset=n)",
    );
    let mut program = result.compile.unwrap().program.unwrap();
    program.plots[0].style.bound_scalars[0].field = "force_overlay".into();
    assert!(pinewasm_csil::validate(&program)
        .unwrap_err()
        .iter()
        .any(|e| e.to_string().contains("constant rendering binding")));
    program.plots[0].style.bound_scalars[0].field = "linewidth".into();
    assert!(pinewasm_csil::validate(&program).is_ok());

    let result = run("//@version=6\nindicator(\"Simple offset\")\nplot(close, offset=timeframe.isdaily ? -2 : 1)");
    let mut program = result.compile.unwrap().program.unwrap();
    assert_eq!(
        program.plots[0].style.runtime_scalars[0].qualifier,
        Qualifier::Simple
    );
    program.plots[0].style.runtime_scalars[0].field = "linewidth".into();
    assert!(pinewasm_csil::validate(&program)
        .unwrap_err()
        .iter()
        .any(|e| e
            .to_string()
            .contains("invalid or duplicate runtime rendering binding")));
}

#[test]
fn omitted_gradient_parameters_emit_explicit_na_roles() {
    use pinewasm_hir::PlotColorRole;
    let result = run("//@version=6\nindicator(\"Gradient defaults\")\np=plot(close)\nq=plot(open)\nfill(p, q, top_value=1)");
    let channels = &result.fills[0].colors;
    assert_eq!(
        channels.iter().map(|c| c.role).collect::<Vec<_>>(),
        pinewasm_hir::FILL_COLOR_ROLES[1..]
    );
    assert_eq!(channels[0].role, PlotColorRole::TopValue);
    assert!(channels[0]
        .values
        .iter()
        .all(|value| matches!(value, pinewasm_compiler::Value::Float(1.0))));
    assert!(channels[1..].iter().all(|channel| channel
        .values
        .iter()
        .all(|value| matches!(value, pinewasm_compiler::Value::Na))));
}

#[test]
fn unknown_contract_versions_and_binding_targets_fail_before_vm_execution() {
    let result = run(
        "//@version=6\nindicator(\"Contract admission\")\nw=input.int(7)\nplot(close, offset=w)",
    );
    let mut program = result.compile.unwrap().program.unwrap();
    assert!(pinewasm_csil::validate(&program).is_ok());
    program.metadata.rendering_contract_version = 99;
    assert!(pinewasm_csil::validate(&program)
        .unwrap_err()
        .iter()
        .any(|e| e.to_string().contains("rendering contract version 99")));
    program.metadata.rendering_contract_version = 1;
    program.plots[0].style.bound_scalars[0].field = "lineWidth".into();
    assert!(pinewasm_csil::validate(&program)
        .unwrap_err()
        .iter()
        .any(|e| e
            .to_string()
            .contains("invalid or duplicate rendering binding")));
    program.plots[0].style.bound_scalars[0].field = "offset".into();
    let duplicate = program.plots[0].style.bound_scalars[0].clone();
    program.plots[0].style.bound_scalars.push(duplicate);
    assert!(pinewasm_csil::validate(&program).is_err());
}

#[test]
fn clearing_a_merged_table_erases_attributes_without_losing_region_ownership() {
    let result = run(r#"//@version=6
indicator("Persistent merge")
var t = table.new(position.top_right, 2, 2)
if barstate.isfirst
    table.cell(t, 0, 0, "Before")
    table.merge_cells(t, 0, 0, 1, 1)
if barstate.islast
    table.clear(t, 0, 0, 1, 1)
    table.cell(t, 0, 0, "After")
plot(close)
"#);
    let cells = &result.drawings.tables[0].cells;
    assert_eq!(cells.len(), 4);
    assert!(cells.iter().all(|c| c.merge == Some([0, 0, 1, 1])));
    let owner = cells.iter().find(|c| c.column == 0 && c.row == 0).unwrap();
    assert!(
        matches!(&owner.attrs["text"], pinewasm_compiler::Value::String(text) if text == "After")
    );
}
