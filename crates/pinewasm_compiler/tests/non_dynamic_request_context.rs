//! Non-dynamic request admission is separate from series-only call scheduling.
//!
//! The first two groups check thirteen request-context cases: nine request
//! contexts and four input-to-const consumers. These check compile-time
//! admission, not chart rendering.

use pinewasm_compiler::{compile, CompileOptions};

fn source(body: &str, dynamic: bool) -> String {
    format!("//@version=6\nstrategy(\"S\", dynamic_requests={dynamic})\n{body}\nplot(close)\n")
}

fn request_count(source: &str) -> usize {
    compile(source, CompileOptions::default())
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code.as_deref() == Some("E0242"))
        .count()
}

#[test]
fn request_contexts_have_expected_admission_and_spans() {
    for (id, source, rejected) in [
        (
            "static_false_input_subjectless",
            r#"//@version=6
strategy("S", dynamic_requests=false)
useWeekly = input.bool(true)
x = switch
    useWeekly => request.security(syminfo.tickerid, "1W", close)
    => close
plot(x)
"#,
            true,
        ),
        (
            "static_false_series_subject_sole_fallback",
            r#"//@version=6
strategy("S", dynamic_requests=false)
switch close > open
    =>
        y = request.security(syminfo.tickerid, "1W", close)
plot(close)
"#,
            false,
        ),
        (
            "static_false_const_subjectless",
            r#"//@version=6
strategy("S", dynamic_requests=false)
useWeekly = true
x = switch
    useWeekly => request.security(syminfo.tickerid, "1W", close)
    => close
plot(x)
"#,
            false,
        ),
        (
            "static_false_simple_subjectless",
            r#"//@version=6
strategy("S", dynamic_requests=false)
simple bool useWeekly = true
x = switch
    useWeekly => request.security(syminfo.tickerid, "1W", close)
    => close
plot(x)
"#,
            false,
        ),
        (
            "static_false_subjectless_sole_fallback",
            r#"//@version=6
strategy("S", dynamic_requests=false)
switch
    =>
        y = request.security(syminfo.tickerid, "1W", close)
plot(close)
"#,
            false,
        ),
        (
            "dynamic_true_input_subjectless",
            r#"//@version=6
strategy("S", dynamic_requests=true)
useWeekly = input.bool(true)
x = switch
    useWeekly => request.security(syminfo.tickerid, "1W", close)
    => close
plot(x)
"#,
            false,
        ),
        (
            "static_false_global",
            r#"//@version=6
strategy("S", dynamic_requests=false)
x = request.security(syminfo.tickerid, "1W", close)
plot(x)
"#,
            false,
        ),
        (
            "static_false_helper_wrapped_input_subjectless",
            r#"//@version=6
strategy("S", dynamic_requests=false)
fetchWeekly() => request.security(syminfo.tickerid, "1W", close)
useWeekly = input.bool(true)
x = switch
    useWeekly => fetchWeekly()
    => close
plot(x)
"#,
            true,
        ),
        (
            "static_false_genuine_simple_subjectless",
            r#"//@version=6
strategy("S", dynamic_requests=false)
simple bool enabled = syminfo.type == "crypto"
x = switch
    enabled => request.security(syminfo.tickerid, "1W", close)
    => close
plot(x)
"#,
            true,
        ),
    ] {
        let result = compile(source, CompileOptions::default());
        assert_eq!(result.success, !rejected, "{id}: {:?}", result.diagnostics);
        let errors: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.is_error())
            .collect();
        if rejected {
            assert_eq!(errors.len(), 1, "{id}: {errors:?}");
            assert_eq!(errors[0].code.as_deref(), Some("E0242"), "{id}");
            let span = errors[0].primary_span().expect("request source span");
            assert_eq!(
                &source[span.start as usize..span.end as usize],
                "request.security(syminfo.tickerid, \"1W\", close)",
                "{id}"
            );
        }
    }
}

#[test]
fn input_consumers_reject_at_the_argument_token() {
    for (id, source) in [
        (
            "plot-force_overlay",
            r#"//@version=6
indicator("Qualifier boundary",overlay=true)
sel=input.bool(true,"sel")
X=true
if not sel
    X := false
plot(close,force_overlay=X)
"#,
        ),
        (
            "plotshape-size",
            r#"//@version=6
indicator("Qualifier boundary",overlay=true)
sel=input.bool(true,"sel")
X=size.small
if not sel
    X := size.large
plotshape(close>open,size=X)
"#,
        ),
        (
            "plotchar-size",
            r#"//@version=6
indicator("Qualifier boundary",overlay=true)
sel=input.bool(true,"sel")
X=size.small
if not sel
    X := size.large
plotchar(close>open,char="x",size=X)
"#,
        ),
        (
            "fill-fillgaps",
            r#"//@version=6
indicator("Qualifier boundary",overlay=true)
sel=input.bool(true,"sel")
X=true
if not sel
    X := false
fill(plot(close>open?close:na),plot(open),color=color.new(color.red,80),fillgaps=X)
"#,
        ),
    ] {
        let result = compile(source, CompileOptions::default());
        assert!(!result.success, "{id}");
        let errors: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.is_error())
            .collect();
        assert_eq!(errors.len(), 1, "{id}: {errors:?}");
        assert_eq!(errors[0].code.as_deref(), Some("E0206"), "{id}");
        assert!(
            errors[0].message.ends_with("found input"),
            "{id}: {errors:?}"
        );
        let span = errors[0].primary_span().expect("argument source span");
        assert_eq!(&source[span.start as usize..span.end as usize], "X", "{id}");
    }
}

const REQUEST: &str = "request.security(syminfo.tickerid, \"1W\", close)";

#[test]
fn request_controls_cover_if_switch_forms_without_treating_function_scope_as_control() {
    for (selector, rejected) in [
        ("enabled = true", false),
        ("simple bool enabled = true", false),
        ("enabled = input.bool(true)", true),
        ("simple bool enabled = syminfo.type == \"crypto\"", true),
    ] {
        for expression in [false, true] {
            for switch in [false, true] {
                for fallback in [false, true] {
                    for wrapped in [false, true] {
                        let prefix = if expression { "value = " } else { "" };
                        let call = if wrapped { "fetch()" } else { REQUEST };
                        let (selected, other) = if fallback {
                            ("close", call)
                        } else {
                            (call, "close")
                        };
                        let branch = if switch {
                            format!("{prefix}switch\n    enabled => {selected}\n    => {other}")
                        } else {
                            format!("{prefix}if enabled\n    {selected}\nelse\n    {other}")
                        };
                        let body = format!("fetch() => {REQUEST}\n{selector}\n{branch}");
                        let program = source(&body, false);
                        let result = compile(&program, CompileOptions::default());
                        assert_eq!(
                            result.success, !rejected,
                            "{program}: {:?}",
                            result.diagnostics
                        );
                        assert_eq!(request_count(&program), usize::from(rejected), "{program}");
                    }
                }
            }
        }
    }
}

#[test]
fn earlier_switch_conditions_control_later_and_default_requests() {
    for earlier in ["input.bool(true)", "syminfo.type == \"crypto\""] {
        for expression in [false, true] {
            for fallback in [false, true] {
                let prefix = if expression { "value = " } else { "" };
                let (later, default) = if fallback {
                    ("close", REQUEST)
                } else {
                    (REQUEST, "close")
                };
                let body = format!("enabled = {earlier}\n{prefix}switch\n    enabled => close\n    true => {later}\n    => {default}");
                let program = source(&body, false);
                assert_eq!(request_count(&program), 1, "{program}");
            }
        }
    }
}

#[test]
fn request_context_restores_for_global_and_nested_global_helpers() {
    for local in [
        "if enabled\n    ignored = close",
        "value = if enabled\n    close\nelse\n    open",
        "switch\n    enabled => close\n    => open",
        "value = switch\n    enabled => close\n    => open",
    ] {
        let program = source(&format!("enabled = input.bool(true)\n{local}\nfetch() => {REQUEST}\nwrapped() => fetch()\nplot(wrapped())"), false);
        let result = compile(&program, CompileOptions::default());
        assert!(result.success, "{program}: {:?}", result.diagnostics);
    }
    // This discarded helper uses analyze_block for its loop-tail plumbing.
    // Its request is before the loop, at an unconditional global call site.
    let program = source(&format!("fetch() =>\n    requested = {REQUEST}\n    for i = 0 to 1\n        ignored = i\nfetch()"), false);
    let result = compile(&program, CompileOptions::default());
    assert!(result.success, "{program}: {:?}", result.diagnostics);
}

#[test]
fn request_context_restores_after_erroneous_value_blocks() {
    for local in [
        "value = if enabled\n    missing\nelse\n    close",
        "value = switch\n    enabled => missing\n    => close",
    ] {
        let program = source(
            &format!("enabled = input.bool(true)\n{local}\nx = {REQUEST}"),
            false,
        );
        let result = compile(&program, CompileOptions::default());
        assert!(!result.success, "{program}");
        assert_eq!(
            request_count(&program),
            0,
            "{program}: {:?}",
            result.diagnostics
        );
    }
}

#[test]
fn request_restrictions_do_not_change_series_only_scheduling() {
    for (selector, warning) in [
        ("enabled = true", false),
        ("simple bool enabled = true", false),
        ("enabled = input.bool(true)", false),
        ("simple bool enabled = syminfo.type == \"crypto\"", false),
        ("enabled = close > open", true),
    ] {
        for expression in [false, true] {
            let prefix = if expression { "value = " } else { "" };
            for dynamic in [false, true] {
                let program = source(
                    &format!(
                        "{selector}\n{prefix}switch\n    enabled => ta.sma(close, 2)\n    => close"
                    ),
                    dynamic,
                );
                let result = compile(&program, CompileOptions::default());
                assert!(result.success, "{program}: {:?}", result.diagnostics);
                assert_eq!(
                    result
                        .diagnostics
                        .iter()
                        .any(|diagnostic| diagnostic.code.as_deref() == Some("W0004")),
                    warning,
                    "{program}: {:?}",
                    result.diagnostics
                );
            }
        }
    }
}

#[test]
fn exact_known_simple_controls_remain_non_dynamic() {
    for (id, source) in [
        (
            "compound_known_bool",
            r#"//@version=6
strategy("S", dynamic_requests=false)
simple bool enabled=true
result = switch
    not (not enabled) => request.security(syminfo.tickerid,"D",close)
    => close
plot(result)
"#,
        ),
        (
            "known_simple_string_subject",
            r#"//@version=6
strategy("S", dynamic_requests=false)
simple string choice="a"
result = switch choice
    "a" => request.security(syminfo.tickerid,"D",close)
    => 0.0
plot(result)
"#,
        ),
    ] {
        let result = compile(source, CompileOptions::default());
        assert!(result.success, "{id}: {:?}", result.diagnostics);
    }
}

#[test]
fn closed_request_provenance_never_substitutes_input_defaults() {
    for (declarations, condition, rejected) in [
        ("simple bool enabled = true", "not (not enabled)", false),
        (
            "simple bool enabled = input.bool(true)",
            "not (not enabled)",
            true,
        ),
        ("simple int count = math.max(1, 2)", "count == 2", false),
        (
            "simple int count = math.max(input.int(1), 2)",
            "count == 2",
            true,
        ),
        (
            "simple bool enabled = true\nsimple bool known = enabled ? true : false",
            "known",
            false,
        ),
        (
            "simple bool enabled = input.bool(true)\nsimple bool known = enabled ? true : false",
            "known",
            true,
        ),
    ] {
        let program = source(
            &format!("{declarations}\nx = if {condition}\n    {REQUEST}\nelse\n    close"),
            false,
        );
        let result = compile(&program, CompileOptions::default());
        assert_eq!(
            result.success, !rejected,
            "{program}: {:?}",
            result.diagnostics
        );
        assert_eq!(request_count(&program), usize::from(rejected), "{program}");
    }
    for (choice, rejected) in [
        ("\"a\"", false),
        ("input.string(\"a\")", true),
        ("syminfo.type", true),
    ] {
        let program = source(&format!("simple string choice = {choice}\nx = switch choice\n    \"a\" => {REQUEST}\n    => close"), false);
        let result = compile(&program, CompileOptions::default());
        assert_eq!(
            result.success, !rejected,
            "{program}: {:?}",
            result.diagnostics
        );
        assert_eq!(request_count(&program), usize::from(rejected), "{program}");
    }
}
