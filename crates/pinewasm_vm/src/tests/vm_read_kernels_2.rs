//! Differential tests of the residual builtin
//! series of `Vm::read_builtin` (strategy snapshot, calendar, `barstate.*`,
//! `timeframe.*`, `session.*`, `time_close`/`timenow`/`time_tradingday`, host
//! metadata) through their `vm_ops` kernels, `MarketData::time_close_at` and
//! `trading_day_at`, `input_value_matches_decl` and the external-source
//! sample, against reference implementations of the bodies. The
//! reference implementation of `Vm::read_builtin` is `vm_read_kernels::old_read_builtin`
//! (it calls the implementations below).

#![allow(
    clippy::too_many_lines,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::match_same_arms
)]

use super::super::session::{self, TfClass};
use super::super::*;
use super::vm_read_kernels::{feed_vm, guarded, key, old_read_builtin, program, Rng};
use pinewasm_hir::{BuiltinSeries, ExternalSourceRef, InputDecl, InputKind, InputValue};
use pinewasm_mir::Value;

// ---- reference implementations of the bodies (`self` -> `vm`/`data`) -------

/// Reference copy of `session::timeframe_class`.
pub(super) fn old_timeframe_class(tf: &str) -> TfClass {
    match session::timeframe_parts(tf).1 {
        None => TfClass::Minutes,
        Some('S') => TfClass::Seconds,
        Some('D') => TfClass::Daily,
        Some('W') => TfClass::Weekly,
        Some('M') => TfClass::Monthly,
        Some(_) => TfClass::Daily,
    }
}

/// Reference copy of `session::timeframe_multiplier`.
pub(super) fn old_timeframe_multiplier(tf: &str) -> i64 {
    session::timeframe_parts(tf).0
}

/// Reference copy of `MarketData::time_close_at`.
pub(super) fn old_time_close_at(data: &MarketData, index: usize) -> i64 {
    if !data.time_close.is_empty() {
        return data.time_close[index];
    }
    data.time_at(index)
        .saturating_add(timeframe_in_seconds(&data.timeframe).saturating_mul(1000))
}

/// Reference copy of `MarketData::fallback_trading_day_for_time`.
pub(super) fn old_fallback_trading_day_for_time(data: &MarketData, time: i64) -> i64 {
    // The exchange's offset at `time`, daylight-saving changes included.
    let offset = data.exchange_offset_at(time);
    let local_seconds = time.div_euclid(1000).saturating_add(offset);
    let day_start_local = local_seconds.div_euclid(86_400) * 86_400;
    // PineWASM encodes the inferred exchange-calendar date at 00:00 UTC. It
    // does not return the UTC instant at which local midnight occurred.
    // Only a host-supplied lane can identify the final trading date of an
    // overnight or otherwise non-calendar-day session exactly.
    day_start_local.saturating_mul(1000)
}

/// Reference copy of `MarketData::trading_day_at`.
pub(super) fn old_trading_day_at(data: &MarketData, index: usize) -> i64 {
    data.trading_day
        .get(index)
        .copied()
        .unwrap_or_else(|| old_fallback_trading_day_for_time(data, data.time_at(index)))
}

// Reference copies of the `Vm` barstate helpers.

pub(super) fn old_is_last_bar(vm: &Vm, index: usize) -> bool {
    match vm.realtime_bar {
        Some(rt) => index == rt,
        None => !vm.has_realtime && index + 1 == vm.bars,
    }
}

/// Whether `index` is a committed historical bar (not the open realtime
/// bar). The realtime bar's own index reads as non-history while its ticks
/// replay; every other (committed) bar is history.
pub(super) fn old_is_history(vm: &Vm, index: usize) -> bool {
    match vm.realtime_bar {
        Some(rt) => index != rt,
        None => true,
    }
}

/// `barstate.isnew`: first execution of the bar. Historical bars run once
/// (always new); the realtime bar is new only on its opening tick.
pub(super) fn old_is_new(vm: &Vm, index: usize) -> bool {
    if vm.realtime_active && Some(index) == vm.realtime_bar {
        vm.bar_is_new
    } else {
        // A committed historical bar: its (only) execution was its first.
        true
    }
}

/// `barstate.isconfirmed`: closing execution of the bar. Historical bars
/// commit on their single execution (always confirmed); the realtime bar
/// is confirmed only on its `is_bar_close` tick.
pub(super) fn old_is_confirmed(vm: &Vm, index: usize) -> bool {
    if vm.realtime_active && Some(index) == vm.realtime_bar {
        vm.bar_is_confirmed
    } else {
        // A committed historical bar is confirmed.
        true
    }
}

/// Reference helper: the final historical bar also qualifies when the
/// dataset has no realtime segment.
pub(super) fn old_is_last_confirmed_history(vm: &Vm, index: usize) -> bool {
    // Historical-only feeds also have a final confirmed historical bar.
    old_is_history(vm, index) && index + 1 == vm.bars
}

/// Reference copy of `Vm::read_strategy`.
pub(super) fn old_read_strategy(vm: &Vm, source: BuiltinSeries, index: usize) -> Value {
    let Some(snap) = vm.broker.as_ref().and_then(|b| b.snapshot_at(index)) else {
        return Value::Na;
    };
    match source {
        BuiltinSeries::StrategyPositionSize => Value::from_f64(snap.position_size),
        BuiltinSeries::StrategyPositionAvgPrice => Value::from_f64(snap.position_avg_price),
        BuiltinSeries::StrategyOpenProfit => Value::from_f64(snap.open_profit),
        BuiltinSeries::StrategyNetProfit => Value::from_f64(snap.net_profit),
        BuiltinSeries::StrategyGrossProfit => Value::from_f64(snap.gross_profit),
        BuiltinSeries::StrategyGrossLoss => Value::from_f64(snap.gross_loss),
        BuiltinSeries::StrategyEquity => Value::from_f64(snap.equity),
        BuiltinSeries::StrategyInitialCapital => Value::from_f64(snap.initial_capital),
        BuiltinSeries::StrategyWinTrades => Value::Int(snap.win_trades),
        BuiltinSeries::StrategyLossTrades => Value::Int(snap.loss_trades),
        BuiltinSeries::StrategyEvenTrades => Value::Int(snap.even_trades),
        BuiltinSeries::StrategyClosedTrades => Value::Int(snap.closed_trades),
        BuiltinSeries::StrategyOpenTrades => Value::Int(snap.open_trades),
        BuiltinSeries::StrategyMaxDrawdown => Value::from_f64(snap.max_drawdown),
        BuiltinSeries::StrategyMaxRunup => Value::from_f64(snap.max_runup),
        BuiltinSeries::StrategyAvgTrade => Value::from_f64(snap.avg_trade),
        BuiltinSeries::StrategyAvgWinningTrade => Value::from_f64(snap.avg_winning_trade),
        BuiltinSeries::StrategyAvgLosingTrade => Value::from_f64(snap.avg_losing_trade),
        BuiltinSeries::StrategyMaxContractsHeldAll => Value::from_f64(snap.max_contracts_held),
        BuiltinSeries::StrategyMaxContractsHeldLong => {
            Value::from_f64(snap.max_contracts_held_long)
        }
        BuiltinSeries::StrategyMaxContractsHeldShort => {
            Value::from_f64(snap.max_contracts_held_short)
        }
        BuiltinSeries::StrategyAccountCurrency => Value::String(snap.account_currency.clone()),
        BuiltinSeries::StrategyAvgLosingTradePercent => {
            Value::from_f64(snap.avg_losing_trade_percent)
        }
        BuiltinSeries::StrategyAvgTradePercent => Value::from_f64(snap.avg_trade_percent),
        BuiltinSeries::StrategyAvgWinningTradePercent => {
            Value::from_f64(snap.avg_winning_trade_percent)
        }
        BuiltinSeries::StrategyClosedTradesFirstIndex => Value::Int(snap.closed_trades_first_index),
        BuiltinSeries::StrategyGrossLossPercent => Value::from_f64(snap.gross_loss_percent),
        BuiltinSeries::StrategyGrossProfitPercent => Value::from_f64(snap.gross_profit_percent),
        BuiltinSeries::StrategyMarginLiquidationPrice => {
            Value::from_f64(snap.margin_liquidation_price)
        }
        BuiltinSeries::StrategyMaxDrawdownPercent => Value::from_f64(snap.max_drawdown_percent),
        BuiltinSeries::StrategyMaxRunupPercent => Value::from_f64(snap.max_runup_percent),
        BuiltinSeries::StrategyNetProfitPercent => Value::from_f64(snap.net_profit_percent),
        BuiltinSeries::StrategyOpenProfitPercent => Value::from_f64(snap.open_profit_percent),
        BuiltinSeries::StrategyOpenTradesCapitalHeld => {
            Value::from_f64(snap.open_trades_capital_held)
        }
        BuiltinSeries::StrategyPositionEntryName => snap
            .position_entry_name
            .as_ref()
            .map_or(Value::Na, |name| Value::String(name.clone())),
        // Unreachable: the caller guards on `is_strategy()`.
        _ => Value::Na,
    }
}

/// Reference match of `Vm::read_strategy`, on a given snapshot.
fn old_strategy_value(source: BuiltinSeries, snap: &broker::StrategySnapshot) -> Value {
    match source {
        BuiltinSeries::StrategyPositionSize => Value::from_f64(snap.position_size),
        BuiltinSeries::StrategyPositionAvgPrice => Value::from_f64(snap.position_avg_price),
        BuiltinSeries::StrategyOpenProfit => Value::from_f64(snap.open_profit),
        BuiltinSeries::StrategyNetProfit => Value::from_f64(snap.net_profit),
        BuiltinSeries::StrategyGrossProfit => Value::from_f64(snap.gross_profit),
        BuiltinSeries::StrategyGrossLoss => Value::from_f64(snap.gross_loss),
        BuiltinSeries::StrategyEquity => Value::from_f64(snap.equity),
        BuiltinSeries::StrategyInitialCapital => Value::from_f64(snap.initial_capital),
        BuiltinSeries::StrategyWinTrades => Value::Int(snap.win_trades),
        BuiltinSeries::StrategyLossTrades => Value::Int(snap.loss_trades),
        BuiltinSeries::StrategyEvenTrades => Value::Int(snap.even_trades),
        BuiltinSeries::StrategyClosedTrades => Value::Int(snap.closed_trades),
        BuiltinSeries::StrategyOpenTrades => Value::Int(snap.open_trades),
        BuiltinSeries::StrategyMaxDrawdown => Value::from_f64(snap.max_drawdown),
        BuiltinSeries::StrategyMaxRunup => Value::from_f64(snap.max_runup),
        BuiltinSeries::StrategyAvgTrade => Value::from_f64(snap.avg_trade),
        BuiltinSeries::StrategyAvgWinningTrade => Value::from_f64(snap.avg_winning_trade),
        BuiltinSeries::StrategyAvgLosingTrade => Value::from_f64(snap.avg_losing_trade),
        BuiltinSeries::StrategyMaxContractsHeldAll => Value::from_f64(snap.max_contracts_held),
        BuiltinSeries::StrategyMaxContractsHeldLong => {
            Value::from_f64(snap.max_contracts_held_long)
        }
        BuiltinSeries::StrategyMaxContractsHeldShort => {
            Value::from_f64(snap.max_contracts_held_short)
        }
        BuiltinSeries::StrategyAccountCurrency => Value::String(snap.account_currency.clone()),
        BuiltinSeries::StrategyAvgLosingTradePercent => {
            Value::from_f64(snap.avg_losing_trade_percent)
        }
        BuiltinSeries::StrategyAvgTradePercent => Value::from_f64(snap.avg_trade_percent),
        BuiltinSeries::StrategyAvgWinningTradePercent => {
            Value::from_f64(snap.avg_winning_trade_percent)
        }
        BuiltinSeries::StrategyClosedTradesFirstIndex => Value::Int(snap.closed_trades_first_index),
        BuiltinSeries::StrategyGrossLossPercent => Value::from_f64(snap.gross_loss_percent),
        BuiltinSeries::StrategyGrossProfitPercent => Value::from_f64(snap.gross_profit_percent),
        BuiltinSeries::StrategyMarginLiquidationPrice => {
            Value::from_f64(snap.margin_liquidation_price)
        }
        BuiltinSeries::StrategyMaxDrawdownPercent => Value::from_f64(snap.max_drawdown_percent),
        BuiltinSeries::StrategyMaxRunupPercent => Value::from_f64(snap.max_runup_percent),
        BuiltinSeries::StrategyNetProfitPercent => Value::from_f64(snap.net_profit_percent),
        BuiltinSeries::StrategyOpenProfitPercent => Value::from_f64(snap.open_profit_percent),
        BuiltinSeries::StrategyOpenTradesCapitalHeld => {
            Value::from_f64(snap.open_trades_capital_held)
        }
        BuiltinSeries::StrategyPositionEntryName => snap
            .position_entry_name
            .as_ref()
            .map_or(Value::Na, |name| Value::String(name.clone())),
        // Unreachable: the caller guards on `is_strategy()`.
        _ => Value::Na,
    }
}

/// Reference copy of `input_value_matches_decl`.
pub(super) fn old_input_value_matches_decl(value: &InputValue, decl: &InputDecl) -> bool {
    if let InputValue::Enum { name, index, .. } = value {
        let InputValue::Enum {
            name: declared_name,
            ..
        } = &decl.defval
        else {
            return false;
        };
        if decl.kind != InputKind::Enum || name != declared_name {
            return false;
        }
        // Manifests list every allowed member in `options`, even when source
        // omitted the argument. An empty list is accepted for backward
        // compatibility with serialized programs that do not list them.
        return decl.constraints.options.is_empty()
            || decl.constraints.options.iter().any(|option| {
                matches!(
                    option,
                    InputValue::Enum {
                        name: option_name,
                        index: option_index,
                        ..
                    } if option_name == name && option_index == index
                )
            });
    }
    match (value, decl.kind) {
        // `input.price` is a float and `input.time` an int; an int default
        // coerces to either, mirroring `input.float`.
        (
            InputValue::Int(_),
            InputKind::Int | InputKind::Float | InputKind::Price | InputKind::Time,
        ) => true,
        (InputValue::Float(value), InputKind::Float | InputKind::Price) => value.is_finite(),
        (InputValue::Bool(_), InputKind::Bool) | (InputValue::String(_), InputKind::String) => true,
        (InputValue::Color { t, .. }, InputKind::Color) => *t <= 100.0,
        (InputValue::Source(series), InputKind::Source) => series.is_input_source(),
        (InputValue::ExternalSource(source), InputKind::Source) => source.is_valid(),
        _ => false,
    }
}

// ---- helpers ----------------------------------------------------------------

const TIMEZONES: [&str; 12] = [
    "",
    "UTC",
    "GMT",
    "UTC-5",
    "UTC+05:30",
    "GMT+0530",
    "UTC+14",
    "UTC-12:30",
    "America/New_York",
    "UTC+15",
    " UTC-3 ",
    "UTC+é",
];

const TIMEFRAMES: [&str; 18] = [
    "1D",
    "60",
    "1W",
    "5S",
    "1M",
    "",
    " 15 ",
    "3X",
    "D",
    "240",
    "12M",
    "0",
    "abc",
    "30s",
    "2w",
    "1d",
    "m",
    "999999999999999999999S",
];

fn opt_i(rng: &mut Rng) -> Option<i64> {
    match rng.next() % 4 {
        0 => None,
        1 => Some(0),
        2 => Some(i64::MIN),
        _ => Some(rng.next() as i64),
    }
}

fn opt_f(rng: &mut Rng) -> Option<f64> {
    if rng.next() % 4 == 0 {
        None
    } else {
        Some(rng.price())
    }
}

/// `feed_vm` plus random host metadata, calendar inputs and realtime state.
fn resid_vm(rng: &mut Rng, bars: usize) -> Vm {
    let mut vm = feed_vm(rng, bars, 100);
    let shape = rng.next();
    vm.data.timeframe = TIMEFRAMES[(shape % 18) as usize].into();
    vm.data.syminfo.timezone = TIMEZONES[((shape >> 8) % 12) as usize].into();
    // Calendar-friendly times half the time (the feed's are often huge).
    if (shape >> 16) % 2 == 0 && !vm.data.time.is_empty() {
        let n = vm.data.time.len();
        vm.data.time = (0..n)
            .map(|_| ((rng.next() % 8_000_000_000_000) as i64) - 2_000_000_000_000)
            .collect();
    }
    vm.data.time_close = match (shape >> 18) % 3 {
        0 => Vec::new(),
        1 => (0..bars).map(|_| rng.next() as i64).collect(),
        _ => {
            let n = rng.len(bars);
            (0..n).map(|_| rng.next() as i64).collect()
        }
    };
    vm.data.trading_day = match (shape >> 20) % 3 {
        0 => Vec::new(),
        1 => (0..bars).map(|_| rng.next() as i64).collect(),
        _ => {
            let n = (rng.next() as usize) % (bars + 2);
            (0..n).map(|_| rng.next() as i64).collect()
        }
    };
    vm.data.mintick = rng.price();
    let s = &mut vm.data.syminfo;
    s.minmove = rng.next() as i64;
    // Empty half the time (read as `na`), without drawing from `rng`.
    s.current_contract = if s.minmove % 2 == 0 {
        String::new()
    } else {
        "CME_MINI:ESZ2026".to_string()
    };
    s.pricescale = rng.next() as i64;
    s.pointvalue = rng.price();
    s.mincontract = rng.price();
    s.expiration_date = opt_i(rng);
    s.employees = opt_i(rng);
    s.shareholders = opt_i(rng);
    s.shares_outstanding_float = opt_f(rng);
    s.shares_outstanding_total = opt_i(rng);
    s.recommendations_buy = opt_i(rng);
    s.recommendations_buy_strong = opt_i(rng);
    s.recommendations_date = opt_i(rng);
    s.recommendations_hold = opt_i(rng);
    s.recommendations_sell = opt_i(rng);
    s.recommendations_sell_strong = opt_i(rng);
    s.recommendations_total = opt_i(rng);
    s.target_price_average = opt_f(rng);
    s.target_price_date = opt_i(rng);
    s.target_price_estimates = opt_f(rng);
    s.target_price_high = opt_f(rng);
    s.target_price_low = opt_f(rng);
    s.target_price_median = opt_f(rng);
    vm.data.chart_left_visible_bar_time = opt_i(rng);
    vm.data.chart_right_visible_bar_time = opt_i(rng);
    let e = &mut vm.data.upcoming_events;
    e.dividends_future_amount = opt_f(rng);
    e.dividends_future_ex_date = opt_i(rng);
    e.dividends_future_pay_date = opt_i(rng);
    e.earnings_future_eps = opt_f(rng);
    e.earnings_future_period_end_time = opt_i(rng);
    e.earnings_future_revenue = opt_f(rng);
    e.earnings_future_time = opt_i(rng);
    let rt = rng.next();
    vm.realtime_bar = match rt % 3 {
        0 => None,
        _ => Some(((rt >> 8) as usize) % (bars + 1)),
    };
    vm.realtime_active = (rt >> 32) % 2 == 0;
    vm.has_realtime = (rt >> 33) % 2 == 0;
    vm.bar_is_new = (rt >> 34) % 2 == 0;
    vm.bar_is_confirmed = (rt >> 35) % 2 == 0;
    vm
}

fn snapshot(rng: &mut Rng) -> broker::StrategySnapshot {
    broker::StrategySnapshot {
        position_size: rng.price(),
        position_avg_price: rng.price(),
        open_profit: rng.price(),
        net_profit: rng.price(),
        gross_profit: rng.price(),
        gross_loss: rng.price(),
        equity: rng.price(),
        initial_capital: rng.price(),
        win_trades: rng.next() as i64,
        loss_trades: rng.next() as i64,
        even_trades: rng.next() as i64,
        closed_trades: rng.next() as i64,
        open_trades: rng.next() as i64,
        max_drawdown: rng.price(),
        max_runup: rng.price(),
        avg_trade: rng.price(),
        avg_winning_trade: rng.price(),
        avg_losing_trade: rng.price(),
        max_contracts_held: rng.price(),
        max_contracts_held_long: rng.price(),
        max_contracts_held_short: rng.price(),
        account_currency: format!("C{}", rng.next() % 7),
        avg_losing_trade_percent: rng.price(),
        avg_trade_percent: rng.price(),
        avg_winning_trade_percent: rng.price(),
        closed_trades_first_index: rng.next() as i64,
        gross_loss_percent: rng.price(),
        gross_profit_percent: rng.price(),
        margin_liquidation_price: rng.price(),
        max_drawdown_percent: rng.price(),
        max_runup_percent: rng.price(),
        net_profit_percent: rng.price(),
        open_profit_percent: rng.price(),
        open_trades_capital_held: rng.price(),
        position_entry_name: if rng.next() % 3 == 0 {
            None
        } else {
            Some(format!("E{}", rng.next() % 5))
        },
    }
}

// ---- the tests ----------------------------------------------------------------

/// `read_builtin` on every series against the reference body, over random host
/// metadata, timezones, timeframes, time/close/trading-day lanes and
/// realtime state (panics included: short lanes, calendar overflow).
#[test]
fn read_builtin_matches_the_old_arm_on_residual_state() {
    let mut rng = Rng(0x0dd5_eed5_2026_1003);
    let offsets = [0u32, 1, 2, 3, 7, u32::MAX];
    let mut cases = 0usize;
    let mut non_na = std::collections::BTreeSet::new();
    for round in 0..400 {
        let bars = 1 + (round % 7);
        let mut vm = resid_vm(&mut rng, bars);
        for bar in 0..bars {
            vm.bar = bar;
            for s in BuiltinSeries::ALL {
                for &offset in &offsets {
                    let old = guarded(|| key(&old_read_builtin(&vm, s, offset)));
                    let new = guarded(|| key(&vm.read_builtin(s, offset)));
                    assert_eq!(new, old, "{s:?} bar={bar} offset={offset} bars={bars}");
                    if old != "Na" && old != "panic" {
                        non_na.insert(format!("{s:?}"));
                    }
                    cases += 1;
                }
            }
        }
    }
    assert!(cases > 1_000_000, "{cases}");
    // Every non-strategy series produced a value somewhere (no broker here).
    let expected = BuiltinSeries::ALL
        .iter()
        .filter(|s| !s.is_strategy())
        .count();
    assert_eq!(non_na.len(), expected, "{non_na:?}");
}

/// `Vm::strategy_value` (the numeric getters through
/// `vm_ops::strategy_series_value`) against the reference match, on random
/// snapshots (non-finite metrics included) and every series.
#[test]
fn strategy_value_matches_the_old_match_on_random_snapshots() {
    let mut rng = Rng(0x57a7_e61c);
    let mut strategy = 0usize;
    for _ in 0..2_000 {
        let snap = snapshot(&mut rng);
        for s in BuiltinSeries::ALL {
            let old = key(&old_strategy_value(s, &snap));
            let new = key(&Vm::strategy_value(s, &snap));
            assert_eq!(new, old, "{s:?}");
            if s.is_strategy() {
                strategy += 1;
            }
        }
    }
    assert_eq!(strategy, 2_000 * 35);
}

/// `read_builtin` of every `strategy.*` series after a real strategy run
/// (recorded snapshots), against the reference `read_strategy`.
#[test]
fn read_strategy_matches_the_old_arm_after_a_run() {
    let prog = program(
        "//@version=6\nstrategy(\"S\", initial_capital=10000)\n\
         if bar_index % 4 == 1\n    strategy.entry(\"L\", strategy.long)\n\
         if bar_index % 4 == 3\n    strategy.close(\"L\")\n\
         plot(strategy.position_size)\n",
    );
    let bars = 24;
    let px = |i: usize| 100.0 + ((i * 7) % 11) as f64 - (i as f64) * 0.25;
    let data = MarketData {
        open: (0..bars).map(px).collect(),
        high: (0..bars).map(|i| px(i) + 2.0).collect(),
        low: (0..bars).map(|i| px(i) - 2.0).collect(),
        close: (0..bars).map(|i| px(i) + 0.5).collect(),
        volume: vec![10.0; bars],
        ..MarketData::default()
    };
    let mut vm = Vm::new(&prog, &data, &RunOptions::default()).expect("vm");
    let (executed, errors) = vm.run_history();
    assert_eq!(executed as usize, bars);
    assert!(errors.is_empty(), "{errors:?}");
    let mut seen = std::collections::BTreeSet::new();
    let mut distinct = std::collections::BTreeSet::new();
    for bar in 0..bars {
        vm.bar = bar;
        for s in BuiltinSeries::ALL {
            if !s.is_strategy() {
                continue;
            }
            for offset in [0u32, 1, 5, 23, 24, u32::MAX] {
                let index = bar.checked_sub(offset as usize);
                let old = match index {
                    Some(index) => key(&old_read_strategy(&vm, s, index)),
                    None => "Na".to_string(),
                };
                let new = key(&vm.read_builtin(s, offset));
                assert_eq!(new, old, "{s:?} bar={bar} offset={offset}");
                assert_eq!(new, key(&old_read_builtin(&vm, s, offset)));
                if new != "Na" {
                    seen.insert(format!("{s:?}"));
                    distinct.insert(format!("{s:?}={new}"));
                }
            }
        }
    }
    // The run trades: most getters are non-`na` and vary across bars.
    assert!(seen.len() >= 25, "{seen:?}");
    assert!(distinct.len() > 100, "{}", distinct.len());
    // Without a broker every strategy read is `na`, reference and new.
    let ind = program("indicator(\"T\")\nplot(close)\n");
    let mut vm = Vm::new(&ind, &data, &RunOptions::default()).expect("vm");
    vm.bar = 3;
    for s in BuiltinSeries::ALL {
        if s.is_strategy() {
            assert_eq!(key(&vm.read_builtin(s, 1)), "Na");
            assert_eq!(key(&old_read_strategy(&vm, s, 2)), "Na");
        }
    }
}

/// `MarketData::time_close_at`, `trading_day_at` and
/// `fallback_trading_day_for_time` against the reference bodies (panics on a
/// short lane included), and the kernels directly.
#[test]
fn time_close_and_trading_day_match_the_old_bodies() {
    let mut rng = Rng(0x71e3_c105e);
    let extremes = [
        i64::MIN,
        i64::MIN + 1,
        -1,
        0,
        1,
        999,
        1000,
        i64::MAX - 1,
        i64::MAX,
    ];
    for round in 0..600 {
        let bars = 1 + (round % 6);
        let mut vm = resid_vm(&mut rng, bars);
        if round % 5 == 0 && !vm.data.time.is_empty() {
            let n = vm.data.time.len();
            vm.data.time = (0..n)
                .map(|_| extremes[(rng.next() % 9) as usize])
                .collect();
        }
        let data = &vm.data;
        for index in 0..bars + 2 {
            let old = guarded(|| old_time_close_at(data, index).to_string());
            let new = guarded(|| data.time_close_at(index).to_string());
            assert_eq!(new, old, "time_close_at {index}");
            let old = guarded(|| old_trading_day_at(data, index).to_string());
            let new = guarded(|| data.trading_day_at(index).to_string());
            assert_eq!(new, old, "trading_day_at {index}");
        }
        for &t in &extremes {
            assert_eq!(
                data.fallback_trading_day_for_time(t),
                old_fallback_trading_day_for_time(data, t)
            );
            for off in [-53_940i64, -1, 0, 1, 19_800, 53_940, i64::MAX, i64::MIN] {
                // An absurd offset overflows the day product on both sides.
                let want = guarded(|| {
                    let local = t.div_euclid(1000).saturating_add(off);
                    (local.div_euclid(86_400) * 86_400)
                        .saturating_mul(1000)
                        .to_string()
                });
                let got = guarded(|| vm_ops::fallback_trading_day(t, off).to_string());
                assert_eq!(got, want, "{t} {off}");
                for tf in [0i64, 1, 60, 86_400, i64::MAX, i64::MIN, -5] {
                    assert_eq!(
                        vm_ops::time_close_fallback(t, tf),
                        t.saturating_add(tf.saturating_mul(1000))
                    );
                }
            }
        }
        assert_eq!(vm_ops::time_close_lane(&[], 9), None);
        assert_eq!(vm_ops::time_close_lane(&[4, 5], 1), Some(5));
        assert_eq!(vm_ops::trading_day_lane(&[4, 5], 1), Some(5));
        assert_eq!(vm_ops::trading_day_lane(&[4, 5], 2), None);
        assert_eq!(vm_ops::trading_day_lane(&[], 0), None);
    }
}

/// The timeframe kernel against the reference `timeframe_class` /
/// `timeframe_multiplier`, on every unit letter and many strings.
#[test]
fn timeframe_series_match_the_old_helpers() {
    let mut strings: Vec<String> = TIMEFRAMES.iter().map(|s| (*s).to_string()).collect();
    for c in 0u8..128 {
        strings.push(format!("7{}", c as char));
        strings.push(format!("{}", c as char));
    }
    strings.extend(["3é".to_string(), "é".to_string(), " 2D\t".to_string()]);
    use vm_ops::TimeframeSeries as T;
    for tf in &strings {
        let class = old_timeframe_class(tf);
        let (mult, unit) = session::timeframe_parts(tf);
        assert_eq!(session::tf_class_of_unit(unit), class, "{tf:?}");
        let want = [
            (T::Multiplier, Value::Int(old_timeframe_multiplier(tf))),
            (T::Isseconds, Value::Bool(class == TfClass::Seconds)),
            (T::Isminutes, Value::Bool(class == TfClass::Minutes)),
            (T::Isdaily, Value::Bool(class == TfClass::Daily)),
            (T::Isweekly, Value::Bool(class == TfClass::Weekly)),
            (T::Ismonthly, Value::Bool(class == TfClass::Monthly)),
            (
                T::Isdwm,
                Value::Bool(matches!(
                    class,
                    TfClass::Daily | TfClass::Weekly | TfClass::Monthly
                )),
            ),
            (
                T::Isintraday,
                Value::Bool(matches!(class, TfClass::Seconds | TfClass::Minutes)),
            ),
        ];
        for (series, value) in want {
            assert_eq!(
                key(&vm_ops::timeframe_series_value(series, mult, unit)),
                key(&value),
                "{tf:?} {series:?}"
            );
        }
    }
    // Every class is reached.
    for (tf, class) in [
        ("5", TfClass::Minutes),
        ("5S", TfClass::Seconds),
        ("D", TfClass::Daily),
        ("2W", TfClass::Weekly),
        ("M", TfClass::Monthly),
        ("3X", TfClass::Daily),
    ] {
        assert_eq!(old_timeframe_class(tf), class);
    }
}

/// The barstate kernels against the reference helpers on the whole small state
/// space, and `history_index` against `checked_sub`.
#[test]
fn barstate_kernels_match_the_old_helpers() {
    let mut rng = Rng(5);
    let mut vm = feed_vm(&mut rng, 4, 100);
    let mut n = 0usize;
    for bars in 0..5usize {
        vm.bars = bars;
        for rt in [
            None,
            Some(0usize),
            Some(1),
            Some(3),
            Some(4),
            Some(usize::MAX),
        ] {
            for flags in 0..16u8 {
                vm.realtime_bar = rt;
                vm.realtime_active = flags & 1 != 0;
                vm.has_realtime = flags & 2 != 0;
                vm.bar_is_new = flags & 4 != 0;
                vm.bar_is_confirmed = flags & 8 != 0;
                let st = vm_ops::BarState {
                    realtime_bar: vm.realtime_bar,
                    realtime_active: vm.realtime_active,
                    has_realtime: vm.has_realtime,
                    bar_is_new: vm.bar_is_new,
                    bar_is_confirmed: vm.bar_is_confirmed,
                    bars: vm.bars,
                };
                for index in 0..6usize {
                    assert_eq!(vm_ops::is_last_bar(index, &st), old_is_last_bar(&vm, index));
                    assert_eq!(vm_ops::is_history(index, &st), old_is_history(&vm, index));
                    assert_eq!(vm_ops::is_new(index, &st), old_is_new(&vm, index));
                    assert_eq!(
                        vm_ops::is_confirmed(index, &st),
                        old_is_confirmed(&vm, index)
                    );
                    assert_eq!(
                        vm_ops::is_last_confirmed_history(index, &st),
                        old_is_last_confirmed_history(&vm, index)
                    );
                    use vm_ops::BarstateSeries as B;
                    let want = [
                        (B::Islast, old_is_last_bar(&vm, index)),
                        (B::Ishistory, old_is_history(&vm, index)),
                        (B::Isrealtime, !old_is_history(&vm, index)),
                        (B::Isconfirmed, old_is_confirmed(&vm, index)),
                        (B::Isnew, old_is_new(&vm, index)),
                        (
                            B::Islastconfirmedhistory,
                            old_is_last_confirmed_history(&vm, index),
                        ),
                    ];
                    for (series, b) in want {
                        assert_eq!(
                            vm_ops::barstate_value(series, index, &st),
                            Value::Bool(b),
                            "{series:?} {st:?} {index}"
                        );
                        n += 1;
                    }
                }
            }
        }
    }
    assert_eq!(n, 5 * 6 * 16 * 6 * 6);
    for bar in [0usize, 1, 5, usize::MAX] {
        for offset in [0u32, 1, 5, 6, u32::MAX] {
            assert_eq!(
                vm_ops::history_index(bar, offset),
                bar.checked_sub(offset as usize)
            );
        }
    }
}

/// `input_value_matches_decl` against the reference body: every value shape
/// (non-finite floats, transparency above 100, enum names and options,
/// invalid sources) on every declared kind and default.
#[test]
fn input_value_matches_decl_matches_the_old_body() {
    let base = program("indicator(\"T\")\nx = input.int(3)\nplot(x)\n");
    let template: InputDecl = base.inputs[0].clone();
    let kinds = [
        InputKind::Int,
        InputKind::Float,
        InputKind::Bool,
        InputKind::String,
        InputKind::Color,
        InputKind::Source,
        InputKind::Price,
        InputKind::Time,
        InputKind::Enum,
    ];
    let ext = |id: &str| {
        InputValue::ExternalSource(ExternalSourceRef {
            script_id: id.into(),
            plot_slot: 0,
            plot_label: "P".into(),
        })
    };
    let enum_value = |name: &str, index: u32| InputValue::Enum {
        name: name.into(),
        index,
        member: "m".into(),
        title: None,
    };
    let color = |t: u8| InputValue::Color {
        r: 1,
        g: 2,
        b: 3,
        t: f64::from(t),
    };
    let mut values = vec![
        InputValue::Int(0),
        InputValue::Int(i64::MIN),
        InputValue::Int(i64::MAX),
        InputValue::Bool(true),
        InputValue::Bool(false),
        InputValue::String(String::new()),
        InputValue::String("s".into()),
        InputValue::Source(BuiltinSeries::Open),
        InputValue::Source(BuiltinSeries::Close),
        InputValue::Source(BuiltinSeries::BarIndex),
        InputValue::Source(BuiltinSeries::StrategyEquity),
        ext("up"),
        ext(""),
        enum_value("E", 0),
        enum_value("E", 1),
        enum_value("E", u32::MAX),
        enum_value("F", 0),
    ];
    for bits in [
        0u64,
        1,
        0x8000_0000_0000_0000,
        0x7ff0_0000_0000_0000,
        0xfff0_0000_0000_0000,
        0x7ff8_0000_0000_0000,
        0x7ff0_0000_0000_0001,
        0xffff_ffff_ffff_ffff,
        0x7fef_ffff_ffff_ffff,
        0x4004_0000_0000_0000,
    ] {
        values.push(InputValue::Float(f64::from_bits(bits)));
    }
    for t in [0u8, 1, 99, 100, 101, 255] {
        values.push(color(t));
    }
    let options = [
        Vec::new(),
        vec![enum_value("E", 1)],
        vec![enum_value("E", 0), enum_value("F", 1), InputValue::Int(1)],
    ];
    let (mut accepted, mut rejected) = (0usize, 0usize);
    for kind in kinds {
        for defval in &values {
            for opts in &options {
                let mut decl = template.clone();
                decl.kind = kind;
                decl.defval = defval.clone();
                decl.constraints.options.clone_from(opts);
                for v in &values {
                    let old = old_input_value_matches_decl(v, &decl);
                    let new = input_value_matches_decl(v, &decl);
                    assert_eq!(new, old, "{v:?} on {kind:?} default {defval:?}");
                    if new {
                        accepted += 1;
                    } else {
                        rejected += 1;
                    }
                }
            }
        }
    }
    assert!(
        accepted > 2_000 && rejected > 20_000,
        "{accepted} {rejected}"
    );
}

/// The external-source sample against the reference expression.
#[test]
fn external_sample_value_matches_the_old_expression() {
    let mut rng = Rng(11);
    for _ in 0..2_000 {
        let n = (rng.next() % 6) as usize;
        let values: Vec<Option<f64>> = (0..n).map(|_| opt_f(&mut rng)).collect();
        for bar in 0..n + 2 {
            let old = match values.get(bar).copied().flatten() {
                Some(value) => Value::from_f64(value),
                None => Value::Na,
            };
            assert_eq!(key(&vm_ops::external_sample_value(&values, bar)), key(&old));
        }
    }
}

/// `resolve_input` of an `input.source` bound to a valid external lane
/// reads the lane's sample at the current bar (`na` for a missing sample
/// and past the lane), as the reference expression does.
#[test]
fn resolve_input_reads_the_external_lane_at_the_current_bar() {
    let prog = program("indicator(\"T\")\ny = input.source(close)\nplot(y)\n");
    let bars = 5;
    let data = MarketData {
        open: vec![1.0; bars],
        high: vec![2.0; bars],
        low: vec![0.5; bars],
        close: vec![1.5; bars],
        volume: vec![1.0; bars],
        ..MarketData::default()
    };
    let ext = ExternalSourceRef {
        script_id: "up".into(),
        plot_slot: 0,
        plot_label: "P".into(),
    };
    let values = vec![Some(9.5), None, Some(-2.25), Some(7.0)];
    let mut options = RunOptions::default();
    options
        .inputs
        .insert(0, InputValue::ExternalSource(ext.clone()));
    options.external_sources.insert(
        0,
        ExternalSourceLane {
            source: ext,
            values: values.clone(),
        },
    );
    let mut vm = Vm::new(&prog, &data, &options).expect("vm");
    let mut floats = 0;
    for bar in 0..bars {
        vm.bar = bar;
        let old = match values.get(bar).copied().flatten() {
            Some(value) => Value::from_f64(value),
            None => Value::Na,
        };
        let new = vm.resolve_input(0, 0).expect("input");
        assert_eq!(key(&new), key(&old), "bar {bar}");
        assert_eq!(
            key(&new),
            key(&super::vm_read_kernels::old_resolve_input(&vm, 0, 0).expect("input"))
        );
        if matches!(new, Value::Float(_)) {
            floats += 1;
        }
    }
    assert_eq!(floats, 3);
}
