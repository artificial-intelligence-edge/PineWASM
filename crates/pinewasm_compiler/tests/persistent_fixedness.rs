use pinewasm_compiler::{compile, compile_and_run, CompileOptions, MarketData, RunOptions, Value};
use pinewasm_hir::{DisplayStyle, InputActiveExpr, InputValue, PlotColorMode, PlotColorRole};

// Producer/consumer invariants for fixed plot offsets. These cases do not
// assert general helper or explicit-qualifier behaviour.
fn market() -> MarketData {
    MarketData {
        open: vec![1.0, 2.0, 1.0, 2.0],
        high: vec![3.0; 4],
        low: vec![0.0; 4],
        close: vec![2.0, 1.0, 2.0, 1.0],
        volume: vec![1.0; 4],
        ..MarketData::default()
    }
}

fn offset_rejects_series(source: &str) {
    let compiled = compile(source, CompileOptions::default());
    assert!(!compiled.success, "unexpected fixed admission: {source}");
    let start = source.find("offset=").expect("offset consumer") as u32 + 7;
    assert!(
        compiled.diagnostics.iter().any(|d| {
            d.code.as_deref() == Some("E0206")
                && d.message.contains("argument `offset`")
                && d.message.ends_with("found series")
                && d.labels.iter().any(|label| label.span.start == start)
        }),
        "{source}: {:?}",
        compiled.diagnostics
    );
}

fn fixed_offset(style: &DisplayStyle, expected: i64) {
    assert!(!style
        .unsupported
        .iter()
        .any(|option| option.field == "offset"));
    assert!(!style
        .runtime_scalars
        .iter()
        .any(|option| option.field == "offset"));
    if let Some(binding) = style
        .bound_scalars
        .iter()
        .find(|entry| entry.field == "offset")
    {
        assert!(
            matches!(
                &binding.expression,
                InputActiveExpr::Value { value: InputValue::Int(value) } if *value == expected
            ),
            "{:?}",
            binding.expression
        );
    } else {
        assert_eq!(i64::from(style.offset), expected);
    }
}

#[test]
fn proved_helper_and_annotated_carried_reads_cannot_freeze_plot_offsets() {
    for body in ["close > open", "true", "not false", "close > close"] {
        // NonConst is a qualifier fact, not a claim that every such helper
        // changes truth value. close > close keeps the direct-control policy.
        let source = format!("//@version=6\nindicator(\"Helper fixedness\")\nf() => {body}\nvar int shift = 1\nplot(close, offset=shift)\nif f()\n    shift := 2\n");
        offset_rejects_series(&source);
    }
    for declaration in ["var simple int", "varip simple int"] {
        let source = format!("//@version=6\nindicator(\"Annotated fixedness\")\n{declaration} shift = 1\nplot(close, offset=shift)\nshift := 2\n");
        offset_rejects_series(&source);
    }
    offset_rejects_series("//@version=6\nindicator(\"Later helper definition\")\nvar int shift = 1\nplot(close, offset=shift)\nif f()\n    shift := 2\nf() => close > open\n");
    offset_rejects_series("//@version=6\nindicator(\"Annotated ternary\")\nvar simple bool enabled = true\nplot(close, offset=enabled ? 1 : 0)\nenabled := false\n");
}

#[test]
fn a_helper_reads_built_ins_its_caller_shadows_with_parameters() {
    // `readBars` resolves `close` and `open` where it is defined, so the
    // caller's same-named parameters cannot make its guard a constant. The
    // summary treats a helper call with arguments as an opaque control, so the
    // offset is refused as unproved (`E0252`) rather than as series (`E0206`).
    let source = "//@version=6\nindicator(\"Helper reads built-ins\")\nreadBars() => close > open\ncaller(float close, float open) => readBars()\nvar int shift = 1\nplot(close, offset=shift)\nif caller(0.0, 1.0)\n    shift := 2\n";
    let compiled = compile(source, CompileOptions::default());
    assert!(!compiled.success, "unexpected fixed admission: {source}");
    let start = source.find("offset=").expect("offset consumer") as u32 + 7;
    assert!(
        compiled.diagnostics.iter().any(|d| {
            d.code.as_deref() == Some("E0252")
                && d.message.contains("argument `offset`")
                && d.labels.iter().any(|label| label.span.start == start)
        }),
        "{source}: {:?}",
        compiled.diagnostics
    );
}

#[test]
fn helper_truth_cannot_weaken_a_scalar_alias_dynamic_dependency() {
    // Helper truth is scoped to controls. It must not prune the Series arm
    // while the ordinary scalar closed-fact evaluator leaves gate opaque.
    offset_rejects_series("//@version=6\nindicator(\"Dynamic alias safeguard\")\nyes() => true\nvar int shift = 1\ngate = yes() ? true : close > open\nplot(close, offset=shift)\nif gate\n    shift := 2\n");
}

#[test]
fn false_lazy_and_opaque_helper_controls_keep_their_safe_values_and_offsets() {
    for (prelude, guard) in [
        ("never() => false", "never()"),
        ("never() => not true", "never()"),
        (
            "never() => false\nchanging() => close > open",
            "never() and changing()",
        ),
        ("changing() => close > open", "false and changing()"),
        ("gate = false\nnever() => gate", "never()"),
        // The helper reads the global `gate`, never the caller's parameter.
        (
            "gate = false\nreadGate() => gate\ncaller(bool gate) => readGate()",
            "caller(true)",
        ),
        ("never() => false\nnever(bool gate) => gate", "never()"),
        ("never(bool gate = false) => gate", "never()"),
        ("never() => false\nnever = 0", "never()"),
        ("leaf() => false\nnever() => leaf()", "never()"),
        ("never() =>\n    bool local = false\n    local", "never()"),
        ("never() => 1 + 1 != 2", "never()"),
    ] {
        let source = format!("//@version=6\nindicator(\"Helper boundary safeguard\")\n{prelude}\nvar int shift = 1\nplot(close, offset=shift)\nplot(shift, title=\"before\")\nif {guard}\n    shift := 2\nplot(shift, title=\"after\")\n");
        let run = compile_and_run(
            &source,
            &market(),
            CompileOptions::default(),
            &RunOptions::default(),
        );
        assert!(
            run.compile.as_ref().expect("compile result").success,
            "{source}: {:?}",
            run.diagnostics
        );
        assert!(
            run.runtime_errors.is_empty(),
            "{source}: {:?}",
            run.runtime_errors
        );
        assert_eq!(run.bars_executed, 4);
        fixed_offset(&run.plots[0].style, 1);
        for title in ["before", "after"] {
            let plot = run
                .plots
                .iter()
                .find(|plot| plot.title.as_deref() == Some(title))
                .expect("value plot");
            assert_eq!(plot.values.len(), 4, "{source}: {title}");
            assert!(
                plot.values
                    .iter()
                    .all(|value| matches!(value, Value::Float(v) if *v == 1.0)),
                "{source}: {:?}",
                plot.values
            );
        }
    }
}

#[test]
fn readonly_stable_prefix_dead_and_ordinary_values_remain_fixed_at_their_sites() {
    for (body, expected) in [
        ("var simple int shift = 1\nplot(close, offset=shift)\nplot(shift)", 1),
        ("var int shift = 1\nshift := 2\nplot(close, offset=shift)\nplot(shift)", 2),
        ("var simple int shift = 1\nplot(close, offset=shift)\nif false\n    shift := 2\nplot(shift)", 1),
        ("never() => 1 + 1 != 2\nvar simple int shift = 1\nplot(close, offset=shift)\nif never()\n    shift := 2\nplot(shift)", 1),
        ("simple bool enabled = true\nplot(close, offset=enabled ? 1 : 0)\nplot(enabled ? 1 : 0)\nenabled := false", 1),
    ] {
        let source = format!("//@version=6\nindicator(\"Fixed producer controls\")\n{body}\n");
        let run = compile_and_run(&source, &market(), CompileOptions::default(), &RunOptions::default());
        assert!(run.compile.as_ref().expect("compile result").success, "{body}: {:?}", run.diagnostics);
        assert!(run.runtime_errors.is_empty(), "{body}: {:?}", run.runtime_errors);
        assert_eq!(run.bars_executed, 4);
        fixed_offset(&run.plots[0].style, expected);
        assert_eq!(run.plots.last().expect("value plot").values.len(), 4);
        assert!(run.plots.last().expect("value plot").values.iter().all(|value| matches!(value, Value::Float(v) if *v == expected as f64)), "{body}: {:?}", run.plots);
    }
}

#[test]
fn effectful_helpers_keep_their_effects_and_unknown_guard_boundary() {
    let source = "//@version=6\nindicator(\"Effectful helper boundary\")\nvar items = array.new_int(0)\nnever() =>\n    array.push(items, 1)\n    false\nvar int shift = 1\nplot(close, offset=shift)\nif never()\n    shift := 2\nplot(array.size(items), title=\"effects\")\n";
    let run = compile_and_run(
        source,
        &market(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        run.compile.as_ref().expect("compile result").success,
        "{:?}",
        run.diagnostics
    );
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    fixed_offset(&run.plots[0].style, 1);
    assert_eq!(
        run.plots[1].values,
        vec![
            Value::Float(1.0),
            Value::Float(2.0),
            Value::Float(3.0),
            Value::Float(4.0)
        ]
    );
}

#[test]
fn carried_input_initializers_keep_registration_defaults_order_and_execution() {
    let source = "//@version=6\nindicator(\"Carried input execution\")\nvar simple bool enabled = input.bool(true)\notherSetting = input.int(7)\nplot(enabled ? 1 : 0, title=\"before\")\nenabled := false\nplot(enabled ? 1 : 0, title=\"after\")\nplot(otherSetting, title=\"second\")\n";
    for override_value in [None, Some(false)] {
        let mut options = RunOptions::default();
        if let Some(value) = override_value {
            options.inputs.insert(0, InputValue::Bool(value));
            options.inputs.insert(1, InputValue::Int(9));
        }
        let run = compile_and_run(source, &market(), CompileOptions::default(), &options);
        let compiled = run.compile.as_ref().expect("compile result");
        assert!(compiled.success, "{:?}", compiled.diagnostics);
        assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
        assert_eq!(run.bars_executed, 4);
        let inputs = &compiled.program.as_ref().expect("program").inputs;
        assert_eq!(inputs.len(), 2);
        assert_eq!(
            (inputs[0].id, inputs[0].title.as_deref()),
            (0, Some("enabled"))
        );
        assert_eq!(
            (inputs[1].id, inputs[1].title.as_deref()),
            (1, Some("otherSetting"))
        );
        assert_eq!(inputs[0].defval, InputValue::Bool(true));
        assert_eq!(inputs[1].defval, InputValue::Int(7));
        let before = if override_value == Some(false) {
            vec![Value::Float(0.0); 4]
        } else {
            vec![
                Value::Float(1.0),
                Value::Float(0.0),
                Value::Float(0.0),
                Value::Float(0.0),
            ]
        };
        assert_eq!(run.plots[0].values, before);
        assert_eq!(run.plots[1].values, vec![Value::Float(0.0); 4]);
        let second = if override_value.is_some() { 9.0 } else { 7.0 };
        assert_eq!(run.plots[2].values, vec![Value::Float(second); 4]);
    }
}

#[test]
fn carried_annotations_do_not_retain_a_static_request_source_proof() {
    let source = "//@version=6\nstrategy(\"Carried request guard\", dynamic_requests=false)\nvar simple bool enabled = true\nfloat requested = na\nif enabled\n    requested := request.security(syminfo.tickerid, timeframe.period, close)\nenabled := false\nplot(requested)\n";
    let compiled = compile(source, CompileOptions::default());
    assert!(!compiled.success);
    assert!(
        compiled
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0242")),
        "{:?}",
        compiled.diagnostics
    );
}

#[test]
fn supported_series_color_consumers_keep_per_bar_paint() {
    // All scalar display offsets have a Simple ceiling. Color is an actual
    // Series-admitting field, with a distinct per-bar runtime color lane.
    let source = "//@version=6\nindicator(\"Carried color consumer\")\nvar simple color paint = #ff0000\nplot(close, color=paint)\npaint := #0000ff\n";
    let run = compile_and_run(
        source,
        &market(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        run.compile.as_ref().expect("compile result").success,
        "{:?}",
        run.diagnostics
    );
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    assert_eq!(
        run.plots[0].style.plot_color_mode,
        Some(PlotColorMode::Series)
    );
    let colors = &run.plots[0]
        .colors
        .iter()
        .find(|lane| lane.role == PlotColorRole::Color)
        .expect("runtime color lane")
        .values;
    assert_eq!(run.bars_executed, 4);
    assert_eq!(colors.len(), 4);
    assert!(matches!(
        &colors[0],
        Value::Color {
            r: 255,
            g: 0,
            b: 0,
            ..
        }
    ));
    assert!(colors[1..].iter().all(|value| matches!(
        value,
        Value::Color {
            r: 0,
            g: 0,
            b: 255,
            ..
        }
    )));
}

#[test]
fn declaration_ceiling_and_recursive_call_errors_keep_their_owners() {
    for (body, code) in [
        ("var simple int value = int(close)", "E0226"),
        ("var simple int value = 1\nvalue := int(close)", "E0218"),
        ("var simple int value = 1\nvalue := value + 1", "E0218"),
        ("const int value = 1\nvalue := 2", "E0217"),
        ("f() => f()\nvar int shift = 1\nplot(close, offset=shift)\nif f()\n    shift := 2", "E0224"),
        ("f() => g()\ng() => f()\nvar int shift = 1\nplot(close, offset=shift)\nif f()\n    shift := 2", "E0224"),
        ("f() => close > open\nf() => false\nvar int shift = 1\nplot(close, offset=shift)\nif f()\n    shift := 2", "E0224"),
    ] {
        let source = format!("//@version=6\nindicator(\"Existing diagnostic owners\")\n{body}\n");
        let compiled = compile(&source, CompileOptions::default());
        assert!(!compiled.success, "{body}");
        assert!(compiled.diagnostics.iter().any(|d| d.code.as_deref() == Some(code)), "{body}: {:?}", compiled.diagnostics);
        if code == "E0224" {
            assert!(!compiled.diagnostics.iter().any(|d| d.code.as_deref() == Some("E0206") && d.message.contains("argument `offset`")), "opaque call acquired an offset proof: {:?}", compiled.diagnostics);
        }
    }
}
