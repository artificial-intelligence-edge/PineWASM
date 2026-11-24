//! Request checks need source tracking, separately from runtime input bindings.
//!
//! Only the first test checks that two source bodies are rejected at the
//! request call with E0242. The remaining tests are local regression checks.
//! Literal-backed Simple controls remain in `non_dynamic_request_context.rs`;
//! rendering metadata expectations below use its chart-context expression protocol.

use pinewasm_compiler::{
    compile, compile_with_libraries, CompileOptions, CompileResult, InputActiveBinaryOp,
    InputActiveExpr, InputActiveUnaryOp, InputValue, Program, SourceId,
};
use sha2::{Digest, Sha256};

const REQUEST: &str = "request.security(syminfo.tickerid, \"1W\", close)";

// Request call inside a chart-type conditional; this compiler rejects it (E0242).
const CHART_IF_CASE: &str = r#"//@version=6
strategy("Chart context request lead", dynamic_requests=false)
result = if chart.is_standard
    request.security(syminfo.tickerid, "1W", close)
else
    close
plot(result)
"#;

// Request call inside a theme switch; this compiler rejects it (E0242).
const THEME_SWITCH_CASE: &str = r#"//@version=6
strategy("Chart context request lead", dynamic_requests=false)
result = switch chart.bg_color
    #ffffff => request.security(syminfo.tickerid, "1W", close)
    => close
plot(result)
"#;

fn program(body: &str, dynamic: bool) -> String {
    format!(
        "//@version=6\nstrategy(\"Request source provenance\", dynamic_requests={dynamic})\n{body}\n"
    )
}

fn guarded(prelude: &str, condition: &str, dynamic: bool) -> String {
    program(
        &format!(
            "{prelude}\nresult = if {condition}\n    {REQUEST}\nelse\n    close\nplot(result)"
        ),
        dynamic,
    )
}

fn assert_request_result(result: &CompileResult, source: &str, rejected_requests: &[usize]) {
    let errors: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.is_error())
        .collect();
    assert_eq!(
        result.success,
        rejected_requests.is_empty(),
        "{source}\n{:?}",
        result.diagnostics
    );
    assert_eq!(
        errors.len(),
        rejected_requests.len(),
        "{source}: {errors:?}"
    );
    let calls: Vec<_> = source
        .match_indices(REQUEST)
        .map(|(start, _)| start)
        .collect();
    for (diagnostic, request_index) in errors.iter().zip(rejected_requests) {
        assert_eq!(diagnostic.code.as_deref(), Some("E0242"), "{source}");
        let start = calls[*request_index] as u32;
        let span = diagnostic.primary_span().expect("local request-call span");
        assert_eq!(span.source, SourceId::MAIN, "{source}");
        assert_eq!(
            (span.start, span.end),
            (start, start + REQUEST.len() as u32),
            "{source}"
        );
    }
}

fn assert_admission(source: &str, rejected_requests: &[usize]) -> CompileResult {
    let result = compile(source, CompileOptions::default());
    assert_request_result(&result, source, rejected_requests);
    result
}

fn compile_ok(source: &str) -> Program {
    assert_admission(source, &[])
        .program
        .expect("program on successful compile")
}

#[test]
fn exact_host_controls_reject_at_local_request_call_spans() {
    for (source, hash, span) in [
        (
            CHART_IF_CASE,
            "7af6ab89adbaa6fa2f19eaeac4b15cac95b0c6b5b77185852894ff6f66c60392",
            (110, 157),
        ),
        (
            THEME_SWITCH_CASE,
            "ba02bf7b670d45277181d1395c6b471bb40106e30d10924b1d391a1922586e80",
            (122, 169),
        ),
    ] {
        let digest = Sha256::digest(source.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(digest, hash);
        let result = assert_admission(source, &[0]);
        let local = result.diagnostics.iter().find(|d| d.is_error()).unwrap();
        let actual = local.primary_span().unwrap();
        // Half-open UTF-8 byte ranges come from the request AST span, not from
        // line/column conversion.
        assert_eq!((actual.start, actual.end), span);
    }
}

#[test]
fn every_chart_type_predicate_remains_host_context_despite_its_literal_value() {
    for predicate in [
        "chart.is_standard",
        "chart.is_heikinashi",
        "chart.is_kagi",
        "chart.is_linebreak",
        "chart.is_pnf",
        "chart.is_range",
        "chart.is_renko",
    ] {
        assert_admission(&guarded("", predicate, false), &[0]);
    }
}

#[test]
fn host_boolean_aliases_unary_casts_and_supported_scalar_calls_do_not_gain_closure() {
    for (prelude, condition) in [
        (
            "first = chart.is_standard\ncontextAliasFlag = first",
            "contextAliasFlag",
        ),
        (
            "simple bool first = chart.is_standard\ncontextAliasFlag = first",
            "not (not contextAliasFlag)",
        ),
        ("", "bool(chart.is_standard)"),
        ("", "chart.is_standard ? true : false"),
        (
            "value = chart.is_standard ? 1 : 0\nsimple int score = math.max(value, 0)",
            "score > 0",
        ),
    ] {
        assert_admission(&guarded(prelude, condition, false), &[0]);
    }
}

#[test]
fn theme_subjects_aliases_and_casts_reject_under_both_host_color_configurations() {
    for color in ["chart.bg_color", "chart.fg_color"] {
        for (prelude, subject) in [
            (String::new(), color.to_string()),
            (format!("held = {color}\nalias = held\n"), "alias".into()),
            (String::new(), format!("color({color})")),
        ] {
            let source = program(
                &format!(
                    "{prelude}result = switch {subject}\n    #ffffff => {REQUEST}\n    => close\nplot(result)"
                ),
                false,
            );
            for (background, foreground) in
                [([255, 255, 255], [0, 0, 0]), ([17, 34, 51], [68, 85, 102])]
            {
                let result = compile(
                    &source,
                    CompileOptions {
                        chart_background: Some(background),
                        chart_foreground: Some(foreground),
                        ..CompileOptions::default()
                    },
                );
                assert_request_result(&result, &source, &[0]);
            }
        }
    }
}

#[test]
fn hidden_subjects_and_earlier_switch_arms_keep_request_control_provenance() {
    for switch in [
        format!("result = switch chart.is_standard\n    true => {REQUEST}\n    => close"),
        format!("result = switch chart.is_standard\n    false => close\n    true => {REQUEST}\n    => close"),
        format!("result = switch chart.is_standard\n    true => close\n    => {REQUEST}"),
        format!("result = switch\n    chart.is_standard => close\n    true => {REQUEST}\n    => close"),
    ] {
        assert_admission(&program(&format!("{switch}\nplot(result)"), false), &[0]);
    }
}

#[test]
fn switch_result_origins_require_closed_conditions_and_closed_values() {
    for prelude in [
        "gate = switch chart.bg_color\n    #ffffff => true\n    => false",
        "gate = switch chart.fg_color\n    #000000 => true\n    => false",
        "gate = switch true\n    true => input.bool(true)\n    => false",
    ] {
        assert_admission(&guarded(prelude, "gate", false), &[0]);
    }
    let source = program(
        &format!(
            "held = switch true\n    true => chart.bg_color\n    => #000000\nresult = switch held\n    #ffffff => {REQUEST}\n    => close\nplot(result)"
        ),
        false,
    );
    assert_admission(&source, &[0]);
    assert_admission(
        &guarded(
            "gate = switch true\n    true => true\n    => false",
            "gate",
            false,
        ),
        &[],
    );
}

#[test]
fn literal_simple_initializers_and_const_scalar_combinations_remain_admissible() {
    for (prelude, condition) in [
        ("const bool enabled = true", "enabled"),
        (
            "simple bool enabled = true\nalias = enabled",
            "not (not alias)",
        ),
        ("simple int value = math.max(1, 2)", "value == 2"),
        (
            "simple int value = 1\nscore = math.sign(value)",
            "score > 0",
        ),
        ("simple bool enabled = true", "enabled ? true : false"),
        ("", "bool(1)"),
    ] {
        assert_admission(&guarded(prelude, condition, false), &[]);
    }
    let source = program(
        &format!(
            "simple string choice = \"a\"\nresult = switch choice\n    \"a\" => {REQUEST}\n    => close\nplot(result)"
        ),
        false,
    );
    assert_admission(&source, &[]);
}

#[test]
fn declared_simple_parameters_capture_the_argument_proof_before_strengthening() {
    let identity = "identity(simple bool enabled) => enabled\nforward(simple bool enabled) => identity(enabled)";
    for (argument, rejected) in [("true", false), ("chart.is_standard", true)] {
        let prelude = format!("{identity}\nselected = forward({argument})");
        assert_admission(
            &guarded(&prelude, "selected", false),
            if rejected { &[0] } else { &[] },
        );
    }
    // Separate inlined calls and same-spelled parameters have unique symbol
    // ids. A chart-context argument must not inherit the preceding literal call's fact,
    // and its taint must not erase the following literal call's fact.
    let source = program(
        &format!(
            "{identity}\nfirst = identity(true)\nhost = identity(chart.is_standard)\nlast = identity(true)\na = if first\n    {REQUEST}\nelse\n    close\nb = if host\n    {REQUEST}\nelse\n    close\nc = if last\n    {REQUEST}\nelse\n    close\nplot(a + b + c)"
        ),
        false,
    );
    assert_admission(&source, &[1]);
    for (argument, rejected) in [
        ("#ffffff", false),
        ("chart.bg_color", true),
        ("chart.fg_color", true),
    ] {
        let source = program(
            &format!(
                "identity(simple color theme) => theme\nselected = identity({argument})\nresult = switch selected\n    #ffffff => {REQUEST}\n    => close\nplot(result)"
            ),
            false,
        );
        assert_admission(&source, if rejected { &[0] } else { &[] });
    }
}

#[test]
fn library_global_and_parameter_origins_survive_namespace_swaps_without_laundering() {
    let library = r#"//@version=6
library("Request guards")
simple bool localEnabled = true
simple bool hostEnabled = chart.is_standard
export literal() => localEnabled
export host() => hostEnabled
export forward(simple bool enabled) => enabled
"#;
    for (expression, rejected) in [
        ("guards.literal()", false),
        ("guards.host()", true),
        ("guards.forward(true)", false),
        ("guards.forward(chart.is_standard)", true),
    ] {
        let source = guarded(
            &format!("import audit/RequestGuards/1 as guards\nselected = {expression}"),
            "selected",
            false,
        );
        let result = compile_with_libraries(
            &source,
            &[("audit/RequestGuards/1".into(), library.into())],
            CompileOptions::default(),
        );
        assert_request_result(&result, &source, if rejected { &[0] } else { &[] });
    }
}

#[test]
fn reassignment_and_persistent_argument_exclusion_do_not_reuse_closed_facts() {
    for prelude in [
        "simple bool enabled = true\nenabled := chart.is_standard",
        "simple bool enabled = true\nif chart.is_standard\n    enabled := false",
    ] {
        assert_admission(&guarded(prelude, "enabled", false), &[0]);
    }
    for persistence in ["var", "varip"] {
        let prelude = format!(
            "identity(value) => value\n{persistence} bool stored = chart.is_standard\nselected = identity(stored)"
        );
        assert_admission(&guarded(&prelude, "selected", false), &[0]);
    }
}

#[test]
fn default_only_global_dynamic_and_loop_controls_retain_their_existing_policy() {
    for body in [
        format!("result = switch chart.is_standard\n    => {REQUEST}\nplot(result)"),
        format!("result = switch chart.bg_color\n    => {REQUEST}\nplot(result)"),
        format!("result = switch\n    => {REQUEST}\nplot(result)"),
        format!("result = {REQUEST}\nplot(result)"),
    ] {
        assert_admission(&program(&body, false), &[]);
    }
    assert_admission(&guarded("", "chart.is_standard", true), &[]);
    assert_admission(&guarded("gate = input.bool(true)", "gate", true), &[]);
    let loop_source = program(
        &format!(
            "for i = 0 to 1\n    simple bool enabled = true\n    selected = if enabled\n        {REQUEST}\n    else\n        close\nplot(close)"
        ),
        false,
    );
    assert_admission(&loop_source, &[0]);
}

#[test]
fn malformed_switches_and_failed_helper_arguments_restore_later_request_context() {
    for prelude in [
        "bad = switch chart.bg_color\n    #ffffff => missing\n    => close",
        "identity(simple bool enabled) => enabled\nbad = identity(\"wrong type\")",
    ] {
        let source = guarded(prelude, "true", false);
        let result = compile(&source, CompileOptions::default());
        assert!(!result.success, "{source}");
        assert!(
            result
                .diagnostics
                .iter()
                .any(pinewasm_compiler::Diagnostic::is_error),
            "{source}"
        );
        assert!(
            result
                .diagnostics
                .iter()
                .all(|d| d.code.as_deref() != Some("E0242")),
            "request context leaked after an earlier error: {source}\n{:?}",
            result.diagnostics
        );
    }
}

#[test]
fn request_source_proof_does_not_change_series_only_call_scheduling() {
    for (condition, warns) in [
        ("chart.is_standard", false),
        ("not chart.is_standard", false),
        ("close > open", true),
    ] {
        let source = program(
            &format!(
                "result = if {condition}\n    ta.sma(close, 2)\nelse\n    close\nplot(result)"
            ),
            false,
        );
        let result = assert_admission(&source, &[]);
        assert_eq!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code.as_deref() == Some("W0004")),
            warns,
            "{source}: {:?}",
            result.diagnostics
        );
    }
}

fn value(value: InputValue) -> InputActiveExpr {
    InputActiveExpr::Value { value }
}

#[test]
fn theme_switch_host_bindings_preserve_the_old_literal_value_tree_and_defaults() {
    for (color, rgb, expected_default) in [
        ("chart.bg_color", [17, 34, 51], true),
        ("chart.fg_color", [68, 85, 102], false),
    ] {
        let source = format!(
            "//@version=6\nindicator(\"Host binding compatibility\")\ngate = switch {color}\n    #112233 => true\n    => false\namount = input.int(3, \"Amount\", active=gate)\nplot(amount)\n"
        );
        let result = compile(
            &source,
            CompileOptions {
                chart_background: Some([17, 34, 51]),
                chart_foreground: Some([68, 85, 102]),
                ..CompileOptions::default()
            },
        );
        assert_request_result(&result, &source, &[]);
        let program = result.program.unwrap();
        assert_eq!(program.inputs.len(), 1);
        let input = &program.inputs[0];
        assert_eq!(input.id, 0);
        assert_eq!(input.title.as_deref(), Some("Amount"));
        assert_eq!(input.defval, InputValue::Int(3));
        assert_eq!(input.active.default, expected_default);
        assert!(input.active.dependencies.is_empty());
        assert_eq!(input.active.direct, None);
        assert_eq!(
            input.active.expression,
            Some(InputActiveExpr::Ternary {
                condition: Box::new(InputActiveExpr::Binary {
                    op: InputActiveBinaryOp::Eq,
                    lhs: Box::new(value(InputValue::Color {
                        r: rgb[0],
                        g: rgb[1],
                        b: rgb[2],
                        t: 0.0
                    })),
                    rhs: Box::new(value(InputValue::Color {
                        r: 17,
                        g: 34,
                        b: 51,
                        t: 0.0
                    })),
                }),
                when_true: Box::new(value(InputValue::Bool(true))),
                when_false: Box::new(value(InputValue::Bool(false))),
            }),
            "the request proof must not erase the earlier host Value origin"
        );
    }
}

#[test]
fn existing_reactive_input_ids_defaults_and_direct_binding_shapes_are_unchanged() {
    // Source for the reactive input metadata regression.
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
    assert_eq!(
        program.inputs.iter().map(|i| i.id).collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4, 5]
    );
    assert_eq!(
        program
            .inputs
            .iter()
            .map(|i| i.title.as_deref())
            .collect::<Vec<_>>(),
        vec![
            Some("enabled"),
            Some("length"),
            Some("mode"),
            Some("smoothing"),
            Some("fixed"),
            Some("")
        ]
    );
    assert_eq!(
        program.inputs.iter().map(|i| &i.defval).collect::<Vec<_>>(),
        vec![
            &InputValue::Bool(false),
            &InputValue::Int(14),
            &InputValue::String("Off".into()),
            &InputValue::Int(3),
            &InputValue::Int(1),
            &InputValue::Int(2)
        ]
    );
    assert_eq!(program.inputs[1].active.dependencies, vec![0]);
    assert!(program.inputs[1].active.default);
    assert_eq!(
        program.inputs[1].active.expression,
        Some(InputActiveExpr::Unary {
            op: InputActiveUnaryOp::Not,
            operand: Box::new(InputActiveExpr::Input { input: 0 }),
        })
    );
    let direct = program.inputs[1]
        .active
        .direct
        .expect("existing direct inversion");
    assert_eq!((direct.input, direct.inverted), (0, true));
    assert!(!program.inputs[3].active.default);
    assert_eq!(program.inputs[3].active.dependencies, vec![2]);
    assert_eq!(program.inputs[3].active.direct, None);
    assert_eq!(
        program.inputs[3].active.expression,
        Some(InputActiveExpr::Binary {
            op: InputActiveBinaryOp::Eq,
            lhs: Box::new(InputActiveExpr::Input { input: 2 }),
            rhs: Box::new(value(InputValue::String("On".into()))),
        })
    );
    assert!(!program.inputs[4].active.default);
    assert!(program.inputs[4].active.dependencies.is_empty());
}

#[test]
fn frozen_input_switch_style_bindings_keep_field_order_and_input_references() {
    // Source for the input style binding regression.
    let program = compile_ok(
        r#"//@version=6
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
"#,
    );
    assert_eq!(
        program
            .inputs
            .iter()
            .map(|input| input.id)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(program.plots.len(), 2);
    assert!(program
        .plots
        .iter()
        .all(|plot| plot.style.unsupported.is_empty()));
    let bindings = &program.plots[0].style.bound_scalars;
    assert_eq!(
        bindings
            .iter()
            .map(|binding| binding.field.as_str())
            .collect::<Vec<_>>(),
        vec!["style", "display", "linestyle"]
    );
    assert_eq!(
        bindings[0].expression,
        InputActiveExpr::Ternary {
            condition: Box::new(InputActiveExpr::Binary {
                op: InputActiveBinaryOp::Eq,
                lhs: Box::new(InputActiveExpr::Input { input: 2 }),
                rhs: Box::new(value(InputValue::String("Histogram".into()))),
            }),
            when_true: Box::new(value(InputValue::String("plot.style_histogram".into()))),
            when_false: Box::new(value(InputValue::String("plot.style_line".into()))),
        }
    );
    let marker = &program.plots[1].style.bound_scalars;
    assert_eq!(marker.len(), 1);
    assert_eq!(marker[0].field, "offset");
    assert_eq!(
        marker[0].expression,
        InputActiveExpr::Ternary {
            condition: Box::new(InputActiveExpr::Input { input: 0 }),
            when_true: Box::new(value(InputValue::Int(0))),
            when_false: Box::new(InputActiveExpr::Unary {
                op: InputActiveUnaryOp::Neg,
                operand: Box::new(value(InputValue::Int(1))),
            }),
        }
    );
    let runtime = &program.plots[0].style.runtime_scalars;
    assert_eq!(runtime.len(), 1);
    assert_eq!(runtime[0].field, "linewidth");
    assert_eq!(runtime[0].qualifier, pinewasm_hir::Qualifier::Input);
    assert!(program.plots[1].style.runtime_scalars.is_empty());
}

// Simple switch-result regressions. The source below is accepted by this
// compiler. All remaining cases are bounded local policy checks.
const SIMPLE_SWITCH_RESULT_CASE: &str = r#"//@version=6
strategy("Simple switch request provenance", dynamic_requests=false)
simple bool selected = true
gate = switch selected
    true => true
    => false
result = if gate
    request.security(syminfo.tickerid, "1W", close)
else
    close
plot(result)
"#;

#[test]
fn exact_source_closed_simple_switch_result_is_accepted() {
    assert_eq!(SIMPLE_SWITCH_RESULT_CASE.len(), 260);
    let digest = Sha256::digest(SIMPLE_SWITCH_RESULT_CASE.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(
        digest,
        "3a4b02d0b7bac2087a091ae66245be4e6c847b4ac37369f1bb1726d759c8642e"
    );
    assert_admission(SIMPLE_SWITCH_RESULT_CASE, &[]);
}

#[test]
fn derived_simple_switch_subject_arm_and_default_shapes_keep_request_admission() {
    // Bounded derived shapes; missing defaults add no new acceptance rule.
    for (prelude, condition) in [
        ("simple bool selected = true\ngate = switch\n    selected => true\n    => false", "gate"),
        ("simple string selected = \"a\"\ngate = switch selected\n    \"a\" => true\n    => false", "gate"),
        ("simple int selected = 2\ngate = switch selected\n    2 => true\n    => false", "gate"),
        ("simple float selected = 2.0\ngate = switch selected\n    2.0 => true\n    => false", "gate"),
        ("simple color selected = #112233\ngate = switch selected\n    #112233 => true\n    => false", "gate"),
        ("simple bool yes = true\nsimple bool no = false\ngate = switch true\n    true => yes\n    => no", "gate"),
        ("simple bool selected = true\ngate = switch selected\n    false => true", "gate"),
        ("simple int selected = 1\nchosen = switch selected\n    2 => 7", "chosen > 0"),
        ("simple bool chosen = true\ngate = switch close > open\n    => chosen", "gate"),
    ] {
        assert_admission(&guarded(prelude, condition, false), &[]);
    }
}

#[test]
fn derived_simple_result_aliases_and_sequential_captures_keep_source_proof() {
    // Sequential captured results are distinct from an inline nested arm block.
    for (prelude, condition) in [
        ("simple bool selected = true\ngate = switch selected\n    true => true\n    => false\nfirst = gate\nalias = first", "alias"),
        ("simple bool selected = true\ngate = switch selected\n    true => true\n    => false\nsimple bool alias = gate", "alias"),
        ("simple bool selected = true\nfirst = switch selected\n    true => true\n    => false\ngate = switch first\n    true => true\n    => false", "gate"),
        ("identity(simple bool value) => value\nsimple bool selected = true\ngate = switch selected\n    true => true\n    => false\nreturned = identity(gate)", "returned"),
    ] {
        assert_admission(&guarded(prelude, condition, false), &[]);
    }
}

#[test]
fn derived_simple_switch_library_results_keep_call_specific_source_proof() {
    let library = r#"//@version=6
library("Simple switch origins")
simple bool chosen = true
export captured() =>
    gate = switch chosen
        true => true
        => false
    gate
export fromArgument(simple bool selected) =>
    switch selected
        true => true
        => false
export host() =>
    switch chart.is_standard
        true => true
        => false
"#;
    for (expression, rejected) in [
        ("guards.captured()", false),
        ("guards.fromArgument(true)", false),
        ("guards.fromArgument(chart.is_standard)", true),
    ] {
        let source = guarded(
            &format!("import audit/SimpleSwitchOrigins/1 as guards\ngate = {expression}"),
            "gate",
            false,
        );
        let result = compile_with_libraries(
            &source,
            &[("audit/SimpleSwitchOrigins/1".into(), library.into())],
            CompileOptions::default(),
        );
        assert_request_result(&result, &source, if rejected { &[0] } else { &[] });
    }
}

#[test]
fn local_policy_unproven_or_incomplete_simple_results_do_not_gain_request_admission() {
    // Local policy boundaries only; these add no new rejection rules. The existing
    // prefix covers the wider chart-context/alias/call/lifecycle matrix. No var Simple
    // lifetime rule is assumed. Inline nested blocks remain unmodeled here.
    for prelude in [
        "gate = switch chart.is_standard\n    true => true\n    => false",
        "selected = input.bool(true)\ngate = switch selected\n    true => true\n    => false",
        "simple bool selected = input.bool(true)\ngate = switch selected\n    true => true\n    => false",
        "series bool selected = true\ngate = switch selected\n    true => true\n    => false",
        "simple bool selected = true\ngate = switch selected\n    true => true\n    => false\ngate := false",
        "simple bool selected = true\ngate = switch selected\n    true => chart.is_standard\n    => false",
        "simple bool selected = true\ngate = switch selected\n    true =>\n        copy = true\n        copy\n    => false",
        "simple bool selected = true\nvar events = array.new_int()\ngate = switch selected\n    true =>\n        array.push(events, 1)\n        true\n    => false",
        "simple bool selected = true\ngate = switch selected\n    true =>\n        switch selected\n            true => true\n            => false\n    => false",
    ] {
        assert_admission(&guarded(prelude, "gate", false), &[0]);
    }
}

#[test]
fn simple_switch_rendering_metadata_keeps_runtime_and_input_bindings_separate() {
    let source = r#"//@version=6
indicator("Simple switch rendering metadata")
enabled = input.bool(false, "Enabled")
width = input.int(7, "Width")
amount = input.int(3, "Amount", active=not enabled)
simple string selected = "a"
shift = switch selected
    "a" => 2
    => -1
plot(close, linewidth=width, offset=shift)
"#;
    let compiled = compile_ok(source);
    assert_eq!(
        compiled
            .inputs
            .iter()
            .map(|input| input.id)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(
        compiled
            .inputs
            .iter()
            .map(|input| input.title.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("Enabled"), Some("Width"), Some("Amount")]
    );
    assert_eq!(
        compiled
            .inputs
            .iter()
            .map(|input| &input.defval)
            .collect::<Vec<_>>(),
        vec![
            &InputValue::Bool(false),
            &InputValue::Int(7),
            &InputValue::Int(3)
        ]
    );
    let active = &compiled.inputs[2].active;
    assert!(active.default);
    assert_eq!(active.dependencies, vec![0]);
    let direct = active.direct.expect("existing inverted Input binding");
    assert_eq!((direct.input, direct.inverted), (0, true));
    assert_eq!(
        active.expression,
        Some(InputActiveExpr::Unary {
            op: InputActiveUnaryOp::Not,
            operand: Box::new(InputActiveExpr::Input { input: 0 }),
        })
    );
    assert_eq!(compiled.plots.len(), 1);
    let style = &compiled.plots[0].style;
    assert_eq!(style.offset, 0);
    assert_eq!(style.line_width, 1);
    assert!(style.unsupported.is_empty());
    assert!(style.bound_scalars.is_empty());
    assert_eq!(style.runtime_scalars.len(), 2);
    let linewidth = &style.runtime_scalars[0];
    assert_eq!(linewidth.field, "linewidth");
    assert_eq!(linewidth.qualifier, pinewasm_hir::Qualifier::Input);
    // Source proof preserves the Simple qualifier for the offset.
    let runtime = &style.runtime_scalars[1];
    assert_eq!(runtime.field, "offset");
    assert_eq!(runtime.qualifier, pinewasm_hir::Qualifier::Simple);
    let slot = &compiled.series_slots[runtime.slot.0 as usize];
    assert_eq!(slot.name, format!("%render{}", runtime.slot.0));
    assert!(slot.persistent);
    assert!(!slot.varip);
    // Annotations are optional; the resolved Int value below verifies
    // the portable runtime-style contract without requiring an allocator tag.

    let market = pinewasm_compiler::MarketData {
        open: vec![1.0, 2.0, 3.0, 4.0],
        high: vec![4.0; 4],
        low: vec![0.0; 4],
        close: vec![1.0, 2.0, 3.0, 4.0],
        volume: vec![1.0; 4],
        ..pinewasm_compiler::MarketData::default()
    };
    let run = pinewasm_compiler::run_program(
        &compiled,
        &market,
        &pinewasm_compiler::RunOptions::default(),
    );
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    assert_eq!(run.bars_executed, 4);
    assert_eq!(run.plots.len(), 1);
    assert_eq!(
        run.plots[0].values,
        vec![
            pinewasm_compiler::Value::Float(1.0),
            pinewasm_compiler::Value::Float(2.0),
            pinewasm_compiler::Value::Float(3.0),
            pinewasm_compiler::Value::Float(4.0)
        ]
    );
    let runtime_style = &run.plots[0].style;
    assert!(runtime_style.runtime_scalars.is_empty());
    assert!(runtime_style.unsupported.is_empty());
    assert_eq!(
        runtime_style
            .bound_scalars
            .iter()
            .map(|binding| binding.field.as_str())
            .collect::<Vec<_>>(),
        vec!["linewidth", "offset"]
    );
    assert_eq!(
        runtime_style.bound_scalars[0].expression,
        value(InputValue::Int(7))
    );
    assert_eq!(
        runtime_style.bound_scalars[1].expression,
        value(InputValue::Int(2))
    );

    // A source-closed Simple result remains Simple at metadata-only sinks.
    let active_source = r#"//@version=6
indicator("Simple switch active qualifier")
simple bool selected = true
gate = switch selected
    true => true
    => false
amount = input.int(3, active=gate)
plot(amount)
"#;
    let rejected = compile(active_source, CompileOptions::default());
    assert!(!rejected.success);
    let errors: Vec<_> = rejected
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.is_error())
        .collect();
    assert_eq!(errors.len(), 1, "{:?}", rejected.diagnostics);
    assert_eq!(errors[0].code.as_deref(), Some("E0230"));
    let span = errors[0].primary_span().expect("active argument span");
    assert_eq!(span.source, SourceId::MAIN);
    let start = active_source.rfind("gate").unwrap() as u32;
    assert_eq!((span.start, span.end), (start, start + 4));
}
