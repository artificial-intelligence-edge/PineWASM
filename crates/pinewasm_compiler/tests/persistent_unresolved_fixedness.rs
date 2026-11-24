use pinewasm_compiler::{
    compile, compile_and_run, CompileOptions, DisplayStyle, InputActiveExpr, InputValue,
    MarketData, RunOptions, RunResult, Severity, Value, MAX_SOURCE_LEN,
};
use pinewasm_hir::{PlotColorMode, PlotColorRole};

// Internal producer/consumer invariants for these sources.
// These four sources are fixed test inputs.
const LITERAL_TRUE: &str = "//@version=6\nindicator(\"Opaque guard residual\")\nf() => true\nvar int shift = 1\nplot(close, offset=shift)\nplot(shift, title=\"before\")\nif f()\n    shift := 2\nplot(shift, title=\"after\")\n";
const ARITHMETIC_TRUE: &str = "//@version=6\nindicator(\"Opaque guard residual\")\nf() => 1 + 1 == 2\nvar int shift = 1\nplot(close, offset=shift)\nplot(shift, title=\"before\")\nif f()\n    shift := 2\nplot(shift, title=\"after\")\n";
const ARITHMETIC_FALSE: &str = "//@version=6\nindicator(\"Opaque guard residual\")\nf() => 1 + 1 != 2\nvar int shift = 1\nplot(close, offset=shift)\nplot(shift, title=\"before\")\nif f()\n    shift := 2\nplot(shift, title=\"after\")\n";
const ARITHMETIC_SERIES: &str = "//@version=6\nindicator(\"Opaque guard residual\")\nf() => close + 0 > open\nvar int shift = 1\nplot(close, offset=shift)\nplot(shift, title=\"before\")\nif f()\n    shift := 2\nplot(shift, title=\"after\")\n";

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

fn options() -> CompileOptions {
    CompileOptions {
        emit_hir: true,
        emit_mir: true,
        emit_csil: true,
        ..CompileOptions::default()
    }
}

fn run(source: &str, run_options: &RunOptions) -> RunResult {
    let result = compile_and_run(source, &market(), options(), run_options);
    let compiled = result.compile.as_ref().expect("compile result");
    assert!(compiled.success, "{source}: {:?}", compiled.diagnostics);
    assert!(compiled.program.is_some());
    assert!(
        result.runtime_errors.is_empty(),
        "{source}: {:?}",
        result.runtime_errors
    );
    assert_eq!(result.bars_executed, 4, "{source}");
    result
}

fn values(result: &RunResult, title: &str, expected: &[f64]) {
    let found: Vec<_> = result
        .plots
        .iter()
        .filter(|plot| plot.title.as_deref() == Some(title))
        .collect();
    assert_eq!(found.len(), 1, "duplicate/missing plot {title}");
    let expected: Vec<_> = expected.iter().copied().map(Value::Float).collect();
    assert_eq!(found[0].values, expected, "complete vector for {title}");
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
    let bound: Vec<_> = style
        .bound_scalars
        .iter()
        .filter(|option| option.field == "offset")
        .collect();
    assert!(bound.len() <= 1, "duplicate offset bindings");
    if let Some(binding) = bound.first() {
        assert!(
            matches!(&binding.expression,
            InputActiveExpr::Value { value: InputValue::Int(value) } if *value == expected),
            "captured offset: {:?}",
            binding.expression
        );
    } else {
        assert_eq!(i64::from(style.offset), expected);
    }
}

// A failed compile must identify the fixed-setting use. E0252 is the local
// unsupported temporal-fixedness proof; E0206 requires a genuine Series fact.
fn rejects_fixed_setting(source: &str, argument: &str, expression: &str, code: &str) {
    let compiled = compile(source, options());
    assert!(
        !compiled.success,
        "silently fixed a carried value: {source}"
    );
    assert!(
        compiled.program.is_none(),
        "rejected source exported a program"
    );
    let needle = format!("{argument}={expression}");
    let start = source.find(&needle).expect("consumer argument") + argument.len() + 1;
    let end = start + expression.len();
    assert!(
        compiled.diagnostics.iter().any(|diagnostic| {
            let explanation = if code == "E0206" {
                diagnostic.message.ends_with("found series")
            } else {
                code == "E0252"
                    && diagnostic.message.contains("cannot prove")
                    && diagnostic.message.contains("fixed")
            };
            diagnostic.severity == Severity::Error
                && diagnostic.code.as_deref() == Some(code)
                && explanation
                && diagnostic.message.contains(argument)
                && diagnostic.labels.iter().any(|label| {
                    label.span.start as usize == start && label.span.end as usize == end
                })
        }),
        "missing consumer-owned fixedness diagnostic: {source}: {:?}",
        compiled.diagnostics
    );
}

#[test]
fn exact_arithmetic_helper_witnesses_cannot_export_stale_fixed_offsets() {
    for (source, code) in [
        (LITERAL_TRUE, "E0206"),
        (ARITHMETIC_TRUE, "E0252"),
        (ARITHMETIC_SERIES, "E0252"),
    ] {
        rejects_fixed_setting(source, "offset", "shift", code);
    }
    let result = run(ARITHMETIC_FALSE, &RunOptions::default());
    fixed_offset(&result.plots[0].style, 1);
    values(&result, "before", &[1.0; 4]);
    values(&result, "after", &[1.0; 4]);
}

#[test]
fn alias_parameter_ternary_and_switch_results_cannot_erase_pending_roots() {
    for (prelude, read) in [
        ("", "shift"),
        ("alias = shift\n", "alias"),
        ("forward(int value) => value\n", "forward(shift)"),
        ("", "shift == 1 ? 1 : 0"),
        (
            "alias = switch true\n    true => shift\n    => 0\n",
            "alias",
        ),
    ] {
        let source = format!("//@version=6\nindicator(\"Pending roots\")\nf() => 1 + 1 == 2\nvar int shift = 1\n{prelude}plot(close, offset={read})\nif f()\n    shift := 2\n");
        rejects_fixed_setting(&source, "offset", read, "E0252");
    }
}

#[test]
fn pending_values_cannot_freeze_other_fixed_display_arguments() {
    for (declaration, argument, read, write) in [
        ("var int width = 1", "linewidth", "width", "width := 2"),
        (
            "var bool routed = true",
            "force_overlay",
            "routed",
            "routed := false",
        ),
    ] {
        let source = format!("//@version=6\nindicator(\"Pending display arguments\")\nf() => 1 + 1 == 2\n{declaration}\nplot(close, {argument}={read})\nif f()\n    {write}\n");
        rejects_fixed_setting(&source, argument, read, "E0252");
    }
}

#[test]
fn arithmetic_helpers_preserve_data_only_carried_values() {
    // Removing only the fixed consumer leaves ordinary legal per-bar values.
    for source in [ARITHMETIC_TRUE, ARITHMETIC_SERIES] {
        let data_only = source.replace("plot(close, offset=shift)\n", "");
        let result = run(&data_only, &RunOptions::default());
        values(&result, "before", &[1.0, 2.0, 2.0, 2.0]);
        values(&result, "after", &[2.0; 4]);
    }
}

#[test]
fn readonly_stable_prefix_dead_and_ordinary_controls_stay_fixed() {
    for (body, expected) in [
        ("var int shift = 1\nplot(close, offset=shift)\nplot(shift, title=\"before\")\nplot(shift, title=\"after\")", 1.0),
        ("var int shift = 1\nshift := 2\nplot(close, offset=shift)\nplot(shift, title=\"before\")\nplot(shift, title=\"after\")", 2.0),
        ("var int shift = 1\nplot(close, offset=shift)\nif false\n    shift := 2\nplot(shift, title=\"before\")\nplot(shift, title=\"after\")", 1.0),
        ("int shift = 1\nplot(close, offset=shift)\nplot(shift, title=\"before\")\nshift := 2\nplot(shift - 1, title=\"after\")", 1.0),
    ] {
        let source = format!("//@version=6\nindicator(\"Fixed controls\")\n{body}\n");
        let result = run(&source, &RunOptions::default());
        fixed_offset(&result.plots[0].style, expected as i64);
        values(&result, "before", &[expected; 4]);
        values(&result, "after", &[expected; 4]);
    }
}

#[test]
fn normal_inlining_proves_false_without_dropping_helper_effects() {
    for prelude in [
        "never() => 1 + 1 != 2",
        "leaf() => 1 + 1 != 2\nnever() => leaf()",
        "never(bool answer = false) => answer",
        "never() =>\n    bool answer = false\n    answer",
    ] {
        let source = format!("//@version=6\nindicator(\"False inlined controls\")\n{prelude}\nvar int shift = 1\nplot(close, offset=shift)\nplot(shift, title=\"before\")\nif never()\n    shift := 2\nplot(shift, title=\"after\")\n");
        let result = run(&source, &RunOptions::default());
        fixed_offset(&result.plots[0].style, 1);
        values(&result, "before", &[1.0; 4]);
        values(&result, "after", &[1.0; 4]);
    }
    let source = "//@version=6\nindicator(\"False control effects\")\nvar items = array.new_int(0)\nnever() =>\n    array.push(items, 1)\n    1 + 1 != 2\nvar int shift = 1\nplot(close, offset=shift)\nplot(shift, title=\"before\")\nif never()\n    shift := 2\nplot(shift, title=\"after\")\nplot(array.size(items), title=\"effects\")\n";
    let result = run(source, &RunOptions::default());
    fixed_offset(&result.plots[0].style, 1);
    values(&result, "before", &[1.0; 4]);
    values(&result, "after", &[1.0; 4]);
    values(&result, "effects", &[1.0, 2.0, 3.0, 4.0]);
}

#[test]
fn proving_false_keeps_input_registration_order_defaults_and_overrides() {
    let source = "//@version=6\nindicator(\"Pending input origins\")\nnever() => 1 + 1 != 2\nvar int shift = input.int(1, \"Shift\")\notherSetting = input.bool(false, \"Other\")\nplot(close, offset=shift)\nplot(shift, title=\"before\")\nif never()\n    shift := 2\nplot(shift, title=\"after\")\nplot(otherSetting ? 1 : 0, title=\"other\")\n";
    for overridden in [false, true] {
        let mut run_options = RunOptions::default();
        if overridden {
            run_options.inputs.insert(0, InputValue::Int(3));
            run_options.inputs.insert(1, InputValue::Bool(true));
        }
        let result = run(source, &run_options);
        let inputs = &result
            .compile
            .as_ref()
            .expect("compile")
            .program
            .as_ref()
            .expect("program")
            .inputs;
        assert_eq!(inputs.len(), 2);
        assert_eq!(
            (inputs[0].id, inputs[0].title.as_deref()),
            (0, Some("Shift"))
        );
        assert_eq!(
            (inputs[1].id, inputs[1].title.as_deref()),
            (1, Some("Other"))
        );
        assert_eq!(inputs[0].defval, InputValue::Int(1));
        assert_eq!(inputs[1].defval, InputValue::Bool(false));
        let expected = if overridden { 3.0 } else { 1.0 };
        values(&result, "before", &[expected; 4]);
        values(&result, "after", &[expected; 4]);
        values(&result, "other", &[if overridden { 1.0 } else { 0.0 }; 4]);
        let style = &result.plots[0].style;
        assert!(!style
            .unsupported
            .iter()
            .any(|entry| entry.field == "offset"));
        assert!(!style
            .runtime_scalars
            .iter()
            .any(|entry| entry.field == "offset"));
        let offset: Vec<_> = style
            .bound_scalars
            .iter()
            .filter(|entry| entry.field == "offset")
            .collect();
        assert_eq!(offset.len(), 1);
        match &offset[0].expression {
            InputActiveExpr::Input { input } => assert_eq!(*input, 0),
            InputActiveExpr::Value {
                value: InputValue::Int(value),
            } => {
                assert_eq!(*value, if overridden { 3 } else { 1 });
            }
            expression => panic!("unresolved/non-input offset: {expression:?}"),
        }
    }
}

#[test]
fn input_defaults_cannot_discharge_a_later_write_as_false() {
    for default in ["false", "true"] {
        let source = format!("//@version=6\nindicator(\"Input guard defaults\")\nallowed = input.bool({default}, \"Allow\")\nf() => allowed\nvar int shift = 1\nplot(close, offset=shift)\nif f()\n    shift := 2\n");
        rejects_fixed_setting(&source, "offset", "shift", "E0252");
    }
}

#[test]
fn source_closed_requests_cannot_borrow_a_pending_initializer() {
    for source in [
        "//@version=6\nstrategy(\"Pending request guard\", dynamic_requests=false)\nf() => 1 + 1 == 2\nvar bool enabled = true\nrequested = close\nif enabled\n    requested := request.security(syminfo.tickerid, timeframe.period, close)\nif f()\n    enabled := false\nplot(requested)\n",
        "//@version=6\nstrategy(\"Pending request guard\", dynamic_requests=false)\nf() => 1 + 1 == 2\nvar bool enabled = true\nrequested = enabled ? request.security(syminfo.tickerid, timeframe.period, close) : close\nif f()\n    enabled := false\nplot(requested)\n",
        "//@version=6\nstrategy(\"Pending request guard\", dynamic_requests=false)\nf() => 1 + 1 == 2\nvar bool enabled = true\nrequested = enabled and request.security(syminfo.tickerid, timeframe.period, close) > 0\nif f()\n    enabled := false\nplot(requested ? 1 : 0)\n",
    ] {
        let compiled = compile(source, options());
        assert!(
            !compiled.success,
            "pending source proof admitted: {:?}",
            compiled.diagnostics
        );
        assert!(compiled.program.is_none());
        let request = "request.security(syminfo.tickerid, timeframe.period, close)";
        let start = source.find(request).expect("request expression");
        let end = start + request.len();
        assert!(
            compiled.diagnostics.iter().any(|diagnostic| {
                diagnostic.severity == Severity::Error
                    && diagnostic.code.as_deref() == Some("E0252")
                    && diagnostic.message.contains("cannot prove")
                    && diagnostic.message.contains("fixed")
                    && diagnostic.labels.iter().any(|label| {
                        label.span.start as usize == start && label.span.end as usize == end
                    })
            }),
            "missing source-owned request/fixedness diagnostic: {:?}",
            compiled.diagnostics
        );
    }
}

#[test]
fn pending_paint_cannot_silently_become_an_initializer_color() {
    for body in ["1 + 1 == 2", "close + 0 > open"] {
        let source = format!("//@version=6\nindicator(\"Pending paint\")\nf() => {body}\nvar color paint = #ff0000\nplot(close, color=paint)\nif f()\n    paint := #0000ff\n");
        let result = run(&source, &RunOptions::default());
        assert_eq!(result.plots.len(), 1);
        assert_eq!(
            result.plots[0].values,
            vec![
                Value::Float(2.0),
                Value::Float(1.0),
                Value::Float(2.0),
                Value::Float(1.0)
            ]
        );
        assert_eq!(
            result.plots[0].style.plot_color_mode,
            Some(PlotColorMode::Series)
        );
        let colors: Vec<_> = result.plots[0]
            .colors
            .iter()
            .filter(|lane| lane.role == PlotColorRole::Color)
            .collect();
        assert_eq!(colors.len(), 1);
        let red = Value::Color {
            r: 255,
            g: 0,
            b: 0,
            t: 0.0,
            input_origin: None,
        };
        let blue = Value::Color {
            r: 0,
            g: 0,
            b: 255,
            t: 0.0,
            input_origin: None,
        };
        assert_eq!(
            colors[0].values,
            vec![red, blue.clone(), blue.clone(), blue]
        );
    }
}

#[test]
fn declared_initializer_and_assignment_ceilings_keep_their_diagnostics() {
    for (body, code) in [
        ("var simple int value = int(close)", "E0226"),
        ("var simple int value = 1\nvalue := int(close)", "E0218"),
        ("const int value = 1\nvalue := 2", "E0217"),
    ] {
        let source = format!("//@version=6\nindicator(\"Existing ceilings\")\n{body}\n");
        let compiled = compile(&source, options());
        assert!(!compiled.success);
        assert!(compiled.program.is_none());
        assert!(
            compiled
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.severity == Severity::Error
                    && diagnostic.code.as_deref() == Some(code)),
            "{body}: {:?}",
            compiled.diagnostics
        );
    }
}

#[test]
fn input_branch_joins_keep_pending_roots_and_while_rejections_keep_their_owner() {
    // The default is not proof that the alias never reads shift: selecting
    // true enables that path, so the cross-bar value can change.
    for default in ["false", "true"] {
        for (selection, code) in [
            ("int alias = 0\nif choose\n    alias := shift\nelse\n    alias := 0", "E0252"),
            ("alias = if choose\n    shift\nelse\n    0", "E0252"),
            ("alias = switch choose\n    true => shift\n    => 0", "E0252"),
            // While-body reassignments already strengthen alias to Series.
            // This zero/one-iteration case keeps the existing E0206 diagnostic;
            // it is not evidence that a loop-result expression is supported.
            ("int alias = 0\ncounter = 0\nwhile choose and counter < 1\n    alias := shift\n    counter := counter + 1", "E0206"),
        ] {
            let source = format!("//@version=6\nindicator(\"Pending alias joins\")\nchoose = input.bool({default}, \"Read carried value\")\nf() => 1 + 1 == 2\nvar int shift = 1\n{selection}\nplot(close, offset=alias)\nif f()\n    shift := 2\n");
            rejects_fixed_setting(&source, "offset", "alias", code);
        }
    }
}

#[test]
fn ignored_pending_arguments_keep_constant_returns_and_eager_effects() {
    let simple = "//@version=6\nindicator(\"Ignored pending argument\")\nignore(int value) => 1\nf() => 1 + 1 == 2\nvar int shift = 1\nplot(close, offset=ignore(shift))\nplot(shift, title=\"before\")\nif f()\n    shift := 2\nplot(shift, title=\"after\")\n";
    let result = run(simple, &RunOptions::default());
    fixed_offset(&result.plots[0].style, 1);
    values(&result, "before", &[1.0, 2.0, 2.0, 2.0]);
    values(&result, "after", &[2.0; 4]);

    // echo's value is ignored, but its argument evaluation still pushes the
    // pre-write value once on each bar. Dropping or repeating that evaluation
    // is not a lawful way to recover the independent constant return.
    let effectful = "//@version=6\nindicator(\"Ignored pending argument effects\")\nvar observed = array.new_int(0)\necho(int value) =>\n    array.push(observed, value)\n    value\nignore(int value) => 1\nf() => close + 0 > open\nvar int shift = 1\nplot(close, offset=ignore(echo(shift)))\nplot(shift, title=\"before\")\nif f()\n    shift := 2\nplot(shift, title=\"after\")\nplot(array.size(observed), title=\"effects\")\nplot(array.get(observed, array.size(observed) - 1), title=\"argument\")\n";
    let result = run(effectful, &RunOptions::default());
    fixed_offset(&result.plots[0].style, 1);
    values(&result, "before", &[1.0, 2.0, 2.0, 2.0]);
    values(&result, "after", &[2.0; 4]);
    values(&result, "effects", &[1.0, 2.0, 3.0, 4.0]);
    values(&result, "argument", &[1.0, 2.0, 2.0, 2.0]);
}

#[test]
fn helper_local_persistent_returns_preserve_temporal_fixedness_and_false_controls() {
    for declaration in ["var int", "var simple int"] {
        let function = format!(
            "f() =>\n    {declaration} shift = 1\n    early = shift\n    shift := 2\n    early\n"
        );
        let fixed_source = format!("//@version=6\nindicator(\"Helper local fixedness\")\n{function}plot(close, offset=f())\n");
        rejects_fixed_setting(&fixed_source, "offset", "f()", "E0252");

        // A different source with one call site exercises the legal data
        // lane. Calling f twice in one program could allocate different
        // persistent locals, so it would not validate the same value.
        let data_source = format!("//@version=6\nindicator(\"Helper local data\")\n{function}plot(f(), title=\"local\")\n");
        let result = run(&data_source, &RunOptions::default());
        values(&result, "local", &[1.0, 2.0, 2.0, 2.0]);

        for (prelude, guard) in [("", "false"), ("never() => 1 + 1 != 2\n", "never()")] {
            let source = format!("//@version=6\nindicator(\"Helper local false control\")\n{prelude}f() =>\n    {declaration} shift = 1\n    early = shift\n    if {guard}\n        shift := 2\n    early\nplot(close, offset=f())\n");
            let result = run(&source, &RunOptions::default());
            assert_eq!(result.plots.len(), 1);
            fixed_offset(&result.plots[0].style, 1);
        }
    }

    let effectful = "//@version=6\nindicator(\"Helper local false effects\")\nvar observed = array.new_int(0)\nnever() =>\n    array.push(observed, 1)\n    1 + 1 != 2\nf() =>\n    var simple int shift = 1\n    early = shift\n    if never()\n        shift := 2\n    early\nplot(close, offset=f())\nplot(array.size(observed), title=\"effects\")\n";
    let result = run(effectful, &RunOptions::default());
    fixed_offset(&result.plots[0].style, 1);
    values(&result, "effects", &[1.0, 2.0, 3.0, 4.0]);

    for (body, code) in [
        (
            "    var simple int shift = int(close)\n    shift\n",
            "E0226",
        ),
        (
            "    var simple int shift = 1\n    shift := int(close)\n    shift\n",
            "E0218",
        ),
    ] {
        let source = format!("//@version=6\nindicator(\"Helper local ceilings\")\nf() =>\n{body}plot(f(), title=\"local\")\n");
        let compiled = compile(&source, options());
        assert!(!compiled.success);
        assert!(compiled.program.is_none());
        assert!(
            compiled
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.severity == Severity::Error
                    && diagnostic.code.as_deref() == Some(code)),
            "{source}: {:?}",
            compiled.diagnostics
        );
    }
}

fn rejects_fixed_setting_with_any_temporal_owner(source: &str, argument: &str, expression: &str) {
    let compiled = compile(source, options());
    assert!(!compiled.success, "unexpected fixed admission: {source}");
    assert!(compiled.program.is_none());
    let needle = format!("{argument}={expression}");
    let start = source.find(&needle).expect("consumer argument") + argument.len() + 1;
    let end = start + expression.len();
    assert!(
        compiled.diagnostics.iter().any(|diagnostic| {
            matches!(diagnostic.code.as_deref(), Some("E0252" | "E0206"))
                && diagnostic.message.contains(argument)
                && diagnostic.labels.iter().any(|label| {
                    label.span.start as usize == start && label.span.end as usize == end
                })
        }),
        "missing consumer-owned temporal diagnostic: {source}: {:?}",
        compiled.diagnostics
    );
}

#[test]
fn simple_runtime_style_scalar_smoke_resolves_a_constant_overwrite() {
    // This smoke covers only the style DTO path. It has no carried read before
    // the write and does not cover admitting a carried root.
    let source = r#"//@version=6
indicator("Simple style runtime scalar")
simple int shift = 1
shift := 2
plot(close, offset=shift)
"#;
    let compiled = compile(source, options());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    let program = compiled.program.as_ref().expect("compiled program");
    let raw = &program.plots[0].style;
    assert_eq!(raw.offset, 0, "style_i32 uses its literal fallback");
    let runtime_offsets: Vec<_> = raw
        .runtime_scalars
        .iter()
        .filter(|binding| binding.field == "offset")
        .collect();
    assert_eq!(runtime_offsets.len(), 1, "raw RuntimeStyleScalar: {raw:?}");
    assert_eq!(
        runtime_offsets[0].qualifier,
        pinewasm_hir::Qualifier::Simple
    );
    let compiled_style_json = serde_json::to_value(raw).expect("compiled style serializes");
    assert_eq!(
        compiled_style_json["runtimeScalars"][0]["qualifier"],
        "Simple"
    );
    assert!((runtime_offsets[0].slot.0 as usize) < program.series_slots.len());
    assert!(!raw
        .bound_scalars
        .iter()
        .any(|binding| binding.field == "offset"));
    assert!(!raw
        .unsupported
        .iter()
        .any(|option| option.field == "offset"));

    let result = run(source, &RunOptions::default());
    let resolved = &result.plots[0].style;
    assert!(resolved.runtime_scalars.is_empty());
    assert!(!resolved
        .unsupported
        .iter()
        .any(|option| option.field == "offset"));
    let resolved_offsets: Vec<_> = resolved
        .bound_scalars
        .iter()
        .filter(|binding| binding.field == "offset")
        .collect();
    assert_eq!(
        resolved_offsets.len(),
        1,
        "resolved BoundStyleScalar: {resolved:?}"
    );
    assert!(matches!(
        &resolved_offsets[0].expression,
        InputActiveExpr::Value {
            value: InputValue::Int(2)
        }
    ));
    fixed_offset(resolved, 2);
}

#[test]
fn a_carried_read_then_reset_remains_series_at_the_fixed_use() {
    let source = r#"//@version=6
indicator("Carried read before reset")
var simple int shift = 1
plot(shift, title="before")
shift := 2
plot(close, offset=shift)
"#;
    rejects_fixed_setting_with_any_temporal_owner(source, "offset", "shift");
}

#[test]
fn aliases_do_not_bypass_the_carried_root_qualifier() {
    let before_reset = r#"//@version=6
indicator("Alias before reset")
var simple int shift = 1
offsetAlias = shift
shift := 2
plot(close, offset=offsetAlias)
"#;
    rejects_fixed_setting_with_any_temporal_owner(before_reset, "offset", "offsetAlias");

    let after_reset = r#"//@version=6
indicator("Alias after reset with carried read")
var simple int shift = 1
plot(shift, title="before")
shift := 2
offsetAlias = shift
plot(close, offset=offsetAlias)
"#;
    rejects_fixed_setting_with_any_temporal_owner(after_reset, "offset", "offsetAlias");
}

#[test]
fn a_fixed_use_before_an_unconditional_reset_remains_rejected() {
    let source = r#"//@version=6
indicator("Fixed use before reset")
var simple int shift = 1
plot(close, offset=shift)
shift := 2
plot(close)
"#;
    rejects_fixed_setting_with_any_temporal_owner(source, "offset", "shift");
}

#[test]
fn conditional_and_loop_writes_do_not_establish_fixed_use_values() {
    let conditional = r#"//@version=6
indicator("Conditional fixed use")
var int shift = 1
if close > open
    shift := 2
plot(close, offset=shift)
"#;
    rejects_fixed_setting(conditional, "offset", "shift", "E0206");

    let looped = r#"//@version=6
indicator("Loop fixed use")
var int shift = 1
for index = 0 to 1
    shift := 2
plot(close, offset=shift)
"#;
    rejects_fixed_setting(looped, "offset", "shift", "E0206");
}

#[test]
fn fixed_values_survive_loops_while_persistent_var_updates_remain_data_only() {
    let source = r#"//@version=6
indicator("Fixed and var values across a loop")
const int fixed = 2
var int shift = 1
plot(close, offset=fixed)
plot(shift, title="before")
for index = 0 to 1
    shift := 2
plot(shift, title="after")
"#;
    let result = run(source, &RunOptions::default());
    fixed_offset(&result.plots[0].style, 2);
    values(&result, "before", &[1.0, 2.0, 2.0, 2.0]);
    values(&result, "after", &[2.0; 4]);
}

#[test]
fn ternary_and_lazy_hir_branches_do_not_leak_reset_markers() {
    let ternary = r#"//@version=6
indicator("Ternary reset control")
reset(bool write) =>
    var simple int local = 1
    prior = local
    if write
        local := 2
    local
choose = input.bool(true, "Write local")
plot(close, offset=choose ? reset(choose) : 0)
"#;
    rejects_fixed_setting_with_any_temporal_owner(ternary, "offset", "choose ? reset(choose) : 0");

    let lazy = r#"//@version=6
indicator("Lazy reset control")
reset(bool write) =>
    var simple int local = 1
    prior = local
    if write
        local := 2
    local
choose = input.bool(false, "Write local")
condition = choose or reset(choose) == 2
plot(close, offset=condition ? 1 : 2)
"#;
    rejects_fixed_setting_with_any_temporal_owner(lazy, "offset", "condition ? 1 : 2");
}

#[test]
fn helper_capture_before_opaque_arithmetic_write_guard_is_a_temporal_root() {
    let source = r#"//@version=6
indicator("Opaque arithmetic guard captured global")
var int shift = 1
readShift() => shift
captured = readShift()
if close + 0 > open
    shift := 2
plot(close, offset=captured)
"#;
    rejects_fixed_setting_with_any_temporal_owner(source, "offset", "captured");
}

#[test]
fn helper_capture_before_input_guard_write_is_a_temporal_root() {
    let source = r#"//@version=6
indicator("Input guard captured global")
var int shift = 1
readShift() => shift
captured = readShift()
enabled = input.bool(true, "Write shift")
if enabled
    shift := 2
plot(close, offset=captured)
"#;
    rejects_fixed_setting_with_any_temporal_owner(source, "offset", "captured");
}

#[test]
fn many_closed_helper_locals_remain_analyzable_under_the_source_bound() {
    let mut source = String::from("//@version=6\nindicator(\"Temporal bookkeeping\")\n");
    for function in 0..64 {
        source.push_str(&format!("helper{function}() =>\n"));
        for local in 0..100 {
            source.push_str(&format!("    value{local} = {local}\n"));
        }
        source.push_str("    value99\n");
    }
    source.push_str("plot(");
    for function in 0..64 {
        if function > 0 {
            source.push_str(" + ");
        }
        source.push_str(&format!("helper{function}()"));
    }
    source.push_str(")\n");
    assert!(source.len() < MAX_SOURCE_LEN);
    let result = compile(&source, CompileOptions::default());
    assert!(
        result.success,
        "many closed helper locals failed analysis: {:?}",
        result.diagnostics
    );
}
