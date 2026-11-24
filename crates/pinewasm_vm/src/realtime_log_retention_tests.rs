//! Log entries are cumulative even when the script state rolls back.
use super::{compile, data_from_close, floats, tick};
use crate::{run, IncrementalSession, RealtimeSim, RunOptions, RuntimeErrorKind};
use pinewasm_hir::LogLevel;
use pinewasm_mir::Value;
use pretty_assertions::assert_eq;

const SOURCE: &str = r#"//@version=6
indicator("Cumulative realtime logs", overlay=true)
var int bars = 0
varip int ticks = 0
bars += 1
ticks += 1
if barstate.isrealtime
    log.info("tick {0}", ticks)
    line.new(bar_index, close, bar_index + 1, close + 1)
    alert("tick", alert.freq_all)
plot(bars)
plot(ticks)
"#;

fn expected_logs() -> Vec<crate::LogEntry> {
    ["tick 3", "tick 4", "tick 5"]
        .into_iter()
        .map(|message| crate::LogEntry {
            level: LogLevel::Info,
            bar: 2,
            message: message.to_string(),
        })
        .collect()
}

#[test]
fn incremental_ticks_retain_exact_log_events_while_state_rolls_back() {
    let program = compile(SOURCE);
    let data = data_from_close(&[10.0, 11.0]);
    let options = RunOptions::default();
    let mut session = IncrementalSession::new(&program, &data, &options).unwrap();
    assert!(session.result().logs.is_empty());
    let expected = expected_logs();
    for (index, update) in [
        tick(2, 12.0, false),
        tick(2, 13.0, false),
        tick(2, 14.0, true),
    ]
    .into_iter()
    .enumerate()
    {
        let result = session.advance(&[update]);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.logs, expected[..=index]);
    }
    let result = session.result();
    assert_eq!(result.bars_executed, 3);
    assert_eq!(
        floats(&result.plots[0].values),
        vec![Some(1.0), Some(2.0), Some(3.0)]
    );
    assert_eq!(
        floats(&result.plots[1].values),
        vec![Some(1.0), Some(2.0), Some(5.0)]
    );
    // Drawings still roll back, and the alert policy is deliberately unchanged.
    assert_eq!(result.drawings.lines.len(), 1);
    assert_eq!(result.alerts.len(), 1);
    assert_eq!(result.alerts[0].bar, 2);
    assert_eq!(result.alerts[0].message, "tick");
}

#[test]
fn one_shot_and_incremental_log_streams_agree_with_the_declared_event_sequence() {
    let program = compile(SOURCE);
    let data = data_from_close(&[10.0, 11.0]);
    let ticks = vec![
        tick(2, 12.0, false),
        tick(2, 13.0, false),
        tick(2, 14.0, true),
    ];
    let one_shot = run(
        &program,
        &data,
        &RunOptions {
            realtime: Some(RealtimeSim {
                ticks: ticks.clone(),
            }),
            ..RunOptions::default()
        },
    );
    let mut incremental = IncrementalSession::new(&program, &data, &RunOptions::default()).unwrap();
    let result = incremental.advance(&ticks);
    assert!(one_shot.errors.is_empty(), "{:?}", one_shot.errors);
    assert_eq!(one_shot.logs, expected_logs());
    assert_eq!(result, one_shot);
}

fn append_literal_log(vm: &mut crate::Vm, message: &str) -> Result<(), crate::RuntimeError> {
    vm.stack.push(Value::String(message.to_string()));
    vm.exec_log(LogLevel::Info, 1, 0)
}

#[test]
fn raw_legacy_restore_remains_a_counterfactual_for_the_log_policy() {
    let program = compile("indicator('Counterfactual')\nplot(close)\n");
    let data = data_from_close(&[1.0]);
    let defaults = RunOptions::default();
    let mut current = crate::Vm::new(&program, &data, &defaults).unwrap();
    let mut legacy = crate::Vm::new(&program, &data, &defaults).unwrap();
    let current_snapshot = current.snapshot(0);
    let baseline_snapshot = legacy.snapshot_reference(0);
    for vm in [&mut current, &mut legacy] {
        append_literal_log(vm, "first").unwrap();
        append_literal_log(vm, "second").unwrap();
    }
    current.restore(0, &current_snapshot).unwrap();
    legacy.restore_reference(0, &baseline_snapshot);
    assert_eq!(
        current
            .logs
            .iter()
            .map(|entry| entry.message.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert!(
        legacy.logs.is_empty(),
        "the alternate restore path must clear its logs"
    );
    // Restore must keep log entries recorded before the rollback.
}

#[test]
fn string_limit_still_stops_growth_without_discarding_prior_log_events() {
    let program = compile("indicator('String limit')\nplot(close)\n");
    let data = data_from_close(&[1.0]);
    let options = RunOptions {
        max_string_bytes_per_run: 2,
        ..RunOptions::default()
    };
    let mut vm = crate::Vm::new(&program, &data, &options).unwrap();
    let snapshot = vm.snapshot(0);
    for _ in 0..2 {
        append_literal_log(&mut vm, "x").unwrap();
        vm.restore(0, &snapshot).unwrap();
    }
    assert_eq!(vm.string_bytes_allocated, 2);
    let before = vm.logs.clone();
    let error = append_literal_log(&mut vm, "x").unwrap_err();
    assert_eq!(error.kind, RuntimeErrorKind::StringLimitExceeded);
    assert_eq!(vm.logs, before);
    assert_eq!(vm.logs.len(), 2);
}

#[test]
fn event_entry_memory_limit_is_cumulative_across_rollbacks() {
    let program = compile("indicator('Entry limit')\nplot(close)\n");
    let data = data_from_close(&[1.0]);
    let options = RunOptions::default();
    let mut vm = crate::Vm::new(&program, &data, &options).unwrap();
    let snapshot = vm.snapshot(0);
    let cost = std::mem::size_of::<crate::LogEntry>() + 1;
    vm.bytes_reserved = options.max_vm_bytes_per_run - 2 * cost;
    for _ in 0..2 {
        append_literal_log(&mut vm, "x").unwrap();
        vm.restore(0, &snapshot).unwrap();
    }
    let before = vm.logs.clone();
    let error = append_literal_log(&mut vm, "x").unwrap_err();
    assert_eq!(error.kind, RuntimeErrorKind::VmMemoryLimitExceeded);
    assert_eq!(vm.logs, before);
    assert_eq!(vm.logs.len(), 2);
}

#[test]
fn a_later_runtime_error_keeps_logs_from_all_executed_ticks() {
    let program = compile("indicator('Stop after log')\nif barstate.isrealtime\n    log.info('close {0}', close)\n    if close > 12\n        runtime.error('stop')\nplot(close)\n");
    let data = data_from_close(&[10.0, 11.0]);
    let mut session = IncrementalSession::new(&program, &data, &RunOptions::default()).unwrap();
    session.advance(&[tick(2, 12.0, false)]);
    let failed = session.advance(&[tick(2, 13.0, false)]);
    assert_eq!(failed.errors.len(), 1);
    assert_eq!(failed.errors[0].kind, RuntimeErrorKind::UserError);
    assert_eq!(
        failed.logs,
        vec![
            crate::LogEntry {
                level: LogLevel::Info,
                bar: 2,
                message: "close 12".to_string()
            },
            crate::LogEntry {
                level: LogLevel::Info,
                bar: 2,
                message: "close 13".to_string()
            },
        ]
    );
    let subsequent = session.advance(&[tick(2, 14.0, true)]);
    assert_eq!(subsequent.logs, failed.logs);
    assert_eq!(subsequent.errors, failed.errors);
}
