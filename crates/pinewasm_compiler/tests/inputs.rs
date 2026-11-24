//! End-to-end tests for the `input.*` interface.
//!
//! Each input kind round-trips into the program metadata; overrides change run
//! results at run time; constraint violations are rejected at compile time
//! (E0230); and the `input` qualifier interacts correctly with the plot-title
//! const-literal rule (an input string is not a const literal, proving the
//! lattice is correct).

use pinewasm_compiler::{
    compile, compile_and_run, CompileOptions, InputActiveDirect, InputDisplay, InputKind,
    InputValue, InputWidget, MarketData, RunOptions, Value,
};
use std::collections::HashMap;

fn data() -> MarketData {
    MarketData {
        open: vec![9.0, 11.0, 12.0, 10.5],
        high: vec![10.5, 12.5, 12.0, 11.0],
        low: vec![8.5, 10.5, 10.0, 9.5],
        close: vec![10.0, 12.0, 11.0, 10.75],
        volume: vec![1000.0, 1500.0, 800.0, 1200.0],
        ..MarketData::default()
    }
}

/// Compile and require success, returning the validated program.
fn compile_ok(source: &str) -> pinewasm_csil::CsilProgram {
    let result = compile(source, CompileOptions::default());
    assert!(
        result.success,
        "expected compile success, got diagnostics: {:?}",
        result.diagnostics
    );
    result.program.expect("program present on success")
}

/// Compile and require a specific error code somewhere in the diagnostics.
fn expect_error_code(source: &str, code: &str) {
    let result = compile(source, CompileOptions::default());
    assert!(
        !result.success,
        "expected compile failure with {code}, but it succeeded"
    );
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some(code)),
        "expected diagnostic {code}, got: {:?}",
        result
            .diagnostics
            .iter()
            .map(|d| (&d.code, &d.message))
            .collect::<Vec<_>>()
    );
}

fn run_with(source: &str, options: &RunOptions) -> Vec<Value> {
    let run = compile_and_run(source, &data(), CompileOptions::default(), options);
    assert!(
        run.runtime_errors.is_empty(),
        "unexpected runtime errors: {:?}",
        run.runtime_errors
    );
    assert!(!run.plots.is_empty(), "expected at least one plot");
    run.plots[0].values.clone()
}

// ---------------------------------------------------------------------------
// Metadata round-trip: each kind records an InputDecl with the right shape.
// ---------------------------------------------------------------------------

#[test]
fn int_input_round_trips_metadata() {
    let program =
        compile_ok("indicator(\"t\")\nlen = input.int(14, \"Length\", 1, 100, 1)\nplot(close)\n");
    assert_eq!(program.inputs.len(), 1);
    let decl = &program.inputs[0];
    assert_eq!(decl.id, 0);
    assert_eq!(decl.kind, InputKind::Int);
    assert_eq!(decl.title.as_deref(), Some("Length"));
    assert_eq!(decl.defval, InputValue::Int(14));
    assert_eq!(decl.constraints.min, Some(InputValue::Int(1)));
    assert_eq!(decl.constraints.max, Some(InputValue::Int(100)));
    assert_eq!(decl.constraints.step, Some(InputValue::Int(1)));
}

#[test]
fn float_input_round_trips_metadata() {
    let program = compile_ok(
        "indicator(\"t\")\nm = input.float(2.0, \"Mult\", 0.5, 5.0, 0.5)\nplot(close * m)\n",
    );
    let decl = &program.inputs[0];
    assert_eq!(decl.kind, InputKind::Float);
    assert_eq!(decl.defval, InputValue::Float(2.0));
    assert_eq!(decl.constraints.min, Some(InputValue::Float(0.5)));
    assert_eq!(decl.constraints.max, Some(InputValue::Float(5.0)));
}

#[test]
fn bool_string_color_source_round_trip_metadata() {
    let program = compile_ok(
        "indicator(\"t\")\n\
         b = input.bool(true, \"On\")\n\
         s = input.string(\"hello\", \"Text\")\n\
         c = input.color(#FF0000, \"Col\")\n\
         src = input.source(close, \"Src\")\n\
         plot(src)\n",
    );
    assert_eq!(program.inputs.len(), 4);
    assert_eq!(program.inputs[0].kind, InputKind::Bool);
    assert_eq!(program.inputs[0].defval, InputValue::Bool(true));
    assert_eq!(program.inputs[1].kind, InputKind::String);
    assert_eq!(program.inputs[1].widget, InputWidget::String);
    assert_eq!(
        program.inputs[1].defval,
        InputValue::String("hello".to_string())
    );
    assert_eq!(program.inputs[2].kind, InputKind::Color);
    assert_eq!(
        program.inputs[2].defval,
        InputValue::Color {
            r: 0xFF,
            g: 0,
            b: 0,
            t: 0.0
        }
    );
    assert_eq!(program.inputs[3].kind, InputKind::Source);
    assert_eq!(
        program.inputs[3].defval,
        InputValue::Source(pinewasm_hir::BuiltinSeries::Close)
    );
}

#[test]
fn bool_input_signature_binds_full_positional_tail_and_input_qualified_active() {
    let program = compile_ok(
        "indicator(\"t\")\n\
         enabled = input.bool(true, \"Enabled\", \"help\", \"row\", \"Group\", true, display.data_window, active=input.bool(true, \"Gate\"))\n\
         plot(enabled ? close : na)\n",
    );
    assert_eq!(program.inputs.len(), 2);
    let enabled = program
        .inputs
        .iter()
        .find(|input| input.title.as_deref() == Some("Enabled"))
        .expect("enabled input present");
    assert_eq!(enabled.kind, InputKind::Bool);
    assert_eq!(enabled.tooltip.as_deref(), Some("help"));
    assert_eq!(enabled.group.as_deref(), Some("Group"));
    assert_eq!(enabled.display, InputDisplay::DataWindow);
}

#[test]
fn bool_input_rejects_series_qualified_active() {
    expect_error_code(
        "indicator(\"t\")\nenabled = input.bool(true, active=close > open)\nplot(enabled ? close : na)\n",
        "E0230",
    );
}

#[test]
fn timeframe_session_symbol_text_area_are_string_inputs() {
    let program = compile_ok(
        "indicator(\"t\")\n\
         tf = input.timeframe(\"D\", \"TF\")\n\
         se = input.session(\"0900-1600\", \"Sess\")\n\
         sy = input.symbol(\"AAPL\", \"Sym\")\n\
         ta = input.text_area(\"notes\", \"Notes\")\n\
         plot(close)\n",
    );
    assert_eq!(program.inputs.len(), 4);
    for decl in &program.inputs {
        assert_eq!(decl.kind, InputKind::String);
    }
    assert_eq!(
        program
            .inputs
            .iter()
            .map(|decl| decl.widget)
            .collect::<Vec<_>>(),
        vec![
            InputWidget::Timeframe,
            InputWidget::Session,
            InputWidget::Symbol,
            InputWidget::TextArea,
        ]
    );
    // Each string-family input carries its exact defval through to metadata, so
    // every member of the cluster is individually value-asserted.
    let defvals: Vec<&InputValue> = program.inputs.iter().map(|d| &d.defval).collect();
    assert_eq!(
        defvals,
        vec![
            &InputValue::String("D".to_string()),         // input.timeframe
            &InputValue::String("0900-1600".to_string()), // input.session
            &InputValue::String("AAPL".to_string()),      // input.symbol
            &InputValue::String("notes".to_string()),     // input.text_area
        ]
    );

    // The presentation discriminator is part of the durable CSIL artifact;
    // hosts do not have to recover it from source text or the shared String
    // runtime kind.
    let mut wire = serde_json::to_value(&program).expect("program serializes");
    let widgets = wire["inputs"]
        .as_array()
        .expect("input manifest")
        .iter()
        .map(|decl| decl["widget"].as_str().expect("widget string"))
        .collect::<Vec<_>>();
    assert_eq!(widgets, ["timeframe", "session", "symbol", "text_area"]);

    // Artifacts without this presentation-only field remain readable and
    // select the conservative standard control.
    for input in wire["inputs"]
        .as_array_mut()
        .expect("mutable input manifest")
    {
        input
            .as_object_mut()
            .expect("input object")
            .remove("widget");
    }
    let legacy: pinewasm_csil::CsilProgram =
        serde_json::from_value(wire).expect("artifact without the widget field deserializes");
    assert!(legacy
        .inputs
        .iter()
        .all(|decl| decl.widget == InputWidget::Standard));
}

#[test]
fn generic_and_namespaced_strings_keep_the_plain_string_widget() {
    let program = compile_ok(
        "indicator(\"t\")\n\
         generic = input(\"generic\")\n\
         named = input.string(\"named\")\n\
         plot(close)\n",
    );
    assert_eq!(
        program
            .inputs
            .iter()
            .map(|decl| (decl.kind, decl.widget))
            .collect::<Vec<_>>(),
        vec![
            (InputKind::String, InputWidget::String),
            (InputKind::String, InputWidget::String),
        ]
    );
}

#[test]
fn string_options_are_preserved_for_host_dropdowns() {
    let program = compile_ok(
        "indicator(\"t\")\nmode = input.string(\"Fast\", \"Mode\", options=[\"Fast\", \"Slow\"])\nplot(close)\n",
    );
    assert_eq!(
        program.inputs[0].constraints.options,
        vec![
            InputValue::String("Fast".to_string()),
            InputValue::String("Slow".to_string()),
        ]
    );
    expect_error_code(
        "indicator(\"t\")\nmode = input.string(\"Other\", options=[\"Fast\", \"Slow\"])\nplot(close)\n",
        "E0230",
    );
}

#[test]
fn input_metadata_survives_json_round_trip() {
    let program = compile_ok("indicator(\"t\")\nlen = input.int(14)\nplot(close)\n");
    let json = program.to_json();
    let back = pinewasm_csil::CsilProgram::from_json(&json).expect("valid json");
    assert_eq!(back.inputs, program.inputs);
}

#[test]
fn input_layout_metadata_survives_compile_and_json_round_trip() {
    let program = compile_ok(
        "indicator(\"t\")\n\
         enabled = input.bool(true, \"Enabled\", tooltip=\"Controls entries\", \
             inline=\"entry\", group=\"Trade settings\", confirm=true)\n\
         length = input.int(14, \"Length\", minval=1, inline=\"entry\", \
             group=\"Trade settings\")\n\
         plot(enabled ? length : 0)\n",
    );
    let enabled = &program.inputs[0];
    assert_eq!(enabled.tooltip.as_deref(), Some("Controls entries"));
    assert_eq!(enabled.inline.as_deref(), Some("entry"));
    assert_eq!(enabled.group.as_deref(), Some("Trade settings"));
    assert!(enabled.confirm);
    let length = &program.inputs[1];
    assert_eq!(length.inline.as_deref(), Some("entry"));
    assert_eq!(length.group.as_deref(), Some("Trade settings"));
    assert_eq!(length.tooltip, None);
    assert!(!length.confirm);

    let json = program.to_json();
    let back = pinewasm_csil::CsilProgram::from_json(&json).expect("valid json");
    assert_eq!(back.inputs, program.inputs);
}

#[test]
fn persistent_const_initializer_reaches_input_layout_metadata() {
    let program = compile_ok(
        "//@version=6\nindicator(\"Persistent group check\")\nvar string groupName = \"Settings\"\nlength = input.int(3, \"Length\", group=groupName, inline=groupName)\nplot(ta.sma(close, length))\n",
    );
    assert_eq!(program.inputs.len(), 1);
    assert_eq!(program.inputs[0].group.as_deref(), Some("Settings"));
    assert_eq!(program.inputs[0].inline.as_deref(), Some("Settings"));
}

#[test]
fn input_int_positional_range_preserves_trailing_confirm() {
    let program = compile_ok(
        "//@version=6\nindicator(\"Inputs\")\n\
         length = input.int(14, \"Length\", 1, 100, 1, confirm=true)\n\
         plot(length)\nplot(close)\n",
    );
    assert!(program.inputs[0].confirm);
}

#[test]
fn input_layout_metadata_requires_literal_values() {
    expect_error_code(
        "indicator(\"t\")\nflag = input.bool(true)\nn = input.int(1, confirm=flag)\nplot(n)\n",
        "E0230",
    );
    expect_error_code(
        "indicator(\"t\")\nn = input.int(1, tooltip=close)\nplot(n)\n",
        "E0230",
    );
}

#[test]
fn numeric_positional_overloads_bind_exact_positions() {
    let program = compile_ok(
        "indicator(\"t\")\n\
         range = input.int(10, \"Range\", 1, 20, 1, \"range tip\", \"row\", \"G\", true, display.data_window, true)\n\
         choice = input.int(10, \"Choice\", [5, 10, 20], \"options tip\", \"row\", \"G\", false, display.status_line, true)\n\
         plot(range + choice)\n",
    );
    let range = &program.inputs[0];
    assert_eq!(range.constraints.min, Some(InputValue::Int(1)));
    assert_eq!(range.constraints.max, Some(InputValue::Int(20)));
    assert_eq!(range.constraints.step, Some(InputValue::Int(1)));
    assert!(range.constraints.options.is_empty());
    assert_eq!(range.tooltip.as_deref(), Some("range tip"));
    assert_eq!(range.display, InputDisplay::DataWindow);
    assert!(range.confirm);

    let choice = &program.inputs[1];
    assert_eq!(
        choice.constraints.options,
        vec![InputValue::Int(5), InputValue::Int(10), InputValue::Int(20)]
    );
    assert!(choice.constraints.min.is_none());
    assert_eq!(choice.tooltip.as_deref(), Some("options tip"));
    assert_eq!(choice.display, InputDisplay::StatusLine);
    assert!(!choice.confirm);
}

#[test]
fn each_input_family_uses_its_documented_signature() {
    let program = compile_ok(
        "indicator(\"t\")\n\
         src = input.source(close, \"Source\", \"tip\", \"row\", \"G\", display.data_window, true, true)\n\
         genericSrc = input(close, \"Generic source\", \"row2\", \"G2\", \"generic tip\", display.status_line, true)\n\
         notes = input.text_area(\"hello\", \"Notes\", \"notes tip\", \"Text\", false, display.none, true)\n\
         symbol = input.symbol(\"MARKET:ACME\", \"Symbol\", \"symbol tip\", \"sym\", \"Data\", false, display.all, true)\n\
         plot(src + genericSrc)\n",
    );
    assert_eq!(program.inputs[0].tooltip.as_deref(), Some("tip"));
    assert!(program.inputs[0].confirm);
    assert_eq!(program.inputs[0].display, InputDisplay::DataWindow);
    assert_eq!(program.inputs[1].inline.as_deref(), Some("row2"));
    assert_eq!(program.inputs[1].group.as_deref(), Some("G2"));
    assert_eq!(program.inputs[1].tooltip.as_deref(), Some("generic tip"));
    assert_eq!(program.inputs[2].group.as_deref(), Some("Text"));
    assert_eq!(program.inputs[2].inline, None);

    expect_error_code(
        "indicator(\"t\")\nn = input(1, confirm=true)\nplot(n)\n",
        "E0228",
    );
    expect_error_code(
        "indicator(\"t\")\ns = input.symbol(\"ACME\", options=[\"ACME\"])\nplot(close)\n",
        "E0228",
    );
    expect_error_code(
        "indicator(\"t\")\ns = input.text_area(\"x\", inline=\"row\")\nplot(close)\n",
        "E0228",
    );
}

#[test]
fn display_defaults_and_combinations() {
    let program = compile_ok(
        "indicator(\"t\")\n\
         b = input.bool(true)\n\
         c = input.color(#FF0000)\n\
         notes = input.text_area(\"x\")\n\
         at = input.time(0)\n\
         n = input.int(1)\n\
         s = input.string(\"x\")\n\
         p = input.price(1.0)\n\
         both = input.int(1, display=display.data_window + display.status_line)\n\
         dataOnly = input.int(1, display=display.all - display.status_line)\n\
         plot(n + p)\n",
    );
    for index in 0..2 {
        assert_eq!(program.inputs[index].display, InputDisplay::None);
    }
    for index in 2..7 {
        assert_eq!(program.inputs[index].display, InputDisplay::All);
    }
    assert_eq!(program.inputs[7].display, InputDisplay::All);
    assert_eq!(program.inputs[8].display, InputDisplay::DataWindow);
}

#[test]
fn display_held_in_a_variable_matches_the_inline_form() {
    // `input.*` takes a const plot_display, so a display constant held in a
    // variable is the same argument as the inline expression it holds.
    let program = compile_ok(
        "indicator(\"t\")\n\
         K = display.all - display.status_line\n\
         B = display.data_window + display.status_line\n\
         N = display.all - display.all\n\
         S = display.status_line\n\
         dataOnly = input.int(1, display=K)\n\
         both = input.int(1, display=B)\n\
         hidden = input.int(1, display=N)\n\
         status = input.int(1, display=S)\n\
         grouped = input.int(1, display=(K))\n\
         plot(dataOnly + both + hidden + status + grouped)\n",
    );
    assert_eq!(program.inputs[0].display, InputDisplay::DataWindow);
    assert_eq!(program.inputs[1].display, InputDisplay::All);
    assert_eq!(program.inputs[2].display, InputDisplay::None);
    assert_eq!(program.inputs[3].display, InputDisplay::StatusLine);
    assert_eq!(program.inputs[4].display, InputDisplay::DataWindow);

    // The same folded set written from `display.all` is valid inline and held,
    // also through a second alias.
    let program = compile_ok(
        "indicator(\"t\")\n\
         P = display.all - display.data_window - display.status_line\n\
         Q = P\n\
         R = display.all - display.status_line\n\
         R2 = R\n\
         a = input.int(1, display=display.all - display.data_window - display.status_line)\n\
         b = input.int(1, display=P)\n\
         c = input.int(1, display=Q)\n\
         d = input.int(1, display=R2)\n\
         plot(a + b + c + d)\n",
    );
    assert_eq!(program.inputs[0].display, InputDisplay::None);
    assert_eq!(program.inputs[1].display, InputDisplay::None);
    assert_eq!(program.inputs[2].display, InputDisplay::None);
    assert_eq!(program.inputs[3].display, InputDisplay::DataWindow);

    // Pane and price-scale destinations stay plot-only however they are held,
    // exactly as they are rejected inline.
    // Both pane and price scale without the rest of `display.all` fold to the
    // same set as `display.all - display.data_window - display.status_line`,
    // so the held form must keep how it was written.
    for held in [
        "display.pane",
        "display.pane + display.data_window",
        "display.all - display.pane",
        "display.pane + display.price_scale",
        "display.pane + display.price_scale + display.data_window",
    ] {
        expect_error_code(
            &format!("indicator(\"t\")\nK = {held}\nn = input.int(1, display=K)\nplot(n)\n"),
            "E0230",
        );
        expect_error_code(
            &format!("indicator(\"t\")\nn = input.int(1, display={held})\nplot(n)\n"),
            "E0230",
        );
    }
}

#[test]
fn active_metadata_resolves_assigned_inputs_aliases_and_defaults() {
    let program = compile_ok(
        "indicator(\"t\")\n\
         enabled = input.bool(false)\n\
         enabledAlias = enabled\n\
         length = input.int(14, active=not enabledAlias)\n\
         mode = input.string(\"Off\", options=[\"On\", \"Off\"])\n\
         smoothing = input.int(3, active=mode == \"On\")\n\
         fixed = input.int(1, active=false)\n\
         blank = input.int(2, \"\")\n\
         plot(length + smoothing + fixed + blank)\n",
    );
    assert_eq!(program.inputs[0].title.as_deref(), Some("enabled"));
    assert_eq!(program.inputs[1].title.as_deref(), Some("length"));
    assert!(program.inputs[1].active.default);
    assert_eq!(program.inputs[1].active.dependencies, vec![0]);
    assert_eq!(
        program.inputs[1].active.direct,
        Some(InputActiveDirect {
            input: 0,
            inverted: true
        })
    );
    assert_eq!(program.inputs[3].title.as_deref(), Some("smoothing"));
    assert!(!program.inputs[3].active.default);
    assert_eq!(program.inputs[3].active.dependencies, vec![2]);
    assert_eq!(program.inputs[3].active.direct, None);
    assert!(!program.inputs[4].active.default);
    assert!(program.inputs[4].active.dependencies.is_empty());
    assert_eq!(program.inputs[5].title.as_deref(), Some(""));

    expect_error_code(
        "indicator(\"t\")\nn = input.int(1, active=close > open)\nplot(n)\n",
        "E0230",
    );
}

#[test]
fn enum_comparison_active_default_uses_member_identity() {
    let program = compile_ok(
        "indicator(\"t\")\n\
         enum Mode\n    off = \"Disabled\"\n    on = \"Enabled\"\n\
         mode = input.enum(Mode.off)\n\
         length = input.int(14, active=mode == Mode.on)\n\
         plot(length)\n",
    );
    assert!(!program.inputs[1].active.default);
    assert_eq!(program.inputs[1].active.dependencies, vec![0]);
}

// ---------------------------------------------------------------------------
// Overrides change run results at run time.
// ---------------------------------------------------------------------------

#[test]
fn int_override_changes_run_result() {
    let source = "indicator(\"t\")\nn = input.int(2)\nplot(close + n)\n";
    // Default: close(10.0) + 2 = 12.0 (int coerces to float alongside close).
    let defaults = run_with(source, &RunOptions::default());
    assert_eq!(defaults[0], Value::Float(12.0));

    // Override input 0 to 100: close + 100.
    let mut inputs = HashMap::new();
    inputs.insert(0, InputValue::Int(100));
    let options = RunOptions {
        inputs,
        ..RunOptions::default()
    };
    let overridden = run_with(source, &options);
    assert_eq!(overridden[0], Value::Float(110.0));
}

#[test]
fn float_override_changes_run_result() {
    let source = "indicator(\"t\")\nm = input.float(1.0)\nplot(close * m)\n";
    let mut inputs = HashMap::new();
    inputs.insert(0, InputValue::Float(3.0));
    let options = RunOptions {
        inputs,
        ..RunOptions::default()
    };
    let values = run_with(source, &options);
    // close[0] = 10.0, times 3.0.
    assert_eq!(values[0], Value::Float(30.0));
}

#[test]
fn bool_override_selects_branch() {
    let source = "indicator(\"t\")\nb = input.bool(false)\nplot(b ? close : 0.0)\n";
    // Default false: plots 0.0.
    let defaults = run_with(source, &RunOptions::default());
    assert_eq!(defaults[0], Value::Float(0.0));
    // Override true: plots close (10.0).
    let mut inputs = HashMap::new();
    inputs.insert(0, InputValue::Bool(true));
    let options = RunOptions {
        inputs,
        ..RunOptions::default()
    };
    let overridden = run_with(source, &options);
    assert_eq!(overridden[0], Value::Float(10.0));
}

#[test]
fn source_input_default_and_override_select_series() {
    let source = "indicator(\"t\")\nsrc = input.source(close)\nplot(src)\n";
    // Default close: [10, 12, 11, 10.75].
    let defaults = run_with(source, &RunOptions::default());
    assert_eq!(defaults[0], Value::Float(10.0));
    assert_eq!(defaults[1], Value::Float(12.0));

    // Override to high: [10.5, 12.5, 12.0, 11.0].
    let mut inputs = HashMap::new();
    inputs.insert(0, InputValue::Source(pinewasm_hir::BuiltinSeries::High));
    let options = RunOptions {
        inputs,
        ..RunOptions::default()
    };
    let overridden = run_with(source, &options);
    assert_eq!(overridden[0], Value::Float(10.5));
    assert_eq!(overridden[1], Value::Float(12.5));
}

#[test]
fn source_input_supports_exactly_the_eight_documented_price_series() {
    use pinewasm_hir::BuiltinSeries;

    let cases = [
        ("open", BuiltinSeries::Open),
        ("high", BuiltinSeries::High),
        ("low", BuiltinSeries::Low),
        ("close", BuiltinSeries::Close),
        ("hl2", BuiltinSeries::Hl2),
        ("hlc3", BuiltinSeries::Hlc3),
        ("ohlc4", BuiltinSeries::Ohlc4),
        ("hlcc4", BuiltinSeries::Hlcc4),
    ];
    for (name, expected) in cases {
        let source = format!("indicator(\"t\")\nsrc = input.source({name})\nplot(src)\n");
        let program = compile_ok(&source);
        assert_eq!(
            program.inputs[0].defval,
            InputValue::Source(expected),
            "{name} must retain its first-class BuiltinSeries identity"
        );
    }

    for invalid in [
        "volume",
        "time",
        "bar_index",
        "barstate.isfirst",
        "true",
        "42",
        "\"close\"",
    ] {
        expect_error_code(
            &format!("indicator(\"t\")\nsrc = input.source({invalid})\nplot(src)\n"),
            "E0230",
        );
    }
}

#[test]
fn source_input_derived_defaults_and_overrides_use_exact_ohlc_formulas() {
    use pinewasm_hir::BuiltinSeries;

    let source = "indicator(\"t\")\nsrc = input.source(hl2)\nplot(src)\n";
    let defaults = run_with(source, &RunOptions::default());
    assert_eq!(defaults[0], Value::Float(f64::midpoint(10.5, 8.5)));
    assert_eq!(defaults[1], Value::Float(f64::midpoint(12.5, 10.5)));

    let mut inputs = HashMap::new();
    inputs.insert(0, InputValue::Source(BuiltinSeries::Ohlc4));
    let overridden = run_with(
        source,
        &RunOptions {
            inputs,
            ..RunOptions::default()
        },
    );
    assert_eq!(overridden[0], Value::Float((9.0 + 10.5 + 8.5 + 10.0) / 4.0));
    assert_eq!(
        overridden[1],
        Value::Float((11.0 + 12.5 + 10.5 + 12.0) / 4.0)
    );
}

#[test]
fn invalid_source_host_override_falls_back_to_the_declared_default() {
    use pinewasm_hir::BuiltinSeries;

    let source = "indicator(\"t\")\nsrc = input.source(close)\nplot(src)\n";
    let mut inputs = HashMap::new();
    inputs.insert(0, InputValue::Source(BuiltinSeries::Volume));
    let values = run_with(
        source,
        &RunOptions {
            inputs,
            ..RunOptions::default()
        },
    );
    assert_eq!(values[0], Value::Float(10.0));
    assert_eq!(values[1], Value::Float(12.0));
}

#[test]
fn input_display_aliases_require_a_const_plot_display_before_folding() {
    let valid = compile_ok(
        "//@version=6\nindicator(\"Input display constant\")\ndisplaySetting = display.all - display.status_line\nlength = input.int(14, \"Length\", display=displaySetting)\nplot(close + length)\n",
    );
    assert_eq!(valid.inputs.len(), 1);
    assert_eq!(valid.inputs[0].display, InputDisplay::DataWindow);

    let source = "//@version=6\nindicator(\"Input display mutable alias\")\nvar displaySetting = display.all + display.none\nlength = input.int(14, \"Length\", display=displaySetting)\ndisplaySetting := display.status_line\nplot(close + length)\n";
    let result = compile(source, CompileOptions::default());
    assert!(!result.success, "mutable input display alias was folded");
    assert!(
        result.program.is_none(),
        "invalid display exported a program"
    );
    let argument = "display=displaySetting";
    let start = source.find(argument).expect("display argument") + "display=".len();
    let end = start + "displaySetting".len();
    assert!(
        result.diagnostics.iter().any(|diagnostic| {
            diagnostic.code.as_deref() == Some("E0252")
                && diagnostic.message.contains("fixed")
                && diagnostic.labels.iter().any(|label| {
                    label.span.start as usize == start && label.span.end as usize == end
                })
        }),
        "missing const input-display diagnostic: {:?}",
        result.diagnostics
    );
}

#[test]
fn documented_const_input_arguments_accept_expressions_and_aliases() {
    let source = r#"indicator("t")
TITLE = "Input " + "title"
TOOLTIP = "const tooltip"
INLINE = "row"
GROUP = "Group"
MINIMUM = 1 + 1
MAXIMUM = 10 * 2
STEP = 2
DEFAULT = MINIMUM + STEP
CONFIRM = not false
DISPLAY = display.data_window
n = input.int(DEFAULT, TITLE, MINIMUM, MAXIMUM, STEP, TOOLTIP, INLINE, GROUP, CONFIRM)
date = input.time(timestamp("27 Dec 2024 09:30 -0500"), "Date")
shade = input.color(color.new(color.blue, 90), "Color")
generic = input(1 + 2, "Generic", display=DISPLAY + display.status_line)
plot(n + date * 0 + generic)
"#;
    let program = compile_ok(source);
    assert_eq!(program.inputs.len(), 4);
    assert_eq!(program.inputs[0].title.as_deref(), Some("Input title"));
    assert_eq!(program.inputs[0].tooltip.as_deref(), Some("const tooltip"));
    assert_eq!(program.inputs[0].inline.as_deref(), Some("row"));
    assert_eq!(program.inputs[0].group.as_deref(), Some("Group"));
    assert!(program.inputs[0].confirm);
    assert_eq!(program.inputs[0].defval, InputValue::Int(4));
    assert_eq!(program.inputs[0].constraints.min, Some(InputValue::Int(2)));
    assert_eq!(program.inputs[0].constraints.max, Some(InputValue::Int(20)));
    assert_eq!(program.inputs[0].constraints.step, Some(InputValue::Int(2)));
    assert_eq!(program.inputs[1].defval, InputValue::Int(1_735_309_800_000));
    assert_eq!(
        program.inputs[2].defval,
        InputValue::Color {
            r: 0x29,
            g: 0x62,
            b: 0xFF,
            t: 90.0,
        }
    );
    assert_eq!(program.inputs[3].defval, InputValue::Int(3));
    assert_eq!(program.inputs[3].display, InputDisplay::All);
}

#[test]
fn input_const_arguments_do_not_accept_weaker_qualifiers() {
    for source in [
        "indicator(\"t\")\ntitle = input.string(\"x\")\nn = input.int(1, title)\nplot(n)\n",
        "indicator(\"t\")\nn = input.int(close + 1)\nplot(n)\n",
        "indicator(\"t\")\nc = input.color(close > open ? color.blue : color.red)\nplot(close)\n",
        "indicator(\"t\")\nt = input.time(time)\nplot(t)\n",
    ] {
        expect_error_code(source, "E0230");
    }
}

#[test]
fn override_with_wrong_kind_is_ignored_falls_back_to_default() {
    // Supplying a String override for an int input is ignored; the default
    // (5) is used, not a runtime abort.
    let source = "indicator(\"t\")\nn = input.int(5)\nplot(close + n)\n";
    let mut inputs = HashMap::new();
    inputs.insert(0, InputValue::String("nonsense".to_string()));
    let options = RunOptions {
        inputs,
        ..RunOptions::default()
    };
    let run = compile_and_run(source, &data(), CompileOptions::default(), &options);
    assert!(run.runtime_errors.is_empty());
    // close(10) + default 5 = 15.
    assert_eq!(run.plots[0].values[0], Value::Float(15.0));
}

#[test]
fn override_for_unknown_id_is_ignored() {
    let source = "indicator(\"t\")\nn = input.int(7)\nplot(close + n)\n";
    let mut inputs = HashMap::new();
    inputs.insert(99, InputValue::Int(1000)); // no such input
    let options = RunOptions {
        inputs,
        ..RunOptions::default()
    };
    let run = compile_and_run(source, &data(), CompileOptions::default(), &options);
    assert!(run.runtime_errors.is_empty());
    assert_eq!(run.plots[0].values[0], Value::Float(17.0));
}

// ---------------------------------------------------------------------------
// Constraint violations (E0230) at compile time.
// ---------------------------------------------------------------------------

#[test]
fn non_const_default_is_rejected() {
    // A series value (close) is not a const literal.
    expect_error_code(
        "indicator(\"t\")\nn = input.int(close)\nplot(close)\n",
        "E0230",
    );
}

#[test]
fn default_below_minval_is_rejected() {
    expect_error_code(
        "indicator(\"t\")\nn = input.int(0, \"n\", 1, 100)\nplot(close)\n",
        "E0230",
    );
}

#[test]
fn default_above_maxval_is_rejected() {
    expect_error_code(
        "indicator(\"t\")\nn = input.int(200, \"n\", 1, 100)\nplot(close)\n",
        "E0230",
    );
}

#[test]
fn default_not_in_options_is_rejected() {
    expect_error_code(
        "indicator(\"t\")\nn = input.int(3, \"n\", options=[1, 2, 4])\nplot(close)\n",
        "E0230",
    );
}

#[test]
fn options_and_range_together_is_rejected() {
    expect_error_code(
        "indicator(\"t\")\nn = input.int(1, \"n\", 1, 100, options=[1, 2])\nplot(close)\n",
        "E0230",
    );
}

#[test]
fn numeric_input_options_overload_rejects_each_range_argument() {
    for (function, default_value, option_values, range_value) in [
        ("input.int", "1", "[1, 2]", "1"),
        ("input.float", "1.0", "[1.0, 2.0]", "1.0"),
    ] {
        for range_name in ["minval", "maxval", "step"] {
            let source = format!(
                "indicator(\"t\")\nn = {function}({default_value}, options={option_values}, {range_name}={range_value})\nplot(close)\n"
            );
            expect_error_code(&source, "E0230");
        }
    }
}

#[test]
fn int_and_float_input_signatures_bind_options_and_range_independently() {
    for source in [
        "indicator(\"t\")\nn = input.int(1, options=[1, 2])\nplot(n)\n",
        "indicator(\"t\")\nn = input.int(1, minval=1, maxval=2, step=1)\nplot(n)\n",
        "indicator(\"t\")\nn = input.float(1.0, options=[1.0, 2.0])\nplot(n)\n",
        "indicator(\"t\")\nn = input.float(1.0, minval=1.0, maxval=2.0, step=1.0)\nplot(n)\n",
    ] {
        compile_ok(source);
    }
}

#[test]
fn wrong_type_default_is_rejected() {
    // A string default for input.int.
    expect_error_code(
        "indicator(\"t\")\nn = input.int(\"x\")\nplot(close)\n",
        "E0230",
    );
}

#[test]
fn source_default_must_be_a_builtin_series() {
    expect_error_code(
        "indicator(\"t\")\nsrc = input.source(42)\nplot(src)\n",
        "E0230",
    );
}

#[test]
fn unknown_input_parameter_name_is_reported() {
    // E0228 is the named-argument error family (reused by input binding).
    expect_error_code(
        "indicator(\"t\")\nn = input.int(14, minvale=1)\nplot(close)\n",
        "E0228",
    );
}

#[test]
fn default_in_options_is_accepted() {
    let program =
        compile_ok("indicator(\"t\")\nn = input.int(2, \"n\", options=[1, 2, 4])\nplot(close)\n");
    assert_eq!(program.inputs[0].constraints.options.len(), 3);
}

// ---------------------------------------------------------------------------
// Qualifier lattice: an input value is `input`-qualified, which is weaker
// than `const`. It must therefore be rejected where a const literal is
// required (plot title), proving the lattice producer is wired correctly.
// ---------------------------------------------------------------------------

#[test]
fn input_string_is_not_a_const_plot_title() {
    // plot's title parameter is `max_qualifier = const`. An input string is
    // `input`-qualified, which is *weaker* than const in the lattice
    // (`Const < Input`), so the qualifier check rejects it (E0206). This is
    // the direct proof that the `input` producer sits correctly in the
    // lattice: it is not accepted where a const is required.
    expect_error_code(
        "indicator(\"t\")\ns = input.string(\"hi\")\nplot(close, s)\n",
        "E0206",
    );
}

#[test]
fn input_string_is_not_a_const_indicator_title() {
    // indicator title likewise requires a `const string`; an input string is
    // `input`-qualified and rejected through the qualifier lattice (E0206,
    // the same code as `plot`'s title), not the type code E0205.
    expect_error_code(
        "s = input.string(\"hi\")\nindicator(s)\nplot(close)\n",
        "E0206",
    );
}

#[test]
fn input_int_is_accepted_as_a_ta_length() {
    // A `ta.*` length may come from an `input.int(...)`, because an input is
    // fixed for the run (`simple int`). Its default is validated at compile
    // time; binding metadata preserves overrides for run initialization.
    let result = compile(
        "indicator(\"t\")\nn = input.int(14)\nplot(ta.sma(close, n))\n",
        CompileOptions::default(),
    );
    assert!(
        result.success,
        "an input int should be accepted as a ta length: {:?}",
        result.diagnostics
    );

    // ta.sma accepts a series-int length, while ta.ema requires a simple int.
    let series_len = compile(
        "indicator(\"t\")\nplot(ta.sma(close, int(close)))\n",
        CompileOptions::default(),
    );
    assert!(series_len.success, "{:?}", series_len.diagnostics);
    expect_error_code(
        "indicator(\"t\")\nplot(ta.ema(close, int(close)))\n",
        "E0219",
    );
}

#[test]
fn input_value_flows_into_arithmetic_as_input_qualifier() {
    // An input int used in arithmetic is fine (series/simple/input all accept
    // an input operand); this compiles and runs.
    let program = compile_ok("indicator(\"t\")\nn = input.int(3)\nplot(close + n)\n");
    assert_eq!(program.inputs.len(), 1);
}

// ---------------------------------------------------------------------------
// Generic `input(defval, ...)` overload dispatch.
// ---------------------------------------------------------------------------

#[test]
fn input_generic_int_defval_dispatches_like_input_int() {
    // `input(5)` records an `InputKind::Int` decl (dispatched on the defval
    // type) and resolves exactly like `input.int(5)`.
    let program = compile_ok("indicator(\"t\")\nn = input(5)\nplot(close + n)\n");
    assert_eq!(program.inputs.len(), 1);
    assert_eq!(program.inputs[0].kind, InputKind::Int);
    assert_eq!(program.inputs[0].defval, InputValue::Int(5));
    // Default run: close(10.0) + 5.
    let values = run_with(
        "indicator(\"t\")\nn = input(5)\nplot(close + n)\n",
        &RunOptions::default(),
    );
    assert_eq!(values[0], Value::Float(15.0));
    // A host override changes it, proving the shared override machinery is used.
    let mut inputs = HashMap::new();
    inputs.insert(0, InputValue::Int(20));
    let options = RunOptions {
        inputs,
        ..RunOptions::default()
    };
    let overridden = run_with(
        "indicator(\"t\")\nn = input(5)\nplot(close + n)\n",
        &options,
    );
    assert_eq!(overridden[0], Value::Float(30.0));
}

#[test]
fn input_generic_float_defval_dispatches_like_input_float() {
    // `input(5.0)` is an `InputKind::Float` (not int) — dispatched on the float
    // literal — and multiplies close like `input.float(5.0)`.
    let program = compile_ok("indicator(\"t\")\nm = input(5.0)\nplot(close * m)\n");
    assert_eq!(program.inputs[0].kind, InputKind::Float);
    assert_eq!(program.inputs[0].defval, InputValue::Float(5.0));
    let values = run_with(
        "indicator(\"t\")\nm = input(5.0)\nplot(close * m)\n",
        &RunOptions::default(),
    );
    // close[0] = 10.0 * 5.0.
    assert_eq!(values[0], Value::Float(50.0));
}

#[test]
fn input_generic_bool_string_color_source_dispatch() {
    let program = compile_ok(
        "indicator(\"t\")\n\
         b = input(true)\n\
         s = input(\"hi\")\n\
         c = input(#00FF00)\n\
         src = input(close)\n\
         plot(src)\n",
    );
    assert_eq!(program.inputs[0].kind, InputKind::Bool);
    assert_eq!(program.inputs[1].kind, InputKind::String);
    assert_eq!(program.inputs[2].kind, InputKind::Color);
    assert_eq!(program.inputs[3].kind, InputKind::Source);
}

#[test]
fn input_generic_named_defval_dispatches() {
    // `input(defval = 7)` (named) also dispatches on the defval type.
    let program = compile_ok("indicator(\"t\")\nn = input(defval = 7)\nplot(close + n)\n");
    assert_eq!(program.inputs[0].kind, InputKind::Int);
    assert_eq!(program.inputs[0].defval, InputValue::Int(7));
}

// ---------------------------------------------------------------------------
// `input.enum(defval, ...)` — enum-typed input.
// ---------------------------------------------------------------------------

#[test]
fn input_enum_defaults_to_member_and_host_override_selects_another() {
    // `input.enum(MyEnum.a)` defaults to `a` (variant index 0). The read is
    // typed as the enum; comparing against members selects a plot value.
    let src = "indicator(\"t\")\n\
               enum MyEnum\n    a = \"Alpha\"\n    b\n    c = \"\"\n\
               e = input.enum(MyEnum.a)\n\
               plot(e == MyEnum.a ? 1.0 : (e == MyEnum.b ? 2.0 : 3.0))\n";
    let program = compile_ok(src);
    assert_eq!(program.inputs.len(), 1);
    assert_eq!(program.inputs[0].kind, InputKind::Enum);
    assert_eq!(
        program.inputs[0].defval,
        InputValue::Enum {
            name: "MyEnum".to_string(),
            index: 0,
            member: "a".to_string(),
            title: Some("Alpha".to_string()),
        }
    );
    assert_eq!(program.inputs[0].constraints.options.len(), 3);
    assert_eq!(
        program.inputs[0].constraints.options[1],
        InputValue::Enum {
            name: "MyEnum".to_string(),
            index: 1,
            member: "b".to_string(),
            title: None,
        }
    );
    assert_eq!(
        program.inputs[0].constraints.options[2],
        InputValue::Enum {
            name: "MyEnum".to_string(),
            index: 2,
            member: "c".to_string(),
            title: Some(String::new()),
        }
    );
    // Default: member `a` → 1.0.
    let values = run_with(src, &RunOptions::default());
    assert_eq!(values[0], Value::Float(1.0));
    // Host override selects member `b` (index 1) → 2.0.
    let mut inputs = HashMap::new();
    inputs.insert(
        0,
        InputValue::Enum {
            name: "MyEnum".to_string(),
            index: 1,
            member: "b".to_string(),
            title: None,
        },
    );
    let options = RunOptions {
        inputs,
        ..RunOptions::default()
    };
    let overridden = run_with(src, &options);
    assert_eq!(overridden[0], Value::Float(2.0));

    // Runtime validation rejects a foreign enum type or an out-of-range tag
    // and falls back to the declaration's default.
    for invalid in [
        InputValue::Enum {
            name: "OtherEnum".to_string(),
            index: 1,
            member: "b".to_string(),
            title: None,
        },
        InputValue::Enum {
            name: "MyEnum".to_string(),
            index: 99,
            member: "unknown".to_string(),
            title: None,
        },
    ] {
        let mut inputs = HashMap::new();
        inputs.insert(0, invalid);
        let values = run_with(
            src,
            &RunOptions {
                inputs,
                ..RunOptions::default()
            },
        );
        assert_eq!(values[0], Value::Float(1.0));
    }
}

#[test]
fn input_enum_options_are_preserved_and_validate_the_default() {
    let src = "indicator(\"t\")\n\
               enum MyEnum\n    a = \"First\"\n    b\n    c\n\
               e = input.enum(MyEnum.b, options=[MyEnum.a, MyEnum.b])\n\
               plot(e == MyEnum.b ? 1.0 : 0.0)\n";
    let program = compile_ok(src);
    assert_eq!(
        program.inputs[0].constraints.options,
        vec![
            InputValue::Enum {
                name: "MyEnum".to_string(),
                index: 0,
                member: "a".to_string(),
                title: Some("First".to_string()),
            },
            InputValue::Enum {
                name: "MyEnum".to_string(),
                index: 1,
                member: "b".to_string(),
                title: None,
            },
        ]
    );
    expect_error_code(
        "indicator(\"t\")\n\
         enum MyEnum\n    a\n    b\n\
         e = input.enum(MyEnum.b, options=[MyEnum.a])\n\
         plot(close)\n",
        "E0230",
    );
}

#[test]
fn input_enum_non_member_defval_is_rejected() {
    // A `defval` that is not a member of any declared enum is E0230.
    expect_error_code(
        "indicator(\"t\")\nenum E\n    a\ne = input.enum(42)\nplot(close)\n",
        "E0230",
    );
}

// ---------------------------------------------------------------------------
// input.color accepts any const color expression (not just a literal).
// ---------------------------------------------------------------------------

/// The `(r, g, b, t)` of the first input's default, which must be a color.
fn color_defval(source: &str) -> (u8, u8, u8, f64) {
    let program = compile_ok(source);
    match program.inputs[0].defval {
        InputValue::Color { r, g, b, t } => (r, g, b, t),
        ref other => panic!("expected a color defval, found {other:?}"),
    }
}

#[test]
fn input_color_accepts_const_color_constructors() {
    // input.color accepts any const color expression where a color is required. Each
    // form must fold to exactly what the same call evaluates to at runtime.
    let src = |defval: &str| {
        format!("//@version=6\nindicator(\"C\")\nc = input.color({defval}, \"C\")\nplot(close, color = c)\n")
    };

    // A plain literal and a color.* constant still work.
    assert_eq!(color_defval(&src("#009988")), (0, 0x99, 0x88, 0.0));
    // color.new replaces the base transparency.
    assert_eq!(
        color_defval(&src("color.new(#009988, 30)")),
        (0, 0x99, 0x88, 30.0)
    );
    // ...including over a literal that already carries an alpha byte.
    assert_eq!(
        color_defval(&src("color.new(#00998880, 30)")),
        (0, 0x99, 0x88, 30.0)
    );
    // color.rgb, with and without the optional transp (default 0 = opaque).
    assert_eq!(
        color_defval(&src("color.rgb(0, 153, 136)")),
        (0, 153, 136, 0.0)
    );
    assert_eq!(
        color_defval(&src("color.rgb(0, 153, 136, 40.0)")),
        (0, 153, 136, 40.0)
    );
    // Nesting composes.
    assert_eq!(
        color_defval(&src("color.new(color.rgb(0, 153, 136), 25)")),
        (0, 153, 136, 25.0)
    );
    // Float channels round, and out-of-range values clamp, exactly as the VM does.
    assert_eq!(
        color_defval(&src("color.rgb(0.4, 152.6, 136.0)")),
        (0, 153, 136, 0.0)
    );
    assert_eq!(
        color_defval(&src("color.new(#009988, 140)")),
        (0, 0x99, 0x88, 100.0)
    );
    assert_eq!(
        color_defval(&src("color.new(#009988, -10)")),
        (0, 0x99, 0x88, 0.0)
    );
    // Named arguments bind the same way.
    assert_eq!(
        color_defval(&src("color.new(color = #009988, transp = 30)")),
        (0, 0x99, 0x88, 30.0)
    );
}

#[test]
fn input_generic_dispatches_a_const_color_constructor_to_input_color() {
    let program = compile_ok(
        "//@version=6\nindicator(\"C\")\nc = input(color.new(#009988, 30), \"C\")\nplot(close, color = c)\n",
    );
    assert_eq!(program.inputs[0].kind, InputKind::Color);
    assert!(matches!(
        program.inputs[0].defval,
        InputValue::Color {
            r: 0,
            g: 0x99,
            b: 0x88,
            t: 30.0
        }
    ));
}

#[test]
fn input_color_still_rejects_a_non_const_color() {
    // A per-bar color is not a compile-time default.
    let result = compile(
        "//@version=6\nindicator(\"C\")\nc = input.color(close > open ? color.red : color.green)\nplot(close, color = c)\n",
        CompileOptions::default(),
    );
    assert!(!result.success, "a series color defval must be rejected");
}

#[test]
fn ta_input_lengths_compile_once_and_match_changed_defaults() {
    let source = concat!(
        "indicator(\"runtime lengths\")\n",
        "n = input.int(14, \"Length\")\n",
        "alias = n\n",
        "smooth(simple int period) =>\n    ta.ema(close, period)\n",
        "plot(ta.sma(close, alias))\nplot(ta.change(close, n))\n",
        "plot(smooth(alias))\nplot(ta.rsi(close, n))\nplot(math.sum(close, n))\n",
        "plot(ta.highest(close, n))\nplot(ta.lowest(close, n))\n",
        "plot(ta.highestbars(close, n))\nplot(ta.lowestbars(close, n))\n",
        "[basis, upper, lower] = ta.bb(close, n, 2.0)\nplot(basis)\nplot(upper)\nplot(lower)\n",
        "[m, signal, hist] = ta.macd(close, n, n, n)\nplot(m)\nplot(signal)\nplot(hist)\n",
    );
    let program = compile_ok(source);
    let serialized = program.to_json();
    let program = pinewasm_csil::CsilProgram::from_json(&serialized).unwrap();
    pinewasm_csil::validate(&program).unwrap();
    let close: Vec<_> = (0..180).map(|i| 100.0 + f64::from((i * 17) % 31)).collect();
    let data = MarketData {
        open: close.clone(),
        high: close.clone(),
        low: close.clone(),
        close,
        volume: vec![100.0; 180],
        ..MarketData::default()
    };
    for length in [14, 50, 2, 1, 14, 50] {
        let options = RunOptions {
            inputs: HashMap::from([(0, InputValue::Int(length))]),
            ..RunOptions::default()
        };
        let actual = pinewasm_vm::run(&program, &data, &options);
        let expected = pinewasm_vm::run(
            &compile_ok(&source.replace("input.int(14", &format!("input.int({length}"))),
            &data,
            &RunOptions::default(),
        );
        assert!(actual.errors.is_empty(), "{:?}", actual.errors);
        assert!(expected.errors.is_empty(), "{:?}", expected.errors);
        for (a, e) in actual.plots.iter().zip(&expected.plots) {
            assert_eq!(a.values, e.values, "length={length}");
        }
        if length > 1 {
            assert_eq!(actual.plots[0].values[length as usize - 2], Value::Na);
        }
    }
    assert_eq!(
        program.to_json(),
        serialized,
        "runs must not mutate the reusable artifact"
    );
}

#[test]
fn persistent_input_ta_lengths_follow_host_overrides() {
    let source = "//@version=6\nindicator(\"Persistent TA length check\")\nvar int n = input.int(3, \"Length\")\nplot(ta.sma(close, n))\nplot(ta.ema(close, n))\n";
    let program = compile_ok(source);
    let default = pinewasm_vm::run(&program, &data(), &RunOptions::default());
    let overridden = pinewasm_vm::run(
        &program,
        &data(),
        &RunOptions {
            inputs: HashMap::from([(0, InputValue::Int(2))]),
            ..RunOptions::default()
        },
    );
    let expected = pinewasm_vm::run(
        &compile_ok(&source.replace("input.int(3", "input.int(2")),
        &data(),
        &RunOptions::default(),
    );
    assert!(default.errors.is_empty(), "{:?}", default.errors);
    assert!(overridden.errors.is_empty(), "{:?}", overridden.errors);
    assert!(expected.errors.is_empty(), "{:?}", expected.errors);
    assert_ne!(default.plots[0].values, overridden.plots[0].values);
    for (actual, expected) in overridden.plots.iter().zip(&expected.plots) {
        assert_eq!(actual.values, expected.values);
    }
}

#[test]
fn float_ta_inputs_and_persistent_aliases_follow_host_overrides() {
    let source = "//@version=6\nindicator(\"Float TA inputs\")\nvar float mult = input.float(2.0, \"Mult\")\nfactor = input.float(3.0, \"Factor\")\n[mid, upper, lower] = ta.bb(close, 2, mult)\n[trend, direction] = ta.supertrend(factor, 2)\nplot(upper)\nplot(trend)\n";
    let program = compile_ok(source);
    assert_eq!(program.ta_input_bindings.len(), 2);
    let overrides = HashMap::from([(0, InputValue::Float(3.0)), (1, InputValue::Float(4.0))]);
    let actual = pinewasm_vm::run(
        &program,
        &data(),
        &RunOptions {
            inputs: overrides,
            ..RunOptions::default()
        },
    );
    let expected_source = source
        .replace("input.float(2.0", "input.float(3.0")
        .replace(
            "input.float(3.0, \"Factor\")",
            "input.float(4.0, \"Factor\")",
        );
    let expected = pinewasm_vm::run(
        &compile_ok(&expected_source),
        &data(),
        &RunOptions::default(),
    );
    assert!(actual.errors.is_empty(), "{:?}", actual.errors);
    assert!(expected.errors.is_empty(), "{:?}", expected.errors);
    for (actual, expected) in actual.plots.iter().zip(&expected.plots) {
        assert_eq!(actual.values, expected.values);
    }
}

#[test]
fn float_ta_inputs_validate_engine_bounds_and_wrong_kind_fallback() {
    let source = "indicator(\"Float TA bounds\")\nmult = input.float(2.0)\npct = input.float(50.0)\n[mid, upper, lower] = ta.bb(close, 2, mult)\nplot(upper)\nplot(ta.percentile_nearest_rank(close, 2, pct))\n";
    let program = compile_ok(source);
    assert_eq!(program.ta_input_bindings.len(), 2);
    let baseline = pinewasm_vm::run(&program, &data(), &RunOptions::default());
    assert!(baseline.errors.is_empty(), "{:?}", baseline.errors);
    let fallback = pinewasm_vm::run(
        &program,
        &data(),
        &RunOptions {
            inputs: HashMap::from([(0, InputValue::Int(7))]),
            ..RunOptions::default()
        },
    );
    assert_eq!(fallback.plots, baseline.plots);
    for bad in [
        InputValue::Float(f64::NAN),
        InputValue::Float(f64::INFINITY),
    ] {
        let result = pinewasm_vm::run(
            &program,
            &data(),
            &RunOptions {
                inputs: HashMap::from([(0, bad)]),
                ..RunOptions::default()
            },
        );
        assert!(!result.errors.is_empty());
    }
    let result = pinewasm_vm::run(
        &program,
        &data(),
        &RunOptions {
            inputs: HashMap::from([(1, InputValue::Float(101.0))]),
            ..RunOptions::default()
        },
    );
    assert!(!result.errors.is_empty());
}

#[test]
fn ta_input_lengths_reject_invalid_overrides_and_keep_wrong_kind_fallback() {
    let program = compile_ok(
        "indicator(\"t\")\nn = input.int(2)\nplot(ta.sma(close, n))\nplot(ta.ema(close, n))\n",
    );
    for length in [0, -1, 100_001, i64::MAX, i64::MIN] {
        let options = RunOptions {
            inputs: HashMap::from([(0, InputValue::Int(length))]),
            ..RunOptions::default()
        };
        let result = pinewasm_vm::run(&program, &data(), &options);
        assert_eq!(
            result.errors[0].kind,
            pinewasm_vm::RuntimeErrorKind::InvalidTaLength
        );
    }
    let result = pinewasm_vm::run(
        &program,
        &data(),
        &RunOptions {
            inputs: HashMap::from([(0, InputValue::Bool(true))]),
            ..RunOptions::default()
        },
    );
    assert!(result.errors.is_empty());
    assert_eq!(
        result.plots[0].values,
        vec![
            Value::Na,
            Value::Float(11.0),
            Value::Float(11.5),
            Value::Float(10.875)
        ]
    );
}

#[test]
fn direct_generic_input_ta_lengths_keep_host_overrides() {
    for call in ["ta.sma", "ta.change"] {
        let source = format!(
            "//@version=6\nindicator(\"Direct input length\")\nplot({call}(close, input(3, \"Length\")))\n"
        );
        let program = compile_ok(&source);
        assert_eq!(program.inputs.len(), 1, "{call}");
        assert_eq!(program.inputs[0].defval, InputValue::Int(3));
        assert_eq!(program.ta_input_bindings.len(), 1, "{call}");
        assert_eq!(program.ta_input_bindings[0].input, 0);

        for length in [3, 2] {
            let options = RunOptions {
                inputs: HashMap::from([(0, InputValue::Int(length))]),
                ..RunOptions::default()
            };
            let actual = pinewasm_vm::run(&program, &data(), &options);
            let expected = compile_and_run(
                &format!("indicator(\"Literal\")\nplot({call}(close, {length}))\n"),
                &data(),
                CompileOptions::default(),
                &RunOptions::default(),
            );
            assert!(actual.errors.is_empty(), "{call}: {:?}", actual.errors);
            assert!(
                expected.runtime_errors.is_empty(),
                "{call}: {:?}",
                expected.runtime_errors
            );
            assert_eq!(
                actual.plots[0].values, expected.plots[0].values,
                "{call}, length {length}"
            );
        }
    }
    let negative = compile_ok(
        "indicator(\"Negated input\")\nplot(ta.sma(close, -input(-3, \"Negative length\")))\n",
    );
    assert!(negative.ta_input_bindings[0].negated);
    let actual = pinewasm_vm::run(
        &negative,
        &data(),
        &RunOptions {
            inputs: HashMap::from([(0, InputValue::Int(-2))]),
            ..RunOptions::default()
        },
    );
    assert!(actual.errors.is_empty(), "{:?}", actual.errors);
    assert_eq!(
        actual.plots[0].values,
        run_with(
            "indicator(\"Literal\")\nplot(ta.sma(close, 2))\n",
            &RunOptions::default()
        )
    );
    expect_error_code(
        "indicator(\"Bad\")\nplot(ta.sma(close, input(3.5)))\n",
        "E0219",
    );
    expect_error_code(
        "indicator(\"Bad\")\nplot(ta.ema(close, int(close)))\n",
        "E0219",
    );
}

#[test]
fn ta_input_lengths_apply_memory_limits_to_the_selected_windows() {
    let source = "indicator(\"memory\")\nn = input.int(100000)\nplot(ta.sma(close, n))\nplot(ta.change(close, n))\nplot(math.sum(close, n))\n";
    let large_default = compile_ok(source);
    let small_default = compile_ok(&source.replace("100000", "2"));
    let options = RunOptions {
        max_vm_bytes_per_run: 1024 * 1024,
        ..RunOptions::default()
    };
    let expected = pinewasm_vm::run(&small_default, &data(), &options);
    assert!(expected.errors.is_empty(), "{:?}", expected.errors);
    let small_override = RunOptions {
        inputs: HashMap::from([(0, InputValue::Int(2))]),
        ..options.clone()
    };
    let actual = pinewasm_vm::run(&large_default, &data(), &small_override);
    assert!(actual.errors.is_empty(), "{:?}", actual.errors);
    for (actual, expected) in actual.plots.iter().zip(&expected.plots) {
        assert_eq!(actual.values, expected.values);
    }
    for (program, options) in [
        (&large_default, options.clone()),
        (
            &small_default,
            RunOptions {
                inputs: HashMap::from([(0, InputValue::Int(100_000))]),
                ..options
            },
        ),
    ] {
        let result = pinewasm_vm::run(program, &data(), &options);
        assert_eq!(
            result.errors[0].kind,
            pinewasm_vm::RuntimeErrorKind::VmMemoryLimitExceeded
        );
    }
}

#[test]
fn extrema_cache_preserves_memory_admission_for_selected_window_lengths() {
    for func in ["highest", "lowest", "highestbars", "lowestbars"] {
        for default in [2, 100_000] {
            let source = format!(
                "indicator(\"memory\")\nn = input.int({default})\nplot(ta.{func}(close, n))\n"
            );
            let program = compile_ok(&source);
            for selected in [2, 100_000] {
                let options = RunOptions {
                    inputs: HashMap::from([(0, InputValue::Int(selected))]),
                    max_vm_bytes_per_run: 1024 * 1024,
                    ..RunOptions::default()
                };
                // The cache fits in the existing state stride. The one-MiB
                // budget still admits 100,000 window values.
                let result = pinewasm_vm::run(&program, &data(), &options);
                assert!(
                    result.errors.is_empty(),
                    "{func}/{default}/{selected}: {:?}",
                    result.errors
                );
                assert_eq!(result.bars_executed, data().close.len() as u32);
                if selected == 100_000 {
                    let limited = pinewasm_vm::run(
                        &program,
                        &data(),
                        &RunOptions {
                            max_vm_bytes_per_run: 512 * 1024,
                            ..options
                        },
                    );
                    assert_eq!(limited.bars_executed, 0);
                    assert_eq!(
                        limited.errors[0].kind,
                        pinewasm_vm::RuntimeErrorKind::VmMemoryLimitExceeded
                    );
                }
            }
        }
    }
}

#[test]
fn ta_input_bindings_reject_corrupt_metadata() {
    let program = compile_ok(
        "indicator(\"t\")\nn = input.int(2)\nplot(ta.sma(close, n))\nplot(ta.ema(close, n))\n",
    );
    for mutation in 0..5 {
        let mut bad = program.clone();
        match mutation {
            0 => bad.ta_input_bindings[0].input = u32::MAX,
            1 => bad.ta_input_bindings.push(bad.ta_input_bindings[0]),
            2 => {
                bad.ta_input_bindings[0].target =
                    pinewasm_hir::TaInputTarget::Sma { slot: u32::MAX }
            }
            3 => bad.header.runtime_abi_version = 0,
            _ => bad.ta_states[0].params[0] = 3,
        }
        assert!(
            pinewasm_csil::validate(&bad).is_err(),
            "mutation {mutation}"
        );
    }
}

#[test]
fn ta_input_aliases_and_helper_parameters_keep_independent_origins() {
    let source = concat!(
        "indicator(\"origins\")\na = input.int(2)\nb = input.int(3)\n",
        "alias = a\nnegative = -b\n",
        "inner(simple int length) =>\n    ta.sma(close, length)\n",
        "outer(simple int length) =>\n    inner(length)\n",
        "plot(outer(alias))\nplot(ta.change(close, -negative))\n",
        "[m, s, h] = ta.macd(close, a, b, a)\nplot(m)\nplot(s)\nplot(h)\n",
    );
    let program = compile_ok(source);
    let options = RunOptions {
        inputs: HashMap::from([(0, InputValue::Int(3)), (1, InputValue::Int(1))]),
        ..RunOptions::default()
    };
    let expected_program = compile_ok(
        &source
            .replace("a = input.int(2)", "a = input.int(3)")
            .replace("b = input.int(3)", "b = input.int(1)"),
    );
    let expected = pinewasm_vm::run(&expected_program, &data(), &RunOptions::default());
    let actual = pinewasm_vm::run(&program, &data(), &options);
    assert!(actual.errors.is_empty(), "{:?}", actual.errors);
    for (a, e) in actual.plots.iter().zip(&expected.plots) {
        assert_eq!(a.values, e.values);
    }
}

#[test]
fn secondary_context_direct_input_lengths_compile_only_for_same_feed() {
    compile_ok("indicator(\"t\")\nn = input.int(2)\nplot(request.security(syminfo.tickerid, \"60\", ta.sma(close, n)))\n");
    expect_error_code(
        "indicator(\"t\")\nn = input.int(2)\nplot(request.security(\"NASDAQ:AAPL\", \"60\", ta.sma(close, n)))\n",
        "E0244",
    );
}
