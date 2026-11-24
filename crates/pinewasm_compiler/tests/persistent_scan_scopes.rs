//! Local behavioral safeguards for scoped persistent-write analysis. These are
//! not compatibility claims or wall-clock performance thresholds.

use pinewasm_compiler::{compile, compile_and_run, CompileOptions, MarketData, RunOptions, Value};

fn program(body: &str) -> String {
    // Completed sibling scopes leave declaration metadata before the active
    // globals below. Their string facts must not affect subsequent joins.
    format!(
        "//@version=6\nindicator(\"Persistent scan scopes\")\nif close > open\n    retired = \"first completed scope\"\nif close < open\n    retired = \"second completed scope\"\n{body}\n"
    )
}

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

fn assert_last_plot(body: &str, expected: &[f64]) {
    let run = compile_and_run(
        &program(body),
        &market(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        run.diagnostics.iter().all(|d| !d.is_error()),
        "{body}: {:?}",
        run.diagnostics
    );
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    assert_eq!(run.bars_executed, 4);
    let values = &run.plots.last().expect("one output").values;
    assert_eq!(values.len(), expected.len());
    for (actual, expected) in values.iter().zip(expected) {
        assert!(
            matches!(actual, Value::Float(actual) if (actual - expected).abs() < 1e-12),
            "{body}: {values:?}"
        );
    }
}

fn assert_carried_rejection(body: &str) {
    let compiled = compile(&program(body), CompileOptions::default());
    assert!(!compiled.success, "carried dependency was lost: {body}");
    assert!(
        compiled.diagnostics.iter().any(|diagnostic| {
            diagnostic.code.as_deref() == Some("E0206")
                && diagnostic.message.ends_with("found series")
        }),
        "{body}: {:?}",
        compiled.diagnostics
    );
}

#[test]
fn sibling_string_locals_keep_their_runtime_values_without_leaking_names() {
    let mut body = String::from("total = 0.0\n");
    let text = "x".repeat(64);
    for _ in 0..64 {
        body.push_str(&format!(
            "if close > open\n    fact = \"{text}\"\n    total := total + str.length(fact)\nelse\n    fact = \"x\"\n    total := total + str.length(fact)\n"
        ));
    }
    // Reusing the same name globally must resolve a new declaration.
    body.push_str("fact = \"end\"\nplot(total + str.length(fact))");
    assert_last_plot(&body, &[4099.0, 67.0, 4099.0, 67.0]);
}

#[test]
fn nested_shadowed_closed_aliases_do_not_replace_the_outer_guard() {
    assert_last_plot(
        r#"selector = input.bool(true)
never = false
marker = "outer"
observed = 0.0
if selector
    never = true
    marker = "middle"
    if close > open
        marker = "inner"
        observed := str.length(marker)
    else
        observed := str.length(marker)
    if never
        observed := observed + 10
var bool flag = true
plot(close, force_overlay=flag)
if never
    flag := false
never := false
plot(observed + str.length(marker) + (flag ? 100 : 0))"#,
        &[120.0, 121.0, 120.0, 121.0],
    );
}

#[test]
fn a_closed_alias_assigned_out_of_a_completed_scope_remains_available() {
    assert_last_plot(
        r#"alias = "before"
if true
    local = "retired"
    alias := local
if close > open
    local = "unrelated"
else
    local = "other"
var bool flag = true
plot(close, force_overlay=flag)
if alias != "retired"
    flag := false
plot(str.length(alias) + (flag ? 100 : 0))"#,
        &[107.0; 4],
    );
}

#[test]
fn matching_branch_facts_keep_a_later_guard_closed() {
    assert_last_plot(
        r#"selector = input.bool(true)
never = true
if selector
    local = "left"
    never := false
else
    local = "right"
    never := false
var bool flag = true
plot(close, force_overlay=flag)
if never
    flag := false
plot(flag ? 1 : 0)"#,
        &[1.0; 4],
    );
}

#[test]
fn differing_or_absent_branch_writes_keep_a_possible_carried_dependency() {
    for branches in [
        "if selector\n    local = \"left\"\n    gate := true\nelse\n    local = \"right\"\n    gate := false",
        "if selector\n    local = \"left\"\n    gate := true",
    ] {
        assert_carried_rejection(&format!(
            "selector = input.bool(true)\ngate = false\n{branches}\nvar bool flag = true\nplot(close, force_overlay=flag)\nif gate\n    flag := false\nplot(flag ? 1 : 0)"
        ));
    }
}

#[test]
fn switch_alternatives_cannot_inherit_a_previous_alternatives_mutable_fact() {
    assert_carried_rejection(
        r#"selector = input.int(0)
gate = true
var bool flag = true
plot(close, force_overlay=flag)
switch selector
    0 =>
        local = "first"
        gate := false
    =>
        local = "second"
        if gate
            flag := false
plot(flag ? 1 : 0)"#,
    );
}

#[test]
fn loop_clearing_keeps_mutable_guards_from_hiding_later_iteration_writes() {
    for loop_header in [
        "for index = 0 to 1",
        "for item in array.from(0, 1)",
        "while counter < 2",
    ] {
        assert_carried_rejection(&format!(
            "gate = false\ncounter = 0\nvar bool flag = true\nplot(close, force_overlay=flag)\n{loop_header}\n    local = \"loop local\"\n    if gate\n        flag := false\n    gate := true\n    counter := counter + 1\nplot(flag ? 1 : 0)"
        ));
    }
}

#[test]
fn completed_scopes_retain_persistent_write_metadata_for_simple_consumers() {
    assert_last_plot(
        r"result = 0.0
if true
    var int length = 1
    length := 2
    result := nz(ta.ema(close, length), 0.0)
plot(result)",
        &[0.0, 1.5, 11.0 / 6.0, 23.0 / 18.0],
    );
}

#[test]
fn an_unknown_call_keeps_its_conservative_effect_on_a_shadowed_outer_slot() {
    assert_carried_rejection(
        r"noop() => 0
var int shift = 1
shift := 2
if input.bool(true)
    shift = 10
    ignored = noop()
plot(close, offset=shift)",
    );
}
