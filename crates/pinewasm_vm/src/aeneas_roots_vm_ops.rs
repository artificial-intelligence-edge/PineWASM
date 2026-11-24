//! Aeneas extraction roots for the arithmetic and comparison kernels (`--features aeneas` only).
//!
//! Thin, behavior-free wrappers whose only job is to make Charon translate the
//! scalar opcode kernels in `crate::vm_ops` (what `Vm::exec_instr` calls for the
//! constant, arithmetic, comparison, logic, `select`, conditional-jump, dynamic
//! user-slot and string-`+` arms, and `pop_float`/`pop_bool`), and the read and
//! pop kernels (the market-feed builtin series, the
//! `max_bars_back` check of the dynamic builtin read, the scalar input values
//! and `Pop`). Extract with
//! `--opaque=pinewasm_vm::vm_ops::vm_f64_ops`, `--opaque=pinewasm_vm::vm_ops::vm_str_ops`
//! and `--opaque=pinewasm_vm::vm_ops::F64Model`, and include the body of
//! `pinewasm_mir::Value::from_f64`
//! (`--include=pinewasm_mir::value::{pinewasm_mir::value::Value}::from_f64`).

#![allow(private_interfaces)]

use crate::vm_ops::{
    self, CmpOp, FeedCols, FeedSeries, FloatArithOp, InputScalar, IntArithOp, OpMismatch, VmConst,
};
use pinewasm_mir::Value;

/// Root that makes the `vm_ops::F64Model` marker part of the extraction.
pub fn root_f64_model(_model: &vm_ops::F64Model) {}

/// Root for `vm_ops::const_value`.
pub fn root_const_value(c: VmConst) -> Value {
    vm_ops::const_value(c)
}

/// Root for `vm_ops::float_operand`.
pub fn root_float_operand(v: Value) -> Result<Option<f64>, Value> {
    vm_ops::float_operand(v)
}

/// Root for `vm_ops::bool_operand`.
pub fn root_bool_operand(v: Value) -> Result<Option<bool>, Value> {
    vm_ops::bool_operand(v)
}

/// Root for `vm_ops::int_arith`.
pub fn root_int_arith(op: IntArithOp, lhs: &Value, rhs: &Value) -> Result<Value, OpMismatch> {
    vm_ops::int_arith(op, lhs, rhs)
}

/// Root for `vm_ops::int_neg`.
pub fn root_int_neg(v: &Value) -> Result<Value, OpMismatch> {
    vm_ops::int_neg(v)
}

/// Root for `vm_ops::int_to_f64`.
pub fn root_int_to_f64(v: &Value) -> Result<Value, OpMismatch> {
    vm_ops::int_to_f64(v)
}

/// Root for `vm_ops::float_arith`.
pub fn root_float_arith(op: FloatArithOp, lhs: Option<f64>, rhs: Option<f64>) -> Value {
    vm_ops::float_arith(op, lhs, rhs)
}

/// Root for `vm_ops::float_neg`.
pub fn root_float_neg(v: Option<f64>) -> Value {
    vm_ops::float_neg(v)
}

/// Root for `vm_ops::cmp_values`.
pub fn root_cmp_values(op: CmpOp, lhs: &Value, rhs: &Value) -> Result<Value, OpMismatch> {
    vm_ops::cmp_values(op, lhs, rhs)
}

/// Root for `vm_ops::cmp_eq`.
pub fn root_cmp_eq(negate: bool, lhs: &Value, rhs: &Value) -> Result<Value, OpMismatch> {
    vm_ops::cmp_eq(negate, lhs, rhs)
}

/// Root for `vm_ops::bool_and`.
pub fn root_bool_and(lhs: Option<bool>, rhs: Option<bool>) -> Value {
    vm_ops::bool_and(lhs, rhs)
}

/// Root for `vm_ops::bool_or`.
pub fn root_bool_or(lhs: Option<bool>, rhs: Option<bool>) -> Value {
    vm_ops::bool_or(lhs, rhs)
}

/// Root for `vm_ops::bool_not`.
pub fn root_bool_not(v: Option<bool>) -> Value {
    vm_ops::bool_not(v)
}

/// Root for `vm_ops::select_value`.
pub fn root_select_value<T>(cond: Option<bool>, then_value: T, else_value: T) -> T {
    vm_ops::select_value(cond, then_value, else_value)
}

/// Root for `vm_ops::jump_if_false_taken`.
pub fn root_jump_if_false_taken(cond: Option<bool>) -> bool {
    vm_ops::jump_if_false_taken(cond)
}

/// Root for `vm_ops::jump_if_exact_taken`.
pub fn root_jump_if_exact_taken(cond: Option<bool>, on: bool) -> bool {
    vm_ops::jump_if_exact_taken(cond, on)
}

/// Root for `vm_ops::history_offset`.
pub fn root_history_offset(v: &Value) -> Option<u64> {
    vm_ops::history_offset(v)
}

/// Root for `vm_ops::user_slot_dynamic`.
pub fn root_user_slot_dynamic<T: Clone>(
    slots: &[Vec<T>],
    offset: &Value,
    limit: u32,
    bar: usize,
    mask: usize,
    slot: usize,
) -> Result<Option<T>, u64> {
    vm_ops::user_slot_dynamic(slots, offset, limit, bar, mask, slot)
}

/// Root for `vm_ops::str_concat_operands`.
#[allow(clippy::type_complexity)]
pub fn root_str_concat_operands<'a>(
    lhs: &'a Value,
    rhs: &'a Value,
) -> Result<Option<(&'a String, &'a String)>, OpMismatch> {
    vm_ops::str_concat_operands(lhs, rhs)
}

/// Root for `vm_ops::time_at`.
pub fn root_time_at(time: &[i64], index: usize) -> i64 {
    vm_ops::time_at(time, index)
}

/// Root for `vm_ops::feed_series_read`.
pub fn root_feed_series_read(
    series: FeedSeries,
    bar: usize,
    offset: u32,
    bars: usize,
    cols: &FeedCols<'_>,
) -> Value {
    vm_ops::feed_series_read(series, bar, offset, bars, cols)
}

/// Root for `vm_ops::feed_series_value`.
pub fn root_feed_series_value(
    series: FeedSeries,
    index: usize,
    bars: usize,
    cols: &FeedCols<'_>,
) -> Value {
    vm_ops::feed_series_value(series, index, bars, cols)
}

/// Root for `vm_ops::dynamic_offset_limit`.
pub fn root_dynamic_offset_limit(offset: u64, limit: u32) -> Option<u32> {
    vm_ops::dynamic_offset_limit(offset, limit)
}

/// Root for `vm_ops::input_scalar_value`.
pub fn root_input_scalar_value(x: InputScalar, float_kind: bool) -> Value {
    vm_ops::input_scalar_value(x, float_kind)
}

/// Root for `vm_ops::discard`.
pub fn root_discard(v: Value) {
    vm_ops::discard(v);
}

// ---- second group of read kernels -------------------------------------------
// The residual numeric and boolean builtin series (strategy snapshot,
// calendar, `barstate.*`, `timeframe.*`, `session.*`, `time_close` and
// `time_tradingday`, host metadata), the scalar input override test and the
// external-source sample. The imports for this group are placed here.

use crate::vm_ops::{
    BarFlag, BarState, BarstateSeries, CalendarPart, HostCols, HostSeries, StrategySeries,
    TimeframeSeries,
};
use pinewasm_broker::StrategySnapshot;
use pinewasm_hir::InputKind;

/// Root for `vm_ops::history_index`.
pub fn root_history_index(bar: usize, offset: u32) -> Option<usize> {
    vm_ops::history_index(bar, offset)
}

/// Root for `vm_ops::opt_int_value`.
pub fn root_opt_int_value(x: Option<i64>) -> Value {
    vm_ops::opt_int_value(x)
}

/// Root for `vm_ops::opt_float_value`.
pub fn root_opt_float_value(x: Option<f64>) -> Value {
    vm_ops::opt_float_value(x)
}

/// Root for `vm_ops::strategy_series_value`.
pub fn root_strategy_series_value(series: StrategySeries, snap: &StrategySnapshot) -> Value {
    vm_ops::strategy_series_value(series, snap)
}

/// Root for `vm_ops::calendar_series_value`.
pub fn root_calendar_series_value(part: CalendarPart, time_ms: i64, off: i64) -> Value {
    vm_ops::calendar_series_value(part, time_ms, off)
}

/// Root for `vm_ops::is_last_bar`.
pub fn root_is_last_bar(index: usize, st: &BarState) -> bool {
    vm_ops::is_last_bar(index, st)
}

/// Root for `vm_ops::is_history`.
pub fn root_is_history(index: usize, st: &BarState) -> bool {
    vm_ops::is_history(index, st)
}

/// Root for `vm_ops::is_new`.
pub fn root_is_new(index: usize, st: &BarState) -> bool {
    vm_ops::is_new(index, st)
}

/// Root for `vm_ops::is_confirmed`.
pub fn root_is_confirmed(index: usize, st: &BarState) -> bool {
    vm_ops::is_confirmed(index, st)
}

/// Root for `vm_ops::is_last_confirmed_history`.
pub fn root_is_last_confirmed_history(index: usize, st: &BarState) -> bool {
    vm_ops::is_last_confirmed_history(index, st)
}

/// Root for `vm_ops::barstate_value`.
pub fn root_barstate_value(series: BarstateSeries, index: usize, st: &BarState) -> Value {
    vm_ops::barstate_value(series, index, st)
}

/// Root for `vm_ops::timeframe_series_value`.
pub fn root_timeframe_series_value(
    series: TimeframeSeries,
    mult: i64,
    unit: Option<char>,
) -> Value {
    vm_ops::timeframe_series_value(series, mult, unit)
}

/// Root for `vm_ops::bar_flag_value`.
pub fn root_bar_flag_value(flag: BarFlag, index: usize, bars: usize) -> Value {
    vm_ops::bar_flag_value(flag, index, bars)
}

/// Root for `vm_ops::time_close_lane`.
pub fn root_time_close_lane(time_close: &[i64], index: usize) -> Option<i64> {
    vm_ops::time_close_lane(time_close, index)
}

/// Root for `vm_ops::time_close_fallback`.
pub fn root_time_close_fallback(open_ms: i64, tf_seconds: i64) -> i64 {
    vm_ops::time_close_fallback(open_ms, tf_seconds)
}

/// Root for `vm_ops::trading_day_lane`.
pub fn root_trading_day_lane(trading_day: &[i64], index: usize) -> Option<i64> {
    vm_ops::trading_day_lane(trading_day, index)
}

/// Root for `vm_ops::fallback_trading_day`.
pub fn root_fallback_trading_day(time: i64, off: i64) -> i64 {
    vm_ops::fallback_trading_day(time, off)
}

/// Root for `vm_ops::host_series_value`.
pub fn root_host_series_value(series: HostSeries, cols: &HostCols<'_>) -> Value {
    vm_ops::host_series_value(series, cols)
}

/// Root for `vm_ops::input_scalar_matches`.
pub fn root_input_scalar_matches(x: InputScalar, kind: InputKind) -> bool {
    vm_ops::input_scalar_matches(x, kind)
}

/// Root for `vm_ops::external_sample_value`.
pub fn root_external_sample_value(values: &[Option<f64>], bar: usize) -> Value {
    vm_ops::external_sample_value(values, bar)
}
