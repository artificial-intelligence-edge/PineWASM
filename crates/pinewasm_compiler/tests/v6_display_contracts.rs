//! Compile acceptance for plot titles, fills and shapes. These checks cover
//! argument overloads and integer parameters, not every default or rendering
//! behavior.

use pinewasm_compiler::{compile, CompileOptions};

#[test]
fn plot_titles_preserve_const_expressions() {
    let result = compile(
        "//@version=6\nindicator(\"Titles\")\nconst string NAME = \"Signal\"\n\
         plot(close, NAME + \" line\")\nplotshape(true, title=NAME + \" marker\")\n",
        CompileOptions::default(),
    );
    assert!(result.success, "{:?}", result.diagnostics);
    let program = result.program.unwrap();
    assert_eq!(program.plots[0].title.as_deref(), Some("Signal line"));
    assert_eq!(program.plots[1].title.as_deref(), Some("Signal marker"));
}

#[test]
fn fill_overloads_keep_distinct_handle_types_and_optional_parameter_order() {
    let result = compile(
        r#"//@version=6
indicator("Fill overloads")
p = plot(close)
q = plot(open)
h = hline(1)
j = hline(0)
fill(plot2=q, plot1=p, color=color.blue, show_last=7)
fill(hline2=j, hline1=h, color=color.red)
fill(p, q, color.blue, "Plot fill", false, 5, true, display.all)
fill(h, j, color.red, "Hline fill", false, true, display.all)
fill(plot1=p, plot2=q, top_value=1)
fill(plot(close), plot(open), color.blue)
"#,
        CompileOptions::default(),
    );
    assert!(result.success, "{:?}", result.diagnostics);
    let program = result.program.unwrap();
    assert_eq!(program.plots.len(), 6, "inline endpoints execute only once");
    assert_eq!(program.fills.len(), 6);
    assert_eq!(program.fills[0].style.show_last, Some(7));
    assert_eq!(program.fills[2].style.show_last, Some(5));
    assert!(!program.fills[2].style.editable);
    assert!(program.fills[2].style.fill_gaps);
    assert!(!program.fills[3].style.editable);
    assert!(program.fills[3].style.fill_gaps);

    for call in [
        "fill(p, h, color.blue)",
        "fill(h, p, color.blue)",
        "fill(h, p, 1, 0, color.blue, color.red)",
        "fill(p, h, 1, 0, color.blue, color.red)",
        "fill(plot1=h, plot2=j, top_value=1, bottom_value=0, top_color=color.blue, bottom_color=color.red)",
        "fill(hline1=p, hline2=q, top_value=1, bottom_value=0, top_color=color.blue, bottom_color=color.red)",
        "fill(hline1=h, hline2=j, show_last=5)",
    ] {
        let source = format!(
            "//@version=6\nindicator(\"Invalid fill\")\np=plot(close)\nq=plot(open)\n\
             h=hline(1)\nj=hline(0)\n{call}\n"
        );
        let result = compile(&source, CompileOptions::default());
        assert!(!result.success, "unexpectedly accepted {call}");
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| matches!(diagnostic.code.as_deref(), Some("E0205" | "E0228"))),
            "{call}: {:?}",
            result.diagnostics
        );
    }
}

#[test]
fn fill_gap_wire_default_is_false_and_explicit_true_roundtrips() {
    let default_style: pinewasm_hir::DisplayStyle = serde_json::from_str("{}").unwrap();
    assert!(!default_style.fill_gaps);
    let absent = serde_json::to_value(&default_style).unwrap();
    assert!(absent.get("fillGaps").is_none());
    let enabled = pinewasm_hir::DisplayStyle {
        fill_gaps: true,
        ..default_style
    };
    let wire = serde_json::to_value(&enabled).unwrap();
    assert_eq!(wire["fillGaps"], true);
    let decoded: pinewasm_hir::DisplayStyle = serde_json::from_value(wire).unwrap();
    assert!(decoded.fill_gaps);
}

#[test]
fn plotshape_rejects_unsupported_location_inbar() {
    let source = "//@version=6\nindicator(\"In-bar\", overlay=true)\nplotshape(close > open, style=shape.diamond, location=location.inbar)\n";
    let result = compile(source, CompileOptions::default());
    assert!(!result.success);
    assert!(!result.diagnostics.is_empty());
}
