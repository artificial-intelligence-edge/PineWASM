//! Rust side of the bounded display-style token table.
//! This checks Serde's enum spelling for the current crate and
//! exercises a serialized, validated CSIL program. It is not a proof
//! that all Rust serialization matches the byte decoder.

use pinewasm_compiler::{
    DisplayKind, MarkerLocation, MarkerShape, MarkerSize, OhlcField, PlotDisplay, PlotLineStyle,
    PlotStyleKind,
};
use pinewasm_csil::lower_mir_with_pass_record;
use serde::Serialize;

fn assert_enum_tokens<T: Serialize + Copy>(variants: &[(T, &str)]) {
    for &(variant, expected) in variants {
        assert_eq!(serde_json::to_value(variant).unwrap(), expected);
    }
}

#[test]
fn serde_enum_vocabulary_matches_token_tables() {
    use DisplayKind::*;
    assert_enum_tokens(&[
        (Plot, "plot"),
        (PlotShape, "plot_shape"),
        (PlotChar, "plot_char"),
        (PlotArrow, "plot_arrow"),
        (PlotCandle, "plot_candle"),
        (PlotBar, "plot_bar"),
        (Hline, "hline"),
        (Bgcolor, "bgcolor"),
        (Barcolor, "barcolor"),
    ]);
    use PlotStyleKind::*;
    assert_enum_tokens(&[
        (Line, "line"),
        (LineBreak, "line_break"),
        (StepLine, "step_line"),
        (StepLineBreak, "step_line_break"),
        (StepLineDiamond, "step_line_diamond"),
        (Histogram, "histogram"),
        (PlotStyleKind::Cross, "cross"),
        (Area, "area"),
        (AreaBreak, "area_break"),
        (Columns, "columns"),
        (Circles, "circles"),
    ]);
    use PlotLineStyle::*;
    assert_enum_tokens(&[(Solid, "solid"), (Dotted, "dotted"), (Dashed, "dashed")]);
    use PlotDisplay::*;
    assert_enum_tokens(&[
        (All, "all"),
        (None, "none"),
        (Pane, "pane"),
        (DataWindow, "data_window"),
        (PriceScale, "price_scale"),
        (StatusLine, "status_line"),
    ]);
    use OhlcField::*;
    assert_enum_tokens(&[
        (Open, "open"),
        (High, "high"),
        (Low, "low"),
        (Close, "close"),
    ]);
    use MarkerShape::*;
    assert_enum_tokens(&[
        (XCross, "x_cross"),
        (MarkerShape::Cross, "cross"),
        (Circle, "circle"),
        (TriangleUp, "triangle_up"),
        (TriangleDown, "triangle_down"),
        (Flag, "flag"),
        (ArrowUp, "arrow_up"),
        (ArrowDown, "arrow_down"),
        (Square, "square"),
        (Diamond, "diamond"),
        (LabelUp, "label_up"),
        (LabelDown, "label_down"),
    ]);
    use MarkerLocation::*;
    assert_enum_tokens(&[
        (AboveBar, "above_bar"),
        (BelowBar, "below_bar"),
        (Top, "top"),
        (Bottom, "bottom"),
        (Absolute, "absolute"),
    ]);
    use MarkerSize::*;
    assert_enum_tokens(&[
        (Auto, "auto"),
        (Tiny, "tiny"),
        (Small, "small"),
        (Normal, "normal"),
        (Large, "large"),
        (Huge, "huge"),
    ]);
}

#[test]
fn style_round_trips_and_invalid_tokens_fail_closed() {
    let source = "//@version=6\nindicator(\"Style snapshot\")\nx = close\nplot(x)\n";
    let parsed = pinewasm_parse::parse(source);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let analyzed = pinewasm_semantics::analyze(&parsed.program);
    assert!(
        analyzed.diagnostics.is_empty(),
        "{:?}",
        analyzed.diagnostics
    );
    let mir = pinewasm_mir::lower(&analyzed.hir);
    let captured = lower_mir_with_pass_record(&mir).unwrap();
    let (program, _) = captured.coalescing_programs();
    pinewasm_csil::validate(&program).unwrap();
    let ordinary = program.to_json();
    let parsed_back = pinewasm_csil::CsilProgram::from_json(&ordinary).unwrap();
    pinewasm_csil::validate(&parsed_back).unwrap();
    assert_eq!(parsed_back.to_json(), ordinary);
    assert_eq!(parsed_back.plots[0].style.kind, DisplayKind::Plot);
    assert_eq!(parsed_back.plots[0].style.plot_style, PlotStyleKind::Line);

    let mut wire: serde_json::Value = serde_json::from_str(&ordinary).unwrap();
    for field in [
        "kind",
        "plotStyle",
        "lineStyle",
        "display",
        "ohlcField",
        "markerShape",
        "markerLocation",
        "markerSize",
    ] {
        let original = wire["plots"][0]["style"][field].clone();
        wire["plots"][0]["style"][field] = serde_json::json!("not_a_variant");
        assert!(
            pinewasm_csil::CsilProgram::from_json(&wire.to_string()).is_err(),
            "unknown {field} token deserialized"
        );
        wire["plots"][0]["style"][field] = serde_json::json!(7);
        assert!(
            pinewasm_csil::CsilProgram::from_json(&wire.to_string()).is_err(),
            "numeric {field} token deserialized"
        );
        wire["plots"][0]["style"][field] = original;
    }
    assert!(
        pinewasm_csil::CsilProgram::from_json("{\"plots\":[],\"plots\":[]}").is_err(),
        "malformed incomplete program was accepted"
    );
    let duplicate_kind = ordinary.replace(
        "\"kind\":\"plot\"",
        "\"kind\":\"plot\",\"kind\":\"barcolor\"",
    );
    assert_ne!(
        duplicate_kind, ordinary,
        "serialized program has no kind field"
    );
    assert!(
        pinewasm_csil::CsilProgram::from_json(&duplicate_kind).is_err(),
        "duplicate style kind field deserialized"
    );
}
