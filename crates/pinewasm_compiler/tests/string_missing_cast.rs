//! Tests for raw typed-missing storage and existing formatting.
//! plotchar metadata checks live in a separate test.
use pinewasm_compiler::{compile_and_run, CompileOptions, MarketData, RunOptions, Value};

#[test]
fn raw_typed_missing_storage_remains_absent_while_formatting_remains_present() {
    let source = r#"//@version=6
indicator("Missing string cast")
string missing = na
plot(na(missing) ? 1.0 : 0.0)
plot(str.tostring(na) == "NaN" ? 1.0 : 0.0)
plot(string(12) == "12" ? 1.0 : 0.0)
plot(string("NaN") == "NaN" ? 1.0 : 0.0)
"#;
    let data = MarketData {
        open: vec![1.0; 2],
        high: vec![1.0; 2],
        low: vec![1.0; 2],
        close: vec![1.0; 2],
        volume: vec![0.0; 2],
        ..MarketData::default()
    };
    let result = compile_and_run(
        source,
        &data,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.diagnostics.iter().all(|d| !d.is_error()),
        "{:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    assert_eq!(result.plots.len(), 4);
    for plot in result.plots {
        assert_eq!(plot.values, vec![Value::Float(1.0); 2]);
    }
}
