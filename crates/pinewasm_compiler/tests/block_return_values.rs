//! Value-producing blocks may end with a declaration or reassignment.

use pinewasm_compiler::{compile_and_run, CompileOptions, MarketData, RunOptions};

#[test]
fn statement_helpers_execute_trailing_loops_and_nested_effects() {
    let source = r#"//@version=6
indicator("Effect helpers")
var array<float> values = array.new<float>(3, na)
identity(float x) => x
initialize(float x) =>
    if na(array.get(values, 0))
        for i = 0 to 2
            array.set(values, i, x + i)
increment() =>
    for i = 0 to 2
        array.set(values, i, array.get(values, i) + 1)
initialize(identity(close))
increment()
plot(array.sum(values))
"#;
    let market = MarketData {
        open: vec![1.0, 4.0],
        high: vec![1.0, 4.0],
        low: vec![1.0, 4.0],
        close: vec![1.0, 4.0],
        volume: vec![1.0; 2],
        ..MarketData::default()
    };
    let result = compile_and_run(
        source,
        &market,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.compile.as_ref().is_some_and(|c| c.success),
        "{:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    assert_eq!(
        serde_json::to_string(&result.plots[0].values).unwrap(),
        "[9.0,12.0]"
    );
}

#[test]
fn assignment_tails_return_the_value_after_the_write() {
    let source = r#"//@version=6
indicator("Assignment returns")
decl(float x) =>
    float result = x * 2
branch(float x) =>
    float result = 0
    if x > 10
        result := x + 3
    else
        result := x - 2
counter() =>
    var float total = 0
    total += 1
plot(decl(close))
plot(branch(close))
plot(counter())
selected = if close > 10
    float answer = close + 1
else
    float answer = close - 1
plot(selected)
"#;
    let close = vec![10.0, 12.0, 9.0];
    let market = MarketData {
        open: close.clone(),
        high: close.clone(),
        low: close.clone(),
        volume: vec![1.0; close.len()],
        close,
        ..MarketData::default()
    };
    let result = compile_and_run(
        source,
        &market,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.compile.as_ref().is_some_and(|c| c.success),
        "{:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    let values: Vec<_> = result
        .plots
        .iter()
        .map(|plot| serde_json::to_string(&plot.values).unwrap())
        .collect();
    assert_eq!(
        values,
        [
            "[20.0,24.0,18.0]",
            "[8.0,15.0,7.0]",
            "[1.0,2.0,3.0]",
            "[9.0,13.0,8.0]"
        ]
    );
}

#[test]
fn parentheses_wrap_at_any_indent_without_changing_block_scope() {
    let source = r#"//@version=6
indicator(
    "Wrapped expressions",
        overlay=true
)
twice(
    float x
) =>
    float y = (
        x +
// A comment and a closing parenthesis can start at column zero.
x
)
    y
plot(
    twice(close),
        color=color.new(
color.red,
0
)
)
plot(close + 1)
"#;
    let market = MarketData {
        open: vec![1.0, 4.0],
        high: vec![1.0, 4.0],
        low: vec![1.0, 4.0],
        close: vec![1.0, 4.0],
        volume: vec![1.0; 2],
        ..MarketData::default()
    };
    let result = compile_and_run(
        source,
        &market,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.compile.as_ref().is_some_and(|c| c.success),
        "{:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    assert_eq!(
        serde_json::to_string(&result.plots[0].values).unwrap(),
        "[2.0,8.0]"
    );
    assert_eq!(
        serde_json::to_string(&result.plots[1].values).unwrap(),
        "[2.0,5.0]"
    );
}

#[test]
fn grouped_and_chained_statement_calls_discard_their_result() {
    let source = r#"//@version=6
indicator("Wrapped effect helpers")
var array<float> values = array.new<float>(3, 0.0)
increment() =>
    for i = 0 to 2
        array.set(values, i, array.get(values, i) + 1)
method bump(array<float> self) =>
    for i = 0 to 2
        array.set(self, i, array.get(self, i) + 10)
current() => values
(increment())
current().bump()
plot(array.sum(values))
"#;
    let market = MarketData {
        open: vec![1.0, 4.0],
        high: vec![1.0, 4.0],
        low: vec![1.0, 4.0],
        close: vec![1.0, 4.0],
        volume: vec![1.0; 2],
        ..MarketData::default()
    };
    let result = compile_and_run(
        source,
        &market,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.compile.as_ref().is_some_and(|c| c.success),
        "{:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    assert_eq!(
        serde_json::to_string(&result.plots[0].values).unwrap(),
        "[33.0,66.0]"
    );
}
