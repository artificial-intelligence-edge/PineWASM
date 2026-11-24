//! Table missingness and collection identity are tested separately.

#[path = "support/table_identity_case.rs"]
mod table_identity_case;

use pinewasm_compiler::{
    compile_and_run, CompileOptions, MarketData, RunOptions, RunResult, RuntimeErrorKind, Value,
};

fn number(value: &Value) -> f64 {
    match value {
        Value::Float(value) => *value,
        Value::Int(value) => *value as f64,
        other => panic!("expected a finite numeric plot, got {other:?}"),
    }
}

fn run_source(source: &str) -> RunResult {
    let market = MarketData {
        open: vec![10.0],
        high: vec![11.0],
        low: vec![9.0],
        close: vec![10.0],
        volume: vec![1.0],
        time: vec![1_704_067_200_000],
        ..MarketData::default()
    };
    let run = compile_and_run(
        source,
        &market,
        CompileOptions::default(),
        &RunOptions {
            max_instructions_per_bar: 200_000,
            max_instructions_per_run: 2_000_000,
            max_collection_elements: 10_000,
            max_string_bytes_per_run: 1_000_000,
            ..RunOptions::default()
        },
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
fn table_collection_identity_survives_displacement() {
    let run = run_source(table_identity_case::SOURCE);
    assert_eq!(run.runtime_errors.len(), 1, "{:?}", run.runtime_errors);
    let error = &run.runtime_errors[0];
    assert_eq!(error.kind, RuntimeErrorKind::UserError);
    assert_eq!(error.bar_index, Some(0));
    assert_eq!(
        error.message,
        "IDENTITY | a-na=true;b-na=true;c-na=false;index-na=-1;index-a=0;index-b=1;index-c=2;includes-na=false"
    );
}

#[test]
fn identity_survives_aliases_container_reads_and_duplicate_search_positions() {
    // These are authored propagation cases for the generic invariant. The
    // shared source above is not modified by these cases.
    let source = r#"//@version=6
indicator("Retired identity routes")
type Holder
    table id
f_id(table id) => id
var table first = table.new(position.top_left, 1, 1)
var table alias = first
var table other = table.new(position.top_right, 1, 1)
var array<table> refs = array.from(first, other, first)
var map<string, table> saved = map.new<string, table>()
map.put(saved, "first", first)
map.put(saved, "other", other)
var matrix<table> grid = matrix.new<table>(1, 2, first)
matrix.set(grid, 0, 1, other)
var Holder holder = Holder.new(first)
var array<table> copied_refs = array.copy(refs)
var Holder copied_holder = Holder.copy(holder)
var table replacement_first = table.new(position.top_left, 1, 1)
var table replacement_second = table.new(position.top_right, 1, 1)
plot(array.indexof(refs, first))
plot(array.lastindexof(refs, first))
plot(array.indexof(refs, alias))
plot(array.indexof(refs, other))
plot(array.includes(refs, first) ? 1 : 0)
plot(array.includes(refs, other) ? 1 : 0)
plot(array.indexof(refs, na))
plot(array.includes(refs, na) ? 1 : 0)
plot(array.indexof(refs, map.get(saved, "first")))
plot(array.indexof(refs, map.get(saved, "other")))
plot(array.indexof(refs, matrix.get(grid, 0, 0)))
plot(array.indexof(refs, matrix.get(grid, 0, 1)))
plot(array.indexof(refs, holder.id))
plot(array.indexof(refs, array.get(copied_refs, 0)))
plot(array.indexof(refs, copied_holder.id))
plot(array.indexof(refs, f_id(first)))
plot(array.indexof(refs, replacement_first))
plot(array.indexof(refs, replacement_second))
"#;
    let run = run_source(source);
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    let expected = [
        0., 2., 0., 1., 1., 1., -1., 0., 0., 1., 0., 1., 0., 0., 0., 0., -1., -1.,
    ];
    assert_eq!(run.plots.len(), expected.len());
    for (plot, value) in run.plots.iter().zip(expected) {
        assert_eq!(plot.values.len(), 1);
        assert_eq!(number(&plot.values[0]), value);
    }
}

#[test]
fn active_handle_identity_and_na_query_remain_distinct() {
    let run = run_source(
        r#"//@version=6
indicator("Active identity checks")
var table first = table.new(position.top_left, 1, 1)
var table alias = first
var table other = table.new(position.top_right, 1, 1)
var array<table> refs = array.from(first, other, first)
plot(array.indexof(refs, alias))
plot(array.lastindexof(refs, alias))
plot(array.indexof(refs, other))
plot(array.indexof(refs, na))
plot(array.includes(refs, na) ? 1 : 0)
plot(na(first) ? 1 : 0)
"#,
    );
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    let expected = [0., 2., 1., -1., 0., 0.];
    assert_eq!(run.plots.len(), expected.len());
    for (plot, value) in run.plots.iter().zip(expected) {
        assert_eq!(plot.values.len(), 1);
        assert_eq!(number(&plot.values[0]), value);
    }
}
