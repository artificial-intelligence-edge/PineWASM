//! Differential tests of the read kernels: `Vm::read_builtin` (feed series
//! through `vm_ops::feed_series_read`),
//! `Vm::resolve_input` (scalar inputs through `vm_ops::input_scalar_value`)
//! and `exec_read_builtin_dynamic` (`vm_ops::dynamic_offset_limit`) against
//! direct reference implementations of the read bodies, on every `BuiltinSeries`,
//! random feeds (non-finite data, short and empty optional lanes, an empty
//! or explicit `time` lane), offsets before bar 0 and past `max_bars_back`,
//! every input kind with type-matching and mismatching overrides, and panics.

#![allow(
    clippy::too_many_lines,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation
)]

use super::super::*;
// Reference implementations of the `read_builtin` and `resolve_input` bodies.
use super::vm_read_kernels_2::{
    old_input_value_matches_decl, old_is_confirmed, old_is_history, old_is_last_bar,
    old_is_last_confirmed_history, old_is_new, old_read_strategy, old_time_close_at,
    old_timeframe_class as timeframe_class, old_timeframe_multiplier as timeframe_multiplier,
    old_trading_day_at,
};
use pinewasm_hir::{BuiltinSeries, ExternalSourceRef, InputKind, InputValue};
use pinewasm_mir::Value;
use session::TfClass;
use std::panic::{catch_unwind, AssertUnwindSafe};

// ---- reference implementations of the bodies (`self` -> `vm`) --------------

/// Reference implementation of the `Vm::read_builtin` body.
pub(super) fn old_read_builtin(vm: &Vm, source: BuiltinSeries, offset: u32) -> Value {
    if source == BuiltinSeries::Close {
        return vm_close_step::close_value(
            vm.bar,
            offset,
            &vm_close_step::Ohlc {
                open: &vm.data.open,
                high: &vm.data.high,
                low: &vm.data.low,
                close: &vm.data.close,
            },
        );
    }
    let Some(index) = vm.bar.checked_sub(offset as usize) else {
        return Value::Na;
    };
    // The `strategy.*` read surface reads a frozen per-bar broker
    // snapshot rather than the market data; handle it separately so the
    // market-data match below stays over OHLCV/time/barstate only.
    if source.is_strategy() {
        return old_read_strategy(vm, source, index);
    }
    let (o, h, l, c) = (
        vm.data.open[index],
        vm.data.high[index],
        vm.data.low[index],
        vm.data.close[index],
    );
    match source {
        BuiltinSeries::Open => Value::from_f64(o),
        BuiltinSeries::High => Value::from_f64(h),
        BuiltinSeries::Low => Value::from_f64(l),
        BuiltinSeries::Close => Value::from_f64(c),
        BuiltinSeries::Volume => Value::from_f64(vm.data.volume[index]),
        // Derived averages. `from_f64` maps a non-finite result to `na`,
        // matching how the raw OHLCV reads normalize bad input data.
        BuiltinSeries::Hl2 => Value::from_f64(f64::midpoint(h, l)),
        BuiltinSeries::Hlc3 => Value::from_f64((h + l + c) / 3.0),
        BuiltinSeries::Ohlc4 => Value::from_f64((o + h + l + c) / 4.0),
        // Double-weighted close: (high + low + close + close) / 4.
        BuiltinSeries::Hlcc4 => Value::from_f64((h + l + c + c) / 4.0),
        // `bar_index` is a series int (the referenced bar's index).
        BuiltinSeries::BarIndex => Value::Int(index as i64),
        // Time axis (epoch ms), series int.
        BuiltinSeries::Time => Value::Int(vm.data.time_at(index)),
        BuiltinSeries::TimeClose => Value::Int(old_time_close_at(&vm.data, index)),
        // `timenow`/`last_bar_*` reference the run's last bar; they hold
        // the same value on every bar, so the history offset only shifts
        // which bar we read them "on" — the value is bar-independent. An
        // empty run (bars == 0) can't reach here (index < bar < bars).
        //
        // `timenow` is deterministic here: it equals the last bar's CLOSE
        // time (docs/runtime.md describes how it differs from wall-clock time).
        // `last_bar_time` is the last bar's open time.
        BuiltinSeries::Timenow => Value::Int(old_time_close_at(&vm.data, vm.bars - 1)),
        BuiltinSeries::LastBarTime => Value::Int(vm.data.time_at(vm.bars - 1)),
        BuiltinSeries::LastBarIndex => Value::Int(vm.bars as i64 - 1),
        // Calendar series: decompose the bar open time in the exchange
        // (fixed-offset) timezone.
        BuiltinSeries::DayOfWeek
        | BuiltinSeries::DayOfMonth
        | BuiltinSeries::Month
        | BuiltinSeries::Year
        | BuiltinSeries::Hour
        | BuiltinSeries::Minute
        | BuiltinSeries::Second
        | BuiltinSeries::WeekOfYear => {
            // The exchange's offset at the bar, daylight-saving changes
            // included.
            let off = vm.data.exchange_offset_at(vm.data.time_at(index));
            let (year, month, day, hour, minute, second, dow) =
                civil_from_epoch_ms(vm.data.time_at(index), off);
            match source {
                BuiltinSeries::DayOfWeek => Value::Int(dow),
                BuiltinSeries::DayOfMonth => Value::Int(day),
                BuiltinSeries::Month => Value::Int(month),
                BuiltinSeries::Year => Value::Int(year),
                BuiltinSeries::Hour => Value::Int(hour),
                BuiltinSeries::Minute => Value::Int(minute),
                BuiltinSeries::Second => Value::Int(second),
                // ISO 8601 week-of-year (1..=53) of the bar's open date;
                // `na` when the date's ISO Thursday is outside the i64
                // epoch-ms range (see `iso_week_of_year`).
                BuiltinSeries::WeekOfYear => {
                    week_of_year_at(vm.data.time_at(index), off).map_or(Value::Na, Value::Int)
                }
                _ => unreachable!(),
            }
        }
        // ---- barstate.*: current-bar execution predicates -------
        // Computed for the *referenced* bar `index` (offset already
        // applied), so `barstate.islast[1]` reads the prior bar's value —
        // consistent with every other series read.
        BuiltinSeries::BarstateIsfirst => Value::Bool(index == 0),
        BuiltinSeries::BarstateIslast => Value::Bool(old_is_last_bar(vm, index)),
        BuiltinSeries::BarstateIshistory => Value::Bool(old_is_history(vm, index)),
        BuiltinSeries::BarstateIsrealtime => Value::Bool(!old_is_history(vm, index)),
        BuiltinSeries::BarstateIsconfirmed => Value::Bool(old_is_confirmed(vm, index)),
        BuiltinSeries::BarstateIsnew => Value::Bool(old_is_new(vm, index)),
        BuiltinSeries::BarstateIslastconfirmedhistory => {
            Value::Bool(old_is_last_confirmed_history(vm, index))
        }
        // ---- timeframe.*: derived from the chart timeframe string,
        // the same value on every bar. `timeframe_class` classifies the
        // trailing unit once. ----
        BuiltinSeries::TimeframePeriod | BuiltinSeries::TimeframeMainPeriod => {
            // `main_period` == `period` here (no secondary contexts).
            Value::String(vm.data.timeframe.trim().to_string())
        }
        BuiltinSeries::TimeframeMultiplier => Value::Int(timeframe_multiplier(&vm.data.timeframe)),
        BuiltinSeries::TimeframeIsseconds => {
            Value::Bool(timeframe_class(&vm.data.timeframe) == TfClass::Seconds)
        }
        BuiltinSeries::TimeframeIsminutes => {
            Value::Bool(timeframe_class(&vm.data.timeframe) == TfClass::Minutes)
        }
        BuiltinSeries::TimeframeIsdaily => {
            Value::Bool(timeframe_class(&vm.data.timeframe) == TfClass::Daily)
        }
        BuiltinSeries::TimeframeIsweekly => {
            Value::Bool(timeframe_class(&vm.data.timeframe) == TfClass::Weekly)
        }
        BuiltinSeries::TimeframeIsmonthly => {
            Value::Bool(timeframe_class(&vm.data.timeframe) == TfClass::Monthly)
        }
        BuiltinSeries::TimeframeIsdwm => Value::Bool(matches!(
            timeframe_class(&vm.data.timeframe),
            TfClass::Daily | TfClass::Weekly | TfClass::Monthly
        )),
        BuiltinSeries::TimeframeIsintraday => Value::Bool(matches!(
            timeframe_class(&vm.data.timeframe),
            TfClass::Seconds | TfClass::Minutes
        )),
        // No tick timeframes in this VM.
        BuiltinSeries::TimeframeIsticks => Value::Bool(false),
        // ---- syminfo.*: host-or-default symbol metadata. ----
        BuiltinSeries::SyminfoTicker => Value::String(vm.data.syminfo.ticker.clone()),
        BuiltinSeries::SyminfoTickerid => {
            let syminfo = &vm.data.syminfo;
            let tickerid = if !syminfo.tickerid.is_empty() {
                syminfo.tickerid.clone()
            } else if !syminfo.main_tickerid.is_empty() {
                syminfo.main_tickerid.clone()
            } else if !syminfo.prefix.is_empty() && !syminfo.ticker.is_empty() {
                format!("{}:{}", syminfo.prefix, syminfo.ticker)
            } else {
                syminfo.ticker.clone()
            };
            Value::String(tickerid)
        }
        BuiltinSeries::SyminfoBasecurrency => Value::String(vm.data.syminfo.basecurrency.clone()),
        BuiltinSeries::SyminfoCurrency => Value::String(vm.data.syminfo.currency.clone()),
        BuiltinSeries::SyminfoDescription => Value::String(vm.data.syminfo.description.clone()),
        BuiltinSeries::SyminfoCountry => Value::String(vm.data.syminfo.country.clone()),
        BuiltinSeries::SyminfoIndustry => Value::String(vm.data.syminfo.industry.clone()),
        BuiltinSeries::SyminfoSector => Value::String(vm.data.syminfo.sector.clone()),
        BuiltinSeries::SyminfoType => Value::String(vm.data.syminfo.kind.clone()),
        BuiltinSeries::SyminfoRoot => Value::String(vm.data.syminfo.root.clone()),
        BuiltinSeries::SyminfoPrefix => Value::String(vm.data.syminfo.prefix.clone()),
        BuiltinSeries::SyminfoSession => Value::String(vm.data.syminfo.session.clone()),
        BuiltinSeries::SyminfoTimezone => Value::String(vm.data.syminfo.timezone.clone()),
        BuiltinSeries::SyminfoVolumetype => Value::String(vm.data.syminfo.volumetype.clone()),
        BuiltinSeries::SyminfoIsin => Value::String(vm.data.syminfo.isin.clone()),
        // Reads the current contract, or `na` when the host sets none.
        BuiltinSeries::SyminfoCurrentContract => {
            if vm.data.syminfo.current_contract.is_empty() {
                Value::Na
            } else {
                Value::String(vm.data.syminfo.current_contract.clone())
            }
        }
        BuiltinSeries::SyminfoMainTickerid => Value::String(vm.data.syminfo.main_tickerid.clone()),
        BuiltinSeries::SyminfoMintick => Value::from_f64(vm.data.mintick),
        BuiltinSeries::SyminfoMinmove => Value::Int(vm.data.syminfo.minmove),
        BuiltinSeries::SyminfoPricescale => Value::Int(vm.data.syminfo.pricescale),
        BuiltinSeries::SyminfoPointvalue => Value::from_f64(vm.data.syminfo.pointvalue),
        BuiltinSeries::SyminfoMincontract => Value::from_f64(vm.data.syminfo.mincontract),
        // ---- Host-or-default. `bid`/`ask` read the optional host
        // per-bar quote series at the referenced bar (`na` when absent or
        // uncovered — no real tick feed). `syminfo.expiration_date` and the
        // visible-bar-time viewport accessors read a per-run host value
        // (`na` by default; the headless VM has no viewport). ----
        BuiltinSeries::Bid => Value::from_f64(vm.data.bid.get(index).copied().unwrap_or(f64::NAN)),
        BuiltinSeries::Ask => Value::from_f64(vm.data.ask.get(index).copied().unwrap_or(f64::NAN)),
        BuiltinSeries::SyminfoExpirationDate => vm
            .data
            .syminfo
            .expiration_date
            .map_or(Value::Na, Value::Int),
        // SYMINFO fundamentals: each reads its optional host field, `na`
        // when absent. Integer counts / epoch-ms dates map to `Value::Int`;
        // share counts and price targets map to `Value::from_f64`.
        BuiltinSeries::SyminfoEmployees => vm.data.syminfo.employees.map_or(Value::Na, Value::Int),
        BuiltinSeries::SyminfoShareholders => {
            vm.data.syminfo.shareholders.map_or(Value::Na, Value::Int)
        }
        BuiltinSeries::SyminfoSharesOutstandingFloat => vm
            .data
            .syminfo
            .shares_outstanding_float
            .map_or(Value::Na, Value::from_f64),
        BuiltinSeries::SyminfoSharesOutstandingTotal => vm
            .data
            .syminfo
            .shares_outstanding_total
            .map_or(Value::Na, Value::Int),
        BuiltinSeries::SyminfoRecommendationsBuy => vm
            .data
            .syminfo
            .recommendations_buy
            .map_or(Value::Na, Value::Int),
        BuiltinSeries::SyminfoRecommendationsBuyStrong => vm
            .data
            .syminfo
            .recommendations_buy_strong
            .map_or(Value::Na, Value::Int),
        BuiltinSeries::SyminfoRecommendationsDate => vm
            .data
            .syminfo
            .recommendations_date
            .map_or(Value::Na, Value::Int),
        BuiltinSeries::SyminfoRecommendationsHold => vm
            .data
            .syminfo
            .recommendations_hold
            .map_or(Value::Na, Value::Int),
        BuiltinSeries::SyminfoRecommendationsSell => vm
            .data
            .syminfo
            .recommendations_sell
            .map_or(Value::Na, Value::Int),
        BuiltinSeries::SyminfoRecommendationsSellStrong => vm
            .data
            .syminfo
            .recommendations_sell_strong
            .map_or(Value::Na, Value::Int),
        BuiltinSeries::SyminfoRecommendationsTotal => vm
            .data
            .syminfo
            .recommendations_total
            .map_or(Value::Na, Value::Int),
        BuiltinSeries::SyminfoTargetPriceAverage => vm
            .data
            .syminfo
            .target_price_average
            .map_or(Value::Na, Value::from_f64),
        BuiltinSeries::SyminfoTargetPriceDate => vm
            .data
            .syminfo
            .target_price_date
            .map_or(Value::Na, Value::Int),
        BuiltinSeries::SyminfoTargetPriceEstimates => vm
            .data
            .syminfo
            .target_price_estimates
            .map_or(Value::Na, Value::from_f64),
        BuiltinSeries::SyminfoTargetPriceHigh => vm
            .data
            .syminfo
            .target_price_high
            .map_or(Value::Na, Value::from_f64),
        BuiltinSeries::SyminfoTargetPriceLow => vm
            .data
            .syminfo
            .target_price_low
            .map_or(Value::Na, Value::from_f64),
        BuiltinSeries::SyminfoTargetPriceMedian => vm
            .data
            .syminfo
            .target_price_median
            .map_or(Value::Na, Value::from_f64),
        BuiltinSeries::ChartLeftVisibleBarTime => vm
            .data
            .chart_left_visible_bar_time
            .map_or(Value::Na, Value::Int),
        BuiltinSeries::ChartRightVisibleBarTime => vm
            .data
            .chart_right_visible_bar_time
            .map_or(Value::Na, Value::Int),
        // ---- dividends.future_* / earnings.future_*: each reads its
        // optional host `upcoming_events` field, `na` when absent. Amounts /
        // EPS / revenue map to `Value::from_f64`; epoch-ms dates / times map
        // to `Value::Int`. ----
        BuiltinSeries::DividendsFutureAmount => vm
            .data
            .upcoming_events
            .dividends_future_amount
            .map_or(Value::Na, Value::from_f64),
        BuiltinSeries::DividendsFutureExDate => vm
            .data
            .upcoming_events
            .dividends_future_ex_date
            .map_or(Value::Na, Value::Int),
        BuiltinSeries::DividendsFuturePayDate => vm
            .data
            .upcoming_events
            .dividends_future_pay_date
            .map_or(Value::Na, Value::Int),
        BuiltinSeries::EarningsFutureEps => vm
            .data
            .upcoming_events
            .earnings_future_eps
            .map_or(Value::Na, Value::from_f64),
        BuiltinSeries::EarningsFuturePeriodEndTime => vm
            .data
            .upcoming_events
            .earnings_future_period_end_time
            .map_or(Value::Na, Value::Int),
        BuiltinSeries::EarningsFutureRevenue => vm
            .data
            .upcoming_events
            .earnings_future_revenue
            .map_or(Value::Na, Value::from_f64),
        BuiltinSeries::EarningsFutureTime => vm
            .data
            .upcoming_events
            .earnings_future_time
            .map_or(Value::Na, Value::Int),
        // ---- session.*: this headless VM models one continuous
        // regular session, so the extended-session predicates are false and
        // the regular ones track the first/last bar of the whole run. ----
        BuiltinSeries::SessionIsfirstbar | BuiltinSeries::SessionIsfirstbarRegular => {
            Value::Bool(index == 0)
        }
        BuiltinSeries::SessionIslastbar | BuiltinSeries::SessionIslastbarRegular => {
            Value::Bool(index + 1 == vm.bars)
        }
        BuiltinSeries::SessionIsmarket => Value::Bool(true),
        BuiltinSeries::SessionIspremarket | BuiltinSeries::SessionIspostmarket => {
            Value::Bool(false)
        }
        // Host-authoritative symbol trading day, with exchange-local
        // calendar midnight as the deterministic no-lane fallback.
        BuiltinSeries::TimeTradingday => Value::Int(old_trading_day_at(&vm.data, index)),
        // The `strategy.*` getters are dispatched to `read_strategy` before
        // this market-data match (see the early return above), so no other
        // variant reaches here.
        _ => unreachable!("strategy getter reached market-data match"),
    }
}

/// Reference implementation of the `Vm::resolve_input` body.
pub(super) fn old_resolve_input(vm: &Vm, input: u32, pc: usize) -> Result<Value, RuntimeError> {
    let decl = vm.program.inputs.get(input as usize).ok_or_else(|| {
        vm.fail(
            RuntimeErrorKind::InvalidProgram,
            format!("input {input} out of range"),
            pc,
        )
    })?;
    // Cross-script sources are a host-only override. Identity travels in
    // the normal input override map while samples travel in a separate
    // lane keyed by this downstream input id. Require both to agree before
    // consuming values; stale/missing/malformed lanes fall back to the
    // declaration's builtin source deterministically.
    if let Some(InputValue::ExternalSource(source)) = vm.options.inputs.get(&input) {
        if decl.kind == InputKind::Source && source.is_valid() {
            if let Some(lane) = vm
                .options
                .external_sources
                .get(&input)
                .filter(|lane| lane.is_valid_for(source))
            {
                return Ok(match lane.values.get(vm.bar).copied().flatten() {
                    Some(value) => Value::from_f64(value),
                    None => Value::Na,
                });
            }
        }
        return Ok(match &decl.defval {
            InputValue::Source(series) if series.is_input_source() => {
                old_read_builtin(vm, *series, 0)
            }
            _ => Value::Na,
        });
    }
    // An override is used only when its value type matches the declared
    // kind; otherwise fall back to the default (hosts validate first).
    let chosen = match vm.options.inputs.get(&input) {
        Some(v) if old_input_value_matches_decl(v, decl) => v,
        _ => &decl.defval,
    };
    Ok(match chosen {
        // A `float` (or `price`) input always yields a float; an int
        // override (accepted by `input_value_matches_decl`) widens rather
        // than leaking an `Int` where the declared type is `float`.
        InputValue::Int(i) if matches!(decl.kind, InputKind::Float | InputKind::Price) => {
            Value::from_f64(*i as f64)
        }
        InputValue::Int(i) => Value::Int(*i),
        InputValue::Float(f) => Value::from_f64(*f),
        InputValue::Bool(b) => Value::Bool(*b),
        InputValue::String(s) => Value::String(s.clone()),
        InputValue::Color { r, g, b, t } => Value::Color {
            r: *r,
            g: *g,
            b: *b,
            t: *t,
            input_origin: None,
        },
        // A source input reads the selected series' current-bar sample.
        InputValue::Source(series) => old_read_builtin(vm, *series, 0),
        // Handled above. This defensive fallback protects direct VM users
        // even if a future caller bypasses that identity/lane check.
        InputValue::ExternalSource(_) => match &decl.defval {
            InputValue::Source(series) if series.is_input_source() => {
                old_read_builtin(vm, *series, 0)
            }
            _ => Value::Na,
        },
        // An enum input resolves to the selected member's variant index —
        // the same `int` any enum member lowers to (see the analyzer's
        // enum-member reference). The name is metadata only.
        InputValue::Enum { index, .. } => Value::Int(*index as i64),
    })
}

/// Reference implementation of the `exec_read_builtin_dynamic` body after the pop,
/// without the push: `Ok(value)` or the too-deep error.
fn old_read_builtin_dynamic(
    vm: &Vm,
    source: BuiltinSeries,
    offset: Value,
    pc: usize,
) -> Result<Value, RuntimeError> {
    Ok(match Vm::runtime_history_offset(offset) {
        Some(offset) => {
            // Reference implementation of the `check_dynamic_history_offset` check.
            let offset = {
                let limit = vm.program.metadata.max_bars_back;
                if offset > u64::from(limit) {
                    return Err(vm.dynamic_history_too_deep(offset, pc));
                }
                offset as u32
            };
            old_read_builtin(vm, source, offset)
        }
        _ => Value::Na,
    })
}

// ---- helpers ----------------------------------------------------------------

pub(super) fn key(v: &Value) -> String {
    match v {
        Value::Float(x) => format!("Float:{:016x}", x.to_bits()),
        other => format!("{other:?}"),
    }
}

fn res_key(r: &Result<Value, RuntimeError>) -> String {
    match r {
        Ok(v) => key(v),
        Err(e) => format!("err:{e:?}"),
    }
}

/// Run `f`, mapping a panic to `"panic"` (the reference and new bodies must
/// panic on the same inputs: a short market lane).
pub(super) fn guarded(f: impl FnOnce() -> String) -> String {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| "panic".into())
}

pub(super) struct Rng(pub(super) u64);
impl Rng {
    pub(super) fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    pub(super) fn price(&mut self) -> f64 {
        match self.next() % 12 {
            0 => f64::NAN,
            1 => f64::INFINITY,
            2 => f64::NEG_INFINITY,
            3 => f64::MAX,
            4 => -f64::MAX,
            5 => f64::from_bits(1),
            6 => -0.0,
            7 => f64::from_bits(self.next()),
            _ => ((self.next() % 200_001) as f64 - 100_000.0) / 64.0,
        }
    }
    fn lane(&mut self, n: usize) -> Vec<f64> {
        (0..n).map(|_| self.price()).collect()
    }
    /// Mostly `full`; one time in nine a shorter length (a panic on both
    /// sides when the read reaches past it).
    pub(super) fn len(&mut self, full: usize) -> usize {
        if self.next() % 9 == 0 {
            (self.next() as usize) % (full + 1)
        } else {
            full
        }
    }
}

pub(super) fn program(src: &str) -> pinewasm_csil::CsilProgram {
    let parsed = pinewasm_parse::parse(src);
    let analysis = pinewasm_semantics::analyze(&parsed.program);
    let mir = pinewasm_mir::lower(&analysis.hir);
    pinewasm_csil::lower_mir(&mir).expect("CSIL lowering")
}

/// A VM over `bars` bars; the lanes are then overwritten directly
/// (bypassing load-time validation) with random data, short lanes, an empty
/// or explicit `time` lane and missing or short quote lanes.
pub(super) fn feed_vm(rng: &mut Rng, bars: usize, max_bars_back: u32) -> Vm {
    let prog = program("indicator(\"T\")\nplot(close)\n");
    let data = MarketData {
        open: vec![1.0; bars],
        high: vec![1.0; bars],
        low: vec![1.0; bars],
        close: vec![1.0; bars],
        volume: vec![1.0; bars],
        ..MarketData::default()
    };
    let mut vm = Vm::new(&prog, &data, &RunOptions::default()).expect("vm");
    // Set after construction: a huge limit would size the history ring.
    std::sync::Arc::make_mut(&mut vm.program)
        .metadata
        .max_bars_back = max_bars_back;
    let shape = rng.next();
    let n = rng.len(bars);
    vm.data.open = rng.lane(n);
    let n = rng.len(bars);
    vm.data.high = rng.lane(n);
    let n = rng.len(bars);
    vm.data.low = rng.lane(n);
    vm.data.close = rng.lane(bars);
    let n = rng.len(bars);
    vm.data.volume = rng.lane(n);
    vm.data.time = match shape % 3 {
        0 => Vec::new(),
        1 => (0..bars)
            .map(|i| 1_700_000_000_000 + (i as i64) * 60_000)
            .collect(),
        _ => {
            let n = rng.len(bars);
            (0..n).map(|_| (rng.next() as i64) >> 3).collect()
        }
    };
    vm.data.time_close = if (shape >> 2) % 2 == 0 {
        Vec::new()
    } else {
        vm.data
            .time
            .iter()
            .map(|t| t.wrapping_add(59_999))
            .collect()
    };
    let n = (rng.next() as usize) % (bars + 2);
    vm.data.bid = rng.lane(n);
    let n = match (shape >> 4) % 3 {
        0 => 0,
        1 => bars,
        _ => (rng.next() as usize) % (bars + 2),
    };
    vm.data.ask = rng.lane(n);
    vm.data.timeframe = ["1D", "60", "1W", "5S", "1M", ""][(shape >> 6) as usize % 6].into();
    vm
}

// ---- the tests ----------------------------------------------------------------

/// `feed_series_of` selects exactly the series the feed kernel computes.
#[test]
fn feed_series_classifier_covers_the_feed_series_only() {
    let feed = [
        BuiltinSeries::Open,
        BuiltinSeries::High,
        BuiltinSeries::Low,
        BuiltinSeries::Volume,
        BuiltinSeries::Hl2,
        BuiltinSeries::Hlc3,
        BuiltinSeries::Ohlc4,
        BuiltinSeries::Hlcc4,
        BuiltinSeries::BarIndex,
        BuiltinSeries::Time,
        BuiltinSeries::LastBarTime,
        BuiltinSeries::LastBarIndex,
        BuiltinSeries::BarstateIsfirst,
        BuiltinSeries::Bid,
        BuiltinSeries::Ask,
    ];
    let mut n = 0;
    for s in BuiltinSeries::ALL {
        let got = vm_ops::feed_series_of(s);
        assert_eq!(got.is_some(), feed.contains(&s), "{s:?}");
        if got.is_some() {
            assert!(!s.is_strategy(), "{s:?}");
            n += 1;
        }
    }
    assert_eq!(n, feed.len());
}

/// `read_builtin` (every series, feed or residual) against the reference body.
#[test]
fn read_builtin_matches_the_old_arm_on_every_series() {
    let mut rng = Rng(0x5eed_1234_abcd_0001);
    let offsets = [0u32, 1, 2, 3, 5, 7, 64, u32::MAX];
    let mut feed_cases = 0usize;
    for round in 0..160 {
        let bars = 1 + (round % 9);
        let mut vm = feed_vm(&mut rng, bars, 100);
        for bar in 0..bars {
            vm.bar = bar;
            for s in BuiltinSeries::ALL {
                for &offset in &offsets {
                    let old = guarded(|| key(&old_read_builtin(&vm, s, offset)));
                    let new = guarded(|| key(&vm.read_builtin(s, offset)));
                    assert_eq!(new, old, "{s:?} bar={bar} offset={offset} bars={bars}");
                    if vm_ops::feed_series_of(s).is_some() {
                        feed_cases += 1;
                    }
                }
            }
        }
    }
    assert!(feed_cases > 50_000, "{feed_cases}");
}

/// The feed kernel against the reference body directly, on lanes of arbitrary
/// bit patterns (every NaN payload, subnormal and overflowing average).
#[test]
fn feed_series_read_matches_the_old_arm_on_random_bits() {
    let mut rng = Rng(77);
    for _ in 0..400 {
        let bars = 1 + (rng.next() % 6) as usize;
        let mut vm = feed_vm(&mut rng, bars, 100);
        let mut bits = || -> Vec<f64> { (0..bars).map(|_| f64::from_bits(rng.next())).collect() };
        vm.data.open = bits();
        vm.data.high = bits();
        vm.data.low = bits();
        vm.data.close = bits();
        vm.data.volume = bits();
        for bar in 0..bars {
            vm.bar = bar;
            for s in BuiltinSeries::ALL {
                let Some(series) = vm_ops::feed_series_of(s) else {
                    continue;
                };
                for offset in 0..=(bars as u32) {
                    let old = guarded(|| key(&old_read_builtin(&vm, s, offset)));
                    let new = guarded(|| {
                        key(&vm_ops::feed_series_read(
                            series,
                            vm.bar,
                            offset,
                            vm.bars,
                            &vm.feed_cols(),
                        ))
                    });
                    assert_eq!(new, old, "{s:?} bar={bar} offset={offset}");
                }
            }
        }
    }
}

/// `ReadBuiltinDynamic` after the pop: the offset check and read
/// against the reference body, on every offset shape and `max_bars_back`.
#[test]
fn read_builtin_dynamic_matches_the_old_arm() {
    let mut rng = Rng(99);
    let mut offsets: Vec<Value> = vec![
        Value::Na,
        Value::Bool(true),
        Value::String("1".into()),
        Value::Color {
            r: 0,
            g: 0,
            b: 0,
            t: 0.0,
            input_origin: None,
        },
        Value::Ref(pinewasm_mir::HandleId(0)),
        Value::Int(i64::MIN),
        Value::Int(i64::MAX),
        Value::Int(i64::from(u32::MAX)),
        Value::Int(i64::from(u32::MAX) + 1),
        Value::Float(f64::NAN),
        Value::Float(f64::INFINITY),
        Value::Float(-0.5),
        Value::Float(-1.0),
        Value::Float(1e300),
        Value::Float(4_294_967_295.9),
        Value::Float(4_294_967_296.0),
    ];
    offsets.extend((-3..12).map(Value::Int));
    offsets.extend(
        [
            -0.0, 0.5, 0.99, 1.5, 2.5, 3.999, 7.0, 99.9, 100.0, 100.5, 101.0,
        ]
        .iter()
        .map(|&f| Value::Float(f)),
    );
    for limit in [0u32, 1, 3, 100, u32::MAX] {
        for round in 0..12 {
            let bars = 1 + round % 7;
            let mut vm = feed_vm(&mut rng, bars, limit);
            for bar in 0..bars {
                vm.bar = bar;
                for s in BuiltinSeries::ALL {
                    for offset in &offsets {
                        let old = guarded(|| {
                            res_key(&old_read_builtin_dynamic(&vm, s, offset.clone(), 3))
                        });
                        let new = guarded(|| {
                            let r = match Vm::runtime_history_offset(offset.clone()) {
                                Some(o) => vm
                                    .check_dynamic_history_offset(o, 3)
                                    .map(|o| vm.read_builtin(s, o)),
                                None => Ok(Value::Na),
                            };
                            res_key(&r)
                        });
                        assert_eq!(new, old, "{s:?} {offset:?} limit={limit} bar={bar}");
                    }
                }
            }
        }
    }
}

/// The whole `ReadBuiltinDynamic` arm (pop, decision, read, push) on a VM.
#[test]
fn exec_read_builtin_dynamic_pushes_the_old_value() {
    let mut rng = Rng(4242);
    for limit in [0u32, 2, 100] {
        let mut vm = feed_vm(&mut rng, 5, limit);
        vm.data.open = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        vm.data.high = vec![1.0; 5];
        vm.data.low = vec![1.0; 5];
        vm.data.close = vec![1.0; 5];
        vm.bar = 4;
        let offsets = [
            Value::Int(0),
            Value::Int(1),
            Value::Float(2.5),
            Value::Int(4),
            Value::Int(9),
            Value::Na,
        ];
        for offset in offsets {
            vm.stack.clear();
            vm.stack.push(offset.clone());
            let new = vm
                .exec_read_builtin_dynamic(BuiltinSeries::Open, 0)
                .map(|()| vm.stack.pop().expect("pushed"));
            let old = old_read_builtin_dynamic(&vm, BuiltinSeries::Open, offset.clone(), 0);
            assert_eq!(res_key(&new), res_key(&old), "{offset:?} limit={limit}");
            assert!(vm.stack.is_empty());
        }
    }
}

/// `resolve_input` against the reference body: every input kind, with each kind
/// of default, and with matching, mismatching and absent overrides, with
/// and without an external-source lane.
#[test]
fn resolve_input_matches_the_old_arm() {
    let base =
        program("indicator(\"T\")\nx = input.int(3)\ny = input.source(close)\nplot(x + y)\n");
    let template = base.inputs[0].clone();
    let source_template = base.inputs[1].clone();
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
    let ext = ExternalSourceRef {
        script_id: "up".into(),
        plot_slot: 0,
        plot_label: "P".into(),
    };
    let enum_value = |name: &str, index: u32| InputValue::Enum {
        name: name.into(),
        index,
        member: "m".into(),
        title: None,
    };
    let values = vec![
        InputValue::Int(0),
        InputValue::Int(-7),
        InputValue::Int(i64::MAX),
        InputValue::Int(i64::MIN),
        InputValue::Int((1 << 53) + 1),
        InputValue::Float(2.5),
        InputValue::Float(-0.0),
        InputValue::Float(f64::NAN),
        InputValue::Float(f64::INFINITY),
        InputValue::Float(f64::from_bits(1)),
        InputValue::Bool(true),
        InputValue::String("s".into()),
        InputValue::Color {
            r: 1,
            g: 2,
            b: 3,
            t: 100.0,
        },
        InputValue::Color {
            r: 1,
            g: 2,
            b: 3,
            t: 101.0,
        },
        InputValue::Source(BuiltinSeries::Open),
        InputValue::Source(BuiltinSeries::Hlc3),
        InputValue::Source(BuiltinSeries::BarIndex),
        InputValue::ExternalSource(ext.clone()),
        enum_value("E", 1),
        enum_value("E", u32::MAX),
        enum_value("F", 0),
    ];
    let mut prog = base.clone();
    prog.inputs.clear();
    for kind in kinds {
        for defval in &values {
            let mut decl = if kind == InputKind::Source {
                source_template.clone()
            } else {
                template.clone()
            };
            decl.id = prog.inputs.len() as u32;
            decl.kind = kind;
            decl.defval = defval.clone();
            prog.inputs.push(decl);
        }
    }
    let n = prog.inputs.len() as u32;
    let data = MarketData {
        open: vec![1.5, 2.5, f64::NAN],
        high: vec![3.0, 4.0, 5.0],
        low: vec![0.5, 1.0, 1.5],
        close: vec![2.0, 3.0, 4.0],
        volume: vec![1.0; 3],
        ..MarketData::default()
    };
    let mut overrides: Vec<Option<InputValue>> = vec![None];
    overrides.extend(values.iter().cloned().map(Some));
    let mut checked = 0usize;
    for ov in &overrides {
        for with_lane in [false, true] {
            let mut options = RunOptions::default();
            if let Some(v) = ov {
                for id in 0..n {
                    options.inputs.insert(id, v.clone());
                }
            }
            if with_lane {
                for id in 0..n {
                    options.external_sources.insert(
                        id,
                        ExternalSourceLane {
                            source: ext.clone(),
                            values: vec![Some(9.5), None, Some(f64::NAN)],
                        },
                    );
                }
            }
            let Ok(mut vm) = Vm::new(&prog, &data, &options) else {
                continue;
            };
            for bar in 0..3 {
                vm.bar = bar;
                for input in 0..=n {
                    // The reference copy has no palette provenance. Set the
                    // input identity on its expected color; keep exact Debug
                    // comparison for every channel and error, and verify that
                    // the production resolver supplies that identity.
                    let old = guarded(|| {
                        let mut expected = old_resolve_input(&vm, input, 5);
                        if let Ok(Value::Color { input_origin, .. }) = &mut expected {
                            *input_origin = Some(input);
                        }
                        res_key(&expected)
                    });
                    let new = guarded(|| res_key(&vm.resolve_input(input, 5)));
                    assert_eq!(new, old, "input={input} override={ov:?} lane={with_lane}");
                    checked += 1;
                }
            }
        }
    }
    assert!(checked > 10_000, "{checked}");
}
