//! Atomic wire admission and internal raw preservation; the expectations for
//! the OHLC body contract live in a separate test file.
use pinewasm_compiler::{compile, run_program, CompileOptions, InputValue, MarketData, RunOptions};
use serde_json::{json, Value};

fn program(source: &str) -> pinewasm_compiler::Program {
    let result = compile(source, CompileOptions::default());
    assert!(result.success, "{:?}", result.diagnostics);
    result.program.unwrap()
}
fn data() -> MarketData {
    MarketData {
        open: vec![10.0; 3],
        high: vec![12.0; 3],
        low: vec![9.0; 3],
        close: vec![11.0, 10.0, 12.0],
        volume: vec![0.0; 3],
        ..MarketData::default()
    }
}
fn candle() -> pinewasm_compiler::Program {
    program("//@version=6\nindicator(\"OHLC wire\")\nplotcandle(open,high,low,close,color=#336699)")
}
fn metadata(wire: &Value) -> Value {
    wire["plots"][0]["style"]["ohlcBodyStyle"].clone()
}

#[test]
fn manifest_and_public_facade_describe_complete_atomic_models() {
    let contract = pinewasm_compiler::rendering_contract();
    assert_eq!(
        contract["dtoFields"]["OhlcBodyColor"],
        json!(["r", "g", "b", "t"])
    );
    assert_eq!(
        contract["dtoFields"]["OhlcBodyStyle"],
        json!(["staticColorPresent", "staticColor", "runtimeColorer"])
    );
    assert!(contract["optionalStyleFields"]
        .as_array()
        .unwrap()
        .contains(&json!("ohlcBodyStyle")));
    assert!(contract["styleDefaults"].get("ohlcBodyStyle").is_none());
    let record = pinewasm_compiler::OhlcBodyStyle {
        static_color_present: false,
        static_color: None,
        runtime_colorer: true,
    };
    assert_eq!(
        serde_json::to_value(record).unwrap(),
        json!({"staticColorPresent":false,"staticColor":null,"runtimeColorer":true})
    );
}
#[test]
fn nested_record_requires_all_fields_and_denies_unknown_or_wrong_types() {
    let p = candle();
    let wire = serde_json::to_value(&p).unwrap();
    let good = metadata(&wire);
    for field in ["staticColorPresent", "staticColor", "runtimeColorer"] {
        let mut invalid = good.clone();
        invalid.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<pinewasm_compiler::OhlcBodyStyle>(invalid).is_err(),
            "{field}"
        );
    }
    for invalid in [
        json!({"staticColorPresent":"true","staticColor":null,"runtimeColorer":false}),
        json!({"staticColorPresent":true,"staticColor":null,"runtimeColorer":0}),
        json!({"staticColorPresent":true,"staticColor":null,"runtimeColorer":false,"extra":false}),
        json!({"staticColorPresent":false,"staticColor":{"r":1,"g":2,"b":3,"t":4},"runtimeColorer":true}),
    ] {
        assert!(serde_json::from_value::<pinewasm_compiler::OhlcBodyStyle>(invalid).is_err());
    }
    let mut null = wire;
    null["plots"][0]["style"]["ohlcBodyStyle"] = Value::Null;
    // The optional extension itself retains ordinary Option compatibility;
    // strictness belongs to a present object, not an invented required marker.
    assert!(serde_json::from_value::<pinewasm_compiler::Program>(null).is_ok());
}
#[test]
fn color_channels_are_bytes_and_transparency_is_finite_bounded_fractional() {
    for t in [0.0, 40.125, 100.0] {
        let color: pinewasm_compiler::OhlcBodyColor =
            serde_json::from_value(json!({"r":0,"g":255,"b":42,"t":t})).unwrap();
        assert_eq!(color.t, t);
    }
    for invalid in [
        json!({"r":-1,"g":0,"b":0,"t":0}),
        json!({"r":256,"g":0,"b":0,"t":0}),
        json!({"r":0.5,"g":0,"b":0,"t":0}),
        json!({"r":0,"g":0,"b":0,"t":-0.1}),
        json!({"r":0,"g":0,"b":0,"t":100.1}),
        json!({"r":0,"g":0,"b":0,"t":null}),
        json!({"r":0,"g":0,"b":0}),
        json!({"r":0,"g":0,"b":0,"t":0,"extra":0}),
    ] {
        assert!(serde_json::from_value::<pinewasm_compiler::OhlcBodyColor>(invalid).is_err());
    }
    for t in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 101.0] {
        let mut p = candle();
        for plot in &mut p.plots {
            plot.style
                .ohlc_body_style
                .as_mut()
                .unwrap()
                .static_color
                .as_mut()
                .unwrap()
                .t = t;
        }
        assert!(pinewasm_csil::validate(&p).is_err());
    }
}
#[test]
fn omitted_extension_preserves_legacy_group_and_raw_execution() {
    let mut p = candle();
    let before = run_program(&p, &data(), &RunOptions::default());
    for plot in &mut p.plots {
        plot.style.ohlc_body_style = None;
    }
    pinewasm_csil::validate(&p).unwrap();
    let after = run_program(&p, &data(), &RunOptions::default());
    for (a, b) in before.plots.iter().zip(&after.plots) {
        assert_eq!(a.values, b.values);
        assert_eq!(a.colors, b.colors);
    }
    let wire = serde_json::to_value(&p).unwrap();
    assert!(wire["plots"][0]["style"].get("ohlcBodyStyle").is_none());
}
#[test]
fn group_requires_identical_body_records_and_all_four_fields() {
    let mut p = candle();
    p.plots[2].style.ohlc_body_style = None;
    assert!(pinewasm_csil::validate(&p).is_err());
    let mut p = candle();
    p.plots[2]
        .style
        .ohlc_body_style
        .as_mut()
        .unwrap()
        .runtime_colorer = true;
    assert!(pinewasm_csil::validate(&p).is_err());
    let mut p = candle();
    p.plots.pop();
    assert!(pinewasm_csil::validate(&p).is_err());
}
#[test]
fn extension_is_owned_only_by_grouped_candle_and_bar_lanes() {
    let record = candle().plots[0].style.ohlc_body_style.clone();
    for source in [
        "indicator(\"plain\")\nplot(close)",
        "indicator(\"marker\")\nplotchar(close)",
        "indicator(\"arrow\")\nplotarrow(close)",
    ] {
        let mut p = program(source);
        assert!(p.plots.iter().all(|p| p.style.ohlc_body_style.is_none()));
        p.plots[0].style.ohlc_body_style = record.clone();
        assert!(pinewasm_csil::validate(&p).is_err());
    }
    let mut p = candle();
    for plot in &mut p.plots {
        plot.style.group = None;
    }
    assert!(pinewasm_csil::validate(&p).is_err());
    let mut p = candle();
    for plot in &mut p.plots {
        plot.style.ohlc_field = None;
    }
    assert!(pinewasm_csil::validate(&p).is_err());
    let mut p = program("indicator(\"visual\")\nbgcolor(#336699)");
    p.visuals[0].style.ohlc_body_style = record.clone();
    assert!(pinewasm_csil::validate(&p).is_err());
    let mut p = program("indicator(\"fill\")\np=plot(close)\nq=plot(open)\nfill(p,q,#336699)");
    p.fills[0].style.ohlc_body_style = record;
    assert!(pinewasm_csil::validate(&p).is_err());
}
#[test]
fn enabled_colorer_requires_declared_role_even_with_empty_runtime_vectors() {
    let p = program(
        "indicator(\"dynamic\")\nplotbar(open,high,low,close,color=close>open?#336699:#669933)",
    );
    assert!(p
        .plots
        .iter()
        .all(|p| p.style.ohlc_body_style.as_ref().unwrap().runtime_colorer));
    let run = run_program(&p, &MarketData::default(), &RunOptions::default());
    assert!(run.runtime_errors.is_empty());
    for plot in run.plots {
        assert!(plot
            .colors
            .iter()
            .any(|c| c.role == pinewasm_compiler::PlotColorRole::Color && c.values.is_empty()));
    }
    let mut missing = p;
    for plot in &mut missing.plots {
        plot.color_roles
            .retain(|r| *r != pinewasm_compiler::PlotColorRole::Color);
    }
    assert!(pinewasm_csil::validate(&missing).is_err());
}
#[test]
fn static_false_binding_retains_independent_raw_color_lane() {
    let p = candle();
    assert!(
        !p.plots[0]
            .style
            .ohlc_body_style
            .as_ref()
            .unwrap()
            .runtime_colorer
    );
    let run = run_program(&p, &data(), &RunOptions::default());
    assert!(run.plots.iter().all(|p| p
        .colors
        .iter()
        .any(|c| c.role == pinewasm_compiler::PlotColorRole::Color)));
}
#[test]
fn compiled_bar_input_property_follows_explicit_runtime_override() {
    let p=program("indicator(\"input defaults\")\nc=input.color(#336699)\nplotbar(open,high,low,close,color=c)");
    let before = run_program(&p, &data(), &RunOptions::default());
    let mut options = RunOptions::default();
    options.inputs.insert(
        p.inputs[0].id,
        InputValue::Color {
            r: 102,
            g: 153,
            b: 51,
            t: 20.0,
        },
    );
    let after = run_program(&p, &data(), &options);
    assert_ne!(before.plots[0].colors, after.plots[0].colors);
    for (a, b) in before.plots.iter().zip(&after.plots) {
        assert_ne!(a.style.ohlc_body_style, b.style.ohlc_body_style);
        assert_eq!(
            b.style.ohlc_body_style.as_ref().unwrap().static_color,
            Some(pinewasm_compiler::OhlcBodyColor {
                r: 102,
                g: 153,
                b: 51,
                t: 20.0
            })
        );
        assert_eq!(a.style.bound_scalars, b.style.bound_scalars);
    }
}
#[test]
fn unresolved_constant_fold_omits_whole_record_with_diagnostic() {
    let p=program("indicator(\"unsupported\")\nplotbar(open,high,low,close,color=color.new(#336699,math.tan(-70)))");
    for plot in p.plots {
        assert!(plot.style.ohlc_body_style.is_none());
        assert!(plot
            .style
            .unsupported
            .iter()
            .any(|o| o.reason.contains("cannot be proven")));
    }
}

#[test]
fn new_records_keep_group_admission_under_legacy_global_version() {
    let mut p = candle();
    p.metadata.rendering_contract_version = 0;
    pinewasm_csil::validate(&p).unwrap();
    p.plots[1].style.ohlc_body_style = None;
    let errors = pinewasm_csil::validate(&p).unwrap_err();
    assert!(errors
        .iter()
        .any(|error| format!("{error:?}").contains("inconsistent routing or styles")));
    let mut p = candle();
    p.metadata.rendering_contract_version = 0;
    p.plots.pop();
    let errors = pinewasm_csil::validate(&p).unwrap_err();
    assert!(errors
        .iter()
        .any(|error| format!("{error:?}").contains("OHLC group")));
}
#[test]
fn extension_absent_legacy_does_not_gain_new_body_group_requirements() {
    let mut p = candle();
    p.metadata.rendering_contract_version = 0;
    for plot in &mut p.plots {
        plot.style.ohlc_body_style = None;
        plot.style.group = None;
        plot.style.ohlc_field = None;
    }
    pinewasm_csil::validate(&p).unwrap();
}

#[test]
fn legacy_visual_cannot_forge_plot_lane_ownership_for_body_extension() {
    let mut p = program("indicator(\"legacy visual\")\nbgcolor(#336699)");
    p.metadata.rendering_contract_version = 0;
    pinewasm_csil::validate(&p).unwrap();
    p.visuals[0].style = candle().plots[0].style.clone();
    let errors = pinewasm_csil::validate(&p).unwrap_err();
    assert!(errors
        .iter()
        .any(|error| format!("{error:?}").contains("OHLC body style belongs only")));
    // This extension does not broaden container/family validation at contract
    // version 0. Its absence preserves the shape admitted there.
    p.visuals[0].style.ohlc_body_style = None;
    pinewasm_csil::validate(&p).unwrap();
}
