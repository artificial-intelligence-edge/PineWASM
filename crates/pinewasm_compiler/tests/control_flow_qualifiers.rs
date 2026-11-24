//! Reassigned values inherit the qualifiers of the conditions controlling writes.
//!
//! PineWASM's qualifier inference covers both expressions and structures in
//! variable declarations. A constant initializer cannot erase an input/simple
//! condition that decides a later assignment. The four argument parameters below
//! accept only const values.

use pinewasm_compiler::{compile, compile_and_run, CompileOptions, MarketData, RunOptions, Value};

const ARGUMENTS: [(&str, &str, &str); 4] = [
    ("plot(close, force_overlay=VALUE)", "true", "false"),
    (
        "plotshape(close > open, size=VALUE)",
        "size.small",
        "size.large",
    ),
    (
        "plotchar(close > open, char=\"x\", size=VALUE)",
        "size.small",
        "size.large",
    ),
    (
        "fill(plot(close), plot(open), color=color.blue, fillgaps=VALUE)",
        "true",
        "false",
    ),
];

fn source(prelude: &str, call: &str) -> String {
    format!("//@version=6\nindicator(\"Control qualifiers\")\n{prelude}\n{call}\n")
}

fn assert_const_rejection(prelude: &str, call: &str) {
    let result = compile(&source(prelude, call), CompileOptions::default());
    assert!(!result.success, "unexpectedly accepted:\n{prelude}\n{call}");
    assert!(
        result.diagnostics.iter().any(|diagnostic| {
            diagnostic.code.as_deref() == Some("E0206")
                && diagnostic.message.contains("must be const or weaker")
        }),
        "expected the argument's const qualifier diagnostic: {:?}",
        result.diagnostics
    );
}

#[test]
fn input_controlled_reassignments_cannot_reach_const_only_arguments() {
    for (call, initial, assigned) in ARGUMENTS {
        for body in [
            format!("if selector\n    value := {assigned}"),
            format!("if selector\n    ignored = 0\nelse\n    value := {assigned}"),
            format!("if selector\n    if true\n        value := {assigned}"),
            format!("if true\n    if selector\n        value := {assigned}"),
            format!("if selector\n    value := {assigned}\nelse\n    value := {initial}"),
            format!("switch selector\n    true => value := {assigned}\n    => value := {initial}"),
            format!("ignored = if selector\n    value := {assigned}\n    0\nelse\n    0"),
            format!("ignored = switch selector\n    true =>\n        value := {assigned}\n        0\n    => 0"),
        ] {
            let prelude =
                format!("selector = input.bool(true)\nvalue = {initial}\n{body}");
            assert_const_rejection(&prelude, &call.replace("VALUE", "value"));
        }
    }
}

#[test]
fn simple_controlled_reassignments_cannot_reach_const_only_arguments() {
    for (call, initial, assigned) in ARGUMENTS {
        let prelude = format!(
            "simple bool selector = true\nvalue = {initial}\nif selector\n    value := {assigned}"
        );
        assert_const_rejection(&prelude, &call.replace("VALUE", "value"));
    }
}

#[test]
fn later_constant_writes_do_not_erase_input_control_dependencies() {
    for (call, initial, assigned) in ARGUMENTS {
        let prelude = format!(
            "selector = input.bool(true)\nvalue = {initial}\nif selector\n    value := {assigned}\nvalue := {initial}"
        );
        assert_const_rejection(&prelude, &call.replace("VALUE", "value"));
    }
}

#[test]
fn inlined_function_reassignments_keep_their_input_control_dependency() {
    for (call, initial, assigned) in ARGUMENTS {
        let prelude = format!(
            "selectValue(selector) =>\n    value = {initial}\n    if selector\n        value := {assigned}\n    value\nselected = selectValue(input.bool(true))"
        );
        assert_const_rejection(&prelude, &call.replace("VALUE", "selected"));
    }
}

#[test]
fn constant_control_and_unrelated_siblings_remain_admissible() {
    for (call, initial, assigned) in ARGUMENTS {
        for prelude in [
            format!("value = {initial}\nif true\n    value := {assigned}"),
            format!(
                "selector = input.bool(true)\nchanged = {initial}\nif selector\n    changed := {assigned}\nvalue = {initial}"
            ),
            format!(
                "selector = input.bool(true)\nchanged = {initial}\nignored = if selector\n    changed := {assigned}\n    0\nelse\n    0\nvalue = {initial}"
            ),
        ] {
            let result = compile(
                &source(&prelude, &call.replace("VALUE", "value")),
                CompileOptions::default(),
            );
            assert!(result.success, "{prelude}: {:?}", result.diagnostics);
        }
    }
}

#[test]
fn input_control_does_not_make_run_stable_calls_intermittent() {
    let result = compile(
        &source(
            "selector = input.bool(true)\nlength = 1\nif selector\n    length := 2\n    smoothed = ta.sma(close, 2)",
            "plot(close, linewidth=length)",
        ),
        CompileOptions::default(),
    );
    assert!(result.success, "{:?}", result.diagnostics);
    assert!(
        result
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.code.as_deref() != Some("W0004")),
        "{:?}",
        result.diagnostics
    );
}

#[test]
fn pure_local_helper_writes_do_not_inherit_the_callers_control_qualifier() {
    for selector in ["selector = input.bool(true)", "simple bool selector = true"] {
        for call in [
            "result = if selector\n    f()\nelse\n    1",
            "result = switch selector\n    true => f()\n    => 1",
        ] {
            let result = compile(
                &source(
                    &format!("needConst(const int value) => value\nf() =>\n    y = 1\n    y := 2\n    needConst(y)\n{selector}"),
                    &format!("{call}\nplot(result)"),
                ),
                CompileOptions::default(),
            );
            assert!(result.success, "{call}: {:?}", result.diagnostics);
        }
    }
}

#[test]
fn tuple_locals_and_nested_helpers_keep_their_own_control_scope() {
    let result = compile(
        &source(
            "needConst(const int value) => value\npair() => [1, 2]\ninner() =>\n    [left, right] = pair()\n    left := 3\n    needConst(left)\nf() => inner()\nselector = input.bool(true)",
            "result = if selector\n    f()\nelse\n    1\nplot(result)",
        ),
        CompileOptions::default(),
    );
    assert!(result.success, "{:?}", result.diagnostics);
}

#[test]
fn declared_simple_locals_can_be_written_at_their_own_branch_depth() {
    let result = compile(
        &source(
            "value = 0\nif close > open\n    simple int local = 1\n    local := 2\n    value := local",
            "plot(value)",
        ),
        CompileOptions::default(),
    );
    assert!(result.success, "{:?}", result.diagnostics);
}

#[test]
fn locally_declared_values_still_inherit_their_own_nested_input_condition() {
    assert_const_rejection(
        "needConst(const int value) => value\nf(selector) =>\n    y = 1\n    if selector\n        y := 2\n    needConst(y)\nresult = f(input.bool(true))",
        "plot(result)",
    );
}

fn switch_with_default_at(default: &str, labels: [&str; 2], position: usize) -> String {
    let mut arms = labels.to_vec();
    arms.insert(position, default);
    format!("switch true\n{}", arms.join("\n"))
}

#[test]
fn switch_default_must_follow_all_labeled_arms() {
    for expression in [false, true] {
        for subjectless in [false, true] {
            for position in 0..=2 {
                let switch = switch_with_default_at(
                    "    => 0",
                    ["    false => 0", "    true => 0"],
                    position,
                );
                let switch = if subjectless {
                    switch.replacen("switch true", "switch", 1)
                } else {
                    switch
                };
                let prefix = if expression { "ignored = " } else { "" };
                let result = compile(
                    &source(&format!("{prefix}{switch}"), "plot(close)"),
                    CompileOptions::default(),
                );
                if position == 2 {
                    assert!(result.success, "{switch}: {:?}", result.diagnostics);
                } else {
                    assert!(!result.success, "{switch}");
                    assert!(result.program.is_none());
                    assert!(
                        result.diagnostics.iter().any(|diagnostic| {
                            diagnostic.code.as_deref() == Some("E0225")
                                && diagnostic.message == "a `switch` default arm must be last"
                        }),
                        "{switch}: {:?}",
                        result.diagnostics
                    );
                    assert!(result
                        .diagnostics
                        .iter()
                        .all(|diagnostic| diagnostic.code.as_deref() != Some("E0206")));
                }
            }
        }
    }
}

#[test]
fn lawful_switch_defaults_inherit_every_labeled_arm_control_qualifier() {
    for selector in [
        "selector = input.bool(true)",
        "simple bool selector = true",
        "selector = close > open",
    ] {
        for (call, initial, assigned) in ARGUMENTS {
            for expression in [false, true] {
                let default = if expression {
                    format!("    =>\n        value := {assigned}\n        0")
                } else {
                    format!("    => value := {assigned}")
                };
                let labels = if expression {
                    ["    false => 0", "    selector => 0"]
                } else {
                    ["    false => ignored = 0", "    selector => ignored = 0"]
                };
                for position in 0..=2 {
                    let switch = switch_with_default_at(&default, labels, position);
                    let prefix = if expression { "ignored = " } else { "" };
                    let prelude = format!("{selector}\nvalue = {initial}\n{prefix}{switch}");
                    let call = call.replace("VALUE", "value");
                    if position == 2 {
                        assert_const_rejection(&prelude, &call);
                    } else {
                        let result = compile(&source(&prelude, &call), CompileOptions::default());
                        assert!(!result.success, "{prelude}");
                        assert!(result.program.is_none());
                        assert!(
                            result
                                .diagnostics
                                .iter()
                                .any(|diagnostic| { diagnostic.code.as_deref() == Some("E0225") }),
                            "{prelude}: {:?}",
                            result.diagnostics
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn lawful_switch_defaults_keep_constant_values_and_pure_helper_locals() {
    for selector in ["selector = true", "selector = input.bool(true)"] {
        for expression in [false, true] {
            let default = if expression {
                "    => f()"
            } else {
                "    => ignored = f()"
            };
            let labels = if expression {
                ["    false => 0", "    selector => 0"]
            } else {
                ["    false => ignored = 0", "    selector => ignored = 0"]
            };
            let switch = switch_with_default_at(default, labels, 2);
            let prefix = if expression { "ignored = " } else { "" };
            let prelude = format!(
                "needConst(const int value) => value\nf() =>\n    local = 1\n    local := 2\n    needConst(local)\n{selector}\n{prefix}{switch}"
            );
            let result = compile(&source(&prelude, "plot(close)"), CompileOptions::default());
            assert!(result.success, "{prelude}: {:?}", result.diagnostics);
        }
    }
    let switch = switch_with_default_at(
        "    => value := false",
        ["    false => ignored = 0", "    true => ignored = 0"],
        2,
    );
    let result = compile(
        &source(
            &format!("value = true\n{switch}"),
            "plot(close, force_overlay=value)",
        ),
        CompileOptions::default(),
    );
    assert!(result.success, "{switch}: {:?}", result.diagnostics);
}

#[test]
fn lawful_switch_defaults_preserve_input_registration_order() {
    for expression in [false, true] {
        let prefix = if expression { "ignored = " } else { "" };
        let prelude = format!(
            "selector = input.bool(true, \"Selector\")\n{prefix}switch true\n    selector =>\n        labeledInput = input.int(2, \"Labeled\")\n    =>\n        defaultInput = input.int(1, \"Default\")"
        );
        let result = compile(&source(&prelude, "plot(close)"), CompileOptions::default());
        assert!(result.success, "{:?}", result.diagnostics);
        let program = result.program.expect("program");
        let inputs: Vec<_> = program
            .inputs
            .iter()
            .map(|input| (input.id, input.title.as_deref()))
            .collect();
        assert_eq!(
            inputs,
            [
                (0, Some("Selector")),
                (1, Some("Labeled")),
                (2, Some("Default"))
            ]
        );
    }
}

#[test]
fn lawful_switch_defaults_keep_series_warning_semantics() {
    for (selector, expect_warning) in [
        ("selector = input.bool(true)", false),
        ("simple bool selector = true", false),
        ("selector = close > open", true),
    ] {
        let switch = switch_with_default_at(
            "    => smoothed = ta.sma(close, 2)",
            ["    false => ignored = 0", "    selector => ignored = 0"],
            2,
        );
        let result = compile(
            &source(&format!("{selector}\n{switch}"), "plot(close)"),
            CompileOptions::default(),
        );
        assert!(result.success, "{switch}: {:?}", result.diagnostics);
        assert_eq!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code.as_deref() == Some("W0004")),
            expect_warning,
            "{selector}\n{switch}: {:?}",
            result.diagnostics
        );
    }
}

#[test]
fn switch_order_diagnostic_preserves_duplicate_and_inner_errors() {
    for expression in [false, true] {
        let (prefix, first, second) = if expression {
            ("ignored = ", "firstMissing", "secondMissing")
        } else {
            ("", "value := firstMissing", "value := secondMissing")
        };
        let prelude = format!(
            "value = 0\n{prefix}switch true\n    => {first}\n    false => 0\n    => {second}"
        );
        let result = compile(&source(&prelude, "plot(close)"), CompileOptions::default());
        assert!(!result.success);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code.as_deref() == Some("E0225")
                    && diagnostic.message.contains("must be last")),
            "{:?}",
            result.diagnostics
        );
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code.as_deref() == Some("E0225")
                    && diagnostic.message.contains("at most one")),
            "{:?}",
            result.diagnostics
        );
        for name in ["firstMissing", "secondMissing"] {
            assert!(
                result
                    .diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.message.contains(name)),
                "{name}: {:?}",
                result.diagnostics
            );
        }
    }
}

#[test]
fn lawful_switch_defaults_preserve_subject_and_selected_effects_once() {
    let market = MarketData {
        open: vec![9.0, 19.0, 29.0, 39.0],
        high: vec![11.0, 21.0, 31.0, 41.0],
        low: vec![8.0, 18.0, 28.0, 38.0],
        close: vec![10.0, 20.0, 30.0, 40.0],
        volume: vec![100.0; 4],
        ..MarketData::default()
    };
    for tuple in [false, true] {
        let (prefix, labels, default, plots, mut expected) = if tuple {
            (
                "[left, right] = ",
                [
                    "    0 =>\n        array.push(events, 10)\n        [10, close]",
                    "    1 =>\n        array.push(events, 20)\n        [20, close + 1]",
                ],
                "    =>\n        array.push(events, 30)\n        [30, close + 2]",
                "plot(left)\nplot(right)",
                vec![vec![10.0, 20.0, 30.0, 10.0], vec![10.0, 21.0, 32.0, 40.0]],
            )
        } else {
            (
                "",
                [
                    "    0 =>\n        array.push(events, 10)\n        value := 10",
                    "    1 =>\n        array.push(events, 20)\n        value := 20",
                ],
                "    =>\n        array.push(events, 30)\n        value := 30",
                "plot(value)",
                vec![vec![10.0, 20.0, 30.0, 10.0]],
            )
        };
        let prelude = format!(
            "var events = array.new_int()\nsubject(a) =>\n    array.push(a, 7)\n    bar_index % 3\nvalue = 0\n{prefix}switch subject(events)\n{}\n{}\n{default}", labels[0], labels[1]
        );
        let run = compile_and_run(
            &source(&prelude, &format!("{plots}\nplot(array.size(events))\nplot(array.get(events, array.size(events) - 1))")),
            &market,
            CompileOptions::default(),
            &RunOptions::default(),
        );
        assert!(
            run.diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.is_error()),
            "{:?}",
            run.diagnostics
        );
        assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
        assert_eq!(run.bars_executed, 4);
        expected.push(vec![2.0, 4.0, 6.0, 8.0]);
        expected.push(vec![10.0, 20.0, 30.0, 10.0]);
        assert_eq!(run.plots.len(), expected.len());
        for (plot, expected) in run.plots.iter().zip(expected) {
            let actual: Vec<_> = plot
                .values
                .iter()
                .map(|value| match value {
                    Value::Int(value) => *value as f64,
                    Value::Float(value) => *value,
                    other => panic!("unexpected plot value {other:?}"),
                })
                .collect();
            assert_eq!(actual, expected, "tuple={tuple}");
        }
    }
}

const DEFAULT_ONLY_SUBJECTS: [(&str, &str); 5] = [
    ("selector = true", " selector"),
    ("selector = input.bool(true)", " selector"),
    ("simple bool selector = true", " selector"),
    ("selector = close > open", " selector"),
    ("", ""),
];

#[test]
fn default_only_switch_writes_do_not_depend_on_unused_subject_qualifiers() {
    for (prelude, subject) in DEFAULT_ONLY_SUBJECTS {
        for (call, initial, assigned) in ARGUMENTS {
            for expression in [false, true] {
                let prefix = if expression { "ignored = " } else { "" };
                let prelude = format!("{prelude}\nvalue = {initial}\n{prefix}switch{subject}\n    => value := {assigned}");
                let result = compile(
                    &source(&prelude, &call.replace("VALUE", "value")),
                    CompileOptions::default(),
                );
                assert!(result.success, "{prelude}: {:?}", result.diagnostics);
            }
        }
    }
}

#[test]
fn default_only_switch_results_keep_the_value_qualifier() {
    for (prelude, subject) in DEFAULT_ONLY_SUBJECTS {
        for (call, _, assigned) in ARGUMENTS {
            let prelude = format!("{prelude}\nvalue = switch{subject}\n    => {assigned}");
            let result = compile(
                &source(&prelude, &call.replace("VALUE", "value")),
                CompileOptions::default(),
            );
            assert!(result.success, "{prelude}: {:?}", result.diagnostics);
        }
        assert_const_rejection(
            &format!("{prelude}\nvalue = switch{subject}\n    => input.bool(true)"),
            "plot(close, force_overlay=value)",
        );
    }
}

#[test]
fn default_only_switches_preserve_outer_and_nested_write_controls() {
    for (prelude, subject) in DEFAULT_ONLY_SUBJECTS {
        for (call, initial, assigned) in ARGUMENTS {
            for expression in [false, true] {
                let prefix = if expression { "ignored = " } else { "" };
                for outer in ["outer = input.bool(true)", "outer = close > open"] {
                    assert_const_rejection(
                        &format!("{prelude}\n{outer}\nvalue = {initial}\nif outer\n    {prefix}switch{subject}\n        => value := {assigned}"),
                        &call.replace("VALUE", "value"),
                    );
                }
                assert_const_rejection(
                    &format!("{prelude}\ninner = input.bool(true)\nvalue = {initial}\n{prefix}switch{subject}\n    =>\n        if inner\n            value := {assigned}\n        0"),
                    &call.replace("VALUE", "value"),
                );
            }
        }
        assert_const_rejection(
            &format!("{prelude}\nvar value = true\nswitch{subject}\n    => value := false"),
            "plot(close, force_overlay=value)",
        );
    }
}

#[test]
fn default_only_switch_calls_are_unconditional_unless_the_caller_is_conditional() {
    for (prelude, subject) in DEFAULT_ONLY_SUBJECTS {
        for expression in [false, true] {
            for conditional_caller in [false, true] {
                let prefix = if expression { "ignored = " } else { "" };
                let switch = format!("{prefix}switch{subject}\n    => ta.sma(close, 2)");
                let body = if conditional_caller {
                    format!(
                        "if close > open\n{}",
                        switch
                            .lines()
                            .map(|line| format!("    {line}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                } else {
                    switch
                };
                let result = compile(
                    &source(&format!("{prelude}\n{body}"), "plot(close)"),
                    CompileOptions::default(),
                );
                assert!(result.success, "{body}: {:?}", result.diagnostics);
                assert_eq!(
                    result
                        .diagnostics
                        .iter()
                        .any(|diagnostic| diagnostic.code.as_deref() == Some("W0004")),
                    conditional_caller,
                    "{prelude}\n{body}: {:?}",
                    result.diagnostics
                );
            }
        }
    }
}

#[test]
fn default_only_switches_preserve_subject_effects_and_input_registration_order() {
    let market = MarketData {
        open: vec![9.0, 11.0, 9.0],
        high: vec![12.0; 3],
        low: vec![8.0; 3],
        close: vec![10.0; 3],
        volume: vec![100.0; 3],
        ..MarketData::default()
    };
    for expression in [false, true] {
        let prefix = if expression { "ignored = " } else { "" };
        for selector in ["input.bool(true, \"Selector\")", "close > open"] {
            let prelude = format!("selector = {selector}\nvar events = array.new_int()\nsubject(a) =>\n    array.push(a, 7)\n    selector\n{prefix}switch subject(events)\n    =>\n        array.push(events, 9)\n        0");
            let run = compile_and_run(
                &source(
                    &prelude,
                    "plot(array.size(events))\nplot(array.get(events, array.size(events) - 1))",
                ),
                &market,
                CompileOptions::default(),
                &RunOptions::default(),
            );
            assert!(
                run.diagnostics
                    .iter()
                    .all(|diagnostic| !diagnostic.is_error()),
                "{:?}",
                run.diagnostics
            );
            assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
            assert_eq!(run.bars_executed, 3);
            let values: Vec<Vec<f64>> = run
                .plots
                .iter()
                .map(|plot| {
                    plot.values
                        .iter()
                        .map(|value| match value {
                            Value::Int(value) => *value as f64,
                            Value::Float(value) => *value,
                            other => panic!("unexpected plot value {other:?}"),
                        })
                        .collect()
                })
                .collect();
            assert_eq!(values, vec![vec![2.0, 4.0, 6.0], vec![9.0; 3]]);
        }
        let result = compile(
            &source(&format!("{prefix}switch input.bool(true, \"Subject\")\n    => input.int(2, \"Fallback\")\nafter = input.int(3, \"After\")"), "plot(after)"),
            CompileOptions::default(),
        );
        assert!(result.success, "{:?}", result.diagnostics);
        let inputs: Vec<_> = result
            .program
            .expect("program")
            .inputs
            .into_iter()
            .map(|input| (input.id, input.title))
            .collect();
        assert_eq!(
            inputs,
            vec![
                (0, Some("Subject".into())),
                (1, Some("Fallback".into())),
                (2, Some("After".into()))
            ]
        );
    }
}

#[test]
fn default_only_switch_subject_type_errors_still_surface() {
    for expression in [false, true] {
        let prefix = if expression { "ignored = " } else { "" };
        let prelude = format!("{prefix}switch array.push(array.new_int(), 1)\n    => 0");
        let result = compile(&source(&prelude, "plot(close)"), CompileOptions::default());
        assert!(!result.success);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code.as_deref() == Some("E0225")
                    && diagnostic.message.contains("subject must produce a value")),
            "{:?}",
            result.diagnostics
        );
    }
}

const LABELED_SUBJECTLESS_SELECTORS: [(&str, &str); 4] = [
    ("selector = true", "const"),
    ("selector = input.bool(true)", "input"),
    ("simple bool selector = true", "simple"),
    ("selector = close > open", "series"),
];

fn assert_exact_const_argument_qualifier(prelude: &str, call: &str, qualifier: &str) {
    let result = compile(&source(prelude, call), CompileOptions::default());
    if qualifier == "const" {
        assert!(result.success, "{prelude}: {:?}", result.diagnostics);
    } else {
        assert!(!result.success, "{prelude}");
        assert!(
            result.diagnostics.iter().any(|diagnostic| {
                diagnostic.code.as_deref() == Some("E0206")
                    && diagnostic.message.ends_with(&format!("found {qualifier}"))
            }),
            "{prelude}: {:?}",
            result.diagnostics
        );
    }
}

#[test]
fn labeled_subjectless_writes_follow_the_actual_condition_qualifier() {
    for (selector, qualifier) in LABELED_SUBJECTLESS_SELECTORS {
        for (call, initial, assigned) in ARGUMENTS {
            for expression in [false, true] {
                let prefix = if expression { "ignored = " } else { "" };
                let prelude = format!("{selector}\nvalue = {initial}\n{prefix}switch\n    selector => value := {assigned}\n    => value := {assigned}");
                assert_exact_const_argument_qualifier(
                    &prelude,
                    &call.replace("VALUE", "value"),
                    qualifier,
                );
            }
        }
    }
}

#[test]
fn labeled_subjectless_results_keep_condition_qualifiers() {
    for (selector, qualifier) in LABELED_SUBJECTLESS_SELECTORS {
        for (call, initial, assigned) in ARGUMENTS {
            let prelude =
                format!("{selector}\nvalue = switch\n    selector => {assigned}\n    => {initial}");
            assert_exact_const_argument_qualifier(
                &prelude,
                &call.replace("VALUE", "value"),
                qualifier,
            );
        }
    }
}

#[test]
fn labeled_subjectless_stateful_calls_warn_only_for_series_control() {
    for (selector, qualifier) in LABELED_SUBJECTLESS_SELECTORS {
        for expression in [false, true] {
            let prefix = if expression { "ignored = " } else { "" };
            for fallback in [false, true] {
                let (labeled, default) = if fallback {
                    ("0.0", "ta.sma(close, 2)")
                } else {
                    ("ta.sma(close, 2)", "0.0")
                };
                for caller_conditional in [false, true] {
                    let switch =
                        format!("{prefix}switch\n    selector => {labeled}\n    => {default}");
                    let body = if caller_conditional {
                        format!(
                            "if close > open\n{}",
                            switch
                                .lines()
                                .map(|line| format!("    {line}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        )
                    } else {
                        switch
                    };
                    let result = compile(
                        &source(&format!("{selector}\n{body}"), "plot(close)"),
                        CompileOptions::default(),
                    );
                    assert!(result.success, "{body}: {:?}", result.diagnostics);
                    let warning = result
                        .diagnostics
                        .iter()
                        .any(|diagnostic| diagnostic.code.as_deref() == Some("W0004"));
                    assert_eq!(
                        warning,
                        qualifier == "series" || caller_conditional,
                        "{selector}\n{body}: {:?}",
                        result.diagnostics
                    );
                }
            }
        }
    }
}

#[test]
fn earlier_subjectless_conditions_still_control_later_writes_and_calls() {
    for (earlier, expected) in [
        ("earlier = input.bool(true)", "input"),
        ("earlier = close > open", "series"),
    ] {
        for expression in [false, true] {
            let prefix = if expression { "ignored = " } else { "" };
            for fallback in [false, true] {
                let (later, default) = if fallback {
                    ("true", "value := false")
                } else {
                    ("value := false", "true")
                };
                let prelude = format!("{earlier}\nlater = input.bool(false)\nvalue = true\n{prefix}switch\n    earlier => true\n    later => {later}\n    => {default}");
                assert_exact_const_argument_qualifier(
                    &prelude,
                    "plot(close, force_overlay=value)",
                    expected,
                );
            }
            let prelude = format!("{earlier}\nlater = input.bool(false)\n{prefix}switch\n    earlier => 0.0\n    later => ta.sma(close, 2)\n    => 0.0");
            let result = compile(&source(&prelude, "plot(close)"), CompileOptions::default());
            assert!(result.success, "{:?}", result.diagnostics);
            assert_eq!(
                result
                    .diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.code.as_deref() == Some("W0004")),
                expected == "series",
                "{:?}",
                result.diagnostics
            );
        }
    }
}
