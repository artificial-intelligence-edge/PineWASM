//! The PineWASM VM: a deterministic interpreter for validated CSIL.
//!
//! Execution model (see `docs/runtime.md`):
//!
//! ```text
//! for bar_index in 0..bars:
//!     run `main` once
//!     user series writes land in the current bar's cell
//!     plot effects land in the current bar's plot cells
//! ```
//!
//! History reads (`offset > 0`) index earlier bars; reads before bar 0 (or
//! of never-written cells) produce `na`. All limits are enforced with
//! structured [`RuntimeError`]s. A panic caused by program behavior is a
//! defect, not part of the VM's public error contract.
//!
//! Persistent slots (`var` declarations) carry the previous bar's final
//! value into the current bar's cell at the start of each bar (bar > 0),
//! and every slot keeps a `written` flag — set by `StoreUserSlotCurrent`,
//! never by carry-forward, persisted across bars — that
//! `JumpIfSlotWritten` branches on so `var` initializers run exactly once.
//!
//! Realtime-bar semantics (`varip`, tick rollback, `barstate.*`) follow one
//! model: an optional [`RealtimeSim`] segment replays ticks on the last
//! bar(s), restoring all non-`varip` state to the bar's start before each
//! tick and committing on `is_bar_close` (see [`run`] and docs/runtime.md).
#![forbid(unsafe_code)]

use fancy_regex::{Regex, RegexBuilder};
use pinewasm_csil::{
    CalendarField, CsilInstr, CsilProgram, DisplayStyle, OrderKind, PlotColorRole, PureFunc,
    TickerFunc, TradeAccessor, MAX_DYNAMIC_WINDOW_DEPTH, MAX_WINDOW_LENGTH,
};
use pinewasm_hir::{
    BuiltinSeries, CellAttr, CollectionFunc, DrawKind, ExternalSourceRef, HtfExpr,
    InputActiveBinaryOp, InputActiveExpr, InputActiveUnaryOp, InputDecl, InputKind, InputValue,
    LogLevel, PivotSystem, StrategyInfo, TaFunc, TaInputTarget, MAX_INPUT_ACTIVE_STRING_BYTES,
};
use pinewasm_mir::heap::{ElemType, Heap, HeapObj, HeapRestoreError};
use pinewasm_mir::{HandleId, Value};
use pinewasm_span::Span;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};
use thiserror::Error;

mod primary_samples;

type SecurityMemoKey = (BuiltinSeries, i64, resample::CalendarAlignment, bool, bool);
type SecurityExprMemoKey = (usize, i64, resample::CalendarAlignment, bool, bool);
type SecurityMemo = HashMap<SecurityMemoKey, Vec<Option<f64>>>;
type SecurityExprMemo = HashMap<SecurityExprMemoKey, Vec<Option<f64>>>;

/// Interpreter and native snapshots share this conversion. Values are
/// executed engine cells, never host reevaluations of Pine expressions.
pub(crate) fn resolve_display_style<'a>(
    source: &DisplayStyle,
    is_fill: bool,
    mut value_at: impl FnMut(u32) -> Option<&'a Value>,
    mut input_at: impl FnMut(u32) -> Option<&'a InputValue>,
    mut input_override_at: impl FnMut(u32) -> Option<&'a InputValue>,
) -> DisplayStyle {
    let mut style = source.clone();
    for entry in &mut style.color_palette {
        let rgba = match entry.source {
            pinewasm_hir::StyleColorSource::Constant { r, g, b, t } => Some((r, g, b, t)),
            pinewasm_hir::StyleColorSource::Input { input } => match input_at(input) {
                Some(InputValue::Color { r, g, b, t }) => Some((*r, *g, *b, *t)),
                _ => None,
            },
        };
        entry.resolved_color = rgba.map(|(r, g, b, t)| Value::color_hex(r, g, b, t));
    }
    // Initial properties were normalized from source constructors. Only an
    // explicit valid run override replaces them; equal values do not erase
    // the distinction between default provenance and selected input values.
    for binding in &source.bound_scalars {
        if binding.field != "ohlc_body_color" {
            continue;
        }
        let InputActiveExpr::Input { input } = &binding.expression else {
            continue;
        };
        let Some(InputValue::Color { r, g, b, t }) = input_override_at(*input) else {
            continue;
        };
        if let Some(body) = &mut style.ohlc_body_style {
            body.static_color = Some(pinewasm_hir::OhlcBodyColor {
                r: *r,
                g: *g,
                b: *b,
                t: *t,
            });
        }
    }
    for binding in &source.runtime_scalars {
        let expression = match value_at(binding.slot.0) {
            Some(Value::Bool(v)) => InputActiveExpr::Value {
                value: InputValue::Bool(*v),
            },
            Some(Value::Int(v)) => InputActiveExpr::Value {
                value: InputValue::Int(*v),
            },
            Some(Value::Float(v)) if v.is_finite() => InputActiveExpr::Value {
                value: InputValue::Float(*v),
            },
            // A display set arrives as one canonical token; see
            // `display_binding` for the projection and its invariant.
            Some(Value::String(v)) if binding.field == "display" => {
                use pinewasm_hir::{
                    display_set, DisplayKind, PlotDisplay, UnsupportedVisualOption,
                };
                if (is_fill
                    || matches!(
                        source.kind,
                        DisplayKind::Hline | DisplayKind::Bgcolor | DisplayKind::Barcolor
                    ))
                    && display_set::mask(v).is_some_and(|mask| {
                        mask != display_set::ALL_MASK && mask & display_set::PINE_SCREENER_MASK != 0
                    })
                {
                    style.display = PlotDisplay::None;
                    style.unsupported.push(UnsupportedVisualOption {
                        field: "display".into(),
                        reason: "This output does not accept a screener display destination".into(),
                    });
                    display_binding::runtime_display_binding(display_set::DISPLAY_NONE)
                } else {
                    display_binding::runtime_display_binding(v)
                }
            }
            Some(Value::String(v)) => InputActiveExpr::Value {
                value: InputValue::String(v.clone()),
            },
            Some(Value::Color { r, g, b, t, .. }) => InputActiveExpr::Value {
                value: InputValue::Color {
                    r: *r,
                    g: *g,
                    b: *b,
                    t: *t,
                },
            },
            _ => InputActiveExpr::Na,
        };
        style
            .bound_scalars
            .retain(|entry| entry.field != binding.field);
        style.bound_scalars.push(pinewasm_hir::BoundStyleScalar {
            field: binding.field.clone(),
            expression,
        });
    }
    style.runtime_scalars.clear();
    style
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeDisplayStyles {
    pub plots: Vec<DisplayStyle>,
    pub visuals: Vec<DisplayStyle>,
    pub fills: Vec<DisplayStyle>,
}

/// The TA engine's builtin lanes in the resampled security context.
struct HtfLanes<'a>(&'a [resample::HtfBar]);

impl pinewasm_ta::Lanes for HtfLanes<'_> {
    fn open(&self, bar: u32) -> f64 {
        self.0.get(bar as usize).map_or(f64::NAN, |b| b.open)
    }
    fn high(&self, bar: u32) -> f64 {
        self.0.get(bar as usize).map_or(f64::NAN, |b| b.high)
    }
    fn low(&self, bar: u32) -> f64 {
        self.0.get(bar as usize).map_or(f64::NAN, |b| b.low)
    }
    fn close(&self, bar: u32) -> f64 {
        self.0.get(bar as usize).map_or(f64::NAN, |b| b.close)
    }
    fn volume(&self, bar: u32) -> f64 {
        self.0.get(bar as usize).map_or(f64::NAN, |b| b.volume)
    }
    fn time_ms(&self, bar: u32) -> i64 {
        self.0.get(bar as usize).map_or(0, |b| b.time)
    }
}

fn chart_calendar_alignment(timeframe: &str) -> resample::CalendarAlignment {
    let (multiplier, unit) = session::timeframe_parts(timeframe);
    let count = u32::try_from(multiplier).unwrap_or(u32::MAX);
    match unit {
        Some('M') => resample::CalendarAlignment::Months(count),
        Some('W') => resample::CalendarAlignment::Weeks(count),
        _ => resample::CalendarAlignment::Fixed,
    }
}

struct SecurityReadSpec<'a> {
    selector: Option<&'a str>,
    source: BuiltinSeries,
    htf_seconds: i64,
    timeframe_expr: Option<&'a str>,
    calendar_months: Option<u32>,
    gaps_on: bool,
    lookahead_on: bool,
}

/// The broker emulator lives in `pinewasm_broker` so the AOT
/// runtime blob can link the identical implementation, exactly as the `ta.*`
/// engine lives in `pinewasm_ta`. This shim preserves the
/// crate-internal `crate::broker::*` paths the VM and its tests use.
mod broker {
    pub use pinewasm_broker::*;
}
mod array_get;
mod array_math;
mod constant_pow;
mod display_binding;
mod format;
mod linalg;
mod loop_bound;
mod math_chain;
mod numeric_call;
mod numeric_stack;
mod session;
mod slot_branch;
mod slot_pair;
mod sma;
mod store_read;
mod ta;
mod ta_input_install;
mod ta_params;
mod ta_site;

/// Aeneas root for the current VM's bounded Color push transition.
#[cfg(feature = "aeneas")]
pub mod aeneas_roots_vm_color_push;
/// Extraction roots for the operation-dictionary generic indexed-distance
/// pruning nodes. Built only with `--features aeneas`.
#[cfg(feature = "aeneas")]
pub mod aeneas_roots_vm_distance_scan;
/// Aeneas extraction roots for the slot-ring, written-flag
/// and plot-row allocation kernels of `vm_layout`. Built only with `--features aeneas`.
#[cfg(feature = "aeneas")]
pub mod aeneas_roots_vm_layout;
/// Aeneas-extractable scalar opcode kernels of `vm_ops`. Built only with
/// `--features aeneas`.
#[cfg(feature = "aeneas")]
pub mod aeneas_roots_vm_ops;
/// Aeneas extraction roots for the user-slot cell kernels. Built only with `--features aeneas`.
#[cfg(feature = "aeneas")]
pub mod aeneas_roots_vm_slots;
/// Aeneas-extractable snapshot kernels. Built only with `--features aeneas`.
#[cfg(feature = "aeneas")]
pub mod aeneas_roots_vm_snapshot;
pub mod aot_host;
mod distance_scan;
mod host_exec;
pub mod resample;
mod vm_charge;
mod vm_close_step;
mod vm_dispatch;
mod vm_fault_location;
mod vm_history_step;
mod vm_layout;
mod vm_memory_reservation;
mod vm_ops;
mod vm_output_prefix;
mod vm_plot_step;
mod vm_publication_axis;
mod vm_resample_reservation;
mod vm_slots;
mod vm_snapshot;
mod vm_stack_push;
/// Re-exported from `pinewasm_broker` at its public path.
pub use pinewasm_broker::strategy_metrics;

use broker::Broker;
use format::{
    default_num_string, format_placeholders, parse_pine_number, str_repeat, str_replace_all,
    str_replace_nth, str_substring, tostring_value, MAX_STRING_LEN,
};
#[cfg(test)]
use format::{format_number, format_pattern, format_pattern_int};
use linalg::{
    identity_matrix, is_symmetric, jacobi_eigen, lu_determinant, lu_inverse, matmul,
    matrix_predicate, matrix_rank, pinv_full_rank,
};
use session::{
    civil_from_epoch_ms, format_time_string, timeframe_parts, timestamp_at, week_of_year_at,
    TimeSession,
};
pub use session::{
    resolve_timezone, timeframe_from_seconds, timeframe_in_seconds, timezone_offset_seconds,
};

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

/// OHLCV chart data plus the time axis and symbol metadata. All price/volume
/// arrays must have equal length; one index = one bar.
///
/// `mintick` is the instrument's minimum price increment
/// (`math.round_to_mintick` rounds to multiples of it); it defaults to
/// 0.01 when omitted and must be finite and positive.
///
/// In JSON, primary OHLCV samples accept numbers, `null` for NaN, and the
/// strings `"Infinity"` and `"-Infinity"`, as emitted by [`chart_request_json`].
/// Auxiliary lanes retain their existing decoding contracts.
///
/// `time` holds each bar's OPEN time as an epoch-millisecond timestamp. When
/// omitted (or empty) it is synthesized as a deterministic daily sequence
/// starting 2020-01-01T00:00:00Z with one `86_400_000` ms step per bar (see
/// [`Self::time_at`]); when present it must match the price arrays' length
/// and be strictly increasing. Execution rejects a time/timeframe combination
/// whose derived bar-close timestamp would exceed the `i64` range, unless
/// a `timeClose` lane supplies representable closes. The first
/// appended realtime bar materializes a synthetic history and records the
/// host tick's timestamp, subject to the same ordering and range checks.
///
/// `timeframe` is a Pine timeframe string (default `"1D"`); it drives
/// `time_close` and the `timeframe.*` predicates. Execution rejects a
/// duration that cannot be represented safely in epoch milliseconds.
///
/// `syminfo` carries symbol metadata (`syminfo.ticker`, `.timezone`, ...).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarketData {
    /// Bar-open prices, oldest to newest.
    #[serde(deserialize_with = "primary_samples::deserialize")]
    pub open: Vec<f64>,
    /// Bar-high prices aligned one-to-one with [`MarketData::open`].
    #[serde(deserialize_with = "primary_samples::deserialize")]
    pub high: Vec<f64>,
    /// Bar-low prices aligned one-to-one with [`MarketData::open`].
    #[serde(deserialize_with = "primary_samples::deserialize")]
    pub low: Vec<f64>,
    /// Bar-close prices aligned one-to-one with [`MarketData::open`].
    #[serde(deserialize_with = "primary_samples::deserialize")]
    pub close: Vec<f64>,
    /// Bar volumes aligned one-to-one with [`MarketData::open`].
    #[serde(deserialize_with = "primary_samples::deserialize")]
    pub volume: Vec<f64>,
    /// Finite positive minimum price increment for the instrument.
    #[serde(default = "default_mintick")]
    pub mintick: f64,
    /// Bar OPEN times, epoch milliseconds. Empty means "synthesize" (see
    /// [`MarketData::time_at`]); a non-empty vector must have one entry per
    /// bar and be strictly increasing.
    #[serde(default)]
    pub time: Vec<i64>,
    /// Optional host-supplied chart-bar closing instants, epoch milliseconds.
    /// A supplied lane must be bar-aligned, later than each bar's open, and
    /// no later than the next supplied open. Empty uses nominal duration.
    #[serde(default, rename = "timeClose")]
    pub time_close: Vec<i64>,
    /// Same-symbol requested-timeframe closing instants, repeated on each
    /// chart bar belonging to that requested bar. Keys are Pine timeframes
    /// for time-based bars (for example `"1W"`); equal-duration calendar and
    /// fixed units remain distinct. Requires `timeClose`. Each lane must cover the complete
    /// chart feed. It supplies the calendar clock, never indicator values.
    /// Realtime appends require rebuilding a complete snapshot.
    #[serde(default, rename = "requestTimeClose")]
    pub request_time_close: std::collections::HashMap<String, Vec<i64>>,
    /// Optional complete OHLCV feeds for same-symbol higher-timeframe
    /// requests. Unlike `requestTimeClose`, these carry the requested bars'
    /// actual values, so `request.security` history can remain correct when
    /// chart bars are sparse or omit entire requested periods.
    #[serde(default, rename = "requestBars")]
    pub request_bars: std::collections::HashMap<String, RequestTimeframeBars>,
    /// Host-supplied trading-day identity for each bar, expressed as the
    /// epoch-millisecond start of that symbol trading day. Empty uses the
    /// deterministic exchange-timezone calendar fallback. A supplied lane
    /// must be bar-aligned and nondecreasing; equal adjacent values allow an
    /// overnight session to cross local midnight without opening a new day.
    #[serde(default, rename = "tradingDay")]
    pub trading_day: Vec<i64>,
    /// Chart timeframe in Pine notation, such as `"1D"`.
    #[serde(default = "default_timeframe")]
    pub timeframe: String,
    /// Symbol, exchange, currency, and timezone metadata for the chart feed.
    #[serde(default)]
    pub syminfo: SymInfo,
    /// Optional per-run host tables backing the scalar `request.*` interfaces
    /// (`request.dividends/splits/earnings/financial/economic/
    /// currency_rate`). Keyed by the request's compile-time selector (e.g.
    /// `"financial/AAPL/TOTAL_REVENUE/FQ"`, `"currency_rate/EUR/USD"`); the
    /// value is a per-bar float series. A missing key (the default: an empty
    /// map) makes the interface return `na` every bar, so the language surface
    /// is complete without the VM needing a second data feed. A present series
    /// shorter than the bar count reads `na` on the uncovered bars.
    #[serde(default, rename = "requestData")]
    pub request_data: std::collections::HashMap<String, Vec<f64>>,
    /// Optional per-run host table backing the order-flow `footprint` interface
    /// (`request.footprint()` and the `footprint.*`/`volume_row.*`
    /// accessors). Keyed by the request's compile-time selector; the value is a
    /// per-bar series of optional [`HostFootprint`] samples. A missing key (the
    /// default: an empty map), or a `null`/absent bar entry, makes
    /// `request.footprint()` build an object whose scalar fields are all `na`,
    /// whose imbalance flags are `false`, and whose `rows` array is empty — so
    /// the language surface is complete without a real order-flow feed. Data
    /// sourcing stays a host concern.
    #[serde(default, rename = "footprintData")]
    pub footprint_data: std::collections::HashMap<String, Vec<Option<HostFootprint>>>,
    /// Optional per-bar host series backing the realtime-only `bid` builtin.
    /// `bid`/`ask` are available only on the `"1T"` tick timeframe;
    /// this headless VM has no tick feed, so it reads this optional series (`na`
    /// by default, or on any bar the series does not cover). Data sourcing is a
    /// host concern — no real quote feed is involved.
    #[serde(default)]
    pub bid: Vec<f64>,
    /// Optional per-bar host series backing the realtime-only `ask` builtin.
    /// Like [`MarketData::bid`]: `na` by default.
    #[serde(default)]
    pub ask: Vec<f64>,
    /// Optional per-run host tables backing the intrabar `request.security_
    /// lower_tf(sym, tf, expr)` interface. Keyed by the request's
    /// compile-time selector; the value is a per-bar array of the intrabar
    /// values within that chart bar. A missing key (the default) or an
    /// uncovered bar yields an EMPTY array — this VM only aggregates UP, so
    /// lower-timeframe intrabar decomposition is a host concern.
    #[serde(default, rename = "securityLowerTfData")]
    pub security_lower_tf_data: std::collections::HashMap<String, Vec<Vec<f64>>>,
    /// Optional host value backing `chart.left_visible_bar_time` (the open time,
    /// epoch ms, of the leftmost visible bar). The headless VM processes the
    /// whole feed and has no viewport, so this is `None` (→ `na`) by default;
    /// a host with a real viewport can supply it.
    #[serde(default, rename = "chartLeftVisibleBarTime")]
    pub chart_left_visible_bar_time: Option<i64>,
    /// Optional host value backing `chart.right_visible_bar_time`. `None`
    /// (→ `na`) by default (no viewport).
    #[serde(default, rename = "chartRightVisibleBarTime")]
    pub chart_right_visible_bar_time: Option<i64>,
    /// Optional host-supplied upcoming corporate-events feed backing the
    /// `dividends.future_*` / `earnings.future_*` built-in series. Every field
    /// is `None` (→ `na`) by default; the headless VM has no live corporate-
    /// actions feed, so data sourcing is a host concern.
    #[serde(default, rename = "upcomingEvents")]
    pub upcoming_events: UpcomingEvents,
}

/// Host-supplied values for the next scheduled dividend and earnings report,
/// backing the `dividends.future_*` / `earnings.future_*` built-in series
/// (see [`MarketData::upcoming_events`]). These are the upcoming-event series
/// variables, NOT the `request.dividends()`/`request.earnings()` field
/// selectors (`dividends.gross/net`, `earnings.actual/estimate/standardized`).
/// Every field defaults to `None` (→ `na`); amounts / EPS / revenue are
/// `Option<f64>`, dates / times (epoch ms) are `Option<i64>`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UpcomingEvents {
    /// `dividends.future_amount` — the per-share amount of the upcoming
    /// dividend. `None` → `na`.
    #[serde(default)]
    pub dividends_future_amount: Option<f64>,
    /// `dividends.future_ex_date` — the ex-dividend date (epoch ms) of the
    /// upcoming dividend. `None` → `na`.
    #[serde(default)]
    pub dividends_future_ex_date: Option<i64>,
    /// `dividends.future_pay_date` — the payment date (epoch ms) of the
    /// upcoming dividend. `None` → `na`.
    #[serde(default)]
    pub dividends_future_pay_date: Option<i64>,
    /// `earnings.future_eps` — host-supplied estimated EPS for the next earnings
    /// report. `None` → `na`.
    #[serde(default)]
    pub earnings_future_eps: Option<f64>,
    /// `earnings.future_period_end_time` — the end time (epoch ms) of the
    /// fiscal period for the upcoming earnings report. `None` → `na`.
    #[serde(default)]
    pub earnings_future_period_end_time: Option<i64>,
    /// `earnings.future_revenue` — the estimated revenue of the upcoming
    /// earnings report. `None` → `na`.
    #[serde(default)]
    pub earnings_future_revenue: Option<f64>,
    /// `earnings.future_time` — the announcement time (epoch ms) of the
    /// upcoming earnings report. `None` → `na`.
    #[serde(default)]
    pub earnings_future_time: Option<i64>,
}

/// One bar's host-supplied order-flow footprint (see [`MarketData::footprint_data`]).
/// Every scalar defaults to `na` and the `rows` list to empty, so a partially
/// specified sample is well-formed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostFootprint {
    /// Aggressor-buy volume for the complete bar.
    #[serde(default = "nan")]
    pub buy_volume: f64,
    /// Aggressor-sell volume for the complete bar.
    #[serde(default = "nan")]
    pub sell_volume: f64,
    /// Total traded volume for the complete bar.
    #[serde(default = "nan")]
    pub total_volume: f64,
    /// Signed buy-minus-sell volume for the complete bar.
    #[serde(default = "nan")]
    pub delta: f64,
    /// Price at the point of control, where the most volume traded.
    #[serde(default = "nan")]
    pub poc: f64,
    /// Upper boundary of the host-computed value area.
    #[serde(default = "nan")]
    pub vah: f64,
    /// Lower boundary of the host-computed value area.
    #[serde(default = "nan")]
    pub val: f64,
    /// The bar's volume rows, lowest row first. Empty by default.
    #[serde(default)]
    pub rows: Vec<HostVolumeRow>,
}

impl Default for HostFootprint {
    /// Every scalar defaults to `na` (not `0.0`), matching the serde field
    /// defaults so an unspecified field is consistently `na` however the sample
    /// is built.
    fn default() -> Self {
        HostFootprint {
            buy_volume: f64::NAN,
            sell_volume: f64::NAN,
            total_volume: f64::NAN,
            delta: f64::NAN,
            poc: f64::NAN,
            vah: f64::NAN,
            val: f64::NAN,
            rows: Vec::new(),
        }
    }
}

/// One host-supplied footprint volume row (see [`HostFootprint::rows`]). Scalars
/// default to `na`, imbalance flags to `false`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostVolumeRow {
    /// Aggressor-buy volume assigned to this price row.
    #[serde(default = "nan")]
    pub buy_volume: f64,
    /// Aggressor-sell volume assigned to this price row.
    #[serde(default = "nan")]
    pub sell_volume: f64,
    /// Total traded volume assigned to this price row.
    #[serde(default = "nan")]
    pub total_volume: f64,
    /// Signed buy-minus-sell volume assigned to this price row.
    #[serde(default = "nan")]
    pub delta: f64,
    /// Upper price boundary of this row.
    #[serde(default = "nan")]
    pub up_price: f64,
    /// Lower price boundary of this row.
    #[serde(default = "nan")]
    pub down_price: f64,
    /// Whether the host classified this row as a buy imbalance.
    #[serde(default)]
    pub has_buy_imbalance: bool,
    /// Whether the host classified this row as a sell imbalance.
    #[serde(default)]
    pub has_sell_imbalance: bool,
}

impl Default for HostVolumeRow {
    /// Scalars default to `na`, imbalance flags to `false` (matching the serde
    /// field defaults).
    fn default() -> Self {
        HostVolumeRow {
            buy_volume: f64::NAN,
            sell_volume: f64::NAN,
            total_volume: f64::NAN,
            delta: f64::NAN,
            up_price: f64::NAN,
            down_price: f64::NAN,
            has_buy_imbalance: false,
            has_sell_imbalance: false,
        }
    }
}

fn nan() -> f64 {
    f64::NAN
}

fn true_value() -> bool {
    true
}

fn default_mintick() -> f64 {
    0.01
}

fn default_timeframe() -> String {
    "1D".to_string()
}

/// Symbol metadata, one struct field per modeled `syminfo.*` property.
/// Browser hosts may send a partial object;
/// omitted fields use the `Default` implementation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SymInfo {
    /// `syminfo.ticker` — the symbol name without the exchange prefix.
    pub ticker: String,
    /// `syminfo.tickerid` — the host's fully qualified symbol identifier.
    /// When omitted, the runtime derives it from `main_tickerid` or
    /// `prefix:ticker`.
    #[serde(default)]
    pub tickerid: String,
    /// `syminfo.currency` — the symbol's quote currency (e.g. "USD").
    pub currency: String,
    /// `syminfo.type` — the market type (e.g. "stock", "crypto").
    #[serde(rename = "type")]
    pub kind: String,
    /// `syminfo.timezone` — the exchange timezone. An IANA name or a
    /// fixed-offset string (`"UTC"`, `"UTC-5"`, `"GMT+0530"`); calendar
    /// series parse the fixed-offset form (full IANA is not implemented).
    pub timezone: String,
    // ---- the remainder of the string `syminfo.*` surface. Each is a
    // per-run constant backing the same-named builtin; a missing host value
    // defaults to the empty string (the default for a symbol
    // without that metadata). ----
    /// `syminfo.basecurrency` — the symbol's base currency (default "").
    #[serde(default)]
    pub basecurrency: String,
    /// `syminfo.description` — the symbol's description (default "").
    #[serde(default)]
    pub description: String,
    /// `syminfo.country` — the symbol's country code (default "").
    #[serde(default)]
    pub country: String,
    /// `syminfo.industry` — the symbol's industry (default "").
    #[serde(default)]
    pub industry: String,
    /// `syminfo.sector` — the symbol's sector (default "").
    #[serde(default)]
    pub sector: String,
    /// `syminfo.root` — the root symbol for derivatives (default "").
    #[serde(default)]
    pub root: String,
    /// `syminfo.prefix` — the exchange prefix (default "").
    #[serde(default)]
    pub prefix: String,
    /// `syminfo.session` — the session specification (default "").
    #[serde(default)]
    pub session: String,
    /// The hours the symbol trades, as a session specification
    /// read on the exchange's clock: `"24x7"`, `"0930-1600"`,
    /// `"1700-1600:1234567"` (days 1 = Sunday to 7). Grouping bars into days
    /// and higher timeframes depends on where a session's day starts, so
    /// such a grouping is computed only for a session whose day start is
    /// unambiguous (see "Exchange time zones and sessions" in
    /// `docs/runtime.md`). Empty (the default) declares none.
    #[serde(default)]
    pub session_hours: String,
    /// `syminfo.volumetype` — the volume type (default "").
    #[serde(default)]
    pub volumetype: String,
    /// `syminfo.main_tickerid` — the main ticker identifier (default "").
    #[serde(default)]
    pub main_tickerid: String,
    /// ISIN supplied by the symbol provider; empty when unavailable.
    #[serde(default)]
    pub isin: String,
    /// `syminfo.current_contract` — the contract a continuous futures symbol
    /// currently follows; empty when unavailable, which reads as `na`.
    #[serde(default)]
    pub current_contract: String,
    // ---- the numeric `syminfo.*` surface. `mintick` lives on
    // `MarketData` (it predates this struct); these are the rest. ----
    /// `syminfo.minmove` — the minimum-move numerator (default 1).
    #[serde(default = "default_minmove")]
    pub minmove: i64,
    /// `syminfo.pricescale` — 10^decimals (default 100, matching the 0.01
    /// `mintick` default: `mintick = minmove / pricescale`).
    #[serde(default = "default_pricescale")]
    pub pricescale: i64,
    /// `syminfo.pointvalue` — the point value of one contract (default 1.0).
    #[serde(default = "default_pointvalue")]
    pub pointvalue: f64,
    /// `syminfo.mincontract` — the minimum tradable quantity (default 1.0).
    #[serde(default = "default_mincontract")]
    pub mincontract: f64,
    /// `syminfo.expiration_date` — a host-provided expiration timestamp (epoch ms)
    /// for a non-continuous futures contract, or
    /// `None` (→ `na`) when unavailable. The host is responsible for symbol
    /// applicability. Host-or-default.
    #[serde(default, rename = "expirationDate")]
    pub expiration_date: Option<i64>,
    // ---- SYMINFO fundamentals (host-or-default, `na` by default). Company
    // fundamental / analyst data the headless VM reads from these OPTIONAL
    // host fields; there is no fundamentals feed. Names
    // follow the `syminfo.*` field names. Integer counts / epoch-ms dates are
    // `Option<i64>`; share counts and price targets are `Option<f64>`. ----
    /// `syminfo.employees` — number of employees. `None` → `na`.
    #[serde(default)]
    pub employees: Option<i64>,
    /// `syminfo.shareholders` — number of shareholders. `None` → `na`.
    #[serde(default)]
    pub shareholders: Option<i64>,
    /// `syminfo.shares_outstanding_float` — shares outstanding excluding
    /// restricted shares. `None` → `na`.
    #[serde(default)]
    pub shares_outstanding_float: Option<f64>,
    /// `syminfo.shares_outstanding_total` — shares outstanding including
    /// restricted shares. `None` → `na`.
    #[serde(default)]
    pub shares_outstanding_total: Option<i64>,
    /// `syminfo.recommendations_buy` — analysts with a "Buy" rating.
    /// `None` → `na`.
    #[serde(default)]
    pub recommendations_buy: Option<i64>,
    /// `syminfo.recommendations_buy_strong` — analysts with a "Strong Buy"
    /// rating. `None` → `na`.
    #[serde(default)]
    pub recommendations_buy_strong: Option<i64>,
    /// `syminfo.recommendations_date` — start date (epoch ms) of the last set
    /// of recommendations. `None` → `na`.
    #[serde(default)]
    pub recommendations_date: Option<i64>,
    /// `syminfo.recommendations_hold` — analysts with a "Hold" rating.
    /// `None` → `na`.
    #[serde(default)]
    pub recommendations_hold: Option<i64>,
    /// `syminfo.recommendations_sell` — analysts with a "Sell" rating.
    /// `None` → `na`.
    #[serde(default)]
    pub recommendations_sell: Option<i64>,
    /// `syminfo.recommendations_sell_strong` — analysts with a "Strong Sell"
    /// rating. `None` → `na`.
    #[serde(default)]
    pub recommendations_sell_strong: Option<i64>,
    /// `syminfo.recommendations_total` — total number of recommendations.
    /// `None` → `na`.
    #[serde(default)]
    pub recommendations_total: Option<i64>,
    /// `syminfo.target_price_average` — average of the last yearly price
    /// targets. `None` → `na`.
    #[serde(default)]
    pub target_price_average: Option<f64>,
    /// `syminfo.target_price_date` — start date (epoch ms) of the last price
    /// target prediction. `None` → `na`.
    #[serde(default)]
    pub target_price_date: Option<i64>,
    /// `syminfo.target_price_estimates` — number of price target predictions.
    /// `None` → `na`.
    #[serde(default)]
    pub target_price_estimates: Option<f64>,
    /// `syminfo.target_price_high` — last highest yearly price target.
    /// `None` → `na`.
    #[serde(default)]
    pub target_price_high: Option<f64>,
    /// `syminfo.target_price_low` — last lowest yearly price target.
    /// `None` → `na`.
    #[serde(default)]
    pub target_price_low: Option<f64>,
    /// `syminfo.target_price_median` — median of the last yearly price
    /// targets. `None` → `na`.
    #[serde(default)]
    pub target_price_median: Option<f64>,
}

fn default_minmove() -> i64 {
    1
}

fn default_pricescale() -> i64 {
    100
}

fn default_pointvalue() -> f64 {
    1.0
}

fn default_mincontract() -> f64 {
    1.0
}

impl Default for SymInfo {
    fn default() -> Self {
        SymInfo {
            ticker: String::new(),
            tickerid: String::new(),
            currency: String::new(),
            kind: String::new(),
            timezone: "UTC".to_string(),
            basecurrency: String::new(),
            description: String::new(),
            country: String::new(),
            industry: String::new(),
            sector: String::new(),
            root: String::new(),
            prefix: String::new(),
            session: String::new(),
            session_hours: String::new(),
            volumetype: String::new(),
            main_tickerid: String::new(),
            isin: String::new(),
            current_contract: String::new(),
            minmove: default_minmove(),
            pricescale: default_pricescale(),
            pointvalue: default_pointvalue(),
            mincontract: default_mincontract(),
            expiration_date: None,
            employees: None,
            shareholders: None,
            shares_outstanding_float: None,
            shares_outstanding_total: None,
            recommendations_buy: None,
            recommendations_buy_strong: None,
            recommendations_date: None,
            recommendations_hold: None,
            recommendations_sell: None,
            recommendations_sell_strong: None,
            recommendations_total: None,
            target_price_average: None,
            target_price_date: None,
            target_price_estimates: None,
            target_price_high: None,
            target_price_low: None,
            target_price_median: None,
        }
    }
}

#[cfg(test)]
mod chart_request_tests;

/// The `{data, runOptions, bars}` request a linked chart module's `chartInit`
/// reads, as JSON. Two things need care that a plain `JSON.stringify` loses:
/// a `Map`-backed option such as `inputs` or `externalSources` must become an
/// object, which the caller's `Map` cannot do by itself, and JSON has no
/// spelling for an infinite primary sample, so `"Infinity"` and
/// `"-Infinity"` stand in for it. A NaN sample is written as `null`. The
/// auxiliary lanes (`requestData`, `requestBars`, `securityLowerTfData`, `bid`,
/// `ask`, and the footprint fields) take
/// no token, so a non-finite value there makes the loader decline the run
/// with a reason; the caller then runs the interpreter.
pub fn chart_request_json(data: &MarketData, options: &RunOptions, bars: usize) -> String {
    let mut object = serde_json::Map::new();
    object.insert(
        "data".to_string(),
        serde_json::to_value(data).unwrap_or_default(),
    );
    object.insert(
        "runOptions".to_string(),
        serde_json::to_value(options).unwrap_or_default(),
    );
    object.insert("bars".to_string(), serde_json::Value::from(bars));
    let mut value = serde_json::Value::Object(object);
    // `Some` of a non-finite sample is written as `null`, which reads back as
    // `None`: a different lane. A string the loader refuses declines the run
    // instead, so the interpreter runs it.
    for (id, lane) in &options.external_sources {
        for (index, sample) in lane.values.iter().enumerate() {
            if matches!(sample, Some(number) if !number.is_finite()) {
                let slot = value
                    .get_mut("runOptions")
                    .and_then(|v| v.get_mut("externalSources"))
                    .and_then(|v| v.get_mut(id.to_string()))
                    .and_then(|v| v.get_mut("values"))
                    .and_then(|v| v.get_mut(index));
                if let Some(slot) = slot {
                    *slot = serde_json::Value::String("non-finite".into());
                }
            }
        }
    }
    let lanes = [
        ("open", &data.open),
        ("high", &data.high),
        ("low", &data.low),
        ("close", &data.close),
        ("volume", &data.volume),
    ];
    for (name, lane) in lanes {
        if let Some(samples) = value["data"][name].as_array_mut() {
            for (sample, &number) in samples.iter_mut().zip(lane) {
                if number == f64::INFINITY {
                    *sample = "Infinity".into();
                } else if number == f64::NEG_INFINITY {
                    *sample = "-Infinity".into();
                }
            }
        }
    }
    value.to_string()
}

impl Default for MarketData {
    fn default() -> Self {
        MarketData {
            open: Vec::new(),
            high: Vec::new(),
            low: Vec::new(),
            close: Vec::new(),
            volume: Vec::new(),
            mintick: default_mintick(),
            time: Vec::new(),
            time_close: Vec::new(),
            request_time_close: std::collections::HashMap::new(),
            request_bars: std::collections::HashMap::new(),
            trading_day: Vec::new(),
            timeframe: default_timeframe(),
            syminfo: SymInfo::default(),
            request_data: std::collections::HashMap::new(),
            footprint_data: std::collections::HashMap::new(),
            bid: Vec::new(),
            ask: Vec::new(),
            security_lower_tf_data: std::collections::HashMap::new(),
            chart_left_visible_bar_time: None,
            chart_right_visible_bar_time: None,
            upcoming_events: UpcomingEvents::default(),
        }
    }
}

/// Host supplied OHLCV bars for one requested timeframe. Arrays are ordered
/// oldest first and must cover the full chart time range.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RequestTimeframeBars {
    /// Requested-bar open instants in epoch milliseconds.
    #[serde(default)]
    pub time: Vec<i64>,
    /// Requested-bar close instants in epoch milliseconds.
    #[serde(default, rename = "timeClose")]
    pub time_close: Vec<i64>,
    /// Requested-bar opening prices.
    #[serde(default)]
    pub open: Vec<f64>,
    /// Requested-bar high prices.
    #[serde(default)]
    pub high: Vec<f64>,
    /// Requested-bar low prices.
    #[serde(default)]
    pub low: Vec<f64>,
    /// Requested-bar closing prices.
    #[serde(default)]
    pub close: Vec<f64>,
    /// Requested-bar volumes.
    #[serde(default)]
    pub volume: Vec<f64>,
}

/// First bar's synthesized open time: 2020-01-01T00:00:00Z in epoch ms.
/// `(2020-01-01 - 1970-01-01) = 18262 days`.
const SYNTHETIC_EPOCH_MS: i64 = 18_262 * MS_PER_DAY;
/// One calendar day in milliseconds (the synthetic step).
const MS_PER_DAY: i64 = 86_400_000;

impl MarketData {
    /// Number of chart bars, defined by the close-price lane.
    pub fn bars(&self) -> usize {
        self.close.len()
    }

    /// Validate the host-supplied lanes and metadata before execution.
    ///
    /// The interpreter and incremental sessions call this automatically.
    /// Linked runtimes can call the same check before constructing VM state,
    /// so malformed auxiliary request lanes are rejected consistently.
    pub fn validate(&self) -> Result<(), RuntimeError> {
        let malformed = |message: String| RuntimeError {
            kind: RuntimeErrorKind::MalformedMarketData,
            message,
            bar_index: None,
            instruction_offset: None,
            span: None,
        };
        let n = self.close.len();
        let all_equal = self.open.len() == n
            && self.high.len() == n
            && self.low.len() == n
            && self.volume.len() == n;
        if !all_equal {
            return Err(malformed(format!(
                "market data arrays must have equal lengths (open {}, high {}, low {}, close {}, volume {})",
                self.open.len(),
                self.high.len(),
                self.low.len(),
                self.close.len(),
                self.volume.len()
            )));
        }
        if !(self.mintick.is_finite() && self.mintick > 0.0) {
            return Err(malformed(format!(
                "mintick must be a finite positive number, found {}",
                self.mintick
            )));
        }
        if session::session_shape(&self.syminfo.session_hours).is_none() {
            return Err(malformed(format!(
                "syminfo.session_hours `{}` is not a session specification such as `24x7` or `0930-1600:23456`",
                self.syminfo.session_hours
            )));
        }
        if resolve_timezone(&self.syminfo.timezone).is_none() {
            return Err(malformed(format!(
                "syminfo.timezone `{}` is not a timezone PineWASM knows: give an IANA name such as `America/New_York` (IANA release {}) or a fixed offset such as `UTC-5`",
                self.syminfo.timezone,
                pinewasm_tz::TZDATA_VERSION
            )));
        }
        if !(self.syminfo.pointvalue.is_finite() && self.syminfo.pointvalue > 0.0) {
            return Err(malformed(format!(
                "syminfo.pointvalue must be a finite positive number, found {}",
                self.syminfo.pointvalue
            )));
        }
        if !(self.syminfo.mincontract.is_finite() && self.syminfo.mincontract > 0.0) {
            return Err(malformed(format!(
                "syminfo.mincontract must be a finite positive number, found {}",
                self.syminfo.mincontract
            )));
        }
        let close_offset_ms = timeframe_in_seconds(&self.timeframe)
            .checked_mul(1000)
            .ok_or_else(|| {
                malformed(format!(
                    "timeframe close time exceeds the i64 timestamp range: timeframe `{}` has no representable millisecond duration",
                    self.timeframe
                ))
            })?;
        // A supplied time axis must match the bar count and be strictly
        // increasing (bar open times are monotonic). An empty vector opts
        // into synthesis and is always valid.
        if !self.time.is_empty() {
            if self.time.len() != n {
                return Err(malformed(format!(
                    "time array length ({}) must equal the bar count ({n})",
                    self.time.len()
                )));
            }
            for w in self.time.windows(2) {
                if w[1] <= w[0] {
                    return Err(malformed(format!(
                        "time array must be strictly increasing, found {} then {}",
                        w[0], w[1]
                    )));
                }
            }
            if let Some((index, open)) = self.time.iter().copied().enumerate().find(|(_, open)| {
                self.time_close.is_empty() && open.checked_add(close_offset_ms).is_none()
            }) {
                return Err(malformed(format!(
                    "timeframe close time exceeds the i64 timestamp range at bar {index}: open {open}, timeframe `{}`",
                    self.timeframe
                )));
            }
        } else if n > 0 {
            let last_index = i64::try_from(n - 1).ok();
            let last_open = last_index
                .and_then(|index| index.checked_mul(MS_PER_DAY))
                .and_then(|offset| SYNTHETIC_EPOCH_MS.checked_add(offset));
            if last_open
                .and_then(|open| open.checked_add(close_offset_ms))
                .is_none()
            {
                return Err(malformed(format!(
                    "timeframe close time exceeds the i64 timestamp range at synthesized bar {} for timeframe `{}`",
                    n - 1,
                    self.timeframe
                )));
            }
        }
        if !self.trading_day.is_empty() {
            if self.trading_day.len() != n {
                return Err(malformed(format!(
                    "tradingDay array length ({}) must equal the bar count ({n})",
                    self.trading_day.len()
                )));
            }
            for window in self.trading_day.windows(2) {
                if window[1] < window[0] {
                    return Err(malformed(format!(
                        "tradingDay array must be nondecreasing, found {} then {}",
                        window[0], window[1]
                    )));
                }
            }
        }
        if !self.time_close.is_empty() {
            if self.time_close.len() != n {
                return Err(malformed(format!(
                    "timeClose array length ({}) must equal the bar count ({n})",
                    self.time_close.len()
                )));
            }
            for (i, &close) in self.time_close.iter().enumerate() {
                if close <= self.time_at(i) || (i + 1 < n && close > self.time_at(i + 1)) {
                    return Err(malformed(format!(
                        "timeClose must follow its open and not overlap the next bar at index {i}"
                    )));
                }
            }
        }
        let mut clock_keys = std::collections::HashSet::new();
        for (tf, lane) in &self.request_time_close {
            let Some(key) = resample::host_timeframe_key(tf) else {
                return Err(malformed(format!(
                    "invalid requestTimeClose timeframe `{tf}`"
                )));
            };
            if !clock_keys.insert(key) {
                return Err(malformed(format!(
                    "duplicate requestTimeClose timeframe identity `{tf}`"
                )));
            }
            if key.0 < timeframe_in_seconds(&self.timeframe) {
                return Err(malformed(format!(
                    "requestTimeClose timeframe `{tf}` is below the chart timeframe"
                )));
            }
            if self.time_close.len() != n || lane.len() != n {
                return Err(malformed(format!(
                    "requestTimeClose `{tf}` and timeClose must cover all {n} chart bars"
                )));
            }
            for (i, &close) in lane.iter().enumerate() {
                if close <= self.time_at(i)
                    || (i > 0 && close < lane[i - 1])
                    || (i > 0 && close != lane[i - 1] && lane[i - 1] > self.time_at(i))
                {
                    return Err(malformed(format!(
                        "requestTimeClose `{tf}` has an inconsistent closing instant at index {i}"
                    )));
                }
            }
        }
        let mut request_bar_keys = std::collections::HashSet::new();
        for (tf, feed) in &self.request_bars {
            let Some(key) = resample::host_timeframe_key(tf) else {
                return Err(malformed(format!("invalid requestBars timeframe `{tf}`")));
            };
            if !request_bar_keys.insert(key) {
                return Err(malformed(format!(
                    "duplicate requestBars timeframe identity `{tf}`"
                )));
            }
            if key.0 < timeframe_in_seconds(&self.timeframe) {
                return Err(malformed(format!(
                    "requestBars timeframe `{tf}` is below the chart timeframe"
                )));
            }
            let count = feed.time.len();
            if count == 0
                || [
                    feed.time_close.len(),
                    feed.open.len(),
                    feed.high.len(),
                    feed.low.len(),
                    feed.close.len(),
                    feed.volume.len(),
                ]
                .iter()
                .any(|&len| len != count)
            {
                return Err(malformed(format!(
                    "requestBars `{tf}` arrays must have one equal, nonzero length"
                )));
            }
            for index in 0..count {
                if feed.time_close[index] <= feed.time[index]
                    || (index > 0 && feed.time[index] <= feed.time[index - 1])
                    || (index > 0 && feed.time[index] < feed.time_close[index - 1])
                    || !feed.open[index].is_finite()
                    || !feed.high[index].is_finite()
                    || !feed.low[index].is_finite()
                    || !feed.close[index].is_finite()
                    || !feed.volume[index].is_finite()
                {
                    return Err(malformed(format!("requestBars `{tf}` has invalid timestamps or nonfinite OHLCV at index {index}")));
                }
            }
            if let Some((clock_tf, lane)) = self
                .request_time_close
                .iter()
                .find(|(clock_tf, _)| resample::host_timeframe_key(clock_tf) == Some(key))
            {
                for (chart_bar, &close) in lane.iter().enumerate() {
                    let bucket = feed
                        .time
                        .partition_point(|&open| open <= self.time_at(chart_bar));
                    if bucket == 0 || feed.time_close[bucket - 1] != close {
                        return Err(malformed(format!(
                            "requestTimeClose `{clock_tf}` conflicts with requestBars `{tf}` at chart index {chart_bar}"
                        )));
                    }
                }
            }
            if n > 0
                && (feed.time[0] > self.time_at(0)
                    || feed.time_close[count - 1] <= self.time_at(n - 1))
            {
                return Err(malformed(format!(
                    "requestBars `{tf}` must cover the complete chart time range"
                )));
            }
        }
        Ok(())
    }

    /// The open time (epoch ms) of bar `index`, from the supplied `time`
    /// array or the synthesized daily sequence when it is empty.
    ///
    /// # Panics
    ///
    /// Panics when an explicit `time` lane is present and `index` is out of
    /// bounds. Execution entry points validate lane lengths; callers using this
    /// inspection helper directly must keep `index < self.bars()`.
    pub fn time_at(&self, index: usize) -> i64 {
        vm_ops::time_at(&self.time, index)
    }

    /// The close time (epoch ms) of bar `index`, from `timeClose` when
    /// supplied, otherwise its open plus the nominal timeframe duration.
    /// Fallback arithmetic saturates for unvalidated input; execution rejects
    /// that overflow when no closing-time lane is supplied.
    ///
    /// # Panics
    ///
    /// Panics when a supplied `time` or `timeClose` lane is indexed outside
    /// its bounds. Execution validates these lengths before reading them.
    pub fn time_close_at(&self, index: usize) -> i64 {
        match vm_ops::time_close_lane(&self.time_close, index) {
            Some(close) => close,
            None => vm_ops::time_close_fallback(
                self.time_at(index),
                timeframe_in_seconds(&self.timeframe),
            ),
        }
    }

    /// The exchange timezone, `syminfo.timezone`. [`Self::validate`], which
    /// every run calls first, refuses a name that does not resolve, so the
    /// UTC returned for one here is never read.
    pub(crate) fn exchange_zone(&self) -> pinewasm_tz::TimeZone<'static> {
        resolve_timezone(&self.syminfo.timezone).unwrap_or(pinewasm_tz::TimeZone::Fixed(0))
    }

    /// The exchange timezone's offset in seconds east of UTC at the
    /// epoch-millisecond instant `time`.
    pub(crate) fn exchange_offset_at(&self, time: i64) -> i64 {
        session::offset_at_ms(&self.exchange_zone(), time)
    }

    /// Bar `i`'s open read on the exchange's wall clock: the open plus the
    /// exchange timezone's offset at it, in epoch milliseconds.
    pub(crate) fn local_time_at(&self, i: usize) -> i64 {
        let time = self.time_at(i);
        time.saturating_add(self.exchange_offset_at(time).saturating_mul(1000))
    }

    fn fallback_trading_day_for_time(&self, time: i64) -> i64 {
        // The fallback trading day is the exchange-local calendar date at 00:00 UTC. It
        // does not return the UTC instant at which local midnight occurred.
        // Only a host-supplied lane can identify the final trading date of an
        // overnight or otherwise non-calendar-day session exactly.
        vm_ops::fallback_trading_day(time, self.exchange_offset_at(time))
    }

    /// Trading-day identity for `index`. Hosts can supply the symbol/session
    /// calendar explicitly; otherwise the stable fallback is exchange-local
    /// calendar date at UTC midnight for the bar's open time.
    ///
    /// # Panics
    ///
    /// Panics when the fallback needs [`Self::time_at`] and `index` is outside
    /// a nonempty explicit `time` lane.
    pub fn trading_day_at(&self, index: usize) -> i64 {
        match vm_ops::trading_day_lane(&self.trading_day, index) {
            Some(day) => day,
            None => self.fallback_trading_day_for_time(self.time_at(index)),
        }
    }
}

fn strategy_risk_session_key(data: &MarketData, bar: usize) -> i64 {
    if timeframe_in_seconds(&data.timeframe) > 86_400 {
        return bar as i64;
    }
    data.trading_day_at(bar)
}

/// Append the broker's order-fill alerts to `alerts` and order the stream by
/// bar (stable, so a bar's script alerts stay ahead of its fills). Shared by
/// [`Vm::outcome`] and the AOT host's strategy drain.
fn merge_order_fill_alerts(alerts: &mut Vec<AlertEvent>, broker: &mut Broker) {
    alerts.extend(
        broker
            .take_order_fill_alerts()
            .into_iter()
            .map(|event| AlertEvent {
                bar: event.bar,
                message: event.message,
                freq: "strategy.order_fill".to_string(),
            }),
    );
    alerts.sort_by_key(|event| event.bar);
}

/// The wire strategy section of a finished run: `broker`'s results marked
/// at `final_close`. Shared by [`Vm::outcome`] and the AOT host's strategy
/// drain, so a native chart run reports exactly what the interpreter would.
fn strategy_outcome(initial_capital: f64, broker: Broker, final_close: f64) -> StrategyOutcome {
    let outcome = broker.into_outcome(final_close);
    let metrics = outcome.metrics;
    StrategyOutcome {
        initial_capital,
        trades: outcome.trades,
        first_retained_trade_index: Some(outcome.first_retained_trade_index),
        retained_trade_pnl_baseline: Some(outcome.retained_trade_pnl_baseline),
        first_entry_bar: outcome.first_entry_bar,
        first_entry_price: outcome.first_entry_price,
        first_entry_account_notional_per_unit: outcome.first_entry_account_notional_per_unit,
        account_currency: outcome.account_currency,
        symbol_currency: outcome.symbol_currency,
        currency_rates: outcome.currency_rates,
        currency_conversion_complete: outcome.currency_conversion_complete,
        position: outcome.position,
        equity: outcome.equity,
        max_contracts_held: outcome.max_contracts_held,
        max_contracts_held_long: outcome.max_contracts_held_long,
        max_contracts_held_short: outcome.max_contracts_held_short,
        margin_used: outcome.margin_used,
        maximum_margin_used: outcome.maximum_margin_used,
        required_account_size: outcome.required_account_size,
        report_aggregates: Some(outcome.report_aggregates),
        margin_calls: outcome.margin_calls,
        total_liquidated: outcome.total_liquidated,
        largest_liquidation: outcome.largest_liquidation,
        net_profit: metrics.net_profit,
        open_profit: metrics.open_profit,
        gross_profit: metrics.gross_profit,
        gross_loss: metrics.gross_loss,
        closed_trades: metrics.closed_trades,
        win_trades: metrics.win_trades,
        loss_trades: metrics.loss_trades,
        even_trades: metrics.even_trades,
        avg_trade: metrics.avg_trade,
        max_drawdown: metrics.max_drawdown,
        max_drawdown_percent: Some(outcome.max_drawdown_percent),
        open_entry_commission: Some(outcome.open_entry_commission),
        max_runup: metrics.max_runup,
    }
}

/// A single realtime price update on the last bar. Ticks replay
/// bar-by-bar realtime execution: the driver overwrites the target bar's
/// OHLCV with this snapshot, restores all non-`varip` state to the bar's
/// start, runs `main` once, and — on an `is_bar_close` tick — commits the
/// result as permanent history before the next tick opens a new bar.
///
/// `bar_index` must reference the last already-committed bar (re-executing
/// it) or the next index (appending a fresh realtime bar); other indices are
/// rejected with a structured error. See docs/runtime.md.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tick {
    /// Which bar this tick lands on: the last committed bar (re-run it) or the
    /// append index `bars` (open a new bar).
    pub bar_index: usize,
    /// Current realtime bar's open price.
    pub open: f64,
    /// Current realtime bar's high price.
    pub high: f64,
    /// Current realtime bar's low price.
    pub low: f64,
    /// Current realtime bar's latest price.
    pub close: f64,
    /// Current cumulative volume for the realtime bar.
    pub volume: f64,
    /// The bar's open time (epoch ms). Used when the tick appends a new bar.
    pub time: i64,
    /// Optional host-supplied trading-day identity for an appended bar.
    /// Existing-bar reruns retain their committed lane value. When absent on
    /// an append, the VM derives the day from this tick's `time` and the symbol
    /// timezone rather than carrying the previous day forward.
    #[serde(default)]
    pub trading_day: Option<i64>,
    /// True on the tick that closes the bar: the execution commits and the
    /// next tick starts a new bar. `barstate.isconfirmed` is true on exactly
    /// this execution.
    pub is_bar_close: bool,
}

/// The optional realtime segment appended after historical execution:
/// an ordered list of [`Tick`]s replayed on the last bar(s).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RealtimeSim {
    /// Ticks in the exact order the VM should replay them.
    pub ticks: Vec<Tick>,
}

/// One per-tick sample of a plot on the realtime bar, captured when
/// `RunOptions::realtime_trace` is set. `values[i]` is plot slot `i`'s value
/// after tick `tick_ordinal`'s execution (before rollback), so hosts can draw
/// the intrabar wiggle. Committed plot history still holds only bar-close
/// values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlotTickSample {
    /// 0-based index of the tick within the realtime sequence.
    pub tick_ordinal: u32,
    /// Bar index the tick executed on.
    pub bar_index: u32,
    /// Whether this tick closed its bar (committed).
    pub is_bar_close: bool,
    /// One value per plot slot, in slot order, after this tick's execution.
    pub values: Vec<Value>,
}

/// Conservative per-order reservation covering broker book entries, lots,
/// closed-trade/report rows, and order-fill alert metadata that one order
/// instruction can cause over its lifetime.
const STRATEGY_ORDER_RESERVATION_BYTES: usize = 4 * 1024;
/// Per historical bar reservation for broker equity/drawdown/session/report
/// samples that grow independently of explicit order instructions.
const STRATEGY_BAR_RESERVATION_BYTES: usize = 1024;

/// Per-run values for a host-selected plot from another script.
///
/// The lane is keyed by the downstream `InputDecl.id` in
/// [`RunOptions::external_sources`]. Its `source` must identify the same
/// script/plot slot as the corresponding [`InputValue::ExternalSource`]
/// override. `None` is Pine `na`; a short lane yields `na` after its final
/// sample. Invalid or missing lanes deterministically fall back to the input's
/// declared builtin source rather than aborting the downstream script.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalSourceLane {
    /// Durable identity of the upstream plot supplying this lane.
    pub source: ExternalSourceRef,
    /// Per-bar numeric values; `None` represents Pine `na`.
    pub values: Vec<Option<f64>>,
}

impl ExternalSourceLane {
    fn is_valid_for(&self, source: &ExternalSourceRef) -> bool {
        source.is_valid()
            && self.source.is_valid()
            && self.source.same_lane(source)
            && self.values.iter().flatten().all(|value| value.is_finite())
    }
}

/// Default per-bar instruction budget: see [`RunOptions::max_instructions_per_bar`].
///
/// This is a deterministic **work bound expressed in instructions**. The interpreter
/// retires hundreds of millions of CSIL instructions per second natively in
/// representative loop-heavy workloads, so this caps a single bar to a
/// bounded amount of work. A runaway `while true` still dies on its first bar,
/// which is the property the budget exists for.
///
/// It is deliberately not 1,000,000. A Pine script may loop over its own
/// history, so its per-bar work legitimately grows with the chart. Scripts with
/// long history-dependent loops exceed that ceiling, so a limit sized for short
/// scripts would stop work that is correct.
///
pub const DEFAULT_MAX_INSTRUCTIONS_PER_BAR: u64 = 64_000_000;

/// Default cumulative instruction budget for one historical + realtime run.
///
/// The per-bar budget stops one pathological bar; this independent ceiling
/// also bounds scripts whose individually-valid work repeats over a long feed.
/// One billion leaves substantial headroom for normal long-history scripts
/// while remaining a finite sandbox boundary.
pub const DEFAULT_MAX_INSTRUCTIONS_PER_RUN: u64 = 1_000_000_000;

/// Default cumulative runtime string-allocation budget for one run: 32 MiB.
pub const DEFAULT_MAX_STRING_BYTES_PER_RUN: usize = 32 * 1024 * 1024;

/// Default aggregate budget for predictable VM-owned state plus cumulative
/// dynamic table/event reservations: 256 MiB.
pub const DEFAULT_MAX_VM_BYTES_PER_RUN: usize = 256 * 1024 * 1024;

/// Default operand-stack depth limit for one execution.
pub const DEFAULT_MAX_STACK_DEPTH: usize = 4_096;

/// Resource budgets, host overrides, and optional execution modes for one VM
/// run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RunOptions {
    /// Budget per bar; exceeding it aborts the run with a structured error.
    /// Defaults to [`DEFAULT_MAX_INSTRUCTIONS_PER_BAR`]. An explicit value is
    /// always honored exactly, including `0`, so a host embedding untrusted
    /// scripts can impose a hard ceiling.
    pub max_instructions_per_bar: u64,
    /// Cumulative instruction budget across historical bars and realtime
    /// ticks. Exceeding it aborts with
    /// [`RuntimeErrorKind::RunInstructionLimitExceeded`]. An explicit `0`
    /// rejects the first instruction, independently of the per-bar limit.
    #[serde(default = "default_max_instructions_per_run")]
    pub max_instructions_per_run: u64,
    /// Cumulative bytes reserved for runtime-created scalar strings and regex
    /// pattern storage. Exceeding the limit aborts the run with a structured
    /// [`RuntimeErrorKind::StringLimitExceeded`] error. The counter never
    /// decreases: this bounds allocation churn as well as live memory.
    #[serde(default = "default_max_string_bytes_per_run")]
    pub max_string_bytes_per_run: usize,
    /// Deterministic logical bytes reserved for metered VM-owned slot rings,
    /// TA state, output lanes/outcome snapshots, stacks, profiling, cloned
    /// feed/program payloads, and cumulative dynamic table/event growth. The
    /// VM checks predictable state with overflow-safe arithmetic before its
    /// first large allocation. This is not a process-RSS or allocator-capacity
    /// ceiling. Realtime rollback snapshot allocations are outside this byte
    /// counter; other collection, string, object, and drawing limits constrain
    /// portions of their source state, but the duplicate can raise peak memory
    /// above this value.
    #[serde(default = "default_max_vm_bytes_per_run")]
    pub max_vm_bytes_per_run: usize,
    /// Operand stack depth limit. Defaults to [`DEFAULT_MAX_STACK_DEPTH`].
    pub max_stack_depth: usize,
    /// Optionally execute only the first N bars. A nonempty realtime segment
    /// requires the complete supplied history and rejects a truncating value.
    pub max_bars: Option<usize>,
    /// Optional realtime tick segment replayed after historical execution.
    /// `None` runs history only.
    #[serde(default)]
    pub realtime: Option<RealtimeSim>,
    /// When true, [`RunOutcome::realtime_trace`] collects a per-tick plot
    /// sample for the realtime segment. Off by default (committed plots are
    /// enough for most hosts).
    #[serde(default)]
    pub realtime_trace: bool,
    /// When true, the interpreter collects per-instruction profiling counters
    /// and returns them as [`RunOutcome::profile`]. Counters accumulate
    /// across every historical bar and every realtime tick. Off by default;
    /// the only cost when off is one predictable branch per dispatched
    /// instruction.
    #[serde(default)]
    pub profile: bool,
    /// Host overrides for `input.*` declarations, keyed by the input **id**
    /// (its index in `CsilProgram::inputs`). An override replaces the declared
    /// default for that input for the whole run; an id with no entry uses the
    /// declared default. An override whose value type does not match the
    /// declaration's kind is ignored (the default is used) rather than
    /// aborting the run — hosts should validate against the serialized
    /// metadata first (see docs/browser.md). The VM validates intrinsic value
    /// domains but does not reapply min/max/step/options to overrides. Keyed by
    /// id, not title, because titles need not be unique.
    #[serde(default)]
    pub inputs: HashMap<u32, InputValue>,
    /// Numeric lanes for host-only cross-script `input.source` selections,
    /// keyed by downstream input id. A lane is consumed only when `inputs[id]`
    /// is the matching [`InputValue::ExternalSource`]. Keeping identity and
    /// values separate prevents stale values from being applied after a user
    /// changes the selected upstream plot.
    #[serde(default)]
    pub external_sources: HashMap<u32, ExternalSourceLane>,
    /// Host overrides for the strategy declaration's broker properties.
    ///
    /// Broker properties a host may override. A
    /// missing field keeps the script's const-folded declaration value. The
    /// VM validates every supplied scalar before applying it, so malformed
    /// external hosts cannot inject NaN/negative account parameters.
    #[serde(default)]
    pub strategy: StrategyRunOverrides,
    /// Total live collection-element budget across the whole run (guards
    /// against heap-growth bombs — a loop that pushes forever). Exceeding it
    /// aborts the run with a structured [`RuntimeErrorKind::CollectionLimitExceeded`].
    #[serde(default = "default_max_collection_elements")]
    pub max_collection_elements: usize,
}

fn default_max_collection_elements() -> usize {
    pinewasm_mir::DEFAULT_MAX_COLLECTION_ELEMENTS
}

fn default_max_string_bytes_per_run() -> usize {
    DEFAULT_MAX_STRING_BYTES_PER_RUN
}

fn default_max_instructions_per_run() -> u64 {
    DEFAULT_MAX_INSTRUCTIONS_PER_RUN
}

fn default_max_vm_bytes_per_run() -> usize {
    DEFAULT_MAX_VM_BYTES_PER_RUN
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            max_instructions_per_bar: DEFAULT_MAX_INSTRUCTIONS_PER_BAR,
            max_instructions_per_run: default_max_instructions_per_run(),
            max_string_bytes_per_run: default_max_string_bytes_per_run(),
            max_vm_bytes_per_run: default_max_vm_bytes_per_run(),
            max_stack_depth: DEFAULT_MAX_STACK_DEPTH,
            max_bars: None,
            realtime: None,
            realtime_trace: false,
            profile: false,
            inputs: HashMap::new(),
            external_sources: HashMap::new(),
            strategy: StrategyRunOverrides::default(),
            max_collection_elements: default_max_collection_elements(),
        }
    }
}

/// Optional per-run broker-property overrides supplied by the host.
/// Field names intentionally mirror [`StrategyInfo`] while
/// every value is optional, preserving the declaration as the source of
/// truth when the host has not changed a property.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StrategyRunOverrides {
    /// Starting account equity.
    pub initial_capital: Option<f64>,
    /// Position-sizing mode token, such as `"fixed"` or `"percent_of_equity"`.
    pub default_qty_type: Option<String>,
    /// Default order quantity interpreted according to `default_qty_type`.
    pub default_qty_value: Option<f64>,
    /// Commission model token, such as `"percent"` or `"cash_per_order"`.
    pub commission_type: Option<String>,
    /// Commission rate or amount interpreted according to `commission_type`.
    pub commission_value: Option<f64>,
    /// Simulated adverse fill movement in minimum ticks.
    pub slippage: Option<u32>,
    /// Maximum entries allowed in the same direction.
    pub pyramiding: Option<u32>,
    /// Required price penetration, in ticks, for historical limit fills.
    pub backtest_fill_limits_assumption: Option<u32>,
    /// Whether orders may fill on the bar on which they are created.
    pub process_orders_on_close: Option<bool>,
    /// Rule token controlling which open entries a close command reduces.
    pub close_entries_rule: Option<String>,
    /// Whether the strategy recalculates on each realtime tick.
    pub calc_on_every_tick: Option<bool>,
    /// Whether a fill triggers an additional strategy calculation.
    pub calc_on_order_fills: Option<bool>,
    /// Whether historical bars replay their available intrabar ticks.
    pub calc_on_every_history_tick: Option<bool>,
    /// Strategy account currency token.
    pub currency: Option<String>,
    /// Required long-position margin percentage.
    pub margin_long: Option<f64>,
    /// Required short-position margin percentage.
    pub margin_short: Option<f64>,
    /// Whether historical fills consult lower-timeframe data when the host supplies it.
    pub use_bar_magnifier: Option<bool>,
    /// Whether fills on synthetic chart types use standard OHLC prices.
    pub fill_orders_on_standard_ohlc: Option<bool>,
    /// Host override for the `calc_bars_count` value taken from the declaration.
    pub calc_bars_count: Option<u32>,
}

impl StrategyRunOverrides {
    fn applied_to(&self, base: &StrategyInfo) -> StrategyInfo {
        let finite_positive = |value: Option<f64>, fallback: f64| {
            value
                .filter(|v| v.is_finite() && *v > 0.0)
                .unwrap_or(fallback)
        };
        let finite_non_negative = |value: Option<f64>, fallback: f64| {
            value
                .filter(|v| v.is_finite() && *v >= 0.0)
                .unwrap_or(fallback)
        };
        let enum_or = |value: &Option<String>, allowed: &[&str], fallback: &String| {
            value
                .as_ref()
                .filter(|candidate| allowed.contains(&candidate.as_str()))
                .cloned()
                .unwrap_or_else(|| fallback.clone())
        };

        StrategyInfo {
            title: base.title.clone(),
            shorttitle: base.shorttitle.clone(),
            overlay: base.overlay,
            format: base.format.clone(),
            precision: base.precision,
            scale: base.scale.clone(),
            max_bars_back: base.max_bars_back,
            initial_capital: finite_positive(self.initial_capital, base.initial_capital),
            default_qty_type: enum_or(
                &self.default_qty_type,
                &["fixed", "cash", "percent_of_equity"],
                &base.default_qty_type,
            ),
            default_qty_value: finite_positive(self.default_qty_value, base.default_qty_value),
            commission_type: enum_or(
                &self.commission_type,
                &["percent", "cash_per_contract", "cash_per_order"],
                &base.commission_type,
            ),
            commission_value: finite_non_negative(self.commission_value, base.commission_value),
            slippage: self.slippage.unwrap_or(base.slippage),
            pyramiding: self.pyramiding.unwrap_or(base.pyramiding),
            backtest_fill_limits_assumption: self
                .backtest_fill_limits_assumption
                .unwrap_or(base.backtest_fill_limits_assumption),
            process_orders_on_close: self
                .process_orders_on_close
                .unwrap_or(base.process_orders_on_close),
            close_entries_rule: enum_or(
                &self.close_entries_rule,
                &["FIFO", "ANY"],
                &base.close_entries_rule,
            ),
            calc_on_every_tick: self.calc_on_every_tick.unwrap_or(base.calc_on_every_tick),
            calc_on_order_fills: self.calc_on_order_fills.unwrap_or(base.calc_on_order_fills),
            calc_on_every_history_tick: self
                .calc_on_every_history_tick
                .unwrap_or(base.calc_on_every_history_tick),
            currency: self
                .currency
                .clone()
                .unwrap_or_else(|| base.currency.clone()),
            margin_long: finite_non_negative(self.margin_long, base.margin_long),
            margin_short: finite_non_negative(self.margin_short, base.margin_short),
            explicit_plot_zorder: base.explicit_plot_zorder,
            max_lines_count: base.max_lines_count,
            max_labels_count: base.max_labels_count,
            max_boxes_count: base.max_boxes_count,
            max_polylines_count: base.max_polylines_count,
            calc_bars_count: self.calc_bars_count.unwrap_or(base.calc_bars_count),
            // `risk_free_rate` is declaration-only. Unlike the
            // broker properties above, it has no host override.
            risk_free_rate: base.risk_free_rate,
            use_bar_magnifier: self.use_bar_magnifier.unwrap_or(base.use_bar_magnifier),
            fill_orders_on_standard_ohlc: self
                .fill_orders_on_standard_ohlc
                .unwrap_or(base.fill_orders_on_standard_ohlc),
            dynamic_requests: base.dynamic_requests,
            behind_chart: base.behind_chart,
            alert_message: base.alert_message.clone(),
            span: base.span,
        }
    }
}

// ---------------------------------------------------------------------------
// Outputs
// ---------------------------------------------------------------------------

/// One output series produced by a `plot(...)` call. `values` has one entry per
/// resident output bar, including an open forming realtime bar. Bars where the
/// plot statement did not execute (for example, because the other `if` branch
/// ran) hold `na`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlotSeries {
    /// Zero-based output slot from the compiled program.
    pub slot: u32,
    /// Optional title supplied by the script.
    pub title: Option<String>,
    /// Host rendering metadata for this output.
    #[serde(default)]
    pub style: DisplayStyle,
    /// Named per-bar color lanes used by the renderer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub colors: Vec<PlotColorSeries>,
    /// One runtime value for every resident output bar, including an open
    /// forming realtime bar that has not yet incremented `bars_executed`.
    pub values: Vec<Value>,
}

/// One named runtime color channel for a display output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlotColorSeries {
    /// Semantic use of this color lane.
    pub role: PlotColorRole,
    /// Per-bar colors, using [`Value::Na`] when no color applies.
    pub values: Vec<Value>,
    /// Selected entry in this output's compiler-owned color palette, one per
    /// resident bar. Null means unavailable/unregistered paint or a numeric
    /// gradient lane. Selection uses exact source identity, never wire RGBA.
    #[serde(default)]
    pub palette_indices: Vec<Option<u32>>,
}

impl PlotColorSeries {
    fn from_values(
        role: PlotColorRole,
        values: &[Value],
        palette: &[pinewasm_hir::StyleColorPaletteEntry],
    ) -> Self {
        use pinewasm_hir::StyleColorSource;
        Self {
            role,
            values: values.to_vec(),
            palette_indices: values
                .iter()
                .map(|value| {
                    let source = match value {
                        Value::Color {
                            input_origin: Some(input),
                            ..
                        } => StyleColorSource::Input { input: *input },
                        Value::Color {
                            r,
                            g,
                            b,
                            t,
                            input_origin: None,
                        } => StyleColorSource::Constant {
                            r: *r,
                            g: *g,
                            b: *b,
                            t: *t,
                        },
                        _ => return None,
                    };
                    palette
                        .iter()
                        .position(|entry| entry.role == role && entry.source == source)
                        .and_then(|index| u32::try_from(index).ok())
                })
                .collect(),
        }
    }
}

/// One color-only chart visual such as `bgcolor` or `barcolor`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VisualSeries {
    /// Zero-based visual slot from the compiled program.
    pub slot: u32,
    /// Optional title supplied by the script.
    pub title: Option<String>,
    /// Host rendering metadata for this visual.
    pub style: DisplayStyle,
    /// Named per-bar color lanes used by the renderer.
    pub colors: Vec<PlotColorSeries>,
}

/// One `fill(...)` band between two plots. `plot1` and `plot2` index the
/// corresponding lanes in [`RunOutcome::plots`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FillSeries {
    /// Zero-based fill slot from the compiled program.
    pub slot: u32,
    /// First plot slot forming the fill boundary.
    pub plot1: u32,
    /// Second plot slot forming the fill boundary.
    pub plot2: u32,
    /// Optional title supplied by the script.
    pub title: Option<String>,
    /// Host rendering metadata for this fill.
    pub style: DisplayStyle,
    /// Named per-bar color lanes used by the renderer.
    pub colors: Vec<PlotColorSeries>,
}

/// The serialized final state of one live drawing object, for hosts to
/// render. `kind` is the namespace (`"line"`/`"label"`/`"box"`); `id` is the
/// object's registration ordinal, counted from 0 across every drawing kind in
/// the order the script created them (the same in the interpreter and in a
/// compiled module, so usable to correlate across a re-run on either); and
/// `fields` maps each member name (`x1`, `color`, `text`, ...) to its final
/// value on the wire (numbers, `#RRGGBB[AA]` colors, strings, or `null` for
/// `na` and for a member that refers to another object). Documented in
/// docs/browser.md.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DrawingObject {
    /// Drawing namespace such as `"line"`, `"label"`, or `"table"`.
    pub kind: String,
    /// Run-local drawing id used to correlate updates for this object's
    /// lifetime: its registration ordinal, never a heap handle.
    pub id: u32,
    /// Member name → final value. Insertion order matches the kind's declared
    /// field (slot) order.
    pub fields: std::collections::BTreeMap<String, Value>,
    /// The table's populated cells, in `(column, row)` order.
    /// Empty (and omitted on the wire) for every non-table kind and for tables
    /// with no written cells. Each entry carries the cell's coordinates and its
    /// set attributes. Documented in docs/runtime.md.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cells: Vec<TableCellDto>,
    /// A polyline's vertices in array order, inlined from its
    /// `array<chart.point>` (raw handles serialize as `null` on the wire, so
    /// hosts could not otherwise reconstruct them). Empty (and omitted on the
    /// wire) for every non-polyline kind. Documented in docs/runtime.md.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub points: Vec<PolyPointDto>,
    /// The two lines referenced by a linefill. The references themselves are
    /// `null` in `fields`; this pair carries the lines' drawing ids so a chart
    /// host can resolve the fill against the serialized line objects. An entry
    /// is `None` when that line is `na` or no longer live.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "lineRefs")]
    pub line_refs: Option<[Option<u32>; 2]>,
}

/// One inlined polyline vertex: the referenced `chart.point`'s `index`,
/// `time`, and `price` values in the on-the-wire form (`null` for `na`).
/// Documented in docs/runtime.md.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolyPointDto {
    /// Bar index coordinate, or [`Value::Na`].
    pub index: Value,
    /// Epoch-millisecond time coordinate, or [`Value::Na`].
    pub time: Value,
    /// Price coordinate, or [`Value::Na`].
    pub price: Value,
}

/// One serialized table cell: its `(column, row)` position, the
/// `merged` flag, and the map of set attributes (`text`, `bgcolor`,
/// `text_color`, ...) to their final values. Only attributes that were actually
/// written appear. Documented in docs/runtime.md.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableCellDto {
    /// Zero-based table column.
    pub column: i64,
    /// Zero-based table row.
    pub row: i64,
    /// Whether this coordinate belongs to a merged cell region.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub merged: bool,
    /// Inclusive merged region [start column, start row, end column, end row].
    /// A flag alone cannot distinguish adjacent merged regions on the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge: Option<[i64; 4]>,
    /// Attribute name → final value (`str.format`-agnostic, on-the-wire form).
    pub attrs: std::collections::BTreeMap<String, Value>,
}

/// The `drawings` section of a [`RunOutcome`]: the final live drawing objects,
/// grouped and ordered per kind (creation order, oldest first). Empty groups
/// are omitted. Documented in docs/browser.md.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Drawings {
    /// Live line objects in creation order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<DrawingObject>,
    /// Live label objects in creation order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<DrawingObject>,
    /// Live box objects in creation order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub boxes: Vec<DrawingObject>,
    /// Live table objects in creation order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tables: Vec<DrawingObject>,
    /// Live polyline objects in creation order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub polylines: Vec<DrawingObject>,
    /// Live line-fill objects in creation order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub linefills: Vec<DrawingObject>,
}

impl Drawings {
    /// Whether no live drawing objects remain in any drawing collection.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
            && self.labels.is_empty()
            && self.boxes.is_empty()
            && self.tables.is_empty()
            && self.polylines.is_empty()
            && self.linefills.is_empty()
    }
}

/// One recorded `log.*` event, in creation order. Recorded events survive
/// realtime rollback, including messages from non-closing ticks. `level` is
/// `"info"`/`"warning"`/`"error"`; `bar` is the (history) bar index the call
/// executed on; `message` is the `str.format`-rendered text. Documented in
/// docs/browser.md.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogEntry {
    /// Script-selected log severity.
    pub level: LogLevel,
    /// Zero-based bar on which the message was recorded.
    pub bar: u32,
    /// Fully formatted log message.
    pub message: String,
}

/// One recorded `alert(...)`/`alertcondition(...)` event, in creation
/// order. `bar` is the bar index; `message` is the alert text; `freq` is the
/// firing-frequency token (`alert.freq_*`). Documented in docs/browser.md.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertEvent {
    /// Zero-based bar on which the alert fired.
    pub bar: u32,
    /// Fully formatted alert message.
    pub message: String,
    /// Pine alert-frequency token retained for host scheduling.
    pub freq: String,
}

/// Machine-readable category for a structured [`RuntimeError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeErrorKind {
    /// A run-constant TA integer input is outside its engine parameter bounds.
    InvalidTaLength,
    /// Price, volume, time, or related host lanes violate [`MarketData`]'s
    /// alignment and value rules.
    MalformedMarketData,
    /// One bar exceeded [`RunOptions::max_instructions_per_bar`].
    InstructionLimitExceeded,
    /// Cumulative historical + realtime work exceeded
    /// `RunOptions::max_instructions_per_run`.
    RunInstructionLimitExceeded,
    /// Operand-stack growth exceeded [`RunOptions::max_stack_depth`].
    StackOverflow,
    /// Underflow/type errors: unreachable after validation, kept as defense
    /// in depth.
    StackUnderflow,
    /// An instruction encountered a value outside its validated operand type.
    TypeMismatch,
    /// Program structure, versioning, control flow, or operands failed
    /// validation, or an execution request violated an invariant required by
    /// the selected run or incremental-session mode.
    InvalidProgram,
    /// A collection index (`array.get`/`set`/`insert`/`remove`/`slice`) was
    /// out of bounds, or `array.first`/`last`/`pop`/`shift` was called on an
    /// empty collection.
    CollectionIndexOutOfBounds,
    /// A string operation received an invalid substring range.
    StringIndexOutOfBounds,
    /// A collection operation would push the run's total live element count
    /// past `RunOptions::max_collection_elements` (a heap-growth bomb guard).
    CollectionLimitExceeded,
    /// Metered VM-owned state would exceed `RunOptions::max_vm_bytes_per_run`
    /// (or its size arithmetic overflowed).
    VmMemoryLimitExceeded,
    /// A runtime string operation or regex memoization would push cumulative
    /// string allocation past `RunOptions::max_string_bytes_per_run`.
    StringLimitExceeded,
    /// A collection method was called on an `na` receiver (an unassigned
    /// collection variable is `na`; calling a method on it is an error).
    NaMethodReceiver,
    /// A field was read or written on an `na` object (an unassigned
    /// user-defined-type variable is `na`; accessing its fields is an error).
    NaFieldAccess,
    /// A `matrix.*` operation received operands whose shapes are incompatible
    /// (e.g. `matrix.mult` with inner dimensions that do not agree, elementwise
    /// `matrix.sum`/`diff` on differently-shaped matrices, `matrix.concat` with
    /// mismatched column counts, `matrix.inv`/`det` on a non-square matrix).
    MatrixDimensionMismatch,
    /// `matrix.inv` was called on a singular (non-invertible) matrix.
    MatrixSingular,
    /// A dynamic history offset exceeded the effective `max_bars_back` buffer
    /// ceiling carried in the CSIL metadata.
    HistoryBufferLimitExceeded,
    /// A `request.security` call requested a timeframe lower than the actual
    /// chart feed's timeframe. The compile-time check can only sanity-bound
    /// the constant timeframe string (the real chart timeframe is a runtime
    /// property carried on `MarketData`); this is the binding check.
    SecurityTimeframeBelowChart,
    /// A `runtime.error(message)` call aborted the run. The `message`
    /// text is carried on the [`RuntimeError`]; an `na`/omitted message aborts
    /// with empty text. Fires on the bar/branch where the call executes.
    UserError,
    /// A valid program reached a runtime operation the local engine cannot
    /// execute, such as an invalid or engine-unsupported `str.match` regular
    /// expression. Strategy realtime ticks are supported by the integrated
    /// broker rollback/fill path and do not use this error kind.
    Unsupported,
}

/// A structured runtime error, addressable back to the source via `span`.
#[derive(Debug, Clone, PartialEq, Error, Serialize, Deserialize)]
#[error("runtime error at bar {bar_index:?}: {message}")]
#[serde(rename_all = "camelCase")]
pub struct RuntimeError {
    /// Machine-readable error category.
    pub kind: RuntimeErrorKind,
    /// Human-readable explanation suitable for a log or diagnostic surface.
    pub message: String,
    /// Zero-based bar being evaluated when the error occurred, when known.
    pub bar_index: Option<u32>,
    /// Bytecode instruction offset within `main`, when known.
    pub instruction_offset: Option<u32>,
    /// Original source range associated with the instruction, when available.
    pub span: Option<Span>,
}

// The strategy wire/report types (`StrategyTrade`, `StrategyPosition`, and
// the strategy report aggregate family) moved to `pinewasm_broker` with
// the emulator that produces them. Re-exported at their existing
// paths so the public API of this crate is unchanged.
pub use pinewasm_broker::{
    StrategyPosition, StrategyReportAggregates, StrategyReportOutliers,
    StrategyReportScopeAggregate, StrategyReportTradeHighlight, StrategyTrade,
};

/// The strategy section of a [`RunOutcome`]. Present only for scripts
/// that declared `strategy(...)`; `None` for indicators. Carries the broker
/// emulator's results: the closed trades, the final open position, the per-bar
/// equity series (`initial_capital + realized + open PnL`), and summary
/// metrics. Documented in docs/browser.md.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StrategyOutcome {
    /// The starting account equity (`strategy(initial_capital=)`).
    pub initial_capital: f64,
    /// Matched entry/exit trades in close order.
    pub trades: Vec<StrategyTrade>,
    /// Global index of the first retained trade, or `closed_trades` when no
    /// detail row survives the filled-order retention window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_retained_trade_index: Option<u32>,
    /// Full-run realized P&L immediately before `trades[0]`, or total realized
    /// P&L when no trade detail is retained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retained_trade_pnl_baseline: Option<f64>,
    /// Bar index of the first successfully executed opening leg. This is
    /// retained independently of closed-trade detail for buy-and-hold reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_entry_bar: Option<u32>,
    /// Fill price of that first opening leg.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_entry_price: Option<f64>,
    /// Account-currency entry notional for one contract at that fill:
    /// `abs(price) * syminfo.pointvalue * fill-bar FX rate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_entry_account_notional_per_unit: Option<f64>,
    /// Strategy account currency after applying `currency.NONE` fallback.
    #[serde(default)]
    pub account_currency: String,
    /// Quote currency of the chart symbol.
    #[serde(default)]
    pub symbol_currency: String,
    /// Per-bar symbol-to-account conversion rate aligned with `equity`.
    /// `None` marks unavailable/invalid FX data on a differing-currency bar;
    /// broker arithmetic preserves the documented identity fallback there.
    #[serde(default)]
    pub currency_rates: Vec<Option<f64>>,
    /// True only when every returned bar had a resolved conversion (including
    /// identity for equal/unset currencies).
    #[serde(default = "true_value")]
    pub currency_conversion_complete: bool,
    /// The open position at the end of the run (flat = `size == 0`).
    pub position: StrategyPosition,
    /// Per-bar account equity, one sample per executed bar.
    pub equity: Vec<f64>,
    /// Maximum absolute aggregate position size held at any point in the run.
    #[serde(default)]
    pub max_contracts_held: f64,
    /// Maximum aggregate long position size held at any point in the run.
    #[serde(default)]
    pub max_contracts_held_long: f64,
    /// Maximum aggregate short position size (magnitude) held in the run.
    #[serde(default)]
    pub max_contracts_held_short: f64,
    /// Required margin at every executed bar's close, aligned with `equity`.
    #[serde(default)]
    pub margin_used: Vec<f64>,
    /// Peak margin required at any broker-observed point in the run.
    #[serde(default)]
    pub maximum_margin_used: f64,
    /// Pathwise maximum of required margin plus the trade-anchored adverse
    /// buffer at the same broker-observed point. Absent when the run never held
    /// a position and when decoding artifacts produced before this telemetry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_account_size: Option<f64>,
    /// Retention-independent full-period report inputs for All/Long/Short.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_aggregates: Option<StrategyReportAggregates>,
    /// Number of successful forced-liquidation events.
    #[serde(default)]
    pub margin_calls: u32,
    /// Total liquidation account notional (`quantity * abs(fill price) *
    /// syminfo.pointvalue * fill-bar symbol-to-account rate`).
    #[serde(default)]
    pub total_liquidated: f64,
    /// Largest single margin-call liquidation account notional.
    #[serde(default)]
    pub largest_liquidation: f64,
    /// Cumulative realized profit (sum of closed-trade `pnl`).
    pub net_profit: f64,
    /// Unrealized profit of the open position at the final bar.
    pub open_profit: f64,
    /// `strategy.grossprofit` — summed profit of winning trades (>= 0).
    pub gross_profit: f64,
    /// `strategy.grossloss` — absolute value of summed losing-trade profit (>= 0).
    pub gross_loss: f64,
    /// `strategy.closedtrades` — total number of closed trades.
    pub closed_trades: u32,
    /// `strategy.wintrades` — closed trades with profit > 0.
    pub win_trades: u32,
    /// `strategy.losstrades` — closed trades with profit < 0.
    pub loss_trades: u32,
    /// `strategy.eventrades` — closed trades with profit == 0.
    pub even_trades: u32,
    /// `strategy.avg_trade` — `net_profit / closed_trades`; `0.0` when flat.
    pub avg_trade: f64,
    /// `strategy.max_drawdown` — largest trade-anchored intrabar
    /// account drawdown (>= 0).
    pub max_drawdown: f64,
    /// Independent intrabar percentage drawdown reported by the broker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_drawdown_percent: Option<f64>,
    /// Opening fees already charged on the remaining open position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_entry_commission: Option<f64>,
    /// `strategy.max_runup` — largest trade-anchored intrabar account
    /// run-up (>= 0).
    pub max_runup: f64,
}

/// Per-instruction profiling counters, present on the outcome only when
/// [`RunOptions::profile`] was set. Seven parallel arrays indexed by `pc` into
/// `main().code`, so hosts can attribute execution back to source without
/// holding the program: `span_*` is the instruction's source map entry
/// (byte-offset half-open range in the given source buffer; source 0 is the
/// main script, `i + 1` is `libraries[i]`), `count` is how many times the
/// instruction executed logically, and `units` is the total budget units it charged
/// (`1` per logical instruction plus the documented "honest metering" extras, exactly
/// mirroring [`RunOptions::max_instructions_per_bar`] accounting). Counts are
/// `f64` on the wire so JS hosts never see `BigInt`; instruction counts cannot
/// reach 2^53 in practice (the per-bar budget bounds them).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VmProfile {
    /// Source id for each instruction, parallel to [`VmProfile::count`].
    pub span_source: Vec<u32>,
    /// Inclusive UTF-8 source start for each instruction.
    pub span_start: Vec<u32>,
    /// Exclusive UTF-8 source end for each instruction.
    pub span_end: Vec<u32>,
    /// Logical execution count for each instruction, including instructions
    /// executed together by the interpreter.
    pub count: Vec<f64>,
    /// Number of adjacent instruction tails executed without their own dispatch.
    /// Subtract this from the sum of `count` to obtain dispatch-loop iterations.
    /// Instruction units and source attribution still include every instruction.
    #[serde(default)]
    pub combined_dispatch_count: f64,
    /// Skipped dispatch tails at each logical instruction. Their sum equals
    /// `combined_dispatch_count`; subtract from `count` to rank the remaining
    /// physical dispatches. Empty when reading profiles from older packages.
    #[serde(default)]
    pub combined_dispatch_by_pc: Vec<f64>,
    /// Total charged budget units for each instruction.
    pub units: Vec<f64>,
    /// `1` where the instruction is pure control flow (jumps, the
    /// `JumpIfSlotWritten` guard in front of `var` initializers, `Return`),
    /// `0` otherwise. Hosts deriving a statement's execution count should
    /// ignore flow instructions: a `var x = ...` line dispatches its guard on
    /// every bar but its initializer exactly once, and the initializer is the
    /// count a profiler should report.
    pub flow: Vec<u8>,
}

/// The live counter storage behind [`VmProfile`]: one execution count
/// and one charged-units total per instruction of `main`. Never snapshotted
/// for realtime rollback — a re-executed tick genuinely re-executed those
/// instructions, so accumulation is the honest record.
#[derive(Clone)]
struct ProfileCounters {
    // Logical counts, the aggregate skipped-tail count, then per-PC tails.
    // Keep the counter in a profiling-only allocation so diagnostics do not
    // enlarge or rearrange the hot Vm struct when profiling is disabled.
    count: Vec<u64>,
    units: Vec<u64>,
}

impl ProfileCounters {
    fn logical_counts(&self) -> &[u64] {
        &self.count[..self.units.len()]
    }

    fn combined_dispatches(&self) -> u64 {
        self.count[self.units.len()]
    }

    fn skipped_by_pc(&self) -> &[u64] {
        &self.count[self.units.len() + 1..]
    }

    fn record_combined_dispatch(&mut self, pc: usize) {
        let len = self.units.len();
        self.count[len] += 1;
        self.count[len + 1 + pc] += 1;
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunOutcome {
    /// Numeric plot outputs in compiled slot order.
    pub plots: Vec<PlotSeries>,
    /// Color-only chart outputs in compiled slot order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub visuals: Vec<VisualSeries>,
    /// Fill-band outputs in compiled slot order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fills: Vec<FillSeries>,
    /// Empty on success; holds the aborting error otherwise. Plots contain
    /// data for bars completed before the failure.
    pub errors: Vec<RuntimeError>,
    /// Number of distinct feed bars committed successfully. Closing an
    /// appended realtime bar increments this count; re-executing an already
    /// counted final historical bar does not.
    pub bars_executed: u32,
    /// Per-tick plot samples for the realtime segment, present only when
    /// [`RunOptions::realtime_trace`] was set and a realtime segment ran.
    /// `None` for history-only runs. Committed [`RunOutcome::plots`] hold the
    /// FINAL (bar-close) values regardless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realtime_trace: Option<Vec<PlotTickSample>>,
    /// The final live drawing objects, serialized per kind for hosts to
    /// render. Empty (and omitted from the wire) for scripts that create no
    /// drawings. See [`Drawings`] and docs/browser.md.
    #[serde(default, skip_serializing_if = "Drawings::is_empty")]
    pub drawings: Drawings,
    /// The recorded `log.*` event stream, in creation order. Empty (and
    /// omitted) for scripts that log nothing. See [`LogEntry`] and docs/browser.md.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub logs: Vec<LogEntry>,
    /// The recorded `alert(...)`/`alertcondition(...)` event stream, in
    /// creation order. Empty (and omitted) when no alert fired. See
    /// [`AlertEvent`] and docs/browser.md.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alerts: Vec<AlertEvent>,
    /// The strategy section: present only when the program declared
    /// `strategy(...)`. Holds the broker emulator's trades, open position,
    /// per-bar equity series, and summary metrics. Omitted from the wire for
    /// indicator scripts. See [`StrategyOutcome`] and docs/browser.md.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<StrategyOutcome>,
    /// Per-instruction profiling counters, present only when
    /// [`RunOptions::profile`] was set. See [`VmProfile`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<VmProfile>,
}

// ---------------------------------------------------------------------------
// Interpreter
// ---------------------------------------------------------------------------

/// Execute a **validated** program. Call [`pinewasm_csil::validate`] first;
/// `run` re-checks only what is cheap (defense in depth) and reports
/// anything else as `InvalidProgram` rather than panicking.
pub fn run(program: &CsilProgram, data: &MarketData, options: &RunOptions) -> RunOutcome {
    run_owned(program, None, data, options)
}

/// Validated immutable code shared by independent, fresh-state evaluations.
///
/// Only code is shared. Each run owns its inputs, history, TA state, broker,
/// outputs and resource budgets. Input specialization copies code only when
/// the selected TA parameters differ from the retained program.
pub struct PreparedProgram {
    program: Arc<CsilProgram>,
}

impl PreparedProgram {
    /// Validate and retain code that cannot subsequently be mutated by a host.
    pub fn new(program: CsilProgram) -> Result<Self, String> {
        pinewasm_csil::validate(&program).map_err(|errors| {
            errors.first().map_or_else(
                || "program failed validation".to_string(),
                std::string::ToString::to_string,
            )
        })?;
        Ok(Self {
            program: Arc::new(program),
        })
    }

    /// Execute with fresh mutable state and the same budgets as [`run`].
    pub fn run(&self, data: &MarketData, options: &RunOptions) -> RunOutcome {
        run_owned(&self.program, Some(&self.program), data, options)
    }
}

fn run_owned(
    program: &CsilProgram,
    owner: Option<&Arc<CsilProgram>>,
    data: &MarketData,
    options: &RunOptions,
) -> RunOutcome {
    if let Err(e) = data.validate() {
        return empty_outcome(e);
    }
    if options
        .realtime
        .as_ref()
        .is_some_and(|sim| !sim.ticks.is_empty())
        && options.max_bars.is_some_and(|max| max < data.bars())
    {
        return empty_outcome(RuntimeError {
            kind: RuntimeErrorKind::InvalidProgram,
            message:
                "realtime runs require the complete supplied history; maxBars cannot truncate it"
                    .to_string(),
            bar_index: None,
            instruction_offset: None,
            span: None,
        });
    }
    // Defense in depth against unvalidated programs: `main()` indexes
    // `functions[0]` and must not panic on adversarial input.
    if program.functions.is_empty() {
        return empty_outcome(RuntimeError {
            kind: RuntimeErrorKind::InvalidProgram,
            message: "program has no functions".to_string(),
            bar_index: None,
            instruction_offset: None,
            span: None,
        });
    }

    let mut vm = match Vm::new_owned(program, owner, data, options) {
        Ok(vm) => vm,
        Err(error) => return empty_outcome(error),
    };

    let (mut bars_executed, mut errors) = vm.run_history();

    // Realtime segment: replay ticks on the last bar(s), committing only
    // on bar-close. Skipped if history already aborted.
    let mut realtime_trace = None;
    if errors.is_empty() {
        if let Some(sim) = &options.realtime {
            match vm.run_realtime(sim, options.realtime_trace, &mut bars_executed) {
                Ok(trace) => realtime_trace = trace,
                Err(e) => errors.push(e),
            }
        }
    }

    let mut outcome = vm.outcome(
        errors,
        bars_executed,
        bars_executed as usize,
        realtime_trace,
    );
    drop(vm);
    // A fresh historical run can replay the unoptimized CSIL only after an
    // instruction-budget failure. Keep the generated offset from the actual
    // optimized run; replace only its source location when the unoptimized copy
    // reaches the same failure and has committed the same visible prefix.
    if vm_fault_location::should_replay_budget_reference(
        options.realtime.is_some(),
        program.main().budget_reference.is_some(),
        outcome.errors.len(),
        outcome.errors.first().map(|error| error.kind),
    ) {
        let mut original = program.clone();
        let main = &mut original.functions[0];
        let reference = main
            .budget_reference
            .take()
            .expect("reference checked above");
        main.code = reference.code;
        main.source_map = reference.source_map;
        main.max_stack = reference.max_stack;
        let reference_outcome = run_owned(&original, None, data, options);
        let matching_failure = reference_outcome.errors.len() == 1
            && reference_outcome.errors[0].kind == outcome.errors[0].kind
            && reference_outcome.errors[0].bar_index == outcome.errors[0].bar_index;
        let visible_prefix_matches = matching_failure.then(|| {
            reference_outcome.bars_executed == outcome.bars_executed
                && reference_outcome.plots == outcome.plots
                && reference_outcome.visuals == outcome.visuals
                && reference_outcome.fills == outcome.fills
                && reference_outcome.drawings == outcome.drawings
                && reference_outcome.logs == outcome.logs
                && reference_outcome.alerts == outcome.alerts
                && reference_outcome.strategy == outcome.strategy
        });
        let current_error = outcome.errors.first();
        let reference_error = reference_outcome.errors.first();
        let selection = vm_fault_location::select_budget_reference_span(
            vm_fault_location::BudgetErrorSummary {
                error_count: outcome.errors.len(),
                kind: current_error.map(|error| error.kind),
                bar_index: current_error.and_then(|error| error.bar_index),
                span: current_error.and_then(|error| error.span),
            },
            vm_fault_location::BudgetErrorSummary {
                error_count: reference_outcome.errors.len(),
                kind: reference_error.map(|error| error.kind),
                bar_index: reference_error.and_then(|error| error.bar_index),
                span: reference_error.and_then(|error| error.span),
            },
            visible_prefix_matches,
        );
        if let vm_fault_location::BudgetSpanSelection::UseReference(reference_span) = selection {
            outcome.errors[0].span = reference_span;
        }
    }
    outcome
}

/// Maximum number of bars retained by one persistent incremental session.
///
/// Hosts should rebuild a session from their bounded feed when this limit is
/// reached. The guard keeps a forgotten or hostile session from growing
/// without bound while leaving ordinary one-shot runs unchanged.
pub const MAX_INCREMENTAL_BARS: usize = 25_000;

/// A compile-once, history-once VM that advances through realtime ticks.
///
/// The session owns every stateful Pine surface, including `var`, `varip`, TA
/// engines, drawings, logs, alerts, and the strategy broker. A failed advance
/// is terminal because the tick may have mutated several coupled subsystems;
/// the host must discard the session and rebuild it from its
/// market-data snapshot.
pub struct IncrementalSession {
    vm: Vm,
    bars_executed: u32,
    failed: Option<RuntimeError>,
}

impl IncrementalSession {
    /// Build a session and execute its confirmed historical prefix once.
    ///
    /// `options.realtime` is intentionally ignored. Realtime work enters only
    /// through [`Self::advance`], which gives hosts an explicit commit and
    /// rollback boundary for each incremental request.
    pub fn new(
        program: &CsilProgram,
        data: &MarketData,
        options: &RunOptions,
    ) -> Result<Self, RuntimeError> {
        data.validate()?;
        if program.functions.is_empty() {
            return Err(RuntimeError {
                kind: RuntimeErrorKind::InvalidProgram,
                message: "program has no functions".to_string(),
                bar_index: None,
                instruction_offset: None,
                span: None,
            });
        }
        if data.bars() > MAX_INCREMENTAL_BARS {
            return Err(session_limit_error(data.bars()));
        }
        if options.max_bars.is_some_and(|max| max < data.bars()) {
            return Err(RuntimeError {
                kind: RuntimeErrorKind::InvalidProgram,
                message: "incremental sessions require the complete supplied history; maxBars cannot truncate it".to_string(),
                bar_index: None,
                instruction_offset: None,
                span: None,
            });
        }

        let mut session_options = options.clone();
        session_options.realtime = None;
        let mut vm = Vm::new(program, data, &session_options)?;
        // The historical prefix precedes a realtime segment, so its barstate
        // truth table must match a one-shot run that has realtime ticks.
        vm.has_realtime = true;
        let (bars_executed, errors) = vm.run_history();
        if let Some(error) = errors.into_iter().next() {
            return Err(error);
        }
        Ok(IncrementalSession {
            vm,
            bars_executed,
            failed: None,
        })
    }

    /// Return the current result without advancing the session.
    pub fn result(&self) -> RunOutcome {
        let errors = self.failed.iter().cloned().collect();
        self.vm
            .outcome(errors, self.bars_executed, self.vm.bars, None)
    }

    /// Apply ordered realtime ticks and return the current visible state.
    ///
    /// An open forming bar is included in plot lanes even though
    /// `bars_executed` counts only committed bars. A closing tick commits the
    /// bar and the next tick may append exactly one successor. Tick/trace
    /// payload and persistent feed/output growth are admitted against
    /// [`RunOptions::max_vm_bytes_per_run`] before their vectors grow.
    pub fn advance(&mut self, ticks: &[Tick]) -> RunOutcome {
        if let Some(error) = self.failed.clone() {
            return self
                .vm
                .outcome(vec![error], self.bars_executed, self.vm.bars, None);
        }
        if ticks
            .iter()
            .any(|tick| tick.bar_index >= MAX_INCREMENTAL_BARS && tick.bar_index >= self.vm.bars)
        {
            let error = session_limit_error(MAX_INCREMENTAL_BARS.saturating_add(1));
            self.failed = Some(error.clone());
            return self
                .vm
                .outcome(vec![error], self.bars_executed, self.vm.bars, None);
        }

        let collect_trace = self.vm.options.realtime_trace;
        if let Err(error) = self
            .vm
            .begin_incremental_advance(ticks.len(), collect_trace)
        {
            self.failed = Some(error.clone());
            return self
                .vm
                .outcome(vec![error], self.bars_executed, self.vm.bars, None);
        }

        let sim = RealtimeSim {
            ticks: ticks.to_vec(),
        };
        let result = self
            .vm
            .run_realtime(&sim, collect_trace, &mut self.bars_executed);
        self.vm.end_incremental_advance();
        match result {
            Ok(trace) => self
                .vm
                .outcome(Vec::new(), self.bars_executed, self.vm.bars, trace),
            Err(error) => {
                self.failed = Some(error.clone());
                self.vm
                    .outcome(vec![error], self.bars_executed, self.vm.bars, None)
            }
        }
    }

    /// Apply ordered realtime ticks and return one sample per tick instead of
    /// the whole visible history.
    ///
    /// [`Self::advance`] answers with every plot lane from bar zero, so its
    /// cost grows with the resident history even when a tick touches a single
    /// bar. A host that only repaints the forming bar pays that snapshot for
    /// nothing, and a measurement that claims to time one bar would be timing
    /// the history behind it. This entry point runs the identical tick path and
    /// returns exactly the per-tick samples [`RunOutcome::realtime_trace`]
    /// carries — one value per plot slot, in slot order, after that tick —
    /// without copying any plot lane.
    ///
    /// Samples are collected regardless of [`RunOptions::realtime_trace`],
    /// which stays the switch for the snapshot-shaped path, and the transient
    /// payload is admitted against [`RunOptions::max_vm_bytes_per_run`] the
    /// same way.
    ///
    /// Failure semantics are the session's own: a failed tick is terminal, and
    /// every later call through either entry point reports that same error.
    ///
    /// # Errors
    ///
    /// Returns the structured [`RuntimeError`] that failed this advance, or the
    /// one that already made the session terminal.
    pub fn advance_samples(&mut self, ticks: &[Tick]) -> Result<Vec<PlotTickSample>, RuntimeError> {
        if let Some(error) = self.failed.clone() {
            return Err(error);
        }
        if ticks
            .iter()
            .any(|tick| tick.bar_index >= MAX_INCREMENTAL_BARS && tick.bar_index >= self.vm.bars)
        {
            let error = session_limit_error(MAX_INCREMENTAL_BARS.saturating_add(1));
            self.failed = Some(error.clone());
            return Err(error);
        }
        if let Err(error) = self.vm.begin_incremental_advance(ticks.len(), true) {
            self.failed = Some(error.clone());
            return Err(error);
        }
        let sim = RealtimeSim {
            ticks: ticks.to_vec(),
        };
        let result = self.vm.run_realtime(&sim, true, &mut self.bars_executed);
        self.vm.end_incremental_advance();
        match result {
            // `collect_trace` was true, so the trace is always present; an
            // empty tick slice legitimately produces an empty sample list.
            Ok(trace) => Ok(trace.unwrap_or_default()),
            Err(error) => {
                self.failed = Some(error.clone());
                Err(error)
            }
        }
    }

    pub fn committed_bars(&self) -> u32 {
        self.bars_executed
    }

    pub fn resident_bars(&self) -> usize {
        self.vm.bars
    }

    pub fn is_failed(&self) -> bool {
        self.failed.is_some()
    }
}

fn session_limit_error(attempted_bars: usize) -> RuntimeError {
    RuntimeError {
        kind: RuntimeErrorKind::MalformedMarketData,
        message: format!(
            "incremental session bar limit exceeded: attempted {attempted_bars}, maximum is {MAX_INCREMENTAL_BARS}; rebuild from a bounded history snapshot"
        ),
        bar_index: None,
        instruction_offset: None,
        span: None,
    }
}

fn empty_outcome(error: RuntimeError) -> RunOutcome {
    RunOutcome {
        plots: Vec::new(),
        visuals: Vec::new(),
        fills: Vec::new(),
        errors: vec![error],
        bars_executed: 0,
        realtime_trace: None,
        drawings: Drawings::default(),
        logs: Vec::new(),
        alerts: Vec::new(),
        strategy: None,
        profile: None,
    }
}

/// One prior-completed anchor-bucket `(high, low, close)` per chart bar for
/// `ta.pivot_point_levels`; `None` on bars before any anchor bucket has closed.
type PivotHlcSeries = Vec<Option<(f64, f64, f64, f64, f64)>>;

#[derive(Clone, Default)]
struct PivotPointState {
    period: Option<(f64, f64, f64, f64)>,
    completed_period: Option<(f64, f64, f64, f64, f64)>,
}

/// Argument-count ceiling for the stack-local call-argument buffer. Sized
/// for the hot loop-body builtins (math.*, array get/set/push — all arity
/// <= 4); the buffer is initialized and dropped on every call instruction,
/// so headroom costs cycles. Wider calls (5+ args: alert-style plumbing,
/// variadic tails) spill to the heap scratch vector.
const INLINE_CALL_ARGS: usize = 4;

/// Arena size below which the heap is never collected. A script holding a few
/// `var` collections stays under this for its whole run and pays nothing;
/// above it, [`Vm::maybe_collect_heap`] tracks twice the live set.
const GC_MIN_ARENA: usize = 4_096;

/// Element count at which the next bar boundary collects: half the headroom
/// still available above what the heap currently holds live.
///
/// Expressed against headroom rather than as a multiple of the live set so it
/// scales with any `max_collection_elements`, including the deliberately tiny
/// caps the limit tests use. At least 1, so a heap already at its cap does not
/// arm a threshold it trivially meets every bar.
fn re_arm_elem_threshold(heap: &Heap) -> usize {
    let live = heap.live_elements();
    let headroom = heap.max_elements().saturating_sub(live);
    live.saturating_add(headroom / 2).max(1)
}

/// Maximum historical offset that can read each user slot. A dynamic numeric
/// window may enlarge the shared ring, but should not pin unrelated arrays in
/// old cells. Compute from validated instructions and resolved input lengths.
fn user_slot_gc_depths(program: &CsilProgram, values: &HashMap<TaInputTarget, i64>) -> Vec<u32> {
    let mut depths = vec![0_u32; program.series_slots.len()];
    for function in &program.functions {
        for instr in &function.code {
            let (slot, depth) = match instr {
                CsilInstr::ReadUserSlot { slot, offset } => (
                    *slot,
                    if *offset == 0 {
                        0
                    } else {
                        values
                            .get(&TaInputTarget::Change { slot: *slot })
                            .map_or(*offset, |value| *value as u32)
                    },
                ),
                CsilInstr::ReadUserSlotDynamic { slot } => (*slot, program.metadata.max_bars_back),
                CsilInstr::WindowMean { slot, length } => (
                    *slot,
                    values
                        .get(&TaInputTarget::Sma { slot: *slot })
                        .map_or(*length, |value| *value as u32)
                        .saturating_sub(1),
                ),
                CsilInstr::WindowMeanDynamic { slot }
                | CsilInstr::WindowExtremumDynamic { slot, .. }
                | CsilInstr::WindowStatisticDynamic { slot, .. } => {
                    (*slot, MAX_DYNAMIC_WINDOW_DEPTH)
                }
                _ => continue,
            };
            if let Some(entry) = depths.get_mut(slot as usize) {
                *entry = (*entry).max(depth);
            }
        }
    }
    depths
}

#[derive(Clone, Copy)]
struct DynamicEmaState {
    length: u32,
    smoother: pinewasm_ta::Smoother,
}

struct Vm {
    program: Arc<CsilProgram>,
    /// Owned working copy of the market data. Historical execution never
    /// mutates it; the realtime driver overwrites the realtime bar's
    /// OHLCV in place (and may append a bar), so the VM owns it rather than
    /// borrowing the caller's immutable data. The clone is one-time per run.
    data: MarketData,
    options: Arc<RunOptions>,
    /// One history ring per user series slot, indexed by `bar & slot_mask`.
    ///
    /// A ring rather than one cell per bar because nothing deeper than
    /// [`CsilProgram::history_depth`] is observable: dynamic offsets past
    /// `max_bars_back` abort, literal offsets and `WindowMean` lengths are
    /// compile-time constants the ring is sized to cover, and a
    /// runtime-length window (`WindowMeanDynamic`, `WindowExtremumDynamic`,
    /// `WindowStatisticDynamic`) is rejected above [`MAX_WINDOW_LENGTH`], so
    /// its observer sizes the ring for [`MAX_DYNAMIC_WINDOW_DEPTH`]. Storing
    /// full history instead costs 24 bytes per slot per bar, and it
    /// pinned every per-bar heap object those cells referenced.
    ///
    /// This mirrors what `pinewasm_aot` has always emitted, so the two
    /// tiers now agree on storage as well as semantics; the AOT differential
    /// tests compare them bar for bar and would catch a ring-indexing slip
    /// immediately.
    user_slots: Vec<Vec<Value>>,
    /// `ring - 1`, for `bar & slot_mask`. The ring is a power of two.
    slot_mask: usize,
    /// Only these per-slot historical cells may be read after a bar boundary.
    /// The global ring can be much deeper because of an unrelated numeric
    /// window; rooting every cell would retain unreachable local collections.
    gc_history_depths: Vec<u32>,
    /// Per-slot "ever stored" flag, persisted across bars. Set by
    /// `StoreUserSlotCurrent` only (not by carry-forward); consulted by
    /// `JumpIfSlotWritten` to give `var` initializers run-once semantics.
    written: Vec<bool>,
    /// One output vector per plot slot, indexed by bar.
    plots: Vec<Vec<Value>>,
    /// Per-plot, per-role runtime colors, indexed by bar.
    plot_colors: Vec<Vec<Vec<Value>>>,
    /// Per-visual, per-role runtime colors, indexed by bar.
    visual_colors: Vec<Vec<Vec<Value>>>,
    /// Per-fill, per-role runtime colors, indexed by bar.
    fill_colors: Vec<Vec<Vec<Value>>>,
    /// One engine state per `ta_states` declaration (`CALL_TA` call site),
    /// over the shared cell region their ring buffers index into.
    ta_state: ta::TaStates,
    /// Runtime-simple EMA lengths share the TA engine's smoothing primitive,
    /// keyed by their synthetic call-site slot.
    dynamic_ema: Vec<Option<DynamicEmaState>>,
    /// Dense, bounded source observations for the slot-based SMA path.
    sma: sma::States,
    stack: Vec<Value>,
    /// Reusable scratch for call-instruction arguments (`CALL_PURE`,
    /// `CALL_COLLECTION`). Argument-taking builtins run millions
    /// of times in loop-heavy scripts; moving the operands here instead of
    /// `split_off`-ing a fresh vector keeps those calls allocation-free.
    /// Swapped out/in around each call via [`Vm::take_call_args`] /
    /// [`Vm::restore_call_args`], so the buffer is never aliased while the
    /// callee borrows it.
    call_args: Vec<Value>,
    /// Reused sort buffer for dynamic median/mode windows. It is reserved at
    /// admission time when such an instruction exists and never grows while
    /// bars are executing.
    window_scratch: Vec<f64>,
    bar: usize,
    /// Effective bar count for this run (`min(data.bars(), max_bars)`).
    /// `last_bar_index`/`last_bar_time`/`timenow` reference `bars - 1`.
    bars: usize,
    /// Remaining cumulative instruction units for the whole run. Unlike the
    /// local per-bar counter this is never reset at a bar or realtime-tick
    /// boundary.
    instructions_remaining: u64,
    /// How many `math.random` values have been drawn this run. Together
    /// with the (per-call) seed key it fully determines every draw, so
    /// runs are byte-for-byte reproducible (see docs/runtime.md).
    random_draws: u64,
    // ---- realtime / barstate driver state -----------------------------
    /// True while executing a realtime tick (not a historical bar). Drives
    /// `barstate.isrealtime` / `ishistory`.
    realtime_active: bool,
    /// Whether the run has a realtime segment at all. Historical `barstate`
    /// values depend on this: with a realtime segment the last historical bar
    /// is `islastconfirmedhistory` and is NOT `islast`.
    has_realtime: bool,
    /// True on the first execution of the current bar (`barstate.isnew`).
    /// Historical bars: always true. Realtime: true on the opening tick.
    bar_is_new: bool,
    /// True on the closing execution of the current bar (`barstate.isconfirmed`).
    /// Historical bars: always true. Realtime: true on an `is_bar_close` tick.
    bar_is_confirmed: bool,
    /// The bar index of the realtime bar currently open (Some while replaying
    /// realtime ticks). `barstate.islast` is true on this bar; historical bars
    /// then read `islast = false`.
    realtime_bar: Option<usize>,
    /// Cross-request realtime rollback and commit state. A one-shot run uses
    /// this exactly once; an incremental session keeps it between calls.
    realtime_driver: RealtimeDriverState,
    /// Original values of historical user-slot cells that an unconfirmed
    /// `table.delete` invalidated. The tick driver's snapshot is temporarily
    /// moved out of `Vm` while a tick runs, so this per-open-bar journal
    /// restores history cells that the ordinary snapshot does not capture.
    realtime_alias_undo: BTreeMap<(usize, usize), Value>,
    /// The per-run collection heap. Reference-typed collections
    /// (`array<T>`) live here; variables hold [`HandleId`]s into it. It
    /// persists across bars (a `var` array's handle carries forward and its
    /// object lives the whole run) and is snapshotted/restored for realtime
    /// rollback.
    heap: Heap,
    distance_cache: distance_scan::Cache,
    // Live cache high-water reservation, retained across rollback/rebuilds.
    distance_cache_reserved_bytes: usize,
    /// Arena size at which the next bar boundary collects the heap. Re-armed to
    /// twice the surviving live set after each collection, so the arena stays
    /// within a constant factor of what the program actually holds and a script
    /// that allocates nothing per bar never collects at all.
    gc_threshold: usize,
    /// Live element count at which the next bar boundary collects. Tracked
    /// separately from the arena because the two budgets run out
    /// independently: one large array per bar exhausts elements while the
    /// arena is still tiny, and a million empty arrays do the reverse.
    ///
    /// Re-armed to half the *remaining* headroom rather than a multiple of the
    /// live set, so a program legitimately holding most of the budget collects
    /// a bounded number of times instead of on every bar.
    gc_elem_threshold: usize,
    /// The per-kind live drawing-object registry. For each registered
    /// drawing kind (`line`/`label`/`box`) an insertion-ordered list of the
    /// live object handles; creation appends, and when the list would exceed
    /// the kind's max count the oldest (front) handle is dropped
    /// (deterministic oldest-first GC). The `drawings` section of the outcome
    /// serializes the final live objects per kind. Snapshotted/restored for
    /// realtime rollback alongside the heap.
    drawings: DrawingRegistry,
    /// Last assigned table lifetime identity. Unlike heap handles and drawing
    /// output ordinals, this counter is never restored on realtime rollback.
    /// Table objects carry the assigned identity through heap cloning/remapping.
    last_table_identity: u64,
    /// The recorded `log.*` event stream, in creation order. Appended by
    /// the `Log` op; snapshotted/restored for realtime rollback.
    logs: Vec<LogEntry>,
    /// The recorded alert event stream, in creation order. Appended by
    /// the `Alert`/`AlertCondition` ops; snapshotted/restored for realtime
    /// rollback.
    alerts: Vec<AlertEvent>,
    /// The program counters of the once-per-bar `alert()` call sites that have
    /// already fired on the current bar, keyed by instruction offset (each
    /// syntactic `alert()` call is a distinct `Alert` opcode at a distinct pc).
    /// `alert()` calls are deduplicated per call site within a bar (see docs/runtime.md):
    /// a single call site executed repeatedly (a loop, or realtime ticks) fires once,
    /// while two distinct `alert()`
    /// calls each fire. Reset at each bar's start.
    alert_fired_sites: HashSet<usize>,
    /// The strategy broker emulator, present only for strategy scripts.
    /// The `Order` opcode places orders into it; the bar loop drives its fill
    /// engine and per-bar equity recording. `None` for indicator scripts.
    broker: Option<Broker>,
    /// The tick price an immediate order fills at while a historical bar is
    /// recalculated after a fill. That recalculation sees the whole bar's
    /// prices in `data`, so `close` there is not the current tick.
    fill_recalculation_price: Option<f64>,
    /// Memoized same-feed `request.security` back-mapped series, keyed
    /// by the call site's `(source, htf_seconds, gaps_on, lookahead_on)`. Each
    /// entry is one value per chart bar (`None` = na / not-yet-available),
    /// computed once by resampling the chart feed to the requested timeframe
    /// and mapping the HTF series back (see
    /// [`resample::Resampled::map_to_chart_bars`]). Deterministic and
    /// feed-only — never mutated after insertion. Host-backed external-symbol
    /// requests read `MarketData::request_data` directly and do not use this.
    security_memo: SecurityMemo,
    /// Memoized COMPOUND `request.security` back-mapped series, keyed by
    /// the call site's `HtfExpr` node address (stable for the run — the program
    /// outlives the loop) plus `(htf_seconds, gaps_on, lookahead_on)`. Each
    /// entry is one value per chart bar, computed once by running the secondary
    /// context over the resampled bars with its own `ta.*` state and mapping
    /// back (see [`Vm::read_security_expr`]). Feed-only and deterministic.
    security_expr_memo: SecurityExprMemo,
    /// Memoized prior-completed anchor-bucket HLC for `ta.pivot_point_levels`,
    /// keyed by `anchor_seconds`. Each entry is one `(high, low, close)`
    /// per chart bar — the most recently COMPLETED anchor bucket at that bar
    /// (`lookahead_off`; `None` before any bucket has closed) — computed once
    /// by resampling the chart feed to the anchor timeframe. The pivot `system`
    /// is applied per read, so one memo serves every system on the same anchor.
    /// Deterministic and feed-only — never mutated after insertion.
    pivot_hlc_memo: HashMap<i64, PivotHlcSeries>,
    /// Stateful pivot calculation buffers keyed by the `CALL_PIVOT_POINT_LEVELS`
    /// instruction offset. Unlike the timeframe adapter above, this
    /// follows the script's actual boolean anchor series.
    pivot_states: HashMap<usize, PivotPointState>,
    /// Memoized `time()` / `time_close()` higher-timeframe bucket opens,
    /// keyed by the requested timeframe string. Each entry maps chart bars to
    /// the open time of the enclosing requested bucket, and is extended in
    /// place when a realtime tick appends a bar (see
    /// [`Vm::time_function_bucket_open`]).
    time_func_memo: HashMap<String, resample::BucketOpens>,
    /// Memoized compiled regex patterns for `str.match`. Most patterns are
    /// const strings; caching keeps the per-bar pure-call path cheap while
    /// still allowing dynamic series string patterns.
    regex_memo: HashMap<String, Result<Arc<Regex>, String>>,
    /// UTF-8 bytes held by keys and cached compile-error strings in
    /// `regex_memo`. Compiled automata have separate builder size limits.
    regex_memo_bytes: usize,
    /// Cumulative runtime string bytes reserved during this run. This is an
    /// allocation-churn bound, so bytes are intentionally never returned.
    string_bytes_allocated: usize,
    /// Predictable preallocation plus cumulative dynamic VM-owned table/event
    /// reservations. Like the string counter this never decreases. The
    /// distance index reuses a peak reservation across rebuilds; its charged
    /// work bounds allocation churn while this reservation bounds its peak.
    bytes_reserved: usize,
    /// Per-call tick and trace payload admitted by an incremental advance.
    /// This participates in peak checks but is cleared after the call because
    /// the payload is not retained by the session.
    transient_vm_bytes: usize,
    /// Realtime append slots already included in one-shot preallocation.
    /// Persistent sessions start at zero and reserve each actual append.
    preallocated_realtime_appends: usize,
    /// Per-instruction profiling counters, allocated only when
    /// [`RunOptions::profile`] is set. Indexed by `pc` into `main().code`.
    /// Accumulates across all bars and realtime ticks; deliberately NOT part
    /// of [`StateSnapshot`] — a rolled-back tick still executed its
    /// instructions, and the honest record keeps them.
    profile: Option<ProfileCounters>,
}

/// The number of registered (live-tracked, GC'd, serialized) drawing kinds:
/// `line`, `label`, `box`, `table`, `polyline`, `linefill`. `chart.point` is
/// not registered.
const REGISTERED_DRAW_KINDS: usize = 6;

/// Regex limits are intentionally independent of host input. Patterns remain
/// expressive enough for Pine-style lookaround while malformed or adversarial
/// dynamic patterns cannot grow an unbounded compile cache or backtrack
/// indefinitely.
const MAX_REGEX_PATTERN_BYTES: usize = 16 * 1024;
const MAX_REGEX_MEMO_ENTRIES: usize = 64;
const MAX_REGEX_MEMO_BYTES: usize = 256 * 1024;
const REGEX_BACKTRACK_LIMIT: usize = 100_000;
const REGEX_DELEGATE_SIZE_LIMIT: usize = 256 * 1024;
const REGEX_DELEGATE_DFA_SIZE_LIMIT: usize = 256 * 1024;

/// One cell of a table's out-of-line grid. Every attribute is
/// optional: a freshly written cell that only sets `text` leaves the rest
/// `None`, and they serialize as absent. `merged` marks the cell as part of a
/// merged span (set by `table.merge_cells`). A cell that has never been touched
/// is absent from the grid entirely (see [`TableGrid`]).
#[derive(Debug, Clone, Default, PartialEq)]
struct TableCell {
    text: Option<Value>,
    width: Option<Value>,
    height: Option<Value>,
    text_color: Option<Value>,
    text_halign: Option<Value>,
    text_valign: Option<Value>,
    text_size: Option<Value>,
    bgcolor: Option<Value>,
    tooltip: Option<Value>,
    text_font_family: Option<Value>,
    text_formatting: Option<Value>,
    merged: bool,
    merge: Option<[i64; 4]>,
}

impl TableCell {
    /// True when no attribute is set and the cell is not merged — such a cell is
    /// dropped from the grid (so `table.clear` truly removes it).
    fn is_empty(&self) -> bool {
        *self == TableCell::default()
    }

    /// Write one attribute by [`CellAttr`].
    fn set(&mut self, attr: CellAttr, value: Value) {
        match attr {
            CellAttr::Text => self.text = Some(value),
            CellAttr::Width => self.width = Some(value),
            CellAttr::Height => self.height = Some(value),
            CellAttr::TextColor => self.text_color = Some(value),
            CellAttr::TextHalign => self.text_halign = Some(value),
            CellAttr::TextValign => self.text_valign = Some(value),
            CellAttr::TextSize => self.text_size = Some(value),
            CellAttr::Bgcolor => self.bgcolor = Some(value),
            CellAttr::Tooltip => self.tooltip = Some(value),
            CellAttr::TextFontFamily => self.text_font_family = Some(value),
            CellAttr::TextFormatting => self.text_formatting = Some(value),
        }
    }

    /// The value set for `attr`, if any (for serialization).
    fn get(&self, attr: CellAttr) -> Option<&Value> {
        match attr {
            CellAttr::Text => self.text.as_ref(),
            CellAttr::Width => self.width.as_ref(),
            CellAttr::Height => self.height.as_ref(),
            CellAttr::TextColor => self.text_color.as_ref(),
            CellAttr::TextHalign => self.text_halign.as_ref(),
            CellAttr::TextValign => self.text_valign.as_ref(),
            CellAttr::TextSize => self.text_size.as_ref(),
            CellAttr::Bgcolor => self.bgcolor.as_ref(),
            CellAttr::Tooltip => self.tooltip.as_ref(),
            CellAttr::TextFontFamily => self.text_font_family.as_ref(),
            CellAttr::TextFormatting => self.text_formatting.as_ref(),
        }
    }
}

/// A table's out-of-line cell grid, keyed by `(column, row)`.
/// Sparse: only touched cells are present. The `columns`/`rows` bounds are
/// captured from the table's declared dimensions at first write so index
/// validation never has to re-read the heap object.
#[derive(Debug, Clone, Default, PartialEq)]
struct TableGrid {
    cells: std::collections::BTreeMap<(i64, i64), TableCell>,
}

/// Conservative accounting charge for one sparse `BTreeMap` table cell,
/// including its key/value payload and allocator/tree-node overhead.
const TABLE_CELL_RESERVATION_BYTES: usize =
    std::mem::size_of::<TableCell>() + std::mem::size_of::<(i64, i64)>() + 64;
/// Deterministic logical VM-memory charge per realtime alias-undo entry. It
/// estimates the key/value payload plus fixed map-node slack; it is not a
/// physical allocator-capacity bound.
const REALTIME_ALIAS_UNDO_ENTRY_RESERVATION_BYTES: usize =
    std::mem::size_of::<((usize, usize), Value)>() + 64;
/// Delete-time work for one sparse table cell: one map entry, its eleven
/// optional value attributes, and the merge metadata / flag.
const TABLE_CELL_DELETE_WORK_UNITS: u64 = 14;

/// Conservative peak bytes per chart bar while building and retaining one
/// simple resample-backed memo (`request.security`, pivot HLC, or time opens).
/// This covers the temporary HTF bars, chart-to-bucket map, and retained
/// chart-bar result. Compound security adds its per-site history lanes below.
const RESAMPLE_CACHE_RESERVATION_BYTES_PER_BAR: usize = 128;
/// Hash-table entry / vector-header slack for one resample-backed memo.
const RESAMPLE_CACHE_ENTRY_RESERVATION_BYTES: usize = 512;

/// The per-run live drawing-object registry. See [`Vm::drawings`].
#[derive(Debug, Clone, Default)]
struct DrawingRegistry {
    /// Insertion-ordered live handles per registered kind (in [`DrawKind`]
    /// order via `slot_of`). `chart.point` is not tracked.
    live: [Vec<HandleId>; REGISTERED_DRAW_KINDS],
    /// The per-kind max live count (from `indicator(max_*_count=)` or the
    /// default), in the same slot order as `live`.
    max: [usize; REGISTERED_DRAW_KINDS],
    /// Per-table out-of-line cell grids, keyed by the table's heap
    /// handle. Snapshotted/restored with the rest of the registry for realtime
    /// rollback. A table with no written cells has no entry here.
    table_cells: std::collections::BTreeMap<u32, TableGrid>,
    /// The drawing id of each live handle, index-aligned with `live`. This is
    /// the `id` a host reads in the outcome's drawings. It is the drawing's
    /// registration ordinal and not its heap handle: a compiled module keeps
    /// scalar arrays and plain objects in linear memory, so the interpreter
    /// and the module give the same drawing different handles, and an id
    /// taken from the handle would differ between them.
    ids: [Vec<u32>; REGISTERED_DRAW_KINDS],
    /// The id the next registered drawing receives: one counter across every
    /// kind, so an id names one drawing for the whole run. It is cloned and
    /// restored with the registry, so a rolled-back realtime tick issues the
    /// same ids again.
    next_id: u32,
}

impl DrawingRegistry {
    /// Append `handle` under a fresh id and return the oldest handle if the
    /// kind's fixed live cap evicts it. The caller uses a line eviction to
    /// end the lifetimes of fills attached to that line.
    fn register(&mut self, slot: usize, handle: HandleId) -> Option<HandleId> {
        self.live[slot].push(handle);
        self.ids[slot].push(self.next_id);
        // One registration follows one successful heap allocation, and the
        // arena stops below `u32::MAX` slots, so this never wraps.
        self.next_id = self.next_id.wrapping_add(1);
        // Creating the (max+1)th deletes the oldest (front).
        if self.live[slot].len() > self.max[slot] {
            self.ids[slot].remove(0);
            Some(self.live[slot].remove(0))
        } else {
            None
        }
    }

    /// Remove `handle` from the live list at `slot`, with its id (no-op if
    /// absent).
    fn unregister(&mut self, slot: usize, handle: HandleId) {
        if let Some(pos) = self.live[slot].iter().position(|&x| x == handle) {
            self.live[slot].remove(pos);
            self.ids[slot].remove(pos);
        }
    }

    /// Remove matching live drawings and their aligned ids while retaining
    /// the creation order of every survivor.
    fn unregister_where(&mut self, slot: usize, mut matches: impl FnMut(HandleId) -> bool) {
        let mut index = 0;
        while index < self.live[slot].len() {
            if matches(self.live[slot][index]) {
                self.live[slot].remove(index);
                self.ids[slot].remove(index);
            } else {
                index += 1;
            }
        }
    }

    /// The `live`/`max` slot index for a registered kind, or `None` for a
    /// non-registered kind (`chart.point`).
    fn slot_of(kind: DrawKind) -> Option<usize> {
        match kind {
            DrawKind::Line => Some(0),
            DrawKind::Label => Some(1),
            DrawKind::Box => Some(2),
            DrawKind::Table => Some(3),
            DrawKind::Polyline => Some(4),
            DrawKind::Linefill => Some(5),
            DrawKind::Point => None,
        }
    }

    /// The registered kind for a slot index (inverse of [`Self::slot_of`]).
    fn kind_of(slot: usize) -> DrawKind {
        match slot {
            0 => DrawKind::Line,
            1 => DrawKind::Label,
            2 => DrawKind::Box,
            3 => DrawKind::Table,
            4 => DrawKind::Polyline,
            _ => DrawKind::Linefill,
        }
    }
}

/// A clone-on-snapshot of all non-`varip` state that a realtime tick
/// may mutate, taken at the realtime bar's start and restored (rollback)
/// before each subsequent tick on that bar. `varip` slots are
/// deliberately excluded — they persist across ticks.
///
/// Snapshot allocations include duplicate slot/output cells, retained TA-engine
/// payload, the heap arena (including collected slots), drawing/table state,
/// and alert-dedup state. They are outside the deterministic VM byte counter.
/// Other collection/string/object and drawing limits constrain portions of the
/// source state, but snapshot duplication can make peak process memory exceed
/// [`RunOptions::max_vm_bytes_per_run`].
#[derive(Clone)]
struct StateSnapshot {
    base: vm_rollback::Snapshot<Value, ta::TaStates, Heap, DrawingRegistry, HashSet<usize>>,
    // Dynamic EMA state is restored alongside the other non-cell state; keep the
    // extracted rollback kernel's generic interface intact while restoring it.
    dynamic_ema: Vec<Option<DynamicEmaState>>,
    sma: sma::States,
    pivot_states: HashMap<usize, PivotPointState>,
}

/// The bodies of `Vm::snapshot` and `Vm::restore` as free functions, generic
/// over the cell type and over the non-cell state (`Vm` itself is outside the
/// Aeneas subset: it holds `f64`, `String`, `BTreeMap` and `dyn` state, and
/// inherent methods cannot be extraction roots). `Vm::snapshot`/`Vm::restore`
/// only pass their fields; the mask, the ring cell, the cell kernels and the
/// non-cell restore are here.
/// Extracted by Aeneas.
pub(crate) mod vm_rollback {
    use crate::vm_snapshot::{self, CellSnapshot};

    /// Whether a program slot is `varip` (its cells survive rollback).
    pub(crate) trait SlotKind {
        fn is_varip(&self) -> bool;
    }

    impl SlotKind for pinewasm_csil::SlotDef {
        fn is_varip(&self) -> bool {
            self.varip
        }
    }

    /// A clone-on-snapshot of all non-`varip` state that a realtime tick
    /// may mutate, taken at the realtime bar's start and restored (rollback)
    /// before each subsequent tick on that bar. `varip` slots are
    /// deliberately excluded — they persist across ticks.
    ///
    /// Snapshot allocations include duplicate slot/output cells, retained TA-engine
    /// payload, the heap arena (including collected slots), drawing/table state,
    /// and alert-dedup state. They are outside the deterministic VM byte counter.
    /// Other collection/string/object and drawing limits constrain portions of the
    /// source state, but snapshot duplication can make peak process memory exceed
    /// [`crate::RunOptions::max_vm_bytes_per_run`].
    ///
    /// The VM instantiates it as `StateSnapshot` (`T = Value`, `Ta = TaStates`,
    /// `H = Heap`, `D = DrawingRegistry`, `S = HashSet<usize>`).
    #[derive(Clone)]
    pub(crate) struct Snapshot<T, Ta, H, D, S> {
        /// The per-cell state: the current cell of every non-`varip` user slot
        /// (a placeholder for `varip` slots), per-slot written flags (a
        /// realtime tick may set them; rollback restores the non-`varip` ones),
        /// and the bar's plot, plot-color, visual-color and fill-color cells.
        pub(crate) cells: CellSnapshot<T>,
        /// A clone of every ta engine state.
        pub(crate) ta_state: Ta,
        /// `math.random` draw counter (so a rolled-back tick redraws identically).
        pub(crate) random_draws: u64,
        /// The bar-start heap. Restore overlays mutations to preexisting
        /// objects reachable from `varip` roots and remaps only reachable
        /// post-snapshot objects; unreachable tick temporaries are discarded.
        /// Snapshot cloning is O(heap size), and root graph traversal adds
        /// O(reachable objects and contained values) per rollback.
        pub(crate) heap: H,
        /// A clone of the live drawing registry: a rolled-back realtime tick's
        /// drawing creations/deletions must be undone. Cost is O(live
        /// drawings) per tick.
        pub(crate) drawings: D,
        /// Test-only log length at snapshot time.
        /// Logs are cumulative observations, not rollback state.
        #[cfg(test)]
        pub(crate) logs_len: usize,
        /// The alert length and once-per-bar fired-site set at snapshot time.
        /// Their existing rollback policy is independent of log retention.
        pub(crate) alerts_len: usize,
        pub(crate) alert_fired_sites: S,
    }

    /// One flag per program slot: is it `varip`?
    pub(crate) fn varip_mask<K: SlotKind>(defs: &[K]) -> Vec<bool> {
        let mut mask = Vec::new();
        let mut i = 0;
        while i < defs.len() {
            mask.push(defs[i].is_varip());
            i += 1;
        }
        mask
    }

    /// The body of `Vm::snapshot`: capture the cells of bar `bar` (user slots
    /// at ring cell `bar & slot_mask`, `placeholder` for `varip` slots) and a
    /// clone of the non-cell state.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn snapshot_state<
        T: Clone,
        K: SlotKind,
        Ta: Clone,
        H: Clone,
        D: Clone,
        S: Clone,
        L,
        A,
    >(
        defs: &[K],
        slot_mask: usize,
        bar: usize,
        user_slots: &[Vec<T>],
        written: &[bool],
        plots: &[Vec<T>],
        plot_colors: &[Vec<Vec<T>>],
        visual_colors: &[Vec<Vec<T>>],
        fill_colors: &[Vec<Vec<T>>],
        placeholder: &T,
        ta_state: &Ta,
        random_draws: u64,
        heap: &H,
        drawings: &D,
        reference_logs: &[L],
        alerts: &[A],
        alert_fired_sites: &S,
    ) -> Snapshot<T, Ta, H, D, S> {
        // Keep the helper's argument shape;
        // production snapshots do not retain a log checkpoint.
        #[cfg(not(test))]
        let _ = reference_logs;
        let varip = varip_mask(defs);
        let cells = vm_snapshot::snapshot_cells(
            user_slots,
            written,
            plots,
            plot_colors,
            visual_colors,
            fill_colors,
            &varip,
            bar & slot_mask,
            bar,
            placeholder,
        );
        Snapshot {
            cells,
            ta_state: ta_state.clone(),
            random_draws,
            heap: heap.clone(),
            drawings: drawings.clone(),
            #[cfg(test)]
            logs_len: reference_logs.len(),
            alerts_len: alerts.len(),
            alert_fired_sites: alert_fired_sites.clone(),
        }
    }

    /// The body of `Vm::restore`: restore the captured cells of bar `bar`
    /// (non-`varip` user slots at ring cell `bar & slot_mask`, their written
    /// flags, and every plot/color cell), then the non-cell state.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn restore_state<T: Clone, K: SlotKind, Ta: Clone, H, D: Clone, S: Clone, A>(
        defs: &[K],
        slot_mask: usize,
        bar: usize,
        user_slots: &mut [Vec<T>],
        written: &mut [bool],
        plots: &mut [Vec<T>],
        plot_colors: &mut [Vec<Vec<T>>],
        visual_colors: &mut [Vec<Vec<T>>],
        fill_colors: &mut [Vec<Vec<T>>],
        ta_state: &mut Ta,
        random_draws: &mut u64,
        heap: &mut H,
        drawings: &mut D,
        alerts: &mut Vec<A>,
        alert_fired_sites: &mut S,
        snap: &Snapshot<T, Ta, H, D, S>,
        restored_heap: H,
    ) {
        let varip = varip_mask(defs);
        vm_snapshot::restore_cells(
            user_slots,
            written,
            plots,
            plot_colors,
            visual_colors,
            fill_colors,
            &varip,
            bar & slot_mask,
            bar,
            &snap.cells,
        );
        restore_non_cell_state(
            ta_state,
            random_draws,
            heap,
            drawings,
            alerts,
            alert_fired_sites,
            snap,
            restored_heap,
        );
    }

    /// The non-cell half of [`restore_state`]: TA engines, random draws,
    /// heap, drawings and alert state. Log events survive rollback; the
    /// alert rollback and once-per-bar dedup policies remain unchanged.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn restore_non_cell_state<T, Ta: Clone, H, D: Clone, S: Clone, A>(
        ta_state: &mut Ta,
        random_draws: &mut u64,
        heap: &mut H,
        drawings: &mut D,
        alerts: &mut Vec<A>,
        alert_fired_sites: &mut S,
        snap: &Snapshot<T, Ta, H, D, S>,
        restored_heap: H,
    ) {
        *ta_state = snap.ta_state.clone();
        *random_draws = snap.random_draws;
        *heap = restored_heap;
        *drawings = snap.drawings.clone();
        alerts.truncate(snap.alerts_len);
        *alert_fired_sites = snap.alert_fired_sites.clone();
    }
}

/// The heap-growing array arms (`array.push`/`unshift`/`insert`/`set`) and
/// the `matrix.new` constructor as free kernels over the heap and the
/// run-cumulative string counter. The handlers only resolve their arguments,
/// call the kernel and map a [`vm_heap_ops::HeapOpError`] to the
/// `RuntimeError` (same kind and message) the inline code raised.
///
/// Every check that can fail without an effect (receiver, index, element
/// coercion) runs before the first charge, and a charge taken before a later
/// failure is given back, so a failed call leaves the heap *and* the string
/// counter exactly as they were. Detecting an element type mismatch after
/// `reserve_string_bytes`, or failing the object budget in `matrix.new`, must
/// therefore release any reservation already taken.
/// Extracted by Aeneas.
// Explicit `match`/`if let` instead of `?`: the Aeneas extraction has no model
// for `Try` (the same exemption as `pinewasm_mir::Heap::alloc`).
#[allow(clippy::question_mark)]
pub(crate) mod vm_heap_ops {
    use pinewasm_mir::{ElemType, HandleId, Heap, HeapLimitError, HeapObj, Value};

    /// The shape of a [`Value`] the kernels inspect. [`value_view`] is the
    /// only function that reads a `Value` (its `f64` payload has no Aeneas
    /// translation, so it is opaque at extraction).
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum ValueView {
        Na,
        Bool,
        Int(i64),
        /// `f64::to_bits` of the payload.
        Float(u64),
        /// `String::len` (UTF-8 bytes) of the payload.
        Str(usize),
        Color,
        Ref(HandleId),
    }

    /// Classify a value.
    #[inline]
    pub(crate) fn value_view(value: &Value) -> ValueView {
        match value {
            Value::Na | Value::DisplacedTable(_) => ValueView::Na,
            Value::Bool(_) => ValueView::Bool,
            Value::Int(i) => ValueView::Int(*i),
            Value::Float(f) => ValueView::Float(f.to_bits()),
            Value::String(s) => ValueView::Str(s.len()),
            Value::Color { .. } => ValueView::Color,
            Value::Ref(h) => ValueView::Ref(*h),
        }
    }

    /// `f as i64` of a float index (saturating, `NaN` is 0).
    #[inline]
    pub(crate) fn float_index(bits: u64) -> i64 {
        f64::from_bits(bits) as i64
    }

    /// Int-to-float element widening, `Value::from_f64(i as f64)`.
    #[inline]
    pub(crate) fn widen_int(i: i64) -> Value {
        Value::from_f64(i as f64)
    }

    /// Why a kernel failed. `Vm::heap_op_error` maps each variant to the
    /// `RuntimeErrorKind` and message of the inline code it replaced.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) enum HeapOpError {
        /// The receiver is `na` (`NaMethodReceiver`).
        NaReceiver,
        /// The receiver is neither a `Ref` nor `na` (`TypeMismatch`).
        NotCollection,
        /// The receiver is not a live array (`InvalidProgram`).
        NotArray,
        /// The index argument is not a number (`CollectionIndexOutOfBounds`).
        IndexNa,
        /// The index is outside the accepted range (`CollectionIndexOutOfBounds`).
        OutOfBounds { index: i64, len: usize },
        /// The element does not conform to the element type (`TypeMismatch`).
        ElemMismatch(ElemType),
        /// A heap element or object budget (`CollectionLimitExceeded`).
        Heap(HeapLimitError),
        /// The cumulative string budget (`StringLimitExceeded`).
        StringLimit { attempted: usize },
        /// A repeated string clone overflows `usize` (`StringLimitExceeded`).
        StringCloneOverflow,
        /// A matrix dimension does not fit `usize` (`CollectionLimitExceeded`).
        DimTooLarge,
        /// A negative matrix dimension (`CollectionIndexOutOfBounds`).
        DimNegative,
        /// A non-integer matrix dimension (`CollectionIndexOutOfBounds`).
        DimNa,
        /// `rows * cols` overflows `usize` (`CollectionLimitExceeded`).
        CountOverflow,
    }

    /// `receiver_handle`: the receiver's heap handle.
    pub(crate) fn receiver(value: &Value) -> Result<HandleId, HeapOpError> {
        match value_view(value) {
            ValueView::Ref(h) => Ok(h),
            ValueView::Na => Err(HeapOpError::NaReceiver),
            _ => Err(HeapOpError::NotCollection),
        }
    }

    /// `array_meta`: the element type and length of the array at `h`.
    pub(crate) fn array_meta(heap: &Heap, h: HandleId) -> Result<(ElemType, usize), HeapOpError> {
        match heap.get(h) {
            Some(HeapObj::Array { elem_ty, items }) => Ok((*elem_ty, items.len())),
            _ => Err(HeapOpError::NotArray),
        }
    }

    /// `index_arg`: an int index, or a truncated float index (defense in
    /// depth; the checker types indices as int).
    pub(crate) fn index_arg(value: &Value) -> Result<i64, HeapOpError> {
        match value_view(value) {
            ValueView::Int(i) => Ok(i),
            ValueView::Float(bits) => Ok(float_index(bits)),
            _ => Err(HeapOpError::IndexNa),
        }
    }

    /// `check_access_index`: `index` must lie in `[-len, len)`; a negative
    /// index counts from the end.
    pub(crate) fn check_access(index: i64, len: usize) -> Result<usize, HeapOpError> {
        match crate::array_get::resolved_index(index, len) {
            Some(i) => Ok(i),
            None => Err(HeapOpError::OutOfBounds { index, len }),
        }
    }

    /// `coerce_element`: int widens to float for a float element type, `na`
    /// passes, a conforming value passes unchanged, anything else is a
    /// mismatch.
    pub(crate) fn coerce(elem_ty: ElemType, value: Value) -> Result<Value, HeapOpError> {
        let ok = match (elem_ty, value_view(&value)) {
            (_, ValueView::Na) => true,
            (ElemType::Float, ValueView::Float(_)) => true,
            (ElemType::Float, ValueView::Int(i)) => return Ok(widen_int(i)),
            (ElemType::Int, ValueView::Int(_)) => true,
            (ElemType::Bool, ValueView::Bool) => true,
            (ElemType::String, ValueView::Str(_)) => true,
            (ElemType::Color, ValueView::Color) => true,
            // A UDT element: any object reference (the checker enforced the
            // concrete type).
            (ElemType::Object(_), ValueView::Ref(_)) => true,
            _ => false,
        };
        if ok {
            Ok(value)
        } else {
            Err(HeapOpError::ElemMismatch(elem_ty))
        }
    }

    /// Bytes allocated by cloning `value` once.
    pub(crate) fn clone_bytes(value: &Value) -> usize {
        match value_view(value) {
            ValueView::Str(n) => n,
            _ => 0,
        }
    }

    /// Bytes allocated by cloning `value` `copies` times.
    pub(crate) fn repeated_clone_bytes(value: &Value, copies: usize) -> Result<usize, HeapOpError> {
        match value_view(value) {
            ValueView::Str(n) => match n.checked_mul(copies) {
                Some(bytes) => Ok(bytes),
                None => Err(HeapOpError::StringCloneOverflow),
            },
            _ => Ok(0),
        }
    }

    /// `reserve_string_bytes`: charge `bytes` against the cumulative string
    /// budget `max`. Cumulative by design (never refunded after success);
    /// on failure the counter is unchanged and the error is the attempted
    /// total.
    pub(crate) fn reserve_string_bytes(
        allocated: &mut usize,
        max: usize,
        bytes: usize,
    ) -> Result<(), usize> {
        let attempted = allocated.saturating_add(bytes);
        if attempted > max {
            return Err(attempted);
        }
        *allocated = attempted;
        Ok(())
    }

    /// `heap.get_mut(h)` narrowed to an array's items (opaque at extraction:
    /// Aeneas has one model of `Option`'s `Try::branch`, and `Heap::get_mut`
    /// uses it through a mutable borrow).
    #[inline]
    pub(crate) fn array_items_mut(heap: &mut Heap, h: HandleId) -> Option<&mut Vec<Value>> {
        match heap.get_mut(h) {
            Some(HeapObj::Array { items, .. }) => Some(items),
            _ => None,
        }
    }

    /// `items.insert(index, value)` (opaque at extraction: the Aeneas library
    /// models `Vec::insert` as an overwrite that rejects `index == len`, so
    /// the extraction uses a hand model of the real shifting insert).
    #[inline]
    pub(crate) fn vec_insert(items: &mut Vec<Value>, index: usize, value: Value) {
        items.insert(index, value);
    }

    /// Where [`grow_array`] puts the new element.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum Place {
        /// `items.push(v)`.
        Back,
        /// `items.insert(0, v)`.
        Front,
        /// `items.insert(i, v)`, with `i <= len` checked by the caller.
        At(usize),
    }

    /// The shared growth sequence of `push`/`unshift`/`insert` on the live
    /// array `h` of element type `elem_ty`: coerce, charge one cell, charge
    /// the string clone (giving the cell back on failure), write.
    pub(crate) fn grow_array(
        heap: &mut Heap,
        allocated: &mut usize,
        max_string: usize,
        h: HandleId,
        elem_ty: ElemType,
        value: &Value,
        place: Place,
    ) -> Result<(), HeapOpError> {
        let v = match coerce(elem_ty, value.clone()) {
            Ok(v) => v,
            Err(error) => return Err(error),
        };
        let bytes = clone_bytes(value);
        if let Err(error) = heap.reserve(1) {
            return Err(HeapOpError::Heap(error));
        }
        if let Err(attempted) = reserve_string_bytes(allocated, max_string, bytes) {
            heap.release(1);
            return Err(HeapOpError::StringLimit { attempted });
        }
        if matches!(place, Place::Back) {
            heap.append_array_value(h, v);
        } else if let Some(items) = array_items_mut(heap, h) {
            match place {
                Place::Back => unreachable!("append handled above"),
                Place::Front => vec_insert(items, 0, v),
                Place::At(i) => vec_insert(items, i, v),
            }
        }
        Ok(())
    }

    /// `array.push(recv, value)`.
    pub(crate) fn array_push(
        heap: &mut Heap,
        allocated: &mut usize,
        max_string: usize,
        recv: &Value,
        value: &Value,
    ) -> Result<(), HeapOpError> {
        let h = match receiver(recv) {
            Ok(h) => h,
            Err(error) => return Err(error),
        };
        let elem_ty = match array_meta(heap, h) {
            Ok((elem_ty, _)) => elem_ty,
            Err(error) => return Err(error),
        };
        grow_array(heap, allocated, max_string, h, elem_ty, value, Place::Back)
    }

    /// `array.unshift(recv, value)`.
    pub(crate) fn array_unshift(
        heap: &mut Heap,
        allocated: &mut usize,
        max_string: usize,
        recv: &Value,
        value: &Value,
    ) -> Result<(), HeapOpError> {
        let h = match receiver(recv) {
            Ok(h) => h,
            Err(error) => return Err(error),
        };
        let elem_ty = match array_meta(heap, h) {
            Ok((elem_ty, _)) => elem_ty,
            Err(error) => return Err(error),
        };
        grow_array(heap, allocated, max_string, h, elem_ty, value, Place::Front)
    }

    /// `array.insert(recv, index, value)`: the index must lie in `[0, len]`
    /// (append at the end).
    pub(crate) fn array_insert(
        heap: &mut Heap,
        allocated: &mut usize,
        max_string: usize,
        recv: &Value,
        index: &Value,
        value: &Value,
    ) -> Result<(), HeapOpError> {
        let h = match receiver(recv) {
            Ok(h) => h,
            Err(error) => return Err(error),
        };
        let (elem_ty, len) = match array_meta(heap, h) {
            Ok(meta) => meta,
            Err(error) => return Err(error),
        };
        let idx = match index_arg(index) {
            Ok(idx) => idx,
            Err(error) => return Err(error),
        };
        if idx < 0 || idx as usize > len {
            return Err(HeapOpError::OutOfBounds { index: idx, len });
        }
        grow_array(
            heap,
            allocated,
            max_string,
            h,
            elem_ty,
            value,
            Place::At(idx as usize),
        )
    }

    /// `array.set(recv, index, value)`: no cell charge; coerce, then charge
    /// the string clone, then write.
    pub(crate) fn array_set(
        heap: &mut Heap,
        allocated: &mut usize,
        max_string: usize,
        recv: &Value,
        index: &Value,
        value: &Value,
    ) -> Result<(), HeapOpError> {
        let h = match receiver(recv) {
            Ok(h) => h,
            Err(error) => return Err(error),
        };
        let (elem_ty, len) = match array_meta(heap, h) {
            Ok(meta) => meta,
            Err(error) => return Err(error),
        };
        let idx = match index_arg(index) {
            Ok(idx) => idx,
            Err(error) => return Err(error),
        };
        let i = match check_access(idx, len) {
            Ok(i) => i,
            Err(error) => return Err(error),
        };
        let v = match coerce(elem_ty, value.clone()) {
            Ok(v) => v,
            Err(error) => return Err(error),
        };
        if let Err(attempted) = reserve_string_bytes(allocated, max_string, clone_bytes(value)) {
            return Err(HeapOpError::StringLimit { attempted });
        }
        if let Some(items) = array_items_mut(heap, h) {
            items[i] = v;
        }
        Ok(())
    }

    /// `array.new<T>(size, initial)`: the size is already decoded. Preserve
    /// the source order: non-charging element-capacity preflight, repeated
    /// string-byte projection, element coercion, cumulative string charge,
    /// then allocation (which charges the cells). If allocation rejects the
    /// object after the string charge, restore the prior counter.
    pub(crate) fn array_new(
        heap: &mut Heap,
        allocated: &mut usize,
        max_string: usize,
        size: usize,
        elem_ty: ElemType,
        initial_arg: &Value,
    ) -> Result<HandleId, HeapOpError> {
        if let Err(error) = heap.reserve(size) {
            return Err(HeapOpError::Heap(error));
        }
        heap.release(size);

        let bytes = match repeated_clone_bytes(initial_arg, size) {
            Ok(bytes) => bytes,
            Err(error) => return Err(error),
        };
        let initial = match coerce(elem_ty, initial_arg.clone()) {
            Ok(value) => value,
            Err(error) => return Err(error),
        };
        let before = *allocated;
        if let Err(attempted) = reserve_string_bytes(allocated, max_string, bytes) {
            return Err(HeapOpError::StringLimit { attempted });
        }
        let items = vec![initial; size];
        match heap.alloc(HeapObj::Array { elem_ty, items }) {
            Ok(handle) => Ok(handle),
            Err(error) => {
                *allocated = before;
                Err(HeapOpError::Heap(error))
            }
        }
    }

    /// `matrix_dim_arg`: a non-negative int dimension.
    pub(crate) fn matrix_dim(value: &Value) -> Result<usize, HeapOpError> {
        match value_view(value) {
            ValueView::Int(i) => {
                if i >= 0 {
                    match usize::try_from(i) {
                        Ok(n) => Ok(n),
                        Err(_) => Err(HeapOpError::DimTooLarge),
                    }
                } else {
                    Err(HeapOpError::DimNegative)
                }
            }
            _ => Err(HeapOpError::DimNa),
        }
    }

    /// The element type of `matrix.new`'s tag argument (`float` when the tag
    /// is not an int or not a valid tag).
    pub(crate) fn matrix_elem_ty(tag: &Value) -> ElemType {
        let t = match value_view(tag) {
            ValueView::Int(t) => t,
            _ => 0,
        };
        match ElemType::from_tag(t) {
            Some(elem_ty) => elem_ty,
            None => ElemType::Float,
        }
    }

    /// `matrix.new(rows, cols, initial, elem_ty_tag)`: dimensions, element
    /// count, coercion, a non-charging capacity preflight, the string charge,
    /// then the allocation (which charges the cells); an allocation failure
    /// gives the string charge back.
    pub(crate) fn matrix_new(
        heap: &mut Heap,
        allocated: &mut usize,
        max_string: usize,
        rows_arg: &Value,
        cols_arg: &Value,
        initial_arg: &Value,
        tag_arg: &Value,
    ) -> Result<HandleId, HeapOpError> {
        let rows = match matrix_dim(rows_arg) {
            Ok(n) => n,
            Err(error) => return Err(error),
        };
        let cols = match matrix_dim(cols_arg) {
            Ok(n) => n,
            Err(error) => return Err(error),
        };
        let elem_ty = matrix_elem_ty(tag_arg);
        let count = match rows.checked_mul(cols) {
            Some(count) => count,
            None => return Err(HeapOpError::CountOverflow),
        };
        let initial = match coerce(elem_ty, initial_arg.clone()) {
            Ok(v) => v,
            Err(error) => return Err(error),
        };
        // Non-charging preflight: `Heap::alloc` charges the cells itself.
        if let Err(error) = heap.reserve(count) {
            return Err(HeapOpError::Heap(error));
        }
        heap.release(count);
        let bytes = match repeated_clone_bytes(initial_arg, count) {
            Ok(bytes) => bytes,
            Err(error) => return Err(error),
        };
        let before = *allocated;
        if let Err(attempted) = reserve_string_bytes(allocated, max_string, bytes) {
            return Err(HeapOpError::StringLimit { attempted });
        }
        let items = vec![initial; count];
        match heap.alloc(HeapObj::Matrix {
            elem_ty,
            rows,
            cols,
            items,
        }) {
            Ok(h) => Ok(h),
            Err(error) => {
                *allocated = before;
                Err(HeapOpError::Heap(error))
            }
        }
    }
}

/// The root enumeration of [`Vm::gc_roots`] over the VM's `Value` stores:
/// every ring cell of every user slot, the operand stack, the pending call
/// arguments, every live drawing handle, and (per table) the table handle and
/// its cells' attribute values. Extracted by Aeneas;
/// `Vm::gc_roots` only walks the table grids' `BTreeMap`s.
// `&Option<Value>` mirrors the destructured `TableCell` fields one-to-one.
#[allow(clippy::ref_option)]
pub(crate) mod vm_gc_roots {
    use super::vm_heap_ops::{value_view, ValueView};
    use super::TableCell;
    use pinewasm_mir::{HandleId, Value};

    /// Push `v`'s handle if it is a `Ref`.
    #[inline]
    pub(crate) fn push_value_root(roots: &mut Vec<HandleId>, v: &Value) {
        if let ValueView::Ref(h) = value_view(v) {
            roots.push(h);
        }
    }

    /// Push the handle of every `Ref` in `values`, in order.
    pub(crate) fn push_values_roots(roots: &mut Vec<HandleId>, values: &[Value]) {
        let mut i = 0;
        while i < values.len() {
            push_value_root(roots, &values[i]);
            i += 1;
        }
    }

    /// The roots held by the slots, the stack, the call arguments and the
    /// drawing registry's live handles, in that order.
    pub(crate) fn gc_roots_core(
        user_slots: &[Vec<Value>],
        stack: &[Value],
        call_args: &[Value],
        live: &[Vec<HandleId>],
    ) -> Vec<HandleId> {
        let mut roots = Vec::new();
        // Every ring cell, not just the current one: history reads reach back
        // `history_ring()` bars, so those cells are live references.
        let mut s = 0;
        while s < user_slots.len() {
            push_values_roots(&mut roots, &user_slots[s]);
            s += 1;
        }
        push_values_roots(&mut roots, stack);
        push_values_roots(&mut roots, call_args);
        let mut k = 0;
        while k < live.len() {
            let kind = &live[k];
            let mut j = 0;
            while j < kind.len() {
                roots.push(kind[j]);
                j += 1;
            }
            k += 1;
        }
        roots
    }

    /// Push the handle of every `Ref` attribute of a table cell, in field
    /// order. Destructured rather than field-listed so adding an attribute is
    /// a compile error here until it is accounted for.
    pub(crate) fn push_cell_roots(roots: &mut Vec<HandleId>, cell: &TableCell) {
        let TableCell {
            text,
            width,
            height,
            text_color,
            text_halign,
            text_valign,
            text_size,
            bgcolor,
            tooltip,
            text_font_family,
            text_formatting,
            merged: _,
            merge: _,
        } = cell;
        push_opt_root(roots, text);
        push_opt_root(roots, width);
        push_opt_root(roots, height);
        push_opt_root(roots, text_color);
        push_opt_root(roots, text_halign);
        push_opt_root(roots, text_valign);
        push_opt_root(roots, text_size);
        push_opt_root(roots, bgcolor);
        push_opt_root(roots, tooltip);
        push_opt_root(roots, text_font_family);
        push_opt_root(roots, text_formatting);
    }

    /// Push the handle of an optional attribute if it is a `Ref`.
    #[inline]
    fn push_opt_root(roots: &mut Vec<HandleId>, v: &Option<Value>) {
        if let Some(v) = v {
            push_value_root(roots, v);
        }
    }
}

/// Integer-arithmetic kernels of the `IntAdd`..`IntRem` and `IntNeg`
/// handlers: `na` propagation plus Rust's checked `i64` operations, where
/// overflow and division/remainder by zero yield `na` (`None`). The handler
/// only pops the operands and pushes `map_or(Value::Na, Value::Int)`.
/// Extracted by Aeneas.
pub(crate) mod vm_int {
    /// The five binary integer instructions.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum IntBinOp {
        Add,
        Sub,
        Mul,
        Div,
        Rem,
    }

    /// `lhs op rhs` on `na`-able `i64` operands. Any `na` operand gives
    /// `na`; so do overflow and division/remainder by zero. Division
    /// truncates toward zero; the remainder takes the sign of the dividend.
    #[inline]
    pub(crate) fn int_binop(op: IntBinOp, lhs: Option<i64>, rhs: Option<i64>) -> Option<i64> {
        match (lhs, rhs) {
            (Some(l), Some(r)) => match op {
                IntBinOp::Add => l.checked_add(r),
                IntBinOp::Sub => l.checked_sub(r),
                IntBinOp::Mul => l.checked_mul(r),
                IntBinOp::Div => l.checked_div(r),
                // `i64::MIN % -1` overflows, so `checked_rem` is `None` there.
                // The explicit guard is redundant in Rust but keeps the
                // extracted code independent of the Aeneas std model of
                // signed `checked_rem`, which returns
                // `Some(0)` for it.
                IntBinOp::Rem => {
                    if l == i64::MIN && r == -1 {
                        None
                    } else {
                        l.checked_rem(r)
                    }
                }
            },
            _ => None,
        }
    }

    /// `-operand`; negating `i64::MIN` overflows, giving `na`.
    #[inline]
    pub(crate) fn int_neg(operand: Option<i64>) -> Option<i64> {
        match operand {
            Some(v) => v.checked_neg(),
            None => None,
        }
    }
}

/// The dynamic-history-offset kernel of `ReadUserSlotDynamic`: decode the
/// popped offset (only a non-negative `Value::Int` is an offset; anything
/// else reads `na`), enforce `max_bars_back`, and locate the ring cell.
/// Extracted by Aeneas.
pub(crate) mod vm_offset {
    /// Outcome of a dynamic history read before the slot lookup.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum DynamicRead {
        /// The offset is not a non-negative integer, or reaches before bar 0.
        Na,
        /// The offset exceeds `max_bars_back`: a structured runtime error.
        TooDeep(u64),
        /// The ring cell holding the value `offset` bars back.
        Cell(usize),
    }

    /// Decode the whole-integer payload; Pine int division may also carry a
    /// fractional runtime value, which the VM truncates before this kernel.
    #[inline]
    pub(crate) fn decode_offset(raw: Option<i64>) -> Option<u64> {
        match raw {
            Some(v) => {
                if v >= 0 {
                    Some(v as u64)
                } else {
                    None
                }
            }
            None => None,
        }
    }

    /// The whole pre-lookup read: decode, the `max_bars_back` check, then
    /// the ring cell of `bar - offset` (`na` before bar 0).
    #[inline]
    pub(crate) fn dynamic_read(
        raw: Option<i64>,
        limit: u32,
        bar: usize,
        mask: usize,
    ) -> DynamicRead {
        dynamic_read_decoded(decode_offset(raw), limit, bar, mask)
    }

    /// The same ring lookup after the VM has decoded a possibly fractional
    /// Pine-int offset. Keeping the whole-integer wrapper preserves the
    /// extracted kernel's test surface.
    pub(crate) fn dynamic_read_decoded(
        decoded: Option<u64>,
        limit: u32,
        bar: usize,
        mask: usize,
    ) -> DynamicRead {
        match decoded {
            None => DynamicRead::Na,
            Some(offset) => {
                if offset > limit as u64 {
                    DynamicRead::TooDeep(offset)
                } else {
                    match crate::vm_charge::ring_read(bar, offset as usize, mask) {
                        Some(cell) => DynamicRead::Cell(cell),
                        None => DynamicRead::Na,
                    }
                }
            }
        }
    }
}

/// The realtime tick driver's control state and its transitions (the
/// open/re-run/rollback decision, the ordinal, and the commit on a closing
/// tick), generic over the snapshot type, plus the `math.random` draw
/// kernel whose counter the snapshot restores. `Vm::run_realtime` performs
/// the side effects (append, carry-forward, snapshot, restore, execution)
/// in between. Extracted by Aeneas.
pub(crate) mod vm_tick {
    /// Cross-request realtime rollback and commit state.
    pub(crate) struct TickDriver<S> {
        pub(crate) open_bar: Option<usize>,
        pub(crate) open_bar_appended: bool,
        pub(crate) bar_snapshot: Option<S>,
        pub(crate) next_tick_ordinal: u32,
    }

    /// The idle driver of a fresh run (no open bar, ordinal 0).
    pub(crate) fn new_driver<S>() -> TickDriver<S> {
        TickDriver {
            open_bar: None,
            open_bar_appended: false,
            bar_snapshot: None,
            next_tick_ordinal: 0,
        }
    }

    impl<S> Default for TickDriver<S> {
        fn default() -> Self {
            new_driver()
        }
    }

    /// What a tick does before executing.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum TickStart {
        /// First tick on `bar == bars`: append a bar and open it.
        Append,
        /// First tick on the last committed bar: re-run (open) it.
        RerunLast,
        /// A later tick on the open bar: roll back to its snapshot.
        Rollback,
    }

    /// The start decision for a tick on `bar` with `bars` bars. `None` is
    /// the structured `InvalidProgram` error (nothing has changed yet).
    /// A first tick records whether it appends, exactly where the driver
    /// always did: before the append can fail.
    pub(crate) fn begin_tick<S>(
        driver: &mut TickDriver<S>,
        bars: usize,
        bar: usize,
    ) -> Option<TickStart> {
        let first_tick_on_bar = match driver.open_bar {
            // An open bar is committed only by its `is_bar_close` tick. A
            // tick on any other bar would abandon it uncommitted.
            Some(open) if open != bar => return None,
            Some(_) => false,
            None => true,
        };
        if first_tick_on_bar {
            let last_committed = bars.saturating_sub(1);
            let opening_new_bar = bar == bars;
            let rerunning_last = bar == last_committed && bars > 0;
            if !opening_new_bar && !rerunning_last {
                return None;
            }
            driver.open_bar_appended = opening_new_bar;
            if opening_new_bar {
                Some(TickStart::Append)
            } else {
                Some(TickStart::RerunLast)
            }
        } else {
            Some(TickStart::Rollback)
        }
    }

    /// Record the bar-start snapshot of the bar a first tick just opened.
    pub(crate) fn open_bar<S>(driver: &mut TickDriver<S>, bar: usize, snapshot: S) {
        driver.bar_snapshot = Some(snapshot);
        driver.open_bar = Some(bar);
    }

    /// After the tick executed: advance the ordinal and, on a closing tick,
    /// commit (an appended bar counts toward `bars_executed`) and close the
    /// bar. Returns whether the bar closed.
    pub(crate) fn finish_tick<S>(
        driver: &mut TickDriver<S>,
        is_bar_close: bool,
        bars_executed: &mut u32,
    ) -> bool {
        driver.next_tick_ordinal = driver.next_tick_ordinal.saturating_add(1);
        if is_bar_close {
            if driver.open_bar_appended {
                *bars_executed += 1;
            }
            driver.open_bar = None;
            driver.open_bar_appended = false;
            driver.bar_snapshot = None;
        }
        is_bar_close
    }

    /// The `index`-th output of a splitmix64 stream seeded with `key`.
    /// Splitmix64 is counter-based, so the n-th output is O(1): the internal
    /// state after n+1 steps is `key + (n+1) * GOLDEN`, then the finalizer
    /// mixes it.
    #[inline]
    pub(crate) fn splitmix64_at(key: u64, index: u64) -> u64 {
        let mut z = key.wrapping_add(index.wrapping_add(1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// One `math.random` draw: the `draws`-th output of the `key` stream,
    /// and the advanced draw counter.
    #[inline]
    pub(crate) fn random_draw(key: u64, draws: u64) -> (u64, u64) {
        (splitmix64_at(key, draws), draws + 1)
    }
}

/// The `WINDOW_MEAN` kernel (`ta.sma` lowers to a hidden series slot plus
/// this instruction). Generic over the float type and the cell type so the
/// Aeneas toolchain can extract it; the
/// VM instantiates it at `f64` and `Value`. The caller keeps the metering,
/// the slot lookup, the length-0 rejection, the error messages and the final
/// `Value::from_f64`.
pub(crate) mod vm_window {
    /// The float operations the kernel needs; the single instance is `f64`.
    pub(crate) trait MeanFloat: Copy {
        /// `0.0`.
        fn zero() -> Self;
        /// `self + other`.
        fn add(self, other: Self) -> Self;
        /// `self / other`.
        fn div(self, other: Self) -> Self;
        /// `f64::from(v)` (exact).
        fn from_u32(v: u32) -> Self;
    }

    /// A ring cell as the kernel sees it: a number (a `Float`, or an `Int`
    /// widened to float), `na`, or any other value (a type mismatch).
    pub(crate) enum MeanCell<F> {
        Num(F),
        Na,
        Other,
    }

    /// How the VM's cell type is viewed by the kernel.
    pub(crate) trait MeanView<F> {
        fn view(&self) -> MeanCell<F>;
    }

    /// The kernel's result: `na` (fewer than `length` bars, or an `na` cell),
    /// the raw mean `sum / length` (before `Value::from_f64`), or the ring
    /// index of the first non-numeric cell met before any `na`.
    pub(crate) enum MeanOut<F> {
        Na,
        Mean(F),
        Mismatch(usize),
    }

    /// Running sum over one contiguous segment of the ring.
    enum Summed<F> {
        Sum(F),
        Na,
        Mismatch(usize),
    }

    /// `sum += cell` for every cell of `cells`, oldest first; stop at the first
    /// `na` or non-numeric cell (`base` is the segment's first ring index).
    fn add_cells<F: MeanFloat, C: MeanView<F>>(cells: &[C], base: usize, sum: F) -> Summed<F> {
        let mut acc = sum;
        let mut i = 0usize;
        while i < cells.len() {
            match cells[i].view() {
                MeanCell::Num(v) => acc = acc.add(v),
                MeanCell::Na => return Summed::Na,
                MeanCell::Other => return Summed::Mismatch(base + i),
            }
            i += 1;
        }
        Summed::Sum(acc)
    }

    /// The window covers bars `bar + 1 - length ..= bar`: `na` while fewer
    /// than `length` bars exist, `na` if any cell is `na`. The live window
    /// occupies at most two contiguous slices of the ring; the cells are
    /// added in chronological order across the wrap. `length > 0` (the
    /// caller rejects 0).
    pub(crate) fn window_mean<F: MeanFloat, C: MeanView<F>>(
        history: &[C],
        bar: usize,
        length: u32,
        mask: usize,
    ) -> MeanOut<F> {
        let len = length as usize;
        if bar + 1 < len {
            return MeanOut::Na;
        }
        let start = (bar + 1 - len) & mask;
        let room = history.len() - start;
        let first_len = if len < room { len } else { room };
        let first = &history[start..start + first_len];
        let second = &history[..len - first_len];
        match add_cells(first, start, F::zero()) {
            Summed::Na => MeanOut::Na,
            Summed::Mismatch(index) => MeanOut::Mismatch(index),
            Summed::Sum(partial) => match add_cells(second, 0, partial) {
                Summed::Na => MeanOut::Na,
                Summed::Mismatch(index) => MeanOut::Mismatch(index),
                Summed::Sum(sum) => MeanOut::Mean(sum.div(F::from_u32(length))),
            },
        }
    }

    /// The shipped primitives. Opaque in the extraction
    /// (`--opaque=pinewasm_vm::vm_window::window_f64_ops`); the extracted code
    /// reads them as the binary64 instance row.
    mod window_f64_ops {
        #[inline(always)]
        pub(super) fn zero() -> f64 {
            0.0
        }
        #[inline(always)]
        pub(super) fn add(a: f64, b: f64) -> f64 {
            a + b
        }
        #[inline(always)]
        pub(super) fn div(a: f64, b: f64) -> f64 {
            a / b
        }
        #[inline(always)]
        pub(super) fn from_u32(v: u32) -> f64 {
            f64::from(v)
        }
    }

    impl MeanFloat for f64 {
        #[inline(always)]
        fn zero() -> f64 {
            window_f64_ops::zero()
        }
        #[inline(always)]
        fn add(self, other: f64) -> f64 {
            window_f64_ops::add(self, other)
        }
        #[inline(always)]
        fn div(self, other: f64) -> f64 {
            window_f64_ops::div(self, other)
        }
        #[inline(always)]
        fn from_u32(v: u32) -> f64 {
            window_f64_ops::from_u32(v)
        }
    }

    impl MeanView<f64> for pinewasm_mir::Value {
        #[inline(always)]
        fn view(&self) -> MeanCell<f64> {
            match self {
                pinewasm_mir::Value::Float(v) => MeanCell::Num(*v),
                // Compiled programs coerce the source to float; widen ints
                // defensively.
                pinewasm_mir::Value::Int(v) => MeanCell::Num(*v as f64),
                pinewasm_mir::Value::Na | pinewasm_mir::Value::DisplacedTable(_) => MeanCell::Na,
                _ => MeanCell::Other,
            }
        }
    }
}

type RealtimeDriverState = vm_tick::TickDriver<StateSnapshot>;

/// Seed key used by `math.random` calls that pass no seed argument.
const DEFAULT_RANDOM_SEED: u64 = 0x5EED_5EED_5EED_5EED;

/// The [`ElemType`] a scalar runtime value belongs to, for `array.from`'s
/// element-type inference. `na` has no type (returns `None`), so inference
/// falls through to a later concrete argument.
fn elem_type_of_value(value: &Value) -> Option<ElemType> {
    match value {
        Value::Float(_) => Some(ElemType::Float),
        Value::Int(_) => Some(ElemType::Int),
        Value::Bool(_) => Some(ElemType::Bool),
        Value::String(_) => Some(ElemType::String),
        Value::Color { .. } => Some(ElemType::Color),
        Value::Na | Value::DisplacedTable(_) | Value::Ref(_) => None,
    }
}

/// A zero of either sign as `+0.0`; anything else unchanged. Window
/// extrema never return `-0.0`: this is the series-length counterpart of
/// `pinewasm_ta`'s `canon_zero`, so `ta.highest`/`ta.lowest`/`ta.range`
/// give the same bits whether the length is a constant or a series.
#[inline]
fn canonical_window_zero(value: f64) -> f64 {
    if value == 0.0 {
        0.0
    } else {
        value
    }
}

/// The numeric value of an array element as `f64` (int widens to float), or
/// `None` for `na` and non-numeric values. Used by the numeric aggregates, which
/// skip `na`.
fn value_as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Float(f) if f.is_finite() => Some(*f),
        Value::Int(i) => Some(vm_ops::vm_f64_ops::from_i64(*i)),
        _ => None,
    }
}

/// Element truthiness for `array.every`/`array.some` (no callback): a
/// `true` bool, or a non-zero, non-`na` number. `na`, `false`, and zero are
/// false; non-numeric non-bool elements (only possible on mistyped arrays the
/// checker rejects) count as false.
fn element_is_true(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::Int(i) => *i != 0,
        Value::Float(f) => f.is_finite() && *f != 0.0,
        _ => false,
    }
}

/// Arithmetic mean of `xs`, or `NaN` for an empty slice (mapped to `na`).
fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        f64::NAN
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

/// Population variance (denominator `n`) of `xs`, or `NaN` when empty.
fn variance(xs: &[f64]) -> f64 {
    variance_with(xs, true)
}

/// Variance of `xs`, dividing by `n` when `biased` (the population form, which is the
/// default) and by `n - 1` otherwise. A single-element unbiased variance
/// divides by zero and normalizes to `na` through `Value::from_f64`.
fn variance_with(xs: &[f64], biased: bool) -> f64 {
    if xs.is_empty() {
        return f64::NAN;
    }
    let m = mean(xs);
    let ssd = xs.iter().map(|x| (x - m) * (x - m)).sum::<f64>();
    ssd / if biased {
        xs.len() as f64
    } else {
        xs.len() as f64 - 1.0
    }
}

/// Median of `xs` (mutated: sorted in place). Even counts average the two
/// middle elements; empty → `NaN`.
fn median(xs: &mut [f64]) -> f64 {
    if xs.is_empty() {
        return f64::NAN;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = xs.len();
    if n % 2 == 1 {
        xs[n / 2]
    } else {
        f64::midpoint(xs[n / 2 - 1], xs[n / 2])
    }
}

/// The most frequently occurring value in `xs`; on a tie, the smallest such
/// value. Empty → `NaN`.
fn mode(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return f64::NAN;
    }
    let mut sorted = xs.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut best_val = sorted[0];
    let mut best_count = 0usize;
    let mut cur_val = sorted[0];
    let mut cur_count = 0usize;
    for &x in &sorted {
        if x == cur_val {
            cur_count += 1;
        } else {
            cur_val = x;
            cur_count = 1;
        }
        // Strict `>` keeps the first (smallest, since sorted) value on ties.
        if cur_count > best_count {
            best_count = cur_count;
            best_val = cur_val;
        }
    }
    best_val
}

/// Covariance of two equal-length numeric sequences, pairing elements by
/// index. A pair is skipped when *either* element is `na`. `biased` selects the
/// denominator: `n` (population) when true, `n - 1` (sample) when false. Fewer
/// than the required paired observations (0 for biased, ≤1 for unbiased) → `NaN`
/// (mapped to `na`).
fn covariance(xs: &[Option<f64>], ys: &[Option<f64>], biased: bool) -> f64 {
    let pairs: Vec<(f64, f64)> = xs
        .iter()
        .zip(ys.iter())
        .filter_map(|(a, b)| match (a, b) {
            (Some(x), Some(y)) => Some((*x, *y)),
            _ => None,
        })
        .collect();
    let n = pairs.len();
    if n == 0 || (!biased && n < 2) {
        return f64::NAN;
    }
    let mx = pairs.iter().map(|(x, _)| *x).sum::<f64>() / n as f64;
    let my = pairs.iter().map(|(_, y)| *y).sum::<f64>() / n as f64;
    let sxy = pairs.iter().map(|(x, y)| (x - mx) * (y - my)).sum::<f64>();
    let denom = if biased { n as f64 } else { (n - 1) as f64 };
    sxy / denom
}

/// The percentile rank of the value at `index` in `items` (the raw array, which
/// may hold `na`). The rank is the percentage of the *other* non-`na` elements
/// that are `<=` the reference value, not counting the reference element,
/// scaled to `0..=100`. An `na` reference, an out-of-range index, or a sole
/// non-`na` element (no others to compare against) → `NaN` (mapped to `na`).
/// Denominator: the count of the other non-na elements, so the result is
/// `100 * count_le_others / count_others`.
fn percentrank(items: &[Value], index: usize) -> f64 {
    let reference = match items.get(index).and_then(value_as_f64) {
        Some(v) => v,
        None => return f64::NAN,
    };
    let mut others = 0usize;
    let mut le = 0usize;
    for (i, v) in items.iter().enumerate() {
        if i == index {
            continue;
        }
        if let Some(x) = value_as_f64(v) {
            others += 1;
            if x <= reference {
                le += 1;
            }
        }
    }
    if others == 0 {
        return f64::NAN;
    }
    100.0 * le as f64 / others as f64
}

/// The value at `percentage` (0..=100) using the nearest-rank method: sort the
/// non-`na` elements ascending and pick the observation at ordinal rank
/// `ceil(percentage/100 * n)` (1-based), clamped to `1..=n`. Empty → `NaN`.
/// Rounding: nearest-rank ordinal `ceil(p/100 * n)`.
fn percentile_nearest_rank(mut xs: Vec<f64>, percentage: f64) -> f64 {
    if xs.is_empty() || !percentage.is_finite() {
        return f64::NAN;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = xs.len();
    let p = percentage.clamp(0.0, 100.0);
    // 1-based ordinal rank; `ceil`, with rank 0 promoted to 1.
    let rank = (p / 100.0 * n as f64).ceil().max(1.0) as usize;
    xs[rank.min(n) - 1]
}

/// The value at `percentage` (0..=100) by linear interpolation between the two
/// neighbouring sorted values. Sort ascending, map the percentage to the
/// fractional position `h = percentage/100 * (n - 1)` (0-based), and interpolate
/// between `xs[floor(h)]` and `xs[ceil(h)]`. Empty → `NaN`. Because the scale is
/// `n - 1`, 0 and 100 return the smallest and largest values.
fn percentile_linear_interpolation(mut xs: Vec<f64>, percentage: f64) -> f64 {
    if xs.is_empty() || !percentage.is_finite() {
        return f64::NAN;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = xs.len();
    if n == 1 {
        return xs[0];
    }
    let p = percentage.clamp(0.0, 100.0);
    let h = p / 100.0 * (n - 1) as f64;
    let lo = h.floor() as usize;
    let hi = h.ceil() as usize;
    if lo == hi {
        xs[lo]
    } else {
        let frac = h - lo as f64;
        xs[lo] + (xs[hi] - xs[lo]) * frac
    }
}

/// Which binary-search variant to run over a sorted-ascending numeric array.
#[derive(Clone, Copy)]
enum BinarySearchMode {
    /// Exact match: an index of `target`, or `-1` if absent.
    Exact,
    /// Leftmost: the first index equal to `target`, else the index just left of
    /// its insertion point (clamped to `0`).
    Leftmost,
    /// Rightmost: the last index equal to `target`, else the index just right of
    /// its insertion point (clamped to `len - 1`).
    Rightmost,
}

/// Binary search over a numeric array sorted ascending. `na` elements
/// (returned as `None` by `value_as_f64`) are treated as `+inf` so they sort to
/// the end, matching the sort functions' `na`-last ordering. Returns an `i64`
/// index (or `-1` for the not-found `Exact` case) — never panics on any input.
fn binary_search(items: &[Value], target: f64, mode: BinarySearchMode) -> i64 {
    let key = |v: &Value| value_as_f64(v).unwrap_or(f64::INFINITY);
    let n = items.len();
    if n == 0 {
        // No elements: -1 for the exact case and, honestly, for the leftmost /
        // rightmost cases too (there is no valid index to point at).
        return -1;
    }
    // lower_bound: first index with key >= target.
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if key(&items[mid]) < target {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let lower = lo;
    // upper_bound: first index with key > target.
    let mut lo2 = lower;
    let mut hi2 = n;
    while lo2 < hi2 {
        let mid = lo2 + (hi2 - lo2) / 2;
        if key(&items[mid]) <= target {
            lo2 = mid + 1;
        } else {
            hi2 = mid;
        }
    }
    let upper = lo2;
    let found = upper > lower; // at least one element equals target.
    match mode {
        BinarySearchMode::Exact => {
            if found {
                lower as i64
            } else {
                -1
            }
        }
        BinarySearchMode::Leftmost => {
            if found {
                lower as i64
            } else {
                // Insertion point is `lower`; the element to its left.
                (lower as i64 - 1).max(0)
            }
        }
        BinarySearchMode::Rightmost => {
            if found {
                (upper - 1) as i64
            } else {
                // Insertion point is `lower`; the element to its right,
                // clamped into range.
                (lower.min(n - 1)) as i64
            }
        }
    }
}

/// Sort an int, float or string array's elements in place (see
/// [`sort_cmp`]); numeric comparison widens ints to floats.
fn sort_values(items: &mut [Value], descending: bool) {
    items.sort_by(|a, b| sort_cmp(a, b, descending));
}

/// The permutation of indices that sorts `items` (stable). Mirrors
/// [`sort_values`]'s ordering.
fn sort_indices(items: &[Value], descending: bool) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..items.len()).collect();
    idx.sort_by(|&i, &j| sort_cmp(&items[i], &items[j], descending));
    idx
}

/// Comparison for the sort functions. na elements and empty strings sort as
/// the greatest values. Numbers compare
/// numerically and strings by code point; `descending` reverses the order.
fn sort_cmp(a: &Value, b: &Value, descending: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let missing = |v: &Value| v.is_na() || matches!(v, Value::String(s) if s.is_empty());
    let ord = match (missing(a), missing(b)) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => match (a, b) {
            (Value::String(x), Value::String(y)) => x.cmp(y),
            _ => match (value_as_f64(a), value_as_f64(b)) {
                (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
                _ => Ordering::Equal,
            },
        },
    };
    if descending {
        ord.reverse()
    } else {
        ord
    }
}

/// Collection search preserves reference identity independently of scalar
/// missingness: two displaced aliases can name the same table lifetime.
/// Retired identities remain distinct from literal `na`. The existing missing-
/// value fallback is unchanged; scalar comparisons still normalize displaced
/// aliases to `na` before applying their comparison policy.
fn values_equal(a: &Value, b: &Value) -> bool {
    if let (Value::DisplacedTable(a), Value::DisplacedTable(b)) = (a, b) {
        return a == b;
    }
    if a.is_na() || b.is_na() {
        return false;
    }
    match (a, b) {
        (Value::Na | Value::DisplacedTable(_), _) | (_, Value::Na | Value::DisplacedTable(_)) => {
            false
        }
        // Float NaN compares unequal (Rust semantics).
        (Value::Float(x), Value::Float(y)) => x == y,
        (l, r) => std::mem::discriminant(l) == std::mem::discriminant(r) && l == r,
    }
}

/// Count a float's Display representation without allocating the temporary
/// string. Very large or subnormal values exceed the old 64-byte estimate.
fn join_float_byte_len(value: f64) -> Option<usize> {
    use std::fmt::Write;
    struct Counter(usize);
    impl std::fmt::Write for Counter {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            self.0 = self.0.checked_add(text.len()).ok_or(std::fmt::Error)?;
            Ok(())
        }
    }
    if !value.is_finite() {
        return Some(2); // Defensive non-finite Float rendering normalizes through Value::Na.
    }
    let mut counter = Counter(0);
    write!(&mut counter, "{value}").ok()?;
    Some(counter.0)
}

/// Render one element for `array.join`; `na` becomes `"NaN"`.
fn render_join_element(value: &Value) -> String {
    match value {
        Value::Na | Value::DisplacedTable(_) => "NaN".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => Value::from_f64(*f).render(),
        Value::String(s) => s.clone(),
        Value::Color { r, g, b, t, .. } => Value::color_hex(*r, *g, *b, *t),
        Value::Ref(_) => value.render(),
    }
}

/// Convert an integral-valued f64 to an exact `Value::Int`, or `na` when it
/// falls outside the i64 range (the checked-overflow rule the integer
/// opcodes follow). `i64::MAX as f64` is exactly 2^63, so the upper bound
/// is strict.
fn f64_to_int(f: f64) -> Value {
    if f >= i64::MIN as f64 && f < i64::MAX as f64 {
        Value::Int(f as i64)
    } else {
        Value::Na
    }
}

/// Clamp an `i64` color argument to `[lo, hi]` and narrow to `u8`. Used by the
/// RGB constructors so an out-of-range channel saturates
/// rather than wrapping or panicking.
fn clamp_u8(v: i64, lo: u8, hi: u8) -> u8 {
    v.clamp(i64::from(lo), i64::from(hi)) as u8
}

/// Map an optional string trade field to a `Value`: `Some(s)` → a string value,
/// `None` (absent id/comment) → `na`.
fn str_or_na(s: Option<&str>) -> Value {
    match s {
        Some(s) => Value::String(s.to_string()),
        None => Value::Na,
    }
}

/// Linearly interpolate `color.from_gradient(value, bottom_value, top_value,
/// bottom_color, top_color)`. The blend fraction is `value`'s position in
/// `[bottom_value, top_value]`, clamped to `[0, 1]` (values below/above the
/// range pin to the bottom/top color). RGB channels round half-away-from-zero;
/// transparency keeps its interpolated percentage until paint serialization.
/// A degenerate range (`bottom == top`)
/// yields the top color. `bottom`/`top` are `(r, g, b, t)` tuples.
fn color_from_gradient(
    value: f64,
    bottom_value: f64,
    top_value: f64,
    bottom: (u8, u8, u8, f64),
    top: (u8, u8, u8, f64),
) -> Value {
    let denom = top_value - bottom_value;
    let frac = if denom == 0.0 {
        1.0
    } else {
        ((value - bottom_value) / denom).clamp(0.0, 1.0)
    };
    let lerp = |lo: u8, hi: u8| -> u8 {
        let blended = f64::from(lo) + (f64::from(hi) - f64::from(lo)) * frac;
        // Round half away from zero; the operands are already in-range so the
        // result cannot exceed a u8's channel bounds.
        blended.round() as u8
    };
    Value::Color {
        r: lerp(bottom.0, top.0),
        g: lerp(bottom.1, top.1),
        b: lerp(bottom.2, top.2),
        t: bottom.3 + (top.3 - bottom.3) * frac,
        input_origin: None,
    }
}

/// Whether a host-supplied [`InputValue`] override is compatible with the
/// declaration. Scalars match the declared [`InputKind`] (with int-to-float
/// widening); enum overrides must also match the declaring type and an allowed
/// member index. Mismatches fall back to the default rather than aborting.
fn input_value_matches_decl(value: &InputValue, decl: &InputDecl) -> bool {
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
        // New manifests materialize every allowed member in `options`, even
        // when source omitted the argument. An empty list is accepted only for
        // backward compatibility with older serialized programs.
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
    // The scalar cases are `vm_ops::input_scalar_matches`: `input.price` is
    // a float and `input.time` an int; an int default coerces to either,
    // mirroring `input.float`; a float is finite; a color's transparency is
    // at most 100.
    match (value, decl.kind) {
        (InputValue::Int(i), kind) => {
            vm_ops::input_scalar_matches(vm_ops::InputScalar::Int(*i), kind)
        }
        (InputValue::Float(value), kind) => {
            vm_ops::input_scalar_matches(vm_ops::InputScalar::Float(*value), kind)
        }
        (InputValue::Bool(b), kind) => {
            vm_ops::input_scalar_matches(vm_ops::InputScalar::Bool(*b), kind)
        }
        (InputValue::Color { r, g, b, t }, kind) => vm_ops::input_scalar_matches(
            vm_ops::InputScalar::Color {
                r: *r,
                g: *g,
                b: *b,
                t: *t,
            },
            kind,
        ),
        (InputValue::String(_), InputKind::String) => true,
        (InputValue::Source(series), InputKind::Source) => series.is_input_source(),
        (InputValue::ExternalSource(source), InputKind::Source) => source.is_valid(),
        _ => false,
    }
}

/// Conservative heap payload requested by predictable [`Vm::new`] state.
/// `Vec`/`String` headers and the second output snapshot built by `outcome`
/// are included so a small tampered CSIL document cannot amplify into an
/// unmetered slot ring or nested output allocation.
fn vm_preallocation_bytes(
    program: &CsilProgram,
    data: &MarketData,
    options: &RunOptions,
    bars: usize,
    history_ring: usize,
) -> Option<usize> {
    vm_preallocation_bytes_with_ta(
        program,
        data,
        options,
        bars,
        history_ring,
        ta::TaStates::allocation_bytes(&program.ta_states)?,
        &HashMap::new(),
    )
}

fn vm_preallocation_bytes_with_ta(
    program: &CsilProgram,
    data: &MarketData,
    options: &RunOptions,
    bars: usize,
    history_ring: usize,
    ta_bytes: usize,
    ta_values: &HashMap<TaInputTarget, i64>,
) -> Option<usize> {
    use std::mem::size_of;

    fn add_product(total: &mut usize, lhs: usize, rhs: usize) -> Option<()> {
        *total = total.checked_add(lhs.checked_mul(rhs)?)?;
        Some(())
    }

    fn role_count<'a>(mut roles: impl Iterator<Item = &'a Vec<PlotColorRole>>) -> Option<usize> {
        roles.try_fold(0usize, |sum, roles| sum.checked_add(roles.len()))
    }

    fn input_value_payload_bytes(value: &InputValue) -> Option<usize> {
        match value {
            InputValue::String(value) => Some(value.len()),
            InputValue::ExternalSource(source) => {
                source.script_id.len().checked_add(source.plot_label.len())
            }
            InputValue::Enum {
                name,
                member,
                title,
                ..
            } => name
                .len()
                .checked_add(member.len())?
                .checked_add(title.as_ref().map_or(0, String::len)),
            _ => Some(0),
        }
    }

    let realtime_ticks = options
        .realtime
        .as_ref()
        .map_or(0usize, |sim| sim.ticks.len());
    let output_bars = bars.checked_add(realtime_ticks)?;
    let plots = program.plots.len();
    let visuals = program.visuals.len();
    let fills = program.fills.len();
    let plot_roles = role_count(program.plots.iter().map(|decl| &decl.color_roles))?;
    let visual_roles = role_count(program.visuals.iter().map(|decl| &decl.color_roles))?;
    let fill_roles = role_count(program.fills.iter().map(|decl| &decl.color_roles))?;
    let color_roles = plot_roles
        .checked_add(visual_roles)?
        .checked_add(fill_roles)?;

    let mut bytes = 0usize;
    let declarations = plots.checked_add(visuals)?.checked_add(fills)?;

    // Output lanes exist once inside Vm and once in the returned snapshot.
    // Count both live copies in the peak reservation.
    for _ in 0..2 {
        add_product(&mut bytes, plots, size_of::<Vec<Value>>())?;
        add_product(
            &mut bytes,
            plots.checked_mul(output_bars)?,
            size_of::<Value>(),
        )?;
        add_product(&mut bytes, declarations, size_of::<Vec<Vec<Value>>>())?;
        add_product(&mut bytes, color_roles, size_of::<Vec<Value>>())?;
        add_product(
            &mut bytes,
            color_roles.checked_mul(output_bars)?,
            size_of::<Value>(),
        )?;
    }

    // Selected palette identities are materialized once in the wire snapshot;
    // the VM itself retains them inside Value::Color, already counted above.
    add_product(&mut bytes, color_roles, size_of::<Vec<Option<u32>>>())?;
    add_product(
        &mut bytes,
        color_roles.checked_mul(output_bars)?,
        size_of::<Option<u32>>(),
    )?;

    // History rings, per-slot GC depths, written flags, and the bounded stack.
    let slots = program.series_slots.len();
    add_product(&mut bytes, slots, size_of::<Vec<Value>>())?;
    add_product(
        &mut bytes,
        slots.checked_mul(history_ring)?,
        size_of::<Value>(),
    )?;
    add_product(&mut bytes, slots, size_of::<u32>())?;
    add_product(&mut bytes, slots, size_of::<bool>())?;
    // Dynamic EMA call sites are indexed by series slot. Realtime rollback
    // retains a second copy while a forming bar is evaluated.
    add_product(
        &mut bytes,
        slots.checked_mul(2)?,
        size_of::<Option<DynamicEmaState>>(),
    )?;
    if program.functions.iter().any(|function| {
        function
            .code
            .iter()
            .any(|instr| matches!(instr, CsilInstr::WindowStatisticDynamic { .. }))
    }) {
        add_product(&mut bytes, MAX_WINDOW_LENGTH as usize, size_of::<f64>())?;
    }
    let pivot_sites = program
        .main()
        .code
        .iter()
        .filter(|instr| matches!(instr, CsilInstr::CallPivotPointLevels { .. }))
        .count();
    // Active per-call pivot accumulator plus the realtime rollback snapshot.
    add_product(
        &mut bytes,
        pivot_sites.checked_mul(2)?,
        size_of::<(usize, PivotPointState)>(),
    )?;
    add_product(
        &mut bytes,
        (program.main().max_stack as usize).min(options.max_stack_depth),
        size_of::<Value>(),
    )?;

    bytes = bytes.checked_add(ta_bytes)?;
    bytes = bytes.checked_add(sma::States::allocation_bytes(program, ta_values)?)?;
    if options.profile {
        let instructions = program.main().code.len();
        add_product(
            &mut bytes,
            instructions.checked_mul(2)?.checked_add(1)?,
            size_of::<u64>(),
        )?;
        add_product(&mut bytes, instructions, size_of::<u64>())?;
    }
    if program.metadata.strategy.is_some() {
        add_product(&mut bytes, output_bars, STRATEGY_BAR_RESERVATION_BYTES)?;
    }
    if options.realtime_trace {
        add_product(
            &mut bytes,
            realtime_ticks,
            size_of::<PlotTickSample>().checked_add(plots.checked_mul(size_of::<Value>())?)?,
        )?;
    }
    if let Some(sim) = &options.realtime {
        bytes = bytes.checked_add(std::mem::size_of_val(sim.ticks.as_slice()))?;
        // Appended realtime bars extend nine scalar lanes (OHLCV, time,
        // trading-day, bid, ask) in the VM-owned feed.
        add_product(
            &mut bytes,
            realtime_ticks,
            7usize
                .checked_mul(size_of::<f64>())?
                .checked_add(2usize.checked_mul(size_of::<i64>())?)?,
        )?;

        // The first appended bar turns an omitted time lane into a full
        // historical lane before adding the realtime value, and so does the
        // first appended tick that carries a `tradingDay` for an omitted
        // trading-day lane. The per-tick reservation above covers the
        // appended value; reserve the synthesized historical prefix as well.
        // This is deliberately pessimistic when a nonempty simulation only
        // reruns the last committed bar.
        if realtime_ticks > 0 {
            let tick_trading_day = sim.ticks.iter().any(|tick| tick.trading_day.is_some());
            let synthesized_identity_lanes = usize::from(data.time.is_empty())
                .checked_add(usize::from(data.trading_day.is_empty() && tick_trading_day))?;
            add_product(
                &mut bytes,
                bars,
                synthesized_identity_lanes.checked_mul(size_of::<i64>())?,
            )?;
        }
    }

    // The VM owns clones so realtime can mutate its feed and sessions can
    // outlive the caller. Count every numeric lane and the dominant nested
    // host tables; the caller's already-owned copy is outside this VM budget.
    for len in [
        data.open.len(),
        data.high.len(),
        data.low.len(),
        data.close.len(),
        data.volume.len(),
        data.bid.len(),
        data.ask.len(),
    ] {
        add_product(&mut bytes, len, size_of::<f64>())?;
    }
    for len in [
        data.time.len(),
        data.time_close.len(),
        data.trading_day.len(),
    ] {
        add_product(&mut bytes, len, size_of::<i64>())?;
    }
    add_product(
        &mut bytes,
        data.request_time_close.len(),
        size_of::<(String, Vec<i64>)>(),
    )?;
    for (key, lane) in &data.request_time_close {
        bytes = bytes.checked_add(key.len())?;
        add_product(&mut bytes, lane.len(), size_of::<i64>())?;
    }
    add_product(
        &mut bytes,
        data.request_bars.len(),
        size_of::<(String, RequestTimeframeBars)>(),
    )?;
    for (key, feed) in &data.request_bars {
        bytes = bytes.checked_add(key.len())?;
        for lane in [&feed.open, &feed.high, &feed.low, &feed.close, &feed.volume] {
            add_product(&mut bytes, lane.len(), size_of::<f64>())?;
        }
        add_product(&mut bytes, feed.time.len(), size_of::<i64>())?;
        add_product(&mut bytes, feed.time_close.len(), size_of::<i64>())?;
    }
    add_product(
        &mut bytes,
        data.request_data.len(),
        size_of::<(String, Vec<f64>)>(),
    )?;
    for (key, lane) in &data.request_data {
        bytes = bytes.checked_add(key.len())?;
        add_product(&mut bytes, lane.len(), size_of::<f64>())?;
    }
    add_product(
        &mut bytes,
        data.security_lower_tf_data.len(),
        size_of::<(String, Vec<Vec<f64>>)>(),
    )?;
    for (key, bars) in &data.security_lower_tf_data {
        bytes = bytes.checked_add(key.len())?;
        add_product(&mut bytes, bars.len(), size_of::<Vec<f64>>())?;
        for lane in bars {
            add_product(&mut bytes, lane.len(), size_of::<f64>())?;
        }
    }
    add_product(
        &mut bytes,
        data.footprint_data.len(),
        size_of::<(String, Vec<Option<HostFootprint>>)>(),
    )?;
    for (key, samples) in &data.footprint_data {
        bytes = bytes.checked_add(key.len())?;
        add_product(
            &mut bytes,
            samples.len(),
            size_of::<Option<HostFootprint>>(),
        )?;
        for sample in samples.iter().flatten() {
            add_product(&mut bytes, sample.rows.len(), size_of::<HostVolumeRow>())?;
        }
    }
    bytes = bytes.checked_add(data.timeframe.len())?;
    for value in [
        &data.syminfo.ticker,
        &data.syminfo.tickerid,
        &data.syminfo.currency,
        &data.syminfo.kind,
        &data.syminfo.timezone,
        &data.syminfo.basecurrency,
        &data.syminfo.description,
        &data.syminfo.country,
        &data.syminfo.industry,
        &data.syminfo.sector,
        &data.syminfo.root,
        &data.syminfo.prefix,
        &data.syminfo.session,
        &data.syminfo.volumetype,
        &data.syminfo.main_tickerid,
    ] {
        bytes = bytes.checked_add(value.len())?;
    }

    // `Vm` retains an `Arc` around a cloned RunOptions. Count host-selected
    // input strings and cross-script lanes so a compact script cannot use an
    // attacker-shaped options envelope to force an unmetered clone.
    add_product(
        &mut bytes,
        options.inputs.len(),
        size_of::<(u32, InputValue)>(),
    )?;
    for value in options.inputs.values() {
        bytes = bytes.checked_add(input_value_payload_bytes(value)?)?;
    }
    add_product(
        &mut bytes,
        options.external_sources.len(),
        size_of::<(u32, ExternalSourceLane)>(),
    )?;
    for lane in options.external_sources.values() {
        bytes = bytes
            .checked_add(lane.source.script_id.len())?
            .checked_add(lane.source.plot_label.len())?;
        add_product(&mut bytes, lane.values.len(), size_of::<Option<f64>>())?;
    }
    for value in [
        &options.strategy.default_qty_type,
        &options.strategy.commission_type,
        &options.strategy.close_entries_rule,
        &options.strategy.currency,
    ] {
        bytes = bytes.checked_add(value.as_ref().map_or(0, String::len))?;
    }

    bytes.checked_add(vm_program_bytes(program)?)
}

fn vm_program_bytes(program: &CsilProgram) -> Option<usize> {
    let mut bytes = 0usize;
    // Retained references are charged by reserved capacity; ordinary CSIL
    // keeps the existing logical clone accounting used by prepared runs.
    bytes = bytes.checked_add(std::mem::size_of_val(program.constants.as_slice()))?;
    for constant in &program.constants {
        bytes = bytes.checked_add(constant.len())?;
    }
    bytes = bytes.checked_add(std::mem::size_of_val(program.series_slots.as_slice()))?;
    bytes = bytes.checked_add(std::mem::size_of_val(program.plots.as_slice()))?;
    bytes = bytes.checked_add(std::mem::size_of_val(program.visuals.as_slice()))?;
    bytes = bytes.checked_add(std::mem::size_of_val(program.fills.as_slice()))?;
    bytes = bytes.checked_add(std::mem::size_of_val(program.ta_states.as_slice()))?;
    for decl in &program.ta_states {
        bytes = bytes.checked_add(std::mem::size_of_val(decl.params.as_slice()))?;
    }
    for function in &program.functions {
        bytes = bytes.checked_add(function.name.len())?;
        bytes = bytes.checked_add(std::mem::size_of_val(function.code.as_slice()))?;
        bytes = bytes.checked_add(std::mem::size_of_val(function.source_map.as_slice()))?;
        if let Some(reference) = &function.budget_reference {
            bytes =
                bytes.checked_add(pinewasm_csil::budget_reference_allocated_bytes(reference)?)?;
        }
    }
    bytes = bytes.checked_add(std::mem::size_of_val(program.ta_input_bindings.as_slice()))?;
    Some(bytes)
}

fn vm_memory_limit_error(limit: usize, detail: &str) -> RuntimeError {
    RuntimeError {
        kind: RuntimeErrorKind::VmMemoryLimitExceeded,
        message: format!("{detail}; VM memory limit is {limit} bytes"),
        bar_index: None,
        instruction_offset: None,
        span: None,
    }
}

// Admission validates that every bound location contains its resolved default.
// Compare the resolver's representation, including float bits and wrong-kind
// fallback, rather than the host override's kind. new_owned has already rejected
// unresolved overrides; keeping None false also makes this predicate fail closed.
fn ta_inputs_are_default(program: &CsilProgram, options: &RunOptions) -> bool {
    program.ta_input_bindings.iter().all(|binding| {
        program
            .resolve_ta_input(binding, options.inputs.get(&binding.input))
            .is_some_and(|value| program.resolve_ta_input(binding, None) == Some(value))
    })
}

/// `run_bar`'s view of the VM for the generic dispatch loop
/// (`vm_dispatch::dispatch_loop`): the per-run counter, the two loop errors,
/// and one instruction's execution.
struct RunBarHost<'a> {
    vm: &'a mut Vm,
    program: &'a CsilProgram,
}

impl vm_dispatch::DispatchHost for RunBarHost<'_> {
    type Instr = CsilInstr;
    type Error = RuntimeError;

    #[inline(always)]
    fn remaining(&mut self) -> &mut u64 {
        &mut self.vm.instructions_remaining
    }

    fn limit_error(&mut self, limit: vm_charge::ChargeLimit, pc: usize) -> RuntimeError {
        self.vm.limit_error(limit, pc)
    }

    fn past_end(&mut self, pc: usize) -> RuntimeError {
        self.vm.fail(
            RuntimeErrorKind::InvalidProgram,
            "execution ran past the end of the function".into(),
            pc,
        )
    }

    #[inline(always)]
    fn exec(
        &mut self,
        instr: &CsilInstr,
        pc: usize,
        budget: &mut u64,
    ) -> Result<vm_dispatch::Flow, RuntimeError> {
        // Test-only observer: the operand stack at the entry of every
        // executed instruction, for the declared-lane soundness tests.
        #[cfg(test)]
        tests::lane_rows::observe(self.vm, pc);
        self.vm.exec_instr(self.program, instr, pc, budget)
    }
}

impl Vm {
    fn new(
        program: &CsilProgram,
        data: &MarketData,
        options: &RunOptions,
    ) -> Result<Self, RuntimeError> {
        Self::new_owned(program, None, data, options)
    }

    fn new_owned(
        program: &CsilProgram,
        owner: Option<&Arc<CsilProgram>>,
        data: &MarketData,
        options: &RunOptions,
    ) -> Result<Self, RuntimeError> {
        if vm_program_bytes(program).is_none_or(|bytes| bytes > options.max_vm_bytes_per_run) {
            return Err(vm_memory_limit_error(
                options.max_vm_bytes_per_run,
                "program clone exceeds the memory budget",
            ));
        }
        let mut ta_values = HashMap::new();
        let resolver = ta_input_install::ProgramInputs {
            program,
            inputs: &options.inputs,
        };
        if let Err(index) =
            ta_input_install::install(&program.ta_input_bindings, &resolver, &mut ta_values)
        {
            let binding = &program.ta_input_bindings[index];
            return Err(RuntimeError {
                kind: RuntimeErrorKind::InvalidTaLength,
                message: match program.ta_input_bounds(binding) {
                    Some((min, max)) => format!(
                        "input {} is outside the legal TA parameter range {min}..={max}",
                        binding.input
                    ),
                    None => format!(
                        "input {} violates the TA float parameter rule",
                        binding.input
                    ),
                },
                bar_index: None,
                instruction_offset: None,
                span: None,
            });
        }
        let bars = vm_layout::run_bars(options.max_bars, data.bars());
        let history_ring = vm_layout::ring_cells(program.history_depth_with_ta_inputs(&ta_values))
            .ok_or_else(|| {
                vm_memory_limit_error(
                    options.max_vm_bytes_per_run,
                    "history-ring size arithmetic overflowed",
                )
            })?;
        let planned_bytes = if ta_values.is_empty() {
            vm_preallocation_bytes(program, data, options, bars, history_ring)
        } else {
            let ta_bytes = ta::TaStates::allocation_bytes_with_params(
                &program.ta_states,
                |state, param, default| {
                    ta_params::state_param_value(&ta_values, state, param, default)
                },
            );
            ta_bytes.and_then(|bytes| {
                vm_preallocation_bytes_with_ta(
                    program,
                    data,
                    options,
                    bars,
                    history_ring,
                    bytes,
                    &ta_values,
                )
            })
        };
        let state_bytes = planned_bytes.ok_or_else(|| {
            vm_memory_limit_error(
                options.max_vm_bytes_per_run,
                "VM state size arithmetic overflowed",
            )
        })?;
        if vm_memory_reservation::project(0, state_bytes, 0, options.max_vm_bytes_per_run).is_err()
        {
            return Err(vm_memory_limit_error(
                options.max_vm_bytes_per_run,
                &format!("VM state requires {state_bytes} bytes"),
            ));
        }
        let gc_history_depths = user_slot_gc_depths(program, &ta_values);
        // Admit the selected windows and the complete owned payload before
        // cloning any program metadata or allocating history/engine storage.
        let program_owner =
            if let Some(owner) = owner.filter(|_| ta_inputs_are_default(program, options)) {
                Arc::clone(owner)
            } else {
                let mut owned_program = program.clone();
                if !ta_values.is_empty() {
                    owned_program.set_ta_inputs(&ta_values);
                }
                Arc::new(owned_program)
            };
        drop(ta_values);
        let program = program_owner.as_ref();
        let strategy_info = program
            .metadata
            .strategy
            .as_ref()
            .map(|base| options.strategy.applied_to(base));
        let layout = vm_layout::alloc_layout(
            program.series_slots.len(),
            history_ring,
            program.plots.len(),
            bars,
            Value::Na,
        );
        Ok(Vm {
            program: Arc::clone(&program_owner),
            data: data.clone(),
            options: Arc::new(options.clone()),
            user_slots: layout.user_slots,
            slot_mask: layout.slot_mask,
            gc_history_depths,
            written: layout.written,
            plots: layout.plots,
            plot_colors: program
                .plots
                .iter()
                .map(|plot| vec![vec![Value::Na; bars]; plot.color_roles.len()])
                .collect(),
            visual_colors: program
                .visuals
                .iter()
                .map(|visual| vec![vec![Value::Na; bars]; visual.color_roles.len()])
                .collect(),
            fill_colors: program
                .fills
                .iter()
                .map(|fill| vec![vec![Value::Na; bars]; fill.color_roles.len()])
                .collect(),
            // One engine state per ta_states declaration; a site's state
            // advances only when its CALL_TA instruction executes.
            ta_state: ta::TaStates::new(&program.ta_states),
            dynamic_ema: vec![None; program.series_slots.len()],
            sma: sma::States::new(program),
            // Capacity is a hint from untrusted input: clamp it so a corrupted
            // max_stack cannot request a huge allocation up front (the stack
            // still grows on demand up to max_stack_depth).
            stack: Vec::with_capacity(
                (program.main().max_stack as usize).min(options.max_stack_depth),
            ),
            call_args: Vec::new(),
            window_scratch: if program.functions.iter().any(|function| {
                function
                    .code
                    .iter()
                    .any(|instr| matches!(instr, CsilInstr::WindowStatisticDynamic { .. }))
            }) {
                Vec::with_capacity(MAX_WINDOW_LENGTH as usize)
            } else {
                Vec::new()
            },
            bar: 0,
            bars,
            instructions_remaining: options.max_instructions_per_run,
            random_draws: 0,
            realtime_active: false,
            has_realtime: options
                .realtime
                .as_ref()
                .is_some_and(|rt| !rt.ticks.is_empty()),
            bar_is_new: true,
            bar_is_confirmed: true,
            realtime_bar: None,
            realtime_driver: RealtimeDriverState::default(),
            realtime_alias_undo: BTreeMap::new(),
            last_table_identity: 0,
            heap: Heap::with_limit(options.max_collection_elements),
            distance_cache: distance_scan::Cache::default(),
            distance_cache_reserved_bytes: 0,
            gc_threshold: GC_MIN_ARENA,
            gc_elem_threshold: options.max_collection_elements.div_ceil(2).max(1),
            drawings: {
                // Start from the default max live count per registered kind,
                // then apply any `indicator(max_*_count=)` override.
                let caps = program.metadata.drawing_caps;
                let cap_or = |cap: Option<u32>, kind: DrawKind| {
                    cap.map_or(kind.default_max(), |n| n as usize)
                };
                DrawingRegistry {
                    live: Default::default(),
                    max: [
                        cap_or(caps.max_lines_count, DrawKind::Line),
                        cap_or(caps.max_labels_count, DrawKind::Label),
                        cap_or(caps.max_boxes_count, DrawKind::Box),
                        DrawKind::Table.default_max(),
                        cap_or(caps.max_polylines_count, DrawKind::Polyline),
                        DrawKind::Linefill.default_max(),
                    ],
                    table_cells: std::collections::BTreeMap::new(),
                    ids: Default::default(),
                    next_id: 0,
                }
            },
            logs: Vec::new(),
            alerts: Vec::new(),
            alert_fired_sites: HashSet::new(),
            fill_recalculation_price: None,
            broker: strategy_info.as_ref().map(|strategy| {
                Broker::new_with_symbol(
                    strategy,
                    data.mintick,
                    data.syminfo.pointvalue,
                    data.syminfo.mincontract,
                    &data.syminfo.currency,
                )
            }),
            security_memo: HashMap::new(),
            security_expr_memo: HashMap::new(),
            pivot_hlc_memo: HashMap::new(),
            pivot_states: HashMap::new(),
            time_func_memo: HashMap::new(),
            regex_memo: HashMap::new(),
            regex_memo_bytes: 0,
            string_bytes_allocated: 0,
            bytes_reserved: state_bytes,
            transient_vm_bytes: 0,
            preallocated_realtime_appends: options
                .realtime
                .as_ref()
                .map_or(0, |sim| sim.ticks.len()),
            profile: options.profile.then(|| {
                let len = program.main().code.len();
                ProfileCounters {
                    count: vec![0; len * 2 + 1],
                    units: vec![0; len],
                }
            }),
        })
    }

    /// Execute the immutable history prefix once. This is shared by one-shot
    /// runs and persistent sessions so incremental execution has one
    /// correctness path rather than a second history interpreter.
    fn run_history(&mut self) -> (u32, Vec<RuntimeError>) {
        let program = self.program.clone();
        let strategy_info = program
            .metadata
            .strategy
            .as_ref()
            .map(|base| self.options.strategy.applied_to(base));
        let strategy_start_bar = strategy_info
            .as_ref()
            .map(|info| info.calc_bars_count as usize)
            .filter(|count| *count > 0)
            .map_or(0, |count| self.bars.saturating_sub(count));
        let mut bars_executed = 0u32;
        let mut errors = Vec::new();

        for bar in 0..self.bars {
            self.bar = bar;
            self.sync_broker_currency_rate(bar, false);
            if bar < strategy_start_bar {
                if let Some(broker) = self.broker.as_mut() {
                    broker.record_equity(bar, self.data.close[bar]);
                }
                bars_executed += 1;
                continue;
            }
            self.bar_is_new = true;
            self.bar_is_confirmed = true;
            self.realtime_active = false;
            self.alert_fired_sites.clear();
            // Per-bar slot preamble. Persistent slots carry the previous
            // bar's value forward; everything else starts the bar as na.
            //
            // The na-init is new with ring storage and is not optional: a
            // full-history vector gave an unwritten cell `Na` for free because
            // nothing had ever touched it, while a ring cell still holds
            // whatever bar `bar - ring` left there. Conditional stores mean
            // plenty of cells go unwritten on any given bar. `pinewasm_aot`
            // emits exactly this preamble for the same reason.
            for (slot, def) in program.series_slots.iter().enumerate() {
                vm_history_step::history_cell_current(
                    &mut self.user_slots[slot],
                    def.persistent,
                    bar,
                    self.slot_mask,
                    Value::Na,
                );
            }
            // Collect here and nowhere else: the preamble has just dropped the
            // last reference to every bar-local collection the previous bar
            // built, and the operand stack is empty between bars, so this is
            // the point of minimum live set.
            self.maybe_collect_heap();
            if let Some(broker) = self.broker.as_mut() {
                let key = strategy_risk_session_key(&self.data, bar);
                broker.begin_session(key, bar, self.data.open[bar]);
            }
            if self.broker.is_some() {
                if let Err(error) = self.run_strategy_history_bar(bar) {
                    errors.push(error);
                    break;
                }
                bars_executed += 1;
                continue;
            }
            if let Some(broker) = self.broker.as_mut() {
                if !broker.process_on_close() {
                    let (open, high, low, close) = (
                        self.data.open[bar],
                        self.data.high[bar],
                        self.data.low[bar],
                        self.data.close[bar],
                    );
                    broker.process_fills(bar, open, high, low, close, false);
                }
            }
            if let Some(broker) = self.broker.as_mut() {
                broker.record_snapshot(
                    bar,
                    self.data.open[bar],
                    self.data.high[bar],
                    self.data.low[bar],
                    self.data.close[bar],
                );
            }
            if let Err(error) = self.run_bar() {
                errors.push(error);
                break;
            }
            if let Some(broker) = self.broker.as_mut() {
                let (open, high, low, close) = (
                    self.data.open[bar],
                    self.data.high[bar],
                    self.data.low[bar],
                    self.data.close[bar],
                );
                if broker.process_on_close() {
                    broker.process_fills(bar, open, high, low, close, true);
                }
                broker.record_equity(bar, close);
            }
            bars_executed += 1;
        }
        (bars_executed, errors)
    }

    /// Resolve compiler-owned fixed style slots from the same engine and
    /// resident bar as the snapshot. No slot identities escape to renderers.
    fn resolved_display_style(
        &self,
        source: &DisplayStyle,
        visible_bars: usize,
        is_fill: bool,
    ) -> DisplayStyle {
        resolve_display_style(
            source,
            is_fill,
            |slot| {
                visible_bars.checked_sub(1).and_then(|bar| {
                    self.user_slots
                        .get(slot as usize)
                        .and_then(|slot| slot.get(bar & self.slot_mask))
                })
            },
            |input| {
                self.program.inputs.get(input as usize).map(|decl| {
                    self.options
                        .inputs
                        .get(&input)
                        .filter(|value| input_value_matches_decl(value, decl))
                        .unwrap_or(&decl.defval)
                })
            },
            |input| {
                self.program.inputs.get(input as usize).and_then(|decl| {
                    self.options
                        .inputs
                        .get(&input)
                        .filter(|value| input_value_matches_decl(value, decl))
                })
            },
        )
    }

    /// Snapshot a wire outcome without consuming the VM. Persistent sessions
    /// call this after every advance while one-shot execution calls it once.
    fn outcome(
        &self,
        errors: Vec<RuntimeError>,
        bars_executed: u32,
        visible_bars: usize,
        realtime_trace: Option<Vec<PlotTickSample>>,
    ) -> RunOutcome {
        let program = &self.program;
        let visible_bars = visible_bars.min(self.bars);
        let plots = self
            .plots
            .iter()
            .enumerate()
            .map(|(i, values)| PlotSeries {
                slot: i as u32,
                title: program.plots[i].title.clone(),
                style: self.resolved_display_style(&program.plots[i].style, visible_bars, false),
                colors: program.plots[i]
                    .color_roles
                    .iter()
                    .copied()
                    .zip(&self.plot_colors[i])
                    .map(|(role, values)| {
                        PlotColorSeries::from_values(
                            role,
                            vm_output_prefix::visible_prefix(values, visible_bars),
                            &program.plots[i].style.color_palette,
                        )
                    })
                    .collect(),
                values: vm_output_prefix::visible_prefix(values, visible_bars).to_vec(),
            })
            .collect();
        let visuals = self
            .visual_colors
            .iter()
            .enumerate()
            .map(|(i, channels)| VisualSeries {
                slot: i as u32,
                title: program.visuals[i].title.clone(),
                style: self.resolved_display_style(&program.visuals[i].style, visible_bars, false),
                colors: program.visuals[i]
                    .color_roles
                    .iter()
                    .copied()
                    .zip(channels)
                    .map(|(role, values)| {
                        PlotColorSeries::from_values(
                            role,
                            vm_output_prefix::visible_prefix(values, visible_bars),
                            &program.visuals[i].style.color_palette,
                        )
                    })
                    .collect(),
            })
            .collect();
        let fills = self
            .fill_colors
            .iter()
            .enumerate()
            .map(|(i, channels)| FillSeries {
                slot: i as u32,
                plot1: program.fills[i].plot1,
                plot2: program.fills[i].plot2,
                title: program.fills[i].title.clone(),
                style: self.resolved_display_style(&program.fills[i].style, visible_bars, true),
                colors: program.fills[i]
                    .color_roles
                    .iter()
                    .copied()
                    .zip(channels)
                    .map(|(role, values)| {
                        PlotColorSeries::from_values(
                            role,
                            vm_output_prefix::visible_prefix(values, visible_bars),
                            &program.fills[i].style.color_palette,
                        )
                    })
                    .collect(),
            })
            .collect();

        let mut alerts = self.alerts.clone();
        let strategy_info = program
            .metadata
            .strategy
            .as_ref()
            .map(|base| self.options.strategy.applied_to(base));
        let mut broker = self.broker.clone();
        if let Some(broker) = broker.as_mut() {
            merge_order_fill_alerts(&mut alerts, broker);
        }
        let strategy = strategy_info.as_ref().map(|strategy| {
            let final_close = visible_bars
                .checked_sub(1)
                .and_then(|bar| self.data.close.get(bar).copied())
                .unwrap_or(f64::NAN);
            strategy_outcome(
                strategy.initial_capital,
                broker.expect("strategy scripts construct a broker"),
                final_close,
            )
        });
        let profile = self.profile.as_ref().map(|profile| {
            let main = program.main();
            let source_map = &main.source_map;
            VmProfile {
                span_source: source_map.iter().map(|span| span.source.0).collect(),
                span_start: source_map.iter().map(|span| span.start).collect(),
                span_end: source_map.iter().map(|span| span.end).collect(),
                count: profile
                    .logical_counts()
                    .iter()
                    .map(|&count| count as f64)
                    .collect(),
                combined_dispatch_count: profile.combined_dispatches() as f64,
                combined_dispatch_by_pc: profile
                    .skipped_by_pc()
                    .iter()
                    .map(|&count| count as f64)
                    .collect(),
                units: profile.units.iter().map(|&units| units as f64).collect(),
                flow: main
                    .code
                    .iter()
                    .map(|instruction| {
                        u8::from(matches!(
                            instruction,
                            CsilInstr::Jump { .. }
                                | CsilInstr::JumpIfFalse { .. }
                                | CsilInstr::JumpIfExact { .. }
                                | CsilInstr::JumpIfSlotWritten { .. }
                                | CsilInstr::Return
                        ))
                    })
                    .collect(),
            }
        });

        RunOutcome {
            plots,
            visuals,
            fills,
            errors,
            bars_executed,
            realtime_trace,
            drawings: self.serialize_drawings(),
            logs: self.logs.clone(),
            alerts,
            strategy,
            profile,
        }
    }

    fn fail(&self, kind: RuntimeErrorKind, message: String, pc: usize) -> RuntimeError {
        let (bar_index, instruction_offset, span) =
            vm_fault_location::fault_location(self.bar, pc, &self.program.main().source_map);
        RuntimeError {
            kind,
            message,
            bar_index: Some(bar_index),
            instruction_offset: Some(instruction_offset),
            span,
        }
    }

    /// Mirror an "honest metering" extra charge into the profiling counters.
    /// Call next to every `budget -= extra` in the dispatch arms so a
    /// profiled instruction's `units` equal exactly what it charged against
    /// the per-bar budget.
    #[inline(always)]
    fn profile_extra(&mut self, pc: usize, extra: u64) {
        if let Some(p) = self.profile.as_mut() {
            vm_charge::profile_extra_units(&mut p.units, pc, extra);
        }
    }

    /// Charge both independent instruction ceilings before an instruction (or
    /// its linear-cost extra work) performs effects. Per-bar exhaustion wins
    /// when both counters are short, preserving the established error kind.
    #[inline(always)]
    fn charge_instructions(
        &mut self,
        budget: &mut u64,
        cost: u64,
        pc: usize,
    ) -> Result<(), RuntimeError> {
        match vm_charge::charge_counters(budget, &mut self.instructions_remaining, cost) {
            Ok(()) => Ok(()),
            Err(limit) => Err(self.limit_error(limit, pc)),
        }
    }

    /// The `RuntimeError` for an exhausted instruction ceiling at `pc`.
    #[cold]
    #[inline(never)]
    fn limit_error(&self, limit: vm_charge::ChargeLimit, pc: usize) -> RuntimeError {
        match limit {
            vm_charge::ChargeLimit::Bar => self.fail(
                RuntimeErrorKind::InstructionLimitExceeded,
                format!(
                    "bar exceeded the limit of {} instructions",
                    self.options.max_instructions_per_bar
                ),
                pc,
            ),
            vm_charge::ChargeLimit::Run => self.fail(
                RuntimeErrorKind::RunInstructionLimitExceeded,
                format!(
                    "run exceeded the cumulative limit of {} instructions",
                    self.options.max_instructions_per_run
                ),
                pc,
            ),
        }
    }

    /// Charge work performed inside one already-dispatched instruction. The
    /// dispatch loop (or AOT host shim) has consumed the first unit; callers
    /// pass only the additional units their input scan/output construction
    /// requires.
    #[inline]
    fn charge_extra_work(
        &mut self,
        budget: &mut u64,
        extra: u64,
        pc: usize,
    ) -> Result<(), RuntimeError> {
        // Fixed-cost calls already paid their dispatch unit. In particular,
        // numeric pure calls and array.get have no additional work to meter.
        if extra == 0 {
            return Ok(());
        }
        self.charge_instructions(budget, extra, pc)?;
        self.profile_extra(pc, extra);
        Ok(())
    }

    /// Reserve runtime-created string bytes before the allocation whenever a
    /// safe projection is available. Reservations are cumulative by design:
    /// repeatedly allocating and dropping large strings is still bounded.
    fn reserve_string_bytes(&mut self, bytes: usize, pc: usize) -> Result<(), RuntimeError> {
        vm_heap_ops::reserve_string_bytes(
            &mut self.string_bytes_allocated,
            self.options.max_string_bytes_per_run,
            bytes,
        )
        .map_err(|attempted| self.string_limit_error(attempted, pc))
    }

    /// Allocate an object after its string bytes were reserved. If the heap
    /// rejects the object, restore exactly the counter value from before that
    /// reservation so a failed clone does not consume the cumulative budget.
    fn alloc_after_string_charge(
        &mut self,
        obj: HeapObj,
        charged_before: usize,
        pc: usize,
    ) -> Result<HandleId, RuntimeError> {
        match self.heap.alloc(obj) {
            Ok(handle) => Ok(handle),
            Err(error) => {
                self.string_bytes_allocated = charged_before;
                Err(self.heap_limit_err(error, pc))
            }
        }
    }

    /// The `StringLimitExceeded` error for a failed cumulative-string charge.
    #[cold]
    fn string_limit_error(&self, attempted: usize, pc: usize) -> RuntimeError {
        self.fail(
            RuntimeErrorKind::StringLimitExceeded,
            format!(
                "run exceeded the cumulative string limit of {} bytes (attempted {attempted})",
                self.options.max_string_bytes_per_run
            ),
            pc,
        )
    }

    /// Map a [`vm_heap_ops`] kernel failure to the `RuntimeError` (kind and
    /// message) the inline handler code raised. `args` are the collection
    /// call's arguments, `value_arg` the index of the element argument.
    #[cold]
    fn heap_op_error(
        &self,
        func: CollectionFunc,
        error: vm_heap_ops::HeapOpError,
        args: &[Value],
        value_arg: usize,
        pc: usize,
    ) -> RuntimeError {
        use vm_heap_ops::HeapOpError as E;
        match error {
            E::NaReceiver => self.fail(
                RuntimeErrorKind::NaMethodReceiver,
                format!(
                    "cannot call `{}` on an na collection (the collection variable was never assigned)",
                    func.name()
                ),
                pc,
            ),
            E::NotCollection => self.fail(
                RuntimeErrorKind::TypeMismatch,
                format!(
                    "`{}` expects a collection receiver, found {}",
                    func.name(),
                    args.first().map_or("na", Value::type_name)
                ),
                pc,
            ),
            E::NotArray => self.fail(
                RuntimeErrorKind::InvalidProgram,
                format!("`{}` receiver is not an array", func.name()),
                pc,
            ),
            E::IndexNa => self.fail(
                RuntimeErrorKind::CollectionIndexOutOfBounds,
                format!("`{}` index is na", func.name()),
                pc,
            ),
            E::OutOfBounds { index, len } => self.oob_error(func, index, len, pc),
            E::ElemMismatch(elem_ty) => self.fail(
                RuntimeErrorKind::TypeMismatch,
                format!(
                    "`{}` element type mismatch: array holds {}, got {}",
                    func.name(),
                    elem_ty.describe(),
                    args.get(value_arg).map_or("na", Value::type_name)
                ),
                pc,
            ),
            E::Heap(e) => self.heap_limit_err(e, pc),
            E::StringLimit { attempted } => self.string_limit_error(attempted, pc),
            E::StringCloneOverflow => self.fail(
                RuntimeErrorKind::StringLimitExceeded,
                "repeated collection string-clone size overflowed".to_string(),
                pc,
            ),
            E::DimTooLarge => self.fail(
                RuntimeErrorKind::CollectionLimitExceeded,
                format!("`{}` dimension is too large", func.name()),
                pc,
            ),
            E::DimNegative => self.fail(
                RuntimeErrorKind::CollectionIndexOutOfBounds,
                format!("`{}` dimension must be non-negative", func.name()),
                pc,
            ),
            E::DimNa => self.fail(
                RuntimeErrorKind::CollectionIndexOutOfBounds,
                format!("`{}` dimension is na", func.name()),
                pc,
            ),
            E::CountOverflow => self.fail(
                RuntimeErrorKind::CollectionLimitExceeded,
                "matrix element count overflows".to_string(),
                pc,
            ),
        }
    }

    /// Reserve VM-owned dynamic memory before materializing a table cell or
    /// appending an event. Reservations are cumulative to prevent churn from
    /// bypassing the cap after cells/events are cleared or drained.
    fn reserve_vm_bytes(&mut self, bytes: usize, pc: usize) -> Result<(), RuntimeError> {
        match vm_memory_reservation::reserve(
            &mut self.bytes_reserved,
            bytes,
            self.transient_vm_bytes,
            self.options.max_vm_bytes_per_run,
        ) {
            Ok(()) => (),
            Err(vm_memory_reservation::ReservationError::Overflow) => {
                return Err(self.fail(
                    RuntimeErrorKind::VmMemoryLimitExceeded,
                    format!(
                        "VM memory accounting overflowed; limit is {} bytes",
                        self.options.max_vm_bytes_per_run
                    ),
                    pc,
                ));
            }
            Err(vm_memory_reservation::ReservationError::LimitExceeded(peak)) => {
                return Err(self.fail(
                    RuntimeErrorKind::VmMemoryLimitExceeded,
                    format!(
                        "run exceeded the VM memory limit of {} bytes (attempted {peak})",
                        self.options.max_vm_bytes_per_run
                    ),
                    pc,
                ));
            }
        }
        Ok(())
    }

    /// Admit the temporary input/trace payload of one incremental call without
    /// charging it cumulatively. Persistent growth performed during the call
    /// still reserves against this transient peak through [`Self::reserve_vm_bytes`].
    ///
    /// `collect_trace` is the caller's decision rather than
    /// [`RunOptions::realtime_trace`] because
    /// [`IncrementalSession::advance_samples`] collects samples whether or not
    /// the option is set; reserving from the option there would admit a payload
    /// the call does not build.
    fn begin_incremental_advance(
        &mut self,
        ticks: usize,
        collect_trace: bool,
    ) -> Result<(), RuntimeError> {
        let transient = self
            .incremental_advance_transient_bytes(ticks, collect_trace)
            .ok_or_else(|| {
                vm_memory_limit_error(
                    self.options.max_vm_bytes_per_run,
                    "incremental realtime request size arithmetic overflowed",
                )
            })?;
        let peak = self.bytes_reserved.checked_add(transient).ok_or_else(|| {
            vm_memory_limit_error(
                self.options.max_vm_bytes_per_run,
                "incremental realtime peak size arithmetic overflowed",
            )
        })?;
        if peak > self.options.max_vm_bytes_per_run {
            return Err(vm_memory_limit_error(
                self.options.max_vm_bytes_per_run,
                &format!("incremental realtime request requires {peak} peak bytes"),
            ));
        }
        self.transient_vm_bytes = transient;
        Ok(())
    }

    fn incremental_advance_transient_bytes(
        &self,
        ticks: usize,
        collect_trace: bool,
    ) -> Option<usize> {
        use std::mem::size_of;

        let mut bytes = ticks.checked_mul(size_of::<Tick>())?;
        if collect_trace {
            let sample_bytes = size_of::<PlotTickSample>()
                .checked_add(self.program.plots.len().checked_mul(size_of::<Value>())?)?;
            bytes = bytes.checked_add(ticks.checked_mul(sample_bytes)?)?;
        }
        Some(bytes)
    }

    fn end_incremental_advance(&mut self) {
        self.transient_vm_bytes = 0;
    }

    /// Reserve persistent feed and output growth for an append not already
    /// covered by a one-shot realtime preflight. `tick_trading_day` says
    /// whether the appended tick carries a `tradingDay`, the only case in
    /// which an append materializes an absent trading-day lane.
    fn reserve_realtime_append(&mut self, tick_trading_day: bool) -> Result<(), RuntimeError> {
        if self.preallocated_realtime_appends > 0 {
            self.preallocated_realtime_appends -= 1;
            return Ok(());
        }

        let bytes = self
            .realtime_append_reservation_bytes(tick_trading_day)
            .ok_or_else(|| {
                vm_memory_limit_error(
                    self.options.max_vm_bytes_per_run,
                    "realtime bar size arithmetic overflowed",
                )
            })?;
        let persistent = self.bytes_reserved.checked_add(bytes).ok_or_else(|| {
            vm_memory_limit_error(
                self.options.max_vm_bytes_per_run,
                "realtime VM state size arithmetic overflowed",
            )
        })?;
        let peak = persistent
            .checked_add(self.transient_vm_bytes)
            .ok_or_else(|| {
                vm_memory_limit_error(
                    self.options.max_vm_bytes_per_run,
                    "realtime VM peak size arithmetic overflowed",
                )
            })?;
        if peak > self.options.max_vm_bytes_per_run {
            return Err(vm_memory_limit_error(
                self.options.max_vm_bytes_per_run,
                &format!("realtime VM state requires {peak} peak bytes"),
            ));
        }
        self.bytes_reserved = persistent;
        Ok(())
    }

    fn realtime_append_reservation_bytes(&self, tick_trading_day: bool) -> Option<usize> {
        use std::mem::size_of;

        let mut bytes = 7usize
            .checked_mul(size_of::<f64>())?
            .checked_add(2usize.checked_mul(size_of::<i64>())?)?;
        let output_lanes = self
            .plot_colors
            .iter()
            .chain(&self.visual_colors)
            .chain(&self.fill_colors)
            .try_fold(self.plots.len(), |sum, channels| {
                sum.checked_add(channels.len())
            })?;
        bytes = bytes.checked_add(
            output_lanes
                .checked_mul(2)
                .and_then(|lanes| lanes.checked_mul(size_of::<Value>()))?,
        )?;
        // Each color role also grows its selected-palette identity snapshot.
        let color_roles = output_lanes.checked_sub(self.plots.len())?;
        bytes = bytes.checked_add(color_roles.checked_mul(size_of::<Option<u32>>())?)?;
        if self.broker.is_some() {
            bytes = bytes.checked_add(STRATEGY_BAR_RESERVATION_BYTES)?;
        }
        let synthesized_identity_lanes = usize::from(self.data.time.is_empty()).checked_add(
            usize::from(self.data.trading_day.is_empty() && tick_trading_day),
        )?;
        bytes = bytes.checked_add(
            self.bars
                .checked_mul(synthesized_identity_lanes)
                .and_then(|cells| cells.checked_mul(size_of::<i64>()))?,
        )?;
        Some(bytes)
    }

    /// Project and reserve one script-selected resample cache before its
    /// `Vec` storage is materialized. Overflow is a structured memory-limit
    /// failure, never an allocator request derived from wrapped arithmetic.
    fn reserve_resample_cache(
        &mut self,
        per_bar_bytes: usize,
        fixed_bytes: usize,
        pc: usize,
    ) -> Result<(), RuntimeError> {
        let bytes = vm_resample_reservation::project(self.bars, per_bar_bytes, fixed_bytes)
            .ok_or_else(|| {
                self.fail(
                    RuntimeErrorKind::VmMemoryLimitExceeded,
                    format!(
                        "resample cache size overflowed; VM memory limit is {} bytes",
                        self.options.max_vm_bytes_per_run
                    ),
                    pc,
                )
            })?;
        self.reserve_vm_bytes(bytes, pc)
    }

    #[inline(always)]
    fn pop(&mut self, pc: usize) -> Result<Value, RuntimeError> {
        match self.stack.pop() {
            Some(value) => Ok(value),
            None => Err(self.underflow(pc)),
        }
    }

    /// Out-of-line stack-underflow constructor so `pop`'s hot path inlines
    /// into the dispatch loop without dragging the error formatting along.
    #[cold]
    #[inline(never)]
    fn underflow(&self, pc: usize) -> RuntimeError {
        self.fail(
            RuntimeErrorKind::StackUnderflow,
            "operand stack underflow (validator should have rejected this program)".into(),
            pc,
        )
    }

    /// Out-of-line error constructor for the per-instruction helpers: the
    /// message closure (and its `format!` machinery) is compiled into this
    /// cold, never-inlined instantiation instead of the caller, so helpers
    /// like `pop_float` stay small enough to inline into the dispatch loop
    /// and pay nothing for their error arm until it is actually taken.
    #[cold]
    #[inline(never)]
    fn fail_with(
        &self,
        kind: RuntimeErrorKind,
        message: impl FnOnce() -> String,
        pc: usize,
    ) -> RuntimeError {
        self.fail(kind, message(), pc)
    }

    /// Move the top `n` operands (argument order preserved: the last
    /// argument was on top) into `buf[..n]`. The stack-local fixed buffer is
    /// the fast path for call instructions — N plain value moves, no Vec
    /// growth checks, no drain iterator, no allocation. The caller has
    /// already checked `stack.len() >= n` and `n <= buf.len()`; this helper
    /// still bounds `n` and uses fallible pops so a violated internal
    /// precondition yields `na` arguments instead of an indexing panic.
    #[inline]
    fn fill_call_args(&mut self, buf: &mut [Value], n: usize) {
        let n = n.min(buf.len());
        for slot in buf[..n].iter_mut().rev() {
            *slot = self.stack.pop().unwrap_or(Value::Na);
        }
    }

    /// Move the top `n` operands into the reusable call-argument scratch
    /// buffer (preserving argument order) and hand it out — the spill path
    /// for calls wider than [`INLINE_CALL_ARGS`]. The caller passes
    /// `&buffer` to the builtin, then MUST hand the buffer back through
    /// [`Vm::restore_call_args`] on the success path so its capacity is
    /// reused by the next call instruction (an error return may skip it —
    /// the run is over). The caller has already checked `stack.len() >= n`.
    #[inline]
    fn take_call_args(&mut self, n: usize) -> Vec<Value> {
        let mut args = std::mem::take(&mut self.call_args);
        args.clear();
        args.extend(self.stack.drain(self.stack.len() - n..));
        args
    }

    /// Return the scratch buffer taken by [`Vm::take_call_args`], dropping
    /// this call's argument values but keeping the allocation.
    #[inline]
    fn restore_call_args(&mut self, mut args: Vec<Value>) {
        args.clear();
        self.call_args = args;
    }

    #[inline(always)]
    fn pop_float(&mut self, pc: usize, what: &str) -> Result<Option<f64>, RuntimeError> {
        match vm_ops::float_operand(self.pop(pc)?.into_na_observation()) {
            Ok(operand) => Ok(operand),
            Err(other) => Err(self.fail_with(
                RuntimeErrorKind::TypeMismatch,
                || format!("{what} requires a float, found {}", other.type_name()),
                pc,
            )),
        }
    }

    #[inline(always)]
    fn pop_int(&mut self, pc: usize, what: &str) -> Result<Option<i64>, RuntimeError> {
        match self.pop(pc)? {
            Value::Na | Value::DisplacedTable(_) => Ok(None),
            Value::Int(v) => Ok(Some(v)),
            other => Err(self.fail_with(
                RuntimeErrorKind::TypeMismatch,
                || format!("{what} requires an int, found {}", other.type_name()),
                pc,
            )),
        }
    }

    #[inline(always)]
    fn pop_bool(&mut self, pc: usize, what: &str) -> Result<Option<bool>, RuntimeError> {
        match vm_ops::bool_operand(self.pop(pc)?.into_na_observation()) {
            Ok(operand) => Ok(operand),
            Err(other) => Err(self.fail_with(
                RuntimeErrorKind::TypeMismatch,
                || format!("{what} requires a bool, found {}", other.type_name()),
                pc,
            )),
        }
    }

    #[inline(always)]
    fn push(&mut self, value: Value, pc: usize) -> Result<(), RuntimeError> {
        if !vm_stack_push::push_within_limit(&mut self.stack, self.options.max_stack_depth, value) {
            return Err(self.overflow(pc));
        }
        Ok(())
    }

    /// Out-of-line stack-overflow constructor; see [`Vm::underflow`].
    #[cold]
    #[inline(never)]
    fn overflow(&self, pc: usize) -> RuntimeError {
        self.fail(
            RuntimeErrorKind::StackOverflow,
            format!(
                "operand stack exceeded {} values",
                self.options.max_stack_depth
            ),
            pc,
        )
    }

    /// Value of a builtin series at `bar - offset`; `na` before bar 0.
    /// Non-finite input data is normalized to `na` at the read boundary.
    fn runtime_history_offset(value: Value) -> Option<u64> {
        vm_ops::history_offset(&value)
    }

    fn check_dynamic_history_offset(&self, offset: u64, pc: usize) -> Result<u32, RuntimeError> {
        let limit = self.program.metadata.max_bars_back;
        match vm_ops::dynamic_offset_limit(offset, limit) {
            Some(offset) => Ok(offset),
            None => Err(self.dynamic_history_too_deep(offset, pc)),
        }
    }

    /// The structured error of a dynamic history offset above
    /// `max_bars_back`.
    #[cold]
    fn dynamic_history_too_deep(&self, offset: u64, pc: usize) -> RuntimeError {
        let limit = self.program.metadata.max_bars_back;
        self.fail(
            RuntimeErrorKind::HistoryBufferLimitExceeded,
            format!("history offset {offset} exceeds the max_bars_back limit of {limit}"),
            pc,
        )
    }

    /// A feed series `offset` bars back (`vm_ops::feed_series_read`).
    #[inline(always)]
    fn read_feed(&self, series: vm_ops::FeedSeries, offset: u32) -> Value {
        vm_ops::feed_series_read(series, self.bar, offset, self.bars, &self.feed_cols())
    }

    /// The market-data lanes of the feed kernel.
    #[inline(always)]
    fn feed_cols(&self) -> vm_ops::FeedCols<'_> {
        vm_ops::FeedCols {
            open: &self.data.open,
            high: &self.data.high,
            low: &self.data.low,
            close: &self.data.close,
            volume: &self.data.volume,
            time: &self.data.time,
            bid: &self.data.bid,
            ask: &self.data.ask,
        }
    }

    #[inline]
    fn read_builtin(&self, source: BuiltinSeries, offset: u32) -> Value {
        if source == BuiltinSeries::Close {
            return vm_close_step::close_value(
                self.bar,
                offset,
                &vm_close_step::Ohlc {
                    open: &self.data.open,
                    high: &self.data.high,
                    low: &self.data.low,
                    close: &self.data.close,
                },
            );
        }
        // OHLCV, the derived averages, `bar_index`, `time`, `last_bar_*`,
        // `barstate.isfirst` and `bid`/`ask`: the pure feed kernel, called
        // with a constant series per arm so that its own dispatch folds away
        // (this table is `vm_ops::feed_series_of`; the tests check both).
        match source {
            BuiltinSeries::Open => return self.read_feed(vm_ops::FeedSeries::Open, offset),
            BuiltinSeries::High => return self.read_feed(vm_ops::FeedSeries::High, offset),
            BuiltinSeries::Low => return self.read_feed(vm_ops::FeedSeries::Low, offset),
            BuiltinSeries::Volume => return self.read_feed(vm_ops::FeedSeries::Volume, offset),
            BuiltinSeries::Hl2 => return self.read_feed(vm_ops::FeedSeries::Hl2, offset),
            BuiltinSeries::Hlc3 => return self.read_feed(vm_ops::FeedSeries::Hlc3, offset),
            BuiltinSeries::Ohlc4 => return self.read_feed(vm_ops::FeedSeries::Ohlc4, offset),
            BuiltinSeries::Hlcc4 => return self.read_feed(vm_ops::FeedSeries::Hlcc4, offset),
            BuiltinSeries::BarIndex => return self.read_feed(vm_ops::FeedSeries::BarIndex, offset),
            BuiltinSeries::Time => return self.read_feed(vm_ops::FeedSeries::Time, offset),
            BuiltinSeries::LastBarTime => {
                return self.read_feed(vm_ops::FeedSeries::LastBarTime, offset)
            }
            BuiltinSeries::LastBarIndex => {
                return self.read_feed(vm_ops::FeedSeries::LastBarIndex, offset)
            }
            BuiltinSeries::BarstateIsfirst => {
                return self.read_feed(vm_ops::FeedSeries::BarstateIsfirst, offset)
            }
            BuiltinSeries::Bid => return self.read_feed(vm_ops::FeedSeries::Bid, offset),
            BuiltinSeries::Ask => return self.read_feed(vm_ops::FeedSeries::Ask, offset),
            _ => {}
        }
        let Some(index) = vm_ops::history_index(self.bar, offset) else {
            return Value::Na;
        };
        // The `strategy.*` read surface reads a frozen per-bar broker
        // snapshot rather than the market data; handle it separately so the
        // market-data match below stays over OHLCV/time/barstate only.
        if source.is_strategy() {
            return self.read_strategy(source, index);
        }
        // The OHLC loads precede every remaining series, as they always
        // have (a short lane panics here, before any series logic).
        let (_o, _h, _l, c) = (
            self.data.open[index],
            self.data.high[index],
            self.data.low[index],
            self.data.close[index],
        );
        // Every numeric or boolean series below is a pure kernel of
        // `vm_ops`, called with a constant series per arm (as the feed
        // series above). The text series, and the two string parses whose
        // results the kernels take (the exchange timezone, the chart
        // timeframe), stay here.
        match source {
            // Read above (`feed_series_read`, `close_value`); unreachable here.
            BuiltinSeries::Open
            | BuiltinSeries::High
            | BuiltinSeries::Low
            | BuiltinSeries::Volume
            | BuiltinSeries::Hl2
            | BuiltinSeries::Hlc3
            | BuiltinSeries::Ohlc4
            | BuiltinSeries::Hlcc4
            | BuiltinSeries::BarIndex
            | BuiltinSeries::Time
            | BuiltinSeries::LastBarTime
            | BuiltinSeries::LastBarIndex
            | BuiltinSeries::BarstateIsfirst
            | BuiltinSeries::Bid
            | BuiltinSeries::Ask => {
                unreachable!("feed series are read by vm_ops::feed_series_read")
            }
            BuiltinSeries::Close => Value::from_f64(c),
            BuiltinSeries::TimeClose => Value::Int(self.data.time_close_at(index)),
            // `timenow`/`last_bar_*` reference the run's last bar; they hold
            // the same value on every bar, so the history offset only shifts
            // which bar we read them "on" — the value is bar-independent. An
            // empty run (bars == 0) can't reach here (index < bar < bars).
            //
            // `timenow` is deterministic here: it equals the last bar's CLOSE
            // time and does not read the wall clock.
            // `last_bar_time` is the last bar's open time.
            BuiltinSeries::Timenow => Value::Int(self.data.time_close_at(self.bars - 1)),
            // Calendar series: decompose the bar open time in the exchange
            // (fixed-offset) timezone. `weekofyear` is the ISO 8601 week
            // (1..=53) of the bar's open date; `na` when the date's ISO
            // Thursday is outside the i64 epoch-ms range (see
            // `iso_week_of_year`).
            BuiltinSeries::DayOfWeek => self.read_calendar(vm_ops::CalendarPart::DayOfWeek, index),
            BuiltinSeries::DayOfMonth => {
                self.read_calendar(vm_ops::CalendarPart::DayOfMonth, index)
            }
            BuiltinSeries::Month => self.read_calendar(vm_ops::CalendarPart::Month, index),
            BuiltinSeries::Year => self.read_calendar(vm_ops::CalendarPart::Year, index),
            BuiltinSeries::Hour => self.read_calendar(vm_ops::CalendarPart::Hour, index),
            BuiltinSeries::Minute => self.read_calendar(vm_ops::CalendarPart::Minute, index),
            BuiltinSeries::Second => self.read_calendar(vm_ops::CalendarPart::Second, index),
            BuiltinSeries::WeekOfYear => {
                self.read_calendar(vm_ops::CalendarPart::WeekOfYear, index)
            }
            // ---- barstate.*: current-bar execution predicates -------
            // Computed for the *referenced* bar `index` (offset already
            // applied), so `barstate.islast[1]` reads the prior bar's value —
            // consistent with every other series read. Historical bars are
            // always new + confirmed + history; the realtime bar carries the
            // per-tick new/confirmed flags and reads as non-history.
            BuiltinSeries::BarstateIslast => {
                self.read_barstate(vm_ops::BarstateSeries::Islast, index)
            }
            BuiltinSeries::BarstateIshistory => {
                self.read_barstate(vm_ops::BarstateSeries::Ishistory, index)
            }
            BuiltinSeries::BarstateIsrealtime => {
                self.read_barstate(vm_ops::BarstateSeries::Isrealtime, index)
            }
            BuiltinSeries::BarstateIsconfirmed => {
                self.read_barstate(vm_ops::BarstateSeries::Isconfirmed, index)
            }
            BuiltinSeries::BarstateIsnew => {
                self.read_barstate(vm_ops::BarstateSeries::Isnew, index)
            }
            BuiltinSeries::BarstateIslastconfirmedhistory => {
                self.read_barstate(vm_ops::BarstateSeries::Islastconfirmedhistory, index)
            }
            // ---- timeframe.*: derived from the chart timeframe string,
            // the same value on every bar. ----
            BuiltinSeries::TimeframePeriod | BuiltinSeries::TimeframeMainPeriod => {
                // `main_period` == `period` here (no secondary contexts).
                Value::String(self.data.timeframe.trim().to_string())
            }
            BuiltinSeries::TimeframeMultiplier => {
                self.read_timeframe(vm_ops::TimeframeSeries::Multiplier)
            }
            BuiltinSeries::TimeframeIsseconds => {
                self.read_timeframe(vm_ops::TimeframeSeries::Isseconds)
            }
            BuiltinSeries::TimeframeIsminutes => {
                self.read_timeframe(vm_ops::TimeframeSeries::Isminutes)
            }
            BuiltinSeries::TimeframeIsdaily => {
                self.read_timeframe(vm_ops::TimeframeSeries::Isdaily)
            }
            BuiltinSeries::TimeframeIsweekly => {
                self.read_timeframe(vm_ops::TimeframeSeries::Isweekly)
            }
            BuiltinSeries::TimeframeIsmonthly => {
                self.read_timeframe(vm_ops::TimeframeSeries::Ismonthly)
            }
            BuiltinSeries::TimeframeIsdwm => self.read_timeframe(vm_ops::TimeframeSeries::Isdwm),
            BuiltinSeries::TimeframeIsintraday => {
                self.read_timeframe(vm_ops::TimeframeSeries::Isintraday)
            }
            // No tick timeframes in this VM.
            BuiltinSeries::TimeframeIsticks => {
                vm_ops::bar_flag_value(vm_ops::BarFlag::TimeframeIsticks, index, self.bars)
            }
            // ---- syminfo.*: host-or-default symbol metadata. ----
            BuiltinSeries::SyminfoTicker => Value::String(self.data.syminfo.ticker.clone()),
            BuiltinSeries::SyminfoTickerid => {
                let syminfo = &self.data.syminfo;
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
            BuiltinSeries::SyminfoBasecurrency => {
                Value::String(self.data.syminfo.basecurrency.clone())
            }
            BuiltinSeries::SyminfoCurrency => Value::String(self.data.syminfo.currency.clone()),
            BuiltinSeries::SyminfoDescription => {
                Value::String(self.data.syminfo.description.clone())
            }
            BuiltinSeries::SyminfoCountry => Value::String(self.data.syminfo.country.clone()),
            BuiltinSeries::SyminfoIndustry => Value::String(self.data.syminfo.industry.clone()),
            BuiltinSeries::SyminfoSector => Value::String(self.data.syminfo.sector.clone()),
            BuiltinSeries::SyminfoType => Value::String(self.data.syminfo.kind.clone()),
            BuiltinSeries::SyminfoRoot => Value::String(self.data.syminfo.root.clone()),
            BuiltinSeries::SyminfoPrefix => Value::String(self.data.syminfo.prefix.clone()),
            BuiltinSeries::SyminfoSession => Value::String(self.data.syminfo.session.clone()),
            BuiltinSeries::SyminfoTimezone => Value::String(self.data.syminfo.timezone.clone()),
            BuiltinSeries::SyminfoVolumetype => Value::String(self.data.syminfo.volumetype.clone()),
            BuiltinSeries::SyminfoIsin => Value::String(self.data.syminfo.isin.clone()),
            BuiltinSeries::SyminfoCurrentContract => {
                let contract = &self.data.syminfo.current_contract;
                if contract.is_empty() {
                    Value::Na
                } else {
                    Value::String(contract.clone())
                }
            }
            BuiltinSeries::SyminfoMainTickerid => {
                Value::String(self.data.syminfo.main_tickerid.clone())
            }
            // ---- The numeric host metadata (`vm_ops::host_series_value`):
            // the numeric `syminfo.*` surface and fundamentals, the
            // visible-bar-time viewport accessors and the
            // `dividends.future_*` / `earnings.future_*` events. Each reads
            // a per-run host value (`na` when an optional field is absent;
            // the headless VM has no viewport). Integer counts / epoch-ms
            // dates are ints; amounts, share counts and price targets are
            // floats (`na` when non-finite). ----
            BuiltinSeries::SyminfoMintick => self.read_host(vm_ops::HostSeries::SyminfoMintick),
            BuiltinSeries::SyminfoMinmove => self.read_host(vm_ops::HostSeries::SyminfoMinmove),
            BuiltinSeries::SyminfoPricescale => {
                self.read_host(vm_ops::HostSeries::SyminfoPricescale)
            }
            BuiltinSeries::SyminfoPointvalue => {
                self.read_host(vm_ops::HostSeries::SyminfoPointvalue)
            }
            BuiltinSeries::SyminfoMincontract => {
                self.read_host(vm_ops::HostSeries::SyminfoMincontract)
            }
            BuiltinSeries::SyminfoExpirationDate => {
                self.read_host(vm_ops::HostSeries::SyminfoExpirationDate)
            }
            BuiltinSeries::SyminfoEmployees => self.read_host(vm_ops::HostSeries::SyminfoEmployees),
            BuiltinSeries::SyminfoShareholders => {
                self.read_host(vm_ops::HostSeries::SyminfoShareholders)
            }
            BuiltinSeries::SyminfoSharesOutstandingFloat => {
                self.read_host(vm_ops::HostSeries::SyminfoSharesOutstandingFloat)
            }
            BuiltinSeries::SyminfoSharesOutstandingTotal => {
                self.read_host(vm_ops::HostSeries::SyminfoSharesOutstandingTotal)
            }
            BuiltinSeries::SyminfoRecommendationsBuy => {
                self.read_host(vm_ops::HostSeries::SyminfoRecommendationsBuy)
            }
            BuiltinSeries::SyminfoRecommendationsBuyStrong => {
                self.read_host(vm_ops::HostSeries::SyminfoRecommendationsBuyStrong)
            }
            BuiltinSeries::SyminfoRecommendationsDate => {
                self.read_host(vm_ops::HostSeries::SyminfoRecommendationsDate)
            }
            BuiltinSeries::SyminfoRecommendationsHold => {
                self.read_host(vm_ops::HostSeries::SyminfoRecommendationsHold)
            }
            BuiltinSeries::SyminfoRecommendationsSell => {
                self.read_host(vm_ops::HostSeries::SyminfoRecommendationsSell)
            }
            BuiltinSeries::SyminfoRecommendationsSellStrong => {
                self.read_host(vm_ops::HostSeries::SyminfoRecommendationsSellStrong)
            }
            BuiltinSeries::SyminfoRecommendationsTotal => {
                self.read_host(vm_ops::HostSeries::SyminfoRecommendationsTotal)
            }
            BuiltinSeries::SyminfoTargetPriceAverage => {
                self.read_host(vm_ops::HostSeries::SyminfoTargetPriceAverage)
            }
            BuiltinSeries::SyminfoTargetPriceDate => {
                self.read_host(vm_ops::HostSeries::SyminfoTargetPriceDate)
            }
            BuiltinSeries::SyminfoTargetPriceEstimates => {
                self.read_host(vm_ops::HostSeries::SyminfoTargetPriceEstimates)
            }
            BuiltinSeries::SyminfoTargetPriceHigh => {
                self.read_host(vm_ops::HostSeries::SyminfoTargetPriceHigh)
            }
            BuiltinSeries::SyminfoTargetPriceLow => {
                self.read_host(vm_ops::HostSeries::SyminfoTargetPriceLow)
            }
            BuiltinSeries::SyminfoTargetPriceMedian => {
                self.read_host(vm_ops::HostSeries::SyminfoTargetPriceMedian)
            }
            BuiltinSeries::ChartLeftVisibleBarTime => {
                self.read_host(vm_ops::HostSeries::ChartLeftVisibleBarTime)
            }
            BuiltinSeries::ChartRightVisibleBarTime => {
                self.read_host(vm_ops::HostSeries::ChartRightVisibleBarTime)
            }
            BuiltinSeries::DividendsFutureAmount => {
                self.read_host(vm_ops::HostSeries::DividendsFutureAmount)
            }
            BuiltinSeries::DividendsFutureExDate => {
                self.read_host(vm_ops::HostSeries::DividendsFutureExDate)
            }
            BuiltinSeries::DividendsFuturePayDate => {
                self.read_host(vm_ops::HostSeries::DividendsFuturePayDate)
            }
            BuiltinSeries::EarningsFutureEps => {
                self.read_host(vm_ops::HostSeries::EarningsFutureEps)
            }
            BuiltinSeries::EarningsFuturePeriodEndTime => {
                self.read_host(vm_ops::HostSeries::EarningsFuturePeriodEndTime)
            }
            BuiltinSeries::EarningsFutureRevenue => {
                self.read_host(vm_ops::HostSeries::EarningsFutureRevenue)
            }
            BuiltinSeries::EarningsFutureTime => {
                self.read_host(vm_ops::HostSeries::EarningsFutureTime)
            }
            // ---- session.*: this headless VM models one continuous
            // regular session, so the extended-session predicates are false and
            // the regular ones track the first/last bar of the whole run. ----
            BuiltinSeries::SessionIsfirstbar | BuiltinSeries::SessionIsfirstbarRegular => {
                vm_ops::bar_flag_value(vm_ops::BarFlag::SessionIsfirstbar, index, self.bars)
            }
            BuiltinSeries::SessionIslastbar | BuiltinSeries::SessionIslastbarRegular => {
                vm_ops::bar_flag_value(vm_ops::BarFlag::SessionIslastbar, index, self.bars)
            }
            BuiltinSeries::SessionIsmarket => {
                vm_ops::bar_flag_value(vm_ops::BarFlag::SessionIsmarket, index, self.bars)
            }
            BuiltinSeries::SessionIspremarket => {
                vm_ops::bar_flag_value(vm_ops::BarFlag::SessionIspremarket, index, self.bars)
            }
            BuiltinSeries::SessionIspostmarket => {
                vm_ops::bar_flag_value(vm_ops::BarFlag::SessionIspostmarket, index, self.bars)
            }
            // Symbol trading day supplied by the host, with exchange-local
            // calendar midnight as the deterministic no-lane fallback.
            BuiltinSeries::TimeTradingday => Value::Int(self.data.trading_day_at(index)),
            // The `strategy.*` getters are dispatched to `read_strategy` before
            // this market-data match (see the early return above), so no other
            // variant reaches here.
            _ => unreachable!("strategy getter reached market-data match"),
        }
    }

    /// A calendar series at bar `index` (`vm_ops::calendar_series_value`),
    /// on the exchange's clock at that bar's open.
    #[inline(always)]
    fn read_calendar(&self, part: vm_ops::CalendarPart, index: usize) -> Value {
        let time = self.data.time_at(index);
        vm_ops::calendar_series_value(part, time, self.data.exchange_offset_at(time))
    }

    /// A `barstate.*` series at bar `index` (`vm_ops::barstate_value`).
    #[inline(always)]
    fn read_barstate(&self, series: vm_ops::BarstateSeries, index: usize) -> Value {
        vm_ops::barstate_value(
            series,
            index,
            &vm_ops::BarState {
                realtime_bar: self.realtime_bar,
                realtime_active: self.realtime_active,
                has_realtime: self.has_realtime,
                bar_is_new: self.bar_is_new,
                bar_is_confirmed: self.bar_is_confirmed,
                bars: self.bars,
            },
        )
    }

    /// A `timeframe.*` series (`vm_ops::timeframe_series_value`) of the
    /// parsed chart timeframe.
    #[inline(always)]
    fn read_timeframe(&self, series: vm_ops::TimeframeSeries) -> Value {
        let (mult, unit) = timeframe_parts(&self.data.timeframe);
        vm_ops::timeframe_series_value(series, mult, unit)
    }

    /// A numeric host-metadata series (`vm_ops::host_series_value`).
    #[inline(always)]
    fn read_host(&self, series: vm_ops::HostSeries) -> Value {
        vm_ops::host_series_value(
            series,
            &vm_ops::HostCols {
                syminfo: &self.data.syminfo,
                events: &self.data.upcoming_events,
                mintick: self.data.mintick,
                chart_left_visible_bar_time: self.data.chart_left_visible_bar_time,
                chart_right_visible_bar_time: self.data.chart_right_visible_bar_time,
            },
        )
    }

    /// Read a `strategy.*` getter off the frozen per-bar broker snapshot
    /// at `index`. A missing snapshot — no broker (impossible: E0241 gates these
    /// at compile time), a bar not yet recorded, or an out-of-range history
    /// offset — reads `na`; `from_f64` maps a `NaN` metric (flat position, no
    /// trades) to `na` too. `source` is guaranteed to satisfy `is_strategy()`.
    fn read_strategy(&self, source: BuiltinSeries, index: usize) -> Value {
        let Some(snap) = self.broker.as_ref().and_then(|b| b.snapshot_at(index)) else {
            return Value::Na;
        };
        Self::strategy_value(source, snap)
    }

    /// A `strategy.*` getter on a recorded snapshot: the numeric getters
    /// are `vm_ops::strategy_series_value`, called with a constant series
    /// per arm; the two text getters stay here.
    #[inline(always)]
    fn strategy_value(source: BuiltinSeries, snap: &broker::StrategySnapshot) -> Value {
        match source {
            BuiltinSeries::StrategyPositionSize => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::PositionSize, snap)
            }
            BuiltinSeries::StrategyPositionAvgPrice => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::PositionAvgPrice, snap)
            }
            BuiltinSeries::StrategyOpenProfit => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::OpenProfit, snap)
            }
            BuiltinSeries::StrategyNetProfit => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::NetProfit, snap)
            }
            BuiltinSeries::StrategyGrossProfit => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::GrossProfit, snap)
            }
            BuiltinSeries::StrategyGrossLoss => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::GrossLoss, snap)
            }
            BuiltinSeries::StrategyEquity => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::Equity, snap)
            }
            BuiltinSeries::StrategyInitialCapital => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::InitialCapital, snap)
            }
            BuiltinSeries::StrategyWinTrades => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::WinTrades, snap)
            }
            BuiltinSeries::StrategyLossTrades => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::LossTrades, snap)
            }
            BuiltinSeries::StrategyEvenTrades => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::EvenTrades, snap)
            }
            BuiltinSeries::StrategyClosedTrades => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::ClosedTrades, snap)
            }
            BuiltinSeries::StrategyOpenTrades => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::OpenTrades, snap)
            }
            BuiltinSeries::StrategyMaxDrawdown => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::MaxDrawdown, snap)
            }
            BuiltinSeries::StrategyMaxRunup => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::MaxRunup, snap)
            }
            BuiltinSeries::StrategyAvgTrade => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::AvgTrade, snap)
            }
            BuiltinSeries::StrategyAvgWinningTrade => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::AvgWinningTrade, snap)
            }
            BuiltinSeries::StrategyAvgLosingTrade => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::AvgLosingTrade, snap)
            }
            BuiltinSeries::StrategyMaxContractsHeldAll => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::MaxContractsHeldAll, snap)
            }
            BuiltinSeries::StrategyMaxContractsHeldLong => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::MaxContractsHeldLong, snap)
            }
            BuiltinSeries::StrategyMaxContractsHeldShort => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::MaxContractsHeldShort, snap)
            }
            BuiltinSeries::StrategyAccountCurrency => Value::String(snap.account_currency.clone()),
            BuiltinSeries::StrategyAvgLosingTradePercent => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::AvgLosingTradePercent, snap)
            }
            BuiltinSeries::StrategyAvgTradePercent => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::AvgTradePercent, snap)
            }
            BuiltinSeries::StrategyAvgWinningTradePercent => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::AvgWinningTradePercent, snap)
            }
            BuiltinSeries::StrategyClosedTradesFirstIndex => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::ClosedTradesFirstIndex, snap)
            }
            BuiltinSeries::StrategyGrossLossPercent => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::GrossLossPercent, snap)
            }
            BuiltinSeries::StrategyGrossProfitPercent => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::GrossProfitPercent, snap)
            }
            BuiltinSeries::StrategyMarginLiquidationPrice => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::MarginLiquidationPrice, snap)
            }
            BuiltinSeries::StrategyMaxDrawdownPercent => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::MaxDrawdownPercent, snap)
            }
            BuiltinSeries::StrategyMaxRunupPercent => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::MaxRunupPercent, snap)
            }
            BuiltinSeries::StrategyNetProfitPercent => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::NetProfitPercent, snap)
            }
            BuiltinSeries::StrategyOpenProfitPercent => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::OpenProfitPercent, snap)
            }
            BuiltinSeries::StrategyOpenTradesCapitalHeld => {
                vm_ops::strategy_series_value(vm_ops::StrategySeries::OpenTradesCapitalHeld, snap)
            }
            BuiltinSeries::StrategyPositionEntryName => snap
                .position_entry_name
                .as_ref()
                .map_or(Value::Na, |name| Value::String(name.clone())),
            // Unreachable: the caller guards on `is_strategy()`.
            _ => Value::Na,
        }
    }

    // ---- request.security ------------------------------------------------

    /// Validate `htf_seconds` against the ACTUAL chart feed timeframe carried
    /// on `MarketData` (a runtime property the compile-time checker cannot
    /// see — it only sanity-bounds the constant timeframe string). The honest
    /// same-feed resample model can only aggregate the chart feed UP to a
    /// higher timeframe, so a request lower than the real chart timeframe is
    /// rejected here rather than silently resampling into a nonsensical
    /// (upside-down) bucketing.
    fn check_security_timeframe(&self, htf_seconds: i64, pc: usize) -> Result<(), RuntimeError> {
        let chart_seconds = timeframe_in_seconds(&self.data.timeframe);
        // The admission test is the extracted kernel.
        if !resample::security_timeframe_admitted(chart_seconds, htf_seconds) {
            return Err(self.fail(
                RuntimeErrorKind::SecurityTimeframeBelowChart,
                format!(
                    "`request.security` timeframe ({htf_seconds}s) is lower than the chart \
                     timeframe ({chart_seconds}s); the feed can only be aggregated to a higher \
                     timeframe"
                ),
                pc,
            ));
        }
        Ok(())
    }

    /// Request timeframes are run-constant: positive seconds are literals,
    /// zero means the chart period, and `-(input_id + 1)` names a string input.
    /// Resolve the latter on each run so host overrides affect resampling.
    fn resolve_security_timeframe(
        &self,
        htf_seconds: i64,
        timeframe_expr: Option<&str>,
        calendar_months: Option<u32>,
        pc: usize,
    ) -> Result<(i64, resample::CalendarAlignment), RuntimeError> {
        if timeframe_expr.is_none() && htf_seconds > 0 {
            return Ok((
                htf_seconds,
                calendar_months.map_or(
                    resample::CalendarAlignment::Fixed,
                    resample::CalendarAlignment::Months,
                ),
            ));
        }
        let selected_expression = if let Some(encoded) = timeframe_expr {
            let encoded = encoded.strip_prefix("@input_expr+").ok_or_else(|| {
                self.fail(
                    RuntimeErrorKind::InvalidProgram,
                    "invalid request timeframe expression".into(),
                    pc,
                )
            })?;
            if encoded.len() > 128 * 1024 || encoded.len() % 2 != 0 {
                return Err(self.fail(
                    RuntimeErrorKind::InvalidProgram,
                    "request timeframe expression is out of range".into(),
                    pc,
                ));
            }
            let bytes = encoded
                .as_bytes()
                .chunks_exact(2)
                .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| {
                    self.fail(
                        RuntimeErrorKind::InvalidProgram,
                        "invalid request timeframe encoding".into(),
                        pc,
                    )
                })?;
            let expression = serde_json::from_slice::<InputActiveExpr>(&bytes).map_err(|_| {
                self.fail(
                    RuntimeErrorKind::InvalidProgram,
                    "invalid request timeframe expression".into(),
                    pc,
                )
            })?;
            let Some(InputValue::String(timeframe)) =
                self.request_input_symbol_value(&expression, 0)
            else {
                return Err(self.fail(
                    RuntimeErrorKind::InvalidProgram,
                    "request timeframe did not resolve to a string".into(),
                    pc,
                ));
            };
            Some(timeframe)
        } else {
            None
        };
        let timeframe = if let Some(ref selected) = selected_expression {
            if selected.is_empty() {
                self.data.timeframe.as_str()
            } else {
                selected.as_str()
            }
        } else if htf_seconds == 0 {
            self.data.timeframe.as_str()
        } else {
            let input = u32::try_from(-i128::from(htf_seconds) - 1).map_err(|_| {
                self.fail(
                    RuntimeErrorKind::InvalidProgram,
                    "request.security timeframe input id is out of range".into(),
                    pc,
                )
            })?;
            self.security_input_timeframe(input).ok_or_else(|| {
                self.fail(
                    RuntimeErrorKind::InvalidProgram,
                    format!("request.security timeframe input {input} is unavailable"),
                    pc,
                )
            })?
        };
        let seconds = timeframe_in_seconds(timeframe);
        const MAX_TF_SECONDS: i64 = 100 * 366 * 86_400;
        if seconds > MAX_TF_SECONDS {
            return Err(self.fail(
                RuntimeErrorKind::Unsupported,
                format!("request.security timeframe `{timeframe}` is out of range"),
                pc,
            ));
        }
        Ok((seconds, chart_calendar_alignment(timeframe)))
    }

    /// Stop the run when the feed's bars cannot be grouped into `seconds`
    /// buckets yet ([`MarketData::bucket_refusal`]).
    fn refuse_unsettled_buckets(
        &self,
        what: &str,
        seconds: i64,
        calendar: resample::CalendarAlignment,
        host_feeds: bool,
        pc: usize,
    ) -> Result<(), RuntimeError> {
        match self.data.bucket_refusal(seconds, calendar, host_feeds) {
            Some(reason) => Err(self.fail(
                RuntimeErrorKind::Unsupported,
                format!("`{what}`: {reason}"),
                pc,
            )),
            None => Ok(()),
        }
    }

    fn security_input_timeframe(&self, input: u32) -> Option<&str> {
        let decl = self.program.inputs.get(input as usize)?;
        if decl.kind != InputKind::String {
            return None;
        }
        let selected = match self.options.inputs.get(&input) {
            Some(InputValue::String(value)) => value.as_str(),
            _ => match &decl.defval {
                InputValue::String(value) => value.as_str(),
                _ => return None,
            },
        };
        Some(if selected.is_empty() {
            self.data.timeframe.as_str()
        } else {
            selected
        })
    }

    /// Read the `request.security` value for the current bar (`self.bar`).
    ///
    /// The chart feed is resampled to `htf_seconds` and the requested builtin
    /// `source` is evaluated over each HTF bar, then mapped back per chart bar
    /// honoring `gaps_on`/`lookahead_on` (see
    /// [`resample::Resampled::map_to_chart_bars`]). The whole back-mapped
    /// series is memoized per `(source, htf_seconds, gaps_on, lookahead_on)`
    /// site on first use, so the resample runs once per run, not once per bar.
    /// A gap / not-yet-available bucket, or a non-finite aggregate, reads `na`.
    // The arguments intentionally mirror the validated ReadSecurity opcode.
    #[allow(clippy::too_many_arguments)]
    fn read_security(
        &mut self,
        selector: Option<&str>,
        source: BuiltinSeries,
        htf_seconds: i64,
        calendar: resample::CalendarAlignment,
        gaps_on: bool,
        lookahead_on: bool,
        pc: usize,
        budget: &mut u64,
    ) -> Result<Value, RuntimeError> {
        if let Some(selector) = selector {
            return Ok(self.read_request(selector));
        }
        let key = (source, htf_seconds, calendar, gaps_on, lookahead_on);
        if !self.security_memo.contains_key(&key) {
            self.refuse_unsettled_buckets("request.security", htf_seconds, calendar, true, pc)?;
            self.charge_extra_work(budget, self.bars as u64, pc)?;
            self.charge_extra_work(
                budget,
                (self.data.request_time_close.len() as u64).saturating_mul(32),
                pc,
            )?;
            self.reserve_resample_cache(
                RESAMPLE_CACHE_RESERVATION_BYTES_PER_BAR,
                RESAMPLE_CACHE_ENTRY_RESERVATION_BYTES,
                pc,
            )?;
            let series = Self::compute_security_series(
                &self.data,
                source,
                htf_seconds,
                calendar,
                gaps_on,
                lookahead_on,
            );
            self.security_memo.insert(key, series);
        }
        // `self.bar` always indexes a real chart bar; a shorter memo (empty
        // feed) or an na cell reads `na`.
        Ok(self
            .security_memo
            .get(&key)
            .and_then(|s| s.get(self.bar).copied().flatten())
            .map_or(Value::Na, Value::from_f64))
    }

    /// Read the current bar's value for a COMPOUND `request.security` call.
    /// The chart feed is resampled to `htf_seconds`; the secondary
    /// context evaluates `expr` over the resampled bars WITH ITS OWN `ta.*`
    /// state (independent of `main`), producing one value per HTF bucket, then
    /// maps back per chart bar under `gaps_on`/`lookahead_on`. The whole
    /// back-mapped series is memoized per site per run.
    // The arguments intentionally mirror the validated ReadSecurityExpr opcode.
    #[allow(clippy::too_many_arguments)]
    fn read_security_expr(
        &mut self,
        expr: &HtfExpr,
        htf_seconds: i64,
        calendar: resample::CalendarAlignment,
        gaps_on: bool,
        lookahead_on: bool,
        pc: usize,
        budget: &mut u64,
    ) -> Result<Value, RuntimeError> {
        // The `HtfExpr` lives in the (run-stable) program, so its address is a
        // stable per-site key for the run.
        let key = (
            std::ptr::from_ref::<HtfExpr>(expr) as usize,
            htf_seconds,
            calendar,
            gaps_on,
            lookahead_on,
        );
        if !self.security_expr_memo.contains_key(&key) {
            self.refuse_unsettled_buckets("request.security", htf_seconds, calendar, true, pc)?;
            self.charge_extra_work(
                budget,
                (self.data.request_time_close.len() as u64).saturating_mul(32),
                pc,
            )?;
            self.charge_extra_work(budget, Self::htf_input_binding_work(expr), pc)?;
            let resolved = self.resolve_htf_input_expr(expr, pc)?;
            let expr_work = Self::htf_expr_work(&resolved).saturating_add(2);
            self.charge_extra_work(budget, (self.bars as u64).saturating_mul(expr_work), pc)?;
            let history_lanes = (expr.ta_slot_count() as usize)
                .checked_add(expr.history_slot_count() as usize)
                .ok_or_else(|| {
                    self.fail(
                        RuntimeErrorKind::VmMemoryLimitExceeded,
                        "compound security history-lane count overflowed".into(),
                        pc,
                    )
                })?;
            let history_bytes_per_bar = history_lanes
                .checked_mul(std::mem::size_of::<Option<f64>>())
                .ok_or_else(|| {
                    self.fail(
                        RuntimeErrorKind::VmMemoryLimitExceeded,
                        "compound security cache size overflowed".into(),
                        pc,
                    )
                })?;
            let per_bar_bytes = RESAMPLE_CACHE_RESERVATION_BYTES_PER_BAR
                .checked_add(history_bytes_per_bar)
                .ok_or_else(|| {
                    self.fail(
                        RuntimeErrorKind::VmMemoryLimitExceeded,
                        "compound security cache size overflowed".into(),
                        pc,
                    )
                })?;
            let history_headers = history_lanes
                .checked_mul(std::mem::size_of::<Vec<Option<f64>>>())
                .and_then(|bytes| bytes.checked_add(RESAMPLE_CACHE_ENTRY_RESERVATION_BYTES))
                .ok_or_else(|| {
                    self.fail(
                        RuntimeErrorKind::VmMemoryLimitExceeded,
                        "compound security cache header size overflowed".into(),
                        pc,
                    )
                })?;
            self.reserve_resample_cache(per_bar_bytes, history_headers, pc)?;
            let series = Self::compute_security_expr_series(
                &self.data,
                &resolved,
                htf_seconds,
                calendar,
                gaps_on,
                lookahead_on,
                self.realtime_active.then_some(self.bar),
            );
            self.security_expr_memo.insert(key, series);
        }
        Ok(self
            .security_expr_memo
            .get(&key)
            .and_then(|s| s.get(self.bar).copied().flatten())
            .map_or(Value::Na, |value| {
                if expr.result_is_bool() {
                    Value::Bool(value != 0.0)
                } else {
                    Value::from_f64(value)
                }
            }))
    }

    /// Bind run-constant source/length inputs before metering or evaluating a
    /// secondary context. The memo belongs to this run, so its site key stays
    /// stable while different runs can select different inputs.
    fn resolve_htf_input_expr(&self, expr: &HtfExpr, pc: usize) -> Result<HtfExpr, RuntimeError> {
        match expr {
            HtfExpr::BoolResult(inner) => Ok(HtfExpr::BoolResult(Box::new(
                self.resolve_htf_input_expr(inner, pc)?,
            ))),
            HtfExpr::InputSource { input } => {
                let decl = self.program.inputs.get(*input as usize).ok_or_else(|| {
                    self.fail(
                        RuntimeErrorKind::InvalidProgram,
                        "HTF source input is missing".into(),
                        pc,
                    )
                })?;
                if decl.kind != InputKind::Source {
                    return Err(self.fail(
                        RuntimeErrorKind::InvalidProgram,
                        "HTF source input has the wrong type".into(),
                        pc,
                    ));
                }
                let source = match self.options.inputs.get(input) {
                    Some(InputValue::Source(source)) if source.is_input_source() => *source,
                    Some(InputValue::ExternalSource(_)) => {
                        return Err(self.fail(
                            RuntimeErrorKind::Unsupported,
                            "external input.source cannot be resampled in request.security".into(),
                            pc,
                        ));
                    }
                    _ => match decl.defval {
                        InputValue::Source(source) if source.is_input_source() => source,
                        _ => {
                            return Err(self.fail(
                                RuntimeErrorKind::InvalidProgram,
                                "HTF source input default is invalid".into(),
                                pc,
                            ))
                        }
                    },
                };
                Ok(HtfExpr::Source(source))
            }
            HtfExpr::EnumSelect {
                input,
                index,
                when_equal,
                otherwise,
            } => {
                let decl = self.program.inputs.get(*input as usize).ok_or_else(|| {
                    self.fail(
                        RuntimeErrorKind::InvalidProgram,
                        "HTF enum input is missing".into(),
                        pc,
                    )
                })?;
                let InputValue::Enum { index: default, .. } = decl.defval else {
                    return Err(self.fail(
                        RuntimeErrorKind::InvalidProgram,
                        "HTF enum input default is invalid".into(),
                        pc,
                    ));
                };
                let selected = match self.options.inputs.get(input) {
                    Some(value @ InputValue::Enum { index, .. })
                        if input_value_matches_decl(value, decl) =>
                    {
                        *index
                    }
                    _ => default,
                };
                let branch = if selected == *index {
                    Some(when_equal.as_ref())
                } else {
                    otherwise.as_deref()
                };
                match branch {
                    Some(branch) => self.resolve_htf_input_expr(branch, pc),
                    None => Ok(HtfExpr::Const(f64::NAN)),
                }
            }
            HtfExpr::TaInput {
                func,
                ta_slot,
                input,
                default_length,
                source,
                ..
            } => {
                let decl = self.program.inputs.get(*input as usize).ok_or_else(|| {
                    self.fail(
                        RuntimeErrorKind::InvalidProgram,
                        "HTF length input is missing".into(),
                        pc,
                    )
                })?;
                if decl.kind != InputKind::Int {
                    return Err(self.fail(
                        RuntimeErrorKind::InvalidProgram,
                        "HTF length input has the wrong type".into(),
                        pc,
                    ));
                }
                let selected = match self.options.inputs.get(input) {
                    Some(InputValue::Int(value)) => *value,
                    _ => i64::from(*default_length),
                };
                let length = u32::try_from(selected)
                    .ok()
                    .filter(|value| {
                        (*func != TaFunc::Hma || *value >= 2)
                            && (1..=MAX_WINDOW_LENGTH).contains(value)
                    })
                    .ok_or_else(|| {
                        self.fail(
                            RuntimeErrorKind::InvalidTaLength,
                            format!(
                                "request.security {} length {selected} is out of range",
                                func.name()
                            ),
                            pc,
                        )
                    })?;
                Ok(HtfExpr::Ta {
                    func: *func,
                    ta_slot: *ta_slot,
                    length,
                    source: Box::new(self.resolve_htf_input_expr(source, pc)?),
                })
            }
            HtfExpr::HistoryInput {
                history_slot,
                offset,
                source,
            } => {
                let value = match offset.evaluate(&|input| self.options.inputs.get(&input).cloned())
                {
                    Some(InputValue::Int(x)) => Value::Int(x),
                    Some(InputValue::Float(x)) => Value::Float(x),
                    _ => Value::Na,
                };
                let Some(selected) = Self::runtime_history_offset(value) else {
                    return Ok(HtfExpr::Const(f64::NAN));
                };
                let offset = self.check_dynamic_history_offset(selected, pc)?;
                Ok(HtfExpr::History {
                    history_slot: *history_slot,
                    offset,
                    source: Box::new(self.resolve_htf_input_expr(source, pc)?),
                })
            }
            HtfExpr::History {
                history_slot,
                offset,
                source,
            } => Ok(HtfExpr::History {
                history_slot: *history_slot,
                offset: *offset,
                source: Box::new(self.resolve_htf_input_expr(source, pc)?),
            }),
            HtfExpr::HistoryRealtime {
                history_slot,
                history_offset,
                realtime_offset,
                source,
            } => Ok(HtfExpr::HistoryRealtime {
                history_slot: *history_slot,
                history_offset: *history_offset,
                realtime_offset: *realtime_offset,
                source: Box::new(self.resolve_htf_input_expr(source, pc)?),
            }),
            HtfExpr::Neg(inner) => Ok(HtfExpr::Neg(Box::new(
                self.resolve_htf_input_expr(inner, pc)?,
            ))),
            HtfExpr::Binary {
                op,
                int_result,
                lhs,
                rhs,
            } => Ok(HtfExpr::Binary {
                op: *op,
                int_result: *int_result,
                lhs: Box::new(self.resolve_htf_input_expr(lhs, pc)?),
                rhs: Box::new(self.resolve_htf_input_expr(rhs, pc)?),
            }),
            HtfExpr::Ta {
                func,
                ta_slot,
                length,
                source,
            } => Ok(HtfExpr::Ta {
                func: *func,
                ta_slot: *ta_slot,
                length: *length,
                source: Box::new(self.resolve_htf_input_expr(source, pc)?),
            }),
            HtfExpr::Const(_)
            | HtfExpr::Source(_)
            | HtfExpr::BarstateIslast
            | HtfExpr::BarstateIsRealtime => Ok(expr.clone()),
        }
    }

    /// Binding is performed once per request site/run. Meter the offset
    /// expression too, before evaluating a serialized arithmetic tree.
    fn htf_input_binding_work(expr: &HtfExpr) -> u64 {
        match expr {
            HtfExpr::BoolResult(inner) => 1u64.saturating_add(Self::htf_input_binding_work(inner)),
            HtfExpr::EnumSelect {
                when_equal,
                otherwise,
                ..
            } => 1u64
                .saturating_add(Self::htf_input_binding_work(when_equal))
                .saturating_add(
                    otherwise
                        .as_ref()
                        .map_or(0, |expr| Self::htf_input_binding_work(expr)),
                ),
            HtfExpr::HistoryInput { offset, source, .. } => {
                let nodes = offset
                    .allocated_bytes()
                    .map_or(u64::MAX, |bytes| {
                        (bytes / std::mem::size_of::<pinewasm_hir::HtfHistoryOffset>()) as u64
                    })
                    .saturating_add(1);
                nodes
                    .saturating_add(1)
                    .saturating_add(Self::htf_input_binding_work(source))
            }
            HtfExpr::HistoryRealtime { source, .. } => {
                1u64.saturating_add(Self::htf_input_binding_work(source))
            }
            HtfExpr::History { source, .. }
            | HtfExpr::Ta { source, .. }
            | HtfExpr::TaInput { source, .. }
            | HtfExpr::Neg(source) => 1u64.saturating_add(Self::htf_input_binding_work(source)),
            HtfExpr::Binary { lhs, rhs, .. } => 1u64
                .saturating_add(Self::htf_input_binding_work(lhs))
                .saturating_add(Self::htf_input_binding_work(rhs)),
            _ => 1,
        }
    }

    /// Conservative work per HTF bucket for a compound security expression.
    /// Windowed TA nodes include their maximum scan length so one CSIL dispatch
    /// cannot hide quadratic work from either instruction ceiling.
    fn htf_expr_work(expr: &HtfExpr) -> u64 {
        match expr {
            HtfExpr::Const(_)
            | HtfExpr::Source(_)
            | HtfExpr::InputSource { .. }
            | HtfExpr::BarstateIslast
            | HtfExpr::BarstateIsRealtime => 1,
            HtfExpr::BoolResult(inner) => 1u64.saturating_add(Self::htf_expr_work(inner)),
            HtfExpr::EnumSelect {
                when_equal,
                otherwise,
                ..
            } => 1u64
                .saturating_add(Self::htf_expr_work(when_equal))
                .saturating_add(
                    otherwise
                        .as_ref()
                        .map_or(0, |expr| Self::htf_expr_work(expr)),
                ),
            HtfExpr::History { source, .. }
            | HtfExpr::HistoryInput { source, .. }
            | HtfExpr::HistoryRealtime { source, .. }
            | HtfExpr::Neg(source) => 1u64.saturating_add(Self::htf_expr_work(source)),
            HtfExpr::Binary { lhs, rhs, .. } => 1u64
                .saturating_add(Self::htf_expr_work(lhs))
                .saturating_add(Self::htf_expr_work(rhs)),
            HtfExpr::Ta { length, source, .. } => 1u64
                .saturating_add(u64::from(*length))
                .saturating_add(Self::htf_expr_work(source)),
            HtfExpr::TaInput {
                default_length,
                source,
                ..
            } => 1u64
                .saturating_add(u64::from(*default_length))
                .saturating_add(Self::htf_expr_work(source)),
        }
    }

    /// Plan the windowed TA sites in a secondary context. Other sites retain
    /// their existing evaluator and occupy no engine state.
    fn collect_htf_window_decls(expr: &HtfExpr, decls: &mut [pinewasm_ta::Decl]) {
        match expr {
            HtfExpr::BoolResult(inner) => Self::collect_htf_window_decls(inner, decls),
            HtfExpr::Ta {
                func,
                ta_slot,
                length,
                source,
            } => {
                if matches!(
                    func,
                    TaFunc::Atr | TaFunc::Hma | TaFunc::Vwma | TaFunc::Rsi | TaFunc::Wma
                ) {
                    if let Some(decl) = decls.get_mut(*ta_slot as usize) {
                        *decl = pinewasm_ta::Decl::new(*func, &[i64::from(*length)])
                            .unwrap_or_default();
                    }
                }
                Self::collect_htf_window_decls(source, decls);
            }
            HtfExpr::History { source, .. }
            | HtfExpr::HistoryInput { source, .. }
            | HtfExpr::HistoryRealtime { source, .. }
            | HtfExpr::Neg(source)
            | HtfExpr::TaInput { source, .. } => Self::collect_htf_window_decls(source, decls),
            HtfExpr::Binary { lhs, rhs, .. } => {
                Self::collect_htf_window_decls(lhs, decls);
                Self::collect_htf_window_decls(rhs, decls);
            }
            HtfExpr::Const(_)
            | HtfExpr::Source(_)
            | HtfExpr::InputSource { .. }
            | HtfExpr::BarstateIslast
            | HtfExpr::BarstateIsRealtime
            | HtfExpr::EnumSelect { .. } => {}
        }
    }

    /// Run the secondary context `expr` over the resampled bars and back-map it
    /// per chart bar. Pure over the feed (deterministic, cacheable): the
    /// per-bucket values are computed once — carrying the secondary context's
    /// own `ta.*` state across HTF bars — then indexed back through
    /// [`resample::Resampled::map_to_chart_bars`].
    fn compute_security_expr_series(
        data: &MarketData,
        expr: &HtfExpr,
        htf_seconds: i64,
        calendar: resample::CalendarAlignment,
        gaps_on: bool,
        lookahead_on: bool,
        realtime_chart_bar: Option<usize>,
    ) -> Vec<Option<f64>> {
        let r = resample::resample_aligned_security(data, htf_seconds, calendar);
        let realtime_bucket = realtime_chart_bar
            .and_then(|chart_bar| r.bucket_of.get(chart_bar).copied())
            .and_then(|bucket| u32::try_from(bucket).ok());
        // One value per HTF bucket, computed in bar order so the secondary
        // context's `ta.*` state (per-slot source history) advances correctly.
        let slot_count = expr.ta_slot_count() as usize;
        let mut ta_histories: Vec<Vec<Option<f64>>> = vec![Vec::new(); slot_count];
        let mut ema_states = vec![pinewasm_ta::Smoother::default(); slot_count];
        let mut decls = vec![pinewasm_ta::Decl::default(); slot_count];
        Self::collect_htf_window_decls(expr, &mut decls);
        let layout = pinewasm_ta::Layout::plan(&decls);
        let mut states = vec![pinewasm_ta::State::Invalid; slot_count];
        let mut cells = vec![f64::NAN; layout.cells as usize];
        let mut scratch = vec![f64::NAN; layout.scratch as usize];
        let mut engine = pinewasm_ta::Engine::init(&decls, &mut states, &mut cells, &mut scratch);
        let lanes = HtfLanes(&r.bars);
        let history_slot_count = expr.history_slot_count() as usize;
        let mut value_histories: Vec<Vec<Option<f64>>> = vec![Vec::new(); history_slot_count];
        let mut per_bucket: Vec<Option<f64>> = Vec::with_capacity(r.bars.len());
        for b in 0..r.bars.len() {
            let v = Self::eval_htf(
                expr,
                &mut ta_histories,
                &mut ema_states,
                &mut value_histories,
                &mut engine,
                &lanes,
                b as u32,
                realtime_bucket,
            );
            per_bucket.push(v);
        }
        let gaps = if gaps_on {
            resample::Gaps::On
        } else {
            resample::Gaps::Off
        };
        let lookahead = if lookahead_on {
            resample::Lookahead::On
        } else {
            resample::Lookahead::Off
        };
        r.map_to_chart_bars(gaps, lookahead, |b| per_bucket[b])
    }

    /// Evaluate a compound [`HtfExpr`] over ONE HTF bar. `ta.*` nodes read (and
    /// extend) their per-slot source history in `histories` — this is the
    /// secondary context's own state, distinct from `main`'s. Called once per
    /// HTF bar in ascending order, so each ta slot's history is exactly the
    /// source values of the buckets seen so far. `None` models an HTF-side
    /// `na` and propagates through arithmetic/comparison.
    // These are the independent state stores needed to evaluate one nested
    // request expression without conflating it with the chart context.
    #[allow(clippy::too_many_arguments)]
    fn eval_htf(
        expr: &HtfExpr,
        ta_histories: &mut [Vec<Option<f64>>],
        ema_states: &mut [pinewasm_ta::Smoother],
        value_histories: &mut [Vec<Option<f64>>],
        engine: &mut pinewasm_ta::Engine<'_>,
        lanes: &HtfLanes<'_>,
        bucket: u32,
        realtime_bucket: Option<u32>,
    ) -> Option<f64> {
        let bar = lanes.0.get(bucket as usize)?;
        match expr {
            HtfExpr::BoolResult(inner) => Self::eval_htf(
                inner,
                ta_histories,
                ema_states,
                value_histories,
                engine,
                lanes,
                bucket,
                realtime_bucket,
            ),
            HtfExpr::Const(v) => Some(*v),
            HtfExpr::BarstateIslast => Some(if bucket as usize + 1 == lanes.0.len() {
                1.0
            } else {
                0.0
            }),
            HtfExpr::BarstateIsRealtime => Some(if realtime_bucket == Some(bucket) {
                1.0
            } else {
                0.0
            }),
            HtfExpr::Source(source) => Self::htf_source_value(bar, *source),
            HtfExpr::InputSource { .. }
            | HtfExpr::TaInput { .. }
            | HtfExpr::HistoryInput { .. }
            | HtfExpr::EnumSelect { .. } => None,
            HtfExpr::History {
                history_slot,
                offset,
                source,
            } => {
                let value = Self::eval_htf(
                    source,
                    ta_histories,
                    ema_states,
                    value_histories,
                    engine,
                    lanes,
                    bucket,
                    realtime_bucket,
                );
                let history = value_histories.get_mut(*history_slot as usize)?;
                history.push(value);
                history
                    .len()
                    .checked_sub(1 + *offset as usize)
                    .and_then(|index| history.get(index).copied().flatten())
            }
            HtfExpr::HistoryRealtime {
                history_slot,
                history_offset,
                realtime_offset,
                source,
            } => {
                let value = Self::eval_htf(
                    source,
                    ta_histories,
                    ema_states,
                    value_histories,
                    engine,
                    lanes,
                    bucket,
                    realtime_bucket,
                );
                let history = value_histories.get_mut(*history_slot as usize)?;
                history.push(value);
                let offset = if realtime_bucket == Some(bucket) {
                    *realtime_offset
                } else {
                    *history_offset
                };
                history
                    .len()
                    .checked_sub(1 + offset as usize)
                    .and_then(|index| history.get(index).copied().flatten())
            }
            HtfExpr::Neg(inner) => Self::eval_htf(
                inner,
                ta_histories,
                ema_states,
                value_histories,
                engine,
                lanes,
                bucket,
                realtime_bucket,
            )
            .map(|v| -v),
            HtfExpr::Binary {
                op,
                int_result,
                lhs,
                rhs,
            } => {
                let l = Self::eval_htf(
                    lhs,
                    ta_histories,
                    ema_states,
                    value_histories,
                    engine,
                    lanes,
                    bucket,
                    realtime_bucket,
                );
                let r = Self::eval_htf(
                    rhs,
                    ta_histories,
                    ema_states,
                    value_histories,
                    engine,
                    lanes,
                    bucket,
                    realtime_bucket,
                );
                Self::htf_binary(*op, *int_result, l, r)
            }
            HtfExpr::Ta {
                func,
                ta_slot,
                length,
                source,
            } => {
                // Evaluate the source in this bucket's context. ATR, HMA,
                // VWMA and RSI use the shared TA engine; EMA keeps its own smoother;
                // SMA and change keep source history. The validator bounds
                // `ta_slot`, and `get_mut` remains defensive.
                let src = Self::eval_htf(
                    source,
                    ta_histories,
                    ema_states,
                    value_histories,
                    engine,
                    lanes,
                    bucket,
                    realtime_bucket,
                );
                if matches!(
                    func,
                    TaFunc::Atr | TaFunc::Hma | TaFunc::Vwma | TaFunc::Rsi | TaFunc::Wma
                ) {
                    let value = engine
                        .advance(
                            *ta_slot,
                            [src.unwrap_or(f64::NAN), f64::NAN, f64::NAN],
                            lanes,
                            bucket,
                        )?
                        .value;
                    return value.is_finite().then_some(value);
                }
                if *func == TaFunc::Ema {
                    let src = src?;
                    let smoother = ema_states.get_mut(*ta_slot as usize)?;
                    let value = smoother.step(src, *length, 2.0 / (f64::from(*length) + 1.0));
                    return value.is_finite().then_some(value);
                }
                let hist = ta_histories.get_mut(*ta_slot as usize)?;
                // SMA counts present source observations in its own
                // requested context. Change retains bar-indexed history.
                if *func != TaFunc::Sma || src.is_some() {
                    hist.push(src);
                }
                let len = *length as usize;
                let n = hist.len();
                match func {
                    // ta.sma: mean of the last `len` present source values;
                    // na until `len` observations exist, hold across na.
                    TaFunc::Sma => {
                        if len == 0 || n < len {
                            return None;
                        }
                        let mut sum = 0.0f64;
                        for cell in &hist[n - len..n] {
                            sum += *cell.as_ref()?;
                        }
                        let mean = sum / len as f64;
                        if mean.is_finite() {
                            Some(mean)
                        } else {
                            None
                        }
                    }
                    // ta.change: source now minus source `len` buckets ago; na
                    // if fewer than `len + 1` buckets or either endpoint is na.
                    TaFunc::Change => {
                        if n <= len {
                            return None;
                        }
                        match (hist[n - 1], hist[n - 1 - len]) {
                            (Some(now), Some(past)) => {
                                let d = now - past;
                                if d.is_finite() {
                                    Some(d)
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        }
                    }
                    // Only SMA and Change reach this history-based path.
                    _ => None,
                }
            }
        }
    }

    /// Apply an [`HtfExpr`] binary operator to two optional operands, with
    /// `na`-propagation. Comparisons use script float tolerance and yield
    /// `1.0`/`0.0` (the secondary context is float-typed); this does not add
    /// exact integer representation to that context. Division/remainder by
    /// zero yields a non-finite value, normalized to `na`. The serialized
    /// `int_result` flag is retained for
    /// compatibility; the float-only secondary context keeps the fractional
    /// quotient even when the expression is statically int.
    fn htf_binary(
        op: pinewasm_hir::BinOp,
        _int_result: bool,
        l: Option<f64>,
        r: Option<f64>,
    ) -> Option<f64> {
        use pinewasm_hir::BinOp;
        let (l, r) = (l?, r?);
        let v = match op {
            BinOp::Add => l + r,
            BinOp::Sub => l - r,
            BinOp::Mul => l * r,
            BinOp::Div => l / r,
            BinOp::Rem => l % r,
            BinOp::Lt => {
                return Some(if pinewasm_num::comparison::lt(l, r) {
                    1.0
                } else {
                    0.0
                })
            }
            BinOp::Le => {
                return Some(if pinewasm_num::comparison::le(l, r) {
                    1.0
                } else {
                    0.0
                })
            }
            BinOp::Gt => {
                return Some(if pinewasm_num::comparison::gt(l, r) {
                    1.0
                } else {
                    0.0
                })
            }
            BinOp::Ge => {
                return Some(if pinewasm_num::comparison::ge(l, r) {
                    1.0
                } else {
                    0.0
                })
            }
            BinOp::Eq => {
                return Some(if pinewasm_num::comparison::eq(l, r) {
                    1.0
                } else {
                    0.0
                })
            }
            BinOp::Ne => {
                return Some(if pinewasm_num::comparison::eq(l, r) {
                    0.0
                } else {
                    1.0
                })
            }
            // Boolean operators are not produced for the float-typed secondary
            // context (the analyzer's arithmetic/comparison subset excludes
            // them); defense in depth.
            BinOp::And | BinOp::Or => return None,
        };
        if v.is_finite() {
            Some(v)
        } else {
            None
        }
    }

    /// Read the current bar's value for a scalar `request.*` interface.
    /// Looks `selector` up in the optional host table
    /// `MarketData::request_data`: a hit yields `Value::from_f64` of the
    /// per-bar sample (non-finite → `na`), a miss (or a sample past the end of
    /// a short host series) yields `na`. No second data feed is involved — the
    /// interface makes the `request.*` language surface complete and testable
    /// while data sourcing stays a host concern.
    fn request_input_symbol_value(
        &self,
        expression: &InputActiveExpr,
        depth: usize,
    ) -> Option<InputValue> {
        if depth > 64 {
            return None;
        }
        match expression {
            InputActiveExpr::Value { value } => Some(value.clone()),
            InputActiveExpr::Na => None,
            InputActiveExpr::Input { input } => {
                let decl = self.program.inputs.get(*input as usize)?;
                let selected = self.options.inputs.get(input).unwrap_or(&decl.defval);
                matches!(
                    (decl.kind, selected),
                    (InputKind::String, InputValue::String(_))
                        | (InputKind::Bool, InputValue::Bool(_))
                        | (InputKind::Int, InputValue::Int(_))
                )
                .then(|| selected.clone())
            }
            InputActiveExpr::Unary {
                op: InputActiveUnaryOp::Neg,
                operand,
            } => match self.request_input_symbol_value(operand, depth + 1)? {
                InputValue::Int(value) => value.checked_neg().map(InputValue::Int),
                _ => None,
            },
            InputActiveExpr::Binary { op, lhs, rhs }
                if matches!(
                    op,
                    InputActiveBinaryOp::Eq
                        | InputActiveBinaryOp::Ne
                        | InputActiveBinaryOp::Add
                        | InputActiveBinaryOp::Sub
                        | InputActiveBinaryOp::Mul
                        | InputActiveBinaryOp::Rem
                ) =>
            {
                let lhs = self.request_input_symbol_value(lhs, depth + 1)?;
                let rhs = self.request_input_symbol_value(rhs, depth + 1)?;
                match (op, lhs, rhs) {
                    (InputActiveBinaryOp::Eq, lhs, rhs) => Some(InputValue::Bool(lhs == rhs)),
                    (InputActiveBinaryOp::Ne, lhs, rhs) => Some(InputValue::Bool(lhs != rhs)),
                    (
                        InputActiveBinaryOp::Add,
                        InputValue::String(lhs),
                        InputValue::String(rhs),
                    ) => {
                        let len = lhs.len().checked_add(rhs.len())?;
                        (len <= MAX_INPUT_ACTIVE_STRING_BYTES)
                            .then(|| InputValue::String(lhs + &rhs))
                    }
                    (InputActiveBinaryOp::Add, InputValue::Int(lhs), InputValue::Int(rhs)) => {
                        lhs.checked_add(rhs).map(InputValue::Int)
                    }
                    (InputActiveBinaryOp::Sub, InputValue::Int(lhs), InputValue::Int(rhs)) => {
                        lhs.checked_sub(rhs).map(InputValue::Int)
                    }
                    (InputActiveBinaryOp::Mul, InputValue::Int(lhs), InputValue::Int(rhs)) => {
                        lhs.checked_mul(rhs).map(InputValue::Int)
                    }
                    (InputActiveBinaryOp::Rem, InputValue::Int(lhs), InputValue::Int(rhs)) => {
                        lhs.checked_rem(rhs).map(InputValue::Int)
                    }
                    _ => None,
                }
            }
            InputActiveExpr::Ternary {
                condition,
                when_true,
                when_false,
            } => {
                let choice = self.request_input_symbol_value(condition, depth + 1);
                let branch = if matches!(choice, Some(InputValue::Bool(true))) {
                    when_true
                } else {
                    when_false
                };
                self.request_input_symbol_value(branch, depth + 1)
            }
            InputActiveExpr::Call { function, args }
                if function == "str.tostring" && args.len() == 1 =>
            {
                match self.request_input_symbol_value(&args[0], depth + 1)? {
                    InputValue::Int(value) => Some(InputValue::String(value.to_string())),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn read_request(&self, selector: &str) -> Value {
        let chart_selector = selector
            .split_once('/')
            .and_then(|(head, rest)| {
                let (member, symbol) = if let Some(member) = head.strip_suffix("_chart_tickerid") {
                    let info = &self.data.syminfo;
                    let symbol = if !info.tickerid.is_empty() {
                        info.tickerid.clone()
                    } else if !info.main_tickerid.is_empty() {
                        info.main_tickerid.clone()
                    } else if !info.prefix.is_empty() && !info.ticker.is_empty() {
                        format!("{}:{}", info.prefix, info.ticker)
                    } else {
                        info.ticker.clone()
                    };
                    (member, symbol)
                } else {
                    let member = head.strip_suffix("_chart_ticker")?;
                    (member, self.data.syminfo.ticker.clone())
                };
                Some(format!("{member}/{symbol}/{rest}"))
            })
            .or_else(|| {
                selector
                    .strip_prefix("security_chart/")
                    .and_then(|rest| {
                        let (symbol, source) = rest.rsplit_once('/')?;
                        Some(format!(
                            "security/{symbol}/{}/{}",
                            self.data.timeframe, source
                        ))
                    })
                    .or_else(|| {
                        selector
                            .strip_prefix("security_expression_chart/")
                            .and_then(|rest| {
                                // The encoded expression contains no slashes. Split
                                // from the right so host symbol IDs may contain them.
                                let mut parts = rest.rsplitn(4, '/');
                                let expression = parts.next()?;
                                let lookahead = parts.next()?;
                                let gaps = parts.next()?;
                                let symbol = parts.next()?;
                                Some(format!(
                            "security_expression/{symbol}/{}/{gaps}/{lookahead}/{expression}",
                            self.data.timeframe,
                        ))
                            })
                    })
                    .or_else(|| {
                        selector.strip_prefix("security_input/").and_then(|rest| {
                            let (input, rest) = rest.split_once('/')?;
                            let input = input.parse::<u32>().ok()?;
                            let (symbol, source) = rest.rsplit_once('/')?;
                            let tf = self.security_input_timeframe(input)?;
                            Some(format!("security/{symbol}/{tf}/{source}"))
                        })
                    })
                    .or_else(|| {
                        selector
                            .strip_prefix("security_expression_input/")
                            .and_then(|rest| {
                                let (input, rest) = rest.split_once('/')?;
                                let input = input.parse::<u32>().ok()?;
                                let mut parts = rest.rsplitn(4, '/');
                                let expression = parts.next()?;
                                let lookahead = parts.next()?;
                                let gaps = parts.next()?;
                                let symbol = parts.next()?;
                                let tf = self.security_input_timeframe(input)?;
                                Some(format!(
                            "security_expression/{symbol}/{tf}/{gaps}/{lookahead}/{expression}"
                        ))
                            })
                    })
            });
        let selector = chart_selector.as_deref().unwrap_or(selector);
        let basecurrency_selector =
            selector
                .split_once("/@basecurrency+")
                .and_then(|(prefix, tail)| {
                    let (encoded, rest) = tail.split_once('/')?;
                    if encoded.len() % 2 != 0 {
                        return None;
                    }
                    let suffix = encoded
                        .as_bytes()
                        .chunks_exact(2)
                        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
                        .collect::<Option<Vec<_>>>()?;
                    let suffix = String::from_utf8(suffix).ok()?;
                    Some(format!(
                        "{prefix}/{}{suffix}/{rest}",
                        self.data.syminfo.basecurrency
                    ))
                });
        let selector = basecurrency_selector.as_deref().unwrap_or(selector);
        let mut resolved = selector.to_owned();
        // Symbol and timeframe may both be input-stable expressions. Resolve
        // their two independent markers before asking the host for a series.
        for _ in 0..2 {
            let Some((prefix, tail)) = resolved.split_once("/@input_expr+") else {
                break;
            };
            let Some((encoded, rest)) = tail.split_once('/') else {
                return Value::Na;
            };
            if encoded.len() > 128 * 1024 || encoded.len() % 2 != 0 {
                return Value::Na;
            }
            let Some(bytes) = encoded
                .as_bytes()
                .chunks_exact(2)
                .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
                .collect::<Option<Vec<_>>>()
            else {
                return Value::Na;
            };
            let Ok(expression) = serde_json::from_slice::<InputActiveExpr>(&bytes) else {
                return Value::Na;
            };
            let Some(InputValue::String(selected)) =
                self.request_input_symbol_value(&expression, 0)
            else {
                return Value::Na;
            };
            let selected = if selected.is_empty()
                && (prefix.starts_with("security/") || prefix.starts_with("security_expression/"))
            {
                self.data.timeframe.as_str()
            } else {
                selected.as_str()
            };
            resolved = format!("{prefix}/{selected}/{rest}");
        }
        self.data
            .request_data
            .get(&resolved)
            .and_then(|series| series.get(self.bar).copied())
            .map_or(Value::Na, Value::from_f64)
    }

    /// Build the current bar's `array<float>` of intrabar values
    /// (`request.security_lower_tf`). Reads the optional host table
    /// `MarketData::security_lower_tf_data[selector][bar]`: a present entry fills
    /// the array with those intrabar values (non-finite → `na`); a miss (the
    /// default) or an uncovered bar yields an EMPTY array. This VM only
    /// aggregates UP, so lower-timeframe intrabar decomposition is a host
    /// concern — no real intrabar feed is involved. Allocating the array can hit
    /// the heap limit, which surfaces as a structured error (never a panic).
    fn build_security_lower_tf(
        &mut self,
        selector: &str,
        pc: usize,
    ) -> Result<Value, RuntimeError> {
        let items: Vec<Value> = self
            .data
            .security_lower_tf_data
            .get(selector)
            .and_then(|series| series.get(self.bar))
            .map(|values| values.iter().map(|v| Value::from_f64(*v)).collect())
            .unwrap_or_default();
        let h = self
            .heap
            .alloc(HeapObj::Array {
                elem_ty: ElemType::Float,
                items,
            })
            .map_err(|e| self.heap_limit_err(e, pc))?;
        Ok(Value::Ref(h))
    }

    /// Build the current bar's `array<float>` of pivot levels for `system` from
    /// the PRIOR completed `anchor_seconds`-period High/Low/Close
    /// (`ta.pivot_point_levels`).
    ///
    /// The chart feed is resampled to the anchor timeframe (the same machinery
    /// as `request.security`), and the most recently COMPLETED anchor bucket at
    /// this chart bar is read under `lookahead_off` semantics — never the
    /// forming bucket, so no future information leaks. Its H/L/C feed the
    /// system's [`PivotSystem::levels`] formula, producing the 11-slot array
    /// `[P, R1, S1, R2, S2, R3, S3, R4, S4, R5, S5]` (a level a system omits is
    /// `na`). Before any anchor bucket has completed (early bars), the array is
    /// EMPTY. The per-anchor prior-bucket HLC map is memoized per run, so the
    /// resample happens once. Allocating the array can hit the heap limit,
    /// which surfaces as a structured error (never a panic).
    fn build_pivot_levels(
        &mut self,
        system: PivotSystem,
        anchor_seconds: i64,
        pc: usize,
        budget: &mut u64,
    ) -> Result<Value, RuntimeError> {
        if !self.pivot_hlc_memo.contains_key(&anchor_seconds) {
            // Pivots group the chart's own bars; they never read a
            // host-supplied requested feed.
            self.refuse_unsettled_buckets(
                "ta.pivot_point_levels",
                anchor_seconds,
                resample::CalendarAlignment::Fixed,
                false,
                pc,
            )?;
            self.charge_extra_work(budget, self.bars as u64, pc)?;
            self.reserve_resample_cache(
                RESAMPLE_CACHE_RESERVATION_BYTES_PER_BAR,
                RESAMPLE_CACHE_ENTRY_RESERVATION_BYTES,
                pc,
            )?;
            let map = Self::compute_pivot_hlc(&self.data, anchor_seconds);
            self.pivot_hlc_memo.insert(anchor_seconds, map);
        }
        // The prior-completed anchor bucket's HLC at this chart bar, if any.
        let hlc = self
            .pivot_hlc_memo
            .get(&anchor_seconds)
            .and_then(|m| m.get(self.bar).copied().flatten());
        // Before any anchor bucket has completed, this timeframe-based shim
        // returns an empty array. Every other bar returns the fixed
        // 11-slot array.
        let items: Vec<Value> = match hlc {
            Some((open, high, low, close, current_open)) => system
                .levels(open, high, low, close, current_open)
                .iter()
                .map(|v| Value::from_f64(*v))
                .collect(),
            None => Vec::new(),
        };
        let h = self
            .heap
            .alloc(HeapObj::Array {
                elem_ty: ElemType::Float,
                items,
            })
            .map_err(|e| self.heap_limit_err(e, pc))?;
        Ok(Value::Ref(h))
    }

    /// Resample the chart feed to `anchor_seconds` and, for each chart bar,
    /// resolve the PRIOR completed anchor period's `(high, low, close)`.
    ///
    /// Pivot levels for the currently-forming anchor period are computed from
    /// the period STRICTLY BEFORE it: at a chart bar whose own anchor bucket is
    /// `own`, the prior period is bucket `own - 1` (the levels are then constant
    /// across the whole current period). On the first anchor period (`own == 0`)
    /// there is no prior period, so the result is `None` (the VM returns an
    /// empty array). This never reads the forming bucket, so no future
    /// information leaks (`lookahead_off`). Pure over the feed (deterministic,
    /// cacheable).
    fn compute_pivot_hlc(data: &MarketData, anchor_seconds: i64) -> PivotHlcSeries {
        let r = resample::resample_days_or_fixed(data, anchor_seconds.max(1));
        (0..data.bars())
            .map(|bar| {
                let own = *r.bucket_of.get(bar)?;
                let prior = own.checked_sub(1)?;
                let b = &r.bars[prior];
                let current = &r.bars[own];
                Some((b.open, b.high, b.low, b.close, current.open))
            })
            .collect()
    }

    // Implements the time/time_close overload with metering context.
    #[allow(clippy::too_many_arguments)]
    fn time_function_value(
        &mut self,
        requested_tf: &str,
        session: &str,
        timezone: &str,
        close_time: bool,
        bars_back: i64,
        timeframe_bars_back: i64,
        pc: usize,
        budget: &mut u64,
    ) -> Result<Value, RuntimeError> {
        if !Self::valid_time_offset(bars_back) || !Self::valid_time_offset(timeframe_bars_back) {
            return Ok(Value::Na);
        }
        let Some((open, nominal_close)) = self.time_function_open_close(
            requested_tf,
            bars_back,
            timeframe_bars_back,
            pc,
            budget,
        )?
        else {
            return Ok(Value::Na);
        };
        let close = match self.session_close_for_open(open, nominal_close, session, timezone, pc)? {
            Some(close) => close,
            None => return Ok(Value::Na),
        };
        Ok(Value::Int(if close_time { close } else { open }))
    }

    fn time_function_open_close(
        &mut self,
        requested_tf: &str,
        bars_back: i64,
        timeframe_bars_back: i64,
        pc: usize,
        budget: &mut u64,
    ) -> Result<Option<(i64, i64)>, RuntimeError> {
        let requested = if requested_tf.trim().is_empty() {
            self.data.timeframe.clone()
        } else {
            requested_tf.to_string()
        };
        let requested_seconds = timeframe_in_seconds(&requested);
        let chart_seconds = timeframe_in_seconds(&self.data.timeframe);
        if requested_seconds < chart_seconds {
            return Ok(None);
        }
        let requested_step_ms = requested_seconds.saturating_mul(1000).max(1);
        let chart_step_ms = chart_seconds.saturating_mul(1000).max(1);
        if requested_seconds == chart_seconds {
            let Some(open) = self.chart_time_for_offset(
                bars_back.saturating_add(timeframe_bars_back),
                chart_step_ms,
                false,
            ) else {
                return Ok(None);
            };
            return Ok(Some((open, open.saturating_add(chart_step_ms))));
        }

        let Some((target_index, target_time)) =
            self.chart_open_for_offset(bars_back, chart_step_ms)
        else {
            return Ok(None);
        };
        let base_open = match target_index {
            Some(index) => self.time_function_bucket_open(&requested, index, pc, budget)?,
            None => Some(Self::bucket_open_from_anchor(
                self.data.time_at(0),
                target_time,
                requested_step_ms,
            )),
        };
        let Some(base_open) = base_open else {
            return Ok(None);
        };
        let open = Self::time_offset_from_open(base_open, requested_step_ms, timeframe_bars_back);
        Ok(Some((open, open.saturating_add(requested_step_ms))))
    }

    fn session_close_for_open(
        &self,
        open: i64,
        nominal_close: i64,
        session: &str,
        timezone: &str,
        pc: usize,
    ) -> Result<Option<i64>, RuntimeError> {
        let session = session.trim();
        // `session.regular` and `session.extended` name a session type, not
        // hours: both
        // stand for the symbol's own session, like an omitted argument. The
        // constants carry their own names as values; accept the bare tokens too.
        let session_type = session.strip_prefix("session.").unwrap_or(session);
        if session.is_empty()
            || session_type.eq_ignore_ascii_case("regular")
            || session_type.eq_ignore_ascii_case("extended")
        {
            return Ok(Some(nominal_close));
        }
        let Some(parsed) = TimeSession::parse(session) else {
            return Ok(None);
        };
        if parsed.is_full_week() {
            return Ok(Some(nominal_close));
        }
        let timezone = timezone.trim();
        let zone = if timezone.is_empty() {
            self.data.exchange_zone()
        } else {
            self.script_zone(timezone, pc)?
        };
        // The session's hours are read on the clock in force at the open.
        // While that clock still holds at the close, the close is that
        // instant: a wall-clock time that occurs twice is read on the
        // open's occurrence. When the clock changes in between, the close's
        // reading is turned back into an instant by the zone's rules, and
        // never lands before the open.
        let offset = session::offset_at_ms(&zone, open);
        let Some(close) = parsed.close_for_open(open, offset) else {
            return Ok(None);
        };
        let close_offset = session::offset_at_ms(&zone, close);
        let session_close = if close_offset == offset {
            close
        } else {
            let local_close = close.saturating_add(offset.saturating_mul(1000));
            let converted = session::utc_from_local_ms(&zone, local_close);
            if converted > open {
                converted
            } else {
                local_close.saturating_sub(close_offset.saturating_mul(1000))
            }
        };
        Ok(Some(nominal_close.min(session_close)))
    }

    fn valid_time_offset(offset: i64) -> bool {
        (-500..=5000).contains(&offset)
    }

    fn time_offset_from_open(open: i64, step_ms: i64, offset: i64) -> i64 {
        let delta = offset.saturating_abs().saturating_mul(step_ms);
        if offset >= 0 {
            open.saturating_sub(delta)
        } else {
            open.saturating_add(delta)
        }
    }

    fn bucket_open_from_anchor(anchor: i64, target_time: i64, step_ms: i64) -> i64 {
        let bucket = target_time
            .saturating_sub(anchor)
            .div_euclid(step_ms.max(1));
        anchor.saturating_add(bucket.saturating_mul(step_ms))
    }

    fn chart_open_for_offset(&self, bars_back: i64, step_ms: i64) -> Option<(Option<usize>, i64)> {
        let target_index = (self.bar as i64).checked_sub(bars_back)?;
        if target_index < 0 {
            return None;
        }
        if let Ok(index) = usize::try_from(target_index) {
            if index < self.bars {
                return Some((Some(index), self.data.time_at(index)));
            }
        }
        if bars_back < 0 {
            let future = bars_back.saturating_abs().saturating_mul(step_ms);
            return Some((None, self.data.time_at(self.bar).saturating_add(future)));
        }
        None
    }

    fn chart_time_for_offset(&self, bars_back: i64, step_ms: i64, close_time: bool) -> Option<i64> {
        let (target_index, target_open) = self.chart_open_for_offset(bars_back, step_ms)?;
        if let Some(index) = target_index {
            return Some(if close_time {
                self.data.time_close_at(index)
            } else {
                target_open
            });
        }
        Some(if close_time {
            target_open.saturating_add(step_ms)
        } else {
            target_open
        })
    }

    /// The open time of the `requested_timeframe` bucket that chart bar
    /// `chart_bar` belongs to, read from a memo of every chart bar's bucket
    /// open that the timeframe's first use builds over the whole feed.
    ///
    /// A realtime tick that appends a bar grows the feed past that memo, so
    /// the entry is brought up to date before every read. Appending does not
    /// move an earlier bar's bucket open, so only the appended bars are
    /// grouped, charged and reserved ([`resample::BucketOpens`]): a streamed
    /// session costs what one run over its feed costs, instead of a rescan of
    /// the whole feed on every appended bar. The exception is the first tick
    /// that brings a `tradingDay` to a feed without a trading-day lane, which
    /// materializes the lane that calendar buckets read and so regroups them
    /// once.
    fn time_function_bucket_open(
        &mut self,
        requested_timeframe: &str,
        chart_bar: usize,
        pc: usize,
        budget: &mut u64,
    ) -> Result<Option<i64>, RuntimeError> {
        let cached = self
            .time_func_memo
            .get(requested_timeframe)
            .map(|opens| (opens.grouped_bars(), opens.stale_bars(&self.data)));
        match cached {
            None => {
                if let Some(reason) = self.data.timeframe_bucket_refusal(requested_timeframe) {
                    return Err(self.fail(
                        RuntimeErrorKind::Unsupported,
                        format!("`time(\"{requested_timeframe}\")`: {reason}"),
                        pc,
                    ));
                }
                self.charge_extra_work(budget, self.bars as u64, pc)?;
                self.reserve_resample_cache(
                    RESAMPLE_CACHE_RESERVATION_BYTES_PER_BAR,
                    RESAMPLE_CACHE_ENTRY_RESERVATION_BYTES
                        .checked_add(requested_timeframe.len())
                        .ok_or_else(|| {
                            self.fail(
                                RuntimeErrorKind::VmMemoryLimitExceeded,
                                "time-function cache key size overflowed".into(),
                                pc,
                            )
                        })?,
                    pc,
                )?;
                let opens = resample::BucketOpens::new(&self.data, requested_timeframe);
                self.time_func_memo
                    .insert(requested_timeframe.to_string(), opens);
            }
            Some((grouped, stale)) if stale > 0 => {
                // Appended bars may bring a clock change (or a session the
                // grouping no longer settles): check as a fresh run would,
                // scanning only the appended bars for a clock change.
                let refusal = match self.time_func_memo.get_mut(requested_timeframe) {
                    Some(opens) => self.data.timeframe_bucket_refusal_incremental(
                        requested_timeframe,
                        opens.clock_scan_mut(),
                    ),
                    None => self.data.timeframe_bucket_refusal(requested_timeframe),
                };
                if let Some(reason) = refusal {
                    return Err(self.fail(
                        RuntimeErrorKind::Unsupported,
                        format!("`time(\"{requested_timeframe}\")`: {reason}"),
                        pc,
                    ));
                }
                self.charge_extra_work(budget, stale as u64, pc)?;
                // The entry grows in place: reserve only the added bars, so a
                // long session reserves what one run over the same feed does.
                self.reserve_vm_bytes(
                    self.bars
                        .saturating_sub(grouped)
                        .saturating_mul(RESAMPLE_CACHE_RESERVATION_BYTES_PER_BAR),
                    pc,
                )?;
                if let Some(opens) = self.time_func_memo.get_mut(requested_timeframe) {
                    opens.refresh(&self.data);
                }
            }
            Some(_) => {}
        }

        Ok(self
            .time_func_memo
            .get(requested_timeframe)
            .and_then(|opens| opens.get(chart_bar)))
    }

    /// Resolve a registered object type's `type_id` by name, or 0 when the
    /// program declares no such type (an empty/miss registry still yields a
    /// correctly-shaped result).
    fn object_type_id(&self, name: &str) -> u32 {
        self.program
            .object_types
            .iter()
            .position(|t| t.name == name)
            .map_or(0, |i| i as u32)
    }

    /// Build the current bar's order-flow `footprint` object. Reads
    /// the optional host table `MarketData::footprint_data[selector][bar]`: a
    /// present sample fills the object's scalar fields (non-finite → `na`), its
    /// imbalance flags, and a `volume_row` per host row; a miss (the default)
    /// yields an object whose scalars are all `na`, flags `false`, and `rows`
    /// empty. No real order-flow feed is involved — data sourcing is a host
    /// concern. Allocating the rows and the object can hit the heap limit, which
    /// surfaces as a structured error (never a panic).
    fn build_footprint(&mut self, selector: &str, pc: usize) -> Result<Value, RuntimeError> {
        use pinewasm_hir::footprint::{FOOTPRINT_TYPE, VOLUME_ROW_TYPE};

        let sample: Option<HostFootprint> = self
            .data
            .footprint_data
            .get(selector)
            .and_then(|series| series.get(self.bar))
            .and_then(std::clone::Clone::clone);

        let row_type_id = self.object_type_id(VOLUME_ROW_TYPE);
        let fp_type_id = self.object_type_id(FOOTPRINT_TYPE);

        // Build a `volume_row` heap object per host row, in host order.
        let host_rows = sample.as_ref().map_or(&[][..], |s| s.rows.as_slice());
        let mut row_handles: Vec<Value> = Vec::with_capacity(host_rows.len());
        for r in host_rows {
            let fields = vec![
                Value::from_f64(r.buy_volume),
                Value::from_f64(r.sell_volume),
                Value::from_f64(r.total_volume),
                Value::from_f64(r.delta),
                Value::from_f64(r.up_price),
                Value::from_f64(r.down_price),
                Value::Bool(r.has_buy_imbalance),
                Value::Bool(r.has_sell_imbalance),
            ];
            let h = self
                .heap
                .alloc(HeapObj::Object {
                    table_identity: None,
                    type_id: row_type_id,
                    fields,
                })
                .map_err(|e| self.heap_limit_err(e, pc))?;
            row_handles.push(Value::Ref(h));
        }
        let rows_array = self
            .heap
            .alloc(HeapObj::Array {
                elem_ty: ElemType::Object(row_type_id),
                items: row_handles,
            })
            .map_err(|e| self.heap_limit_err(e, pc))?;

        // The footprint object: seven scalar order-flow fields then the rows
        // array (field-slot order mirrors `footprint::FOOTPRINT_FIELDS`).
        let s = sample.as_ref();
        let fields = vec![
            Value::from_f64(s.map_or(f64::NAN, |s| s.buy_volume)),
            Value::from_f64(s.map_or(f64::NAN, |s| s.sell_volume)),
            Value::from_f64(s.map_or(f64::NAN, |s| s.total_volume)),
            Value::from_f64(s.map_or(f64::NAN, |s| s.delta)),
            Value::from_f64(s.map_or(f64::NAN, |s| s.poc)),
            Value::from_f64(s.map_or(f64::NAN, |s| s.vah)),
            Value::from_f64(s.map_or(f64::NAN, |s| s.val)),
            Value::Ref(rows_array),
        ];
        let h = self
            .heap
            .alloc(HeapObj::Object {
                table_identity: None,
                type_id: fp_type_id,
                fields,
            })
            .map_err(|e| self.heap_limit_err(e, pc))?;
        Ok(Value::Ref(h))
    }

    /// Backs `footprint.get_row_by_price(fp, price)`. Resolves `footprint` to a
    /// heap object (an `na` receiver → structured `NaFieldAccess`, never a
    /// panic), reads its `rows` array (field slot 7), and returns the first
    /// `volume_row` whose `[down_price, up_price]` range contains `price`, or
    /// `na` when none matches (or `price`/the bounds are `na`). Field slots
    /// mirror `footprint::VOLUME_ROW_FIELDS` (`up_price` = 4, `down_price` = 5).
    fn footprint_row_by_price(
        &self,
        footprint: &Value,
        price: &Value,
        pc: usize,
    ) -> Result<Value, RuntimeError> {
        let fp = self.object_handle(footprint, pc)?;
        let price = match price {
            Value::Float(p) if p.is_finite() => *p,
            // An `na` (or non-finite) price cannot match any row.
            _ => return Ok(Value::Na),
        };
        // Read the rows-array handle from footprint field slot 7.
        let rows_handle = match self.heap.get(fp) {
            Some(HeapObj::Object { fields, .. }) => match fields.get(7) {
                Some(Value::Ref(h)) => *h,
                _ => return Ok(Value::Na),
            },
            _ => {
                return Err(self.fail(
                    RuntimeErrorKind::InvalidProgram,
                    "FOOTPRINT_ROW_BY_PRICE receiver is not a footprint object".into(),
                    pc,
                ))
            }
        };
        let items: Vec<Value> = match self.heap.get(rows_handle) {
            Some(HeapObj::Array { items, .. }) => items.clone(),
            _ => return Ok(Value::Na),
        };
        for item in items {
            let Value::Ref(row) = item else { continue };
            if let Some(HeapObj::Object { fields, .. }) = self.heap.get(row) {
                let up = match fields.get(4) {
                    Some(Value::Float(v)) if v.is_finite() => *v,
                    _ => continue,
                };
                let down = match fields.get(5) {
                    Some(Value::Float(v)) if v.is_finite() => *v,
                    _ => continue,
                };
                if down <= price && price <= up {
                    return Ok(Value::Ref(row));
                }
            }
        }
        Ok(Value::Na)
    }

    /// Resample `data` to `htf_seconds` and back-map builtin `source` per chart
    /// bar under `gaps_on`/`lookahead_on`. Pure over the feed (no `self`
    /// state), so it is deterministic and cacheable.
    fn compute_security_series(
        data: &MarketData,
        source: BuiltinSeries,
        htf_seconds: i64,
        calendar: resample::CalendarAlignment,
        gaps_on: bool,
        lookahead_on: bool,
    ) -> Vec<Option<f64>> {
        let r = resample::resample_aligned_security(data, htf_seconds, calendar);
        let gaps = if gaps_on {
            resample::Gaps::On
        } else {
            resample::Gaps::Off
        };
        let lookahead = if lookahead_on {
            resample::Lookahead::On
        } else {
            resample::Lookahead::Off
        };
        r.map_to_chart_bars(gaps, lookahead, |b| {
            let bar = &r.bars[b];
            Self::htf_source_value(bar, source)
        })
    }

    /// Evaluate a plain OHLCV/derived builtin `source` over one HTF bar. Only
    /// the price series (`BuiltinSeries::is_price_source`) lower into
    /// `request.security` — the analyzer rejects every other source with
    /// `E0244`/`E0246` — so the `_ => None` arm is defensive: an unsupported
    /// source reads `na`. A non-finite aggregate (e.g. na input) also reads
    /// `na`. Keep the matched set in sync with `is_price_source`.
    fn htf_source_value(bar: &resample::HtfBar, source: BuiltinSeries) -> Option<f64> {
        let (o, h, l, c) = (bar.open, bar.high, bar.low, bar.close);
        let v = match source {
            BuiltinSeries::Open => o,
            BuiltinSeries::High => h,
            BuiltinSeries::Low => l,
            BuiltinSeries::Close => c,
            BuiltinSeries::Volume => bar.volume,
            BuiltinSeries::Hl2 => f64::midpoint(h, l),
            BuiltinSeries::Hlc3 => (h + l + c) / 3.0,
            BuiltinSeries::Ohlc4 => (o + h + l + c) / 4.0,
            BuiltinSeries::Hlcc4 => (h + l + c + c) / 4.0,
            // Non-price series (time, bar_index, calendar, barstate, strategy)
            // are not plain price sources (see `BuiltinSeries::is_price_source`).
            _ => return None,
        };
        if v.is_finite() {
            Some(v)
        } else {
            None
        }
    }

    /// Resolve `inputs[input]` to a runtime [`Value`] for the current bar: the
    /// host override (when present and type-compatible with the declared kind)
    /// or the declared default. For `input.source` the resolved value is the
    /// selected builtin series' sample or a matching host-supplied external
    /// lane sample at the current bar (offset 0).
    fn resolve_input(&self, input: u32, pc: usize) -> Result<Value, RuntimeError> {
        let decl = self.program.inputs.get(input as usize).ok_or_else(|| {
            self.fail(
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
        if let Some(InputValue::ExternalSource(source)) = self.options.inputs.get(&input) {
            if decl.kind == InputKind::Source && source.is_valid() {
                if let Some(lane) = self
                    .options
                    .external_sources
                    .get(&input)
                    .filter(|lane| lane.is_valid_for(source))
                {
                    return Ok(vm_ops::external_sample_value(&lane.values, self.bar));
                }
            }
            return Ok(match &decl.defval {
                InputValue::Source(series) if series.is_input_source() => {
                    self.read_builtin(*series, 0)
                }
                _ => Value::Na,
            });
        }
        // An override is used only when its value type matches the declared
        // kind; otherwise fall back to the default (hosts validate first).
        let chosen = match self.options.inputs.get(&input) {
            Some(v) if input_value_matches_decl(v, decl) => v,
            _ => &decl.defval,
        };
        Ok(match chosen {
            // A `float` (or `price`) input always yields a float; an int
            // override (accepted by `input_value_matches_decl`) widens rather
            // than leaking an `Int` where the declared type is `float`.
            InputValue::Int(i) => vm_ops::input_scalar_value(
                vm_ops::InputScalar::Int(*i),
                matches!(decl.kind, InputKind::Float | InputKind::Price),
            ),
            InputValue::Float(f) => {
                vm_ops::input_scalar_value(vm_ops::InputScalar::Float(*f), false)
            }
            InputValue::Bool(b) => vm_ops::input_scalar_value(vm_ops::InputScalar::Bool(*b), false),
            InputValue::String(s) => Value::String(s.clone()),
            InputValue::Color { r, g, b, t } => Value::Color {
                r: *r,
                g: *g,
                b: *b,
                t: *t,
                input_origin: Some(input),
            },
            // A source input reads the selected series' current-bar sample.
            InputValue::Source(series) => self.read_builtin(*series, 0),
            // Handled above. This defensive fallback protects direct VM users
            // even if a future caller bypasses that identity/lane check.
            InputValue::ExternalSource(_) => match &decl.defval {
                InputValue::Source(series) if series.is_input_source() => {
                    self.read_builtin(*series, 0)
                }
                _ => Value::Na,
            },
            // An enum input resolves to the selected member's variant index —
            // the same `int` any enum member lowers to (see the analyzer's
            // enum-member reference). The name is metadata only.
            InputValue::Enum { index, .. } => {
                vm_ops::input_scalar_value(vm_ops::InputScalar::Enum(*index), false)
            }
        })
    }

    /// A numeric operand of a pure builtin as `f64`: `na` maps to `None`,
    /// ints widen (compiled programs pre-coerce; the widening is defense
    /// in depth), anything else is a type error.
    #[inline]
    fn pure_num(
        &self,
        func: PureFunc,
        value: &Value,
        pc: usize,
    ) -> Result<Option<f64>, RuntimeError> {
        match value {
            Value::Na | Value::DisplacedTable(_) => Ok(None),
            Value::Int(v) => Ok(Some(*v as f64)),
            Value::Float(v) => Ok(Some(*v)),
            other => Err(self.fail_with(
                RuntimeErrorKind::TypeMismatch,
                || {
                    format!(
                        "{} requires numeric arguments, found {}",
                        func.name(),
                        other.type_name()
                    )
                },
                pc,
            )),
        }
    }

    // ---- collection core -------------------------------------------------

    /// Resolve a receiver argument to a heap handle, mapping an `na` receiver
    /// to a structured [`RuntimeErrorKind::NaMethodReceiver`] error (an
    /// unassigned collection variable is `na`). A non-`Ref`, non-`na` value is
    /// a validator-caught program error (defense in depth).
    #[inline]
    fn receiver_handle(
        &self,
        func: CollectionFunc,
        value: &Value,
        pc: usize,
    ) -> Result<HandleId, RuntimeError> {
        match value {
            Value::Ref(h) => Ok(*h),
            Value::Na | Value::DisplacedTable(_) => Err(self.fail_with(
                RuntimeErrorKind::NaMethodReceiver,
                || {
                    format!(
                        "cannot call `{}` on an na collection (the collection variable was never assigned)",
                        func.name()
                    )
                },
                pc,
            )),
            other => Err(self.fail_with(
                RuntimeErrorKind::TypeMismatch,
                || {
                    format!(
                        "`{}` expects a collection receiver, found {}",
                        func.name(),
                        other.type_name()
                    )
                },
                pc,
            )),
        }
    }

    /// Resolve an object-field-access receiver to a heap handle, mapping an
    /// `na` object to a structured [`RuntimeErrorKind::NaFieldAccess`] error (an
    /// unassigned UDT variable is `na`). A non-`Ref`, non-`na` value is a
    /// validator-caught program error (defense in depth).
    #[inline]
    fn object_handle(&self, value: &Value, pc: usize) -> Result<HandleId, RuntimeError> {
        match value {
            Value::Ref(h) => Ok(*h),
            Value::Na | Value::DisplacedTable(_) => Err(self.fail_with(
                RuntimeErrorKind::NaFieldAccess,
                || {
                    "cannot access a field of an na object (the object variable was never assigned)"
                        .into()
                },
                pc,
            )),
            other => Err(self.fail_with(
                RuntimeErrorKind::TypeMismatch,
                || {
                    format!(
                        "field access expects an object, found {}",
                        other.type_name()
                    )
                },
                pc,
            )),
        }
    }

    /// Validate a table cell coordinate: read the table's
    /// declared `columns` (field slot 1) and `rows` (field slot 2) from `h`,
    /// require `column`/`row` to be non-`na` integers in `0..columns` /
    /// `0..rows`, and return them as `(i64, i64)`. An out-of-range or `na`
    /// coordinate is a structured `CollectionIndexOutOfBounds` error (never a
    /// panic).
    fn table_cell_coord(
        &self,
        h: HandleId,
        column: &Value,
        row: &Value,
        pc: usize,
    ) -> Result<(i64, i64), RuntimeError> {
        let (columns, rows) = match self.heap.get(h) {
            Some(HeapObj::Object { fields, .. }) => {
                let cols = match fields.get(1) {
                    Some(Value::Int(n)) => *n,
                    _ => 0,
                };
                let rows = match fields.get(2) {
                    Some(Value::Int(n)) => *n,
                    _ => 0,
                };
                (cols, rows)
            }
            _ => {
                return Err(self.fail(
                    RuntimeErrorKind::InvalidProgram,
                    "table cell receiver is not a table object".into(),
                    pc,
                ))
            }
        };
        let col = match column {
            Value::Int(n) => *n,
            _ => {
                return Err(self.fail(
                    RuntimeErrorKind::CollectionIndexOutOfBounds,
                    "table cell column is not an integer".into(),
                    pc,
                ))
            }
        };
        let r = match row {
            Value::Int(n) => *n,
            _ => {
                return Err(self.fail(
                    RuntimeErrorKind::CollectionIndexOutOfBounds,
                    "table cell row is not an integer".into(),
                    pc,
                ))
            }
        };
        if col < 0 || col >= columns || r < 0 || r >= rows {
            return Err(self.fail(
                RuntimeErrorKind::CollectionIndexOutOfBounds,
                format!("table cell ({col}, {r}) is out of range for a {columns}x{rows} table"),
                pc,
            ));
        }
        Ok((col, r))
    }

    /// The array's element type and current length for `h` (defense in depth:
    /// an out-of-range handle or a non-array object is a program error).
    fn array_meta(
        &self,
        func: CollectionFunc,
        h: HandleId,
        pc: usize,
    ) -> Result<(ElemType, usize), RuntimeError> {
        match self.heap.get(h) {
            Some(HeapObj::Array { elem_ty, items }) => Ok((*elem_ty, items.len())),
            _ => Err(self.fail(
                RuntimeErrorKind::InvalidProgram,
                format!("`{}` receiver is not an array", func.name()),
                pc,
            )),
        }
    }

    /// Read an int index argument (na is treated as out of bounds, never a
    /// panic) as an i64.
    #[inline]
    fn index_arg(
        &self,
        func: CollectionFunc,
        value: &Value,
        pc: usize,
    ) -> Result<i64, RuntimeError> {
        match value {
            Value::Int(i) => Ok(*i),
            // A float index (should not occur — the checker types indices as
            // int) is truncated as defense in depth.
            Value::Float(f) => Ok(*f as i64),
            _ => Err(self.fail_with(
                RuntimeErrorKind::CollectionIndexOutOfBounds,
                || format!("`{}` index is na", func.name()),
                pc,
            )),
        }
    }

    /// Bounds-check an access index against `[0, len)`; out-of-range is a
    /// structured error. `array.get`, `array.set` and `array.remove` use
    /// [`Self::resolve_access_index`], which also takes negative indices.
    #[inline]
    fn check_access_index(
        &self,
        func: CollectionFunc,
        index: i64,
        len: usize,
        pc: usize,
    ) -> Result<usize, RuntimeError> {
        if crate::array_get::checked_index(index, len).is_none() {
            return Err(self.fail_with(
                RuntimeErrorKind::CollectionIndexOutOfBounds,
                || {
                    format!(
                        "`{}` index {index} out of bounds for array of size {len}",
                        func.name()
                    )
                },
                pc,
            ));
        }
        Ok(index as usize)
    }

    /// [`Self::check_access_index`] for the functions that count a negative
    /// index from the end (`-1` is the last element).
    #[inline]
    fn resolve_access_index(
        &self,
        func: CollectionFunc,
        index: i64,
        len: usize,
        pc: usize,
    ) -> Result<usize, RuntimeError> {
        match crate::array_get::resolved_index(index, len) {
            Some(i) => Ok(i),
            None => Err(self.fail_with(
                RuntimeErrorKind::CollectionIndexOutOfBounds,
                || {
                    format!(
                        "`{}` index {index} out of bounds for array of size {len}",
                        func.name()
                    )
                },
                pc,
            )),
        }
    }

    /// Coerce an element value to the array's element type (int → float
    /// widening for a float array; otherwise the value passes through). `na`
    /// is preserved. A hard type mismatch (e.g. a string into a float array)
    /// is a structured error (defense in depth behind the checker's `E0234`).
    fn coerce_element(
        &self,
        func: CollectionFunc,
        elem_ty: ElemType,
        value: Value,
        pc: usize,
    ) -> Result<Value, RuntimeError> {
        if value.is_na() {
            return Ok(value);
        }
        let ok = match (elem_ty, &value) {
            (ElemType::Float, Value::Float(_)) => true,
            (ElemType::Float, Value::Int(i)) => return Ok(Value::from_f64(*i as f64)),
            (ElemType::Int, Value::Int(_)) => true,
            (ElemType::Bool, Value::Bool(_)) => true,
            (ElemType::String, Value::String(_)) => true,
            (ElemType::Color, Value::Color { .. }) => true,
            // A UDT element (collections-of-UDTs): any object reference is
            // accepted (the static checker already enforced the concrete type).
            (ElemType::Object(_), Value::Ref(_)) => true,
            _ => false,
        };
        if ok {
            Ok(value)
        } else {
            Err(self.fail(
                RuntimeErrorKind::TypeMismatch,
                format!(
                    "`{}` element type mismatch: array holds {}, got {}",
                    func.name(),
                    elem_ty.describe(),
                    value.type_name()
                ),
                pc,
            ))
        }
    }

    /// Bytes allocated when the direct string values in `values` are cloned.
    /// Collection cells store owned [`Value`]s, so cloning a string cell also
    /// clones its backing allocation. Project that allocation with checked
    /// arithmetic before any `Vec` materialization.
    fn projected_string_clone_bytes<'a>(
        &self,
        values: impl IntoIterator<Item = &'a Value>,
        pc: usize,
    ) -> Result<usize, RuntimeError> {
        let mut bytes = 0usize;
        for value in values {
            if let Value::String(s) = value {
                bytes = bytes.checked_add(s.len()).ok_or_else(|| {
                    self.fail(
                        RuntimeErrorKind::StringLimitExceeded,
                        "collection string-clone size overflowed".to_string(),
                        pc,
                    )
                })?;
            }
        }
        Ok(bytes)
    }

    /// Bytes allocated by cloning `value` `copies` times. This is used by
    /// collection constructors and fills, whose compact source form can
    /// otherwise expand one script string into a large number of allocations.
    fn projected_repeated_string_clone_bytes(
        &self,
        value: &Value,
        copies: usize,
        pc: usize,
    ) -> Result<usize, RuntimeError> {
        match value {
            Value::String(s) => s.len().checked_mul(copies).ok_or_else(|| {
                self.fail(
                    RuntimeErrorKind::StringLimitExceeded,
                    "repeated collection string-clone size overflowed".to_string(),
                    pc,
                )
            }),
            _ => Ok(0),
        }
    }

    /// Map a [`HeapLimitError`] to a structured runtime error.
    fn heap_limit_err(&self, e: pinewasm_mir::HeapLimitError, pc: usize) -> RuntimeError {
        self.fail(RuntimeErrorKind::CollectionLimitExceeded, e.to_string(), pc)
    }

    /// Every handle the program can still reach, for [`Heap::collect`].
    ///
    /// Completeness is a correctness requirement, not an optimization: a handle
    /// omitted here loses its object and reads as absent. The VM's other `Value`
    /// stores are deliberately *not* scanned, each for a checked reason:
    ///
    /// * `plots` / `plot_colors` / `visual_colors` / `fill_colors` hold only
    ///   what `PlotF64`, the styled-plot ops and `Fill` write, which is
    ///   `Float`/`Color`/`Na` and never a `Ref`. They are also O(bars), so scanning them would make collection
    ///   cost grow with chart length.
    /// * `ta_state` is `f64`-shaped throughout (`ta::TaState`); no variant
    ///   stores a `Value`.
    /// * `broker` holds no `Value` at all.
    /// * A [`StateSnapshot`] carries its own `Heap` clone, so a realtime
    ///   rollback restores a self-consistent heap and its slot cells are not
    ///   roots of the live one.
    fn gc_roots(&self) -> Vec<HandleId> {
        let mut roots = Vec::new();
        // Mark only cells a future instruction can read. Collection slots
        // often have no history reads, even when an unrelated dynamic numeric
        // window makes the shared ring 131072 cells deep. This runs just after
        // the bar preamble, so the current cell already contains any `var`
        // carry-forward reference.
        for (slot, history) in self.user_slots.iter().enumerate() {
            let depth = (self.gc_history_depths[slot] as usize).min(self.bar);
            for offset in 0..=depth {
                let index = (self.bar - offset) & self.slot_mask;
                vm_gc_roots::push_value_root(&mut roots, &history[index]);
            }
        }
        roots.extend(vm_gc_roots::gc_roots_core(
            &[],
            &self.stack,
            &self.call_args,
            &self.drawings.live,
        ));
        self.push_table_roots(&mut roots);
        roots
    }

    /// Push every table handle and its cells' attribute roots.
    fn push_table_roots(&self, roots: &mut Vec<HandleId>) {
        for (handle, grid) in &self.drawings.table_cells {
            // The table itself is already in `live`; its grid is keyed by the
            // same handle, and a cell attribute is a scalar today. Both are
            // scanned anyway so the grid can never become the one root that a
            // future attribute type quietly escapes through.
            roots.push(HandleId(*handle));
            for cell in grid.cells.values() {
                vm_gc_roots::push_cell_roots(roots, cell);
            }
        }
    }

    /// An AOT host's share of a module's collection (docs/aot.md §13):
    /// collect this heap from the drawing registry and `module_roots`, the
    /// host references the module holds, and return the live element and
    /// object counts.
    ///
    /// These are exactly the roots [`Vm::gc_roots`] would contribute on the
    /// interpreter, where this heap and the module's are one: the module
    /// holds every slot cell and sends the host references among the cells
    /// the interpreter roots and among the native objects reachable from
    /// them. The host's stack and call arguments hold nothing between host
    /// calls that a later instruction reads, just as the interpreter's are
    /// empty at a bar boundary, so they are not roots.
    pub(crate) fn collect_aot_host_heap(&mut self, module_roots: &[HandleId]) -> (usize, usize) {
        let mut roots = vm_gc_roots::gc_roots_core(&[], &[], &[], &self.drawings.live);
        roots.extend_from_slice(module_roots);
        self.push_table_roots(&mut roots);
        self.heap.collect(&roots);
        (self.heap.live_elements(), self.heap.len())
    }

    /// Collect the heap if the arena has grown past [`Vm::gc_threshold`], then
    /// re-arm the threshold at twice the surviving live set.
    ///
    /// Called only at a bar boundary. A program whose collections all persist
    /// keeps its live set, sees nothing reclaimed, and doubles the threshold, so
    /// the amortized cost of collecting is proportional to allocation rate
    /// rather than to chart length.
    fn maybe_collect_heap(&mut self) {
        let arena_pressure = self.heap.arena_len() >= self.gc_threshold;
        let elem_pressure = self.heap.live_elements() >= self.gc_elem_threshold;
        if !arena_pressure && !elem_pressure {
            return;
        }
        let roots = self.gc_roots();
        self.heap.collect(&roots);
        // Re-arm against the *arena*, not the live set. Collected slots are
        // never reissued, so `arena_len` only grows: re-arming from the live
        // count would leave the threshold below an arena that is already past
        // it, and every later bar would sweep. Doubling keeps the sweeps
        // geometric, so their total cost stays linear in allocations.
        self.gc_threshold = GC_MIN_ARENA.max(self.heap.arena_len().saturating_mul(2));
        self.gc_elem_threshold = re_arm_elem_threshold(&self.heap);
    }

    /// The number of elements an O(n) collection call processes, for metering.
    /// Constructors cost the produced length; receiver operations cost the
    /// receiver's current size (concat adds the second array's size); the
    /// superlinear matrix operations (product, Kronecker, LU family, integer
    /// power) cost their true work, not just the receiver's grid. O(1)
    /// callers never invoke this. A best-effort estimate: `na`/bad receivers
    /// cost 1 (they abort anyway).
    fn collection_extra_cost(&self, func: CollectionFunc, args: &[Value]) -> u64 {
        use CollectionFunc as F;
        // The live element count of the collection `v` refers to (array
        // length, matrix cell count, or map entry count), or 1 if `v` is not a
        // live collection handle.
        let recv_len = |v: &Value| -> u64 {
            match v.as_ref().and_then(|h| self.heap.get(h)) {
                Some(obj) => obj.element_count() as u64,
                _ => 1,
            }
        };
        // The (rows, columns) of the live matrix `v` refers to, if any. Used
        // by the superlinear matrix costs below; a non-matrix operand falls
        // back to the linear cost (the op aborts with a structured error).
        let matrix_dims = |v: &Value| -> Option<(u64, u64)> {
            match v.as_ref().and_then(|h| self.heap.get(h)) {
                Some(HeapObj::Matrix { rows, cols, .. }) => Some((*rows as u64, *cols as u64)),
                _ => None,
            }
        };
        match func {
            F::ArrayFrom => args.len() as u64,
            // str.split cost: the source length (an upper bound on the number of
            // produced pieces). A non-string source costs 1 (it aborts anyway).
            F::StrSplit => match args.first() {
                Some(Value::String(s)) => s.chars().count() as u64 + 1,
                _ => 1,
            },
            F::ArrayNewFloat
            | F::ArrayNewInt
            | F::ArrayNewBool
            | F::ArrayNewString
            | F::ArrayNewColor
            | F::ArrayNewObject => match args.first() {
                Some(Value::Int(n)) if *n > 0 => *n as u64,
                _ => 1,
            },
            // concat / put_all cost both collections' sizes.
            F::ArrayConcat | F::MapPutAll | F::MatrixConcat => {
                recv_len(&args[0]) + args.get(1).map_or(0, recv_len)
            }
            // matrix.new_<T>: cost the produced cell count (rows * columns).
            // `saturating_mul` so absurd dimensions (whose product overflows
            // i64) do not panic in the metering; the cost saturates huge and
            // trips the per-bar instruction budget, and the op handler's own
            // `checked_mul` (see `F::MatrixNew` execution) rejects the count as
            // `CollectionLimitExceeded` — a structured error, never a panic.
            F::MatrixNew => match (args.first(), args.get(1)) {
                (Some(Value::Int(r)), Some(Value::Int(c))) if *r > 0 && *c > 0 => {
                    r.saturating_mul(*c) as u64
                }
                _ => 1,
            },
            // matrix.mult(A[r×c], B[c×c2]) performs r*c*c2 multiply-adds (every
            // output cell sums over the inner dimension), so charge the full
            // triple product — charging only A's grid would let one call do
            // orders of magnitude more work than the budget records. The
            // scalar-multiply form (second operand not a matrix) stays O(cells).
            F::MatrixMult => {
                let cells = recv_len(&args[0]).max(1);
                match args.get(1).and_then(matrix_dims) {
                    Some((_, c2)) => cells.saturating_mul(c2.max(1)),
                    None => cells,
                }
            }
            // The Kronecker product writes (and computes) cells1 * cells2
            // output cells: charge the product of both grids, not their sum.
            F::MatrixKron => {
                let a = recv_len(&args[0]).max(1);
                let b = args.get(1).map_or(1, recv_len).max(1);
                a.saturating_mul(b)
            }
            // The LU-elimination family (determinant, inverse, rank) is
            // O(rows * cols * min(rows, cols)) — n^3 for square matrices —
            // so charge cells * min-dimension, not just the grid size.
            F::MatrixDet | F::MatrixInv | F::MatrixRank => {
                let cells = recv_len(&args[0]).max(1);
                let min_dim = matrix_dims(&args[0]).map_or(1, |(r, c)| r.min(c).max(1));
                cells.saturating_mul(min_dim)
            }
            // matrix.pow runs `exponent` n×n matrix multiplies (each O(n^3),
            // i.e. cells * n), so cost it `exponent * cells * n`. This makes a
            // large exponent or grid trip the per-bar instruction budget at
            // dispatch (InstructionLimit) instead of hanging in an unmetered
            // internal loop.
            F::MatrixPow => {
                let cells = recv_len(&args[0]).max(1);
                let n = matrix_dims(&args[0]).map_or(1, |(r, _)| r.max(1));
                let exp = match args.get(1) {
                    Some(Value::Int(p)) if *p > 0 => *p as u64,
                    _ => 1,
                };
                exp.saturating_mul(cells).saturating_mul(n)
            }
            // Receiver-taking O(n) operations: cost the receiver's size.
            _ => recv_len(&args[0]),
        }
    }

    /// Execute one `array.*` collection call. Constructors allocate; operations
    /// resolve the receiver handle (na → structured error) and mutate/read the
    /// heap object. Every void mutator returns `Value::Na` (the placeholder the
    /// stack discipline expects).
    fn call_collection(
        &mut self,
        func: CollectionFunc,
        args: &[Value],
        pc: usize,
    ) -> Result<Value, RuntimeError> {
        use CollectionFunc as F;
        match func {
            F::InternalDistanceScanEligible => Ok(Value::Bool(self.distance_scan_eligible(args))),
            F::InternalDistanceScan => Err(self.fail(
                RuntimeErrorKind::InvalidProgram,
                "internal distance scan requires its metered dispatcher".into(),
                pc,
            )),
            // ---- constructors (size, initial) ----------------------------
            F::ArrayNewFloat
            | F::ArrayNewInt
            | F::ArrayNewBool
            | F::ArrayNewString
            | F::ArrayNewColor
            | F::ArrayNewObject => {
                let elem_ty = match func {
                    F::ArrayNewFloat => ElemType::Float,
                    F::ArrayNewInt => ElemType::Int,
                    F::ArrayNewBool => ElemType::Bool,
                    F::ArrayNewString => ElemType::String,
                    F::ArrayNewObject => ElemType::from_tag(match &args[2] {
                        Value::Int(tag) => *tag,
                        _ => 0,
                    })
                    .unwrap_or(ElemType::Float),
                    _ => ElemType::Color,
                };
                // args = [size, initial], plus type_tag for object arrays.
                let size = match &args[0] {
                    Value::Int(i) if *i >= 0 => usize::try_from(*i).map_err(|_| {
                        self.fail(
                            RuntimeErrorKind::CollectionLimitExceeded,
                            format!("`{}` size is too large", func.name()),
                            pc,
                        )
                    })?,
                    Value::Int(_) => {
                        return Err(self.fail(
                            RuntimeErrorKind::CollectionIndexOutOfBounds,
                            format!("`{}` size must be non-negative", func.name()),
                            pc,
                        ));
                    }
                    // na size defaults to 0 (defense in depth).
                    _ => 0,
                };
                let h = vm_heap_ops::array_new(
                    &mut self.heap,
                    &mut self.string_bytes_allocated,
                    self.options.max_string_bytes_per_run,
                    size,
                    elem_ty,
                    &args[1],
                )
                .map_err(|error| self.heap_op_error(func, error, args, 1, pc))?;
                Ok(Value::Ref(h))
            }
            F::ArrayFrom => {
                // Element type inferred from the first non-na arg (the checker
                // has verified they all agree); default to float if all na.
                let elem_ty = args
                    .iter()
                    .find_map(|value| {
                        elem_type_of_value(value).or_else(|| match value {
                            Value::Ref(handle) => match self.heap.get(*handle) {
                                Some(HeapObj::Object { type_id, .. }) => {
                                    Some(ElemType::Object(*type_id))
                                }
                                _ => None,
                            },
                            _ => None,
                        })
                    })
                    .unwrap_or(ElemType::Float);
                self.check_capacity(args.len(), pc)?;
                let string_bytes = self.projected_string_clone_bytes(args.iter(), pc)?;
                let charged_before = self.string_bytes_allocated;
                self.reserve_string_bytes(string_bytes, pc)?;
                let mut items = Vec::with_capacity(args.len());
                for a in args {
                    match self.coerce_element(func, elem_ty, a.clone(), pc) {
                        Ok(v) => items.push(v),
                        Err(error) => {
                            // A mismatch leaves the string budget as it was.
                            self.string_bytes_allocated = charged_before;
                            return Err(error);
                        }
                    }
                }
                let h = self.alloc_after_string_charge(
                    HeapObj::Array { elem_ty, items },
                    charged_before,
                    pc,
                )?;
                Ok(Value::Ref(h))
            }

            // ---- str.split(source, separator) -> array<string> -----------
            // Lives in the `str.*` surface but returns a heap array, so it
            // allocates here like a constructor. `na` source or separator
            // propagates to an `na` result (the collection is never built).
            F::StrSplit => {
                let (source, sep) = match (&args[0], &args[1]) {
                    (Value::String(s), Value::String(d)) => (s.as_str(), d.as_str()),
                    // na source or separator yields na.
                    _ => return Ok(Value::Na),
                };
                // Count both cells and output bytes without allocating. In
                // particular, reject a separator bomb before `collect` can
                // materialize more than the configured collection limit.
                let (piece_count, string_bytes) = if sep.is_empty() {
                    (source.chars().count(), source.len())
                } else {
                    source
                        .split(sep)
                        .try_fold((0usize, 0usize), |(count, bytes), piece| {
                            Some((count.checked_add(1)?, bytes.checked_add(piece.len())?))
                        })
                        .ok_or_else(|| {
                            self.fail(
                                RuntimeErrorKind::CollectionLimitExceeded,
                                "`str.split` result size overflowed".to_string(),
                                pc,
                            )
                        })?
                };
                self.check_capacity(piece_count, pc)?;
                let charged_before = self.string_bytes_allocated;
                self.reserve_string_bytes(string_bytes, pc)?;
                let mut items = Vec::with_capacity(piece_count);
                if sep.is_empty() {
                    // Empty separator: split into individual characters
                    // (Unicode code points).
                    items.extend(source.chars().map(|c| Value::String(c.to_string())));
                } else {
                    items.extend(source.split(sep).map(|p| Value::String(p.to_string())));
                }
                // `alloc` charges the element count against the heap cap
                // (never panics: a limit is a structured error).
                let h = self.alloc_after_string_charge(
                    HeapObj::Array {
                        elem_ty: ElemType::String,
                        items,
                    },
                    charged_before,
                    pc,
                )?;
                Ok(Value::Ref(h))
            }

            // ---- map.new (two int type tags: key, val) -------------------
            F::MapNew => {
                let key_ty = ElemType::from_tag(match &args[0] {
                    Value::Int(t) => *t,
                    _ => 0,
                })
                .unwrap_or(ElemType::Float);
                let val_ty = ElemType::from_tag(match &args[1] {
                    Value::Int(t) => *t,
                    _ => 0,
                })
                .unwrap_or(ElemType::Float);
                let h = self
                    .heap
                    .alloc(HeapObj::Map {
                        key_ty,
                        val_ty,
                        entries: Vec::new(),
                    })
                    .map_err(|e| self.heap_limit_err(e, pc))?;
                Ok(Value::Ref(h))
            }

            // ---- map receiver operations ---------------------------------
            F::MapPut
            | F::MapGet
            | F::MapContains
            | F::MapRemove
            | F::MapSize
            | F::MapClear
            | F::MapKeys
            | F::MapValues
            | F::MapCopy
            | F::MapPutAll => self.call_map_op(func, args, pc),

            // ---- matrix constructor --------------------------------------
            F::MatrixNew => {
                // args = [rows, columns, initial, elem_ty_tag]
                let h = vm_heap_ops::matrix_new(
                    &mut self.heap,
                    &mut self.string_bytes_allocated,
                    self.options.max_string_bytes_per_run,
                    &args[0],
                    &args[1],
                    &args[2],
                    &args[3],
                )
                .map_err(|error| self.heap_op_error(func, error, args, 2, pc))?;
                Ok(Value::Ref(h))
            }

            // ---- matrix receiver operations ------------------------------
            F::MatrixGet
            | F::MatrixSet
            | F::MatrixRows
            | F::MatrixColumns
            | F::MatrixElementsCount
            | F::MatrixFill
            | F::MatrixCopy
            | F::MatrixAddRow
            | F::MatrixAddCol
            | F::MatrixRemoveRow
            | F::MatrixRemoveCol
            | F::MatrixSwapRows
            | F::MatrixSwapColumns
            | F::MatrixReverse
            | F::MatrixTranspose
            | F::MatrixReshape
            | F::MatrixSubmatrix
            | F::MatrixConcat
            | F::MatrixSum
            | F::MatrixDiff
            | F::MatrixMult
            | F::MatrixDet
            | F::MatrixInv
            | F::MatrixTrace
            | F::MatrixPow
            | F::MatrixKron
            | F::MatrixAvg
            | F::MatrixMin
            | F::MatrixMax
            | F::MatrixMedian
            | F::MatrixMode
            | F::MatrixIsSquare
            | F::MatrixIsIdentity
            | F::MatrixIsDiagonal
            | F::MatrixIsSymmetric
            | F::MatrixIsAntisymmetric
            | F::MatrixIsTriangular
            | F::MatrixIsStochastic
            | F::MatrixIsBinary
            | F::MatrixIsZero
            | F::MatrixIsAntidiagonal
            | F::MatrixRank
            | F::MatrixRow
            | F::MatrixCol
            | F::MatrixEigenvalues
            | F::MatrixEigenvectors
            | F::MatrixPinv
            | F::MatrixSort => self.call_matrix_op(func, args, pc),

            // ---- array operations ----------------------------------------
            _ => self.call_array_op(func, args, pc),
        }
    }

    /// The map's key and value element types for handle `h` (defense in depth:
    /// a non-map object or out-of-range handle is a program error).
    fn map_meta(
        &self,
        func: CollectionFunc,
        h: HandleId,
        pc: usize,
    ) -> Result<(ElemType, ElemType), RuntimeError> {
        match self.heap.get(h) {
            Some(HeapObj::Map { key_ty, val_ty, .. }) => Ok((*key_ty, *val_ty)),
            _ => Err(self.fail(
                RuntimeErrorKind::InvalidProgram,
                format!("`{}` receiver is not a map", func.name()),
                pc,
            )),
        }
    }

    /// The receiver-taking `map.*` operations. Resolves the receiver handle
    /// (`na` → structured error), then reads/mutates the heap map. Maps are
    /// insertion-ordered: `put` overwrites in place (keeping position) or
    /// appends; `keys`/`values` project in insertion order. Every void mutator
    /// returns `Value::Na`.
    /// Coerce a map key and reject `na`: an `na` key can never be found by
    /// `values_equal` (na compares unequal to everything), so accepting one
    /// would create an unreachable, unremovable entry. Keys are non-na.
    fn map_key(
        &self,
        func: CollectionFunc,
        key_ty: ElemType,
        raw: Value,
        pc: usize,
    ) -> Result<Value, RuntimeError> {
        let key = self.coerce_element(func, key_ty, raw, pc)?;
        if key.is_na() {
            return Err(self.fail(
                RuntimeErrorKind::TypeMismatch,
                "map key must not be `na`".to_string(),
                pc,
            ));
        }
        Ok(key)
    }

    fn call_map_op(
        &mut self,
        func: CollectionFunc,
        args: &[Value],
        pc: usize,
    ) -> Result<Value, RuntimeError> {
        use CollectionFunc as F;
        let h = self.receiver_handle(func, &args[0], pc)?;
        let (key_ty, val_ty) = self.map_meta(func, h, pc)?;

        match func {
            F::MapSize => {
                let n = match self.heap.get(h) {
                    Some(HeapObj::Map { entries, .. }) => entries.len(),
                    _ => 0,
                };
                Ok(Value::Int(n as i64))
            }

            F::MapContains => {
                let key_bytes = self.projected_repeated_string_clone_bytes(&args[1], 1, pc)?;
                let key = self.map_key(func, key_ty, args[1].clone(), pc)?;
                self.reserve_string_bytes(key_bytes, pc)?;
                let found = match self.heap.get(h) {
                    Some(HeapObj::Map { entries, .. }) => {
                        entries.iter().any(|(k, _)| values_equal(k, &key))
                    }
                    _ => false,
                };
                Ok(Value::Bool(found))
            }

            F::MapGet => {
                let key_bytes = self.projected_repeated_string_clone_bytes(&args[1], 1, pc)?;
                let key = self.map_key(func, key_ty, args[1].clone(), pc)?;
                self.reserve_string_bytes(key_bytes, pc)?;
                let existing = match self.heap.get(h) {
                    Some(HeapObj::Map { entries, .. }) => {
                        entries.iter().position(|(k, _)| values_equal(k, &key))
                    }
                    _ => None,
                };
                let value_bytes = existing
                    .and_then(|idx| match self.heap.get(h) {
                        Some(HeapObj::Map { entries, .. }) => entries.get(idx),
                        _ => None,
                    })
                    .map_or(Ok(0), |(_, value)| {
                        self.projected_repeated_string_clone_bytes(value, 1, pc)
                    })?;
                self.reserve_string_bytes(value_bytes, pc)?;
                let out = match (self.heap.get(h), existing) {
                    (Some(HeapObj::Map { entries, .. }), Some(idx)) => entries[idx].1.clone(),
                    _ => Value::Na,
                };
                Ok(out)
            }

            F::MapPut => {
                let clone_bytes = self.projected_string_clone_bytes(args[1..=2].iter(), pc)?;
                let key = self.map_key(func, key_ty, args[1].clone(), pc)?;
                let val = self.coerce_element(func, val_ty, args[2].clone(), pc)?;
                self.reserve_string_bytes(clone_bytes, pc)?;
                // Overwrite in place if present (keeping insertion position),
                // returning the previous value; otherwise reserve budget and
                // append.
                let existing = match self.heap.get(h) {
                    Some(HeapObj::Map { entries, .. }) => {
                        entries.iter().position(|(k, _)| values_equal(k, &key))
                    }
                    _ => None,
                };
                if let Some(idx) = existing {
                    if let Some(HeapObj::Map { entries, .. }) = self.heap.get_mut(h) {
                        let prev = std::mem::replace(&mut entries[idx].1, val);
                        return Ok(prev);
                    }
                    Ok(Value::Na)
                } else {
                    self.heap
                        .reserve(1)
                        .map_err(|e| self.heap_limit_err(e, pc))?;
                    if let Some(HeapObj::Map { entries, .. }) = self.heap.get_mut(h) {
                        entries.push((key, val));
                    }
                    // A new key had no previous value.
                    Ok(Value::Na)
                }
            }

            F::MapRemove => {
                let key_bytes = self.projected_repeated_string_clone_bytes(&args[1], 1, pc)?;
                let key = self.map_key(func, key_ty, args[1].clone(), pc)?;
                self.reserve_string_bytes(key_bytes, pc)?;
                let removed = if let Some(HeapObj::Map { entries, .. }) = self.heap.get_mut(h) {
                    entries
                        .iter()
                        .position(|(k, _)| values_equal(k, &key))
                        .map(|idx| entries.remove(idx).1)
                } else {
                    None
                };
                match removed {
                    Some(v) => {
                        self.heap.release(1);
                        Ok(v)
                    }
                    None => Ok(Value::Na),
                }
            }

            F::MapClear => {
                let removed = if let Some(HeapObj::Map { entries, .. }) = self.heap.get_mut(h) {
                    let n = entries.len();
                    entries.clear();
                    n
                } else {
                    0
                };
                self.heap.release(removed);
                Ok(Value::Na)
            }

            F::MapKeys => {
                let (count, clone_bytes) = match self.heap.get(h) {
                    Some(HeapObj::Map { entries, .. }) => (
                        entries.len(),
                        self.projected_string_clone_bytes(entries.iter().map(|(k, _)| k), pc)?,
                    ),
                    _ => (0, 0),
                };
                self.check_capacity(count, pc)?;
                let charged_before = self.string_bytes_allocated;
                self.reserve_string_bytes(clone_bytes, pc)?;
                let keys: Vec<Value> = match self.heap.get(h) {
                    Some(HeapObj::Map { entries, .. }) => {
                        entries.iter().map(|(k, _)| k.clone()).collect()
                    }
                    _ => Vec::new(),
                };
                let new = self.alloc_after_string_charge(
                    HeapObj::Array {
                        elem_ty: key_ty,
                        items: keys,
                    },
                    charged_before,
                    pc,
                )?;
                Ok(Value::Ref(new))
            }

            F::MapValues => {
                let (count, clone_bytes) = match self.heap.get(h) {
                    Some(HeapObj::Map { entries, .. }) => (
                        entries.len(),
                        self.projected_string_clone_bytes(entries.iter().map(|(_, v)| v), pc)?,
                    ),
                    _ => (0, 0),
                };
                self.check_capacity(count, pc)?;
                let charged_before = self.string_bytes_allocated;
                self.reserve_string_bytes(clone_bytes, pc)?;
                let vals: Vec<Value> = match self.heap.get(h) {
                    Some(HeapObj::Map { entries, .. }) => {
                        entries.iter().map(|(_, v)| v.clone()).collect()
                    }
                    _ => Vec::new(),
                };
                let new = self.alloc_after_string_charge(
                    HeapObj::Array {
                        elem_ty: val_ty,
                        items: vals,
                    },
                    charged_before,
                    pc,
                )?;
                Ok(Value::Ref(new))
            }

            F::MapCopy => {
                let (count, clone_bytes) = match self.heap.get(h) {
                    Some(HeapObj::Map { entries, .. }) => (
                        entries.len(),
                        self.projected_string_clone_bytes(
                            entries.iter().flat_map(|(k, v)| [k, v]),
                            pc,
                        )?,
                    ),
                    _ => (0, 0),
                };
                self.check_capacity(count, pc)?;
                let charged_before = self.string_bytes_allocated;
                self.reserve_string_bytes(clone_bytes, pc)?;
                let entries: Vec<(Value, Value)> = match self.heap.get(h) {
                    Some(HeapObj::Map { entries, .. }) => entries.clone(),
                    _ => Vec::new(),
                };
                let new = self.alloc_after_string_charge(
                    HeapObj::Map {
                        key_ty,
                        val_ty,
                        entries,
                    },
                    charged_before,
                    pc,
                )?;
                Ok(Value::Ref(new))
            }

            F::MapPutAll => {
                let src = self.receiver_handle(func, &args[1], pc)?;
                if src == h {
                    return Ok(Value::Na);
                }
                let (new_count, clone_bytes) = match (self.heap.get(h), self.heap.get(src)) {
                    (
                        Some(HeapObj::Map {
                            entries: dst_entries,
                            ..
                        }),
                        Some(HeapObj::Map {
                            entries: src_entries,
                            ..
                        }),
                    ) => {
                        let new_count = src_entries
                            .iter()
                            .filter(|(key, _)| {
                                !dst_entries
                                    .iter()
                                    .any(|(dst_key, _)| values_equal(dst_key, key))
                            })
                            .count();
                        let clone_bytes = self.projected_string_clone_bytes(
                            src_entries.iter().flat_map(|(k, v)| [k, v]),
                            pc,
                        )?;
                        (new_count, clone_bytes)
                    }
                    _ => (0, 0),
                };
                self.reserve_cells(new_count, pc)?;
                if let Err(error) = self.reserve_string_bytes(clone_bytes, pc) {
                    self.heap.release(new_count);
                    return Err(error);
                }
                // Snapshot the source's entries (src may equal dest — aliasing).
                let src_entries: Vec<(Value, Value)> = match self.heap.get(src) {
                    Some(HeapObj::Map { entries, .. }) => entries.clone(),
                    _ => Vec::new(),
                };
                for (k, v) in src_entries {
                    // Overwrite in place, else reserve and append.
                    let existing = match self.heap.get(h) {
                        Some(HeapObj::Map { entries, .. }) => {
                            entries.iter().position(|(ek, _)| values_equal(ek, &k))
                        }
                        _ => None,
                    };
                    if let Some(idx) = existing {
                        if let Some(HeapObj::Map { entries, .. }) = self.heap.get_mut(h) {
                            entries[idx].1 = v;
                        }
                    } else if let Some(HeapObj::Map { entries, .. }) = self.heap.get_mut(h) {
                        entries.push((k, v));
                    }
                }
                Ok(Value::Na)
            }

            _ => Err(self.fail(
                RuntimeErrorKind::InvalidProgram,
                format!("`{}` is not a map operation", func.name()),
                pc,
            )),
        }
    }

    /// The matrix's element type and shape for handle `h` (defense in depth: a
    /// non-matrix object or out-of-range handle is a program error).
    fn matrix_meta(
        &self,
        func: CollectionFunc,
        h: HandleId,
        pc: usize,
    ) -> Result<(ElemType, usize, usize), RuntimeError> {
        match self.heap.get(h) {
            Some(HeapObj::Matrix {
                elem_ty,
                rows,
                cols,
                ..
            }) => Ok((*elem_ty, *rows, *cols)),
            _ => Err(self.fail(
                RuntimeErrorKind::InvalidProgram,
                format!("`{}` receiver is not a matrix", func.name()),
                pc,
            )),
        }
    }

    /// A structured matrix dimension-mismatch error.
    fn dim_err(&self, func: CollectionFunc, detail: &str, pc: usize) -> RuntimeError {
        self.fail(
            RuntimeErrorKind::MatrixDimensionMismatch,
            format!("`{}`: {detail}", func.name()),
            pc,
        )
    }

    /// The cells of numeric matrix `h` as an owned `(rows, cols, Vec<f64>)`
    /// snapshot; `na` cells become `NaN`. Used by the linear-algebra ops.
    fn matrix_floats(&self, h: HandleId) -> Option<(usize, usize, Vec<f64>)> {
        match self.heap.get(h) {
            Some(HeapObj::Matrix {
                rows, cols, items, ..
            }) => Some((
                *rows,
                *cols,
                items
                    .iter()
                    .map(|v| value_as_f64(v).unwrap_or(f64::NAN))
                    .collect(),
            )),
            _ => None,
        }
    }

    /// Allocate a fresh `rows × cols` float matrix from `data` (row-major).
    /// `NaN` cells serialize to `na`.
    fn alloc_float_matrix(
        &mut self,
        rows: usize,
        cols: usize,
        data: Vec<f64>,
        pc: usize,
    ) -> Result<Value, RuntimeError> {
        let items: Vec<Value> = data
            .into_iter()
            .map(|x| {
                if x.is_finite() {
                    Value::from_f64(x)
                } else {
                    Value::Na
                }
            })
            .collect();
        let h = self
            .heap
            .alloc(HeapObj::Matrix {
                elem_ty: ElemType::Float,
                rows,
                cols,
                items,
            })
            .map_err(|e| self.heap_limit_err(e, pc))?;
        Ok(Value::Ref(h))
    }

    /// The receiver-taking `matrix.*` operations. Resolves the receiver handle
    /// (`na` → structured error), then reads/mutates the heap matrix. Numeric
    /// linear-algebra ops operate on an `f64` snapshot and allocate a fresh
    /// float matrix. Every void mutator returns `Value::Na`; structural line
    /// operations allocate `array<T>` values carrying the matrix element tag.
    fn call_matrix_op(
        &mut self,
        func: CollectionFunc,
        args: &[Value],
        pc: usize,
    ) -> Result<Value, RuntimeError> {
        use CollectionFunc as F;
        let h = self.receiver_handle(func, &args[0], pc)?;
        let (elem_ty, rows, cols) = self.matrix_meta(func, h, pc)?;
        match func {
            // ---- shape queries -------------------------------------------
            F::MatrixRows => Ok(Value::Int(rows as i64)),
            F::MatrixColumns => Ok(Value::Int(cols as i64)),
            F::MatrixElementsCount => Ok(Value::Int((rows * cols) as i64)),

            // ---- get / set -----------------------------------------------
            F::MatrixGet => {
                let (r, c) = (
                    self.index_arg(func, &args[1], pc)?,
                    self.index_arg(func, &args[2], pc)?,
                );
                let (ri, ci) = self.check_cell(func, r, c, rows, cols, pc)?;
                let clone_bytes = match self.heap.get(h) {
                    Some(HeapObj::Matrix { items, .. }) => {
                        self.projected_repeated_string_clone_bytes(&items[ri * cols + ci], 1, pc)?
                    }
                    _ => 0,
                };
                self.reserve_string_bytes(clone_bytes, pc)?;
                let v = match self.heap.get(h) {
                    Some(HeapObj::Matrix { items, .. }) => items[ri * cols + ci].clone(),
                    _ => Value::Na,
                };
                Ok(v)
            }
            F::MatrixSet => {
                let (r, c) = (
                    self.index_arg(func, &args[1], pc)?,
                    self.index_arg(func, &args[2], pc)?,
                );
                let (ri, ci) = self.check_cell(func, r, c, rows, cols, pc)?;
                let clone_bytes = self.projected_repeated_string_clone_bytes(&args[3], 1, pc)?;
                let v = self.coerce_element(func, elem_ty, args[3].clone(), pc)?;
                self.reserve_string_bytes(clone_bytes, pc)?;
                if let Some(HeapObj::Matrix { items, .. }) = self.heap.get_mut(h) {
                    items[ri * cols + ci] = v;
                }
                Ok(Value::Na)
            }

            // ---- fill (whole matrix; -1 sentinels = full range) ----------
            F::MatrixFill => {
                let from_row = self.index_arg(func, &args[2], pc)?;
                let to_row = self.index_arg(func, &args[3], pc)?;
                let from_col = self.index_arg(func, &args[4], pc)?;
                let to_col = self.index_arg(func, &args[5], pc)?;
                let r0 = from_row.max(0) as usize;
                let r1 = if to_row < 0 {
                    rows
                } else {
                    (to_row as usize).min(rows)
                };
                let c0 = from_col.max(0) as usize;
                let c1 = if to_col < 0 {
                    cols
                } else {
                    (to_col as usize).min(cols)
                };
                let filled = r1
                    .saturating_sub(r0)
                    .checked_mul(c1.saturating_sub(c0))
                    .ok_or_else(|| {
                        self.fail(
                            RuntimeErrorKind::CollectionLimitExceeded,
                            "matrix fill size overflowed".to_string(),
                            pc,
                        )
                    })?;
                let clone_bytes = self.projected_repeated_string_clone_bytes(
                    &args[1],
                    filled.saturating_add(1),
                    pc,
                )?;
                let v = self.coerce_element(func, elem_ty, args[1].clone(), pc)?;
                self.reserve_string_bytes(clone_bytes, pc)?;
                if let Some(HeapObj::Matrix { items, .. }) = self.heap.get_mut(h) {
                    for r in r0..r1 {
                        for c in c0..c1 {
                            items[r * cols + c] = v.clone();
                        }
                    }
                }
                Ok(Value::Na)
            }

            // ---- copy ----------------------------------------------------
            F::MatrixCopy => {
                let count = rows.checked_mul(cols).ok_or_else(|| {
                    self.fail(
                        RuntimeErrorKind::CollectionLimitExceeded,
                        "matrix copy size overflowed".to_string(),
                        pc,
                    )
                })?;
                self.check_capacity(count, pc)?;
                let clone_bytes = match self.heap.get(h) {
                    Some(HeapObj::Matrix { items, .. }) => {
                        self.projected_string_clone_bytes(items.iter(), pc)?
                    }
                    _ => 0,
                };
                let charged_before = self.string_bytes_allocated;
                self.reserve_string_bytes(clone_bytes, pc)?;
                let items = match self.heap.get(h) {
                    Some(HeapObj::Matrix { items, .. }) => items.clone(),
                    _ => Vec::new(),
                };
                let nh = self.alloc_after_string_charge(
                    HeapObj::Matrix {
                        elem_ty,
                        rows,
                        cols,
                        items,
                    },
                    charged_before,
                    pc,
                )?;
                Ok(Value::Ref(nh))
            }

            // ---- add_row / add_col ---------------------------------------
            F::MatrixAddRow => {
                // An `na` index or array is the omitted optional: append, and
                // fill the new row with `na`.
                let idx = if matches!(args[1], Value::Na) {
                    rows as i64
                } else {
                    self.index_arg(func, &args[1], pc)?
                };
                let line_h = if matches!(args[2], Value::Na) {
                    None
                } else {
                    Some(self.receiver_handle(func, &args[2], pc)?)
                };
                let line_len = match line_h {
                    Some(line_h) => self.array_meta(func, line_h, pc)?.1,
                    None => cols,
                };
                // Preserve an established column count even after the last
                // row is removed. Only a dimensionless 0x0 matrix adopts it.
                let width = if rows == 0 && cols == 0 {
                    line_len
                } else {
                    cols
                };
                if line_len != width {
                    return Err(self.dim_err(
                        func,
                        &format!("row length {line_len} does not match column count {width}"),
                        pc,
                    ));
                }
                if idx < 0 || idx as usize > rows {
                    return Err(self.oob(func, idx, rows, "row", pc));
                }
                // An empty row costs no cells, so the element budget cannot
                // bound dimension growth. Check both heap and public limits.
                let new_rows = rows
                    .checked_add(1)
                    .filter(|dimension| i64::try_from(*dimension).is_ok())
                    .ok_or_else(|| {
                        self.fail(
                            RuntimeErrorKind::CollectionLimitExceeded,
                            "matrix row insertion count overflows".to_string(),
                            pc,
                        )
                    })?;
                self.reserve_cells(width, pc)?;
                let clone_bytes = match line_h.and_then(|line_h| self.heap.get(line_h)) {
                    Some(HeapObj::Array { items, .. }) => {
                        self.projected_string_clone_bytes(items.iter(), pc)?
                    }
                    _ => 0,
                };
                if let Err(error) = self.reserve_string_bytes(clone_bytes, pc) {
                    self.heap.release(width);
                    return Err(error);
                }
                let new_row = if line_h.is_none() {
                    vec![Value::Na; width]
                } else {
                    match self.array_line(func, &args[2], elem_ty, pc) {
                        Ok(line) => line,
                        Err(error) => {
                            self.heap.release(width);
                            return Err(error);
                        }
                    }
                };
                if let Some(HeapObj::Matrix {
                    rows: r,
                    cols: c,
                    items,
                    ..
                }) = self.heap.get_mut(h)
                {
                    let at = (idx as usize) * width;
                    items.splice(at..at, new_row);
                    *r = new_rows;
                    if *c == 0 {
                        *c = width;
                    }
                }
                Ok(Value::Na)
            }
            F::MatrixAddCol => {
                let idx = if matches!(args[1], Value::Na) {
                    cols as i64
                } else {
                    self.index_arg(func, &args[1], pc)?
                };
                let line_h = if matches!(args[2], Value::Na) {
                    None
                } else {
                    Some(self.receiver_handle(func, &args[2], pc)?)
                };
                let line_len = match line_h {
                    Some(line_h) => self.array_meta(func, line_h, pc)?.1,
                    None => rows,
                };
                let height = if rows == 0 && cols == 0 {
                    line_len
                } else {
                    rows
                };
                if line_len != height {
                    return Err(self.dim_err(
                        func,
                        &format!("column length {line_len} does not match row count {height}"),
                        pc,
                    ));
                }
                if idx < 0 || idx as usize > cols {
                    return Err(self.oob(func, idx, cols, "column", pc));
                }
                // Zero-height columns likewise need a dimension guard before
                // any charge, cloning, or row-major buffer reconstruction.
                let new_cols = cols
                    .checked_add(1)
                    .filter(|dimension| i64::try_from(*dimension).is_ok())
                    .ok_or_else(|| {
                        self.fail(
                            RuntimeErrorKind::CollectionLimitExceeded,
                            "matrix column insertion count overflows".to_string(),
                            pc,
                        )
                    })?;
                self.reserve_cells(height, pc)?;
                // Rebuilding the row-major grid clones every retained matrix
                // cell plus every inserted column cell.
                let clone_bytes = match (
                    self.heap.get(h),
                    line_h.and_then(|line_h| self.heap.get(line_h)),
                ) {
                    (
                        Some(HeapObj::Matrix { items, .. }),
                        Some(HeapObj::Array {
                            items: line_items, ..
                        }),
                    ) => self
                        .projected_string_clone_bytes(items.iter().chain(line_items.iter()), pc)?,
                    (Some(HeapObj::Matrix { items, .. }), None) => {
                        self.projected_string_clone_bytes(items.iter(), pc)?
                    }
                    _ => 0,
                };
                if let Err(error) = self.reserve_string_bytes(clone_bytes, pc) {
                    self.heap.release(height);
                    return Err(error);
                }
                let new_col = if line_h.is_none() {
                    vec![Value::Na; height]
                } else {
                    match self.array_line(func, &args[2], elem_ty, pc) {
                        Ok(line) => line,
                        Err(error) => {
                            self.heap.release(height);
                            return Err(error);
                        }
                    }
                };
                let ci = idx as usize;
                if let Some(HeapObj::Matrix {
                    rows: r,
                    cols: c,
                    items,
                    ..
                }) = self.heap.get_mut(h)
                {
                    let mut out = Vec::with_capacity(height * new_cols);
                    for (ri, cell) in new_col.into_iter().enumerate() {
                        let base = ri * *c;
                        out.extend_from_slice(&items[base..base + ci]);
                        out.push(cell);
                        out.extend_from_slice(&items[base + ci..base + *c]);
                    }
                    *items = out;
                    *c = new_cols;
                    if *r == 0 {
                        *r = height;
                    }
                }
                Ok(Value::Na)
            }

            // ---- remove_row / remove_col ---------------------------------
            F::MatrixRemoveRow => {
                let idx = self.index_arg(func, &args[1], pc)?;
                if idx < 0 || idx as usize >= rows {
                    return Err(self.oob(func, idx, rows, "row", pc));
                }
                let ri = idx as usize;
                // Pre-allocate the zero-cell result object so an object/arena
                // limit failure leaves the receiver untouched. The removed
                // cells are then moved, not cloned, from matrix to array; the
                // run-wide live-element and string-allocation totals therefore
                // remain unchanged.
                let result = self
                    .heap
                    .alloc(HeapObj::Array {
                        elem_ty,
                        items: Vec::new(),
                    })
                    .map_err(|e| self.heap_limit_err(e, pc))?;
                let mut removed = Vec::with_capacity(cols);
                if let Some(HeapObj::Matrix { rows: r, items, .. }) = self.heap.get_mut(h) {
                    removed.extend(items.drain(ri * cols..(ri + 1) * cols));
                    *r -= 1;
                }
                if let Some(HeapObj::Array { items, .. }) = self.heap.get_mut(result) {
                    *items = removed;
                }
                Ok(Value::Ref(result))
            }
            F::MatrixRemoveCol => {
                let idx = self.index_arg(func, &args[1], pc)?;
                if idx < 0 || idx as usize >= cols {
                    return Err(self.oob(func, idx, cols, "column", pc));
                }
                let ci = idx as usize;
                // As above, allocate the result shell before changing the
                // receiver. Repartition the old row-major buffer by moving its
                // values, which preserves reference handles and avoids fake
                // string-clone charges for a transfer of ownership.
                let result = self
                    .heap
                    .alloc(HeapObj::Array {
                        elem_ty,
                        items: Vec::new(),
                    })
                    .map_err(|e| self.heap_limit_err(e, pc))?;
                let mut removed = Vec::with_capacity(rows);
                if let Some(HeapObj::Matrix { cols: c, items, .. }) = self.heap.get_mut(h) {
                    let new_cols = *c - 1;
                    let mut out = Vec::with_capacity(rows * new_cols);
                    for (index, value) in std::mem::take(items).into_iter().enumerate() {
                        if index % *c == ci {
                            removed.push(value);
                        } else {
                            out.push(value);
                        }
                    }
                    *items = out;
                    *c = new_cols;
                }
                if let Some(HeapObj::Array { items, .. }) = self.heap.get_mut(result) {
                    *items = removed;
                }
                Ok(Value::Ref(result))
            }

            // ---- swap_rows / swap_columns --------------------------------
            F::MatrixSwapRows => {
                let a = self.index_arg(func, &args[1], pc)?;
                let b = self.index_arg(func, &args[2], pc)?;
                if a < 0 || a as usize >= rows || b < 0 || b as usize >= rows {
                    return Err(self.oob(func, a.max(b), rows, "row", pc));
                }
                let (ai, bi) = (a as usize, b as usize);
                if let Some(HeapObj::Matrix { items, .. }) = self.heap.get_mut(h) {
                    for c in 0..cols {
                        items.swap(ai * cols + c, bi * cols + c);
                    }
                }
                Ok(Value::Na)
            }
            F::MatrixSwapColumns => {
                let a = self.index_arg(func, &args[1], pc)?;
                let b = self.index_arg(func, &args[2], pc)?;
                if a < 0 || a as usize >= cols || b < 0 || b as usize >= cols {
                    return Err(self.oob(func, a.max(b), cols, "column", pc));
                }
                let (ai, bi) = (a as usize, b as usize);
                if let Some(HeapObj::Matrix { items, .. }) = self.heap.get_mut(h) {
                    for r in 0..rows {
                        items.swap(r * cols + ai, r * cols + bi);
                    }
                }
                Ok(Value::Na)
            }

            // ---- reverse (both axes) -------------------------------------
            F::MatrixReverse => {
                if let Some(HeapObj::Matrix { items, .. }) = self.heap.get_mut(h) {
                    items.reverse();
                }
                Ok(Value::Na)
            }

            // ---- transpose -----------------------------------------------
            F::MatrixTranspose => {
                let count = rows.checked_mul(cols).ok_or_else(|| {
                    self.fail(
                        RuntimeErrorKind::CollectionLimitExceeded,
                        "matrix transpose size overflowed".to_string(),
                        pc,
                    )
                })?;
                self.check_capacity(count, pc)?;
                let clone_bytes = match self.heap.get(h) {
                    Some(HeapObj::Matrix { items, .. }) => {
                        self.projected_string_clone_bytes(items.iter(), pc)?
                    }
                    _ => 0,
                };
                let charged_before = self.string_bytes_allocated;
                self.reserve_string_bytes(clone_bytes, pc)?;
                let mut out = Vec::with_capacity(count);
                if let Some(HeapObj::Matrix { items, .. }) = self.heap.get(h) {
                    // An empty grid can have an enormous column count without
                    // allocating cells. Preserve its shape without visiting
                    // those empty columns, even in unoptimized builds.
                    let columns_to_copy = if count == 0 { 0 } else { cols };
                    for c in 0..columns_to_copy {
                        for r in 0..rows {
                            out.push(items[r * cols + c].clone());
                        }
                    }
                }
                let nh = self.alloc_after_string_charge(
                    HeapObj::Matrix {
                        elem_ty,
                        rows: cols,
                        cols: rows,
                        items: out,
                    },
                    charged_before,
                    pc,
                )?;
                Ok(Value::Ref(nh))
            }

            // ---- reshape (in place, same element count) ------------------
            F::MatrixReshape => {
                let nr = self.index_arg(func, &args[1], pc)?;
                let nc = self.index_arg(func, &args[2], pc)?;
                // `checked_mul` so absurd target dimensions cannot overflow
                // `usize` into a panic; an overflowing product can never equal
                // the (heap-capped) existing element count, so it falls into
                // the dimension-mismatch error path.
                if nr < 0 || nc < 0 || (nr as usize).checked_mul(nc as usize) != Some(rows * cols) {
                    return Err(self.dim_err(
                        func,
                        &format!(
                            "cannot reshape a {rows}x{cols} matrix to {nr}x{nc} (element counts differ)"
                        ),
                        pc,
                    ));
                }
                if let Some(HeapObj::Matrix {
                    rows: r, cols: c, ..
                }) = self.heap.get_mut(h)
                {
                    *r = nr as usize;
                    *c = nc as usize;
                }
                Ok(Value::Na)
            }

            // ---- submatrix (from_row, to_row, from_col, to_col) ----------
            F::MatrixSubmatrix => {
                let fr = self.index_arg(func, &args[1], pc)?;
                let tr = self.index_arg(func, &args[2], pc)?;
                let fc = self.index_arg(func, &args[3], pc)?;
                let tc = self.index_arg(func, &args[4], pc)?;
                if fr < 0
                    || tr < fr
                    || tr as usize > rows
                    || fc < 0
                    || tc < fc
                    || tc as usize > cols
                {
                    return Err(self.dim_err(
                        func,
                        &format!("range ({fr}..{tr}, {fc}..{tc}) out of bounds for {rows}x{cols}"),
                        pc,
                    ));
                }
                let (nr, nc) = ((tr - fr) as usize, (tc - fc) as usize);
                let count = nr.checked_mul(nc).ok_or_else(|| {
                    self.fail(
                        RuntimeErrorKind::CollectionLimitExceeded,
                        "submatrix size overflowed".to_string(),
                        pc,
                    )
                })?;
                self.check_capacity(count, pc)?;
                let clone_bytes = match self.heap.get(h) {
                    Some(HeapObj::Matrix { items, .. }) => self.projected_string_clone_bytes(
                        (fr as usize..tr as usize).flat_map(|r| {
                            (fc as usize..tc as usize).map(move |c| &items[r * cols + c])
                        }),
                        pc,
                    )?,
                    _ => 0,
                };
                let charged_before = self.string_bytes_allocated;
                self.reserve_string_bytes(clone_bytes, pc)?;
                let mut out = Vec::with_capacity(count);
                if let Some(HeapObj::Matrix { items, .. }) = self.heap.get(h) {
                    for r in fr as usize..tr as usize {
                        for c in fc as usize..tc as usize {
                            out.push(items[r * cols + c].clone());
                        }
                    }
                }
                let nh = self.alloc_after_string_charge(
                    HeapObj::Matrix {
                        elem_ty,
                        rows: nr,
                        cols: nc,
                        items: out,
                    },
                    charged_before,
                    pc,
                )?;
                Ok(Value::Ref(nh))
            }

            // ---- concat (append id2's rows to id1) -----------------------
            F::MatrixConcat => {
                let h2 = self.receiver_handle(func, &args[1], pc)?;
                let (_, r2, c2) = self.matrix_meta(func, h2, pc)?;
                // Only a true 0x0 receiver has no established width. A 0xN
                // matrix retains N even when the appended operand is empty.
                let adopts_width = rows == 0 && cols == 0;
                if cols != c2 && !adopts_width {
                    return Err(self.dim_err(
                        func,
                        &format!("column counts differ ({cols} vs {c2})"),
                        pc,
                    ));
                }
                // Zero-width matrices can grow without charging any cells.
                // Keep their row count representable by both the heap and
                // the public i64-valued shape queries before any mutation.
                let new_rows = rows
                    .checked_add(r2)
                    .filter(|rows| i64::try_from(*rows).is_ok())
                    .ok_or_else(|| {
                        self.fail(
                            RuntimeErrorKind::CollectionLimitExceeded,
                            "matrix concat row count overflows".to_string(),
                            pc,
                        )
                    })?;
                let add_count = r2.checked_mul(c2).ok_or_else(|| {
                    self.fail(
                        RuntimeErrorKind::CollectionLimitExceeded,
                        "matrix concat size overflowed".to_string(),
                        pc,
                    )
                })?;
                self.reserve_cells(add_count, pc)?;
                let clone_bytes = match self.heap.get(h2) {
                    Some(HeapObj::Matrix { items, .. }) => {
                        self.projected_string_clone_bytes(items.iter(), pc)?
                    }
                    _ => 0,
                };
                if let Err(error) = self.reserve_string_bytes(clone_bytes, pc) {
                    self.heap.release(add_count);
                    return Err(error);
                }
                let add = match self.heap.get(h2) {
                    Some(HeapObj::Matrix { items, .. }) => items.clone(),
                    _ => Vec::new(),
                };
                if let Some(HeapObj::Matrix {
                    rows: r,
                    cols: c,
                    items,
                    ..
                }) = self.heap.get_mut(h)
                {
                    if adopts_width {
                        *c = c2;
                    }
                    items.extend(add);
                    *r = new_rows;
                }
                Ok(args[0].clone())
            }

            // ---- elementwise sum / diff (matrix or scalar) ---------------
            F::MatrixSum | F::MatrixDiff => {
                let (_, _, a) = self.matrix_floats(h).unwrap_or((0, 0, Vec::new()));
                let sign = if matches!(func, F::MatrixDiff) {
                    -1.0
                } else {
                    1.0
                };
                let out: Vec<f64> = if let Value::Ref(h2) = &args[1] {
                    let (r2, c2, b) = self
                        .matrix_floats(*h2)
                        .ok_or_else(|| self.dim_err(func, "second operand is not a matrix", pc))?;
                    if r2 != rows || c2 != cols {
                        return Err(self.dim_err(
                            func,
                            &format!("shapes differ ({rows}x{cols} vs {r2}x{c2})"),
                            pc,
                        ));
                    }
                    a.iter().zip(b.iter()).map(|(x, y)| x + sign * y).collect()
                } else {
                    let s = value_as_f64(&args[1]).unwrap_or(f64::NAN);
                    a.iter().map(|x| x + sign * s).collect()
                };
                self.check_capacity(rows * cols, pc)?;
                self.alloc_float_matrix(rows, cols, out, pc)
            }

            // ---- mult (matrix product, scalar, or vector) ----------------
            F::MatrixMult => {
                let (_, _, a) = self.matrix_floats(h).unwrap_or((0, 0, Vec::new()));
                if let Value::Ref(h2) = &args[1] {
                    let (r2, c2, b) = self
                        .matrix_floats(*h2)
                        .ok_or_else(|| self.dim_err(func, "second operand is not a matrix", pc))?;
                    if cols != r2 {
                        return Err(self.dim_err(
                            func,
                            &format!("inner dimensions disagree ({rows}x{cols} * {r2}x{c2})"),
                            pc,
                        ));
                    }
                    // Reserve the result cells (heap-cap check) BEFORE allocating
                    // the output vector, so a product whose result exceeds the
                    // collection cap fails cleanly instead of OOM-allocating a
                    // huge grid (e.g. a 100000x1 times a 1x100000 -> 1e10 cells).
                    // `checked_mul` guards the count against `usize` overflow.
                    let out_cells = rows.checked_mul(c2).ok_or_else(|| {
                        self.dim_err(func, &format!("result {rows}x{c2} matrix is too large"), pc)
                    })?;
                    self.check_capacity(out_cells, pc)?;
                    let mut out = vec![0.0; out_cells];
                    for i in 0..rows {
                        for k in 0..cols {
                            let aik = a[i * cols + k];
                            for j in 0..c2 {
                                out[i * c2 + j] += aik * b[k * c2 + j];
                            }
                        }
                    }
                    self.alloc_float_matrix(rows, c2, out, pc)
                } else {
                    let s = value_as_f64(&args[1]).unwrap_or(f64::NAN);
                    let out: Vec<f64> = a.iter().map(|x| x * s).collect();
                    self.check_capacity(rows * cols, pc)?;
                    self.alloc_float_matrix(rows, cols, out, pc)
                }
            }

            // ---- kron (Kronecker product) --------------------------------
            F::MatrixKron => {
                let h2 = self.receiver_handle(func, &args[1], pc)?;
                let (r2, c2, b) = self
                    .matrix_floats(h2)
                    .ok_or_else(|| self.dim_err(func, "second operand is not a matrix", pc))?;
                let (_, _, a) = self.matrix_floats(h).unwrap_or((0, 0, Vec::new()));
                // The Kronecker result is (rows*r2) x (cols*c2); its dimensions
                // and cell count can overflow `usize` for large operands, so
                // compute them with `checked_mul` and reserve the cells (heap-cap
                // check) BEFORE allocating the output grid — a clean error, never
                // an overflow panic or a giant OOM allocation.
                let dims = rows
                    .checked_mul(r2)
                    .zip(cols.checked_mul(c2))
                    .and_then(|(nr, nc)| nr.checked_mul(nc).map(|cells| (nr, nc, cells)));
                let (nr, nc, out_cells) = match dims {
                    Some(d) => d,
                    None => {
                        return Err(self.dim_err(func, "Kronecker result matrix is too large", pc))
                    }
                };
                self.check_capacity(out_cells, pc)?;
                let mut out = vec![0.0; out_cells];
                for i in 0..rows {
                    for j in 0..cols {
                        let aij = a[i * cols + j];
                        for p in 0..r2 {
                            for q in 0..c2 {
                                out[(i * r2 + p) * nc + (j * c2 + q)] = aij * b[p * c2 + q];
                            }
                        }
                    }
                }
                self.alloc_float_matrix(nr, nc, out, pc)
            }

            // ---- pow (integer matrix power) ------------------------------
            F::MatrixPow => {
                if rows != cols {
                    return Err(self.dim_err(func, "matrix.pow requires a square matrix", pc));
                }
                let p = self.index_arg(func, &args[1], pc)?;
                if p < 0 {
                    return Err(self.dim_err(func, "matrix.pow requires a non-negative power", pc));
                }
                let (_, _, a) = self.matrix_floats(h).unwrap_or((0, 0, Vec::new()));
                let n = rows;
                // The `exponent * cells * n` charge in `collection_extra_cost`
                // (applied at dispatch) already aborted a pathologically large
                // exponent or grid with `InstructionLimitExceeded`, so this
                // loop is budget-bounded and cannot hang.
                let mut acc = identity_matrix(n);
                for _ in 0..p {
                    acc = matmul(&acc, &a, n, n, n);
                }
                self.check_capacity(n * n, pc)?;
                self.alloc_float_matrix(n, n, acc, pc)
            }

            // ---- det / inv / trace (LU with partial pivoting) ------------
            F::MatrixDet => {
                if rows != cols {
                    return Err(self.dim_err(func, "determinant requires a square matrix", pc));
                }
                let (_, _, a) = self.matrix_floats(h).unwrap_or((0, 0, Vec::new()));
                Ok(match lu_determinant(&a, rows) {
                    Some(d) => Value::from_f64(d),
                    None => Value::Na,
                })
            }
            F::MatrixInv => {
                if rows != cols {
                    return Err(self.dim_err(func, "inverse requires a square matrix", pc));
                }
                let (_, _, a) = self.matrix_floats(h).unwrap_or((0, 0, Vec::new()));
                match lu_inverse(&a, rows) {
                    Some(inv) => {
                        self.check_capacity(rows * cols, pc)?;
                        self.alloc_float_matrix(rows, cols, inv, pc)
                    }
                    None => Err(self.fail(
                        RuntimeErrorKind::MatrixSingular,
                        format!("`{}`: matrix is singular (non-invertible)", func.name()),
                        pc,
                    )),
                }
            }
            F::MatrixTrace => {
                let n = rows.min(cols);
                let (_, _, a) = self.matrix_floats(h).unwrap_or((0, 0, Vec::new()));
                let mut sum = 0.0;
                let mut any = false;
                for i in 0..n {
                    let v = a[i * cols + i];
                    if v.is_finite() {
                        sum += v;
                        any = true;
                    }
                }
                Ok(if any { Value::from_f64(sum) } else { Value::Na })
            }
            F::MatrixRank => {
                let (_, _, a) = self.matrix_floats(h).unwrap_or((0, 0, Vec::new()));
                Ok(match matrix_rank(&a, rows, cols) {
                    Some(r) => Value::Int(r as i64),
                    // A non-finite cell makes the rank undefined → na.
                    None => Value::Na,
                })
            }

            // ---- row / col line extraction (fresh array<T>) --------------
            F::MatrixRow => {
                let r = self.index_arg(func, &args[1], pc)?;
                if r < 0 || r as usize >= rows {
                    return Err(self.oob(func, r, rows, "row", pc));
                }
                let ri = r as usize;
                self.check_capacity(cols, pc)?;
                let clone_bytes = match self.heap.get(h) {
                    Some(HeapObj::Matrix { items, .. }) => self.projected_string_clone_bytes(
                        (0..cols).map(|c| &items[ri * cols + c]),
                        pc,
                    )?,
                    _ => 0,
                };
                let charged_before = self.string_bytes_allocated;
                self.reserve_string_bytes(clone_bytes, pc)?;
                let items: Vec<Value> = match self.heap.get(h) {
                    Some(HeapObj::Matrix { items, .. }) => {
                        (0..cols).map(|c| items[ri * cols + c].clone()).collect()
                    }
                    _ => Vec::new(),
                };
                let new = self.alloc_after_string_charge(
                    HeapObj::Array { elem_ty, items },
                    charged_before,
                    pc,
                )?;
                Ok(Value::Ref(new))
            }
            F::MatrixCol => {
                let c = self.index_arg(func, &args[1], pc)?;
                if c < 0 || c as usize >= cols {
                    return Err(self.oob(func, c, cols, "column", pc));
                }
                let ci = c as usize;
                self.check_capacity(rows, pc)?;
                let clone_bytes = match self.heap.get(h) {
                    Some(HeapObj::Matrix { items, .. }) => self.projected_string_clone_bytes(
                        (0..rows).map(|r| &items[r * cols + ci]),
                        pc,
                    )?,
                    _ => 0,
                };
                let charged_before = self.string_bytes_allocated;
                self.reserve_string_bytes(clone_bytes, pc)?;
                let items: Vec<Value> = match self.heap.get(h) {
                    Some(HeapObj::Matrix { items, .. }) => {
                        (0..rows).map(|r| items[r * cols + ci].clone()).collect()
                    }
                    _ => Vec::new(),
                };
                let new = self.alloc_after_string_charge(
                    HeapObj::Array { elem_ty, items },
                    charged_before,
                    pc,
                )?;
                Ok(Value::Ref(new))
            }

            // ---- eigen / pinv / sort -------------------------------------
            // Eigenvalues of a SYMMETRIC matrix via cyclic Jacobi → a fresh
            // array<float> (descending). Non-square / non-symmetric / non-finite
            // input is a structured error (general spectra are out of scope).
            F::MatrixEigenvalues => {
                if rows != cols {
                    return Err(self.dim_err(func, "eigenvalues require a square matrix", pc));
                }
                let (_, _, a) = self.matrix_floats(h).unwrap_or((0, 0, Vec::new()));
                if a.iter().any(|x| !x.is_finite()) {
                    return Err(self.dim_err(
                        func,
                        "eigenvalues require a matrix with no na cells",
                        pc,
                    ));
                }
                if !is_symmetric(&a, rows, 1e-9) {
                    return Err(self.dim_err(
                        func,
                        "eigenvalues are supported only for symmetric matrices (general/complex spectra are out of scope)",
                        pc,
                    ));
                }
                let (values, _) = jacobi_eigen(&a, rows);
                let items: Vec<Value> = values.into_iter().map(Value::from_f64).collect();
                self.alloc_float_array(items, pc)
            }
            // Eigenvectors of a SYMMETRIC matrix → a new matrix whose columns are
            // the unit eigenvectors (matched to the descending eigenvalue order).
            F::MatrixEigenvectors => {
                if rows != cols {
                    return Err(self.dim_err(func, "eigenvectors require a square matrix", pc));
                }
                let (_, _, a) = self.matrix_floats(h).unwrap_or((0, 0, Vec::new()));
                if a.iter().any(|x| !x.is_finite()) {
                    return Err(self.dim_err(
                        func,
                        "eigenvectors require a matrix with no na cells",
                        pc,
                    ));
                }
                if !is_symmetric(&a, rows, 1e-9) {
                    return Err(self.dim_err(
                        func,
                        "eigenvectors are supported only for symmetric matrices (general/complex spectra are out of scope)",
                        pc,
                    ));
                }
                let (_, vectors) = jacobi_eigen(&a, rows);
                self.check_capacity(rows * cols, pc)?;
                self.alloc_float_matrix(rows, cols, vectors, pc)
            }
            // Moore-Penrose pseudo-inverse of a full-rank matrix (normal
            // equations). Rank-deficient input (singular Gram matrix) is a
            // structured error (the SVD-based general pinv is out of scope).
            F::MatrixPinv => {
                let (_, _, a) = self.matrix_floats(h).unwrap_or((0, 0, Vec::new()));
                match pinv_full_rank(&a, rows, cols) {
                    Some(p) => {
                        self.check_capacity(rows * cols, pc)?;
                        // A^+ has the transposed shape: cols × rows.
                        self.alloc_float_matrix(cols, rows, p, pc)
                    }
                    None => Err(self.fail(
                        RuntimeErrorKind::MatrixSingular,
                        format!(
                            "`{}`: matrix is rank-deficient (the SVD-based general pseudo-inverse is out of scope)",
                            func.name()
                        ),
                        pc,
                    )),
                }
            }
            // Sort the receiver's rows by a column under an order constant
            // (0 = ascending, else descending). Stable and in-place.
            // Out-of-range column → structured error.
            F::MatrixSort => {
                let col = self.index_arg(func, &args[1], pc)?;
                if col < 0 || col as usize >= cols {
                    return Err(self.oob(func, col, cols, "column", pc));
                }
                let ci = col as usize;
                let descending = matches!(&args[2], Value::Int(i) if *i != 0);
                let (matrix_type, a) = match self.heap.get(h) {
                    Some(HeapObj::Matrix { elem_ty, items, .. }) => (*elem_ty, items.clone()),
                    _ => (ElemType::Float, Vec::new()),
                };
                let keys = if let ElemType::Object(expected_type) = matrix_type {
                    let sort_field = match args.get(3) {
                        Some(Value::Int(index)) if *index >= 0 => *index as usize,
                        _ => 0,
                    };
                    (0..rows)
                        .map(|row| {
                            let object_handle = match &a[row * cols + ci] {
                                Value::Ref(handle) => *handle,
                                Value::Na | Value::DisplacedTable(_) => {
                                    return Err(self.fail(
                                        RuntimeErrorKind::NaFieldAccess,
                                        "`matrix.sort` cannot read a sort field from an na object".to_string(),
                                        pc,
                                    ));
                                }
                                _ => {
                                    return Err(self.fail(
                                        RuntimeErrorKind::TypeMismatch,
                                        "`matrix.sort` expected a user-defined object in the sort column".to_string(),
                                        pc,
                                    ));
                                }
                            };
                            match self.heap.get(object_handle) {
                                Some(HeapObj::Object { type_id, fields, .. })
                                    if *type_id == expected_type => fields
                                        .get(sort_field)
                                        .cloned()
                                        .ok_or_else(|| {
                                            self.fail(
                                                RuntimeErrorKind::TypeMismatch,
                                                "`matrix.sort` sort field is outside the object layout".to_string(),
                                                pc,
                                            )
                                        }),
                                _ => Err(self.fail(
                                    RuntimeErrorKind::TypeMismatch,
                                    "`matrix.sort` object type does not match the matrix element type".to_string(),
                                    pc,
                                )),
                            }
                        })
                        .collect::<Result<Vec<_>, _>>()?
                } else {
                    (0..rows).map(|row| a[row * cols + ci].clone()).collect()
                };
                let mut order: Vec<usize> = (0..rows).collect();
                order.sort_by(|&r1, &r2| {
                    let k1 = &keys[r1];
                    let k2 = &keys[r2];
                    let missing = |key: &Value| {
                        key.is_na() || matches!(key, Value::String(value) if value.is_empty())
                    };
                    let na_order = match (missing(k1), missing(k2)) {
                        (true, true) => Some(std::cmp::Ordering::Equal),
                        (true, false) => Some(if descending {
                            std::cmp::Ordering::Less
                        } else {
                            std::cmp::Ordering::Greater
                        }),
                        (false, true) => Some(if descending {
                            std::cmp::Ordering::Greater
                        } else {
                            std::cmp::Ordering::Less
                        }),
                        (false, false) => None,
                    };
                    if let Some(cmp) = na_order {
                        return cmp;
                    }
                    let cmp = match (k1, k2) {
                        (Value::Int(a), Value::Int(b)) => a.cmp(b),
                        (Value::Float(a), Value::Float(b)) => {
                            a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
                        }
                        (Value::String(a), Value::String(b)) => a.cmp(b),
                        _ => std::cmp::Ordering::Equal,
                    };
                    if descending {
                        cmp.reverse()
                    } else {
                        cmp
                    }
                });
                let mut out = Vec::with_capacity(rows * cols);
                for &src_row in &order {
                    out.extend_from_slice(&a[src_row * cols..src_row * cols + cols]);
                }
                if let Some(HeapObj::Matrix { items, .. }) = self.heap.get_mut(h) {
                    *items = out;
                }
                Ok(Value::Na)
            }

            // ---- aggregates ----------------------------------------------
            F::MatrixAvg | F::MatrixMin | F::MatrixMax | F::MatrixMedian | F::MatrixMode => {
                let nums = self.matrix_numeric_cells(h);
                Ok(self.matrix_aggregate(func, &nums))
            }

            // ---- predicates ----------------------------------------------
            F::MatrixIsSquare => Ok(Value::Bool(rows == cols)),
            F::MatrixIsIdentity
            | F::MatrixIsDiagonal
            | F::MatrixIsSymmetric
            | F::MatrixIsAntisymmetric
            | F::MatrixIsTriangular
            | F::MatrixIsStochastic
            | F::MatrixIsBinary
            | F::MatrixIsZero
            | F::MatrixIsAntidiagonal => {
                let (_, _, a) = self.matrix_floats(h).unwrap_or((0, 0, Vec::new()));
                Ok(Value::Bool(matrix_predicate(func, &a, rows, cols)))
            }

            _ => Err(self.fail(
                RuntimeErrorKind::InvalidProgram,
                format!("`{}` is not a matrix operation", func.name()),
                pc,
            )),
        }
    }

    /// Charge `count` cells against the heap budget before growing an
    /// EXISTING object in place (`Heap::get_mut` + splice/extend, no
    /// `Heap::alloc` follows) — this is the only charge for that growth.
    fn reserve_cells(&mut self, count: usize, pc: usize) -> Result<(), RuntimeError> {
        self.heap
            .reserve(count)
            .map_err(|e| self.heap_limit_err(e, pc))
    }

    /// Check that `count` additional cells would fit the heap budget, without
    /// charging them, before building a fresh output buffer for an operation
    /// whose result is handed to `Heap::alloc` (or `alloc_float_matrix`, which
    /// itself calls `Heap::alloc`) right after. `Heap::alloc` charges the
    /// real count itself, so charging it here too would double-count every
    /// cell of the new object — and since nothing ever un-charges a
    /// pre-charge that isn't followed by a matching release, that budget
    /// would leak for the rest of the run. This still fails fast (before the
    /// potentially large buffer is built) without leaving any lasting charge.
    fn check_capacity(&mut self, count: usize, pc: usize) -> Result<(), RuntimeError> {
        self.heap
            .reserve(count)
            .map_err(|e| self.heap_limit_err(e, pc))?;
        self.heap.release(count);
        Ok(())
    }

    /// Bounds-check a matrix cell coordinate against `rows × cols`.
    fn check_cell(
        &self,
        func: CollectionFunc,
        r: i64,
        c: i64,
        rows: usize,
        cols: usize,
        pc: usize,
    ) -> Result<(usize, usize), RuntimeError> {
        if r < 0 || r as usize >= rows || c < 0 || c as usize >= cols {
            return Err(self.fail(
                RuntimeErrorKind::CollectionIndexOutOfBounds,
                format!(
                    "`{}` cell ({r}, {c}) out of bounds for {rows}x{cols} matrix",
                    func.name()
                ),
                pc,
            ));
        }
        Ok((r as usize, c as usize))
    }

    /// A structured out-of-bounds error for a row/column index.
    fn oob(
        &self,
        func: CollectionFunc,
        idx: i64,
        len: usize,
        what: &str,
        pc: usize,
    ) -> RuntimeError {
        self.fail(
            RuntimeErrorKind::CollectionIndexOutOfBounds,
            format!(
                "`{}` {what} index {idx} out of bounds (count {len})",
                func.name()
            ),
            pc,
        )
    }

    /// Resolve an `array<T>` argument to its element vector, coercing each cell
    /// to `elem_ty`. Used by `matrix.add_row`/`add_col`.
    fn array_line(
        &self,
        func: CollectionFunc,
        value: &Value,
        elem_ty: ElemType,
        pc: usize,
    ) -> Result<Vec<Value>, RuntimeError> {
        let h = self.receiver_handle(func, value, pc)?;
        match self.heap.get(h) {
            Some(HeapObj::Array { items, .. }) => {
                let mut out = Vec::with_capacity(items.len());
                for v in items.clone() {
                    out.push(self.coerce_element(func, elem_ty, v, pc)?);
                }
                Ok(out)
            }
            _ => Err(self.fail(
                RuntimeErrorKind::TypeMismatch,
                format!("`{}` expects an array of values to insert", func.name()),
                pc,
            )),
        }
    }

    /// The non-`na` numeric cells of matrix `h` as `f64`s (ints widen). Used by
    /// the matrix aggregates, which skip `na` cells.
    fn matrix_numeric_cells(&self, h: HandleId) -> Vec<f64> {
        match self.heap.get(h) {
            Some(HeapObj::Matrix { items, .. }) => items.iter().filter_map(value_as_f64).collect(),
            _ => Vec::new(),
        }
    }

    /// The matrix aggregate reductions (avg/min/max/median/mode), reusing the
    /// array aggregate policy: skip `na`, empty → `na`.
    fn matrix_aggregate(&self, func: CollectionFunc, nums: &[f64]) -> Value {
        use CollectionFunc as F;
        if nums.is_empty() {
            return Value::Na;
        }
        match func {
            F::MatrixAvg => Value::from_f64(mean(nums)),
            F::MatrixMin => Value::from_f64(nums.iter().copied().fold(f64::INFINITY, f64::min)),
            F::MatrixMax => Value::from_f64(nums.iter().copied().fold(f64::NEG_INFINITY, f64::max)),
            F::MatrixMedian => {
                let mut sorted = nums.to_vec();
                Value::from_f64(median(&mut sorted))
            }
            F::MatrixMode => Value::from_f64(mode(nums)),
            _ => Value::Na,
        }
    }

    /// The receiver-taking array operations (everything except the
    /// constructors). Split out to keep `call_collection` readable.
    fn call_array_op(
        &mut self,
        func: CollectionFunc,
        args: &[Value],
        pc: usize,
    ) -> Result<Value, RuntimeError> {
        use CollectionFunc as F;
        let h = self.receiver_handle(func, &args[0], pc)?;
        let (elem_ty, len) = self.array_meta(func, h, pc)?;

        match func {
            F::InternalDistanceScanEligible | F::InternalDistanceScan => {
                unreachable!("internal scan has its own dispatcher")
            }
            F::ArraySize => Ok(Value::Int(len as i64)),

            F::ArrayPush => {
                vm_heap_ops::array_push(
                    &mut self.heap,
                    &mut self.string_bytes_allocated,
                    self.options.max_string_bytes_per_run,
                    &args[0],
                    &args[1],
                )
                .map_err(|error| self.heap_op_error(func, error, args, 1, pc))?;
                Ok(Value::Na)
            }
            F::ArrayUnshift => {
                vm_heap_ops::array_unshift(
                    &mut self.heap,
                    &mut self.string_bytes_allocated,
                    self.options.max_string_bytes_per_run,
                    &args[0],
                    &args[1],
                )
                .map_err(|error| self.heap_op_error(func, error, args, 1, pc))?;
                Ok(Value::Na)
            }
            F::ArrayPop => {
                if len == 0 {
                    return Err(self.empty_error(func, pc));
                }
                let out = if let Some(HeapObj::Array { items, .. }) = self.heap.get_mut(h) {
                    items.pop().unwrap_or(Value::Na)
                } else {
                    Value::Na
                };
                self.heap.release(1);
                Ok(out)
            }
            F::ArrayShift => {
                if len == 0 {
                    return Err(self.empty_error(func, pc));
                }
                let out = if let Some(HeapObj::Array { items, .. }) = self.heap.get_mut(h) {
                    items.remove(0)
                } else {
                    Value::Na
                };
                self.heap.release(1);
                Ok(out)
            }
            F::ArrayGet => {
                // A na index yields na, even on a nonempty array.
                if args[1].is_na() {
                    return Ok(Value::Na);
                }
                let idx = self.index_arg(func, &args[1], pc)?;
                let i = self.resolve_access_index(func, idx, len, pc)?;
                let clone_bytes = match self.heap.get(h) {
                    Some(HeapObj::Array { items, .. }) => {
                        self.projected_repeated_string_clone_bytes(&items[i], 1, pc)?
                    }
                    _ => 0,
                };
                self.reserve_string_bytes(clone_bytes, pc)?;
                Ok(match self.heap.get(h) {
                    Some(HeapObj::Array { items, .. }) => items[i].clone(),
                    _ => Value::Na,
                })
            }
            F::ArraySet => {
                // A na index makes the write a no-op. Do not
                // coerce or charge the value when no cell is selected.
                if args[1].is_na() {
                    return Ok(Value::Na);
                }
                vm_heap_ops::array_set(
                    &mut self.heap,
                    &mut self.string_bytes_allocated,
                    self.options.max_string_bytes_per_run,
                    &args[0],
                    &args[1],
                    &args[2],
                )
                .map_err(|error| self.heap_op_error(func, error, args, 2, pc))?;
                Ok(Value::Na)
            }
            F::ArrayInsert => {
                vm_heap_ops::array_insert(
                    &mut self.heap,
                    &mut self.string_bytes_allocated,
                    self.options.max_string_bytes_per_run,
                    &args[0],
                    &args[1],
                    &args[2],
                )
                .map_err(|error| self.heap_op_error(func, error, args, 2, pc))?;
                Ok(Value::Na)
            }
            F::ArrayRemove => {
                let idx = self.index_arg(func, &args[1], pc)?;
                let i = self.resolve_access_index(func, idx, len, pc)?;
                let out = if let Some(HeapObj::Array { items, .. }) = self.heap.get_mut(h) {
                    items.remove(i)
                } else {
                    Value::Na
                };
                self.heap.release(1);
                Ok(out)
            }
            F::ArrayClear => {
                if let Some(HeapObj::Array { items, .. }) = self.heap.get_mut(h) {
                    items.clear();
                }
                self.heap.release(len);
                Ok(Value::Na)
            }
            F::ArrayFirst => {
                if len == 0 {
                    return Err(self.empty_error(func, pc));
                }
                let clone_bytes = match self.heap.get(h) {
                    Some(HeapObj::Array { items, .. }) => {
                        self.projected_repeated_string_clone_bytes(&items[0], 1, pc)?
                    }
                    _ => 0,
                };
                self.reserve_string_bytes(clone_bytes, pc)?;
                Ok(match self.heap.get(h) {
                    Some(HeapObj::Array { items, .. }) => items[0].clone(),
                    _ => Value::Na,
                })
            }
            F::ArrayLast => {
                if len == 0 {
                    return Err(self.empty_error(func, pc));
                }
                let clone_bytes = match self.heap.get(h) {
                    Some(HeapObj::Array { items, .. }) => {
                        self.projected_repeated_string_clone_bytes(&items[len - 1], 1, pc)?
                    }
                    _ => 0,
                };
                self.reserve_string_bytes(clone_bytes, pc)?;
                Ok(match self.heap.get(h) {
                    Some(HeapObj::Array { items, .. }) => items[len - 1].clone(),
                    _ => Value::Na,
                })
            }
            F::ArrayReverse => {
                if let Some(HeapObj::Array { items, .. }) = self.heap.get_mut(h) {
                    items.reverse();
                }
                Ok(Value::Na)
            }
            F::ArrayIncludes => {
                let clone_bytes = self.projected_repeated_string_clone_bytes(&args[1], 1, pc)?;
                let target = self.coerce_element(func, elem_ty, args[1].clone(), pc)?;
                self.reserve_string_bytes(clone_bytes, pc)?;
                let found = match self.heap.get(h) {
                    Some(HeapObj::Array { items, .. }) => {
                        items.iter().any(|v| values_equal(v, &target))
                    }
                    _ => false,
                };
                Ok(Value::Bool(found))
            }
            F::ArrayIndexOf => {
                let clone_bytes = self.projected_repeated_string_clone_bytes(&args[1], 1, pc)?;
                let target = self.coerce_element(func, elem_ty, args[1].clone(), pc)?;
                self.reserve_string_bytes(clone_bytes, pc)?;
                let idx = match self.heap.get(h) {
                    Some(HeapObj::Array { items, .. }) => items
                        .iter()
                        .position(|v| values_equal(v, &target))
                        .map_or(-1, |i| i as i64),
                    _ => -1,
                };
                Ok(Value::Int(idx))
            }
            // `array.every(id)` / `array.some(id)`: no callback. An element is
            // "true" when it is a `true` bool or a non-zero, non-`na` number.
            // Empty arrays follow the vacuous-truth convention (`every` → true,
            // `some` → false); the na/empty rules are defined in docs/ir.md.
            F::ArrayEvery => {
                let all = match self.heap.get(h) {
                    Some(HeapObj::Array { items, .. }) => items.iter().all(element_is_true),
                    _ => true,
                };
                Ok(Value::Bool(all))
            }
            F::ArraySome => {
                let any = match self.heap.get(h) {
                    Some(HeapObj::Array { items, .. }) => items.iter().any(element_is_true),
                    _ => false,
                };
                Ok(Value::Bool(any))
            }
            F::ArrayLastIndexOf => {
                let clone_bytes = self.projected_repeated_string_clone_bytes(&args[1], 1, pc)?;
                let target = self.coerce_element(func, elem_ty, args[1].clone(), pc)?;
                self.reserve_string_bytes(clone_bytes, pc)?;
                let idx = match self.heap.get(h) {
                    Some(HeapObj::Array { items, .. }) => items
                        .iter()
                        .rposition(|v| values_equal(v, &target))
                        .map_or(-1, |i| i as i64),
                    _ => -1,
                };
                Ok(Value::Int(idx))
            }
            F::ArrayCopy => {
                self.check_capacity(len, pc)?;
                let clone_bytes = match self.heap.get(h) {
                    Some(HeapObj::Array { items, .. }) => {
                        self.projected_string_clone_bytes(items.iter(), pc)?
                    }
                    _ => 0,
                };
                let charged_before = self.string_bytes_allocated;
                self.reserve_string_bytes(clone_bytes, pc)?;
                let items = match self.heap.get(h) {
                    Some(HeapObj::Array { items, .. }) => items.clone(),
                    _ => Vec::new(),
                };
                // `alloc` charges the copied elements against the budget.
                let new_h = self.alloc_after_string_charge(
                    HeapObj::Array { elem_ty, items },
                    charged_before,
                    pc,
                )?;
                Ok(Value::Ref(new_h))
            }
            F::ArraySlice => {
                let from = self.index_arg(func, &args[1], pc)?;
                let to = self.index_arg(func, &args[2], pc)?;
                if from < 0 || to < 0 || from > to || to as usize > len {
                    return Err(self.slice_error(func, from, to, len, pc));
                }
                let (from, to) = (from as usize, to as usize);
                let count = to - from;
                self.check_capacity(count, pc)?;
                let clone_bytes = match self.heap.get(h) {
                    Some(HeapObj::Array { items, .. }) => {
                        self.projected_string_clone_bytes(items[from..to].iter(), pc)?
                    }
                    _ => 0,
                };
                let charged_before = self.string_bytes_allocated;
                self.reserve_string_bytes(clone_bytes, pc)?;
                let items: Vec<Value> = match self.heap.get(h) {
                    Some(HeapObj::Array { items, .. }) => items[from..to].to_vec(),
                    _ => Vec::new(),
                };
                // `alloc` charges the sliced elements against the budget.
                let new_h = self.alloc_after_string_charge(
                    HeapObj::Array { elem_ty, items },
                    charged_before,
                    pc,
                )?;
                Ok(Value::Ref(new_h))
            }
            F::ArrayConcat => {
                let src = self.receiver_handle(func, &args[1], pc)?;
                let (src_len, clone_bytes) = match self.heap.get(src) {
                    Some(HeapObj::Array { items, .. }) => (
                        items.len(),
                        self.projected_string_clone_bytes(items.iter(), pc)?,
                    ),
                    _ => {
                        return Err(self.fail(
                            RuntimeErrorKind::InvalidProgram,
                            format!("`{}` second argument is not an array", func.name()),
                            pc,
                        ));
                    }
                };
                self.reserve_cells(src_len, pc)?;
                if let Err(error) = self.reserve_string_bytes(clone_bytes, pc) {
                    self.heap.release(src_len);
                    return Err(error);
                }
                let src_items = match self.heap.get(src) {
                    Some(HeapObj::Array { items, .. }) => items.clone(),
                    _ => Vec::new(),
                };
                if let Some(HeapObj::Array { items, .. }) = self.heap.get_mut(h) {
                    items.extend(src_items);
                }
                // concat returns the (mutated) receiver handle.
                Ok(Value::Ref(h))
            }
            F::ArrayFill => {
                // Resolve omitted bounds from the receiver already evaluated.
                let from = if args.len() >= 3 {
                    self.index_arg(func, &args[2], pc)?
                } else {
                    0
                };
                // index_to defaults to `na`, which fills to the end.
                let to = match args.get(3) {
                    Some(value) if !value.is_na() => self.index_arg(func, value, pc)?,
                    _ => len as i64,
                };
                if from < 0 || to < 0 || from > to || to as usize > len {
                    return Err(self.slice_error(func, from, to, len, pc));
                }
                let (from, to) = (from as usize, to as usize);
                // One clone creates the temporary `v`; the loop clones it
                // once per assigned cell. Account for both allocation paths.
                let copies = to.saturating_sub(from).saturating_add(1);
                let clone_bytes =
                    self.projected_repeated_string_clone_bytes(&args[1], copies, pc)?;
                let v = self.coerce_element(func, elem_ty, args[1].clone(), pc)?;
                self.reserve_string_bytes(clone_bytes, pc)?;
                if let Some(HeapObj::Array { items, .. }) = self.heap.get_mut(h) {
                    for slot in items.iter_mut().take(to).skip(from) {
                        *slot = v.clone();
                    }
                }
                Ok(Value::Na)
            }
            F::ArrayJoin => {
                let sep = if args.len() == 2 {
                    match &args[1] {
                        Value::String(s) => s.as_str(),
                        _ => "",
                    }
                } else {
                    ""
                };
                // `render_join_element` allocates one temporary string per
                // cell and `join` then allocates the result. Reserve an upper
                // bound for both before either allocation. Direct string
                // cells use their exact byte length; floats are counted without
                // allocation. Remaining scalar variants fit in 64 bytes.
                let (item_count, rendered_bytes) = match self.heap.get(h) {
                    Some(HeapObj::Array { items, .. }) => {
                        let rendered = items.iter().try_fold(0usize, |bytes, value| {
                            let len = match value {
                                Value::String(s) => s.len(),
                                Value::Float(f) => join_float_byte_len(*f)?,
                                _ => 64,
                            };
                            bytes.checked_add(len)
                        });
                        (items.len(), rendered)
                    }
                    _ => (0, Some(0)),
                };
                let rendered_bytes = rendered_bytes.ok_or_else(|| {
                    self.fail(
                        RuntimeErrorKind::StringLimitExceeded,
                        "`array.join` output size overflowed".to_string(),
                        pc,
                    )
                })?;
                let joined_bytes = sep
                    .len()
                    .checked_mul(item_count.saturating_sub(1))
                    .and_then(|separators| rendered_bytes.checked_add(separators))
                    .ok_or_else(|| {
                        self.fail(
                            RuntimeErrorKind::StringLimitExceeded,
                            "`array.join` output size overflowed".to_string(),
                            pc,
                        )
                    })?;
                let allocation_bytes =
                    rendered_bytes.checked_add(joined_bytes).ok_or_else(|| {
                        self.fail(
                            RuntimeErrorKind::StringLimitExceeded,
                            "`array.join` allocation size overflowed".to_string(),
                            pc,
                        )
                    })?;
                self.reserve_string_bytes(allocation_bytes, pc)?;
                let joined = match self.heap.get(h) {
                    Some(HeapObj::Array { items, .. }) => items
                        .iter()
                        .map(render_join_element)
                        .collect::<Vec<_>>()
                        .join(sep),
                    _ => String::new(),
                };
                Ok(Value::String(joined))
            }

            // ---- numeric aggregates --------------------------------------
            // Each skips `na` elements; an empty (or all-`na`) array yields
            // `na`, like every sibling aggregate.
            F::ArraySum => {
                let xs = self.numeric_elements(h);
                Ok(if xs.is_empty() {
                    Value::Na
                } else {
                    Value::from_f64(xs.iter().sum())
                })
            }
            F::ArrayAvg => {
                let xs = self.numeric_elements(h);
                Ok(Value::from_f64(mean(&xs)))
            }
            // args: (id, nth). `nth = 0` is the extreme itself; a larger `nth`
            // walks in from that end. An `nth` past the last element is `na`.
            F::ArrayMin | F::ArrayMax => {
                let mut xs = self.numeric_elements(h);
                let nth = value_as_f64(&args[1]).unwrap_or(0.0) as i64;
                if nth < 0 {
                    return Ok(Value::Na);
                }
                if nth == 0 {
                    return Ok(Value::from_f64(if func == F::ArrayMin {
                        xs.iter().copied().fold(f64::INFINITY, f64::min)
                    } else {
                        xs.iter().copied().fold(f64::NEG_INFINITY, f64::max)
                    }));
                }
                xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                if func == F::ArrayMax {
                    xs.reverse();
                }
                Ok(xs
                    .get(nth as usize)
                    .copied()
                    .map_or(Value::Na, Value::from_f64))
            }
            F::ArrayRange => {
                let xs = self.numeric_elements(h);
                if xs.is_empty() {
                    Ok(Value::Na)
                } else {
                    let lo = xs.iter().copied().fold(f64::INFINITY, f64::min);
                    let hi = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                    Ok(Value::from_f64(hi - lo))
                }
            }
            F::ArrayMedian => {
                let mut xs = self.numeric_elements(h);
                Ok(Value::from_f64(median(&mut xs)))
            }
            F::ArrayMode => {
                let xs = self.numeric_elements(h);
                Ok(Value::from_f64(mode(&xs)))
            }
            // args: (id, biased). The checker materializes `biased` as a bool
            // (default true); a stray `na` is treated as the default.
            F::ArrayVariance | F::ArrayStdev => {
                let xs = self.numeric_elements(h);
                let biased = !matches!(&args[1], Value::Bool(false));
                let v = variance_with(&xs, biased);
                Ok(Value::from_f64(if func == F::ArrayStdev {
                    v.sqrt()
                } else {
                    v
                }))
            }

            // ---- covariance / percentile / rank / binary search ----------
            // args: (id1, id2, biased). Pairs elements by index, skipping any
            // pair with an `na` on either side.
            F::ArrayCovariance => {
                let xs = self.optional_elements(h);
                let other = self.receiver_handle(func, &args[1], pc)?;
                let ys = self.optional_elements(other);
                // The checker materializes `biased` as a bool (default true); a
                // stray `na` is treated as the default.
                let biased = !matches!(&args[2], Value::Bool(false));
                Ok(Value::from_f64(covariance(&xs, &ys, biased)))
            }
            // args: (id, index). Percentile rank of the element at `index`.
            F::ArrayPercentRank => {
                let idx = self.index_arg(func, &args[1], pc)?;
                let i = self.check_access_index(func, idx, len, pc)?;
                let out = match self.heap.get(h) {
                    Some(HeapObj::Array { items, .. }) => percentrank(items, i),
                    _ => f64::NAN,
                };
                Ok(Value::from_f64(out))
            }
            // args: (id, percentage).
            F::ArrayPercentileNearestRank => {
                let pct = value_as_f64(&args[1]).unwrap_or(f64::NAN);
                let xs = self.numeric_elements(h);
                Ok(Value::from_f64(percentile_nearest_rank(xs, pct)))
            }
            F::ArrayPercentileLinearInterpolation => {
                let pct = value_as_f64(&args[1]).unwrap_or(f64::NAN);
                let xs = self.numeric_elements(h);
                Ok(Value::from_f64(percentile_linear_interpolation(xs, pct)))
            }
            // args: (id, val). The array must be sorted ascending. An `na` search value never matches.
            F::ArrayBinarySearch | F::ArrayBinarySearchLeftmost | F::ArrayBinarySearchRightmost => {
                let mode = match func {
                    F::ArrayBinarySearch => BinarySearchMode::Exact,
                    F::ArrayBinarySearchLeftmost => BinarySearchMode::Leftmost,
                    _ => BinarySearchMode::Rightmost,
                };
                let target = match value_as_f64(&args[1]) {
                    Some(v) => v,
                    // A non-numeric / `na` target: nothing can match it.
                    None => return Ok(Value::Int(-1)),
                };
                let idx = match self.heap.get(h) {
                    Some(HeapObj::Array { items, .. }) => binary_search(items, target, mode),
                    _ => -1,
                };
                Ok(Value::Int(idx))
            }

            // ---- numeric transforms --------------------------------------
            F::ArrayAbs => {
                let items: Vec<Value> = match self.heap.get(h) {
                    Some(HeapObj::Array { items, .. }) => items
                        .iter()
                        .map(|v| match value_as_f64(v) {
                            Some(x) => Value::from_f64(x.abs()),
                            None => Value::Na,
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                self.alloc_float_array(items, pc)
            }
            F::ArrayStandardize => {
                let xs = self.numeric_elements(h);
                let m = mean(&xs);
                let sd = variance(&xs).sqrt();
                let items: Vec<Value> = match self.heap.get(h) {
                    Some(HeapObj::Array { items, .. }) => items
                        .iter()
                        .map(|v| match value_as_f64(v) {
                            Some(x) if sd.is_finite() && sd != 0.0 => Value::from_f64((x - m) / sd),
                            _ => Value::Na,
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                self.alloc_float_array(items, pc)
            }

            // ---- ordering ------------------------------------------------
            // args = [id, order]; order 0 = ascending, else descending.
            F::ArraySort => {
                let descending = matches!(&args[1], Value::Int(i) if *i != 0);
                if let Some(HeapObj::Array { items, .. }) = self.heap.get_mut(h) {
                    sort_values(items, descending);
                }
                Ok(Value::Na)
            }
            F::ArraySortIndices => {
                let descending = matches!(&args[1], Value::Int(i) if *i != 0);
                let indices = match self.heap.get(h) {
                    Some(HeapObj::Array { items, .. }) => sort_indices(items, descending),
                    _ => Vec::new(),
                };
                let items: Vec<Value> = indices.into_iter().map(|i| Value::Int(i as i64)).collect();
                let h = self
                    .heap
                    .alloc(HeapObj::Array {
                        elem_ty: ElemType::Int,
                        items,
                    })
                    .map_err(|e| self.heap_limit_err(e, pc))?;
                Ok(Value::Ref(h))
            }

            // Constructors are handled in `call_collection`.
            F::ArrayNewFloat
            | F::ArrayNewInt
            | F::ArrayNewBool
            | F::ArrayNewString
            | F::ArrayNewColor
            | F::ArrayNewObject
            | F::ArrayFrom
            | F::StrSplit => unreachable!("constructors handled in call_collection"),

            // Map functions are routed to `call_map_op` in `call_collection`.
            F::MapNew
            | F::MapPut
            | F::MapGet
            | F::MapContains
            | F::MapRemove
            | F::MapSize
            | F::MapClear
            | F::MapKeys
            | F::MapValues
            | F::MapCopy
            | F::MapPutAll => unreachable!("map functions handled in call_map_op"),

            // Matrix functions are routed to `call_matrix_op` (and MatrixNew is
            // built in `call_collection`).
            F::MatrixNew
            | F::MatrixGet
            | F::MatrixSet
            | F::MatrixRows
            | F::MatrixColumns
            | F::MatrixElementsCount
            | F::MatrixFill
            | F::MatrixCopy
            | F::MatrixAddRow
            | F::MatrixAddCol
            | F::MatrixRemoveRow
            | F::MatrixRemoveCol
            | F::MatrixSwapRows
            | F::MatrixSwapColumns
            | F::MatrixReverse
            | F::MatrixTranspose
            | F::MatrixReshape
            | F::MatrixSubmatrix
            | F::MatrixConcat
            | F::MatrixSum
            | F::MatrixDiff
            | F::MatrixMult
            | F::MatrixDet
            | F::MatrixInv
            | F::MatrixTrace
            | F::MatrixPow
            | F::MatrixKron
            | F::MatrixAvg
            | F::MatrixMin
            | F::MatrixMax
            | F::MatrixMedian
            | F::MatrixMode
            | F::MatrixIsSquare
            | F::MatrixIsIdentity
            | F::MatrixIsDiagonal
            | F::MatrixIsSymmetric
            | F::MatrixIsAntisymmetric
            | F::MatrixIsTriangular
            | F::MatrixIsStochastic
            | F::MatrixIsBinary
            | F::MatrixIsZero
            | F::MatrixIsAntidiagonal
            | F::MatrixRank
            | F::MatrixRow
            | F::MatrixCol
            | F::MatrixEigenvalues
            | F::MatrixEigenvectors
            | F::MatrixPinv
            | F::MatrixSort => unreachable!("matrix functions handled in call_matrix_op"),
        }
    }

    /// The non-`na` numeric elements of array `h` as `f64`s (int elements
    /// widen). Used by the aggregate functions, which skip `na`.
    fn numeric_elements(&self, h: HandleId) -> Vec<f64> {
        match self.heap.get(h) {
            Some(HeapObj::Array { items, .. }) => items.iter().filter_map(value_as_f64).collect(),
            _ => Vec::new(),
        }
    }

    /// Every element of an array as `Option<f64>`, preserving position (`None`
    /// for `na`/non-numeric). Used by `array.covariance`, which pairs the two
    /// arrays by index.
    fn optional_elements(&self, h: HandleId) -> Vec<Option<f64>> {
        match self.heap.get(h) {
            Some(HeapObj::Array { items, .. }) => items.iter().map(value_as_f64).collect(),
            _ => Vec::new(),
        }
    }

    /// Allocate a fresh `array<float>` from `items` (charging the heap budget).
    fn alloc_float_array(&mut self, items: Vec<Value>, pc: usize) -> Result<Value, RuntimeError> {
        let h = self
            .heap
            .alloc(HeapObj::Array {
                elem_ty: ElemType::Float,
                items,
            })
            .map_err(|e| self.heap_limit_err(e, pc))?;
        Ok(Value::Ref(h))
    }

    fn empty_error(&self, func: CollectionFunc, pc: usize) -> RuntimeError {
        self.fail(
            RuntimeErrorKind::CollectionIndexOutOfBounds,
            format!("`{}` called on an empty array", func.name()),
            pc,
        )
    }

    fn oob_error(&self, func: CollectionFunc, index: i64, len: usize, pc: usize) -> RuntimeError {
        self.fail(
            RuntimeErrorKind::CollectionIndexOutOfBounds,
            format!(
                "`{}` index {index} out of bounds for array of size {len}",
                func.name()
            ),
            pc,
        )
    }

    fn slice_error(
        &self,
        func: CollectionFunc,
        from: i64,
        to: i64,
        len: usize,
        pc: usize,
    ) -> RuntimeError {
        self.fail(
            RuntimeErrorKind::CollectionIndexOutOfBounds,
            format!(
                "`{}` range [{from}, {to}) out of bounds for array of size {len}",
                func.name()
            ),
            pc,
        )
    }

    /// Evaluate one pure builtin over its popped arguments (in argument
    /// order). Every function propagates `na` (any `na` argument → `na`
    /// result) except `na()`/`nz()`; domain errors (√-1, log 0, asin 2,
    /// overflow) surface as `na` through the usual non-finite → `na`
    /// normalization.
    /// Resolve the optional trailing `timezone` argument shared by the
    /// `weekofyear`/`year`/`month`/... calendar function forms, and
    /// `str.format_time`. `None` (argument omitted) uses `syminfo.timezone`;
    /// a `Value::String` names a timezone ([`resolve_timezone`]), and an
    /// unknown name stops the run; a `Value::Na` gives `Ok(None)` (the
    /// caller yields `na`); any other value falls back to the syminfo
    /// default.
    fn calendar_zone(
        &self,
        tz_arg: Option<&Value>,
        pc: usize,
    ) -> Result<Option<pinewasm_tz::TimeZone<'static>>, RuntimeError> {
        match tz_arg {
            Some(Value::String(z)) => self.script_zone(z, pc).map(Some),
            Some(Value::Na | Value::DisplacedTable(_)) => Ok(None),
            None | Some(_) => Ok(Some(self.data.exchange_zone())),
        }
    }

    /// The timezone a script names, or a run-stopping error for a name
    /// PineWASM does not know: it is never read as UTC.
    fn script_zone(
        &self,
        name: &str,
        pc: usize,
    ) -> Result<pinewasm_tz::TimeZone<'static>, RuntimeError> {
        resolve_timezone(name).ok_or_else(|| {
            self.fail(
                RuntimeErrorKind::Unsupported,
                format!(
                    "timezone `{name}` is not a timezone PineWASM knows: give an IANA name such as `America/New_York` (IANA release {}) or a fixed offset such as `UTC-5`",
                    pinewasm_tz::TZDATA_VERSION
                ),
                pc,
            )
        })
    }

    /// Evaluate a `timestamp` from six ordered `int` date/time fields
    /// (`year, month, day, hour, minute, second`) and a timezone offset (0 for
    /// the UTC overload). Any na field, a non-int field, or i64 overflow of any
    /// intermediate of the calendar arithmetic (month carry, day count, or the
    /// seconds/milliseconds products) yields `na`, in debug and release builds
    /// alike.
    fn eval_timestamp(fields: &[Value], tz_offset_seconds: i64) -> Value {
        let mut nums = [0i64; 6];
        for (slot, v) in nums.iter_mut().zip(fields) {
            match v {
                Value::Int(n) => *slot = *n,
                _ => return Value::Na,
            }
        }
        match timestamp_at(&nums, tz_offset_seconds) {
            Some(ms) => Value::Int(ms),
            None => Value::Na,
        }
    }

    /// Fold a `ticker.*` call's arguments into a deterministic canonical
    /// ticker-id string. The form is `<prefix>:<arg0>[:<arg1>...]` for the
    /// derived-chart functions and a joined form for `new`/`modify`/`inherit`.
    /// Any `na` argument yields `na`.
    fn eval_ticker(func: TickerFunc, args: &[Value]) -> Value {
        // Stringify one argument for the canonical id; `na` short-circuits the
        // whole result to na.
        fn token(v: &Value) -> Option<String> {
            match v {
                Value::Na | Value::DisplacedTable(_) => None,
                Value::String(s) => Some(s.clone()),
                Value::Int(n) => Some(n.to_string()),
                // Format floats without a trailing ".0" for whole numbers so
                // the id is stable and human-readable.
                Value::Float(f) => Some(format!("{f}")),
                Value::Bool(b) => Some(b.to_string()),
                Value::Color { .. } | Value::Ref(_) => None,
            }
        }
        let mut parts: Vec<String> = Vec::with_capacity(args.len() + 1);
        parts.push(func.canonical_prefix().to_string());
        for a in args {
            match token(a) {
                Some(t) => parts.push(t),
                None => return Value::Na,
            }
        }
        Value::String(parts.join(":"))
    }

    /// Conservative byte bound for rendering one scalar into a string.
    /// Numeric/color formatting is bounded by a small fixed allowance; an
    /// existing string contributes its exact UTF-8 length.
    fn rendered_value_bytes(value: &Value) -> usize {
        match value {
            Value::String(s) => s.len(),
            Value::Na
            | Value::DisplacedTable(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Bool(_)
            | Value::Color { .. } => 128,
            Value::Ref(_) => 0,
        }
    }

    /// Project the scalar string a pure call may create. `Some(0)` means the
    /// function is a known string producer whose result is empty; `None`
    /// means no pre-allocation projection is available, in which case the
    /// actual returned string is charged immediately after the call.
    fn projected_pure_string_bytes(&self, func: PureFunc, args: &[Value]) -> Option<usize> {
        let string_len = |index: usize| match args.get(index) {
            Some(Value::String(s)) => s.len(),
            _ => 0,
        };
        match func {
            PureFunc::Nz => {
                let selected = if args.first().is_some_and(Value::is_na) {
                    args.get(1)
                } else {
                    args.first()
                };
                selected.and_then(|v| match v {
                    Value::String(s) => Some(s.len()),
                    _ => None,
                })
            }
            PureFunc::StrTostring => Some(match args.first() {
                Some(Value::String(s)) => s.len(),
                Some(Value::Ref(handle)) => {
                    return self.collection_string_bytes(*handle, args.get(1));
                }
                Some(value) => Self::rendered_value_bytes(value).saturating_add(string_len(1)),
                None => 0,
            }),
            PureFunc::StrFormat => {
                let Some(Value::String(fmt)) = args.first() else {
                    return Some(0);
                };
                // Each opening brace can introduce at most one substitution.
                // Escapes/malformed placeholders only make this an overestimate.
                let placeholders = fmt.bytes().filter(|b| *b == b'{').count();
                let max_sub = args
                    .iter()
                    .skip(1)
                    .map(Self::rendered_value_bytes)
                    .max()
                    .unwrap_or(0);
                Some(
                    fmt.len()
                        .saturating_add(placeholders.saturating_mul(max_sub)),
                )
            }
            PureFunc::SyminfoPrefix
            | PureFunc::SyminfoTicker
            | PureFunc::StrUpper
            | PureFunc::StrLower
            | PureFunc::StrTrim
            | PureFunc::StrSubstring => Some(string_len(0)),
            PureFunc::StrReplaceAll => {
                let (
                    Some(Value::String(source)),
                    Some(Value::String(target)),
                    Some(Value::String(rep)),
                ) = (args.first(), args.get(1), args.get(2))
                else {
                    return Some(0);
                };
                if target.is_empty() || rep.len() <= target.len() {
                    Some(source.len())
                } else {
                    // Input bytes are charged before this scan. The exact
                    // count avoids rejecting a missing target merely because
                    // a worst-case expansion would have been large.
                    let matches = source.matches(target).count();
                    Some(
                        source
                            .len()
                            .saturating_add(matches.saturating_mul(rep.len() - target.len())),
                    )
                }
            }
            PureFunc::StrReplace => {
                let (
                    Some(Value::String(source)),
                    Some(Value::String(target)),
                    Some(Value::String(rep)),
                ) = (args.first(), args.get(1), args.get(2))
                else {
                    return Some(0);
                };
                let occurrence = match args.get(3) {
                    Some(Value::Int(n)) if *n >= 0 => *n as usize,
                    Some(Value::Float(n)) if *n >= 0.0 => *n as usize,
                    _ => return Some(source.len()),
                };
                let found =
                    !target.is_empty() && source.match_indices(target).nth(occurrence).is_some();
                Some(if found {
                    source
                        .len()
                        .saturating_sub(target.len())
                        .saturating_add(rep.len())
                } else {
                    source.len()
                })
            }
            PureFunc::StrRepeat => {
                let Some(Value::String(source)) = args.first() else {
                    return Some(0);
                };
                let count = match args.get(1) {
                    Some(Value::Int(n)) => *n as f64,
                    Some(Value::Float(n)) => *n,
                    _ => return Some(0),
                };
                if count < 0.0 {
                    return Some(0);
                }
                let n = count as usize;
                let sep = match args.get(2) {
                    Some(Value::String(s)) => s.len(),
                    _ => 0,
                };
                let capped_projection = source
                    .len()
                    .saturating_mul(n)
                    .saturating_add(sep.saturating_mul(n));
                if capped_projection > MAX_STRING_LEN {
                    Some(0)
                } else {
                    Some(
                        source
                            .len()
                            .saturating_mul(n)
                            .saturating_add(sep.saturating_mul(n.saturating_sub(1))),
                    )
                }
            }
            PureFunc::StrMatch => Some(string_len(0)),
            PureFunc::StrFormatTime => Some(string_len(1).saturating_mul(8).saturating_add(64)),
            PureFunc::TimeframeFromSeconds => Some(64),
            PureFunc::Ticker(_) => Some(
                args.iter()
                    .map(Self::rendered_value_bytes)
                    .fold(64usize, usize::saturating_add),
            ),
            _ => None,
        }
    }

    /// Charge the string bytes inspected and projected by a pure call before
    /// invoking it, then reserve projected output bytes before allocation.
    /// This function is shared by the interpreter and AOT `host.vm` path.
    fn charge_pure_string_work(
        &mut self,
        func: PureFunc,
        args: &[Value],
        pc: usize,
        budget: &mut u64,
    ) -> Result<Option<usize>, RuntimeError> {
        let inspected = args.iter().fold(0usize, |total, value| {
            total.saturating_add(match value {
                Value::String(s) => s.len(),
                // `str.tostring` reads an array or matrix element by element
                // and formats each element with the whole `format` pattern,
                // whose parsing and fraction digits cost its length.
                Value::Ref(handle) if func == PureFunc::StrTostring => {
                    let per_element = match args.get(1) {
                        Some(Value::String(format)) => format.len().saturating_add(1),
                        _ => 1,
                    };
                    match self.heap.get(*handle) {
                        Some(HeapObj::Array { items, .. } | HeapObj::Matrix { items, .. }) => {
                            items.len().saturating_mul(per_element)
                        }
                        _ => 0,
                    }
                }
                _ => 0,
            })
        });
        self.charge_extra_work(budget, (inspected as u64).saturating_sub(1), pc)?;

        // Projection may scan a charged input (replace/format). Do it only
        // after the input-byte charge has succeeded.
        let projected = self.projected_pure_string_bytes(func, args);
        if let Some(bytes) = projected {
            self.charge_extra_work(budget, bytes as u64, pc)?;
            self.reserve_string_bytes(bytes, pc)?;
        }
        Ok(projected)
    }

    fn call_pure(
        &mut self,
        func: PureFunc,
        args: &[Value],
        pc: usize,
        budget: &mut u64,
    ) -> Result<Value, RuntimeError> {
        // One-argument float functions share a single table-driven path.
        //
        // The transcendentals come from `pinewasm_num`, not from `std`.
        // `std` binds them to the host platform's libm, which made the same
        // script disagree in the last ulp between macOS, glibc, and the
        // browser — and left AOT artifacts with nothing exact to be compiled
        // against. `sqrt`, `to_degrees`, and `to_radians` are exact
        // operations (a square root and a single multiply), so they stay on
        // the intrinsics.
        let float_unary: Option<fn(f64) -> f64> = match func {
            PureFunc::MathSqrt => Some(f64::sqrt),
            PureFunc::MathExp => Some(pinewasm_num::raw::exp),
            // Natural (base-e) logarithm; log(0) and log(negative) are
            // -inf/NaN and normalize to na.
            PureFunc::MathLog => Some(pinewasm_num::raw::ln),
            PureFunc::MathLog10 => Some(pinewasm_num::raw::log10),
            PureFunc::MathSin => Some(pinewasm_num::raw::sin),
            PureFunc::MathCos => Some(pinewasm_num::raw::cos),
            PureFunc::MathTan => Some(pinewasm_num::raw::tan),
            PureFunc::MathAsin => Some(pinewasm_num::raw::asin),
            PureFunc::MathAcos => Some(pinewasm_num::raw::acos),
            PureFunc::MathAtan => Some(pinewasm_num::raw::atan),
            // NOT `f64::to_degrees`/`to_radians`: those multiply by a folded
            // `180/pi`. This implementation multiplies then divides, in a fixed
            // order defined in pinewasm_num. Same operation count. See
            // `pinewasm_num::raw::to_degrees`.
            PureFunc::MathToDegrees => Some(pinewasm_num::raw::to_degrees),
            PureFunc::MathToRadians => Some(pinewasm_num::raw::to_radians),
            _ => None,
        };
        if let Some(f) = float_unary {
            let x = self.pure_num(func, &args[0], pc)?;
            return Ok(x.map_or(Value::Na, |v| Value::from_f64(f(v))));
        }

        Ok(match func {
            // na(x): true iff x is na — a plain bool, never na itself.
            PureFunc::Na => Value::Bool(
                args[0].is_na() || matches!(&args[0], Value::String(value) if value.is_empty()),
            ),
            // nz(x, replacement): select; the compiler materialized the
            // typed default when the source form omitted the replacement.
            PureFunc::Nz => {
                if args[0].is_na() {
                    args[1].clone()
                } else {
                    args[0].clone()
                }
            }
            PureFunc::MathAbs => match &args[0] {
                Value::Na | Value::DisplacedTable(_) => Value::Na,
                // Checked: abs(i64::MIN) overflows → na.
                Value::Int(v) => v.checked_abs().map_or(Value::Na, Value::Int),
                Value::Float(v) => Value::from_f64(v.abs()),
                other => {
                    return Err(self.fail(
                        RuntimeErrorKind::TypeMismatch,
                        format!(
                            "math.abs requires a numeric argument, found {}",
                            other.type_name()
                        ),
                        pc,
                    ))
                }
            },
            PureFunc::MathSign => match &args[0] {
                Value::Na | Value::DisplacedTable(_) => Value::Na,
                Value::Int(v) => Value::Int(v.signum()),
                // NaN never enters the value domain, so the three-way
                // comparison is total.
                Value::Float(v) => Value::Float(if *v > 0.0 {
                    1.0
                } else if *v < 0.0 {
                    -1.0
                } else {
                    0.0
                }),
                other => {
                    return Err(self.fail(
                        RuntimeErrorKind::TypeMismatch,
                        format!(
                            "math.sign requires a numeric argument, found {}",
                            other.type_name()
                        ),
                        pc,
                    ))
                }
            },
            PureFunc::MathFloor | PureFunc::MathCeil | PureFunc::MathRound if args.len() == 1 => {
                match &args[0] {
                    Value::Na | Value::DisplacedTable(_) => Value::Na,
                    // Identity on ints (already whole).
                    Value::Int(v) => Value::Int(*v),
                    Value::Float(v) => {
                        // Rounds half away from zero, as `f64::round` does.
                        let rounded = match func {
                            PureFunc::MathFloor => v.floor(),
                            PureFunc::MathCeil => v.ceil(),
                            _ => v.round(),
                        };
                        f64_to_int(rounded)
                    }
                    other => {
                        return Err(self.fail(
                            RuntimeErrorKind::TypeMismatch,
                            format!(
                                "{} requires a numeric argument, found {}",
                                func.name(),
                                other.type_name()
                            ),
                            pc,
                        ))
                    }
                }
            }
            // math.round(value, precision): float result rounded to
            // `precision` decimal digits, half away from zero. Only a
            // runtime-computed precision arrives here; a statically-known one
            // outside [0, 100] was already rejected as E0248.
            PureFunc::MathRound => {
                let value = self.pure_num(func, &args[0], pc)?;
                let precision = match &args[1] {
                    Value::Na | Value::DisplacedTable(_) => None,
                    Value::Int(p) => Some(*p),
                    other => {
                        return Err(self.fail(
                            RuntimeErrorKind::TypeMismatch,
                            format!(
                                "math.round precision must be an int, found {}",
                                other.type_name()
                            ),
                            pc,
                        ))
                    }
                };
                match (value, precision) {
                    (Some(v), Some(p)) => {
                        // Clamped to [0, 100].
                        let factor =
                            pinewasm_num::raw::powi(10.0, pinewasm_num::clamp_precision(p));
                        let scaled = v * factor;
                        if scaled.is_finite() {
                            Value::from_f64(scaled.round() / factor)
                        } else {
                            Value::Na
                        }
                    }
                    _ => Value::Na,
                }
            }
            PureFunc::MathMin | PureFunc::MathMax => {
                if args.iter().any(Value::is_na) {
                    Value::Na
                } else if args.iter().all(|a| matches!(a, Value::Int(_))) {
                    // All-int arguments keep the exact int type.
                    let ints = args.iter().map(|a| match a {
                        Value::Int(v) => *v,
                        _ => unreachable!("checked all-int above"),
                    });
                    let folded = if func == PureFunc::MathMin {
                        ints.min()
                    } else {
                        ints.max()
                    };
                    folded.map_or(Value::Na, Value::Int)
                } else {
                    let mut acc: Option<f64> = None;
                    for arg in args {
                        let v = self
                            .pure_num(func, arg, pc)?
                            .expect("na arguments handled above");
                        acc = Some(match acc {
                            None => v,
                            Some(a) => {
                                if func == PureFunc::MathMin {
                                    a.min(v)
                                } else {
                                    a.max(v)
                                }
                            }
                        });
                    }
                    Value::from_f64(acc.expect("argc >= 2"))
                }
            }
            PureFunc::MathAvg => {
                let mut sum = 0.0f64;
                for arg in args {
                    match self.pure_num(func, arg, pc)? {
                        Some(v) => sum += v,
                        None => return Ok(Value::Na),
                    }
                }
                Value::from_f64(sum / args.len() as f64)
            }
            PureFunc::MathPow => {
                let base = self.pure_num(func, &args[0], pc)?;
                let exponent = self.pure_num(func, &args[1], pc)?;
                match (base, exponent) {
                    (Some(b), Some(e)) => Value::from_f64(pinewasm_num::raw::powf(b, e)),
                    _ => Value::Na,
                }
            }
            // math.random(min, max[, seed]): one keyed, counter-based
            // splitmix64 stream per run (see docs/runtime.md for the
            // determinism contract).
            PureFunc::MathRandom => {
                let min = self.pure_num(func, &args[0], pc)?;
                let max = self.pure_num(func, &args[1], pc)?;
                let seed = match args.get(2) {
                    None => Some(DEFAULT_RANDOM_SEED),
                    Some(Value::Na | Value::DisplacedTable(_)) => None,
                    Some(Value::Int(s)) => Some(*s as u64),
                    Some(other) => {
                        return Err(self.fail(
                            RuntimeErrorKind::TypeMismatch,
                            format!(
                                "math.random seed must be an int, found {}",
                                other.type_name()
                            ),
                            pc,
                        ))
                    }
                };
                match (min, max, seed) {
                    (Some(lo), Some(hi), Some(key)) => {
                        let (draw, draws) = vm_tick::random_draw(key, self.random_draws);
                        self.random_draws = draws;
                        // Top 53 bits → uniform in [0, 1).
                        let unit = (draw >> 11) as f64 / (1u64 << 53) as f64;
                        Value::from_f64(lo + unit * (hi - lo))
                    }
                    _ => Value::Na,
                }
            }
            PureFunc::MathRoundToMintick => {
                // mintick was validated finite and positive before bar 0.
                let mintick = self.data.mintick;
                match self.pure_num(func, &args[0], pc)? {
                    Some(v) => Value::from_f64((v / mintick).round() * mintick),
                    None => Value::Na,
                }
            }
            PureFunc::MathFloor | PureFunc::MathCeil => {
                unreachable!("single-argument forms handled above (argc validated)")
            }

            // ---- str.* namespace -----------------------------------------
            // All are na-propagating: any `na` argument yields `na`. String
            // operands are extracted with `pure_str`; the compiled programs
            // pass exactly the operand shapes the checker/validator enforce.
            // A formatted collection's length is known only once it is
            // written: charge the bytes written, whether or not they fit.
            PureFunc::StrTostring => match (&args[0], args.get(1)) {
                (Value::Ref(handle), Some(Value::String(format))) => {
                    let (text, written) = self.collection_string(*handle, Some(format), pc)?;
                    self.charge_extra_work(budget, written as u64, pc)?;
                    text.map_or(Value::Na, Value::String)
                }
                _ => self.str_tostring(&args[0], args.get(1), pc)?,
            },
            PureFunc::StrTonumber => match self.pure_str(func, &args[0], pc)? {
                None => Value::Na,
                Some(s) => parse_pine_number(s),
            },
            PureFunc::StrLength => match self.pure_str(func, &args[0], pc)? {
                None => Value::Na,
                // Unicode code points, not bytes.
                Some(s) => Value::Int(s.chars().count() as i64),
            },
            // Split a ticker id on its first `:`. An id with no prefix has no
            // exchange to report (`na`) and is entirely its own symbol name.
            PureFunc::SyminfoPrefix | PureFunc::SyminfoTicker => {
                match self.pure_str(func, &args[0], pc)? {
                    None => Value::Na,
                    Some(id) => match id.split_once(':') {
                        Some((prefix, ticker)) => Value::String(
                            if func == PureFunc::SyminfoPrefix {
                                prefix
                            } else {
                                ticker
                            }
                            .to_string(),
                        ),
                        None if func == PureFunc::SyminfoPrefix => Value::Na,
                        None => Value::String(id.to_string()),
                    },
                }
            }
            PureFunc::StrUpper | PureFunc::StrLower => {
                match self.pure_str(func, &args[0], pc)? {
                    None => Value::Na,
                    Some(s) => {
                        // ASCII-only case change (non-ASCII letters are
                        // left unchanged).
                        let mapped: String = if func == PureFunc::StrUpper {
                            s.chars().map(|c| c.to_ascii_uppercase()).collect()
                        } else {
                            s.chars().map(|c| c.to_ascii_lowercase()).collect()
                        };
                        Value::String(mapped)
                    }
                }
            }
            PureFunc::StrTrim => match self.pure_str(func, &args[0], pc)? {
                // na source gives an empty string.
                None => Value::String(String::new()),
                // Strip spaces, tabs, and newlines from both ends.
                Some(s) => Value::String(
                    s.trim_matches(|c: char| c == ' ' || c == '\t' || c == '\n' || c == '\r')
                        .to_string(),
                ),
            },
            PureFunc::StrContains | PureFunc::StrStartswith | PureFunc::StrEndswith => {
                match (
                    self.pure_str(func, &args[0], pc)?,
                    self.pure_str(func, &args[1], pc)?,
                ) {
                    (Some(hay), Some(needle)) => Value::Bool(match func {
                        PureFunc::StrContains => hay.contains(needle),
                        PureFunc::StrStartswith => hay.starts_with(needle),
                        _ => hay.ends_with(needle),
                    }),
                    _ => Value::Na,
                }
            }
            PureFunc::StrPos => match (
                self.pure_str(func, &args[0], pc)?,
                self.pure_str(func, &args[1], pc)?,
            ) {
                // Code-point index of the first occurrence, or `na` if absent.
                // Returns na, not -1, so `na(str.pos(...))` works as a guard.
                (Some(hay), Some(needle)) => match hay.find(needle) {
                    Some(byte_idx) => Value::Int(hay[..byte_idx].chars().count() as i64),
                    None => Value::Na,
                },
                _ => Value::Na,
            },
            PureFunc::StrMatch => match (
                self.pure_str(func, &args[0], pc)?,
                self.pure_str(func, &args[1], pc)?,
            ) {
                // Regex match: the first matched substring, or "" if absent.
                // `fancy-regex` supports lookaround in patterns while keeping
                // invalid/evaluation failures structured.
                (Some(hay), Some(pat)) => {
                    if pat.len() > MAX_REGEX_PATTERN_BYTES {
                        return Err(self.fail(
                            RuntimeErrorKind::StringLimitExceeded,
                            format!(
                                "`str.match` pattern is {} bytes; maximum is {MAX_REGEX_PATTERN_BYTES}",
                                pat.len()
                            ),
                            pc,
                        ));
                    }
                    let compiled = if let Some(cached) = self.regex_memo.get(pat) {
                        cached.clone()
                    } else {
                        // RegexBuilder owns one pattern string and build()
                        // clones it into the compiled representation. Charge
                        // both before either allocation.
                        self.reserve_string_bytes(pat.len().saturating_mul(2), pc)?;
                        let mut builder = RegexBuilder::new(pat);
                        builder
                            .backtrack_limit(REGEX_BACKTRACK_LIMIT)
                            .delegate_size_limit(REGEX_DELEGATE_SIZE_LIMIT)
                            .delegate_dfa_size_limit(REGEX_DELEGATE_DFA_SIZE_LIMIT);
                        let compiled = builder.build().map(Arc::new).map_err(|err| err.to_string());
                        let memo_bytes = pat
                            .len()
                            .saturating_add(compiled.as_ref().err().map_or(0, String::len));
                        if self.regex_memo.len() < MAX_REGEX_MEMO_ENTRIES
                            && self.regex_memo_bytes.saturating_add(memo_bytes)
                                <= MAX_REGEX_MEMO_BYTES
                        {
                            self.reserve_string_bytes(memo_bytes, pc)?;
                            self.regex_memo_bytes =
                                self.regex_memo_bytes.saturating_add(memo_bytes);
                            self.regex_memo.insert(pat.to_string(), compiled.clone());
                        }
                        compiled
                    };
                    let regex = compiled.map_err(|err| {
                        self.fail(
                            RuntimeErrorKind::Unsupported,
                            format!("`str.match` invalid regex pattern: {err}"),
                            pc,
                        )
                    })?;
                    let matched = regex.find(hay).map_err(|err| {
                        self.fail(
                            RuntimeErrorKind::Unsupported,
                            format!("`str.match` regex evaluation failed: {err}"),
                            pc,
                        )
                    })?;
                    Value::String(matched.map(|m| m.as_str().to_string()).unwrap_or_default())
                }
                _ => Value::Na,
            },
            PureFunc::StrFormatTime => {
                // A na time is formatted as the Unix epoch.
                let time = match &args[0] {
                    Value::Int(t) => *t,
                    Value::Na | Value::DisplacedTable(_) => 0,
                    _ => return Ok(Value::Na),
                };
                // format: optional; default is the ISO-8601 pattern. na -> na.
                let format: &str = match args.get(1) {
                    None => pinewasm_hir::STR_FORMAT_TIME_DEFAULT_FORMAT,
                    Some(Value::String(f)) => f.as_str(),
                    Some(Value::Na | Value::DisplacedTable(_)) => return Ok(Value::Na),
                    // Defensive: a non-string non-na format is a program error.
                    Some(_) => pinewasm_hir::STR_FORMAT_TIME_DEFAULT_FORMAT,
                };
                // timezone: optional; default `syminfo.timezone`. na -> na.
                let off = match self.calendar_zone(args.get(2), pc)? {
                    Some(zone) => session::offset_at_ms(&zone, time),
                    None => return Ok(Value::Na),
                };
                Value::String(format_time_string(time, format, off))
            }
            PureFunc::StrSubstring => {
                let source = self.pure_str(func, &args[0], pc)?;
                let begin = Some(self.pure_num(func, &args[1], pc)?.unwrap_or(0.0));
                let end = match args.get(2) {
                    None => Some(None), // 2-arg form: to the end.
                    Some(v) => self.pure_num(func, v, pc)?.map(Some),
                };
                match (source, begin, end) {
                    (Some(s), Some(b), Some(e)) => str_substring(s, b, e).map_err(|message| {
                        self.fail(RuntimeErrorKind::StringIndexOutOfBounds, message, pc)
                    })?,
                    _ => Value::Na,
                }
            }
            PureFunc::StrReplaceAll => {
                match (
                    self.pure_str(func, &args[0], pc)?,
                    self.pure_str(func, &args[1], pc)?,
                    self.pure_str(func, &args[2], pc)?,
                ) {
                    (Some(src), Some(target), Some(rep)) => str_replace_all(src, target, rep),
                    _ => Value::Na,
                }
            }
            PureFunc::StrReplace => {
                let occurrence = self.pure_num(func, &args[3], pc)?;
                match (
                    self.pure_str(func, &args[0], pc)?,
                    self.pure_str(func, &args[1], pc)?,
                    self.pure_str(func, &args[2], pc)?,
                    occurrence,
                ) {
                    (Some(src), Some(target), Some(rep), Some(occ)) => {
                        str_replace_nth(src, target, rep, occ)
                    }
                    _ => Value::Na,
                }
            }
            PureFunc::StrRepeat => {
                let source = self.pure_str(func, &args[0], pc)?;
                let count = self.pure_num(func, &args[1], pc)?;
                let separator = match args.get(2) {
                    None => Some(String::new()),
                    Some(v) => self.pure_str(func, v, pc)?.map(str::to_string),
                };
                match (source, count, separator) {
                    (Some(s), Some(n), Some(sep)) => str_repeat(s, n, &sep),
                    _ => Value::Na,
                }
            }
            PureFunc::StrFormat => self.str_format(args, pc)?,

            // ---- color.* namespace -------------------------------------
            // new(color, transp): replace transparency (clamped 0..=100). An
            // `na` color or `na` transp yields `na` (na-propagating).
            PureFunc::ColorNew => match (&args[0], self.pure_num(func, &args[1], pc)?) {
                (Value::Color { r, g, b, .. }, Some(transp)) => Value::Color {
                    r: *r,
                    g: *g,
                    b: *b,
                    t: transp.clamp(0.0, 100.0),
                    input_origin: None,
                },
                (Value::Na | Value::DisplacedTable(_), _) | (_, None) => Value::Na,
                (other, _) => return Err(self.color_type_err(func, other, pc)),
            },
            // rgb(red, green, blue, transp): channels clamped 0..=255, transp
            // clamped 0..=100. An `na` argument yields `na`.
            PureFunc::ColorRgb => {
                let red = self.pure_color_int(func, &args[0], pc)?;
                let green = self.pure_color_int(func, &args[1], pc)?;
                let blue = self.pure_color_int(func, &args[2], pc)?;
                let transp = self.pure_num(func, &args[3], pc)?;
                match (red, green, blue, transp) {
                    (Some(r), Some(g), Some(b), Some(t)) => Value::Color {
                        r: clamp_u8(r, 0, 255),
                        g: clamp_u8(g, 0, 255),
                        b: clamp_u8(b, 0, 255),
                        t: t.clamp(0.0, 100.0),
                        input_origin: None,
                    },
                    _ => Value::Na,
                }
            }
            // from_gradient(value, bottom_value, top_value, bottom_color,
            // top_color): interpolate all four RGBA channels by value's
            // clamped position in [bottom_value, top_value]. Any `na` numeric
            // or color argument yields `na`.
            PureFunc::ColorFromGradient => {
                let value = self.pure_num(func, &args[0], pc)?;
                let bottom_value = self.pure_num(func, &args[1], pc)?;
                let top_value = self.pure_num(func, &args[2], pc)?;
                let bottom = self.pure_color_rgba(func, &args[3], pc)?;
                let top = self.pure_color_rgba(func, &args[4], pc)?;
                match (value, bottom_value, top_value, bottom, top) {
                    (Some(v), Some(bv), Some(tv), Some(bc), Some(tc)) => {
                        color_from_gradient(v, bv, tv, bc, tc)
                    }
                    _ => Value::Na,
                }
            }
            // r/g/b/t(color): extract a channel. An `na` color yields `na`.
            PureFunc::ColorR
            | PureFunc::ColorG
            | PureFunc::ColorB
            | PureFunc::ColorT
            | PureFunc::ColorRFloat
            | PureFunc::ColorGFloat
            | PureFunc::ColorBFloat
            | PureFunc::ColorTFloat => match &args[0] {
                Value::Na | Value::DisplacedTable(_) => Value::Na,
                Value::Color { r, g, b, t, .. } => {
                    let channel = match func {
                        PureFunc::ColorR | PureFunc::ColorRFloat => f64::from(*r),
                        PureFunc::ColorG | PureFunc::ColorGFloat => f64::from(*g),
                        PureFunc::ColorB | PureFunc::ColorBFloat => f64::from(*b),
                        _ => *t,
                    };
                    if func.color_channel_is_float() {
                        Value::Float(channel)
                    } else {
                        Value::Int(channel.round() as i64)
                    }
                }
                other => return Err(self.color_type_err(func, other, pc)),
            },

            // ---- strategy trade accessors / conversions ----------------
            // `trade_num` (an int, or `na`) indexes the broker's retained
            // trade list; an out-of-range index reads `na` (never a panic).
            PureFunc::StrategyClosedTrade(field) => self.read_trade(field, &args[0], false),
            PureFunc::StrategyOpenTrade(field) => self.read_trade(field, &args[0], true),
            // Currency conversions: identity unless the host supplies a rate
            // (see docs/runtime.md). `na`
            // propagates.
            PureFunc::StrategyConvertToAccount | PureFunc::StrategyConvertToSymbol => {
                match self.pure_num(func, &args[0], pc)? {
                    Some(v) => Value::from_f64(v * self.strategy_conversion_rate(func)),
                    None => Value::Na,
                }
            }
            PureFunc::StrategyDefaultEntryQty => match self.pure_num(func, &args[0], pc)? {
                Some(fill_price) => self.broker.as_ref().map_or(Value::Na, |broker| {
                    Value::from_f64(broker.default_entry_qty(fill_price))
                }),
                None => Value::Na,
            },
            // timeframe.in_seconds(tf): na argument propagates to na.
            PureFunc::TimeframeInSeconds => match self.pure_str(func, &args[0], pc)? {
                None => Value::Na,
                Some(s) => Value::Int(timeframe_in_seconds(s)),
            },
            // timeframe.from_seconds(seconds): na argument propagates to na.
            PureFunc::TimeframeFromSeconds => match &args[0] {
                Value::Na | Value::DisplacedTable(_) => Value::Na,
                Value::Int(n) => Value::String(timeframe_from_seconds(*n)),
                _ => Value::Na,
            },
            PureFunc::TimeOpen | PureFunc::TimeClose => {
                let as_int = |value: &Value| match value {
                    Value::Int(v) => Some(*v),
                    _ => None,
                };
                match (
                    self.pure_str(func, &args[0], pc)?,
                    self.pure_str(func, &args[1], pc)?,
                    self.pure_str(func, &args[2], pc)?,
                    as_int(&args[3]),
                    as_int(&args[4]),
                ) {
                    (
                        Some(tf),
                        Some(session),
                        Some(timezone),
                        Some(bars_back),
                        Some(timeframe_bars_back),
                    ) => self.time_function_value(
                        tf,
                        session,
                        timezone,
                        matches!(func, PureFunc::TimeClose),
                        bars_back,
                        timeframe_bars_back,
                        pc,
                        budget,
                    )?,
                    _ => Value::Na,
                }
            }
            // timestamp(year, month, day, hour, minute, second): six ints (UTC
            // overload). Any na field yields na; i64 overflow yields na.
            PureFunc::Timestamp => Self::eval_timestamp(&args[..6], 0),
            // timestamp(timezone, year, month, day, hour, minute, second): the
            // fields are a wall-clock reading in the timezone; na tz → na. A
            // reading that occurs twice is the earlier instant, and one that
            // never occurs is read with the offset before the change
            // (`pinewasm_tz::TimeZone::utc_from_local`).
            PureFunc::TimestampTz => match &args[0] {
                Value::Na | Value::DisplacedTable(_) => Value::Na,
                Value::String(tz) => {
                    let zone = self.script_zone(tz, pc)?;
                    match Self::eval_timestamp(&args[1..7], 0) {
                        // `na` where the conversion could leave i64, as the
                        // calendar arithmetic is.
                        Value::Int(local) if session::local_ms_in_range(local) => {
                            Value::Int(session::utc_from_local_ms(&zone, local))
                        }
                        Value::Int(_) => Value::Na,
                        other => other,
                    }
                }
                _ => Value::Na,
            },
            // ticker.*(...): fold the arguments into a deterministic canonical
            // ticker-id string. Any na argument yields na. Real symbology resolution is a host
            // concern.
            PureFunc::Ticker(tf) => Self::eval_ticker(tf, args),

            // weekofyear(time[, timezone]): ISO 8601 week-of-year of the
            // decomposed date. na time/tz -> na; default tz `syminfo.timezone`.
            PureFunc::WeekOfYear => {
                let time = match &args[0] {
                    Value::Int(t) => *t,
                    _ => return Ok(Value::Na),
                };
                let off = match self.calendar_zone(args.get(1), pc)? {
                    Some(zone) => session::offset_at_ms(&zone, time),
                    None => return Ok(Value::Na),
                };
                // `na` when the ISO Thursday leaves the i64 epoch-ms range.
                week_of_year_at(time, off).map_or(Value::Na, Value::Int)
            }
            // year/month/dayofmonth/dayofweek/hour/minute/second(time[,
            // timezone]): the FUNCTION forms — decompose an arbitrary UNIX-ms
            // time into the named civil component in the given fixed-offset
            // timezone (default `syminfo.timezone`). na time/tz -> na. Mirrors
            // the calendar *variable* forms, applied to the passed time.
            PureFunc::Calendar(field) => {
                let time = match &args[0] {
                    Value::Int(t) => *t,
                    _ => return Ok(Value::Na),
                };
                let off = match self.calendar_zone(args.get(1), pc)? {
                    Some(zone) => session::offset_at_ms(&zone, time),
                    None => return Ok(Value::Na),
                };
                let (year, month, day, hour, minute, second, dow) = civil_from_epoch_ms(time, off);
                Value::Int(match field {
                    CalendarField::Year => year,
                    CalendarField::Month => month,
                    CalendarField::DayOfMonth => day,
                    CalendarField::DayOfWeek => dow,
                    CalendarField::Hour => hour,
                    CalendarField::Minute => minute,
                    CalendarField::Second => second,
                })
            }

            // ---- type casts ----------------------------------------------
            // int(x): truncate a float toward zero; na → na. `f64_to_int` uses
            // `as i64` truncation (toward zero) and maps out-of-range/NaN to na.
            PureFunc::CastInt => match &args[0] {
                Value::Na | Value::DisplacedTable(_) => Value::Na,
                Value::Int(v) => Value::Int(*v),
                Value::Float(v) => f64_to_int(v.trunc()),
                other => {
                    return Err(self.fail(
                        RuntimeErrorKind::TypeMismatch,
                        format!(
                            "int() requires a numeric argument, found {}",
                            other.type_name()
                        ),
                        pc,
                    ))
                }
            },
            // float(x): widen an int; na → na.
            PureFunc::CastFloat => match &args[0] {
                Value::Na | Value::DisplacedTable(_) => Value::Na,
                Value::Int(v) => Value::from_f64(*v as f64),
                Value::Float(v) => Value::from_f64(*v),
                other => {
                    return Err(self.fail(
                        RuntimeErrorKind::TypeMismatch,
                        format!(
                            "float() requires a numeric argument, found {}",
                            other.type_name()
                        ),
                        pc,
                    ))
                }
            },
            // bool(x): false for na / false / a numeric zero; true otherwise
            // NOT na-propagating.
            PureFunc::CastBool => match &args[0] {
                Value::Na | Value::DisplacedTable(_) => Value::Bool(false),
                Value::Bool(b) => Value::Bool(*b),
                Value::Int(v) => Value::Bool(*v != 0),
                Value::Float(v) => Value::Bool(*v != 0.0),
                other => {
                    return Err(self.fail(
                        RuntimeErrorKind::TypeMismatch,
                        format!(
                            "bool() requires a scalar argument, found {}",
                            other.type_name()
                        ),
                        pc,
                    ))
                }
            },
            // color(x): identity on a color; na → na.
            PureFunc::CastColor => match &args[0] {
                Value::Na | Value::DisplacedTable(_) => Value::Na,
                c @ Value::Color { .. } => c.clone(),
                other => {
                    return Err(self.fail(
                        RuntimeErrorKind::TypeMismatch,
                        format!(
                            "color() requires a color argument, found {}",
                            other.type_name()
                        ),
                        pc,
                    ))
                }
            },

            _ => unreachable!("float-unary functions handled above"),
        })
    }

    /// Resolve one symbol-to-account rate from the host lane. Historical bars
    /// use their exact aligned sample. Realtime updates use the latest valid
    /// confirmed sample at or before the forming bar. Equal/unset currencies
    /// are resolved identity; missing differing-currency data stays `None` so
    /// the broker can report incomplete conversion while using its compatibility
    /// identity fallback for arithmetic.
    fn symbol_to_account_rate_at(&self, bar: usize, realtime: bool) -> Option<f64> {
        let broker = self.broker.as_ref()?;
        let account = broker.account_currency();
        let symbol = broker.symbol_currency();
        let is_set = |currency: &str| !currency.is_empty() && currency != "NONE";
        if !is_set(account) || !is_set(symbol) || account == symbol {
            return Some(1.0);
        }
        let key = format!("currency_rate/{symbol}/{account}");
        let series = self.data.request_data.get(&key)?;
        let valid = |rate: &&f64| rate.is_finite() && **rate > 0.0;
        if realtime {
            series
                .iter()
                .take(bar.saturating_add(1))
                .rfind(valid)
                .copied()
        } else {
            series
                .get(bar)
                .copied()
                .filter(|rate| rate.is_finite() && *rate > 0.0)
        }
    }

    fn sync_broker_currency_rate(&mut self, bar: usize, realtime: bool) {
        let rate = self.symbol_to_account_rate_at(bar, realtime);
        if let Some(broker) = self.broker.as_mut() {
            broker.sync_currency_rate(bar, rate);
        }
    }

    /// The account/symbol conversion rate for `strategy.convert_to_*`. Both
    /// directions use the broker's synchronized symbol-to-account rate so
    /// script conversions and all monetary broker calculations agree exactly.
    fn strategy_conversion_rate(&self, func: PureFunc) -> f64 {
        let symbol_to_account = self
            .broker
            .as_ref()
            .map_or(1.0, broker::Broker::current_currency_rate);
        if matches!(func, PureFunc::StrategyConvertToAccount) {
            symbol_to_account
        } else {
            1.0 / symbol_to_account
        }
    }

    /// Read one field of a closed (`open == false`) or open (`open == true`)
    /// trade at the runtime `trade_num` index. Out-of-range or `na` indices read
    /// `na`; string fields read `na` when absent. Never panics.
    fn read_trade(&self, field: TradeAccessor, index_arg: &Value, open: bool) -> Value {
        let Some(broker) = self.broker.as_ref() else {
            return Value::Na;
        };
        let idx = match index_arg {
            Value::Int(i) if *i >= 0 => *i as usize,
            Value::Float(f) if f.is_finite() && *f >= 0.0 => *f as usize,
            _ => return Value::Na,
        };
        let Some(trade) = broker.trade_view(idx, open) else {
            return Value::Na;
        };
        // `entry_time`/`exit_time` map a bar index to its epoch-ms via the feed.
        let time_at = |bar: u32| Value::Int(self.data.time_at(bar as usize));
        match field {
            TradeAccessor::EntryPrice => Value::from_f64(trade.entry_price),
            TradeAccessor::ExitPrice => Value::from_f64(trade.exit_price),
            TradeAccessor::Size => Value::from_f64(trade.size),
            TradeAccessor::Profit => Value::from_f64(trade.profit),
            TradeAccessor::ProfitPercent => {
                Value::from_f64(trade.profit / trade.entry_notional * 100.0)
            }
            TradeAccessor::Commission => Value::from_f64(trade.commission),
            TradeAccessor::EntryBarIndex => Value::Int(trade.entry_bar as i64),
            TradeAccessor::ExitBarIndex => Value::Int(trade.exit_bar as i64),
            TradeAccessor::EntryTime => time_at(trade.entry_bar),
            TradeAccessor::ExitTime => time_at(trade.exit_bar),
            TradeAccessor::MaxRunup => Value::from_f64(trade.max_runup),
            TradeAccessor::MaxRunupPercent => {
                Value::from_f64(trade.max_runup / trade.entry_notional * 100.0)
            }
            TradeAccessor::MaxDrawdown => Value::from_f64(trade.max_drawdown),
            TradeAccessor::MaxDrawdownPercent => {
                Value::from_f64(trade.max_drawdown / trade.entry_notional * 100.0)
            }
            TradeAccessor::EntryId => str_or_na(trade.entry_id),
            TradeAccessor::ExitId => str_or_na(trade.exit_id),
            TradeAccessor::EntryComment => str_or_na(trade.entry_comment),
            TradeAccessor::ExitComment => str_or_na(trade.exit_comment),
        }
    }

    /// A numeric RGB channel, rounded before clamping. Transparency uses
    /// `pure_num` separately so its fractional percentage is retained.
    fn pure_color_int(
        &self,
        func: PureFunc,
        value: &Value,
        pc: usize,
    ) -> Result<Option<i64>, RuntimeError> {
        match value {
            Value::Na | Value::DisplacedTable(_) => Ok(None),
            Value::Int(v) => Ok(Some(*v)),
            // Float RGB channels round to the nearest integer; the caller clamps to the
            // channel range. A non-finite float is `na`. The
            // `as i64` cast saturates rather than panicking.
            Value::Float(v) => Ok(v.is_finite().then(|| v.round() as i64)),
            other => Err(self.color_type_err(func, other, pc)),
        }
    }

    /// A `color` operand: `na` maps to `None`, a color yields its `(r,g,b,t)`
    /// channels; anything else is a type error (defense in depth).
    fn pure_color_rgba(
        &self,
        func: PureFunc,
        value: &Value,
        pc: usize,
    ) -> Result<Option<(u8, u8, u8, f64)>, RuntimeError> {
        match value {
            Value::Na | Value::DisplacedTable(_) => Ok(None),
            Value::Color { r, g, b, t, .. } => Ok(Some((*r, *g, *b, *t))),
            other => Err(self.color_type_err(func, other, pc)),
        }
    }

    /// The standard type error for a `color.*` argument.
    fn color_type_err(&self, func: PureFunc, other: &Value, pc: usize) -> RuntimeError {
        self.fail(
            RuntimeErrorKind::TypeMismatch,
            format!(
                "{} received an unexpected {} argument",
                func.name(),
                other.type_name()
            ),
            pc,
        )
    }

    /// A string operand of a pure builtin: `na` maps to `None`, a string
    /// borrows, anything else is a type error (compiled programs pass the
    /// validated shapes; this is defense in depth).
    fn pure_str<'v>(
        &self,
        func: PureFunc,
        value: &'v Value,
        pc: usize,
    ) -> Result<Option<&'v str>, RuntimeError> {
        match value {
            Value::Na | Value::DisplacedTable(_) => Ok(None),
            Value::String(s) => Ok(Some(s)),
            other => Err(self.fail(
                RuntimeErrorKind::TypeMismatch,
                format!(
                    "{} requires string arguments, found {}",
                    func.name(),
                    other.type_name()
                ),
                pc,
            )),
        }
    }

    /// `str.tostring(value[, format])`: stringify a scalar, array or matrix.
    /// An `na` value gives "NaN". The optional format string drives
    /// numeric formatting; an `na` format is treated as absent (default
    /// formatting).
    fn str_tostring(
        &self,
        value: &Value,
        format: Option<&Value>,
        pc: usize,
    ) -> Result<Value, RuntimeError> {
        // `str.tostring(na)` yields the string "NaN", never `na`
        // (`tostring_value` maps `Na` → "NaN"); do not short-circuit.
        let fmt = match format {
            None | Some(Value::Na | Value::DisplacedTable(_)) => None,
            Some(Value::String(f)) => Some(f.as_str()),
            Some(other) => {
                return Err(self.fail(
                    RuntimeErrorKind::TypeMismatch,
                    format!(
                        "str.tostring format must be a string, found {}",
                        other.type_name()
                    ),
                    pc,
                ))
            }
        };
        if let Value::Ref(handle) = value {
            return self
                .collection_string(*handle, fmt, pc)
                .map(|(text, _)| text.map_or(Value::Na, Value::String));
        }
        Ok(Value::String(tostring_value(value, fmt, self.data.mintick)))
    }

    /// `str.tostring` of an array (`[1, 22, -3]`) or a matrix (one
    /// bracketed row per line). Each element is written as `str.tostring`
    /// writes that scalar, with the same format; `na` elements read "NaN".
    /// Like the other string-growing operations, a result longer than
    /// [`MAX_STRING_LEN`] is `na` (`None`). Without a format the length is
    /// known exactly beforehand, so an over-long result is not written at
    /// all; with one, the length is checked after each element, so the text
    /// never grows past the cap by more than one element. Also returns the
    /// bytes written, which a formatted call is charged for.
    fn collection_string(
        &self,
        handle: HandleId,
        fmt: Option<&str>,
        pc: usize,
    ) -> Result<(Option<String>, usize), RuntimeError> {
        if fmt.is_none()
            && self
                .collection_string_len(handle)
                .is_some_and(|len| len > MAX_STRING_LEN)
        {
            return Ok((None, 0));
        }
        let write_line = |out: &mut String, items: &[Value]| -> bool {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                out.push_str(&tostring_value(item, fmt, self.data.mintick));
                if out.len() > MAX_STRING_LEN {
                    return false;
                }
            }
            out.push(']');
            out.len() <= MAX_STRING_LEN
        };
        let mut out = String::new();
        let complete = match self.heap.get(handle) {
            Some(HeapObj::Array { items, .. }) => write_line(&mut out, items),
            // As in `collection_string_len`: every row writes at least three
            // bytes, so a taller matrix is `na` without writing.
            Some(HeapObj::Matrix { rows, .. })
                if rows.saturating_mul(3) > MAX_STRING_LEN.saturating_add(1) =>
            {
                false
            }
            Some(HeapObj::Matrix {
                rows, cols, items, ..
            }) => (0..*rows).all(|row| {
                if row > 0 {
                    out.push('\n');
                }
                write_line(&mut out, &items[row * cols..(row + 1) * cols])
            }),
            _ => {
                return Err(self.fail(
                    RuntimeErrorKind::TypeMismatch,
                    "str.tostring expects an array or matrix".to_string(),
                    pc,
                ))
            }
        };
        let written = out.len();
        Ok((complete.then_some(out), written))
    }

    /// The exact byte length `collection_string` writes without a format, or
    /// `MAX_STRING_LEN + 1` once it is known to exceed the cap, without
    /// walking further. `None` for a handle that is not an array or matrix of
    /// writable elements.
    fn collection_string_len(&self, handle: HandleId) -> Option<usize> {
        const OVER: usize = MAX_STRING_LEN + 1;
        let element = |value: &Value| -> Option<usize> {
            Some(match value {
                Value::String(s) => s.len(),
                Value::Bool(b) => b.to_string().len(),
                Value::Int(i) => i.to_string().len(),
                Value::Float(f) => default_num_string(*f).len(),
                Value::Na | Value::DisplacedTable(_) => 3,
                Value::Color { .. } | Value::Ref(_) => return None,
            })
        };
        let line = |items: &[Value]| -> Option<usize> {
            let mut bytes = 2usize + 2 * items.len().saturating_sub(1);
            for item in items {
                bytes = bytes.saturating_add(element(item)?);
                if bytes > MAX_STRING_LEN {
                    return Some(OVER);
                }
            }
            Some(bytes)
        };
        match self.heap.get(handle)? {
            HeapObj::Array { items, .. } => line(items),
            HeapObj::Matrix {
                rows, cols, items, ..
            } => {
                // Every row writes at least `[]` and a line break. A matrix
                // with zero columns holds no cells to charge, so check this
                // bound before walking its rows.
                if rows.saturating_mul(3) > MAX_STRING_LEN.saturating_add(1) {
                    return Some(OVER);
                }
                let mut bytes = rows.saturating_sub(1);
                for row in 0..*rows {
                    bytes = bytes.saturating_add(line(&items[row * cols..(row + 1) * cols])?);
                    if bytes > MAX_STRING_LEN {
                        return Some(OVER);
                    }
                }
                Some(bytes)
            }
            _ => None,
        }
    }

    /// The bytes `collection_string` writes, projected before allocation: the
    /// exact length, 0 when that exceeds [`MAX_STRING_LEN`] (the result is
    /// then `na` and nothing is written, as for `str.repeat`), or `None` with
    /// a `format`, whose output grows with the value (digit grouping on a
    /// large float). A formatted result is charged at the bytes written.
    fn collection_string_bytes(&self, handle: HandleId, format: Option<&Value>) -> Option<usize> {
        if matches!(format, Some(Value::String(_))) {
            return None;
        }
        let bytes = self.collection_string_len(handle)?;
        Some(if bytes > MAX_STRING_LEN { 0 } else { bytes })
    }

    /// `str.format(formatString, arg0, ...)`: `{N}` placeholder substitution.
    /// A `na` format string yields `na`; individual `na` args stringify to
    /// `"NaN"`. Implements plain `{N}` plus the `MessageFormat` `number`
    /// type (`{0,number,#.##}`, `integer`/`currency`/`percent` keywords —
    /// see `render_placeholder`); `date`/`time`/`choice` types are not
    /// implemented.
    fn str_format(&self, args: &[Value], pc: usize) -> Result<Value, RuntimeError> {
        let fmt = match &args[0] {
            Value::Na | Value::DisplacedTable(_) => return Ok(Value::Na),
            Value::String(f) => f.clone(),
            other => {
                return Err(self.fail(
                    RuntimeErrorKind::TypeMismatch,
                    format!(
                        "str.format format must be a string, found {}",
                        other.type_name()
                    ),
                    pc,
                ))
            }
        };
        let subs = &args[1..];
        Ok(Value::String(format_placeholders(
            &fmt,
            subs,
            self.data.mintick,
        )))
    }

    /// Order arguments already occupy a contiguous stack tail in slot order.
    /// Lend that tail to the broker decoder, then drop only consumed values
    /// and restore the same allocation. No argument vector or reverse copy.
    #[inline(never)]
    fn exec_order_operands(&mut self, kind: OrderKind, pc: usize) -> Result<(), RuntimeError> {
        let count = kind.operand_count() as usize;
        if self.stack.len() < count {
            // Ordinary top-first pops consumed every available operand before
            // reporting underflow. Preserve that partial state on invalid IL.
            self.stack.clear();
            return Err(self.underflow(pc));
        }
        let mut stack = std::mem::take(&mut self.stack);
        let start = stack.len() - count;
        let result = self.place_order(kind, &stack[start..], pc);
        stack.truncate(start);
        self.stack = stack;
        result
    }

    /// Place an order from a decoded `Order` opcode. `ops` are the
    /// operands in the kind's slot order (see `pinewasm_hir::OrderKind`).
    /// Order primitives are pure book effects; nonsensical operands (an `na`
    /// direction, a non-positive qty, an exit with no legs) drop the order
    /// rather than intentionally aborting the run. Unexpected internal panics
    /// remain defects, not part of the program-input error contract.
    fn place_order(
        &mut self,
        kind: OrderKind,
        ops: &[Value],
        pc: usize,
    ) -> Result<(), RuntimeError> {
        // One order instruction can populate several broker-owned vectors
        // over its lifetime (pending/deferred book, lots, closed trades,
        // report samples, fill-alert metadata). Reserve a conservative unit
        // before any book effect; cumulative charging also bounds replace /
        // cancel churn.
        self.reserve_vm_bytes(STRATEGY_ORDER_RESERVATION_BYTES, pc)?;
        // Order ids, comments, OCA names, and alert text are cloned out of the
        // operand stack into broker-owned books/metadata. Charge that retained
        // UTF-8 before any clone so a large literal cannot multiply through
        // rapid order placement while bypassing the cumulative string cap.
        let mut broker_string_bytes = 0usize;
        for operand in ops {
            if let Value::String(value) = operand {
                broker_string_bytes =
                    broker_string_bytes
                        .checked_add(value.len())
                        .ok_or_else(|| {
                            self.fail(
                                RuntimeErrorKind::StringLimitExceeded,
                                format!(
                                    "broker string accounting overflowed; limit is {} bytes",
                                    self.options.max_string_bytes_per_run
                                ),
                                pc,
                            )
                        })?;
            }
        }
        self.reserve_string_bytes(broker_string_bytes, pc)?;
        let execution_bar = self.bar;
        let execution_price = self.fill_recalculation_price.unwrap_or_else(|| {
            self.data
                .close
                .get(execution_bar)
                .copied()
                .unwrap_or(f64::NAN)
        });

        // Operand decoders. A missing/`na` operand yields `None`.
        let as_string = |v: &Value| -> Option<String> {
            match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            }
        };
        let optional_string =
            |v: &Value| -> Option<String> { as_string(v).filter(|value| !value.is_empty()) };
        let as_float = |v: &Value| -> Option<f64> {
            match v {
                Value::Float(f) if f.is_finite() => Some(*f),
                Value::Int(i) => Some(*i as f64),
                _ => None,
            }
        };
        let as_int = |v: &Value| -> Option<i64> {
            match v {
                Value::Int(i) => Some(*i),
                _ => None,
            }
        };
        let strategy_direction = |v: &Value| -> Option<f64> {
            match v {
                Value::String(token) if token == "strategy.long" => Some(1.0),
                Value::String(token) if token == "strategy.short" => Some(-1.0),
                // Also accepts a numeric sign for the direction.
                Value::Float(sign) if sign.is_finite() && *sign != 0.0 => Some(sign.signum()),
                Value::Int(sign) if *sign != 0 => Some((*sign as f64).signum()),
                _ => None,
            }
        };
        let as_bool = |v: &Value| -> bool { matches!(v, Value::Bool(true)) };

        // These risk parameters are `simple string`, not compile-time-only
        // enums. The semantic pass validates literal typos, while the VM must
        // validate input-derived values after host overrides have been
        // applied. Do this before borrowing the broker so an invalid token can
        // produce a source-mapped runtime error without a conflicting borrow.
        let risk_entry_direction = if kind == OrderKind::RiskAllowEntryIn {
            match ops.first() {
                Some(Value::String(token)) => match token.as_str() {
                    "strategy.direction.long" => Some(1),
                    "strategy.direction.short" => Some(-1),
                    "strategy.direction.all" => Some(0),
                    _ => {
                        return Err(self.fail(
                            RuntimeErrorKind::TypeMismatch,
                            format!(
                                "strategy.risk.allow_entry_in value must be strategy.direction.all, strategy.direction.long, or strategy.direction.short, found `{token}`"
                            ),
                            pc,
                        ));
                    }
                },
                Some(other) => {
                    return Err(self.fail(
                        RuntimeErrorKind::TypeMismatch,
                        format!(
                            "strategy.risk.allow_entry_in value must be a direction string, found {}",
                            other.type_name()
                        ),
                        pc,
                    ));
                }
                None => {
                    return Err(self.fail(
                        RuntimeErrorKind::InvalidProgram,
                        "strategy.risk.allow_entry_in is missing its value operand".to_string(),
                        pc,
                    ));
                }
            }
        } else {
            None
        };
        let risk_unit = if matches!(
            kind,
            OrderKind::RiskMaxDrawdown | OrderKind::RiskMaxIntradayLoss
        ) {
            let member = if kind == OrderKind::RiskMaxDrawdown {
                "strategy.risk.max_drawdown"
            } else {
                "strategy.risk.max_intraday_loss"
            };
            match ops.get(1) {
                Some(Value::String(token)) => match token.as_str() {
                    "strategy.cash" => Some(broker::RiskUnit::Cash),
                    "strategy.percent_of_equity" => Some(broker::RiskUnit::PercentOfEquity),
                    _ => {
                        return Err(self.fail(
                            RuntimeErrorKind::TypeMismatch,
                            format!(
                                "{member} type must be strategy.cash or strategy.percent_of_equity, found `{token}`"
                            ),
                            pc,
                        ));
                    }
                },
                Some(other) => {
                    return Err(self.fail(
                        RuntimeErrorKind::TypeMismatch,
                        format!(
                            "{member} type must be a risk-unit string, found {}",
                            other.type_name()
                        ),
                        pc,
                    ));
                }
                None => {
                    return Err(self.fail(
                        RuntimeErrorKind::InvalidProgram,
                        format!("{member} is missing its type operand"),
                        pc,
                    ));
                }
            }
        } else {
            None
        };
        let risk_threshold = if matches!(
            kind,
            OrderKind::RiskMaxDrawdown | OrderKind::RiskMaxIntradayLoss
        ) {
            let member = if kind == OrderKind::RiskMaxDrawdown {
                "strategy.risk.max_drawdown"
            } else {
                "strategy.risk.max_intraday_loss"
            };
            let Some(value) = ops.first().and_then(as_float) else {
                return Err(self.fail(
                    RuntimeErrorKind::TypeMismatch,
                    format!("{member} value must be a finite non-negative number"),
                    pc,
                ));
            };
            if !value.is_finite() || value < 0.0 {
                return Err(self.fail(
                    RuntimeErrorKind::TypeMismatch,
                    format!("{member} value must be a finite non-negative number, found {value}"),
                    pc,
                ));
            }
            if risk_unit == Some(broker::RiskUnit::PercentOfEquity) && value > 100.0 {
                return Err(self.fail(
                    RuntimeErrorKind::TypeMismatch,
                    format!(
                        "{member} percent-of-equity value must be between 0 and 100 inclusive, found {value}"
                    ),
                    pc,
                ));
            }
            Some(value)
        } else {
            None
        };

        // No broker => not a strategy script (guarded by E0241 at compile time);
        // defensively ignore.
        let Some(broker) = self.broker.as_mut() else {
            return Ok(());
        };

        match kind {
            OrderKind::Entry | OrderKind::Order => {
                // [id, direction, qty, limit, stop, oca_name, oca_type,
                //  comment, alert_message, disable_alert]
                let id = as_string(&ops[0]).unwrap_or_default();
                // A missing or invalid direction drops the order.
                let Some(sign) = strategy_direction(&ops[1]) else {
                    return Ok(());
                };
                let qty = as_float(&ops[2]);
                let limit = as_float(&ops[3]);
                let stop = as_float(&ops[4]);
                let metadata = broker::OrderMetadata {
                    oca_name: optional_string(&ops[5]),
                    oca_type: broker::OcaType::from_token(as_string(&ops[6]).as_deref()),
                    comment: optional_string(&ops[7]),
                    alert_message: optional_string(&ops[8]),
                    disable_alert: as_bool(&ops[9]),
                };
                if kind == OrderKind::Entry {
                    broker.place_entry(id, sign, qty, limit, stop, metadata);
                } else {
                    broker.place_order(id, sign, qty, limit, stop, metadata);
                }
            }
            OrderKind::Exit => {
                // [id, from_entry, qty, qty_percent, profit, limit, loss, stop,
                //  trail_price, trail_points, trail_offset, oca_name, comment,
                //  comment_profit, comment_loss, comment_trailing,
                //  alert_message, alert_profit, alert_loss, alert_trailing,
                //  disable_alert]
                let id = as_string(&ops[0]).unwrap_or_default();
                let from_entry = as_string(&ops[1]);
                let qty = as_float(&ops[2]);
                let qty_percent = as_float(&ops[3]);
                let profit = as_float(&ops[4]);
                let limit = as_float(&ops[5]);
                let loss = as_float(&ops[6]);
                let stop = as_float(&ops[7]);
                let trail_price = as_float(&ops[8]);
                let trail_points = as_float(&ops[9]);
                let trail_offset = as_float(&ops[10]);
                let metadata = broker::ExitMetadata {
                    oca_name: optional_string(&ops[11]),
                    comment: optional_string(&ops[12]),
                    comment_profit: optional_string(&ops[13]),
                    comment_loss: optional_string(&ops[14]),
                    comment_trailing: optional_string(&ops[15]),
                    alert_message: optional_string(&ops[16]),
                    alert_profit: optional_string(&ops[17]),
                    alert_loss: optional_string(&ops[18]),
                    alert_trailing: optional_string(&ops[19]),
                    disable_alert: as_bool(&ops[20]),
                };
                broker.place_exit(
                    id,
                    from_entry,
                    qty,
                    qty_percent,
                    profit,
                    limit,
                    loss,
                    stop,
                    trail_price,
                    trail_points,
                    trail_offset,
                    metadata,
                );
            }
            OrderKind::Close => {
                // [id, comment, qty, qty_percent, alert_message, immediately,
                //  disable_alert]
                let id = as_string(&ops[0]);
                let qty = as_float(&ops[2]);
                let qty_percent = as_float(&ops[3]);
                let metadata = broker::OrderMetadata {
                    comment: optional_string(&ops[1]),
                    alert_message: optional_string(&ops[4]),
                    disable_alert: as_bool(&ops[6]),
                    ..broker::OrderMetadata::default()
                };
                broker.place_close(
                    id,
                    qty,
                    qty_percent,
                    metadata,
                    as_bool(&ops[5]).then_some((execution_bar, execution_price)),
                );
            }
            OrderKind::CloseAll => {
                // [comment, alert_message, immediately, disable_alert]
                let metadata = broker::OrderMetadata {
                    comment: optional_string(&ops[0]),
                    alert_message: optional_string(&ops[1]),
                    disable_alert: as_bool(&ops[3]),
                    ..broker::OrderMetadata::default()
                };
                broker.place_close(
                    None,
                    None,
                    None,
                    metadata,
                    as_bool(&ops[2]).then_some((execution_bar, execution_price)),
                );
            }
            OrderKind::Cancel => {
                // [id]
                if let Some(id) = as_string(&ops[0]) {
                    broker.cancel(&id);
                }
            }
            OrderKind::CancelAll => {
                broker.cancel_all();
            }
            OrderKind::RiskMaxPositionSize => {
                // [contracts] — register/refresh the position-size cap. A
                // missing/`na`/non-positive value disables it.
                broker.set_max_position_size_at_site(pc, as_float(&ops[0]).unwrap_or(f64::NAN));
            }
            OrderKind::RiskAllowEntryIn => {
                // [value] — the token was validated before borrowing the broker.
                broker.set_allow_entry_in_at_site(pc, risk_entry_direction.unwrap_or_default());
            }
            OrderKind::RiskMaxDrawdown => {
                // [value, type, alert_message] — halt and flatten when equity
                // drawdown trips.
                broker.set_max_drawdown_at_site(
                    pc,
                    risk_threshold.expect("validated risk threshold"),
                    risk_unit.unwrap_or(broker::RiskUnit::PercentOfEquity),
                    optional_string(&ops[2]),
                );
            }
            OrderKind::RiskMaxIntradayLoss => {
                // [value, type, alert_message] — halt and flatten for the
                // current risk session.
                broker.set_max_intraday_loss_at_site(
                    pc,
                    risk_threshold.expect("validated risk threshold"),
                    risk_unit.unwrap_or(broker::RiskUnit::PercentOfEquity),
                    optional_string(&ops[2]),
                );
            }
            OrderKind::RiskMaxIntradayFilledOrders => {
                // [count, alert_message] — halt and flatten for the current
                // risk session.
                broker.set_max_intraday_filled_orders_at_site(
                    pc,
                    as_int(&ops[0]).map_or(f64::NAN, |count| count as f64),
                    optional_string(&ops[1]),
                );
            }
            OrderKind::RiskMaxConsLossDays => {
                // [count, alert_message?] — permanently halt after consecutive losing sessions.
                broker.set_max_cons_loss_days_at_site(
                    pc,
                    as_int(&ops[0]).map_or(f64::NAN, |count| count as f64),
                    optional_string(&ops[1]),
                );
            }
        }
        Ok(())
    }

    #[inline(never)]
    fn exec_window_mean(
        &mut self,
        slot: u32,
        length: u32,
        pc: usize,
        budget: &mut u64,
    ) -> Result<(), RuntimeError> {
        // Honest metering: computing a window mean scans
        // `length` cells, so the instruction costs `length`
        // budget units in total (one was already charged at
        // dispatch).
        let extra = u64::from(length).saturating_sub(1);
        self.charge_instructions(budget, extra, pc)?;
        self.profile_extra(pc, extra);

        let len = length as usize;
        let history = self.user_slots.get(slot as usize).ok_or_else(|| {
            self.fail(
                RuntimeErrorKind::InvalidProgram,
                format!("series slot {slot} out of range"),
                pc,
            )
        })?;
        if len == 0 {
            // Rejected by the validator; defense in depth.
            return Err(self.fail(
                RuntimeErrorKind::InvalidProgram,
                "window mean of length 0".into(),
                pc,
            ));
        }
        let source = match &history[self.bar & self.slot_mask] {
            Value::Float(value) => Some(*value),
            Value::Int(value) => Some(*value as f64),
            Value::Na => None,
            other => {
                return Err(self.fail(
                    RuntimeErrorKind::TypeMismatch,
                    format!(
                        "window mean requires numeric cells, found {}",
                        other.type_name()
                    ),
                    pc,
                ))
            }
        };
        // An na source consumes no observation. Retaining a dense source
        // ring separately from bar history preserves older valid values even
        // when a missing run is longer than the raw history ring.
        let value = self
            .sma
            .mean(slot, self.bar, source, length)
            .map_or(Value::Na, Value::from_f64);
        self.push(value, pc)
    }

    #[inline(never)]
    fn exec_window_extremum_dynamic(
        &mut self,
        slot: u32,
        length: u32,
        highest: bool,
        pc: usize,
        budget: &mut u64,
    ) -> Result<(), RuntimeError> {
        let extra = u64::from(length).saturating_sub(1);
        self.charge_instructions(budget, extra, pc)?;
        self.profile_extra(pc, extra);
        let history = self.user_slots.get(slot as usize).ok_or_else(|| {
            self.fail(
                RuntimeErrorKind::InvalidProgram,
                format!("series slot {slot} out of range"),
                pc,
            )
        })?;
        let len = length as usize;
        let value = if self.bar + 1 < len || history[self.bar & self.slot_mask].is_na() {
            Value::Na
        } else {
            let mut best = None;
            for offset in (0..len).rev() {
                let cell = &history[(self.bar - offset) & self.slot_mask];
                let value = match cell {
                    Value::Float(value) => *value,
                    Value::Int(value) => *value as f64,
                    Value::Na | Value::DisplacedTable(_) => continue,
                    other => {
                        return Err(self.fail(
                            RuntimeErrorKind::TypeMismatch,
                            format!(
                                "window extremum requires numeric cells, found {}",
                                other.type_name()
                            ),
                            pc,
                        ));
                    }
                };
                best = Some(match best {
                    Some(previous) if highest => f64::max(previous, value),
                    Some(previous) => f64::min(previous, value),
                    None => value,
                });
            }
            // Window extrema never return `-0.0` (the engine's rule for
            // constant lengths, `pinewasm_ta`'s `canon_zero`):
            // `f64::max`/`f64::min` leave the sign of a zero result to the
            // target, and a zero extremum is `+0.0` on every one.
            best.map_or(Value::Na, |best| {
                Value::from_f64(canonical_window_zero(best))
            })
        };
        self.push(value, pc)
    }

    fn exec_window_statistic_dynamic(
        &mut self,
        slot: u32,
        func: pinewasm_hir::TaFunc,
        length: usize,
        pc: usize,
        budget: &mut u64,
    ) -> Result<(), RuntimeError> {
        use pinewasm_hir::TaFunc;

        let extra = (length as u64).saturating_sub(1);
        self.charge_instructions(budget, extra, pc)?;
        self.profile_extra(pc, extra);
        let mut scratch = std::mem::take(&mut self.window_scratch);
        scratch.clear();
        let result = if self.bar + 1 < length {
            Value::Na
        } else {
            let history = self.user_slots.get(slot as usize).ok_or_else(|| {
                self.fail(
                    RuntimeErrorKind::InvalidProgram,
                    format!("series slot {slot} out of range"),
                    pc,
                )
            })?;
            let mut offset = length;
            while offset > 0 {
                offset -= 1;
                let cell = &history[(self.bar - offset) & self.slot_mask];
                let value = match cell {
                    Value::Float(value) => *value,
                    Value::Int(value) => *value as f64,
                    Value::Na | Value::DisplacedTable(_) => f64::NAN,
                    other => {
                        self.window_scratch = scratch;
                        return Err(self.fail(
                            RuntimeErrorKind::TypeMismatch,
                            format!(
                                "dynamic TA statistic requires numeric cells, found {}",
                                other.type_name()
                            ),
                            pc,
                        ));
                    }
                };
                scratch.push(value);
            }
            match func {
                TaFunc::Median | TaFunc::MedianInt => {
                    scratch.sort_by(f64::total_cmp);
                    let mid = scratch.len() / 2;
                    let result = if scratch.len() % 2 == 1 {
                        scratch[mid]
                    } else {
                        f64::midpoint(scratch[mid - 1], scratch[mid])
                    };
                    ta::decode(func, result)
                }
                TaFunc::Mode | TaFunc::ModeInt => {
                    scratch.sort_by(f64::total_cmp);
                    let mut best = scratch[0];
                    let mut best_len = 0usize;
                    let mut run_value = scratch[0];
                    let mut run_len = 0usize;
                    for &value in &scratch {
                        if value == run_value {
                            run_len += 1;
                        } else {
                            run_value = value;
                            run_len = 1;
                        }
                        if run_len > best_len {
                            best = run_value;
                            best_len = run_len;
                        }
                    }
                    ta::decode(func, best)
                }
                TaFunc::Range | TaFunc::RangeInt => {
                    let (mut high, mut low) = (f64::MIN, f64::MAX);
                    let mut present = false;
                    for &value in &scratch {
                        present |= !value.is_nan();
                        high = high.max(value);
                        low = low.min(value);
                    }
                    if present {
                        // A zero maximum is
                        // `+0.0`, which fixes the difference whatever the
                        // sign of a zero minimum.
                        ta::decode(func, canonical_window_zero(high) - low)
                    } else {
                        // A window with no value has no range. Without this
                        // the fold's start values gave `-inf`, which the int
                        // form decoded as `i64::MIN`.
                        Value::Na
                    }
                }
                _ => {
                    self.window_scratch = scratch;
                    return Err(self.fail(
                        RuntimeErrorKind::InvalidProgram,
                        format!("unsupported dynamic statistic {func:?}"),
                        pc,
                    ));
                }
            }
        };
        self.window_scratch = scratch;
        self.push(result, pc)
    }

    fn run_bar(&mut self) -> Result<(), RuntimeError> {
        if let Some(broker) = self.broker.as_mut() {
            broker.begin_script_execution();
        }
        let program = self.program.clone();
        let main = program.main();
        let code = &main.code;
        let mut budget = self.options.max_instructions_per_bar;
        self.stack.clear();

        // The loop (charge one unit, fetch, execute, follow the control
        // flow) is the generic driver `vm_dispatch::dispatch_loop`, which is
        // extracted and proved; the opcode arms are `exec_instr`.
        let mut host = RunBarHost {
            vm: self,
            program: &program,
        };
        vm_dispatch::dispatch_loop(&mut host, code, &mut budget)
    }

    /// Execute the instruction at `pc` for `run_bar`'s dispatch loop:
    /// `Flow::Jump` for a taken branch, `Flow::Return` for `Return`, and
    /// `Flow::Next` otherwise. `budget` is the per-bar counter, charged by the
    /// linear-cost arms.
    #[inline(always)]
    fn exec_instr(
        &mut self,
        program: &CsilProgram,
        instr: &CsilInstr,
        pc: usize,
        budget: &mut u64,
    ) -> Result<vm_dispatch::Flow, RuntimeError> {
        // Profiling hook: one execution + the dispatch unit. The
        // "honest metering" extras charged inside the arms below mirror
        // themselves via `profile_extra` next to each `budget -= extra`.
        if let Some(p) = self.profile.as_mut() {
            p.count[pc] += 1;
            p.units[pc] += 1;
        }

        match instr {
            CsilInstr::PushNa => self.push(vm_ops::const_value(vm_ops::VmConst::Na), pc)?,
            CsilInstr::PushBool(b) => {
                self.push(vm_ops::const_value(vm_ops::VmConst::Bool(*b)), pc)?;
            }
            CsilInstr::PushI64(v) => {
                if *v == 0 {
                    if let Some(flow) = self.exec_loop_bound(program, Value::Int(*v), pc, budget) {
                        return Ok(flow);
                    }
                }
                self.push(vm_ops::const_value(vm_ops::VmConst::Int(*v)), pc)?;
            }
            CsilInstr::PushF64(v) => {
                if v.get().to_bits() & !(1_u64 << 63) == 0 {
                    if let Some(flow) =
                        self.exec_loop_bound(program, Value::Float(v.get()), pc, budget)
                    {
                        return Ok(flow);
                    }
                }
                if v.get().to_bits() == 2.0_f64.to_bits() {
                    // Keep the pow kernel, with the exponent
                    // known at this call site for constant propagation.
                    if let Some(flow) = self.exec_constant_pow(program, 2.0, pc, budget) {
                        return Ok(flow);
                    }
                }
                self.push(vm_ops::const_value(vm_ops::VmConst::Float(v.get())), pc)?;
            }
            CsilInstr::PushStr(id) => {
                let s = program.constants.get(*id as usize).ok_or_else(|| {
                    self.fail(
                        RuntimeErrorKind::InvalidProgram,
                        format!("string constant {id} out of range"),
                        pc,
                    )
                })?;
                self.reserve_string_bytes(s.len(), pc)?;
                self.push(Value::String(s.clone()), pc)?;
            }
            CsilInstr::PushColor { r, g, b, t } => self.push(
                vm_ops::const_value(vm_ops::VmConst::Color {
                    r: *r,
                    g: *g,
                    b: *b,
                    t: t.get(),
                }),
                pc,
            )?,
            CsilInstr::Dup => {
                let top = self.pop(pc)?;
                self.push(top.clone(), pc)?;
                self.push(top, pc)?;
            }
            CsilInstr::Pop => vm_ops::discard(self.pop(pc)?),

            CsilInstr::ReadBuiltin { source, offset } => {
                if *source == BuiltinSeries::Close {
                    match vm_close_step::close_step(
                        &mut self.stack,
                        self.options.max_stack_depth,
                        self.bar,
                        *offset,
                        &vm_close_step::Ohlc {
                            open: &self.data.open,
                            high: &self.data.high,
                            low: &self.data.low,
                            close: &self.data.close,
                        },
                    ) {
                        Ok(()) => {}
                        Err(vm_close_step::CloseStepError::StackOverflow) => {
                            return Err(self.overflow(pc));
                        }
                    }
                } else {
                    let value = self.read_builtin(*source, *offset);
                    self.push(value, pc)?;
                }
            }
            CsilInstr::ReadUserSlot { slot, offset } => {
                let value = vm_slots::slot_read(
                    &self.user_slots,
                    self.bar,
                    *offset as usize,
                    self.slot_mask,
                    *slot as usize,
                )
                .unwrap_or(Value::Na);
                match &value {
                    Value::Ref(_) => {
                        if let Some(flow) = self.exec_slot_pair(program, &value, pc, budget) {
                            return Ok(flow);
                        }
                    }
                    Value::Bool(condition) => {
                        if let Some(flow) =
                            self.exec_slot_branch(program, Some(*condition), pc, budget)
                        {
                            return Ok(flow);
                        }
                    }
                    Value::Na | Value::DisplacedTable(_) => {
                        if let Some(flow) = self.exec_slot_branch(program, None, pc, budget) {
                            return Ok(flow);
                        }
                    }
                    _ => {}
                }
                self.push(value, pc)?;
            }
            CsilInstr::ReadBuiltinDynamic { source } => {
                self.exec_read_builtin_dynamic(*source, pc)?;
            }
            CsilInstr::ReadUserSlotDynamic { slot } => {
                let offset = self.pop(pc)?;
                let limit = self.program.metadata.max_bars_back;
                let value = match vm_ops::user_slot_dynamic(
                    &self.user_slots,
                    &offset,
                    limit,
                    self.bar,
                    self.slot_mask,
                    *slot as usize,
                ) {
                    Ok(cell) => cell.unwrap_or(Value::Na),
                    Err(offset) => return Err(self.dynamic_history_too_deep(offset, pc)),
                };
                self.push(value, pc)?;
            }
            CsilInstr::ReadInput { input } => {
                let value = self.resolve_input(*input, pc)?;
                self.push(value, pc)?;
            }
            CsilInstr::ReadSecurity {
                selector,
                source,
                htf_seconds,
                timeframe_expr,
                calendar_months,
                gaps_on,
                lookahead_on,
            } => {
                self.exec_read_security(
                    SecurityReadSpec {
                        selector: selector.as_deref(),
                        source: *source,
                        htf_seconds: *htf_seconds,
                        timeframe_expr: timeframe_expr.as_deref(),
                        calendar_months: *calendar_months,
                        gaps_on: *gaps_on,
                        lookahead_on: *lookahead_on,
                    },
                    pc,
                    budget,
                )?;
            }
            CsilInstr::ReadSecurityExpr {
                expr,
                htf_seconds,
                timeframe_expr,
                calendar_months,
                gaps_on,
                lookahead_on,
            } => {
                self.exec_read_security_expr(
                    expr,
                    *htf_seconds,
                    timeframe_expr.as_deref(),
                    *calendar_months,
                    *gaps_on,
                    *lookahead_on,
                    pc,
                    budget,
                )?;
            }
            CsilInstr::ReadRequest { func: _, selector } => {
                let value = self.read_request(selector);
                self.push(value, pc)?;
            }
            CsilInstr::BuildFootprint { selector } => {
                let value = self.build_footprint(selector, pc)?;
                self.push(value, pc)?;
            }
            CsilInstr::ReadSecurityLowerTf { selector } => {
                let value = self.build_security_lower_tf(selector, pc)?;
                self.push(value, pc)?;
            }
            CsilInstr::ReadPivotLevels {
                system,
                anchor_seconds,
            } => {
                let value = self.build_pivot_levels(*system, *anchor_seconds, pc, budget)?;
                self.push(value, pc)?;
            }
            CsilInstr::ReadTimeTf { close, tf_seconds } => {
                let timeframe = timeframe_from_seconds(*tf_seconds);
                let value =
                    self.time_function_value(&timeframe, "", "", *close, 0, 0, pc, budget)?;
                self.push(value, pc)?;
            }
            CsilInstr::FootprintRowByPrice => {
                let price = self.pop(pc)?;
                let footprint = self.pop(pc)?;
                let value = self.footprint_row_by_price(&footprint, &price, pc)?;
                self.push(value, pc)?;
            }
            CsilInstr::StoreUserSlotCurrent { slot } => {
                if let Some(flow) = self.exec_store_current_read(program, *slot, pc, budget) {
                    return Ok(flow);
                }
                let value = self.pop(pc)?;
                if !vm_slots::slot_store(
                    &mut self.user_slots,
                    &mut self.written,
                    self.bar,
                    self.slot_mask,
                    *slot as usize,
                    value,
                ) {
                    return Err(self.fail(
                        RuntimeErrorKind::InvalidProgram,
                        format!("series slot {slot} out of range"),
                        pc,
                    ));
                }
            }

            CsilInstr::WindowMean { slot, length } => {
                self.exec_window_mean(*slot, *length, pc, budget)?;
            }
            CsilInstr::WindowMeanDynamic { slot } => {
                let value = self.pop(pc)?;
                let length = Self::runtime_history_offset(value).ok_or_else(|| {
                    self.fail(
                        RuntimeErrorKind::InvalidTaLength,
                        format!("ta.sma length must be an integer from 1 to {MAX_WINDOW_LENGTH}"),
                        pc,
                    )
                })?;
                if !(1..=u64::from(MAX_WINDOW_LENGTH)).contains(&length) {
                    return Err(self.fail(
                        RuntimeErrorKind::InvalidTaLength,
                        format!("ta.sma length {length} is outside 1..={MAX_WINDOW_LENGTH}"),
                        pc,
                    ));
                }
                self.exec_window_mean(*slot, length as u32, pc, budget)?;
            }
            CsilInstr::WindowExtremumDynamic { slot, highest } => {
                let value = self.pop(pc)?;
                let length = Self::runtime_history_offset(value).ok_or_else(|| {
                    self.fail(
                        RuntimeErrorKind::InvalidTaLength,
                        format!(
                            "ta.highest/lowest length must be an integer from 1 to \
                             {MAX_WINDOW_LENGTH}"
                        ),
                        pc,
                    )
                })?;
                if !(1..=u64::from(MAX_WINDOW_LENGTH)).contains(&length) {
                    return Err(self.fail(
                        RuntimeErrorKind::InvalidTaLength,
                        format!(
                            "ta.highest/lowest length {length} is outside 1..={MAX_WINDOW_LENGTH}"
                        ),
                        pc,
                    ));
                }
                self.exec_window_extremum_dynamic(*slot, length as u32, *highest, pc, budget)?;
            }
            CsilInstr::WindowStatisticDynamic { slot, func } => {
                let value = self.pop(pc)?;
                let length = match value {
                    Value::Int(value) if (1..=i64::from(MAX_WINDOW_LENGTH)).contains(&value) => {
                        value as usize
                    }
                    _ => {
                        return Err(self.fail(
                            RuntimeErrorKind::InvalidTaLength,
                            format!(
                                "dynamic TA length must be an int from 1 to {MAX_WINDOW_LENGTH}"
                            ),
                            pc,
                        ));
                    }
                };
                self.exec_window_statistic_dynamic(*slot, *func, length, pc, budget)?;
            }
            CsilInstr::EmaSimpleDynamic { slot } => {
                let length_value = self.pop(pc)?;
                let source = self.pop(pc)?;
                let length = match length_value {
                    Value::Int(value) if (1..=i64::from(MAX_WINDOW_LENGTH)).contains(&value) => {
                        value as u32
                    }
                    _ => {
                        return Err(self.fail(
                            RuntimeErrorKind::InvalidTaLength,
                            format!(
                                "ta.ema length must be a simple int from 1 to {MAX_WINDOW_LENGTH}"
                            ),
                            pc,
                        ));
                    }
                };
                let slot_index = *slot as usize;
                if slot_index >= self.dynamic_ema.len() {
                    return Err(self.fail(
                        RuntimeErrorKind::InvalidProgram,
                        format!("EMA state slot {slot} out of range"),
                        pc,
                    ));
                }
                if self.dynamic_ema[slot_index].is_some_and(|state| state.length != length) {
                    return Err(self.fail(
                        RuntimeErrorKind::InvalidTaLength,
                        "ta.ema simple length changed during the run".into(),
                        pc,
                    ));
                }
                let state = self.dynamic_ema[slot_index].get_or_insert(DynamicEmaState {
                    length,
                    smoother: pinewasm_ta::Smoother::default(),
                });
                let result = match source {
                    Value::Na | Value::DisplacedTable(_) => Value::Na,
                    Value::Float(value) if !value.is_finite() => Value::Na,
                    Value::Float(value) => Value::from_f64(state.smoother.step(
                        value,
                        length,
                        2.0 / (f64::from(length) + 1.0),
                    )),
                    other => {
                        return Err(self.fail(
                            RuntimeErrorKind::TypeMismatch,
                            format!("ta.ema source must be float, found {}", other.type_name()),
                            pc,
                        ));
                    }
                };
                self.push(result, pc)?;
            }

            CsilInstr::CallPure { func, argc } => {
                if *func == PureFunc::MathAbs && *argc == 1 {
                    if let Some(flow) = self.exec_math_chain(program, pc, budget) {
                        return Ok(flow);
                    }
                }
                self.exec_call_pure_op(*func, *argc, pc, budget)?;
            }
            CsilInstr::CallPivotPointLevels { argc } => {
                self.exec_call_pivot_point_levels(*argc, pc)?;
            }

            CsilInstr::CallCollection { func, argc } => {
                if *func == CollectionFunc::ArrayGet && *argc == 2 {
                    if let Some(flow) = self.exec_array_math_chain(program, pc, budget) {
                        return Ok(flow);
                    }
                }
                self.exec_call_collection(*func, *argc, pc, budget)?;
            }

            CsilInstr::NewObject {
                type_id,
                field_count,
            } => {
                self.exec_new_object(*type_id, *field_count, pc, budget)?;
            }

            CsilInstr::CopyObject { type_id } => {
                self.exec_copy_object(*type_id, pc, budget)?;
            }

            CsilInstr::GetField { field_idx } => {
                self.exec_get_field(*field_idx, pc)?;
            }

            CsilInstr::SetField { field_idx } => {
                self.exec_set_field(*field_idx, pc)?;
            }

            CsilInstr::RegisterDrawing { kind } => {
                self.exec_register_drawing(*kind, pc, budget)?;
            }

            CsilInstr::ReadDrawingsAll { kind } => {
                self.exec_read_drawings_all(*kind, pc, budget)?;
            }

            CsilInstr::DeleteDrawing { kind } => {
                self.exec_delete_drawing(*kind, pc, budget)?;
            }

            CsilInstr::SetCell { attrs } => {
                self.exec_set_cell(attrs, pc)?;
            }

            CsilInstr::ClearCells => {
                self.exec_clear_cells(pc)?;
            }

            CsilInstr::MergeCells => {
                self.exec_merge_cells(pc, budget)?;
            }

            CsilInstr::Log { level, argc } => {
                self.exec_log(*level, *argc, pc)?;
            }

            CsilInstr::Alert => {
                self.exec_alert(pc)?;
            }

            CsilInstr::AlertCondition => {
                self.exec_alert_condition(pc)?;
            }

            CsilInstr::Order { kind } => {
                self.exec_order_operands(*kind, pc)?;
            }

            CsilInstr::CallTa { state } => {
                let index = *state as usize;
                let Some(decl) = program.ta_states.get(index) else {
                    return Err(self.fail(
                        RuntimeErrorKind::InvalidProgram,
                        format!("ta state {state} out of range"),
                        pc,
                    ));
                };
                // Honest metering: window-rescanning functions cost
                // their window length in total (one unit was already
                // charged at dispatch); O(1) functions cost 1.
                let extra = decl.func.engine_cost(&decl.params).saturating_sub(1);
                self.charge_instructions(budget, extra, pc)?;
                self.profile_extra(pc, extra);

                let arity = decl.func.engine_operands().len();
                if self.stack.len() < arity {
                    return Err(self.fail(
                        RuntimeErrorKind::StackUnderflow,
                        "operand stack underflow (validator should have rejected this program)"
                            .into(),
                        pc,
                    ));
                }
                // Operands are already contiguous in argument order.
                // The TA bridge only borrows them, so avoid moving them
                // through an initialized Value buffer on every call.
                let base = self.stack.len() - arity;
                let args = &self.stack[base..];
                let data = &self.data;
                let bar = self.bar;
                if decl.func.is_tuple() {
                    // Tuple-returning: advance once and push the tuple's
                    // leading `engine_result_arity()` values, left to right
                    // (the last element ends up on top of the stack, ready
                    // for the destructure stores). `advance_tuple` returns a
                    // fixed [Value; 3]; a shorter tuple (`ta.supertrend`'s 2
                    // values) leaves the trailing slots `na`, which the
                    // arity slice excludes.
                    let result =
                        ta::advance_tuple(&mut self.ta_state, *state, decl, args, data, bar);
                    self.stack.truncate(base);
                    let values = match result {
                        Ok(Some(values)) => values,
                        Ok(None) => {
                            return Err(self.fail(
                                RuntimeErrorKind::InvalidProgram,
                                format!("ta state {state} does not declare an engine function"),
                                pc,
                            ))
                        }
                        Err(message) => {
                            return Err(self.fail(RuntimeErrorKind::TypeMismatch, message, pc))
                        }
                    };
                    let arity = decl.func.engine_result_arity() as usize;
                    for value in values.into_iter().take(arity) {
                        self.push(value, pc)?;
                    }
                } else {
                    let result = ta::advance(&mut self.ta_state, *state, decl, args, data, bar);
                    self.stack.truncate(base);
                    let value = match result {
                        Ok(Some(value)) => value,
                        // An Invalid state (or missing entry): the
                        // validator would have rejected this program.
                        Ok(None) => {
                            return Err(self.fail(
                                RuntimeErrorKind::InvalidProgram,
                                format!("ta state {state} does not declare an engine function"),
                                pc,
                            ))
                        }
                        Err(message) => {
                            return Err(self.fail(RuntimeErrorKind::TypeMismatch, message, pc))
                        }
                    };
                    self.push(value, pc)?;
                }
            }

            CsilInstr::IntAdd
            | CsilInstr::IntSub
            | CsilInstr::IntMul
            | CsilInstr::IntRem
            | CsilInstr::PineIntDiv => {
                // Preserve exact checked i64 operations until a Pine int
                // division produces a fractional runtime value.
                let rhs = self.pop(pc)?;
                let lhs = self.pop(pc)?;
                let op = match instr {
                    CsilInstr::IntAdd => vm_ops::IntArithOp::Add,
                    CsilInstr::IntSub => vm_ops::IntArithOp::Sub,
                    CsilInstr::IntMul => vm_ops::IntArithOp::Mul,
                    CsilInstr::IntRem => vm_ops::IntArithOp::Rem,
                    _ => vm_ops::IntArithOp::PineDiv,
                };
                let value = match vm_ops::int_arith(op, lhs.na_observation(), rhs.na_observation())
                {
                    Ok(value) => value,
                    Err(vm_ops::OpMismatch) => {
                        return Err(self.fail(
                            RuntimeErrorKind::TypeMismatch,
                            format!(
                                "integer arithmetic requires numeric operands, found {} and {}",
                                lhs.type_name(),
                                rhs.type_name()
                            ),
                            pc,
                        ))
                    }
                };
                self.push(value, pc)?;
            }
            CsilInstr::IntDiv => {
                // Integer division truncates toward zero.
                let rhs = self.pop_int(pc, "integer arithmetic")?;
                let lhs = self.pop_int(pc, "integer arithmetic")?;
                self.push(
                    vm_int::int_binop(vm_int::IntBinOp::Div, lhs, rhs)
                        .map_or(Value::Na, Value::Int),
                    pc,
                )?;
            }
            CsilInstr::IntNeg => {
                let operand = self.pop(pc)?;
                let value = match vm_ops::int_neg(operand.na_observation()) {
                    Ok(value) => value,
                    Err(vm_ops::OpMismatch) => {
                        return Err(self.fail(
                            RuntimeErrorKind::TypeMismatch,
                            format!(
                                "integer negation requires a number, found {}",
                                operand.type_name()
                            ),
                            pc,
                        ))
                    }
                };
                self.push(value, pc)?;
            }
            CsilInstr::IntToF64 => {
                let operand = self.pop(pc)?;
                let value = match vm_ops::int_to_f64(operand.na_observation()) {
                    Ok(value) => value,
                    Err(vm_ops::OpMismatch) => {
                        return Err(self.fail(
                            RuntimeErrorKind::TypeMismatch,
                            format!(
                                "int-to-float conversion requires a number, found {}",
                                operand.type_name()
                            ),
                            pc,
                        ))
                    }
                };
                self.push(value, pc)?;
            }
            CsilInstr::StrConcat => {
                self.exec_str_concat(pc, budget)?;
            }

            CsilInstr::Select => {
                let rhs = self.pop(pc)?;
                let lhs = self.pop(pc)?;
                let cond = self.pop_bool(pc, "selection")?;
                // An `na` condition selects the else value — na is
                // treated as false, consistent with JumpIfFalse
                // (documented assumption). Both branches were already
                // evaluated: the ternary is eager by design.
                self.push(vm_ops::select_value(cond, lhs, rhs), pc)?;
            }

            CsilInstr::F64Add
            | CsilInstr::F64Sub
            | CsilInstr::F64Mul
            | CsilInstr::F64Div
            | CsilInstr::F64Rem => {
                // Any na operand → na (Pine NA propagation). Division by
                // zero and overflow produce non-finite values → na;
                // remainder takes the sign of the dividend, and x % 0 is NaN → na.
                let op = match instr {
                    CsilInstr::F64Add => vm_ops::FloatArithOp::Add,
                    CsilInstr::F64Sub => vm_ops::FloatArithOp::Sub,
                    CsilInstr::F64Mul => vm_ops::FloatArithOp::Mul,
                    CsilInstr::F64Div => vm_ops::FloatArithOp::Div,
                    _ => vm_ops::FloatArithOp::Rem,
                };
                if numeric_stack::evaluate(op, &mut self.stack, self.options.max_stack_depth) {
                    return Ok(vm_dispatch::Flow::Next);
                }
                let rhs = self.pop_float(pc, "arithmetic")?;
                let lhs = self.pop_float(pc, "arithmetic")?;
                self.push(vm_ops::float_arith(op, lhs, rhs), pc)?;
            }
            CsilInstr::F64Neg => {
                let operand = self.pop_float(pc, "negation")?;
                self.push(vm_ops::float_neg(operand), pc)?;
            }

            CsilInstr::CmpGt | CsilInstr::CmpGe | CsilInstr::CmpLt | CsilInstr::CmpLe => {
                let rhs = self.pop(pc)?;
                let lhs = self.pop(pc)?;
                // Numeric-polymorphic: int-int compares exactly as i64,
                // float-float as f64. A mixed pair cannot be produced by
                // the compiler (semantics coerces statically), but the
                // VM still promotes the int side — defense in depth.
                // A comparison with a missing (na) operand returns false.
                let op = match instr {
                    CsilInstr::CmpGt => vm_ops::CmpOp::Gt,
                    CsilInstr::CmpGe => vm_ops::CmpOp::Ge,
                    CsilInstr::CmpLt => vm_ops::CmpOp::Lt,
                    _ => vm_ops::CmpOp::Le,
                };
                let value = match vm_ops::cmp_values(op, lhs.na_observation(), rhs.na_observation())
                {
                    Ok(value) => value,
                    Err(vm_ops::OpMismatch) => {
                        return Err(self.fail(
                            RuntimeErrorKind::TypeMismatch,
                            format!(
                                "comparison requires numeric operands, found {} and {}",
                                lhs.type_name(),
                                rhs.type_name()
                            ),
                            pc,
                        ))
                    }
                };
                self.push(value, pc)?;
            }
            CsilInstr::CmpEq | CsilInstr::CmpNe => {
                self.exec_cmp_eq(matches!(instr, CsilInstr::CmpNe), pc)?;
            }

            CsilInstr::BoolAnd | CsilInstr::BoolOr => {
                let rhs = self.pop_bool(pc, "logic")?;
                let lhs = self.pop_bool(pc, "logic")?;
                // Kleene three-valued logic over na (documented
                // assumption; see docs/runtime.md).
                let value = if matches!(instr, CsilInstr::BoolAnd) {
                    vm_ops::bool_and(lhs, rhs)
                } else {
                    vm_ops::bool_or(lhs, rhs)
                };
                self.push(value, pc)?;
            }
            CsilInstr::BoolNot => {
                let operand = self.pop_bool(pc, "logic")?;
                self.push(vm_ops::bool_not(operand), pc)?;
            }

            CsilInstr::Jump { target } => {
                return Ok(vm_dispatch::Flow::Jump(*target as usize));
            }
            CsilInstr::JumpIfFalse { target } => {
                let cond = self.pop_bool(pc, "condition")?;
                // na conditions are treated as false, so they take this
                // branch just like an explicit false condition.
                if vm_ops::jump_if_false_taken(cond) {
                    return Ok(vm_dispatch::Flow::Jump(*target as usize));
                }
            }
            CsilInstr::JumpIfExact { on, target } => {
                let cond = self.pop_bool(pc, "condition")?;
                // Exact-value test: `na` never jumps. Short-circuits
                // `and`/`or` only when the lhs alone decides; an na lhs
                // falls through so the Kleene combine still sees the rhs.
                if vm_ops::jump_if_exact_taken(cond, *on) {
                    return Ok(vm_dispatch::Flow::Jump(*target as usize));
                }
            }
            CsilInstr::JumpIfSlotWritten { slot, target } => {
                let written =
                    vm_slots::slot_written(&self.written, *slot as usize).ok_or_else(|| {
                        self.fail(
                            RuntimeErrorKind::InvalidProgram,
                            format!("series slot {slot} out of range"),
                            pc,
                        )
                    })?;
                if written {
                    return Ok(vm_dispatch::Flow::Jump(*target as usize));
                }
            }

            CsilInstr::PlotF64 { plot_slot } => {
                match vm_plot_step::plot_step(
                    &mut self.stack,
                    &mut self.plots,
                    self.bar,
                    *plot_slot as usize,
                ) {
                    Ok(()) => {}
                    Err(vm_plot_step::PlotStepError::Underflow) => {
                        return Err(self.underflow(pc));
                    }
                    Err(vm_plot_step::PlotStepError::TypeMismatch(other)) => {
                        return Err(self.fail(
                            RuntimeErrorKind::TypeMismatch,
                            format!("plot requires a float, found {}", other.type_name()),
                            pc,
                        ))
                    }
                    Err(vm_plot_step::PlotStepError::OutOfRange) => {
                        return Err(self.fail(
                            RuntimeErrorKind::InvalidProgram,
                            format!("plot slot {plot_slot} out of range"),
                            pc,
                        ))
                    }
                }
            }

            CsilInstr::PlotStyledF64 {
                plot_slot,
                color_count,
            } => {
                let mut colors = Vec::with_capacity(*color_count as usize);
                for _ in 0..*color_count {
                    let color = self.pop(pc)?;
                    if !matches!(
                        color,
                        Value::Color { .. } | Value::Na | Value::DisplacedTable(_)
                    ) {
                        return Err(self.fail(
                            RuntimeErrorKind::TypeMismatch,
                            format!("plot color requires a color, found {}", color.type_name()),
                            pc,
                        ));
                    }
                    colors.push(color);
                }
                colors.reverse();
                let value = self.pop(pc)?;
                if !matches!(
                    value,
                    Value::Float(_) | Value::Na | Value::DisplacedTable(_)
                ) {
                    return Err(self.fail(
                        RuntimeErrorKind::TypeMismatch,
                        format!("plot requires a float, found {}", value.type_name()),
                        pc,
                    ));
                }
                let plot = *plot_slot as usize;
                let bar = self.bar;
                let Some(cell) = self
                    .plots
                    .get_mut(plot)
                    .and_then(|series| series.get_mut(bar))
                else {
                    return Err(self.fail(
                        RuntimeErrorKind::InvalidProgram,
                        format!("plot slot {plot_slot} out of range"),
                        pc,
                    ));
                };
                *cell = value;
                let Some(expected_colors) = self.plot_colors.get(plot).map(Vec::len) else {
                    return Err(self.fail(
                        RuntimeErrorKind::InvalidProgram,
                        format!("plot color slot {plot_slot} out of range"),
                        pc,
                    ));
                };
                if expected_colors != colors.len() {
                    return Err(self.fail(
                        RuntimeErrorKind::InvalidProgram,
                        format!(
                            "plot slot {plot_slot} expected {} color channel(s), got {}",
                            expected_colors,
                            colors.len()
                        ),
                        pc,
                    ));
                }
                let channels = &mut self.plot_colors[plot];
                for (channel, color) in channels.iter_mut().zip(colors) {
                    channel[bar] = color;
                }
            }

            CsilInstr::VisualColor {
                visual_slot,
                color_count,
            } => {
                let mut colors = Vec::with_capacity(*color_count as usize);
                for _ in 0..*color_count {
                    let color = self.pop(pc)?;
                    if !matches!(
                        color,
                        Value::Color { .. } | Value::Na | Value::DisplacedTable(_)
                    ) {
                        return Err(self.fail(
                            RuntimeErrorKind::TypeMismatch,
                            format!("chart visual requires a color, found {}", color.type_name()),
                            pc,
                        ));
                    }
                    colors.push(color);
                }
                colors.reverse();
                let visual = *visual_slot as usize;
                let Some(expected_colors) = self.visual_colors.get(visual).map(Vec::len) else {
                    return Err(self.fail(
                        RuntimeErrorKind::InvalidProgram,
                        format!("visual slot {visual_slot} out of range"),
                        pc,
                    ));
                };
                if expected_colors != colors.len() {
                    return Err(self.fail(
                        RuntimeErrorKind::InvalidProgram,
                        format!(
                            "visual slot {visual_slot} expected {} color channel(s), got {}",
                            expected_colors,
                            colors.len()
                        ),
                        pc,
                    ));
                }
                let channels = &mut self.visual_colors[visual];
                let bar = self.bar;
                for (channel, color) in channels.iter_mut().zip(colors) {
                    channel[bar] = color;
                }
            }

            CsilInstr::FillColor {
                fill_slot,
                color_count,
            } => {
                self.exec_fill_color(*fill_slot, *color_count, pc)?;
            }

            CsilInstr::RuntimeError => {
                self.exec_runtime_error(pc)?;
            }

            CsilInstr::Return => return Ok(vm_dispatch::Flow::Return),
        }
        Ok(vm_dispatch::Flow::Next)
    }

    // ---- realtime driver -------------------------------------------------

    /// Snapshot all non-`varip` state that a realtime tick may mutate, taken
    /// at the realtime bar's start. `varip` slots are excluded so they persist
    /// across ticks.
    /// Build the `drawings` DTO: for each registered kind, serialize its live
    /// objects (creation order) by reading each handle's `HeapObj::Object`
    /// fields and mapping them to the kind's declared member names. A handle
    /// whose heap object is missing or of the wrong shape is skipped (defense
    /// in depth; a validated program never produces one).
    /// Serialize table `h`'s out-of-line cell grid into DTO cells
    /// in `(column, row)` order (the `BTreeMap` key order). Empty cells are
    /// already absent from the grid, so every returned cell has at least one
    /// attribute or the merged flag.
    fn drawing_wire_value(
        domain: Option<pinewasm_hir::drawing_options::DrawingOptionDomain>,
        value: &Value,
    ) -> Value {
        if let (Some(domain), Value::String(string)) = (domain, value) {
            if let Some(canonical) = domain.canonical_value(string) {
                return Value::String(canonical.to_string());
            }
        }
        value.clone()
    }

    fn serialize_table_cells(&self, h: HandleId) -> Vec<TableCellDto> {
        let Some(grid) = self.drawings.table_cells.get(&h.0) else {
            return Vec::new();
        };
        grid.cells
            .iter()
            .filter(|(_, cell)| !cell.is_empty())
            .map(|(&(column, row), cell)| {
                let mut attrs = std::collections::BTreeMap::new();
                for attr in CellAttr::all() {
                    if let Some(v) = cell.get(*attr) {
                        attrs.insert(
                            attr.name().to_string(),
                            Self::drawing_wire_value(
                                pinewasm_hir::drawing_options::cell_option_domain(*attr),
                                v,
                            ),
                        );
                    }
                }
                TableCellDto {
                    column,
                    row,
                    merged: cell.merged,
                    merge: cell.merge,
                    attrs,
                }
            })
            .collect()
    }

    /// Inline a polyline's `points` array: dereference the
    /// `array<chart.point>` handle and each point handle into
    /// `(index, time, price)` DTOs, since `Value::Ref` serializes as `null`.
    /// Slot order follows `POINT_FIELDS` (time, index, price), which is the
    /// point record's own field order and not the DTO's field order. A missing
    /// or malformed handle contributes no vertex (defense in depth; a validated
    /// program never produces one).
    fn serialize_polyline_points(&self, points: &Value) -> Vec<PolyPointDto> {
        let Value::Ref(handle) = points else {
            return Vec::new();
        };
        let Some(HeapObj::Array { items, .. }) = self.heap.get(*handle) else {
            return Vec::new();
        };
        items
            .iter()
            .filter_map(|item| {
                let Value::Ref(point) = item else {
                    return None;
                };
                let Some(HeapObj::Object { fields, .. }) = self.heap.get(*point) else {
                    return None;
                };
                Some(PolyPointDto {
                    time: fields.first().cloned().unwrap_or(Value::Na),
                    index: fields.get(1).cloned().unwrap_or(Value::Na),
                    price: fields.get(2).cloned().unwrap_or(Value::Na),
                })
            })
            .collect()
    }

    fn serialize_drawings(&self) -> Drawings {
        let mut out = Drawings::default();
        // The ids of the live lines by handle, for a linefill's references.
        let line_ids: std::collections::BTreeMap<HandleId, u32> =
            DrawingRegistry::slot_of(DrawKind::Line)
                .map(|slot| {
                    self.drawings.live[slot]
                        .iter()
                        .copied()
                        .zip(self.drawings.ids[slot].iter().copied())
                        .collect()
                })
                .unwrap_or_default();
        for slot in 0..self.drawings.live.len() {
            let kind = DrawingRegistry::kind_of(slot);
            let schema = kind.fields();
            let mut group = Vec::new();
            for (&h, &id) in self.drawings.live[slot]
                .iter()
                .zip(&self.drawings.ids[slot])
            {
                let Some(HeapObj::Object { fields, .. }) = self.heap.get(h) else {
                    continue;
                };
                let mut map = std::collections::BTreeMap::new();
                for (i, field) in schema.iter().enumerate() {
                    // A reference member (a linefill's lines, a polyline's
                    // points array) is `na` here, which is what it already
                    // was on the wire: its handle is private to the engine
                    // that allocated it and names nothing a host can look up.
                    // `line_refs` and `points` below carry what it refers to.
                    match fields.get(i) {
                        Some(Value::Ref(_)) => {
                            map.insert(field.name.to_string(), Value::Na);
                        }
                        Some(v) => {
                            map.insert(
                                field.name.to_string(),
                                Self::drawing_wire_value(
                                    pinewasm_hir::drawing_options::drawing_option_domain(
                                        kind, field.name,
                                    ),
                                    v,
                                ),
                            );
                        }
                        None => {}
                    }
                }
                // Tables carry their out-of-line cell grid;
                // other kinds have none. Serialize populated cells in
                // (column, row) order.
                let cells = if kind == DrawKind::Table {
                    self.serialize_table_cells(h)
                } else {
                    Vec::new()
                };
                // Polylines carry their vertices out of line the same way: the
                // `points` field itself is an array handle (serialized `null`),
                // so inline the dereferenced chart.points for hosts.
                let points = if kind == DrawKind::Polyline {
                    schema
                        .iter()
                        .position(|field| field.name == "points")
                        .and_then(|slot| fields.get(slot))
                        .map(|v| self.serialize_polyline_points(v))
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                // A linefill's line handles are `na` in the ordinary fields
                // map. Inline the drawing ids of the lines they name so hosts
                // can render the filled region. A line that is no longer live
                // (deleted, or evicted by the live cap) is not in the output,
                // so its reference is `None`.
                let line_refs = if kind == DrawKind::Linefill {
                    let line_id = |name: &str| {
                        schema
                            .iter()
                            .position(|field| field.name == name)
                            .and_then(|index| fields.get(index))
                            .and_then(Value::as_ref)
                            .and_then(|handle| line_ids.get(&handle).copied())
                    };
                    Some([line_id("line1"), line_id("line2")])
                } else {
                    None
                };
                group.push(DrawingObject {
                    kind: kind.namespace().to_string(),
                    id,
                    fields: map,
                    cells,
                    points,
                    line_refs,
                });
            }
            match kind {
                DrawKind::Line => out.lines = group,
                DrawKind::Label => out.labels = group,
                DrawKind::Box => out.boxes = group,
                DrawKind::Table => out.tables = group,
                DrawKind::Polyline => out.polylines = group,
                DrawKind::Linefill => out.linefills = group,
                DrawKind::Point => {}
            }
        }
        out
    }

    fn snapshot(&self, bar: usize) -> StateSnapshot {
        #[cfg(test)]
        if vm_snapshot::reference_mode() {
            return self.snapshot_reference(bar);
        }
        // varip cells are NOT captured: they survive rollback. The kernel
        // stores a placeholder `Na` so indices line up; restore skips them.
        let base = vm_rollback::snapshot_state(
            &self.program.series_slots,
            self.slot_mask,
            bar,
            &self.user_slots,
            &self.written,
            &self.plots,
            &self.plot_colors,
            &self.visual_colors,
            &self.fill_colors,
            &Value::Na,
            &self.ta_state,
            self.random_draws,
            &self.heap,
            &self.drawings,
            &self.logs,
            &self.alerts,
            &self.alert_fired_sites,
        );
        StateSnapshot {
            base,
            dynamic_ema: self.dynamic_ema.clone(),
            sma: self.sma.clone(),
            pivot_states: self.pivot_states.clone(),
        }
    }

    /// Restore the bar-start snapshot (rollback) before the next tick on the
    /// same realtime bar. `varip` slots (and their written flags) are left
    /// untouched — they persist across ticks.
    fn restore(&mut self, bar: usize, snap: &StateSnapshot) -> Result<(), RuntimeError> {
        // A rollback can replace an appended row without changing its handle or length.
        self.distance_cache = distance_scan::Cache::default();
        #[cfg(test)]
        if vm_snapshot::reference_mode() {
            // Run the direct restore body while applying
            // the cumulative-log policy outside it. Differential runs
            // still compare complete outcomes, including exact log events.
            let cumulative_logs = self.logs.clone();
            self.restore_reference(bar, snap);
            let () = assert_eq!(
                self.logs,
                cumulative_logs[..snap.base.logs_len.min(cumulative_logs.len())],
                "restore must only truncate the exact log prefix"
            );
            self.logs = cumulative_logs;
            self.restore_historical_aliases();
            return Ok(());
        }
        // Prepare the merged heap and every root rewrite before changing any
        // VM field. A dangling edge or budget failure therefore leaves the
        // pre-restore state intact. `varip` references survive rollback, and
        // the objects reachable through them keep their state:
        // only that graph is merged into the bar-start heap
        // ([`pinewasm_mir::heap::restore_varip_heap`]).
        let (restored_heap, remapped_roots) = pinewasm_mir::heap::restore_varip_heap(
            &snap.base.heap,
            &self.heap,
            &vm_rollback::varip_mask(&self.program.series_slots),
            &self.user_slots,
        )
        .map_err(|error| self.heap_restore_error(bar, error))?;
        vm_rollback::restore_state(
            &self.program.series_slots,
            self.slot_mask,
            bar,
            &mut self.user_slots,
            &mut self.written,
            &mut self.plots,
            &mut self.plot_colors,
            &mut self.visual_colors,
            &mut self.fill_colors,
            &mut self.ta_state,
            &mut self.random_draws,
            &mut self.heap,
            &mut self.drawings,
            &mut self.alerts,
            &mut self.alert_fired_sites,
            &snap.base,
            restored_heap,
        );
        self.dynamic_ema.clone_from(&snap.dynamic_ema);
        self.sma.clone_from(&snap.sma);
        self.pivot_states.clone_from(&snap.pivot_states);
        // `restore_state` leaves `varip` cells alone; point their handles at
        // the rebuilt heap.
        pinewasm_mir::heap::rewrite_varip_cells(&mut self.user_slots, &remapped_roots);
        self.restore_historical_aliases();
        Ok(())
    }

    /// Restore prior-bar slot cells rewritten by table deletion during the
    /// active realtime bar. Current cells follow the existing snapshot rules,
    /// including `varip` persistence; this journal only holds non-current
    /// history cells changed by that side effect.
    fn restore_historical_aliases(&mut self) {
        let (user_slots, undo) = (&mut self.user_slots, &self.realtime_alias_undo);
        for (&(slot, index), value) in undo {
            if let Some(cell) = user_slots
                .get_mut(slot)
                .and_then(|history| history.get_mut(index))
            {
                *cell = value.clone();
            }
        }
    }

    fn heap_restore_error(&self, bar: usize, error: HeapRestoreError) -> RuntimeError {
        let kind = match error {
            HeapRestoreError::Limit(_) => RuntimeErrorKind::CollectionLimitExceeded,
            HeapRestoreError::DanglingReference(_)
            | HeapRestoreError::MismatchedLimits
            | HeapRestoreError::ArithmeticOverflow => RuntimeErrorKind::InvalidProgram,
        };
        RuntimeError {
            kind,
            message: format!("realtime heap rollback failed: {error}"),
            bar_index: Some(bar as u32),
            instruction_offset: None,
            span: None,
        }
    }

    /// The raw `snapshot` body, kept for comparison tests.
    #[cfg(test)]
    fn snapshot_reference(&self, bar: usize) -> StateSnapshot {
        let mut slot_cells = Vec::with_capacity(self.user_slots.len());
        for (slot, def) in self.program.series_slots.iter().enumerate() {
            if def.varip {
                slot_cells.push(Value::Na);
            } else {
                slot_cells.push(self.user_slots[slot][bar & self.slot_mask].clone());
            }
        }
        let base = vm_rollback::Snapshot {
            cells: vm_snapshot::CellSnapshot {
                slots: vm_snapshot::SlotCells {
                    cells: slot_cells,
                    written: self.written.clone(),
                },
                plot_cells: self.plots.iter().map(|p| p[bar].clone()).collect(),
                plot_color_cells: self
                    .plot_colors
                    .iter()
                    .map(|channels| channels.iter().map(|values| values[bar].clone()).collect())
                    .collect(),
                visual_color_cells: self
                    .visual_colors
                    .iter()
                    .map(|channels| channels.iter().map(|values| values[bar].clone()).collect())
                    .collect(),
                fill_color_cells: self
                    .fill_colors
                    .iter()
                    .map(|channels| channels.iter().map(|values| values[bar].clone()).collect())
                    .collect(),
            },
            ta_state: self.ta_state.clone(),
            random_draws: self.random_draws,
            heap: self.heap.clone(),
            drawings: self.drawings.clone(),
            logs_len: self.logs.len(),
            alerts_len: self.alerts.len(),
            alert_fired_sites: self.alert_fired_sites.clone(),
        };
        StateSnapshot {
            base,
            dynamic_ema: self.dynamic_ema.clone(),
            sma: self.sma.clone(),
            pivot_states: self.pivot_states.clone(),
        }
    }

    /// The raw `restore` body, kept for comparison tests.
    #[cfg(test)]
    fn restore_reference(&mut self, bar: usize, snap: &StateSnapshot) {
        vm_snapshot::note_reference_restore();
        let cells = &snap.base.cells;
        for (slot, def) in self.program.series_slots.iter().enumerate() {
            if def.varip {
                continue;
            }
            self.user_slots[slot][bar & self.slot_mask] = cells.slots.cells[slot].clone();
            self.written[slot] = cells.slots.written[slot];
        }
        for (plot, cell) in cells.plot_cells.iter().enumerate() {
            self.plots[plot][bar] = cell.clone();
        }
        for (plot, channels) in cells.plot_color_cells.iter().enumerate() {
            for (channel, cell) in channels.iter().enumerate() {
                self.plot_colors[plot][channel][bar] = cell.clone();
            }
        }
        for (visual, channels) in cells.visual_color_cells.iter().enumerate() {
            for (channel, cell) in channels.iter().enumerate() {
                self.visual_colors[visual][channel][bar] = cell.clone();
            }
        }
        for (fill, channels) in cells.fill_color_cells.iter().enumerate() {
            for (channel, cell) in channels.iter().enumerate() {
                self.fill_colors[fill][channel][bar] = cell.clone();
            }
        }
        self.ta_state = snap.base.ta_state.clone();
        self.dynamic_ema.clone_from(&snap.dynamic_ema);
        self.sma.clone_from(&snap.sma);
        self.pivot_states.clone_from(&snap.pivot_states);
        self.random_draws = snap.base.random_draws;
        self.heap = snap.base.heap.clone();
        self.drawings = snap.base.drawings.clone();
        self.logs.truncate(snap.base.logs_len);
        self.alerts.truncate(snap.base.alerts_len);
        self.alert_fired_sites
            .clone_from(&snap.base.alert_fired_sites);
    }

    /// Schedule the additional execution required when an immediate close
    /// fills during `run_bar()` and `calc_on_order_fills` is enabled. Broker
    /// state intentionally survives rollback; script/plot/drawing state is
    /// restored to the bar snapshot before each fill-triggered recalculation.
    #[allow(clippy::too_many_arguments)]
    fn recalculate_after_immediate_fills(
        &mut self,
        bar: usize,
        snap: Option<&StateSnapshot>,
        _open: f64,
        _high: f64,
        _low: f64,
        close: f64,
    ) -> Result<(), RuntimeError> {
        loop {
            let recalculate = self.broker.as_mut().is_some_and(|broker| {
                let fills = broker.take_immediate_fill_count();
                fills > 0 && broker.calc_on_order_fills()
            });
            if !recalculate {
                return Ok(());
            }
            let Some(snap) = snap else {
                return Ok(());
            };
            self.restore(bar, snap)?;
            if let Some(broker) = self.broker.as_mut() {
                // An immediate order did not exist earlier in this bar. Mark
                // the resulting position at its actual fill tick only.
                broker.record_snapshot(bar, close, close, close, close);
            }
            self.run_bar()?;
        }
    }

    /// Execute one historical strategy bar as the broker emulator's four
    /// ordered OHLC ticks. Script state rolls back to the bar-open snapshot
    /// between executions (except `varip`), while broker orders/fills persist.
    /// This realizes `calc_on_order_fills` and the
    /// `calc_on_every_history_tick` flag without exposing future high/low data
    /// to an earlier tick.
    fn run_strategy_history_bar(&mut self, bar: usize) -> Result<(), RuntimeError> {
        let (open, high, low, close, volume) = (
            self.data.open[bar],
            self.data.high[bar],
            self.data.low[bar],
            self.data.close[bar],
            self.data.volume[bar],
        );
        // Visit the extreme nearer the open first (PineWASM path model).
        let high_first = (high - open).abs() <= (open - low).abs();
        let path = if high_first {
            [open, high, low, close]
        } else {
            [open, low, high, close]
        };
        // Close-only historical scripts never execute
        // before the closing vertex, so there is no script state to roll back.
        // Keep the full path for order-fill or every-history-tick recalculation.
        let needs_rollback = self.broker.as_ref().is_some_and(|broker| {
            broker.calc_on_every_history_tick() || broker.calc_on_order_fills()
        });
        let bar_snapshot = needs_rollback.then(|| self.snapshot(bar));
        let mut running_high = open;
        let mut running_low = open;
        let mut previous_price = open;

        for (tick_index, price) in path.into_iter().enumerate() {
            if tick_index > 0 {
                if let Some(snapshot) = bar_snapshot.as_ref() {
                    self.restore(bar, snapshot)?;
                }
            }
            running_high = running_high.max(price);
            running_low = running_low.min(price);
            // The broker consumes the path prices directly. Only script
            // execution observes the feed, so close-only strategies need no
            // intermediate OHLCV writes. Keep the closing write even for
            // those strategies: it preserves the path-derived extrema when
            // supplied OHLC values are not internally ordered.
            if needs_rollback || tick_index == 3 {
                self.data.high[bar] = running_high;
                self.data.low[bar] = running_low;
                self.data.close[bar] = price;
                self.data.volume[bar] = volume * (tick_index as f64 + 1.0) / 4.0;
            }
            self.bar_is_new = tick_index == 0;
            self.bar_is_confirmed = tick_index == 3;

            let fills = self.broker.as_mut().map_or(0, |broker| {
                // Historical bars model a continuous intrabar path between
                // adjacent OHLC vertices.  Use the previous vertex as the
                // segment open so crossed stop/limit orders fill at their
                // trigger price; passing `price` for every field incorrectly
                // treated each vertex as a gap and filled at the extreme.
                broker.process_fills(
                    bar,
                    previous_price,
                    previous_price.max(price),
                    previous_price.min(price),
                    price,
                    false,
                )
            });
            if let Some(broker) = self.broker.as_mut() {
                // Each historical call represents one available OHLC vertex.
                // Existing lots accumulate the path vertex by vertex, while a
                // lot opened on this segment cannot inherit earlier extrema.
                broker.observe_market(bar, price, price, price, price);
            }
            let (every_history_tick, on_fill) =
                self.broker.as_ref().map_or((false, false), |broker| {
                    (
                        broker.calc_on_every_history_tick(),
                        broker.calc_on_order_fills(),
                    )
                });
            let regular_execution = every_history_tick || tick_index == 3;
            if regular_execution || (fills > 0 && on_fill) {
                if let Some(broker) = self.broker.as_mut() {
                    broker.capture_script_snapshot(bar);
                }
            }

            // A fill-triggered calculation and the normal close-tick
            // calculation are distinct events when both apply.
            if fills > 0 && on_fill {
                // A historical bar is recalculated after a fill with
                // the whole bar's prices, not the path up to the fill: an
                // exit placed from `ta.atr(14)` after a fill at the open uses
                // the bar's own high and low.
                // Orders it places still fill only along the rest of the path.
                self.data.high[bar] = high;
                self.data.low[bar] = low;
                self.data.close[bar] = close;
                self.data.volume[bar] = volume;
                // An immediate order placed there still fills at the tick.
                self.fill_recalculation_price = Some(price);
                let recalculated = self.run_bar().and_then(|()| {
                    self.recalculate_after_immediate_fills(
                        bar,
                        bar_snapshot.as_ref(),
                        open,
                        running_high,
                        running_low,
                        price,
                    )
                });
                self.fill_recalculation_price = None;
                recalculated?;
                if regular_execution {
                    if let Some(snapshot) = bar_snapshot.as_ref() {
                        self.restore(bar, snapshot)?;
                    }
                    self.data.high[bar] = running_high;
                    self.data.low[bar] = running_low;
                    self.data.close[bar] = price;
                    self.data.volume[bar] = volume * (tick_index as f64 + 1.0) / 4.0;
                    if let Some(broker) = self.broker.as_mut() {
                        broker.record_snapshot(bar, price, price, price, price);
                    }
                    self.run_bar()?;
                    self.recalculate_after_immediate_fills(
                        bar,
                        bar_snapshot.as_ref(),
                        open,
                        running_high,
                        running_low,
                        price,
                    )?;
                }
            } else if regular_execution {
                self.run_bar()?;
                self.recalculate_after_immediate_fills(
                    bar,
                    bar_snapshot.as_ref(),
                    open,
                    running_high,
                    running_low,
                    price,
                )?;
            }

            if tick_index == 3 {
                let (process_close, on_fill) =
                    self.broker.as_ref().map_or((false, false), |broker| {
                        (broker.process_on_close(), broker.calc_on_order_fills())
                    });
                if process_close {
                    let close_fills = self.broker.as_mut().map_or(0, |broker| {
                        broker.process_fills(bar, close, close, close, close, true)
                    });
                    if close_fills > 0 && on_fill {
                        if let Some(snapshot) = bar_snapshot.as_ref() {
                            self.restore(bar, snapshot)?;
                        }
                        if let Some(broker) = self.broker.as_mut() {
                            broker.record_snapshot(bar, close, close, close, close);
                        }
                        self.run_bar()?;
                        self.recalculate_after_immediate_fills(
                            bar,
                            bar_snapshot.as_ref(),
                            open,
                            high,
                            low,
                            close,
                        )?;
                    }
                }
            }
            previous_price = price;
        }

        self.data.high[bar] = high;
        self.data.low[bar] = low;
        self.data.close[bar] = close;
        self.data.volume[bar] = volume;
        if let Some(broker) = self.broker.as_mut() {
            broker.record_equity(bar, close);
        }
        Ok(())
    }

    /// Replay the realtime tick segment on the last bar(s). Returns the
    /// optional per-tick plot trace. Updates `bars_executed` when a tick
    /// appends and commits a new bar. See docs/runtime.md.
    fn run_realtime(
        &mut self,
        sim: &RealtimeSim,
        collect_trace: bool,
        bars_executed: &mut u32,
    ) -> Result<Option<Vec<PlotTickSample>>, RuntimeError> {
        let mut trace: Option<Vec<PlotTickSample>> = collect_trace.then(Vec::new);
        // Move the driver out so its snapshot can be borrowed while `self`
        // mutates. It is restored even when a tick returns a structured error.
        let mut driver = std::mem::take(&mut self.realtime_driver);
        let result = (|| {
            for tick in &sim.ticks {
                let ordinal = driver.next_tick_ordinal;
                let bar = tick.bar_index;
                // Is this the first tick landing on `bar`? Once a bar is open,
                // subsequent ticks on it are re-runs (roll back to its start).
                // The first tick on a bar either re-runs the last committed
                // bar or appends the next; validated against the pre-open
                // `self.bars` so a mid-bar recompute cannot flip the decision.
                let Some(start) = vm_tick::begin_tick(&mut driver, self.bars, bar) else {
                    let last_committed = self.bars.saturating_sub(1);
                    let message = match driver.open_bar {
                        Some(open) => format!(
                            "realtime tick {ordinal} targets bar {bar} but bar {open} is still open; only bar {open} is valid until a closing tick commits it"
                        ),
                        None => format!(
                            "realtime tick {ordinal} targets bar {bar} but only bar {last_committed} (re-run) or {} (append) are valid",
                            self.bars
                        ),
                    };
                    return Err(RuntimeError {
                        kind: RuntimeErrorKind::InvalidProgram,
                        message,
                        bar_index: Some(bar as u32),
                        instruction_offset: None,
                        span: None,
                    });
                };
                let first_tick_on_bar = start != vm_tick::TickStart::Rollback;

                if first_tick_on_bar {
                    let opening_new_bar = start == vm_tick::TickStart::Append;
                    if opening_new_bar {
                        self.append_bar(tick.time, tick.trading_day)?;
                    }
                    // Carry-forward at the start of a realtime bar, exactly like a
                    // historical bar: persistent slots inherit the prior bar's
                    // final value. (Skipped for bar 0, which has no predecessor.)
                    //
                    // `varip` is the exception: it persists across the whole
                    // session and is excluded from the snapshot/restore path, so
                    // it must NOT be re-carried when the last committed bar is
                    // *re-run*, which would clobber its established session value.
                    // Only carry a varip slot forward on a genuine append, where
                    // its fresh cell is `na`.
                    let cur = bar & self.slot_mask;
                    let prev = bar.wrapping_sub(1) & self.slot_mask;
                    if opening_new_bar {
                        // A genuinely fresh bar, so its ring cell still holds
                        // bar - ring and must be cleared or carried, exactly
                        // like a historical bar.
                        for (slot, def) in self.program.series_slots.iter().enumerate() {
                            self.user_slots[slot][cur] = if def.persistent && bar > 0 {
                                self.user_slots[slot][prev].clone()
                            } else {
                                Value::Na
                            };
                        }
                        // Same collection point as the historical loop, for the
                        // same reason: an incremental session runs up to
                        // MAX_INCREMENTAL_BARS appended bars, so a per-bar
                        // collection would otherwise stay charged for the
                        // session the way it used to for a chart. Only on a
                        // genuine append: re-running the last committed bar is
                        // owned by the per-tick snapshot, whose heap clone must
                        // not be collected out from under it.
                        self.maybe_collect_heap();
                    } else if bar > 0 {
                        // Re-running the last committed bar: the cell holds
                        // that bar's own committed state, which the per-tick
                        // snapshot/restore owns. Carry forward, clear nothing.
                        for (slot, def) in self.program.series_slots.iter().enumerate() {
                            if def.persistent && !def.varip {
                                self.user_slots[slot][cur] = self.user_slots[slot][prev].clone();
                            }
                        }
                    }
                    // A newly opened realtime bar resets the once-per-bar alert
                    // dedup, captured in the snapshot so per-tick rollback restores
                    // it consistently.
                    self.alert_fired_sites.clear();
                    self.realtime_alias_undo.clear();
                    vm_tick::open_bar(&mut driver, bar, self.snapshot(bar));
                } else if let Some(snap) = &driver.bar_snapshot {
                    self.restore(bar, snap)?;
                }

                // Overwrite the bar's OHLCV with the tick snapshot.
                self.data.open[bar] = tick.open;
                self.data.high[bar] = tick.high;
                self.data.low[bar] = tick.low;
                self.data.close[bar] = tick.close;
                self.data.volume[bar] = tick.volume;

                // The feed just changed (an appended bar, or this bar's OHLCV
                // overwritten by the tick), so the `request.security` back-mapped
                // memos, computed once over the historical feed length, are stale
                // or too short. Clear them so the next read recomputes over the
                // current data (including the appended/updated bar); otherwise an
                // appended bar reads `na` and a re-run reads the pre-tick value.
                self.security_memo.clear();
                self.security_expr_memo.clear();

                // Set the driver/barstate context and execute once.
                self.bar = bar;
                self.realtime_active = true;
                self.realtime_bar = Some(bar);
                self.bar_is_new = first_tick_on_bar;
                self.bar_is_confirmed = tick.is_bar_close;
                self.sync_broker_currency_rate(bar, true);
                if first_tick_on_bar {
                    let key = strategy_risk_session_key(&self.data, bar);
                    if let Some(broker) = self.broker.as_mut() {
                        broker.begin_session(key, bar, tick.open);
                    }
                }

                let fills = self.broker.as_mut().map_or(0, |broker| {
                    broker.process_fills(bar, tick.close, tick.close, tick.close, tick.close, false)
                });
                if let Some(broker) = self.broker.as_mut() {
                    // Realtime OHLC fields are cumulative snapshots. A newly filled
                    // lot starts at this available tick, not at an earlier high/low.
                    broker.record_snapshot(bar, tick.close, tick.close, tick.close, tick.close);
                }
                let is_strategy = self.broker.is_some();
                let (every_tick, on_fill) = self.broker.as_ref().map_or((true, false), |broker| {
                    (broker.calc_on_every_tick(), broker.calc_on_order_fills())
                });
                let regular_execution = !is_strategy || every_tick || tick.is_bar_close;
                if fills > 0 && on_fill {
                    self.run_bar()?;
                    if let Some(snap) = &driver.bar_snapshot {
                        self.recalculate_after_immediate_fills(
                            bar,
                            Some(snap),
                            tick.open,
                            tick.high,
                            tick.low,
                            tick.close,
                        )?;
                    }
                    if regular_execution {
                        if let Some(snap) = &driver.bar_snapshot {
                            self.restore(bar, snap)?;
                        }
                        if let Some(broker) = self.broker.as_mut() {
                            broker.record_snapshot(
                                bar, tick.close, tick.close, tick.close, tick.close,
                            );
                        }
                        self.run_bar()?;
                        if let Some(snap) = &driver.bar_snapshot {
                            self.recalculate_after_immediate_fills(
                                bar,
                                Some(snap),
                                tick.open,
                                tick.high,
                                tick.low,
                                tick.close,
                            )?;
                        }
                    }
                } else if regular_execution {
                    self.run_bar()?;
                    if let Some(snap) = &driver.bar_snapshot {
                        self.recalculate_after_immediate_fills(
                            bar,
                            Some(snap),
                            tick.open,
                            tick.high,
                            tick.low,
                            tick.close,
                        )?;
                    }
                }

                if tick.is_bar_close && is_strategy {
                    let process_close = self
                        .broker
                        .as_ref()
                        .is_some_and(broker::Broker::process_on_close);
                    if process_close {
                        let close_fills = self.broker.as_mut().map_or(0, |broker| {
                            broker.process_fills(
                                bar, tick.close, tick.close, tick.close, tick.close, true,
                            )
                        });
                        if close_fills > 0 && on_fill {
                            if let Some(snap) = &driver.bar_snapshot {
                                self.restore(bar, snap)?;
                            }
                            if let Some(broker) = self.broker.as_mut() {
                                broker.record_snapshot(
                                    bar, tick.close, tick.close, tick.close, tick.close,
                                );
                            }
                            self.run_bar()?;
                            if let Some(snap) = &driver.bar_snapshot {
                                self.recalculate_after_immediate_fills(
                                    bar,
                                    Some(snap),
                                    tick.open,
                                    tick.high,
                                    tick.low,
                                    tick.close,
                                )?;
                            }
                        }
                    }
                    if let Some(broker) = self.broker.as_mut() {
                        broker.record_equity(bar, tick.close);
                    }
                }

                if let Some(trace) = trace.as_mut() {
                    trace.push(PlotTickSample {
                        tick_ordinal: ordinal,
                        bar_index: bar as u32,
                        is_bar_close: tick.is_bar_close,
                        values: self.plots.iter().map(|p| p[bar].clone()).collect(),
                    });
                }
                // On bar-close: commit. The current cells become permanent; a new
                // bar (if this one was appended) is now part of committed history,
                // and the next tick opens a fresh bar.
                if vm_tick::finish_tick(&mut driver, tick.is_bar_close, bars_executed) {
                    // The bar is now history; clear the realtime frame so a
                    // subsequent tick opens the next bar cleanly.
                    self.realtime_bar = None;
                    self.realtime_active = false;
                    self.realtime_alias_undo.clear();
                }
            }
            Ok(trace)
        })();
        self.realtime_driver = driver;
        result
    }

    /// Append one bar to the working market data for a realtime tick that opens
    /// a new bar. OHLCV are seeded to the tick's close (overwritten right
    /// after) and the time axis is extended; every per-bar VM vector grows by
    /// one `na` cell.
    fn append_bar(&mut self, time: i64, trading_day: Option<i64>) -> Result<(), RuntimeError> {
        if !self.data.time_close.is_empty()
            || !self.data.request_time_close.is_empty()
            || !self.data.request_bars.is_empty()
        {
            return Err(RuntimeError {
                kind: RuntimeErrorKind::MalformedMarketData,
                message: "closing-time arrays require a complete history snapshot; rebuild before appending realtime bars".into(),
                bar_index: Some(self.bars as u32),
                instruction_offset: None,
                span: None,
            });
        }
        // Resolve and validate the new identity before growing any VM vector,
        // so malformed realtime input cannot leave a partially appended bar.
        let previous_time = self
            .data
            .time
            .last()
            .copied()
            .or_else(|| self.bars.checked_sub(1).map(|bar| self.data.time_at(bar)));
        if let Some(previous_time) = previous_time {
            if time <= previous_time {
                return Err(RuntimeError {
                    kind: RuntimeErrorKind::MalformedMarketData,
                    message: format!(
                        "realtime time array must be strictly increasing, found {previous_time} then {time}"
                    ),
                    bar_index: Some(self.bars as u32),
                    instruction_offset: None,
                    span: None,
                });
            }
        }
        let close_offset_ms = timeframe_in_seconds(&self.data.timeframe).checked_mul(1000);
        if close_offset_ms
            .and_then(|offset| time.checked_add(offset))
            .is_none()
        {
            return Err(RuntimeError {
                kind: RuntimeErrorKind::MalformedMarketData,
                message: format!(
                    "timeframe close time exceeds the i64 timestamp range at realtime bar {}: open {time}, timeframe `{}`",
                    self.bars, self.data.timeframe
                ),
                bar_index: Some(self.bars as u32),
                instruction_offset: None,
                span: None,
            });
        }
        let appended_trading_day =
            trading_day.unwrap_or_else(|| self.data.fallback_trading_day_for_time(time));
        let previous_trading_day = self.data.trading_day.last().copied().or_else(|| {
            self.bars
                .checked_sub(1)
                .map(|bar| self.data.trading_day_at(bar))
        });
        if previous_trading_day.is_some_and(|previous| appended_trading_day < previous) {
            return Err(RuntimeError {
                kind: RuntimeErrorKind::MalformedMarketData,
                message: format!(
                    "realtime tradingDay must be nondecreasing, found {} then {appended_trading_day}",
                    previous_trading_day.expect("checked above")
                ),
                bar_index: Some(self.bars as u32),
                instruction_offset: None,
                span: None,
            });
        }
        // One-shot runs reserve their complete realtime envelope in `Vm::new`.
        // Persistent sessions cannot know future ticks, so their actual feed
        // and output growth is admitted here, after validation and before the
        // first vector changes.
        self.reserve_realtime_append(trading_day.is_some())?;
        let materialized_time = self.data.time.is_empty().then(|| {
            let mut times = (0..self.bars)
                .map(|bar| self.data.time_at(bar))
                .collect::<Vec<_>>();
            times.push(time);
            times
        });
        // Grow OHLCV (values are overwritten by the caller immediately).
        self.data.open.push(0.0);
        self.data.high.push(0.0);
        self.data.low.push(0.0);
        self.data.close.push(0.0);
        self.data.volume.push(0.0);
        // Materialize a previously synthetic history on first append so the
        // host-supplied tick time becomes the new bar's observable open time.
        if let Some(times) = materialized_time {
            self.data.time = times;
        } else {
            self.data.time.push(time);
        }
        // Extend the trading-day lane only when the feed carries one or this
        // tick brings one. A feed without a lane stays without one while its
        // ticks omit `tradingDay`: `trading_day_at` already derives the
        // appended bar's day from its own time, and keeping the lane absent
        // keeps calendar buckets on the close date that one run over the
        // same bars uses. Materializing a lane from the open dates here would
        // regroup weekly and monthly buckets of history bars.
        //
        // A tick that does carry a day materializes the lane for the history
        // first, so it can provide an exact symbol-session day even when the
        // history relied on fallback values. An absent tick value is derived
        // from this tick's own time, never copied from the previous bar.
        // Decide before materializing: a zero-bar history materializes an
        // empty prefix, and the tick's day must still start the lane.
        let extends_lane = !self.data.trading_day.is_empty() || trading_day.is_some();
        if self.data.trading_day.is_empty() && trading_day.is_some() {
            self.data.trading_day = (0..self.bars)
                .map(|bar| self.data.trading_day_at(bar))
                .collect();
        }
        if extends_lane {
            self.data.trading_day.push(appended_trading_day);
        }
        // Grow every per-bar VM vector. Slot history is deliberately absent:
        // it is a fixed-size ring, so an appended bar reuses a cell rather
        // than adding one. The bar loop's preamble clears or carries that
        // cell before anything reads it.

        vm_layout::append_cells(&mut self.plots, Value::Na);
        for plot in &mut self.plot_colors {
            for channel in plot {
                channel.push(Value::Na);
            }
        }
        for visual in &mut self.visual_colors {
            for channel in visual {
                channel.push(Value::Na);
            }
        }
        for fill in &mut self.fill_colors {
            for channel in fill {
                channel.push(Value::Na);
            }
        }
        self.bars += 1;
        Ok(())
    }
}

#[cfg(test)]
mod initial_ta_budget_guard_tests {
    use super::*;

    // Initial state-budget comparison. The existing
    // arithmetic projection must preserve this guard at every usize value.
    fn reference_refuses(state_bytes: usize, limit: usize) -> bool {
        state_bytes > limit
    }

    fn compile(source: &str) -> CsilProgram {
        let parsed = pinewasm_parse::parse(source);
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        let mir = pinewasm_mir::lower(&analysis.hir);
        pinewasm_csil::lower_mir(&mir).expect("focused fixture lowers")
    }

    fn feed() -> MarketData {
        MarketData {
            open: vec![1.0; 128],
            high: vec![2.0; 128],
            low: vec![0.0; 128],
            close: vec![1.5; 128],
            volume: vec![10.0; 128],
            ..MarketData::default()
        }
    }

    #[test]
    fn zero_reservation_matches_frozen_guard_on_usize_boundary_grid() {
        use vm_memory_reservation::ReservationError;
        let edges = [
            0,
            1,
            2,
            7,
            8,
            39,
            40,
            127,
            128,
            511,
            512,
            100_000,
            usize::MAX / 2,
            usize::MAX / 2 + 1,
            usize::MAX - 1,
            usize::MAX,
        ];
        for state_bytes in edges {
            for limit in edges {
                let projected = vm_memory_reservation::project(0, state_bytes, 0, limit);
                assert_eq!(projected.is_err(), reference_refuses(state_bytes, limit));
                assert_eq!(
                    projected,
                    if state_bytes <= limit {
                        Ok(state_bytes)
                    } else {
                        Err(ReservationError::LimitExceeded(state_bytes))
                    },
                    "state_bytes={state_bytes}, limit={limit}"
                );
            }
        }
    }

    #[test]
    fn actual_constructor_preserves_exact_cap_refusal_and_reserved_counter() {
        for source in [
            "indicator(\"guard\")\nplot(close)\n",
            "indicator(\"guard-ta\")\nplot(ta.ema(close, 3))\n",
            "indicator(\"guard-sma\")\nplot(ta.sma(close, 3))\n",
        ] {
            let program = compile(source);
            if source.contains("guard-ta") {
                assert!(
                    !program.ta_states.is_empty(),
                    "fixture reaches TA allocation"
                );
            }
            let data = feed();
            let defaults = RunOptions::default();
            let bytes = vm_preallocation_bytes(
                &program,
                &data,
                &defaults,
                data.bars(),
                program.history_ring() as usize,
            )
            .expect("bounded initial state");
            assert!(bytes > vm_program_bytes(&program).expect("program bytes"));

            for limit in [bytes, bytes + 1, usize::MAX] {
                let options = RunOptions {
                    max_vm_bytes_per_run: limit,
                    ..defaults.clone()
                };
                let vm = Vm::new(&program, &data, &options).expect("admitted initial state");
                assert_eq!(vm.bytes_reserved, bytes);
                assert_eq!(vm.transient_vm_bytes, 0);
            }

            for limit in [
                bytes - 1,
                vm_program_bytes(&program).expect("program bytes"),
            ] {
                let options = RunOptions {
                    max_vm_bytes_per_run: limit,
                    ..defaults.clone()
                };
                let error = match Vm::new(&program, &data, &options) {
                    Ok(_) => panic!("over-budget initial state must be refused"),
                    Err(error) => error,
                };
                assert_eq!(
                    error,
                    vm_memory_limit_error(limit, &format!("VM state requires {bytes} bytes"))
                );
            }
        }
    }

    #[test]
    fn selected_ta_input_uses_its_own_exact_state_bytes_and_keeps_input_errors() {
        let program =
            compile("indicator(\"guard-input\")\nn = input.int(3)\nplot(ta.ema(close, n))\nplot(ta.sma(close, n))\n");
        assert!(!program.ta_input_bindings.is_empty());
        let data = feed();
        let mut options = RunOptions::default();
        options.inputs.insert(0, InputValue::Int(7));
        let resolver = ta_input_install::ProgramInputs {
            program: &program,
            inputs: &options.inputs,
        };
        let mut values = HashMap::new();
        assert_eq!(
            ta_input_install::install(&program.ta_input_bindings, &resolver, &mut values),
            Ok(())
        );
        assert!(
            !values.is_empty(),
            "fixture reaches selected-parameter branch"
        );
        let ta_bytes = ta::TaStates::allocation_bytes_with_params(
            &program.ta_states,
            |state, param, default| ta_params::state_param_value(&values, state, param, default),
        )
        .expect("bounded selected TA bytes");
        let ring = vm_layout::ring_cells(program.history_depth_with_ta_inputs(&values))
            .expect("bounded selected history ring");
        let bytes = vm_preallocation_bytes_with_ta(
            &program,
            &data,
            &options,
            data.bars(),
            ring,
            ta_bytes,
            &values,
        )
        .expect("bounded selected state bytes");
        assert!(bytes > vm_program_bytes(&program).expect("program bytes"));

        options.max_vm_bytes_per_run = bytes;
        let vm = Vm::new(&program, &data, &options).expect("exact selected-state budget");
        assert_eq!(vm.bytes_reserved, bytes);
        options.max_vm_bytes_per_run = bytes - 1;
        let error = match Vm::new(&program, &data, &options) {
            Ok(_) => panic!("selected state must honor the one-byte-short budget"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            vm_memory_limit_error(bytes - 1, &format!("VM state requires {bytes} bytes"))
        );

        options.inputs.insert(0, InputValue::Int(0));
        let error = match Vm::new(&program, &data, &options) {
            Ok(_) => panic!("invalid TA input must precede initial state accounting"),
            Err(error) => error,
        };
        assert_eq!(error.kind, RuntimeErrorKind::InvalidTaLength);
        assert_eq!(error.bar_index, None);
        assert_eq!(error.instruction_offset, None);
        assert_eq!(error.span, None);
    }

    #[test]
    fn earlier_program_guard_keeps_its_exact_error_and_precedence() {
        let program = compile("indicator(\"guard\")\nplot(close)\n");
        let data = feed();
        let limit = vm_program_bytes(&program).expect("program bytes") - 1;
        let options = RunOptions {
            max_vm_bytes_per_run: limit,
            ..RunOptions::default()
        };
        let error = match Vm::new(&program, &data, &options) {
            Ok(_) => panic!("program clone guard must be first"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            vm_memory_limit_error(limit, "program clone exceeds the memory budget")
        );
    }
}

/// The persistent Vm state an AOT host run keeps between `host.vm` calls
/// (docs/aot.md §5). The AOT module owns lanes, slots, and control flow;
/// everything the interpreter owns — the collection heap, drawings, event
/// streams, request memoization, the PRNG counter — lives here and is
/// swapped into a real [`Vm`] for each hosted instruction, so hosted
/// opcodes execute the interpreter's own code paths on the interpreter's
/// own state.
pub(crate) struct AotVmState {
    data: MarketData,
    stack: Vec<Value>,
    call_args: Vec<Value>,
    random_draws: u64,
    last_table_identity: u64,
    heap: Heap,
    distance_cache: distance_scan::Cache,
    // Live cache high-water reservation, retained across rollback/rebuilds.
    distance_cache_reserved_bytes: usize,
    drawings: DrawingRegistry,
    logs: Vec<LogEntry>,
    alerts: Vec<AlertEvent>,
    alert_fired_sites: HashSet<usize>,
    security_memo: SecurityMemo,
    security_expr_memo: SecurityExprMemo,
    pivot_hlc_memo: HashMap<i64, PivotHlcSeries>,
    pivot_states: HashMap<usize, PivotPointState>,
    time_func_memo: HashMap<String, resample::BucketOpens>,
    regex_memo: HashMap<String, Result<Arc<Regex>, String>>,
    regex_memo_bytes: usize,
    string_bytes_allocated: usize,
    bytes_reserved: usize,
    /// Empty vectors recycled across calls so per-call Vm assembly never
    /// allocates: user slots / plots are AOT-owned lanes, not host state.
    user_slots: Vec<Vec<Value>>,
    /// Planned once per run from the program: each scan reads every
    /// instruction, which per `host.vm` call cost more than the call.
    slot_mask: usize,
    gc_history_depths: Vec<u32>,
    written: Vec<bool>,
    plots: Vec<Vec<Value>>,
    plot_colors: Vec<Vec<Vec<Value>>>,
    visual_colors: Vec<Vec<Vec<Value>>>,
    fill_colors: Vec<Vec<Vec<Value>>>,
    ta_state: ta::TaStates,
    dynamic_ema: Vec<Option<DynamicEmaState>>,
    sma: sma::States,
    /// A `strategy()` chart module's broker: built as [`Vm::new`] builds it,
    /// and advanced bar by bar as the module's host calls arrive
    /// ([`Vm::aot_broker_reach`]), so its orders, fills and results are the
    /// interpreter's own.
    broker: Option<Broker>,
    /// The bar whose pre-body broker work has run and whose post-body work
    /// has not: every earlier bar is complete.
    broker_open: Option<usize>,
    /// Why this run's strategy cannot follow the native bar loop whatever
    /// bars it runs, when it cannot: a setting that recalculates the script
    /// ([`aot_strategy_refusal`]) or market data the interpreter rejects.
    run_refusal: Option<&'static str>,
    /// A strategy's first bar whose closing tick would not show the script
    /// the bar as supplied ([`first_unshown_bar`]): a run that executes it
    /// cannot follow the native bar loop (see [`AotVmState::strategy_refusal`]).
    unshown_bar: Option<usize>,
    /// The host references a module's collection has sent since the last
    /// one: roots of the next [`Vm::collect_aot_host_heap`].
    pub(crate) module_roots: Vec<HandleId>,
    pub(crate) host_gc_arena_due: usize,
    pub(crate) host_gc_elems_due: usize,
    /// The interpreter [`AotVmState::enter`] assembled around this state,
    /// kept between hosted calls. While it is parked it holds every
    /// container [`Vm::from_aot_state`] moves, and the fields above that
    /// mirror them are empty ([`AotVmState::unpark`] moves them back).
    parked: Option<Box<Vm>>,
}

/// Why a `strategy()` chart module cannot run natively over any bars, or
/// `None` when it can. The module executes its body once per bar, at the
/// close, over the bars as supplied: the interpreter's own sequencing for a
/// close-only strategy whenever its closing tick shows the script the same
/// bar, which [`first_unshown_bar`] checks for the bars a run executes.
///
/// Recalculating after fills (`calc_on_order_fills`), on every historical
/// tick (`calc_on_every_history_tick`), or only over a trailing window
/// (`calc_bars_count`) changes which executions happen, so those settings
/// refuse. They arrive through `options.strategy` too, so they are checked
/// after the overrides apply.
fn aot_strategy_refusal(info: &StrategyInfo) -> Option<&'static str> {
    if info.calc_on_order_fills {
        return Some("calc_on_order_fills recalculates the script after fills");
    }
    if info.calc_on_every_history_tick {
        return Some("calc_on_every_history_tick recalculates the script on every tick");
    }
    if info.calc_bars_count > 0 {
        return Some("calc_bars_count skips the script on early bars");
    }
    None
}

/// Why a strategy chart module's run must use the interpreter when it
/// executes a bar its closing tick would not show as supplied.
const UNSHOWN_BAR_REFUSAL: &str = "a bar's high or low does not bound its other prices";

/// The first bar of `data` whose closing tick would not show the script the
/// bar as supplied (see [`closing_tick_shows_supplied_bar`]). Only a run
/// that executes it is refused, so a host compares it with its own bar
/// count rather than the feed's.
fn first_unshown_bar(data: &MarketData) -> Option<usize> {
    (0..data.bars()).find(|&bar| !closing_tick_shows_supplied_bar(data, bar))
}

/// Whether [`Vm::run_strategy_history_bar`]'s closing tick shows the script
/// bar `bar` as supplied. That tick writes the high and low of the bar's
/// price path and rescales volume by `4 / 4`; they differ from the supplied
/// bar when the supplied high is below another of its prices (or the low
/// above one), when the high or low is missing (the path's extremes skip
/// na, so a missing open or close leaves them as supplied), or when volume
/// is within a factor of four of overflowing. Non-finite volumes read na
/// either way. A lane with no value for the bar is malformed data, which
/// the interpreter reports ([`MarketData::validate`]), so it does not show
/// the bar either and the run goes there.
fn closing_tick_shows_supplied_bar(data: &MarketData, bar: usize) -> bool {
    let lane = |values: &[f64]| values.get(bar).copied();
    let (Some(open), Some(high), Some(low), Some(close), Some(volume)) = (
        lane(&data.open),
        lane(&data.high),
        lane(&data.low),
        lane(&data.close),
        lane(&data.volume),
    ) else {
        return false;
    };
    let high_first = (high - open).abs() <= (open - low).abs();
    let path = if high_first {
        [open, high, low, close]
    } else {
        [open, low, high, close]
    };
    let (mut running_high, mut running_low) = (open, open);
    for price in path {
        running_high = running_high.max(price);
        running_low = running_low.min(price);
    }
    let rescaled = volume * 4.0 / 4.0;
    running_high.to_bits() == high.to_bits()
        && running_low.to_bits() == low.to_bits()
        && (rescaled.to_bits() == volume.to_bits() || !(rescaled.is_finite() || volume.is_finite()))
}

impl AotVmState {
    /// A fresh run state over `data` for a host that runs `bars` bars.
    ///
    /// `bars` is the host's effective run length (a linked chart request's
    /// `bars`), not the feed length: typed fill lanes are sized to it, so a
    /// long feed run over a few bars does not allocate lanes for bars the
    /// host never executes.
    pub(crate) fn new(
        program: &CsilProgram,
        data: MarketData,
        options: &RunOptions,
        bars: usize,
    ) -> AotVmState {
        // `Vm::new`'s broker, from the same declaration, overrides and symbol.
        let strategy_info = program
            .metadata
            .strategy
            .as_ref()
            .map(|base| options.strategy.applied_to(base));
        let broker = strategy_info.as_ref().map(|strategy| {
            Broker::new_with_symbol(
                strategy,
                data.mintick,
                data.syminfo.pointvalue,
                data.syminfo.mincontract,
                &data.syminfo.currency,
            )
        });
        // The broker reads every lane, so a feed the interpreter would reject
        // (`MarketData::validate`) goes there for its structured error.
        let run_refusal = strategy_info.as_ref().and_then(|strategy| {
            aot_strategy_refusal(strategy).or_else(|| {
                data.validate()
                    .err()
                    .map(|_| "the market data is malformed")
            })
        });
        let unshown_bar = strategy_info
            .as_ref()
            .and_then(|_| first_unshown_bar(&data));
        AotVmState {
            broker,
            broker_open: None,
            run_refusal,
            unshown_bar,
            module_roots: Vec::new(),
            host_gc_arena_due: GC_MIN_ARENA,
            host_gc_elems_due: crate::aot_host::host_gc_threshold(
                0,
                options.max_collection_elements.div_ceil(2).max(1),
            ),
            data,
            stack: Vec::new(),
            call_args: Vec::new(),
            random_draws: 0,
            last_table_identity: 0,
            heap: Heap::with_limit(options.max_collection_elements),
            distance_cache: distance_scan::Cache::default(),
            distance_cache_reserved_bytes: 0,
            drawings: {
                // Mirrors `run()`'s registry construction exactly (drawing caps).
                let caps = program.metadata.drawing_caps;
                let cap_or = |cap: Option<u32>, kind: DrawKind| {
                    cap.map_or(kind.default_max(), |n| n as usize)
                };
                DrawingRegistry {
                    live: Default::default(),
                    max: [
                        cap_or(caps.max_lines_count, DrawKind::Line),
                        cap_or(caps.max_labels_count, DrawKind::Label),
                        cap_or(caps.max_boxes_count, DrawKind::Box),
                        DrawKind::Table.default_max(),
                        cap_or(caps.max_polylines_count, DrawKind::Polyline),
                        DrawKind::Linefill.default_max(),
                    ],
                    table_cells: std::collections::BTreeMap::new(),
                    ids: Default::default(),
                    next_id: 0,
                }
            },
            logs: Vec::new(),
            alerts: Vec::new(),
            alert_fired_sites: HashSet::new(),
            security_memo: HashMap::new(),
            security_expr_memo: HashMap::new(),
            pivot_hlc_memo: HashMap::new(),
            pivot_states: HashMap::new(),
            time_func_memo: HashMap::new(),
            regex_memo: HashMap::new(),
            regex_memo_bytes: 0,
            string_bytes_allocated: 0,
            bytes_reserved: 0,
            user_slots: Vec::new(),
            slot_mask: program.history_ring() as usize - 1,
            // The AOT host does not call `maybe_collect_heap`; use a
            // conservative bound if that ever changes.
            gc_history_depths: vec![program.history_depth(); program.series_slots.len()],
            written: Vec::new(),
            plots: Vec::new(),
            plot_colors: Vec::new(),
            visual_colors: Vec::new(),
            fill_colors: program
                .fills
                .iter()
                .map(|fill| {
                    if fill
                        .color_roles
                        .iter()
                        .any(|role| role.value_domain() == "float-or-na")
                    {
                        vec![vec![Value::Na; bars]; fill.color_roles.len()]
                    } else {
                        Vec::new()
                    }
                })
                .collect(),
            ta_state: ta::TaStates::default(),
            dynamic_ema: Vec::new(),
            sma: sma::States::default(),
            parked: None,
        }
    }

    /// The market data this run executes over.
    pub(crate) fn data(&self) -> &MarketData {
        match &self.parked {
            Some(vm) => &vm.data,
            None => &self.data,
        }
    }

    pub(crate) fn clear_alert_dedup(&mut self) {
        match &mut self.parked {
            Some(vm) => vm.alert_fired_sites.clear(),
            None => self.alert_fired_sites.clear(),
        }
    }

    /// What the run has reserved of its string budget
    /// ([`RunOptions::max_string_bytes_per_run`]).
    pub(crate) fn string_bytes_allocated(&self) -> usize {
        match &self.parked {
            Some(vm) => vm.string_bytes_allocated,
            None => self.string_bytes_allocated,
        }
    }

    /// Set what the run has reserved of its string budget. A linked module
    /// reserves for the instructions it runs itself and reports the total
    /// before each hosted call ([`aot_host::VmHost::vm_call_metered`]).
    pub(crate) fn set_string_bytes_allocated(&mut self, bytes: usize) {
        match &mut self.parked {
            Some(vm) => vm.string_bytes_allocated = bytes,
            None => self.string_bytes_allocated = bytes,
        }
    }

    pub(crate) fn take_logs(&mut self) -> Vec<LogEntry> {
        match &mut self.parked {
            Some(vm) => std::mem::take(&mut vm.logs),
            None => std::mem::take(&mut self.logs),
        }
    }

    pub(crate) fn take_alerts(&mut self) -> Vec<AlertEvent> {
        match &mut self.parked {
            Some(vm) => std::mem::take(&mut vm.alerts),
            None => std::mem::take(&mut self.alerts),
        }
    }

    /// The run's interpreter, positioned on `bar` of `bars`: the [`Vm`]
    /// that [`Vm::from_aot_state`] assembles, kept between calls instead of
    /// taken apart after each one. A module makes hundreds of hosted calls
    /// per bar, and moving every container out of this state and back cost
    /// more than the instructions the calls execute.
    ///
    /// A reused interpreter is the one a fresh assembly would produce:
    /// [`Vm::into_aot_state`] and [`Vm::from_aot_state`] carry every
    /// persistent container across unchanged, and the fields they do not
    /// carry are set here to the values `from_aot_state` gives them.
    pub(crate) fn enter(
        &mut self,
        program: &Arc<CsilProgram>,
        options: &Arc<RunOptions>,
        bars: usize,
        bar: usize,
    ) -> &mut Vm {
        let slot_mask = self.slot_mask;
        let vm = match self.parked.take() {
            Some(mut vm) => {
                vm.gc_threshold = GC_MIN_ARENA;
                vm.gc_elem_threshold = options.max_collection_elements.div_ceil(2).max(1);
                vm.slot_mask = slot_mask;
                if vm.window_scratch.capacity() != 0 {
                    vm.window_scratch = Vec::new();
                }
                vm.bar = bar;
                vm.bars = bars;
                vm.instructions_remaining = u64::MAX;
                vm.realtime_active = false;
                vm.has_realtime = false;
                vm.bar_is_new = true;
                vm.bar_is_confirmed = true;
                vm.realtime_bar = None;
                vm.realtime_driver = RealtimeDriverState::default();
                vm.transient_vm_bytes = 0;
                vm.preallocated_realtime_appends = 0;
                vm.profile = None;
                vm
            }
            None => Box::new(Vm::from_aot_state(program, options, bars, bar, self)),
        };
        self.parked.insert(vm)
    }

    /// Move a parked interpreter's containers back into this state, as
    /// [`Vm::into_aot_state`] does after a hosted call.
    fn unpark(&mut self) {
        if let Some(vm) = self.parked.take() {
            vm.into_aot_state(self);
        }
    }

    /// Why a strategy run over the first `bars` bars cannot follow the
    /// native bar loop, or `None` when it can (always `None` for an
    /// indicator). A bar past `bars` never runs, so it cannot refuse.
    pub(crate) fn strategy_refusal(&self, bars: usize) -> Option<&'static str> {
        self.run_refusal.or_else(|| {
            self.unshown_bar
                .filter(|&bar| bar < bars)
                .map(|_| UNSHOWN_BAR_REFUSAL)
        })
    }
}

impl Vm {
    /// Assemble a [`Vm`] around the AOT host state for one hosted
    /// instruction (or a drain). O(1): every container is moved and the
    /// program and options are shared, not copied.
    pub(crate) fn from_aot_state(
        program: &Arc<CsilProgram>,
        options: &Arc<RunOptions>,
        bars: usize,
        bar: usize,
        st: &mut AotVmState,
    ) -> Vm {
        st.unpark();
        Vm {
            program: Arc::clone(program),
            data: std::mem::take(&mut st.data),
            options: Arc::clone(options),
            // The native tier drives its own bar loop, so this Vm never reaches
            // `run_history`'s collection point; the threshold is inert here.
            gc_threshold: GC_MIN_ARENA,
            gc_elem_threshold: options.max_collection_elements.div_ceil(2).max(1),
            user_slots: std::mem::take(&mut st.user_slots),
            slot_mask: st.slot_mask,
            gc_history_depths: std::mem::take(&mut st.gc_history_depths),
            written: std::mem::take(&mut st.written),
            plots: std::mem::take(&mut st.plots),
            plot_colors: std::mem::take(&mut st.plot_colors),
            visual_colors: std::mem::take(&mut st.visual_colors),
            fill_colors: std::mem::take(&mut st.fill_colors),
            ta_state: std::mem::take(&mut st.ta_state),
            dynamic_ema: std::mem::take(&mut st.dynamic_ema),
            sma: std::mem::take(&mut st.sma),
            stack: std::mem::take(&mut st.stack),
            call_args: std::mem::take(&mut st.call_args),
            window_scratch: Vec::new(),
            bar,
            bars,
            // The emitted module owns the cumulative counter and mirrors
            // host-op extra charges after each call. Keep the transient Vm's
            // duplicate counter inert so work is charged exactly once.
            instructions_remaining: u64::MAX,
            random_draws: st.random_draws,
            realtime_active: false,
            has_realtime: false,
            bar_is_new: true,
            bar_is_confirmed: true,
            realtime_bar: None,
            realtime_driver: RealtimeDriverState::default(),
            realtime_alias_undo: BTreeMap::new(),
            last_table_identity: st.last_table_identity,
            heap: std::mem::take(&mut st.heap),
            distance_cache: std::mem::take(&mut st.distance_cache),
            distance_cache_reserved_bytes: st.distance_cache_reserved_bytes,
            drawings: std::mem::take(&mut st.drawings),
            logs: std::mem::take(&mut st.logs),
            alerts: std::mem::take(&mut st.alerts),
            alert_fired_sites: std::mem::take(&mut st.alert_fired_sites),
            broker: st.broker.take(),
            fill_recalculation_price: None,
            security_memo: std::mem::take(&mut st.security_memo),
            security_expr_memo: std::mem::take(&mut st.security_expr_memo),
            pivot_hlc_memo: std::mem::take(&mut st.pivot_hlc_memo),
            pivot_states: std::mem::take(&mut st.pivot_states),
            time_func_memo: std::mem::take(&mut st.time_func_memo),
            regex_memo: std::mem::take(&mut st.regex_memo),
            regex_memo_bytes: st.regex_memo_bytes,
            string_bytes_allocated: st.string_bytes_allocated,
            bytes_reserved: st.bytes_reserved,
            transient_vm_bytes: 0,
            preallocated_realtime_appends: 0,
            profile: None,
        }
    }

    /// Return every persistent container to the host state, which must
    /// hold no parked interpreter (one would own the same containers).
    pub(crate) fn into_aot_state(mut self, st: &mut AotVmState) {
        st.data = std::mem::take(&mut self.data);
        st.user_slots = std::mem::take(&mut self.user_slots);
        st.gc_history_depths = std::mem::take(&mut self.gc_history_depths);
        st.written = std::mem::take(&mut self.written);
        st.plots = std::mem::take(&mut self.plots);
        st.plot_colors = std::mem::take(&mut self.plot_colors);
        st.visual_colors = std::mem::take(&mut self.visual_colors);
        st.fill_colors = std::mem::take(&mut self.fill_colors);
        st.ta_state = std::mem::take(&mut self.ta_state);
        st.dynamic_ema = std::mem::take(&mut self.dynamic_ema);
        st.sma = std::mem::take(&mut self.sma);
        st.stack = std::mem::take(&mut self.stack);
        st.call_args = std::mem::take(&mut self.call_args);
        st.random_draws = self.random_draws;
        st.last_table_identity = self.last_table_identity;
        st.heap = std::mem::take(&mut self.heap);
        st.distance_cache = std::mem::take(&mut self.distance_cache);
        st.distance_cache_reserved_bytes = self.distance_cache_reserved_bytes;
        st.drawings = std::mem::take(&mut self.drawings);
        st.logs = std::mem::take(&mut self.logs);
        st.alerts = std::mem::take(&mut self.alerts);
        st.alert_fired_sites = std::mem::take(&mut self.alert_fired_sites);
        st.broker = self.broker.take();
        st.security_memo = std::mem::take(&mut self.security_memo);
        st.security_expr_memo = std::mem::take(&mut self.security_expr_memo);
        st.pivot_hlc_memo = std::mem::take(&mut self.pivot_hlc_memo);
        st.pivot_states = std::mem::take(&mut self.pivot_states);
        st.time_func_memo = std::mem::take(&mut self.time_func_memo);
        st.regex_memo = std::mem::take(&mut self.regex_memo);
        st.regex_memo_bytes = self.regex_memo_bytes;
        st.string_bytes_allocated = self.string_bytes_allocated;
        st.bytes_reserved = self.bytes_reserved;
    }

    /// The broker work [`Vm::run_history`] does on a close-only strategy bar
    /// before the script executes: the bar's currency rate, the risk
    /// session, the four-tick fill walk of [`Vm::run_strategy_history_bar`]
    /// (with its script snapshot), then the execution boundary
    /// [`Vm::run_bar`] opens.
    fn aot_strategy_open_bar(&mut self, bar: usize) {
        self.sync_broker_currency_rate(bar, false);
        let session = strategy_risk_session_key(&self.data, bar);
        let (open, high, low, close) = (
            self.data.open[bar],
            self.data.high[bar],
            self.data.low[bar],
            self.data.close[bar],
        );
        if let Some(broker) = self.broker.as_mut() {
            broker.begin_session(session, bar, open);
            // The interpreter's broker has no `BrokerLimits`, so the limit
            // checks around the walk cannot fail.
            let _ = broker.process_bar_ticks(bar, open, high, low, close);
            broker.begin_script_execution();
        }
    }

    /// The broker work after the script executes on a close-only strategy
    /// bar: the immediate-fill count the interpreter takes (and, without
    /// `calc_on_order_fills`, ignores), the `process_orders_on_close` pass,
    /// and the bar's equity sample.
    fn aot_strategy_close_bar(&mut self, bar: usize) {
        let close = self.data.close[bar];
        if let Some(broker) = self.broker.as_mut() {
            broker.take_immediate_fill_count();
            let _ = broker.finish_bar(bar, close);
        }
    }

    /// Advance a strategy chart module's broker to `bar`'s script execution:
    /// close the open bar, run every bar the module made no host call on as
    /// a bar whose script placed no orders, and open `bar`. `open` is the
    /// open bar ([`AotVmState`]'s `broker_open`); calls arrive in execution
    /// order, so `bar` is never before it. A no-op without a broker.
    pub(crate) fn aot_broker_reach(&mut self, bar: usize, open: &mut Option<usize>) {
        if self.broker.is_none() || *open == Some(bar) {
            return;
        }
        let next = match *open {
            Some(current) => {
                self.aot_strategy_close_bar(current);
                current + 1
            }
            None => 0,
        };
        for skipped in next..bar {
            self.aot_strategy_open_bar(skipped);
            self.aot_strategy_close_bar(skipped);
        }
        self.aot_strategy_open_bar(bar);
        *open = Some(bar);
    }

    /// Finish a strategy chart module's broker after its run stopped with
    /// `bars_executed` complete bars out of `bars`, and return the
    /// interpreter's strategy section for that run. A run that stopped early
    /// errored on bar `bars_executed`, which the interpreter leaves open: its
    /// pre-script broker work ran, its post-script work did not.
    pub(crate) fn aot_strategy_outcome(
        &mut self,
        bars_executed: usize,
        bars: usize,
        open: &mut Option<usize>,
    ) -> Option<StrategyOutcome> {
        let strategy = self
            .program
            .metadata
            .strategy
            .as_ref()
            .map(|base| self.options.strategy.applied_to(base))?;
        if bars_executed < bars {
            self.aot_broker_reach(bars_executed, open);
        } else if bars > 0 {
            self.aot_broker_reach(bars - 1, open);
            self.aot_strategy_close_bar(bars - 1);
            *open = None;
        }
        let final_close = bars_executed
            .checked_sub(1)
            .and_then(|bar| self.data.close.get(bar).copied())
            .unwrap_or(f64::NAN);
        let mut broker = self.broker.take()?;
        merge_order_fill_alerts(&mut self.alerts, &mut broker);
        Some(strategy_outcome(
            strategy.initial_capital,
            broker,
            final_close,
        ))
    }
}

#[cfg(test)]
mod tests;

/// Differential tests for the extracted kernels (`vm_int`, `vm_offset`,
/// `vm_tick`): each kernel against a direct model of the same operation,
/// on random and edge-case inputs.
#[cfg(test)]
mod fin_vmkern_differential {
    use super::{vm_charge, vm_int, vm_offset, vm_tick};

    /// A small deterministic xorshift64* stream (no `rand` dependency).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn i64_edge(&mut self) -> i64 {
            const EDGES: [i64; 9] = [
                i64::MIN,
                i64::MIN + 1,
                -2,
                -1,
                0,
                1,
                2,
                i64::MAX - 1,
                i64::MAX,
            ];
            match self.next() % 4 {
                0 => EDGES[(self.next() % EDGES.len() as u64) as usize],
                1 => (self.next() % 2001) as i64 - 1000,
                2 => (self.next() as i64) >> (self.next() % 63),
                _ => self.next() as i64,
            }
        }
        fn opt_i64(&mut self) -> Option<i64> {
            if self.next() % 8 == 0 {
                None
            } else {
                Some(self.i64_edge())
            }
        }
    }

    /// Direct model of the `IntAdd`..`IntRem` handler body.
    fn int_binop_reference(
        op: vm_int::IntBinOp,
        lhs: Option<i64>,
        rhs: Option<i64>,
    ) -> Option<i64> {
        match (lhs, rhs) {
            (Some(l), Some(r)) => match op {
                vm_int::IntBinOp::Add => l.checked_add(r),
                vm_int::IntBinOp::Sub => l.checked_sub(r),
                vm_int::IntBinOp::Mul => l.checked_mul(r),
                vm_int::IntBinOp::Div => l.checked_div(r),
                vm_int::IntBinOp::Rem => l.checked_rem(r),
            },
            _ => None,
        }
    }

    #[test]
    fn int_kernels_match_reference() {
        use vm_int::IntBinOp::*;
        let mut rng = Rng(0x1234_5678_9ABC_DEF1);
        let mut some = 0usize;
        for _ in 0..400_000 {
            let (l, r) = (rng.opt_i64(), rng.opt_i64());
            for op in [Add, Sub, Mul, Div, Rem] {
                let got = vm_int::int_binop(op, l, r);
                assert_eq!(got, int_binop_reference(op, l, r), "{op:?} {l:?} {r:?}");
                some += usize::from(got.is_some());
            }
            assert_eq!(
                vm_int::int_neg(l),
                l.and_then(i64::checked_neg),
                "neg {l:?}"
            );
        }
        assert!(some > 1_000_000, "coverage floor: {some}");
        assert_eq!(vm_int::int_binop(Div, Some(i64::MIN), Some(-1)), None);
        assert_eq!(vm_int::int_binop(Rem, Some(i64::MIN), Some(-1)), None);
        assert_eq!(vm_int::int_binop(Rem, Some(-7), Some(2)), Some(-1));
        assert_eq!(vm_int::int_neg(Some(i64::MIN)), None);
    }

    /// Direct model of the `ReadUserSlotDynamic` decode, check and cell lookup
    /// (`Err(offset)` is the `max_bars_back` error).
    fn dynamic_read_reference(
        raw: Option<i64>,
        limit: u32,
        bar: usize,
        mask: usize,
    ) -> Result<Option<usize>, u64> {
        let decoded = match raw {
            Some(v) if v >= 0 => u64::try_from(v).ok(),
            _ => None,
        };
        match decoded {
            Some(offset) => {
                if offset > u64::from(limit) {
                    return Err(offset);
                }
                let offset = offset as u32 as usize;
                Ok(vm_charge::ring_read(bar, offset, mask))
            }
            None => Ok(None),
        }
    }

    #[test]
    fn dynamic_read_matches_reference() {
        let mut rng = Rng(0x0DDB_A11C_AFE0_0001);
        let (mut cells, mut deep, mut na) = (0usize, 0usize, 0usize);
        for _ in 0..400_000 {
            let raw = match rng.next() % 3 {
                0 => rng.opt_i64(),
                _ => Some((rng.next() % 3000) as i64 - 50),
            };
            let limit = match rng.next() % 4 {
                0 => u32::MAX,
                1 => 0,
                _ => (rng.next() % 2000) as u32,
            };
            let bar = (rng.next() % 5000) as usize;
            let mask = (1usize << (rng.next() % 12)) - 1;
            let got = vm_offset::dynamic_read(raw, limit, bar, mask);
            let expected = dynamic_read_reference(raw, limit, bar, mask);
            match (got, expected) {
                (vm_offset::DynamicRead::Na, Ok(None)) => na += 1,
                (vm_offset::DynamicRead::TooDeep(a), Err(b)) => {
                    assert_eq!(a, b);
                    deep += 1;
                }
                (vm_offset::DynamicRead::Cell(a), Ok(Some(b))) => {
                    assert_eq!(a, b);
                    cells += 1;
                }
                (got, expected) => panic!("{raw:?} {limit} {bar} {mask}: {got:?} vs {expected:?}"),
            }
            let decoded = match raw {
                Some(v) if v >= 0 => u64::try_from(v).ok(),
                _ => None,
            };
            assert_eq!(vm_offset::decode_offset(raw), decoded);
        }
        assert!(
            cells > 10_000 && deep > 10_000 && na > 10_000,
            "{cells} {deep} {na}"
        );
    }

    /// Direct model of the driver bookkeeping of `Vm::run_realtime` (open,
    /// validate, record the snapshot, ordinal, commit), with the snapshot
    /// reduced to the bar it was taken at and `self.bars` grown on append.
    #[derive(Default, Debug, PartialEq, Eq, Clone)]
    struct ReferenceDriver {
        open_bar: Option<usize>,
        open_bar_appended: bool,
        bar_snapshot: Option<usize>,
        next_tick_ordinal: u32,
    }

    /// One tick on the model driver; `Err(())` is the `InvalidProgram`
    /// error. Returns whether it rolled back.
    fn reference_tick(
        driver: &mut ReferenceDriver,
        bars: &mut usize,
        bar: usize,
        close: bool,
        bars_executed: &mut u32,
    ) -> Result<bool, ()> {
        if driver.open_bar.is_some_and(|open| open != bar) {
            return Err(());
        }
        let first_tick_on_bar = driver.open_bar != Some(bar);
        let mut rolled_back = false;
        if first_tick_on_bar {
            let last_committed = bars.saturating_sub(1);
            let opening_new_bar = bar == *bars;
            let rerunning_last = bar == last_committed && *bars > 0;
            if !opening_new_bar && !rerunning_last {
                return Err(());
            }
            driver.open_bar_appended = opening_new_bar;
            if opening_new_bar {
                *bars += 1;
            }
            driver.bar_snapshot = Some(bar);
            driver.open_bar = Some(bar);
        } else if driver.bar_snapshot.is_some() {
            rolled_back = true;
        }
        driver.next_tick_ordinal = driver.next_tick_ordinal.saturating_add(1);
        if close {
            if driver.open_bar_appended {
                *bars_executed += 1;
            }
            driver.open_bar = None;
            driver.open_bar_appended = false;
            driver.bar_snapshot = None;
        }
        Ok(rolled_back)
    }

    /// The same tick through the kernels, in `run_realtime`'s order.
    fn kernel_tick(
        driver: &mut vm_tick::TickDriver<usize>,
        bars: &mut usize,
        bar: usize,
        close: bool,
        bars_executed: &mut u32,
    ) -> Result<bool, ()> {
        let Some(start) = vm_tick::begin_tick(driver, *bars, bar) else {
            return Err(());
        };
        let mut rolled_back = false;
        if start != vm_tick::TickStart::Rollback {
            if start == vm_tick::TickStart::Append {
                *bars += 1;
            }
            vm_tick::open_bar(driver, bar, bar);
        } else if driver.bar_snapshot.is_some() {
            rolled_back = true;
        }
        vm_tick::finish_tick(driver, close, bars_executed);
        Ok(rolled_back)
    }

    #[test]
    fn tick_driver_matches_reference() {
        let mut rng = Rng(0x7157_0000_0000_0001);
        let (mut rejected, mut rollbacks, mut commits) = (0usize, 0usize, 0u32);
        for _ in 0..20_000 {
            let mut reference = ReferenceDriver::default();
            let mut kernel = vm_tick::TickDriver::<usize>::default();
            let start_bars = (rng.next() % 4) as usize;
            let (mut rb, mut kb) = (start_bars, start_bars);
            let (mut re, mut ke) = (0u32, 0u32);
            for _ in 0..40 {
                let bar = match rng.next() % 6 {
                    0 => (rng.next() % 8) as usize,
                    1 | 2 => rb,
                    _ => rb.saturating_sub(1),
                };
                let close = rng.next() % 3 == 0;
                let r = reference_tick(&mut reference, &mut rb, bar, close, &mut re);
                let k = kernel_tick(&mut kernel, &mut kb, bar, close, &mut ke);
                assert_eq!(r, k);
                rejected += usize::from(r.is_err());
                rollbacks += usize::from(r == Ok(true));
                assert_eq!((rb, re), (kb, ke));
                let kernel_view = ReferenceDriver {
                    open_bar: kernel.open_bar,
                    open_bar_appended: kernel.open_bar_appended,
                    bar_snapshot: kernel.bar_snapshot,
                    next_tick_ordinal: kernel.next_tick_ordinal,
                };
                assert_eq!(reference, kernel_view);
            }
            commits += re;
        }
        assert!(
            rejected > 10_000 && rollbacks > 10_000 && commits > 10_000,
            "{rejected} {rollbacks} {commits}"
        );
    }

    /// Direct model of `splitmix64_at`.
    fn splitmix64_reference(key: u64, index: u64) -> u64 {
        let mut z = key.wrapping_add(index.wrapping_add(1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    #[test]
    fn random_draw_matches_reference() {
        let mut rng = Rng(0x5EED_0000_0000_0001);
        for _ in 0..400_000 {
            let key = rng.next();
            let draws = match rng.next() % 4 {
                0 => rng.next() % 64,
                1 => u64::MAX - 1 - rng.next() % 64,
                _ => rng.next() >> 1,
            };
            let (draw, next) = vm_tick::random_draw(key, draws);
            assert_eq!(draw, splitmix64_reference(key, draws));
            assert_eq!(next, draws + 1);
        }
        // Known answer: the first output of the default-seed stream.
        assert_eq!(
            vm_tick::splitmix64_at(super::DEFAULT_RANDOM_SEED, 0),
            splitmix64_reference(super::DEFAULT_RANDOM_SEED, 0)
        );
    }
}

/// Differential tests of the heap kernels (`vm_heap_ops`,
/// `vm_gc_roots`) against direct models of the handler logic,
/// on random heaps, receivers, indices, values and budgets.
#[cfg(test)]
mod fin_vmheap_differential {
    use super::vm_heap_ops::{self, HeapOpError, Place};
    use super::{vm_gc_roots, TableCell};
    use pinewasm_mir::{ElemType, HandleId, Heap, HeapObj, Value};

    /// A small deterministic xorshift64* stream (no `rand` dependency).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        fn elem_ty(&mut self) -> ElemType {
            match self.below(7) {
                0 => ElemType::Float,
                1 => ElemType::Int,
                2 => ElemType::Bool,
                3 => ElemType::String,
                4 => ElemType::Color,
                5 => ElemType::Object(self.below(3) as u32),
                _ => ElemType::Object(u32::MAX),
            }
        }
        fn value(&mut self, handles: u32) -> Value {
            match self.below(9) {
                0 => Value::Na,
                1 => Value::Bool(self.below(2) == 0),
                2 => Value::Int(match self.below(4) {
                    0 => i64::MIN,
                    1 => i64::MAX,
                    _ => self.below(9) as i64 - 3,
                }),
                3 => Value::Float(match self.below(4) {
                    0 => -2.5,
                    1 => 1.0e30,
                    2 => 3.75,
                    _ => -0.0,
                }),
                4 => Value::String("x".repeat(self.below(40) as usize)),
                5 => Value::Color {
                    r: 1,
                    g: 2,
                    b: 3,
                    t: 4.0,
                    input_origin: None,
                },
                _ => Value::Ref(HandleId(self.below(u64::from(handles) + 2) as u32)),
            }
        }
        /// A value that conforms to `elem_ty` (or is `na`).
        fn conforming(&mut self, elem_ty: ElemType) -> Value {
            match (elem_ty, self.below(8)) {
                (_, 0) => Value::Na,
                (ElemType::Float, 1) => Value::Int(self.below(9) as i64 - 3),
                (ElemType::Float, _) => Value::Float(0.5),
                (ElemType::Int, _) => Value::Int(self.below(9) as i64 - 3),
                (ElemType::Bool, _) => Value::Bool(true),
                (ElemType::String, _) => Value::String("y".repeat(self.below(6) as usize)),
                (ElemType::Color, _) => Value::Color {
                    r: 0,
                    g: 0,
                    b: 0,
                    t: 0.0,
                    input_origin: None,
                },
                (ElemType::Object(_), _) => Value::Ref(HandleId(self.below(4) as u32)),
            }
        }
        /// A heap with a few arrays, a matrix and a map, small budgets.
        fn heap(&mut self) -> Heap {
            let max_elements = self.below(24) as usize;
            let max_objects = 1 + self.below(8) as usize;
            let mut heap = Heap::with_limits(usize::MAX / 2, max_objects);
            let n = self.below(max_objects as u64 + 1);
            for _ in 0..n {
                let obj = match self.below(4) {
                    0 | 1 => {
                        let elem_ty = self.elem_ty();
                        let len = self.below(4);
                        let items = (0..len).map(|_| self.value(4)).collect();
                        HeapObj::Array { elem_ty, items }
                    }
                    2 => HeapObj::Matrix {
                        elem_ty: ElemType::Float,
                        rows: 1,
                        cols: 1,
                        items: vec![Value::Float(1.0)],
                    },
                    _ => HeapObj::Map {
                        key_ty: ElemType::Int,
                        val_ty: ElemType::Int,
                        entries: vec![(Value::Int(1), Value::Int(2))],
                    },
                };
                let _ = heap.alloc(obj);
            }
            // Re-cap the element budget below the current live count sometimes.
            let live = heap.live_elements();
            let cap = live + max_elements.min(3);
            let mut capped = Heap::with_limits(cap, max_objects);
            for i in 0..heap.arena_len() {
                if let Some(obj) = heap.get(HandleId(i as u32)) {
                    let _ = capped.alloc(obj.clone());
                }
            }
            capped
        }
    }

    /// Model of `coerce_element` (error = mismatch).
    fn coerce_reference(elem_ty: ElemType, value: Value) -> Result<Value, HeapOpError> {
        if matches!(value, Value::Na) {
            return Ok(Value::Na);
        }
        let ok = match (elem_ty, &value) {
            (ElemType::Float, Value::Float(_)) => true,
            (ElemType::Float, Value::Int(i)) => return Ok(Value::from_f64(*i as f64)),
            (ElemType::Int, Value::Int(_)) => true,
            (ElemType::Bool, Value::Bool(_)) => true,
            (ElemType::String, Value::String(_)) => true,
            (ElemType::Color, Value::Color { .. }) => true,
            (ElemType::Object(_), Value::Ref(_)) => true,
            _ => false,
        };
        if ok {
            Ok(value)
        } else {
            Err(HeapOpError::ElemMismatch(elem_ty))
        }
    }

    fn reserve_str_reference(
        allocated: &mut usize,
        max: usize,
        bytes: usize,
    ) -> Result<(), HeapOpError> {
        let attempted = allocated.saturating_add(bytes);
        if attempted > max {
            return Err(HeapOpError::StringLimit { attempted });
        }
        *allocated = attempted;
        Ok(())
    }

    fn string_len(value: &Value) -> usize {
        match value {
            Value::String(s) => s.len(),
            _ => 0,
        }
    }

    /// Model of the prologue of `call_array_op` (`receiver_handle`, `array_meta`).
    fn prologue_reference(
        heap: &Heap,
        recv: &Value,
    ) -> Result<(HandleId, ElemType, usize), HeapOpError> {
        let h = match recv {
            Value::Ref(h) => *h,
            Value::Na => return Err(HeapOpError::NaReceiver),
            _ => return Err(HeapOpError::NotCollection),
        };
        match heap.get(h) {
            Some(HeapObj::Array { elem_ty, items }) => Ok((h, *elem_ty, items.len())),
            _ => Err(HeapOpError::NotArray),
        }
    }

    fn index_reference(value: &Value) -> Result<i64, HeapOpError> {
        match value {
            Value::Int(i) => Ok(*i),
            Value::Float(f) => Ok(*f as i64),
            _ => Err(HeapOpError::IndexNa),
        }
    }

    /// Model of the `ArrayPush`/`ArrayUnshift`/`ArrayInsert` growth sequence
    /// (charge cell, charge string, coerce, write).
    fn grow_reference(
        heap: &mut Heap,
        allocated: &mut usize,
        max: usize,
        h: HandleId,
        elem_ty: ElemType,
        value: &Value,
        place: Place,
    ) -> Result<(), HeapOpError> {
        let clone_bytes = string_len(value);
        heap.reserve(1).map_err(HeapOpError::Heap)?;
        if let Err(error) = reserve_str_reference(allocated, max, clone_bytes) {
            heap.release(1);
            return Err(error);
        }
        let v = match coerce_reference(elem_ty, value.clone()) {
            Ok(value) => value,
            Err(error) => {
                heap.release(1);
                return Err(error);
            }
        };
        if let Some(HeapObj::Array { items, .. }) = heap.get_mut(h) {
            match place {
                Place::Back => items.push(v),
                Place::Front => items.insert(0, v),
                Place::At(i) => items.insert(i, v),
            }
        }
        Ok(())
    }

    #[derive(Clone, Copy, Debug)]
    enum Op {
        Push,
        Unshift,
        Insert,
        Set,
    }

    fn reference(
        op: Op,
        heap: &mut Heap,
        allocated: &mut usize,
        max: usize,
        recv: &Value,
        index: &Value,
        value: &Value,
    ) -> Result<(), HeapOpError> {
        let (h, elem_ty, len) = prologue_reference(heap, recv)?;
        match op {
            Op::Push => grow_reference(heap, allocated, max, h, elem_ty, value, Place::Back),
            Op::Unshift => grow_reference(heap, allocated, max, h, elem_ty, value, Place::Front),
            Op::Insert => {
                let idx = index_reference(index)?;
                if idx < 0 || idx as usize > len {
                    return Err(HeapOpError::OutOfBounds { index: idx, len });
                }
                grow_reference(
                    heap,
                    allocated,
                    max,
                    h,
                    elem_ty,
                    value,
                    Place::At(idx as usize),
                )
            }
            Op::Set => {
                let idx = index_reference(index)?;
                let at = if idx < 0 {
                    // A negative index counts from the end.
                    usize::try_from(idx.unsigned_abs())
                        .ok()
                        .filter(|from_end| *from_end <= len)
                        .map(|from_end| len - from_end)
                } else {
                    usize::try_from(idx).ok().filter(|at| *at < len)
                };
                let Some(at) = at else {
                    return Err(HeapOpError::OutOfBounds { index: idx, len });
                };
                reserve_str_reference(allocated, max, string_len(value))?;
                let v = coerce_reference(elem_ty, value.clone())?;
                if let Some(HeapObj::Array { items, .. }) = heap.get_mut(h) {
                    items[at] = v;
                }
                Ok(())
            }
        }
    }

    fn kernel(
        op: Op,
        heap: &mut Heap,
        allocated: &mut usize,
        max: usize,
        recv: &Value,
        index: &Value,
        value: &Value,
    ) -> Result<(), HeapOpError> {
        match op {
            Op::Push => vm_heap_ops::array_push(heap, allocated, max, recv, value),
            Op::Unshift => vm_heap_ops::array_unshift(heap, allocated, max, recv, value),
            Op::Insert => vm_heap_ops::array_insert(heap, allocated, max, recv, index, value),
            Op::Set => vm_heap_ops::array_set(heap, allocated, max, recv, index, value),
        }
    }

    /// The kernels agree with the model arms on every success and on
    /// every failure's error, except that an element mismatch now wins over a
    /// budget failure; and every kernel failure leaves the heap and the
    /// string counter unchanged (the model could leave the counter
    /// charged).
    #[test]
    fn array_kernels_match_reference() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let (mut ok, mut atomic_fix, mut precedence) = (0u32, 0u32, 0u32);
        for _ in 0..150_000 {
            let heap0 = rng.heap();
            let op = match rng.below(4) {
                0 => Op::Push,
                1 => Op::Unshift,
                2 => Op::Insert,
                _ => Op::Set,
            };
            let recv = match rng.below(6) {
                0 => rng.value(8),
                _ => Value::Ref(HandleId(rng.below(heap0.arena_len() as u64 + 1) as u32)),
            };
            let index = match rng.below(5) {
                0 => rng.value(4),
                _ => Value::Int(rng.below(7) as i64 - 1),
            };
            let value = match (rng.below(3), &recv) {
                (0, _) => rng.value(8),
                (_, Value::Ref(h)) => match heap0.get(*h) {
                    Some(HeapObj::Array { elem_ty, .. }) => rng.conforming(*elem_ty),
                    _ => rng.value(8),
                },
                _ => rng.value(8),
            };
            let max = rng.below(60) as usize;
            let allocated0 = rng.below(50) as usize;

            let (mut ha, mut aa) = (heap0.clone(), allocated0);
            let (mut hb, mut ab) = (heap0.clone(), allocated0);
            let ra = reference(op, &mut ha, &mut aa, max, &recv, &index, &value);
            let rb = kernel(op, &mut hb, &mut ab, max, &recv, &index, &value);
            match (&ra, &rb) {
                (Ok(()), Ok(())) => {
                    ok += 1;
                    assert_eq!(ha, hb, "{op:?}");
                    assert_eq!(aa, ab, "{op:?}");
                }
                (Err(ea), Err(eb)) => {
                    assert_eq!(hb, heap0, "{op:?}: kernel failure changed the heap");
                    assert_eq!(
                        ab, allocated0,
                        "{op:?}: kernel failure charged the string budget"
                    );
                    assert_eq!(ha, heap0, "{op:?}: model failure changed the heap");
                    if aa != allocated0 {
                        atomic_fix += 1;
                    }
                    if ea != eb {
                        precedence += 1;
                        assert!(
                            matches!(eb, HeapOpError::ElemMismatch(_))
                                && matches!(
                                    ea,
                                    HeapOpError::Heap(_) | HeapOpError::StringLimit { .. }
                                ),
                            "{op:?}: {ea:?} vs {eb:?}"
                        );
                    }
                }
                _ => panic!("{op:?}: model {ra:?}, kernel {rb:?}"),
            }
        }
        assert!(ok > 5_000, "too few successes: {ok}");
        assert!(
            atomic_fix > 50,
            "too few fixed partial effects: {atomic_fix}"
        );
        assert!(precedence > 50, "too few precedence changes: {precedence}");
    }

    /// Model of `F::MatrixNew`.
    fn matrix_new_reference(
        heap: &mut Heap,
        allocated: &mut usize,
        max: usize,
        args: &[Value; 4],
    ) -> Result<HandleId, HeapOpError> {
        let dim = |v: &Value| match v {
            Value::Int(i) if *i >= 0 => usize::try_from(*i).map_err(|_| HeapOpError::DimTooLarge),
            Value::Int(_) => Err(HeapOpError::DimNegative),
            _ => Err(HeapOpError::DimNa),
        };
        let rows = dim(&args[0])?;
        let cols = dim(&args[1])?;
        let elem_ty = ElemType::from_tag(match &args[3] {
            Value::Int(t) => *t,
            _ => 0,
        })
        .unwrap_or(ElemType::Float);
        let count = rows.checked_mul(cols).ok_or(HeapOpError::CountOverflow)?;
        heap.reserve(count).map_err(HeapOpError::Heap)?;
        heap.release(count);
        let string_bytes = match &args[2] {
            Value::String(s) => s
                .len()
                .checked_mul(count)
                .ok_or(HeapOpError::StringCloneOverflow)?,
            _ => 0,
        };
        reserve_str_reference(allocated, max, string_bytes)?;
        let initial = coerce_reference(elem_ty, args[2].clone())?;
        let items = vec![initial; count];
        heap.alloc(HeapObj::Matrix {
            elem_ty,
            rows,
            cols,
            items,
        })
        .map_err(HeapOpError::Heap)
    }

    #[test]
    fn matrix_new_matches_reference() {
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);
        let (mut ok, mut atomic_fix) = (0u32, 0u32);
        for _ in 0..40_000 {
            let heap0 = rng.heap();
            let dim = |rng: &mut Rng| match rng.below(8) {
                0 => rng.value(2),
                1 => Value::Int(-1 - rng.below(3) as i64),
                2 => Value::Int(1 << 32),
                3 => Value::Int(i64::MAX),
                _ => Value::Int(rng.below(4) as i64),
            };
            let tag = match rng.below(4) {
                0 => rng.value(2),
                1 => Value::Int(rng.below(6) as i64),
                2 => Value::Int(1000 + rng.below(3) as i64),
                _ => Value::Int(1000 + (1i64 << 32)),
            };
            let initial = match rng.below(3) {
                0 => rng.value(4),
                _ => {
                    let t = match &tag {
                        Value::Int(t) => *t,
                        _ => 0,
                    };
                    rng.conforming(ElemType::from_tag(t).unwrap_or(ElemType::Float))
                }
            };
            let args = [dim(&mut rng), dim(&mut rng), initial, tag];
            let max = rng.below(200) as usize;
            let allocated0 = rng.below(100) as usize;
            let (mut ha, mut aa) = (heap0.clone(), allocated0);
            let (mut hb, mut ab) = (heap0.clone(), allocated0);
            let ra = matrix_new_reference(&mut ha, &mut aa, max, &args);
            let rb = vm_heap_ops::matrix_new(
                &mut hb, &mut ab, max, &args[0], &args[1], &args[2], &args[3],
            );
            match (&ra, &rb) {
                (Ok(a), Ok(b)) => {
                    ok += 1;
                    assert_eq!(a, b);
                    assert_eq!(ha, hb);
                    assert_eq!(aa, ab);
                }
                (Err(ea), Err(eb)) => {
                    assert_eq!(hb, heap0);
                    assert_eq!(ab, allocated0, "kernel failure charged the string budget");
                    assert_eq!(ha, heap0);
                    if aa != allocated0 {
                        atomic_fix += 1;
                    }
                    if ea != eb {
                        assert!(
                            matches!(eb, HeapOpError::ElemMismatch(_))
                                && matches!(
                                    ea,
                                    HeapOpError::Heap(_)
                                        | HeapOpError::StringLimit { .. }
                                        | HeapOpError::StringCloneOverflow
                                ),
                            "{ea:?} vs {eb:?}"
                        );
                    }
                }
                _ => panic!("model {ra:?}, kernel {rb:?}"),
            }
        }
        assert!(ok > 2_000, "too few successes: {ok}");
        assert!(
            atomic_fix > 20,
            "too few fixed partial effects: {atomic_fix}"
        );
    }

    /// Model of the `TableCell::visit_values` order.
    fn cell_values_reference(cell: &TableCell) -> Vec<&Value> {
        [
            &cell.text,
            &cell.width,
            &cell.height,
            &cell.text_color,
            &cell.text_halign,
            &cell.text_valign,
            &cell.text_size,
            &cell.bgcolor,
            &cell.tooltip,
            &cell.text_font_family,
            &cell.text_formatting,
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    #[test]
    fn gc_roots_kernel_matches_reference() {
        let mut rng = Rng(0x94D0_49BB_1331_11EB);
        let mut seen = 0usize;
        for _ in 0..5_000 {
            let slots: Vec<Vec<Value>> = (0..rng.below(4))
                .map(|_| (0..rng.below(5)).map(|_| rng.value(50)).collect())
                .collect();
            let stack: Vec<Value> = (0..rng.below(6)).map(|_| rng.value(50)).collect();
            let call_args: Vec<Value> = (0..rng.below(4)).map(|_| rng.value(50)).collect();
            let live: Vec<Vec<HandleId>> = (0..rng.below(4))
                .map(|_| {
                    (0..rng.below(4))
                        .map(|_| HandleId(rng.below(50) as u32))
                        .collect()
                })
                .collect();
            let opt = |rng: &mut Rng| {
                if rng.below(3) == 0 {
                    Some(rng.value(50))
                } else {
                    None
                }
            };
            let cells: Vec<TableCell> = (0..rng.below(4))
                .map(|_| TableCell {
                    text: opt(&mut rng),
                    width: opt(&mut rng),
                    height: opt(&mut rng),
                    text_color: opt(&mut rng),
                    text_halign: opt(&mut rng),
                    text_valign: opt(&mut rng),
                    text_size: opt(&mut rng),
                    bgcolor: opt(&mut rng),
                    tooltip: opt(&mut rng),
                    text_font_family: opt(&mut rng),
                    text_formatting: opt(&mut rng),
                    merged: rng.below(2) == 0,
                    merge: None,
                })
                .collect();

            // Model of the `gc_roots` body (with one table holding `cells`).
            let mut expected = Vec::new();
            let push = |roots: &mut Vec<HandleId>, v: &Value| {
                if let Value::Ref(h) = v {
                    roots.push(*h);
                }
            };
            for slot in &slots {
                for cell in slot {
                    push(&mut expected, cell);
                }
            }
            for v in &stack {
                push(&mut expected, v);
            }
            for v in &call_args {
                push(&mut expected, v);
            }
            for kind in &live {
                expected.extend(kind.iter().copied());
            }
            expected.push(HandleId(7));
            for cell in &cells {
                for v in cell_values_reference(cell) {
                    push(&mut expected, v);
                }
            }

            let mut got = vm_gc_roots::gc_roots_core(&slots, &stack, &call_args, &live);
            got.push(HandleId(7));
            for cell in &cells {
                vm_gc_roots::push_cell_roots(&mut got, cell);
            }
            seen += got.len();
            assert_eq!(got, expected);
        }
        assert!(seen > 10_000, "too few roots: {seen}");
    }
}

/// Differential tests for the `WINDOW_MEAN` kernel against a direct
/// loop model.
#[cfg(test)]
mod fin_vmkernc_differential {
    use super::vm_window::{self, MeanOut};
    use pinewasm_mir::Value;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        fn cell(&mut self, na_rate: u64) -> Value {
            match self.below(na_rate + 12) {
                0 => Value::Bool(true),
                1 => Value::String("s".into()),
                2..=4 => Value::Int(self.below(19) as i64 - 9),
                n if n < 12 => Value::Float(match self.below(5) {
                    0 => 1.0e308,
                    1 => -0.0,
                    2 => 0.1,
                    3 => -3.25,
                    _ => 7.5,
                }),
                _ => Value::Na,
            }
        }
    }

    /// Model of the `exec_window_mean` computation (without metering,
    /// slot lookup and the length-0 guard); `Err(index)` is a type mismatch
    /// at that ring index.
    fn reference(history: &[Value], bar: usize, len: usize, mask: usize) -> Result<Value, usize> {
        if bar + 1 < len {
            return Ok(Value::Na);
        }
        let start = (bar + 1 - len) & mask;
        let mut sum = 0.0f64;
        let mut all_present = true;
        let first_len = len.min(history.len() - start);
        'window: for (base, cells) in [
            (start, &history[start..start + first_len]),
            (0, &history[..len - first_len]),
        ] {
            for (i, cell) in cells.iter().enumerate() {
                match cell {
                    Value::Float(v) => sum += v,
                    Value::Int(v) => sum += *v as f64,
                    Value::Na => {
                        all_present = false;
                        break 'window;
                    }
                    _ => return Err(base + i),
                }
            }
        }
        Ok(if all_present {
            Value::from_f64(sum / len as f64)
        } else {
            Value::Na
        })
    }

    fn kernel(history: &[Value], bar: usize, len: u32, mask: usize) -> Result<Value, usize> {
        match vm_window::window_mean::<f64, Value>(history, bar, len, mask) {
            MeanOut::Na => Ok(Value::Na),
            MeanOut::Mean(mean) => Ok(Value::from_f64(mean)),
            MeanOut::Mismatch(index) => Err(index),
        }
    }

    fn same(a: &Result<Value, usize>, b: &Result<Value, usize>) -> bool {
        match (a, b) {
            (Ok(Value::Float(x)), Ok(Value::Float(y))) => x.to_bits() == y.to_bits(),
            _ => a == b,
        }
    }

    #[test]
    fn window_mean_kernel_matches_reference() {
        let mut rng = Rng(0x5EED_CAFE);
        let (mut means, mut nas, mut mismatches) = (0u32, 0u32, 0u32);
        for round in 0..20_000 {
            let ring = 1usize << rng.below(6);
            let mask = ring - 1;
            let na_rate = if round % 2 == 0 { 0 } else { 3 };
            let history: Vec<Value> = (0..ring).map(|_| rng.cell(na_rate)).collect();
            let len = 1 + rng.below(ring as u64) as u32;
            let bar = match rng.below(4) {
                0 => rng.below(ring as u64) as usize,
                _ => rng.below(1000) as usize,
            };
            let expected = reference(&history, bar, len as usize, mask);
            let got = kernel(&history, bar, len, mask);
            assert!(
                same(&got, &expected),
                "ring {history:?} bar {bar} len {len}: {got:?} vs {expected:?}"
            );
            match expected {
                Ok(Value::Na) => nas += 1,
                Ok(_) => means += 1,
                Err(_) => mismatches += 1,
            }
        }
        assert!(means > 1_000 && nas > 1_000 && mismatches > 1_000);
    }

    #[test]
    fn window_mean_kernel_panics_where_the_reference_panics() {
        // A window longer than the ring (never built by the VM: the ring is
        // sized from the history depth) panics in both versions.
        let history = vec![Value::Float(1.0); 2];
        let r = std::panic::catch_unwind(|| reference(&history, 9, 7, 1));
        let k = std::panic::catch_unwind(|| kernel(&history, 9, 7, 1));
        assert!(r.is_err() && k.is_err());
    }
}

/// The charge-then-allocate collection arms refund
/// their string charge when the object cap rejects the allocation.
#[cfg(test)]
mod fin_vmkernc_f3 {
    use super::{MarketData, RunOptions, RuntimeErrorKind};
    use pinewasm_csil::CollectionFunc;
    use pinewasm_mir::{ElemType, Heap, Value};

    fn vm_with_one_object_slot() -> super::Vm {
        let program = {
            let parsed = pinewasm_parse::parse("indicator(\"C\")\nplot(close)\n");
            let analysis = pinewasm_semantics::analyze(&parsed.program);
            let mir = pinewasm_mir::lower(&analysis.hir);
            pinewasm_csil::lower_mir(&mir).expect("CSIL lowering")
        };
        let data = MarketData {
            open: vec![1.0],
            high: vec![1.0],
            low: vec![1.0],
            close: vec![1.0],
            volume: vec![1.0],
            ..MarketData::default()
        };
        let options = RunOptions::default();
        let mut vm = super::Vm::new(&program, &data, &options).expect("small VM state");
        vm.heap = Heap::with_limits(1_000, 1);
        vm
    }

    fn string(s: &str) -> Value {
        Value::String(s.to_string())
    }

    fn string_map_with_entry(vm: &mut super::Vm) -> Value {
        let string_tag = Value::Int(ElemType::String.to_tag());
        let map = vm
            .call_collection(CollectionFunc::MapNew, &[string_tag.clone(), string_tag], 0)
            .expect("string map");
        vm.call_collection(
            CollectionFunc::MapPut,
            &[map.clone(), string("k"), string("v")],
            0,
        )
        .expect("entry");
        map
    }

    /// Charge-then-allocate arms refund direct-string charges when the object
    /// cap rejects allocation. The object-array arm is also exercised with a
    /// valid reference element; it has no direct string charge to refund.
    #[test]
    fn object_limit_after_string_charge_refunds_the_charge() {
        type Setup = fn(&mut super::Vm) -> Vec<Value>;
        let cases: Vec<(CollectionFunc, Setup)> = vec![
            (CollectionFunc::ArrayNewString, |vm| {
                vm.heap
                    .alloc(pinewasm_mir::HeapObj::Array {
                        elem_ty: ElemType::Int,
                        items: vec![],
                    })
                    .expect("occupy the only object slot");
                vec![Value::Int(2), string("abc")]
            }),
            (CollectionFunc::ArrayNewObject, |vm| {
                let source = vm
                    .heap
                    .alloc(pinewasm_mir::HeapObj::Object {
                        table_identity: None,
                        type_id: 0,
                        fields: vec![],
                    })
                    .expect("source object");
                vec![
                    Value::Int(1),
                    Value::Ref(source),
                    Value::Int(ElemType::Object(0).to_tag()),
                ]
            }),
            (CollectionFunc::ArrayFrom, |vm| {
                vm.heap
                    .alloc(pinewasm_mir::HeapObj::Array {
                        elem_ty: ElemType::Int,
                        items: vec![],
                    })
                    .expect("occupy the only object slot");
                vec![string("ab"), string("c")]
            }),
            (CollectionFunc::StrSplit, |vm| {
                vm.heap
                    .alloc(pinewasm_mir::HeapObj::Array {
                        elem_ty: ElemType::Int,
                        items: vec![],
                    })
                    .expect("occupy the only object slot");
                vec![string("a,b"), string(",")]
            }),
            (CollectionFunc::ArrayCopy, |vm| {
                let a = vm
                    .call_collection(CollectionFunc::ArrayFrom, &[string("xy")], 0)
                    .expect("source array");
                vec![a]
            }),
            (CollectionFunc::ArraySlice, |vm| {
                let a = vm
                    .call_collection(CollectionFunc::ArrayFrom, &[string("xy"), string("z")], 0)
                    .expect("source array");
                vec![a, Value::Int(0), Value::Int(2)]
            }),
            (CollectionFunc::MatrixCopy, |vm| {
                let tag = Value::Int(ElemType::String.to_tag());
                let m = vm
                    .call_collection(
                        CollectionFunc::MatrixNew,
                        &[Value::Int(1), Value::Int(2), string("q"), tag],
                        0,
                    )
                    .expect("source matrix");
                vec![m]
            }),
            (CollectionFunc::MatrixRow, |vm| {
                let tag = Value::Int(ElemType::String.to_tag());
                let m = vm
                    .call_collection(
                        CollectionFunc::MatrixNew,
                        &[Value::Int(1), Value::Int(2), string("q"), tag],
                        0,
                    )
                    .expect("source matrix");
                vec![m, Value::Int(0)]
            }),
            (CollectionFunc::MatrixCol, |vm| {
                let tag = Value::Int(ElemType::String.to_tag());
                let m = vm
                    .call_collection(
                        CollectionFunc::MatrixNew,
                        &[Value::Int(1), Value::Int(2), string("q"), tag],
                        0,
                    )
                    .expect("source matrix");
                vec![m, Value::Int(0)]
            }),
            (CollectionFunc::MatrixSubmatrix, |vm| {
                let tag = Value::Int(ElemType::String.to_tag());
                let m = vm
                    .call_collection(
                        CollectionFunc::MatrixNew,
                        &[Value::Int(1), Value::Int(2), string("q"), tag],
                        0,
                    )
                    .expect("source matrix");
                vec![
                    m,
                    Value::Int(0),
                    Value::Int(1),
                    Value::Int(0),
                    Value::Int(2),
                ]
            }),
            (CollectionFunc::MatrixTranspose, |vm| {
                let tag = Value::Int(ElemType::String.to_tag());
                let m = vm
                    .call_collection(
                        CollectionFunc::MatrixNew,
                        &[Value::Int(1), Value::Int(2), string("q"), tag],
                        0,
                    )
                    .expect("source matrix");
                vec![m]
            }),
            (CollectionFunc::MapCopy, |vm| {
                vec![string_map_with_entry(vm)]
            }),
            (CollectionFunc::MapKeys, |vm| {
                vec![string_map_with_entry(vm)]
            }),
            (CollectionFunc::MapValues, |vm| {
                vec![string_map_with_entry(vm)]
            }),
        ];
        assert_eq!(
            cases.len(),
            14,
            "cover every allocation arm that can charge string bytes"
        );
        for (func, setup) in cases {
            let mut vm = vm_with_one_object_slot();
            let args = setup(&mut vm);
            let charged = vm.string_bytes_allocated;
            let objects = vm.heap.len();
            let error = vm
                .call_collection(func, &args, 0)
                .expect_err("the result needs a second object slot");
            assert_eq!(
                error.kind,
                RuntimeErrorKind::CollectionLimitExceeded,
                "{func:?}"
            );
            assert_eq!(
                vm.string_bytes_allocated, charged,
                "{func:?} kept its charge"
            );
            assert_eq!(vm.heap.len(), objects, "{func:?}");
        }
    }
}

/// Differential checks for the `ArrayNew` kernel against a direct model of the
/// operation. The model below deliberately spells out each operation
/// rather than calling the extracted helpers, so the comparison can catch an
/// ordering or accounting regression in the kernel itself.
#[cfg(test)]
mod fin_vmkernc_array_new_differential {
    use super::vm_heap_ops::{self, HeapOpError};
    use super::{MarketData, RunOptions, RuntimeErrorKind, Vm};
    use pinewasm_hir::CollectionFunc;
    use pinewasm_mir::{ElemType, HandleId, Heap, HeapLimitKind, HeapObj, Value};

    fn vm_with_options(options: RunOptions) -> Vm {
        let parsed = pinewasm_parse::parse("indicator(\"C\")\nplot(close)\n");
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        let mir = pinewasm_mir::lower(&analysis.hir);
        let program = pinewasm_csil::lower_mir(&mir).expect("CSIL lowering");
        let data = MarketData {
            open: vec![1.0],
            high: vec![1.0],
            low: vec![1.0],
            close: vec![1.0],
            volume: vec![1.0],
            ..MarketData::default()
        };
        Vm::new(&program, &data, &options).expect("small VM state")
    }

    fn array_new_reference(
        heap: &mut Heap,
        allocated: &mut usize,
        max_string: usize,
        size: usize,
        elem_ty: ElemType,
        initial_arg: &Value,
    ) -> Result<HandleId, HeapOpError> {
        // Inline order: reserve/release preflight, checked
        // repeated-string projection, coercion, string charge, then alloc.
        if let Err(error) = heap.reserve(size) {
            return Err(HeapOpError::Heap(error));
        }
        heap.release(size);

        let bytes = match initial_arg {
            Value::String(value) => match value.len().checked_mul(size) {
                Some(bytes) => bytes,
                None => return Err(HeapOpError::StringCloneOverflow),
            },
            _ => 0,
        };
        let initial = match (elem_ty, initial_arg) {
            (_, Value::Na) => Value::Na,
            (ElemType::Float, Value::Float(_)) => initial_arg.clone(),
            (ElemType::Float, Value::Int(value)) => Value::from_f64(*value as f64),
            (ElemType::Int, Value::Int(_)) => initial_arg.clone(),
            (ElemType::Bool, Value::Bool(_)) => initial_arg.clone(),
            (ElemType::String, Value::String(_)) => initial_arg.clone(),
            (ElemType::Color, Value::Color { .. }) => initial_arg.clone(),
            (ElemType::Object(_), Value::Ref(_)) => initial_arg.clone(),
            _ => return Err(HeapOpError::ElemMismatch(elem_ty)),
        };

        let charged_before = *allocated;
        let attempted = allocated.saturating_add(bytes);
        if attempted > max_string {
            return Err(HeapOpError::StringLimit { attempted });
        }
        *allocated = attempted;
        let items = vec![initial; size];
        match heap.alloc(HeapObj::Array { elem_ty, items }) {
            Ok(handle) => Ok(handle),
            Err(error) => {
                *allocated = charged_before;
                Err(HeapOpError::Heap(error))
            }
        }
    }

    fn assert_matches_reference(
        heap: Heap,
        allocated: usize,
        max_string: usize,
        size: usize,
        elem_ty: ElemType,
        initial: &Value,
    ) -> (Result<HandleId, HeapOpError>, Heap, usize) {
        let before_heap = heap.clone();
        let before_allocated = allocated;

        let mut reference_heap = heap.clone();
        let mut reference_allocated = allocated;
        let expected = array_new_reference(
            &mut reference_heap,
            &mut reference_allocated,
            max_string,
            size,
            elem_ty,
            initial,
        );

        let mut actual_heap = heap;
        let mut actual_allocated = allocated;
        let actual = vm_heap_ops::array_new(
            &mut actual_heap,
            &mut actual_allocated,
            max_string,
            size,
            elem_ty,
            initial,
        );

        assert_eq!(actual, expected);
        assert_eq!(actual_heap, reference_heap);
        assert_eq!(actual_allocated, reference_allocated);
        if actual.is_err() {
            assert_eq!(
                actual_heap, before_heap,
                "failed construction changed heap state"
            );
            assert_eq!(
                actual_allocated, before_allocated,
                "failed construction changed string accounting"
            );
        }
        (actual, actual_heap, actual_allocated)
    }

    #[test]
    fn success_zero_size_and_repeated_string_bytes_match_the_reference() {
        let (created, heap, allocated) = assert_matches_reference(
            Heap::with_limits(8, 8),
            0,
            6,
            3,
            ElemType::String,
            &Value::String("xy".to_string()),
        );
        let handle = created.expect("three string elements fit both budgets");
        assert_eq!(allocated, 6);
        assert_eq!(heap.live_elements(), 3);
        assert_eq!(
            heap.get(handle),
            Some(&HeapObj::Array {
                elem_ty: ElemType::String,
                items: vec![Value::String("xy".to_string()); 3],
            })
        );

        let (created, heap, allocated) = assert_matches_reference(
            Heap::with_limits(0, 1),
            4,
            4,
            0,
            ElemType::String,
            &Value::String("unused".to_string()),
        );
        let handle = created.expect("zero-size arrays need no element or string budget");
        assert_eq!(allocated, 4);
        assert_eq!(heap.live_elements(), 0);
        assert_eq!(heap.len(), 1);
        assert_eq!(
            heap.get(handle),
            Some(&HeapObj::Array {
                elem_ty: ElemType::String,
                items: vec![],
            })
        );
    }

    #[test]
    fn capacity_and_coercion_errors_keep_their_source_precedence() {
        let (result, _, _) = assert_matches_reference(
            Heap::with_limits(0, 4),
            0,
            0,
            1,
            ElemType::Float,
            &Value::String("x".to_string()),
        );
        assert!(matches!(
            result,
            Err(HeapOpError::Heap(error)) if error.kind == HeapLimitKind::Elements
        ));

        let (result, _, _) = assert_matches_reference(
            Heap::with_limits(4, 4),
            2,
            2,
            1,
            ElemType::Float,
            &Value::String("x".to_string()),
        );
        assert_eq!(result, Err(HeapOpError::ElemMismatch(ElemType::Float)));
    }

    #[test]
    fn cumulative_string_limit_failure_is_atomic_and_reports_the_attempted_total() {
        let (result, heap, allocated) = assert_matches_reference(
            Heap::with_limits(8, 8),
            5,
            10,
            3,
            ElemType::String,
            &Value::String("xy".to_string()),
        );
        assert_eq!(result, Err(HeapOpError::StringLimit { attempted: 11 }));
        assert_eq!(heap.len(), 0);
        assert_eq!(heap.live_elements(), 0);
        assert_eq!(allocated, 5);
    }

    #[test]
    fn object_cap_failure_refunds_the_string_charge_and_preserves_the_heap() {
        let mut heap = Heap::with_limits(8, 1);
        heap.alloc(HeapObj::Object {
            table_identity: None,
            type_id: 0,
            fields: vec![],
        })
        .expect("occupy the only object slot");
        let before_heap = heap.clone();
        let (result, after_heap, allocated) = assert_matches_reference(
            heap,
            1,
            3,
            2,
            ElemType::String,
            &Value::String("q".to_string()),
        );
        assert!(matches!(
            result,
            Err(HeapOpError::Heap(error)) if error.kind == HeapLimitKind::Objects
        ));
        assert_eq!(after_heap, before_heap);
        assert_eq!(allocated, 1);
    }

    #[test]
    fn dispatcher_preserves_capacity_and_type_errors_and_decodes_object_type() {
        let mut vm = vm_with_options(RunOptions {
            max_collection_elements: 0,
            max_string_bytes_per_run: 0,
            ..RunOptions::default()
        });
        let before_heap = vm.heap.clone();
        let error = vm
            .call_collection(
                CollectionFunc::ArrayNewFloat,
                &[Value::Int(1), Value::String("x".to_string())],
                0,
            )
            .expect_err("capacity preflight precedes coercion and string charging");
        assert_eq!(error.kind, RuntimeErrorKind::CollectionLimitExceeded);
        assert_eq!(vm.heap, before_heap);
        assert_eq!(vm.string_bytes_allocated, 0);

        let mut vm = vm_with_options(RunOptions {
            max_collection_elements: 4,
            max_string_bytes_per_run: 0,
            ..RunOptions::default()
        });
        let error = vm
            .call_collection(
                CollectionFunc::ArrayNewFloat,
                &[Value::Int(1), Value::String("x".to_string())],
                0,
            )
            .expect_err("coercion precedes the cumulative string charge");
        assert_eq!(error.kind, RuntimeErrorKind::TypeMismatch);
        assert!(error.message.contains("array holds float, got string"));
        assert_eq!(vm.heap.len(), 0);
        assert_eq!(vm.string_bytes_allocated, 0);

        let mut vm = vm_with_options(RunOptions::default());
        let source = vm
            .heap
            .alloc(HeapObj::Object {
                table_identity: None,
                type_id: 7,
                fields: vec![],
            })
            .expect("source object");
        let result = vm
            .call_collection(
                CollectionFunc::ArrayNewObject,
                &[
                    Value::Int(1),
                    Value::Ref(source),
                    Value::Int(ElemType::Object(7).to_tag()),
                ],
                0,
            )
            .expect("object constructor uses the shared kernel");
        let Value::Ref(array) = result else {
            panic!("array constructor returns a heap reference")
        };
        assert_eq!(
            vm.heap.get(array),
            Some(&HeapObj::Array {
                elem_ty: ElemType::Object(7),
                items: vec![Value::Ref(source)],
            })
        );
    }
}
