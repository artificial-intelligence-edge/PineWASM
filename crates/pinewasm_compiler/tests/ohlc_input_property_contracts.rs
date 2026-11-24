//! Rendering regression tests for the VM.
use pinewasm_compiler::{compile, run_program, CompileOptions, InputValue, MarketData, RunOptions};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/ohlc_input_property_cases.json")).unwrap()
}
fn market() -> MarketData {
    MarketData {
        open: vec![10.0; 3],
        high: vec![12.0; 3],
        low: vec![9.0; 3],
        close: vec![11.0, 10.0, 12.0],
        volume: vec![0.0; 3],
        ..MarketData::default()
    }
}
fn program(source: &str) -> pinewasm_compiler::Program {
    let c = compile(source, CompileOptions::default());
    assert!(c.success, "{:?}", c.diagnostics);
    c.program.unwrap()
}
fn input_program() -> pinewasm_compiler::Program {
    program("//@version=6\nindicator(\"Property binding\")\nc=input.color(#336699)\nplotbar(open,high,low,close,color=c)")
}
fn bindings(style: &pinewasm_compiler::DisplayStyle) -> Vec<&pinewasm_hir::BoundStyleScalar> {
    style
        .bound_scalars
        .iter()
        .filter(|b| b.field == "ohlc_body_color")
        .collect()
}
fn check_case(index: usize) {
    let f = fixture();
    let row = &f["cases"][index];
    let source = row["source"].as_str().unwrap();
    assert_eq!(hash(source.as_bytes()), row["sourceSha256"]);

    let c = compile(
        source,
        CompileOptions {
            emit_csil: true,
            ..CompileOptions::default()
        },
    );
    assert!(c.success, "{}: {:?}", row["id"], c.diagnostics);
    let listing = hash(c.csil_text.as_ref().unwrap().as_bytes());
    let p = c.program.unwrap();
    assert_eq!(p.inputs.len(), 1);
    assert_eq!(
        p.inputs[0].id,
        row["expectedInputId"].as_u64().unwrap() as u32
    );
    let expected: pinewasm_compiler::OhlcBodyStyle =
        serde_json::from_value(row["expected"].clone()).unwrap();
    let selected = &row["structuredPropertyInput"];
    let value = InputValue::Color {
        r: selected["r"].as_u64().unwrap() as u8,
        g: selected["g"].as_u64().unwrap() as u8,
        b: selected["b"].as_u64().unwrap() as u8,
        t: selected["t"].as_f64().unwrap(),
    };
    let mut opts = RunOptions::default();
    opts.inputs.insert(p.inputs[0].id, value);
    let run = run_program(&p, &market(), &opts);
    assert!(
        run.runtime_errors.is_empty(),
        "{}: {:?}",
        row["id"],
        run.runtime_errors
    );
    let raw = run
        .plots
        .iter()
        .map(|p| (&p.values, &p.colors))
        .collect::<Vec<_>>();
    eprintln!(
        "OHLC_INPUT_PRESERVATION {}",
        json!({"id":row["id"],"sourceSha256":row["sourceSha256"],
        "expectedCsilSha256":listing,"rawSha256":hash(&serde_json::to_vec(&raw).unwrap())})
    );
    assert_eq!(run.plots.len(), 4);
    for lane in &run.plots {
        assert_eq!(
            lane.style.ohlc_body_style.as_ref(),
            Some(&expected),
            "{}",
            row["id"]
        );
    }
    for lane in &p.plots {
        let bound = bindings(&lane.style);
        if row["expectedBodyBinding"] == true {
            assert_eq!(bound.len(), 1);
            assert_eq!(
                bound[0].expression,
                pinewasm_hir::InputActiveExpr::Input {
                    input: p.inputs[0].id
                }
            );
        } else {
            assert!(bound.is_empty());
        }
    }
}
macro_rules! fixture_case {
    ($name:ident,$index:expr) => {
        #[test]
        fn $name() {
            check_case($index);
        }
    };
}
fixture_case!(ohlc_input_property_00, 0);
fixture_case!(ohlc_input_property_01, 1);
fixture_case!(ohlc_input_property_02, 2);
fixture_case!(ohlc_input_property_03, 3);
fixture_case!(ohlc_input_property_04, 4);
fixture_case!(ohlc_input_property_05, 5);
fixture_case!(ohlc_input_property_06, 6);
fixture_case!(ohlc_input_property_07, 7);

#[test]
fn binding_descriptor_and_exact_serialized_shape() {
    let c = pinewasm_compiler::rendering_contract();
    assert!(c["bindingFields"]
        .as_array()
        .unwrap()
        .contains(&json!("ohlc_body_color")));
    assert_eq!(c["bindingQualifiers"]["ohlc_body_color"], "Input");
    let p = input_program();
    pinewasm_csil::validate(&p).unwrap();
    for plot in p.plots {
        let bound = bindings(&plot.style);
        assert_eq!(bound.len(), 1);
        assert_eq!(
            serde_json::to_value(bound[0]).unwrap(),
            json!({"field":"ohlc_body_color","expression":{"kind":"input","input":0}})
        );
    }
}
#[test]
fn equal_default_colors_keep_distinct_actual_input_ids() {
    let p=program("indicator(\"Two IDs\")\na=input.color(#336699)\nb=input.color(#336699)\nplotbar(open,high,low,close,color=a)\nplotbar(open,high,low,close,color=b)");
    assert_eq!(p.inputs.len(), 2);
    assert_eq!(p.plots.len(), 8);
    let mut options = RunOptions::default();
    options.inputs.insert(
        p.inputs[1].id,
        InputValue::Color {
            r: 18,
            g: 52,
            b: 86,
            t: 58.0,
        },
    );
    let run = run_program(&p, &market(), &options);
    assert!(run.runtime_errors.is_empty());
    for i in 0..8 {
        let id = p.inputs[usize::from(i >= 4)].id;
        assert_eq!(
            bindings(&p.plots[i].style)[0].expression,
            pinewasm_hir::InputActiveExpr::Input { input: id }
        );
        let expected = if i < 4 {
            pinewasm_compiler::OhlcBodyColor {
                r: 51,
                g: 102,
                b: 153,
                t: 0.0,
            }
        } else {
            pinewasm_compiler::OhlcBodyColor {
                r: 18,
                g: 52,
                b: 86,
                t: 58.0,
            }
        };
        assert_eq!(
            run.plots[i]
                .style
                .ohlc_body_style
                .as_ref()
                .unwrap()
                .static_color,
            Some(expected)
        );
    }
}
#[test]
fn explicit_override_equal_to_raw_default_keeps_override_provenance() {
    let p=program("indicator(\"Default provenance\")\nc=input.color(color.new(#336699,70.5))\nplotbar(open,high,low,close,color=c)");
    let InputValue::Color { t, .. } = &p.inputs[0].defval else {
        panic!("color input")
    };
    assert_eq!(*t, 70.5);
    let normal = run_program(&p, &market(), &RunOptions::default());
    let mut options = RunOptions::default();
    options
        .inputs
        .insert(p.inputs[0].id, p.inputs[0].defval.clone());
    let explicit = run_program(&p, &market(), &options);
    assert!(normal.runtime_errors.is_empty() && explicit.runtime_errors.is_empty());
    for (a, b) in normal.plots.iter().zip(explicit.plots) {
        assert_eq!(
            a.style
                .ohlc_body_style
                .as_ref()
                .unwrap()
                .static_color
                .as_ref()
                .unwrap()
                .t,
            70.0
        );
        assert_eq!(
            b.style
                .ohlc_body_style
                .as_ref()
                .unwrap()
                .static_color
                .as_ref()
                .unwrap()
                .t,
            70.5
        );
        // Equal selected raw values must not change engine colors or numbers.
        assert_eq!(a.values, b.values);
        assert_eq!(a.colors, b.colors);
    }
}
#[test]
fn invalid_overrides_never_become_static_properties() {
    let p = input_program();
    let base = run_program(&p, &market(), &RunOptions::default());
    for value in [
        InputValue::String("#123456".into()),
        InputValue::Color {
            r: 18,
            g: 52,
            b: 86,
            t: -1.0,
        },
        InputValue::Color {
            r: 18,
            g: 52,
            b: 86,
            t: 101.0,
        },
        InputValue::Color {
            r: 18,
            g: 52,
            b: 86,
            t: f64::NAN,
        },
    ] {
        let mut options = RunOptions::default();
        options.inputs.insert(p.inputs[0].id, value);
        let result = run_program(&p, &market(), &options);
        assert!(result.runtime_errors.is_empty());
        assert_eq!(result.plots.len(), 4);
        for (a, b) in base.plots.iter().zip(result.plots) {
            assert_eq!(a.style.ohlc_body_style, b.style.ohlc_body_style);
        }
    }
}
#[test]
fn explicit_override_resolves_with_zero_bars_and_keeps_empty_lanes() {
    let p = input_program();
    let mut options = RunOptions::default();
    options.inputs.insert(
        p.inputs[0].id,
        InputValue::Color {
            r: 18,
            g: 52,
            b: 86,
            t: 40.125,
        },
    );
    let result = run_program(&p, &MarketData::default(), &options);
    assert!(result.runtime_errors.is_empty());
    for plot in result.plots {
        assert!(plot.values.is_empty());
        assert!(plot.colors.iter().all(|c| c.values.is_empty()));
        assert_eq!(
            plot.style
                .ohlc_body_style
                .as_ref()
                .unwrap()
                .static_color
                .as_ref()
                .unwrap()
                .t,
            40.125
        );
    }
}
#[test]
fn arbitrary_expression_wrong_input_or_runtime_target_is_rejected() {
    let p = input_program();
    assert_eq!(bindings(&p.plots[0].style).len(), 1);
    for expression in [
        pinewasm_hir::InputActiveExpr::Value {
            value: InputValue::Color {
                r: 51,
                g: 102,
                b: 153,
                t: 0.0,
            },
        },
        pinewasm_hir::InputActiveExpr::Input { input: 99 },
    ] {
        let mut bad = p.clone();
        for plot in &mut bad.plots {
            plot.style
                .bound_scalars
                .iter_mut()
                .find(|b| b.field == "ohlc_body_color")
                .unwrap()
                .expression = expression.clone();
        }
        assert!(pinewasm_csil::validate(&bad).is_err());
    }
    let mut bad = p.clone();
    bad.inputs[0].kind = pinewasm_hir::InputKind::Bool;
    bad.inputs[0].defval = InputValue::Bool(false);
    assert!(pinewasm_csil::validate(&bad).is_err());
    let mut bad = p;
    for plot in &mut bad.plots {
        plot.style
            .runtime_scalars
            .push(pinewasm_hir::RuntimeStyleScalar {
                field: "ohlc_body_color".into(),
                slot: pinewasm_hir::SeriesSlot(0),
                qualifier: pinewasm_hir::Qualifier::Input,
            });
    }
    assert!(pinewasm_csil::validate(&bad).is_err());
}
#[test]
fn property_target_requires_body_record_and_complete_bar_group() {
    let p = input_program();
    for mutation in 0..7 {
        let mut bad = p.clone();
        match mutation {
            0 => {
                for plot in &mut bad.plots {
                    plot.style.kind = pinewasm_hir::DisplayKind::PlotCandle;
                }
            }
            1 => {
                for plot in &mut bad.plots {
                    plot.style.ohlc_body_style = None;
                }
            }
            2 => {
                for plot in &mut bad.plots {
                    plot.style.group = None;
                }
            }
            3 => {
                for plot in &mut bad.plots {
                    plot.style
                        .ohlc_body_style
                        .as_mut()
                        .unwrap()
                        .static_color_present = false;
                    plot.style.ohlc_body_style.as_mut().unwrap().static_color = None;
                }
            }
            4 => {
                for plot in &mut bad.plots {
                    plot.style.ohlc_body_style.as_mut().unwrap().runtime_colorer = false;
                }
            }
            5 => {
                bad.plots[0]
                    .style
                    .bound_scalars
                    .retain(|b| b.field != "ohlc_body_color");
            }
            _ => {
                let extra = bindings(&bad.plots[0].style)[0].clone();
                bad.plots[0].style.bound_scalars.push(extra);
            }
        }
        assert!(pinewasm_csil::validate(&bad).is_err(), "mutation{mutation}");
    }
}
#[test]
fn binding_ownership_is_strict_even_for_legacy_global_version() {
    let mut p = input_program();
    p.metadata.rendering_contract_version = 0;
    pinewasm_csil::validate(&p).unwrap();
    let body = p.plots[0].style.clone();
    let mut visual = program("indicator(\"Legacy visual\")\nbgcolor(#336699)");
    visual.metadata.rendering_contract_version = 0;
    visual.visuals[0].style = body;
    assert!(pinewasm_csil::validate(&visual).is_err());
}
