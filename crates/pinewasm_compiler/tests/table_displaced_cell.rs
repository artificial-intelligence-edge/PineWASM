//! This source calls table.cell on a same-position retired table and then
//! raises a runtime error with a marker message. This regression covers that
//! call and message; it does not cover other table setters.

use pinewasm_compiler::{
    compile_and_run, CompileOptions, MarketData, RunOptions, RunResult, RuntimeErrorKind,
};

fn run_source(source: &str) -> RunResult {
    let market = MarketData {
        open: vec![10.0],
        high: vec![11.0],
        low: vec![9.0],
        close: vec![10.0],
        volume: vec![1.0],
        ..MarketData::default()
    };
    let run = compile_and_run(
        source,
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
    run
}

#[test]
fn retired_table_cell_reaches_the_alias_lifetime_marker() {
    let source = r#"//@version=6
indicator("Table alias lifecycle", overlay=false)
var table first = table.new(position.top_right, 1, 1)
var table alias = first
var table newest = table.new(position.top_right, 1, 1)
if barstate.isfirst
    table.cell(first, 0, 0, "first remains addressable")
    table.cell(newest, 0, 0, "newest")
    runtime.error("MARKER7 | first-na=" + str.tostring(na(first)) + ";alias-na=" + str.tostring(na(alias)) + ";newest-na=" + str.tostring(na(newest)) + ";all-count=" + str.tostring(array.size(table.all)))
plot(0)
"#;
    let run = run_source(source);
    assert_eq!(run.runtime_errors.len(), 1, "{:?}", run.runtime_errors);
    assert_eq!(run.runtime_errors[0].kind, RuntimeErrorKind::UserError);
    assert_eq!(
        run.runtime_errors[0].message,
        "MARKER7 | first-na=true;alias-na=true;newest-na=false;all-count=1"
    );
}

#[test]
fn literal_table_na_still_aborts_before_the_marker9_message() {
    let source = r#"//@version=6
indicator("Literal table na cell", overlay=false)
var table t = na
if barstate.isfirst
    table.cell(t, 0, 0, "ignored")
    runtime.error("MARKER9 | literal-na-cell-continued")
plot(0)
"#;
    let run = run_source(source);
    assert_eq!(run.runtime_errors.len(), 1, "{:?}", run.runtime_errors);
    // The VM reports this na table cell as NaFieldAccess.
    assert_eq!(run.runtime_errors[0].kind, RuntimeErrorKind::NaFieldAccess);
    assert!(!run.runtime_errors[0].message.contains("MARKER9"));
    assert_eq!(run.bars_executed, 0);
}
