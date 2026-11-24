//! Complete v5 fixtures in `tests/regression/`, each paired with its expected
//! `-v6.pine` migration. For every fixture the tests check that migrating
//! the v5 source reproduces the golden exactly, that the golden is already v6
//! (idempotence), and that running the migrated script over synthetic bars
//! reproduces a plot trace derived by hand from v5 semantics. The expected
//! traces are written from the v5 rules.

use pinewasm_compiler::{compile_and_run, CompileOptions, MarketData, RunOptions, Value};
use pinewasm_migrate::{migrate, MigrationOptions, Status};
use pinewasm_span::Severity;

const V5: &str = include_str!("../../../tests/regression/migration-parity-runtime-audit-v5.pine");
const PINEWASM_V6: &str =
    include_str!("../../../tests/regression/migration-parity-runtime-audit-v6.pine");
const LOOP_V5: &str = include_str!("../../../tests/regression/migration-parity-loop-bound-v5.pine");
const LOOP_PINEWASM_V6: &str =
    include_str!("../../../tests/regression/migration-parity-loop-bound-v6.pine");

fn run(script: &str, closes: &[f64]) -> Vec<Vec<Option<f64>>> {
    let bars = closes.len();
    let market = MarketData {
        open: vec![0.0; bars],
        high: closes.to_vec(),
        low: closes.to_vec(),
        close: closes.to_vec(),
        volume: vec![100.0; bars],
        time: (0..bars).map(|index| index as i64 * 60_000).collect(),
        mintick: 0.01,
        ..Default::default()
    };
    let result = compile_and_run(
        script,
        &market,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    let errors: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == Severity::Error)
        .collect();
    assert!(errors.is_empty(), "{errors:?}\n{script}");
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}\n{script}",
        result.runtime_errors
    );
    result
        .plots
        .iter()
        .map(|plot| {
            plot.values
                .iter()
                .map(|value| match value {
                    Value::Float(value) => Some(*value),
                    Value::Int(value) => Some(*value as f64),
                    Value::Na => None,
                    other => panic!("unexpected plot value {other:?}"),
                })
                .collect()
        })
        .collect()
}

/// Migrates `v5`, checks the exact golden and idempotence, and returns the
/// migrated script's plot trace.
fn migrate_and_run(v5: &str, golden: &str, closes: &[f64]) -> Vec<Vec<Option<f64>>> {
    let migrated = migrate(v5, &MigrationOptions::default());
    assert_eq!(
        migrated.status,
        Status::Migrated,
        "{:?}",
        migrated.parse_errors
    );
    assert!(!migrated
        .findings
        .iter()
        .any(|finding| finding.rule == "internal"));
    assert_eq!(migrated.output, golden);
    let again = migrate(golden, &MigrationOptions::default());
    assert_eq!(again.status, Status::AlreadyV6);
    assert_eq!(again.output, golden);
    run(&migrated.output, closes)
}

#[test]
fn runtime_audit_script_preserves_v5_runtime_traces() {
    let closes = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
    let local = migrate_and_run(V5, PINEWASM_V6, &closes);
    // Both operands of `and` are evaluated, so `tick()` runs on every bar and
    // the persistent counter reaches bar_index + 1. `var firstMissing` is
    // set once on the first bar, where `5[1]` has no history and is `na`.
    // `5 / 2` divides two const ints and truncates to 2.
    let expected_v5_trace = vec![
        vec![
            Some(1.0),
            Some(2.0),
            Some(3.0),
            Some(4.0),
            Some(5.0),
            Some(6.0),
        ],
        vec![
            Some(1.0),
            Some(2.0),
            Some(3.0),
            Some(4.0),
            Some(5.0),
            Some(6.0),
        ],
        vec![Some(1.0); 6],
        vec![Some(2.0); 6],
    ];
    assert_eq!(local, expected_v5_trace);
}

#[test]
fn changing_loop_bound_script_preserves_v5_runtime_trace() {
    let closes = [1.0, 2.0, 3.0];
    let local = migrate_and_run(LOOP_V5, LOOP_PINEWASM_V6, &closes);
    // The end bound `array.size(a) - 1` is evaluated once at loop entry, so the
    // loop runs for i = 0, 1, 2 and pops all three elements: 3 + 2 + 1 = 6.
    // The array is recreated on every bar. A bound re-evaluated after each
    // pop would stop after two iterations with 3 + 2 = 5.
    assert_eq!(local, vec![vec![Some(6.0); closes.len()]]);
}
