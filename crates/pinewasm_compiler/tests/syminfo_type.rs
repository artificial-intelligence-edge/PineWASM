use pinewasm_compiler::{compile_and_run, CompileOptions, MarketData, RunOptions, Value};

#[test]
fn syminfo_type_parses_and_reads_host_symbol_kind() {
    let source = r#"//@version=6
indicator("Symbol Type")
isForex = syminfo.type == "forex"
plot(isForex ? 1.0 : 0.0)
"#;
    let mut data = MarketData {
        open: vec![1.0],
        high: vec![1.1],
        low: vec![0.9],
        close: vec![1.0],
        volume: vec![0.0],
        ..MarketData::default()
    };
    data.syminfo.kind = "forex".to_string();

    let run = compile_and_run(
        source,
        &data,
        CompileOptions::default(),
        &RunOptions::default(),
    );

    assert!(
        run.compile.as_ref().is_some_and(|result| result.success),
        "compile diagnostics: {:?}",
        run.diagnostics
    );
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    assert_eq!(run.bars_executed, 1);
    assert_eq!(run.plots[0].values, vec![Value::Float(1.0)]);
}
