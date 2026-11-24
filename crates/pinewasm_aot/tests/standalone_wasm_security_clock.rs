//! Standalone WebAssembly strategies over long intraday feeds.
//!
//! Three regressions, each on gapless 15-minute bars:
//!
//! 1. **No look-ahead.** With `lookahead_off`, an hourly
//!    `request.security` close changes on the hour's last chart bar (the
//!    :45 bar, which closes at :00), so an order placed on the change fills
//!    at the next bar's open, on the hour. Hourly buckets walked from the
//!    feed's first bar would expose the close one bar early, so a feed
//!    starting at :45 would fill at :45.
//! 2. **Clock buckets.** 60-minute, 240-minute and daily buckets follow the
//!    clock, so a feed starting at 00:15 trades exactly like one starting at
//!    00:00 once both have warmed up. The `pinewasm_vm` interpreter buckets
//!    the same way.
//! 3. **Memory.** A simple strategy runs 250,000 bars (about seven years)
//!    with full results in 192 MiB, and a host memory limit a run
//!    cannot fit in surfaces as the documented allocation error rather than a
//!    trap.

use pinewasm_aot::{
    compile_standalone_wasm_program, StandaloneWasmModuleType, StandaloneWasmOptions,
};
use pinewasm_compiler::{compile, CompileOptions};
use pinewasm_vm::{run, MarketData, RunOptions};

const MINUTE_MS: i64 = 60_000;
const BAR_MS: i64 = 15 * MINUTE_MS;
const DAY_MS: i64 = 86_400_000;
/// 2023-11-15T00:00:00Z.
const MIDNIGHT: i64 = 1_700_006_400_000;

const HOURLY_ROLL: &str = concat!(
    "//@version=6\n",
    "strategy(\"RS 15m->60m\", overlay=true, initial_capital=1000000, default_qty_type=strategy.fixed, default_qty_value=1, pyramiding=1, process_orders_on_close=false)\n",
    "float htfClose = request.security(syminfo.tickerid, \"60\", close, lookahead=barmerge.lookahead_off)\n",
    "bool htfRoll = ta.change(htfClose) != 0\n",
    "if htfRoll\n",
    "    strategy.entry(\"HTF_ROLL\", strategy.long, qty=1)\n",
    "if htfRoll[1]\n",
    "    strategy.close(\"HTF_ROLL\")\n",
);

const MULTI_TIMEFRAME: &str = concat!(
    "//@version=6\n",
    "strategy(\"MTF\", initial_capital=1000000, default_qty_type=strategy.fixed, default_qty_value=1)\n",
    "h1 = request.security(syminfo.tickerid, \"60\", close)\n",
    "h4 = request.security(syminfo.tickerid, \"240\", ta.sma(close, 3))\n",
    "d1 = request.security(syminfo.tickerid, \"D\", close)\n",
    "if ta.change(h1) != 0 and h1 > h4\n",
    "    strategy.entry(\"L\", strategy.long)\n",
    "if ta.change(d1) != 0 or h1 < h4\n",
    "    strategy.close(\"L\")\n",
);

const SIMPLE: &str = concat!(
    "//@version=6\n",
    "strategy(\"Simple\", initial_capital=1000000, default_qty_type=strategy.fixed, default_qty_value=1)\n",
    "fast = ta.sma(close, 10)\n",
    "slow = ta.sma(close, 30)\n",
    "if ta.crossover(fast, slow)\n",
    "    strategy.entry(\"L\", strategy.long)\n",
    "if ta.crossunder(fast, slow)\n",
    "    strategy.close(\"L\")\n",
);

/// `n` gapless 15-minute bars from `start`. Prices depend on the bar's
/// time, not its index, so two feeds that overlap carry identical bars.
fn feed(start: i64, n: usize) -> MarketData {
    let mut data = MarketData {
        timeframe: "15".into(),
        ..MarketData::default()
    };
    for i in 0..n {
        let time = start + i as i64 * BAR_MS;
        let t = (time / BAR_MS) as f64;
        let price = 100.0 + (t * 0.37).sin() * 3.0 + (t * 0.011).sin() * 9.0;
        data.open.push(price);
        data.high.push(price + 1.0);
        data.low.push(price - 1.0);
        data.close.push(price + 0.25 * (t * 1.3).cos());
        data.volume.push(1000.0);
        data.time.push(time);
    }
    data
}

fn program(source: &str) -> pinewasm_csil::CsilProgram {
    let result = compile(source, CompileOptions::all());
    assert!(
        !result
            .diagnostics
            .iter()
            .any(pinewasm_span::Diagnostic::is_error),
        "{:?}",
        result.diagnostics
    );
    result.program.expect("program")
}

fn module(source: &str) -> Vec<u8> {
    compile_standalone_wasm_program(
        &program(source),
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Strategy,
        },
    )
    .expect("strategy standalone WebAssembly emit")
}

/// The IN document with full results, built without a JSON value tree so a
/// quarter-million-bar request stays cheap.
fn request(data: &MarketData) -> String {
    let mut out = String::from(
        "{\"version\":1,\"params\":{},\"include_results\":true,\"timeframe\":\"15\",\"timezone\":\"UTC\",\"session_hours\":\"\",\"bars\":[",
    );
    for i in 0..data.bars() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"time\":{},\"open\":{:?},\"high\":{:?},\"low\":{:?},\"close\":{:?},\"volume\":{:?}}}",
            data.time[i], data.open[i], data.high[i], data.low[i], data.close[i], data.volume[i]
        ));
    }
    out.push_str("]}");
    out
}

/// How one evaluation ended.
#[derive(Debug)]
enum Outcome {
    Ok(serde_json::Value),
    /// The module reported an error through `last_error`.
    Error(String),
    /// The module trapped.
    Trap(String),
}

/// Evaluate `input` on a fresh instance whose host refuses to grow memory
/// past `memory_limit` bytes (`None`: only the module's declared maximum).
fn evaluate(wasm: &[u8], input: &str, memory_limit: Option<usize>) -> Outcome {
    let engine = wasmi::Engine::default();
    let module = wasmi::Module::new(&engine, wasm).expect("wasmi accepts the module");
    let mut limits = wasmi::StoreLimitsBuilder::new();
    if let Some(bytes) = memory_limit {
        limits = limits.memory_size(bytes);
    }
    let mut store = wasmi::Store::new(&engine, limits.build());
    store.limiter(|limits| limits);
    let instance = <wasmi::Linker<wasmi::StoreLimits>>::new(&engine)
        .instantiate_and_start(&mut store, &module)
        .expect("instantiate with an empty linker");
    let memory = instance
        .get_memory(&store, "memory")
        .expect("memory export");
    let ptr = instance
        .get_typed_func::<i32, i32>(&store, "alloc")
        .expect("alloc export")
        .call(&mut store, input.len() as i32)
        .expect("alloc call") as usize;
    if ptr == 0 {
        return Outcome::Error("alloc refused the input".into());
    }
    memory.data_mut(&mut store)[ptr..ptr + input.len()].copy_from_slice(input.as_bytes());
    let packed = match instance
        .get_typed_func::<(i32, i32), i64>(&store, "evaluate_indicator_cabi")
        .expect("evaluate export")
        .call(&mut store, (ptr as i32, input.len() as i32))
    {
        Ok(packed) => packed as u64,
        Err(trap) => return Outcome::Trap(trap.to_string()),
    };
    let slice = |store: &wasmi::Store<_>, packed: u64| {
        let (at, len) = ((packed >> 32) as usize, (packed & 0xFFFF_FFFF) as usize);
        memory.data(store)[at..at + len].to_vec()
    };
    if packed == 0 {
        let error = instance
            .get_typed_func::<(), i64>(&store, "last_error")
            .expect("last_error export")
            .call(&mut store, ())
            .expect("last_error call") as u64;
        return Outcome::Error(String::from_utf8(slice(&store, error)).expect("UTF-8"));
    }
    Outcome::Ok(serde_json::from_slice(&slice(&store, packed)).expect("OUT is JSON"))
}

fn results(wasm: &[u8], data: &MarketData) -> serde_json::Value {
    match evaluate(wasm, &request(data), None) {
        Outcome::Ok(out) => out,
        other => panic!("evaluation failed: {other:?}"),
    }
}

/// `(entry time, exit time)` of every trade in a full-results document.
fn trade_times(out: &serde_json::Value, data: &MarketData) -> Vec<(i64, i64)> {
    let at = |bar: &serde_json::Value| data.time[bar.as_f64().expect("bar index") as usize];
    out["strategy"]["trades"]
        .as_array()
        .expect("trades")
        .iter()
        .map(|trade| (at(&trade["entryBar"]), at(&trade["exitBar"])))
        .collect()
}

/// The same pairs from the `pinewasm_vm` interpreter.
fn vm_trade_times(source: &str, data: &MarketData) -> Vec<(i64, i64)> {
    let outcome = run(&program(source), data, &RunOptions::default());
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    outcome
        .strategy
        .expect("strategy outcome")
        .trades
        .iter()
        .map(|trade| {
            (
                data.time[trade.entry_bar as usize],
                data.time[trade.exit_bar as usize],
            )
        })
        .collect()
}

#[test]
fn lookahead_off_hourly_close_fills_on_the_hour_wherever_the_feed_starts() {
    let wasm = module(HOURLY_ROLL);
    for first_bar in 0..4 {
        let data = feed(MIDNIGHT + first_bar * BAR_MS, 200);
        let trades = trade_times(&results(&wasm, &data), &data);
        assert!(
            trades.len() > 40,
            "start +{first_bar} bars: {} trades",
            trades.len()
        );
        for (entry, exit) in &trades {
            assert_eq!(
                entry.rem_euclid(3_600_000),
                0,
                "start +{first_bar} bars: entry at {entry}"
            );
            assert_eq!(*exit - *entry, BAR_MS, "start +{first_bar} bars");
        }
        assert_eq!(
            vm_trade_times(HOURLY_ROLL, &data),
            trades,
            "start +{first_bar} bars"
        );
    }
}

#[test]
fn higher_timeframe_buckets_follow_the_clock_not_the_first_bar() {
    let wasm = module(MULTI_TIMEFRAME);
    // Four days of warm-up, then compare everything after it.
    let warm = MIDNIGHT + 4 * DAY_MS;
    let after = |trades: Vec<(i64, i64)>| -> Vec<(i64, i64)> {
        trades
            .into_iter()
            .filter(|(entry, _)| *entry >= warm)
            .collect()
    };
    let midnight = feed(MIDNIGHT, 30 * 96);
    let quarter_past = feed(MIDNIGHT + BAR_MS, 30 * 96 - 1);
    let aot = after(trade_times(&results(&wasm, &midnight), &midnight));
    assert!(aot.len() > 20, "{} trades", aot.len());
    assert_eq!(
        after(trade_times(&results(&wasm, &quarter_past), &quarter_past)),
        aot
    );
    assert_eq!(after(vm_trade_times(MULTI_TIMEFRAME, &midnight)), aot);
    assert_eq!(after(vm_trade_times(MULTI_TIMEFRAME, &quarter_past)), aot);
}

#[test]
fn a_quarter_million_intraday_bars_run_without_trapping() {
    // Holding about 1 KiB a bar would need most of the 256 MiB maximum, and
    // longer feeds would trap. Hold it to three quarters.
    let data = feed(MIDNIGHT, 250_000);
    match evaluate(&module(SIMPLE), &request(&data), Some(192 << 20)) {
        Outcome::Ok(out) => {
            assert!(out["strategy"]["trades"].as_array().expect("trades").len() > 1_000);
        }
        other => panic!("evaluation failed: {other:?}"),
    }
}

#[test]
fn a_host_memory_limit_is_a_documented_error_not_a_trap() {
    let wasm = module(MULTI_TIMEFRAME);
    let input = request(&feed(MIDNIGHT, 20_000));
    let mut refused = 0;
    for limit_mib in [4, 6, 8, 10, 12, 16, 24] {
        match evaluate(&wasm, &input, Some(limit_mib << 20)) {
            Outcome::Ok(_) => {}
            Outcome::Error(message) => {
                assert!(
                    message.starts_with("memory allocation failed: "),
                    "{limit_mib} MiB: {message}"
                );
                refused += 1;
            }
            Outcome::Trap(trap) => panic!("{limit_mib} MiB: trapped: {trap}"),
        }
    }
    assert!(refused > 0, "no limit was tight enough to refuse the run");
    assert!(
        matches!(evaluate(&wasm, &input, Some(64 << 20)), Outcome::Ok(_)),
        "the run fits in 64 MiB"
    );
}
