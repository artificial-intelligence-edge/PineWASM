//! Public compiler entry points must retain ordinary log calls across rollback.

use pinewasm_compiler::{
    compile, run_program, CompileOptions, IncrementalSession, LogEntry, MarketData, RealtimeSim,
    RunOptions, Tick,
};
use pinewasm_hir::LogLevel;

const SOURCE: &str = r#"//@version=6
indicator("Ordinary log sites")
if bar_index >= 2
    log.info("info {0}", close)
    log.warning("warning {0}", close)
    log.error("error {0}", close)
plot(close)
"#;

#[test]
fn ordinary_log_sites_keep_exact_levels_messages_and_bars_in_both_public_paths() {
    let compiled = compile(SOURCE, CompileOptions::default());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    let program = compiled.program.unwrap();
    let history = MarketData {
        open: vec![10.0, 11.0],
        high: vec![10.0, 11.0],
        low: vec![10.0, 11.0],
        close: vec![10.0, 11.0],
        volume: vec![1.0, 1.0],
        time: vec![1_700_000_000_000, 1_700_000_060_000],
        ..MarketData::default()
    };
    let ticks = [
        (2, 12.0, false),
        (2, 13.0, false),
        (2, 14.0, true),
        (3, 15.0, true),
    ]
    .into_iter()
    .map(|(bar_index, close, is_bar_close)| Tick {
        bar_index,
        open: close,
        high: close,
        low: close,
        close,
        volume: 1.0,
        time: 1_700_000_000_000 + i64::try_from(bar_index).unwrap() * 60_000,
        trading_day: None,
        is_bar_close,
    })
    .collect::<Vec<_>>();
    // These are independent expectations from the declared source sites and
    // tick input, including the first committed bar after repeated rollback.
    let expected = [
        (LogLevel::Info, 2, "info 12"),
        (LogLevel::Warning, 2, "warning 12"),
        (LogLevel::Error, 2, "error 12"),
        (LogLevel::Info, 2, "info 13"),
        (LogLevel::Warning, 2, "warning 13"),
        (LogLevel::Error, 2, "error 13"),
        (LogLevel::Info, 2, "info 14"),
        (LogLevel::Warning, 2, "warning 14"),
        (LogLevel::Error, 2, "error 14"),
        (LogLevel::Info, 3, "info 15"),
        (LogLevel::Warning, 3, "warning 15"),
        (LogLevel::Error, 3, "error 15"),
    ]
    .into_iter()
    .map(|(level, bar, message)| LogEntry {
        level,
        bar,
        message: message.to_string(),
    })
    .collect::<Vec<_>>();
    let defaults = RunOptions::default();
    let mut incremental = IncrementalSession::new(&program, &history, &defaults).unwrap();
    assert!(incremental.result().logs.is_empty());
    for (index, update) in ticks.iter().enumerate() {
        let result = incremental.advance(std::slice::from_ref(update));
        assert!(
            result.runtime_errors.is_empty(),
            "{:?}",
            result.runtime_errors
        );
        assert_eq!(result.logs, expected[..3 * (index + 1)]);
    }
    let one_shot = run_program(
        &program,
        &history,
        &RunOptions {
            realtime: Some(RealtimeSim { ticks }),
            ..defaults
        },
    );
    assert!(
        one_shot.runtime_errors.is_empty(),
        "{:?}",
        one_shot.runtime_errors
    );
    assert_eq!(one_shot.logs, expected);
    assert_eq!(incremental.result().logs, one_shot.logs);
}
