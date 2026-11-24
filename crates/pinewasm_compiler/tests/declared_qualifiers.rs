//! Explicit variable qualifiers survive parsing and constrain initialization/writes.

use pinewasm_compiler::{compile, compile_and_run, CompileOptions, MarketData, RunOptions};

#[test]
fn declared_qualifiers_preserve_values_and_input_metadata() {
    let source = r#"//@version=6
indicator("Declared qualifiers")
const string GROUP = "Filter"
const color LINE = #123456
simple int length = input.int(2, group=GROUP)
series float src = close
simple float scale = 2.0
plot(ta.ema(src, length) * scale, color=LINE)
const = 3
simple = 4
series = 5
plot(const + simple + series)
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
        "[null,5.0]"
    );
    assert_eq!(
        serde_json::to_string(&result.plots[1].values).unwrap(),
        "[12.0,12.0]"
    );
    let program = result.compile.unwrap().program.unwrap();
    assert_eq!(program.inputs[0].group.as_deref(), Some("Filter"));
}

#[test]
fn explicit_qualifiers_reject_stronger_initializers_and_const_rebinding() {
    for declaration in [
        "const float x = close",
        "simple float x = close",
        "const float x = 1.0\nx := 2.0",
        "const float x = 1.0\nx += 2.0",
        "simple float x = 1.0\nx := close",
        "series int length = input.int(2)\nx = ta.ema(close, length)",
    ] {
        let source =
            format!("//@version=6\nindicator(\"Invalid qualifier\")\n{declaration}\nplot(x)\n");
        let result = compile(&source, CompileOptions::default());
        assert!(!result.success, "unexpectedly accepted {declaration}");
        assert!(
            result.diagnostics.iter().any(|d| matches!(
                d.code.as_deref(),
                Some("E0217" | "E0218" | "E0219" | "E0226")
            )),
            "{:?}",
            result.diagnostics
        );
    }
}

#[test]
fn const_reference_permits_content_changes_but_not_rebinding() {
    let accepted = r#"//@version=6
indicator("Const reference")
const array<float> values = array.new<float>(1, 0.0)
array.set(values, 0, close)
plot(array.get(values, 0))
"#;
    let result = compile(accepted, CompileOptions::default());
    assert!(result.success, "{:?}", result.diagnostics);
    let rejected = accepted.replace(
        "array.set(values, 0, close)",
        "values := array.new<float>(1, close)",
    );
    let result = compile(&rejected, CompileOptions::default());
    assert!(!result.success);
    assert!(result
        .diagnostics
        .iter()
        .any(|d| d.code.as_deref() == Some("E0217")));
}

#[test]
fn declared_simple_rejects_writes_nested_below_the_declaration() {
    let nested = r#"//@version=6
indicator("Nested simple write")
simple float x = 1.0
if close > open
    x := 2.0
plot(x)
"#;
    let result = compile(nested, CompileOptions::default());
    assert!(!result.success, "a nested write made `x` series");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0218")),
        "{:?}",
        result.diagnostics
    );

    // A write at the declaration's own depth, even inside a branch, keeps it.
    let same_level = r#"//@version=6
indicator("Same-level simple write")
z = 0.0
if close > open
    simple float x = 1.0
    x := 2.0
    z := x
plot(z)
"#;
    let result = compile(same_level, CompileOptions::default());
    assert!(result.success, "{:?}", result.diagnostics);
}
