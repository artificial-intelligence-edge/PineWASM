//! The strategy order fill simulator.
//!
//! This module owns the order book and the fill loop that turns the
//! `strategy.*` order primitives into trades, a signed position, and a per-bar
//! equity series. It is a pure, browser-agnostic state machine driven by the
//! VM's bar loop. Program-input failures are expected to remain structured:
//! every out-of-range or nonsensical order is dropped or clamped, not aborted.
//!
//! # Fill model
//!
//! These are PineWASM's implemented rules. Each rule is pinned by a
//! value-asserting test in `tests.rs`.
//!
//! * Orders placed while executing bar `N` are **pending**; they become
//!   eligible on the next bar (`N+1`), or on the **same** bar's close when
//!   `process_orders_on_close` is set.
//! * A **market** order fills at the eligible bar's `open` (or `close` under
//!   `process_orders_on_close`).
//! * A **limit** order fills when the bar trades at its price or better:
//!   a buy limit fills at `min(open, limit)` when `low <= limit`; a sell limit
//!   fills at `max(open, limit)` when `high >= limit`. (If the open already
//!   satisfies the limit, the fill is at the open — the simulator does not wait.)
//! * A **stop** order fills when the bar trades through the stop:
//!   a buy stop fills at `max(open, stop)` when `high >= stop`; a sell stop
//!   fills at `min(open, stop)` when `low <= stop`. (If the open is already past
//!   the stop, the fill is at the open.)
//! * **Both a limit and a stop on the same entry/order** create a stop-limit
//!   order: crossing the stop activates its limit, which fills only when the
//!   remaining path reaches the limit at that price or better.
//! * **Slippage**: `slippage` ticks × `mintick` are added adversely to market
//!   and stop fills (worse for the filling side), not to limit-only fills.
//! * **Commission**: `percent` (of fill notional), `cash_per_contract`, or
//!   `cash_per_order`, charged on every fill and subtracted from realized `PnL`.
//! * **Pyramiding**: market `strategy.entry` calls are blocked once the count
//!   of open and queued same-direction entries reaches `max(1, pyramiding)`;
//!   raw values 0 and 1 therefore both allow one entry. Price-based entry calls
//!   made while the open-trade count is below the cap may coexist in the book;
//!   if several trigger, all of them fill even when that exceeds the cap.
//!   `strategy.order` ignores pyramiding.
//! * A **reversing** entry (opposite direction, larger size) closes the whole
//!   open position and opens the remainder in the new direction.

use crate::{
    BrokerError, BrokerLimits, CommissionSplit, StrategyPosition, StrategyProperties, StrategyTrade,
};
use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Closed-trade detail is retained only while both of the trade's boundary
/// orders remain inside the latest 9,000 successfully executed orders. This is
/// deliberately an order window, not a closed-trade row cap: one fill can close
/// several lots, and an old entry can make a newly closed row immediately
/// ineligible for detail retention.
const MAX_RETAINED_FILLED_ORDERS: u64 = 9_000;

/// The ordinal of a closed-trade row's older boundary fill. The row leaves
/// the retention window as soon as this fill does.
fn retention_ordinal(trade: &StrategyTrade) -> u64 {
    trade.entry_order_ordinal.min(trade.exit_order_ordinal)
}

/// Bounds over the retained closed-trade rows that let the broker evict
/// exactly the aged-out rows without visiting every row after every fill.
///
/// Rows are appended in close order, so the advancing window usually ages out
/// a prefix of them. A row can age out behind the front only if a row ahead
/// of it has a larger [`retention_ordinal`], for example a long-held lot
/// closed after a younger one under `close_entries_rule = "ANY"`. Such a row
/// is out of order.
#[derive(Debug, Clone)]
struct TradeRetention {
    /// The largest retention ordinal among the retained rows, or 0 when none
    /// is retained.
    newest_ordinal: u64,
    /// No larger than the retention ordinal of any retained out-of-order row,
    /// and `u64::MAX` when none has been recorded since the bounds were last
    /// recomputed. A stale lower value costs only an unneeded full pass.
    out_of_order_floor: u64,
}

impl TradeRetention {
    const fn new() -> Self {
        Self {
            newest_ordinal: 0,
            out_of_order_floor: u64::MAX,
        }
    }

    /// Account for `trade` as it is appended behind every retained row.
    fn record(&mut self, trade: &StrategyTrade) {
        let ordinal = retention_ordinal(trade);
        if ordinal < self.newest_ordinal {
            self.out_of_order_floor = self.out_of_order_floor.min(ordinal);
        }
        self.newest_ordinal = self.newest_ordinal.max(ordinal);
    }

    /// Remove every row whose retention ordinal is below
    /// `first_retained_order` and keep the rest in order, exactly as a
    /// `retain` over all rows would. Once the aged-out front run is popped,
    /// any aged-out row left sits behind the retained front row, so it is out
    /// of order and its ordinal is at least `out_of_order_floor`. A full pass
    /// is therefore needed only when that floor is below
    /// `first_retained_order`. The pass recomputes both bounds, so every full
    /// pass is paid for by a different out-of-order row leaving the window.
    fn prune(&mut self, trades: &mut VecDeque<StrategyTrade>, first_retained_order: u64) {
        while trades
            .front()
            .is_some_and(|trade| retention_ordinal(trade) < first_retained_order)
        {
            trades.pop_front();
        }
        if trades.is_empty() {
            *self = Self::new();
        } else if self.out_of_order_floor < first_retained_order {
            let mut newest_ordinal = 0;
            let mut out_of_order_floor = u64::MAX;
            trades.retain(|trade| {
                let ordinal = retention_ordinal(trade);
                if ordinal < first_retained_order {
                    return false;
                }
                if ordinal < newest_ordinal {
                    out_of_order_floor = out_of_order_floor.min(ordinal);
                }
                newest_ordinal = newest_ordinal.max(ordinal);
                true
            });
            *self = Self {
                newest_ordinal,
                out_of_order_floor,
            };
        }
        // Otherwise the front run held every aged-out row. The newest row
        // was not among them: each has a smaller ordinal than the retained
        // new front. `newest_ordinal` is therefore still exact.
    }
}

/// The unit of a `strategy.risk.max_drawdown`/`max_intraday_loss` threshold:
/// account currency (`strategy.cash`) or percent of equity
/// (`strategy.percent_of_equity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskUnit {
    Cash,
    PercentOfEquity,
}

#[derive(Debug, Clone)]
struct SiteRule<T> {
    site: usize,
    config: T,
}

/// Insert, update, or remove one globally-active risk rule without disturbing
/// rules registered by other syntactic calls. The VM uses the stable CSIL
/// instruction offset as `site`, so recalculations update in place instead of
/// appending one duplicate per bar or order-fill execution.
fn set_site_rule<T>(rules: &mut Vec<SiteRule<T>>, site: usize, config: Option<T>) {
    if let Some(index) = rules.iter().position(|rule| rule.site == site) {
        if let Some(config) = config {
            rules[index].config = config;
        } else {
            rules.remove(index);
        }
    } else if let Some(config) = config {
        rules.push(SiteRule { site, config });
    }
}

fn positive_count(value: f64) -> Option<u32> {
    if value.is_finite() && value > 0.0 {
        Some(libm::floor(value).min(u32::MAX as f64) as u32)
    } else {
        None
    }
}

/// Floor a positive quantity to the symbol's tradable increment. Advancing the
/// quotient by one representable value prevents decimal boundaries such as
/// `0.3 / 0.1` from becoming `2.9999999999999996` and losing one contract step.
/// Values genuinely more than one ULP below a boundary still floor normally.
fn floor_to_increment(qty: f64, increment: f64) -> f64 {
    if !qty.is_finite() || qty <= 0.0 || !increment.is_finite() || increment <= 0.0 {
        return 0.0;
    }
    let units = qty / increment;
    if !units.is_finite() {
        return qty;
    }
    // `units` is finite and positive here. Advancing its IEEE-754 bits is the
    // MSRV-compatible equivalent of `f64::next_up()` for this domain.
    let quantized = libm::floor(f64::from_bits(units.to_bits() + 1)) * increment;
    if quantized.is_finite() {
        quantized.max(0.0)
    } else {
        qty
    }
}

/// One placed-but-unfilled order in the book.
#[derive(Debug, Clone)]
pub struct PendingOrder {
    /// Stable identity of the opening order generation that created this
    /// pending entry/order. Named `strategy.exit()` calls made while flat bind
    /// to this exact generation, so a later same-id entry does not inherit an
    /// earlier exit call.
    opening_placement_id: Option<u64>,
    /// Script execution whose latest call created or modified this opening
    /// generation. Distinct same-execution calls with the same id modify this
    /// generation; later executions can add another physical generation while
    /// pyramiding capacity remains.
    opening_execution_id: Option<u64>,
    /// The order/entry id (`strategy.entry`'s `id`, or the exit's own id).
    pub id: String,
    /// True for a pyramiding-aware entry (`strategy.entry`); false for a basic
    /// `strategy.order` and for exits (which close, never open).
    pub is_entry: bool,
    /// Direction sign: `+1.0` buy (long), `-1.0` sell (short).
    pub sign: f64,
    /// Target magnitude in contracts, or `None` to resolve the strategy's
    /// `default_qty_*` at fill time (price-dependent for cash sizing).
    pub magnitude: Option<f64>,
    /// Limit price, or `None` for no limit leg.
    pub limit: Option<f64>,
    /// Stop price, or `None` for no stop leg.
    pub stop: Option<f64>,
    /// The comment attached to the order (recorded on the resulting trade).
    pub comment: Option<String>,
    /// Per-leg exit comments.  When present they override `comment` for the
    /// matching take-profit, stop-loss, or trailing-stop fill.
    pub comment_profit: Option<String>,
    pub comment_loss: Option<String>,
    pub comment_trailing: Option<String>,
    /// The explicit message exposed for a broker order-fill alert.  The three
    /// per-leg exit variants override this value for their matching fill.
    pub alert_message: Option<String>,
    pub alert_profit: Option<String>,
    pub alert_loss: Option<String>,
    pub alert_trailing: Option<String>,
    /// `disable_alert=true` suppresses the order-fill alert for this order.
    pub disable_alert: bool,
    /// Optional One-Cancels-All group.  `oca_type=none` deliberately stores no
    /// group, even when the call supplied an `oca_name`.
    pub oca_name: Option<String>,
    pub oca_type: OcaType,
    /// True when this order only reduces/closes the position (an exit/close);
    /// such orders never flip the position past flat.
    pub reduce_only: bool,
    /// Entry id whose quantity this close/exit targets. FIFO may still close
    /// older lots first; `close_entries_rule = "ANY"` prioritizes this id.
    pub target_entry: Option<String>,
    /// The concrete open-trade lot whose quantity this `strategy.exit()`
    /// child order reserved. A single exit call can create one child per open
    /// trade; keeping the reservation on the child prevents later calls from
    /// claiming the same quantity. Other reduce-only orders leave this unset.
    pub target_lot: Option<u64>,
    /// For entry/order calls carrying both stop and limit, the stop activates a
    /// resting limit. This state survives bars until that limit fills/cancels.
    pub stop_limit_activated: bool,
    /// Trailing-stop activation level and offset (in ticks), plus the most
    /// favorable price observed after activation.  The latter is live order
    /// state and therefore survives across bars until fill/cancellation.
    pub trail_activation: Option<f64>,
    pub trail_offset_ticks: Option<f64>,
    pub trail_best: Option<f64>,
    /// Label recorded on a closing order made by `strategy.close`
    /// (`"Close entry(s) order <id>"`) or `strategy.close_all`
    /// (`"Close position order"`). `None` for every other order.
    exit_order_name: Option<String>,
}

/// The three `strategy.oca.*` policies.  The group identity is the pair
/// `(oca_name, oca_type)`, so equal names with different types form distinct
/// groups.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OcaType {
    #[default]
    None,
    Cancel,
    Reduce,
}

impl OcaType {
    pub fn from_token(token: Option<&str>) -> Self {
        match token {
            Some("strategy.oca.cancel") => Self::Cancel,
            Some("strategy.oca.reduce") => Self::Reduce,
            _ => Self::None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FillKind {
    Market,
    Limit,
    Stop,
    Trailing,
}

/// An explicit order-fill alert produced by the order fill simulator.  Creating a
/// running alert remains a host/UI concern; this event is the
/// deterministic trigger/message surface a host can subscribe to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderFillAlert {
    pub bar: u32,
    pub message: String,
}

/// Metadata common to `strategy.entry`, `strategy.order`, `strategy.close`,
/// and `strategy.close_all`.  Keeping it bundled prevents the canonical CSIL
/// operand order from leaking into the broker API.
#[derive(Debug, Clone, Default)]
pub struct OrderMetadata {
    pub oca_name: Option<String>,
    pub oca_type: OcaType,
    pub comment: Option<String>,
    pub alert_message: Option<String>,
    pub disable_alert: bool,
}

/// Text/OCA metadata specific to a `strategy.exit` bracket.
#[derive(Debug, Clone, Default)]
pub struct ExitMetadata {
    pub oca_name: Option<String>,
    pub comment: Option<String>,
    pub comment_profit: Option<String>,
    pub comment_loss: Option<String>,
    pub comment_trailing: Option<String>,
    pub alert_message: Option<String>,
    pub alert_profit: Option<String>,
    pub alert_loss: Option<String>,
    pub alert_trailing: Option<String>,
    pub disable_alert: bool,
}

/// Which declared order price is being rounded to the tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrderPriceKind {
    Limit,
    Stop,
}

/// The price/sizing recipe retained for a `strategy.exit()` call without a
/// `from_entry` argument. Pine keeps such a call alive until the position is
/// flat and creates a separate exit child for every later entry. Storing the
/// recipe (rather than a pre-resolved aggregate order) is important because
/// relative price levels are based on each trade's own entry price.
#[derive(Debug, Clone)]
struct PersistentExit {
    id: String,
    qty: Option<f64>,
    qty_percent: Option<f64>,
    profit: Option<f64>,
    limit: Option<f64>,
    loss: Option<f64>,
    stop: Option<f64>,
    trail_price: Option<f64>,
    trail_points: Option<f64>,
    trail_offset: Option<f64>,
    metadata: ExitMetadata,
}

/// A named exit issued after a matching opening order was placed but before it
/// filled. The recipe is scoped to those exact order generations; unlike a
/// no-`from_entry` persistent exit, it never attaches to later same-id entries.
#[derive(Debug, Clone)]
struct DeferredNamedExit {
    template: PersistentExit,
    eligible_opening_placements: Vec<u64>,
}

/// Full-run report state intentionally independent from the public retained
/// trade deque.
#[derive(Debug, Clone, Default)]
struct ReportScopeAccumulator {
    aggregate: crate::StrategyReportScopeAggregate,
}

/// One lot of the open position, retained so exits realize FIFO against the
/// original entry price and bar.
#[derive(Debug, Clone)]
struct Lot {
    /// Stable identity used by per-trade exit reservations. Entry ids are not
    /// unique when pyramiding, so they cannot identify one open trade.
    id: u64,
    entry_id: String,
    entry_bar: usize,
    entry_price: f64,
    /// Strategy equity immediately before this physical trade opened. For a
    /// pyramided leg this includes then-current MTM on earlier open legs; that
    /// branch follows the rule literally and is not illustrated by a worked
    /// example.
    equity_on_entry: f64,
    /// Lowest closed-trade equity observed before this trade opened.
    /// Run-up is anchored here, not at unrealized troughs reached during this
    /// trade.
    min_equity_before_entry: f64,
    /// Symbol-to-account FX rate in force when this lot opened. Missing host
    /// conversion data deliberately falls back to identity but is surfaced as
    /// incomplete in the run outcome.
    entry_fx_rate: f64,
    /// Account-currency entry notional for one contract, frozen at the entry
    /// bar's FX rate. Partial exits multiply this by the quantity closed.
    entry_account_notional_per_unit: f64,
    /// Zero-based ordinal of the successful order fill that opened this lot.
    /// Retention of a later closed-trade row depends on both this ordinal and
    /// the closing fill's ordinal.
    entry_order_ordinal: u64,
    /// The opening order's comment, retained for `*trades.entry_comment`.
    entry_comment: Option<String>,
    /// Signed remaining quantity of this lot (same sign as the position).
    qty: f64,
    /// Entry quantity before partial exits. A `strategy.exit(qty_percent=...)`
    /// percentage is based on the initial quantity, not the quantity left after
    /// an earlier exit.
    initial_qty: f64,
    /// The opening commission charged for this lot, per unit of opened
    /// quantity. While open, `open_profit` subtracts this carried amount; on
    /// close its pro-rated share moves into the trade's realized `pnl`. It is
    /// never also deducted from `realized` at entry.
    entry_commission_per_unit: f64,
    /// Unrealized profit at the most recent mark price, including the entry
    /// commission share carried by this open lot.
    current_profit: f64,
    /// Largest favorable excursion reached by this open lot so far.
    max_runup: f64,
    /// Largest adverse excursion reached by this open lot so far.
    max_drawdown: f64,
}

/// The order fill simulator state for one strategy run.
#[derive(Debug, Clone)]
pub struct Broker {
    initial_capital: f64,
    default_qty_type: String,
    default_qty_value: f64,
    commission_type: String,
    commission_value: f64,
    /// Slippage in ticks; multiplied by `mintick` for the price adjustment.
    slippage_ticks: f64,
    mintick: f64,
    /// Currency value of a one-point move for one contract.
    pointvalue: f64,
    /// Smallest tradable quantity increment.
    mincontract: f64,
    /// Price observed by the current script execution, distinct from a later fill.
    execution_price: f64,
    pyramiding: u32,
    process_orders_on_close: bool,
    calc_on_every_tick: bool,
    calc_on_order_fills: bool,
    calc_on_every_history_tick: bool,
    backtest_fill_limits_assumption: u32,
    close_entries_rule: String,
    margin_long: f64,
    margin_short: f64,
    /// The account currency token (`strategy(currency=)`), used by
    /// `strategy.convert_to_account/symbol`. Empty/"NONE" means unset (identity).
    account_currency: String,
    /// Quote currency of the chart symbol (`syminfo.currency`).
    symbol_currency: String,
    /// Symbol-to-account rate for the current broker bar. Missing/invalid data
    /// on a differing-currency run uses 1.0 for compatibility while the
    /// corresponding rate entry remains `None`.
    current_currency_rate: f64,
    /// Resolved symbol-to-account rates aligned with the broker's equity lane.
    /// Identity conversion is `Some(1.0)`; unavailable differing-currency data
    /// is `None`.
    currency_rates: Vec<Option<f64>>,
    /// Every syntactic `strategy.risk.max_position_size` cap, keyed by its
    /// stable CSIL call site. The strictest (smallest) positive cap applies.
    max_position_size_rules: Vec<SiteRule<f64>>,
    /// Every `strategy.risk.allow_entry_in` restriction, keyed by call site.
    /// Each config is `+1` long-only, `-1` short-only, or `0` both. A direction
    /// is allowed only when every registered rule allows it (intersection).
    allow_entry_rules: Vec<SiteRule<i8>>,
    /// Every `strategy.risk.max_drawdown(value, type)` threshold. Each rule is
    /// evaluated in its own unit so cash and percent calls remain simultaneously
    /// active; the first strict rule to trip supplies the alert message.
    risk_max_drawdown_rules: Vec<SiteRule<(f64, RiskUnit, Option<String>)>>,
    /// Highest equity observed for max-drawdown risk enforcement.
    drawdown_peak_equity: f64,
    /// True once a permanent risk rule has halted new order placement.
    risk_halted: bool,
    /// Every `strategy.risk.max_intraday_loss(value, type)` threshold.
    risk_max_intraday_loss_rules: Vec<SiteRule<(f64, RiskUnit, Option<String>)>>,
    /// Every `strategy.risk.max_intraday_filled_orders(count)` threshold.
    risk_max_intraday_filled_orders_rules: Vec<SiteRule<(u32, Option<String>)>>,
    /// Every `strategy.risk.max_cons_loss_days(count)` threshold.
    risk_max_cons_loss_days_rules: Vec<SiteRule<(u32, Option<String>)>>,
    /// Count of consecutive completed risk sessions whose ending equity was
    /// lower than their starting equity.
    consecutive_loss_days: u32,
    /// Number of filled script orders in the current risk session.
    intraday_filled_orders: u32,
    /// Current risk session/day key, supplied by the VM from bar timestamps.
    current_session_key: Option<i64>,
    /// Equity baseline for intraday loss and completed-day loss accounting.
    session_start_equity: f64,
    /// Maximum marked account equity observed in the current risk session.
    /// `strategy.risk.max_intraday_loss` measures giveback from this high-water
    /// mark, while `session_start_equity` remains the loss-day baseline.
    intraday_peak_equity: f64,
    /// Last marked equity in the current session, used to close out the
    /// previous session when a new key starts.
    last_session_equity: Option<f64>,
    /// A non-permanent halt that expires when `current_session_key` changes.
    intraday_halt_key: Option<i64>,

    /// Orders placed on the current bar, awaiting their eligible bar.
    pending: Vec<PendingOrder>,
    /// Active no-`from_entry` exit recipes. Named exits are intentionally not
    /// retained: they only apply to trades existing on or before the bar of the
    /// call, even when later trades reuse the same id.
    persistent_exits: Vec<PersistentExit>,
    /// Named exits awaiting opening orders that already existed at the call.
    deferred_named_exits: Vec<DeferredNamedExit>,
    next_opening_placement_id: u64,
    /// Monotonic identity for each main-script calculation, including
    /// fill-triggered recalculations on the same available tick.
    opening_execution_id: u64,
    /// Entry-slot occupancy frozen at the beginning of that calculation.
    /// Price-based `strategy.entry()` calls use this snapshot so distinct
    /// orders generated by one execution share the pyramiding exception.
    opening_execution_long_occupancy: u32,
    opening_execution_short_occupancy: u32,
    next_lot_id: u64,
    /// Explicit `alert_message` events produced by successful order fills.
    order_fill_alerts: Vec<OrderFillAlert>,
    /// Same-tick fills created by `strategy.close*(immediately=true)`.  The VM
    /// drains this counter after an execution to schedule
    /// `calc_on_order_fills` recalculation without exposing broker internals.
    immediate_fill_count: usize,
    /// The open position, oldest lot first (FIFO).
    lots: Vec<Lot>,
    /// Realized profit so far (net of commission), used for equity.
    realized: f64,
    /// Closed trades in close order.
    trades: VecDeque<StrategyTrade>,
    /// Retention bounds over `trades`. Every row enters `trades` through
    /// `record_closed_trade` and leaves through `prune_closed_trade_detail`,
    /// which keep these current.
    trade_retention: TradeRetention,
    /// Number of successfully executed broker orders across the whole run.
    /// The current value is also the zero-based ordinal assigned to the next
    /// successful fill. One multi-lot close or reversal advances it once.
    successful_order_fills_total: u64,
    /// Durable benchmark anchor captured by the first successfully executed
    /// opening leg. Unlike closed-trade detail, these fields survive both an
    /// entry that remains open and the 9,000-filled-order retention window.
    first_entry_bar: Option<u32>,
    first_entry_price: Option<f64>,
    first_entry_account_notional_per_unit: Option<f64>,
    /// Global count and incremental metrics include records whose entry or
    /// exit orders have aged out of the retained 9,000-filled-order window.
    closed_trades_total: u32,
    gross_profit: f64,
    gross_loss: f64,
    win_trades: u32,
    loss_trades: u32,
    even_trades: u32,
    realized_peak: f64,
    realized_trough: f64,
    max_drawdown: f64,
    max_runup: f64,
    // Report cash extrema include paid entry fees; script-visible extrema are gross.
    report_max_drawdown: f64,
    report_max_runup: f64,
    /// Peak/trough of marked account equity used only by the Pine
    /// `strategy.max_*_percent` built-ins. These percentages have a distinct
    /// peak-to-trough definition from the trade-anchored absolute report
    /// metrics above.
    equity_peak: f64,
    equity_trough: f64,
    max_drawdown_percent: f64,
    report_drawdown_percent: f64,
    max_runup_percent: f64,
    trade_percent_sum: f64,
    winning_trade_percent_sum: f64,
    losing_trade_percent_sum: f64,
    report_all: ReportScopeAccumulator,
    report_long: ReportScopeAccumulator,
    report_short: ReportScopeAccumulator,
    /// One compact full-run sample supports exact three-standard-deviation
    /// outliers for all three report scopes without retaining evicted trade
    /// rows or duplicating every P&L into All plus its direction.
    report_pnl_samples: Vec<(u32, f64, f64)>,
    /// Per-bar equity samples (`initial_capital + realized + open PnL`).
    equity: Vec<f64>,
    /// Margin required by the open lots at each completed bar's close. This
    /// is the source for the run report's average and usage figure.
    margin_used: Vec<f64>,
    /// Peak margin required at any broker-observed price/fill point.
    maximum_margin_used: f64,
    /// Whether this run ever held a position. This distinguishes a genuine
    /// zero-valued account-size requirement from a strategy that never traded.
    ever_held_position: bool,
    /// Successful forced-liquidation events generated by the margin engine.
    margin_calls: u32,
    /// A close-tick deficit is liquidated at the next available open.
    pending_close_margin: Option<(usize, f64)>,
    /// Total liquidation account notional (`quantity * abs(fill price) *
    /// pointvalue * fill-bar symbol-to-account rate`).
    total_liquidated: f64,
    /// Largest single margin-call liquidation account notional in this run.
    largest_liquidation: f64,
    /// Per-bar snapshots of the `strategy.*` read surface, one per executed
    /// bar, recorded at the start of the bar after prior-bar fills. Indexed by
    /// bar; a `strategy.position_size[1]` read indexes `snapshots[bar - 1]`.
    snapshots: Vec<Option<StrategySnapshot>>,
    /// Run report peak aggregate absolute position held at one time.
    max_contracts_held: f64,
    /// Run report peak aggregate long position.
    max_contracts_held_long: f64,
    /// Run report peak aggregate short-position magnitude.
    max_contracts_held_short: f64,
    /// Pine `strategy.max_contracts_held_*` variables are maxima for one open
    /// trade, not aggregate pyramided exposure. Keep their directional peaks
    /// separate from the report telemetry above.
    max_trade_contracts_held: f64,
    max_trade_contracts_held_long: f64,
    max_trade_contracts_held_short: f64,
    /// Entry id that initially opened the current market position; stable
    /// across pyramiding and partial FIFO closes until flat/reversal.
    position_entry_name: Option<String>,
    /// Optional bounds on the growable state, checked by the bar-walk entry
    /// points. Defaults to unlimited, matching the interpreter's behavior.
    limits: BrokerLimits,
}

/// One bar's frozen view of the `strategy.*` read surface. The VM records
/// this at the start of each bar (after prior-bar fills) and reads getters off
/// it, so a getter observes a stable within-bar value and history works.
#[derive(Debug, Clone)]
pub struct StrategySnapshot {
    pub position_size: f64,
    /// Average entry price, or `NaN` when flat (the getter maps `NaN` to `na`).
    pub position_avg_price: f64,
    pub open_profit: f64,
    pub net_profit: f64,
    pub gross_profit: f64,
    pub gross_loss: f64,
    pub equity: f64,
    pub initial_capital: f64,
    pub win_trades: i64,
    pub loss_trades: i64,
    pub even_trades: i64,
    pub closed_trades: i64,
    pub open_trades: i64,
    pub max_drawdown: f64,
    pub max_runup: f64,
    /// `net_profit / closed_trades`, or `NaN` when none.
    pub avg_trade: f64,
    /// `gross_profit / win_trades`, or `NaN` when none.
    pub avg_winning_trade: f64,
    /// `gross_loss / loss_trades`, or `NaN` when none.
    pub avg_losing_trade: f64,
    pub max_contracts_held: f64,
    pub max_contracts_held_long: f64,
    pub max_contracts_held_short: f64,
    pub account_currency: String,
    pub avg_losing_trade_percent: f64,
    pub avg_trade_percent: f64,
    pub avg_winning_trade_percent: f64,
    pub closed_trades_first_index: i64,
    pub gross_loss_percent: f64,
    pub gross_profit_percent: f64,
    pub margin_liquidation_price: f64,
    pub max_drawdown_percent: f64,
    pub max_runup_percent: f64,
    pub net_profit_percent: f64,
    pub open_profit_percent: f64,
    pub open_trades_capital_held: f64,
    pub position_entry_name: Option<String>,
}

impl Broker {
    /// Construct a broker with default symbol economics (`mintick`-only).
    /// The VM test suite's constructor; kept public because the
    /// broker no longer lives in the crate that owns those tests.
    pub fn new(info: &impl StrategyProperties, mintick: f64) -> Broker {
        Self::new_with_symbol(info, mintick, 1.0, 1.0, "")
    }

    /// Construct a broker with the symbol economics needed for sizing, P/L,
    /// commission notional, and margin calculations.
    pub fn new_with_symbol(
        info: &impl StrategyProperties,
        mintick: f64,
        pointvalue: f64,
        mincontract: f64,
        symbol_currency: &str,
    ) -> Broker {
        Broker {
            initial_capital: info.initial_capital(),
            default_qty_type: info.default_qty_type().to_string(),
            default_qty_value: info.default_qty_value(),
            commission_type: info.commission_type().to_string(),
            commission_value: info.commission_value(),
            slippage_ticks: info.slippage() as f64,
            mintick,
            // `MarketData::validate` rejects non-finite/nonpositive symbol
            // economics before constructing the broker. Keep the supplied
            // values exact here instead of silently normalizing bad data.
            pointvalue,
            mincontract,
            execution_price: f64::NAN,
            pyramiding: info.pyramiding(),
            process_orders_on_close: info.process_orders_on_close(),
            calc_on_every_tick: info.calc_on_every_tick(),
            calc_on_order_fills: info.calc_on_order_fills(),
            calc_on_every_history_tick: info.calc_on_every_history_tick(),
            backtest_fill_limits_assumption: info.backtest_fill_limits_assumption(),
            close_entries_rule: info.close_entries_rule().to_string(),
            margin_long: info.margin_long(),
            margin_short: info.margin_short(),
            account_currency: if info.currency().is_empty() || info.currency() == "NONE" {
                symbol_currency.to_string()
            } else {
                info.currency().to_string()
            },
            symbol_currency: symbol_currency.to_string(),
            current_currency_rate: 1.0,
            currency_rates: Vec::new(),
            max_position_size_rules: Vec::new(),
            allow_entry_rules: Vec::new(),
            risk_max_drawdown_rules: Vec::new(),
            drawdown_peak_equity: info.initial_capital(),
            risk_halted: false,
            risk_max_intraday_loss_rules: Vec::new(),
            risk_max_intraday_filled_orders_rules: Vec::new(),
            risk_max_cons_loss_days_rules: Vec::new(),
            consecutive_loss_days: 0,
            intraday_filled_orders: 0,
            current_session_key: None,
            session_start_equity: info.initial_capital(),
            intraday_peak_equity: info.initial_capital(),
            last_session_equity: None,
            intraday_halt_key: None,
            pending: Vec::new(),
            persistent_exits: Vec::new(),
            deferred_named_exits: Vec::new(),
            next_opening_placement_id: 1,
            opening_execution_id: 0,
            opening_execution_long_occupancy: 0,
            opening_execution_short_occupancy: 0,
            next_lot_id: 1,
            order_fill_alerts: Vec::new(),
            immediate_fill_count: 0,
            lots: Vec::new(),
            realized: 0.0,
            trades: VecDeque::new(),
            trade_retention: TradeRetention::new(),
            successful_order_fills_total: 0,
            first_entry_bar: None,
            first_entry_price: None,
            first_entry_account_notional_per_unit: None,
            closed_trades_total: 0,
            gross_profit: 0.0,
            gross_loss: 0.0,
            win_trades: 0,
            loss_trades: 0,
            even_trades: 0,
            realized_peak: info.initial_capital(),
            realized_trough: info.initial_capital(),
            max_drawdown: 0.0,
            max_runup: 0.0,
            report_max_drawdown: 0.0,
            report_max_runup: 0.0,
            equity_peak: info.initial_capital(),
            equity_trough: info.initial_capital(),
            max_drawdown_percent: 0.0,
            report_drawdown_percent: 0.0,
            max_runup_percent: 0.0,
            trade_percent_sum: 0.0,
            winning_trade_percent_sum: 0.0,
            losing_trade_percent_sum: 0.0,
            report_all: ReportScopeAccumulator::default(),
            report_long: ReportScopeAccumulator::default(),
            report_short: ReportScopeAccumulator::default(),
            report_pnl_samples: Vec::new(),
            equity: Vec::new(),
            margin_used: Vec::new(),
            maximum_margin_used: 0.0,
            ever_held_position: false,
            margin_calls: 0,
            pending_close_margin: None,
            total_liquidated: 0.0,
            largest_liquidation: 0.0,
            snapshots: Vec::new(),
            max_contracts_held: 0.0,
            max_contracts_held_long: 0.0,
            max_contracts_held_short: 0.0,
            max_trade_contracts_held: 0.0,
            max_trade_contracts_held_long: 0.0,
            max_trade_contracts_held_short: 0.0,
            position_entry_name: None,
            limits: BrokerLimits::default(),
        }
    }

    /// The signed size of the open position.
    pub fn position_size(&self) -> f64 {
        self.lots.iter().map(|l| l.qty).sum()
    }

    /// The three-way direction of the open position. `f64::signum` returns
    /// -1.0 for negative zero, but a flat position has no direction.
    fn position_sign(&self) -> f64 {
        Self::position_sign_for(self.position_size())
    }

    fn position_sign_for(size: f64) -> f64 {
        if size == 0.0 {
            0.0
        } else {
            size.signum()
        }
    }

    /// The average entry price of the open position (0.0 when flat).
    pub fn avg_price(&self) -> f64 {
        let size = self.position_size();
        if size == 0.0 {
            return 0.0;
        }
        // Weighted by absolute quantity.
        let total: f64 = self.lots.iter().map(|l| l.entry_price * l.qty.abs()).sum();
        let abs: f64 = self.lots.iter().map(|l| l.qty.abs()).sum();
        if abs == 0.0 {
            0.0
        } else {
            total / abs
        }
    }

    /// The account currency token (`strategy(currency=)`); empty or "NONE"
    /// means unset. Read by `strategy.convert_to_account/symbol`.
    pub fn account_currency(&self) -> &str {
        &self.account_currency
    }

    pub fn symbol_currency(&self) -> &str {
        &self.symbol_currency
    }

    fn currencies_require_conversion(&self) -> bool {
        let is_set = |currency: &str| !currency.is_empty() && currency != "NONE";
        is_set(&self.symbol_currency)
            && is_set(&self.account_currency)
            && self.symbol_currency != self.account_currency
    }

    /// Synchronize the broker to one market bar's symbol-to-account rate.
    /// The VM calls this before any fills, snapshots, sizing, or script reads on
    /// that bar. Equal/unset currencies resolve to identity. A missing/invalid
    /// differing-currency rate uses the identity fallback but
    /// records `None` so hosts cannot mistake the result for complete FX data.
    pub fn sync_currency_rate(&mut self, bar: usize, rate: Option<f64>) {
        let resolved = if self.currencies_require_conversion() {
            rate.filter(|value| value.is_finite() && *value > 0.0)
        } else {
            Some(1.0)
        };
        self.current_currency_rate = resolved.unwrap_or(1.0);
        if self.currency_rates.len() <= bar {
            self.currency_rates.resize(bar + 1, None);
        }
        self.currency_rates[bar] = resolved;
    }

    pub fn current_currency_rate(&self) -> f64 {
        self.current_currency_rate
    }

    /// Register one `strategy.risk.max_position_size(qty)` cap for direct
    /// broker tests. Runtime registrations use [`Self::set_max_position_size_at_site`]
    /// so separate syntactic calls cannot overwrite one another.
    pub fn set_max_position_size(&mut self, qty: f64) {
        self.set_max_position_size_at_site(0, qty);
    }

    pub fn set_max_position_size_at_site(&mut self, site: usize, qty: f64) {
        let cap = (qty.is_finite() && qty > 0.0).then_some(qty);
        set_site_rule(&mut self.max_position_size_rules, site, cap);
    }

    /// Register one `strategy.risk.allow_entry_in(value)` rule for direct
    /// broker tests. `dir` is `+1` long-only, `-1` short-only, or `0` both.
    pub fn set_allow_entry_in(&mut self, dir: i8) {
        self.set_allow_entry_in_at_site(0, dir);
    }

    pub fn set_allow_entry_in_at_site(&mut self, site: usize, dir: i8) {
        set_site_rule(&mut self.allow_entry_rules, site, Some(dir));
    }

    /// Register `strategy.risk.max_drawdown(value, type)`. Zero is a valid,
    /// immediately-triggering threshold; only a non-finite value removes the
    /// site if it has not already halted the strategy. Once halted, the halt is
    /// permanent for this run.
    pub fn set_max_drawdown(&mut self, value: f64, unit: RiskUnit) {
        self.set_max_drawdown_with_alert(value, unit, None);
    }

    pub fn set_max_drawdown_with_alert(
        &mut self,
        value: f64,
        unit: RiskUnit,
        alert_message: Option<String>,
    ) {
        self.set_max_drawdown_at_site(0, value, unit, alert_message);
    }

    pub fn set_max_drawdown_at_site(
        &mut self,
        site: usize,
        value: f64,
        unit: RiskUnit,
        alert_message: Option<String>,
    ) {
        let config = (value.is_finite() && value >= 0.0).then(|| {
            (
                value,
                unit,
                alert_message.filter(|message| !message.is_empty()),
            )
        });
        set_site_rule(&mut self.risk_max_drawdown_rules, site, config);
    }

    /// Register `strategy.risk.max_intraday_loss(value, type)`.
    pub fn set_max_intraday_loss(&mut self, value: f64, unit: RiskUnit) {
        self.set_max_intraday_loss_with_alert(value, unit, None);
    }

    pub fn set_max_intraday_loss_with_alert(
        &mut self,
        value: f64,
        unit: RiskUnit,
        alert_message: Option<String>,
    ) {
        self.set_max_intraday_loss_at_site(0, value, unit, alert_message);
    }

    pub fn set_max_intraday_loss_at_site(
        &mut self,
        site: usize,
        value: f64,
        unit: RiskUnit,
        alert_message: Option<String>,
    ) {
        let config = (value.is_finite() && value >= 0.0).then(|| {
            (
                value,
                unit,
                alert_message.filter(|message| !message.is_empty()),
            )
        });
        set_site_rule(&mut self.risk_max_intraday_loss_rules, site, config);
    }

    /// Register `strategy.risk.max_intraday_filled_orders(count)`.
    pub fn set_max_intraday_filled_orders(&mut self, count: f64) {
        self.set_max_intraday_filled_orders_with_alert(count, None);
    }

    pub fn set_max_intraday_filled_orders_with_alert(
        &mut self,
        count: f64,
        alert_message: Option<String>,
    ) {
        self.set_max_intraday_filled_orders_at_site(0, count, alert_message);
    }

    pub fn set_max_intraday_filled_orders_at_site(
        &mut self,
        site: usize,
        count: f64,
        alert_message: Option<String>,
    ) {
        let config = positive_count(count)
            .map(|count| (count, alert_message.filter(|message| !message.is_empty())));
        set_site_rule(
            &mut self.risk_max_intraday_filled_orders_rules,
            site,
            config,
        );
    }

    /// Register `strategy.risk.max_cons_loss_days(count, alert_message?)`.
    pub fn set_max_cons_loss_days(&mut self, count: f64) {
        self.set_max_cons_loss_days_with_alert(count, None);
    }

    pub fn set_max_cons_loss_days_with_alert(&mut self, count: f64, alert_message: Option<String>) {
        self.set_max_cons_loss_days_at_site(0, count, alert_message);
    }

    pub fn set_max_cons_loss_days_at_site(
        &mut self,
        site: usize,
        count: f64,
        alert_message: Option<String>,
    ) {
        let config = positive_count(count)
            .map(|count| (count, alert_message.filter(|message| !message.is_empty())));
        set_site_rule(&mut self.risk_max_cons_loss_days_rules, site, config);
    }

    // Risk directive accessors expose the stored directive so the registered
    // contract is observable (see the broker-store test).
    /// The first stored `strategy.risk.max_drawdown(value, type)` directive, or
    /// `None` if no call site has a finite, non-negative threshold.
    #[allow(dead_code)]
    pub fn risk_max_drawdown(&self) -> Option<(f64, RiskUnit)> {
        self.risk_max_drawdown_rules
            .first()
            .map(|rule| (rule.config.0, rule.config.1))
    }

    /// The stored `strategy.risk.max_intraday_loss(value, type)` directive, or
    /// `None`.
    #[allow(dead_code)]
    pub fn risk_max_intraday_loss(&self) -> Option<(f64, RiskUnit)> {
        self.risk_max_intraday_loss_rules
            .first()
            .map(|rule| (rule.config.0, rule.config.1))
    }

    /// The stored `strategy.risk.max_intraday_filled_orders(count)` directive, or
    /// `None`.
    #[allow(dead_code)]
    pub fn risk_max_intraday_filled_orders(&self) -> Option<u32> {
        self.risk_max_intraday_filled_orders_rules
            .iter()
            .map(|rule| rule.config.0)
            .min()
    }

    /// The stored `strategy.risk.max_cons_loss_days(count)` directive, or `None`.
    #[allow(dead_code)]
    pub fn risk_max_cons_loss_days(&self) -> Option<u32> {
        self.risk_max_cons_loss_days_rules
            .iter()
            .map(|rule| rule.config.0)
            .min()
    }

    pub fn risk_rule_site_counts(&self) -> [usize; 6] {
        [
            self.max_position_size_rules.len(),
            self.allow_entry_rules.len(),
            self.risk_max_drawdown_rules.len(),
            self.risk_max_intraday_loss_rules.len(),
            self.risk_max_intraday_filled_orders_rules.len(),
            self.risk_max_cons_loss_days_rules.len(),
        ]
    }

    /// Begin or continue a broker risk session. The VM supplies `session_key`
    /// from the bar's exchange-time trading day (or the bar index for
    /// higher-than-daily charts). Intraday halts expire when the key changes.
    pub fn begin_session(&mut self, session_key: i64, bar: usize, open: f64) {
        let opening_equity = self.equity_at_price(open);
        if self.current_session_key == Some(session_key) {
            if opening_equity.is_finite() {
                self.intraday_peak_equity = self.intraday_peak_equity.max(opening_equity);
            }
            if self.last_session_equity.is_none() {
                self.last_session_equity = Some(opening_equity);
            }
            return;
        }

        if self.current_session_key.is_some() {
            let ending_equity = self.last_session_equity.unwrap_or(opening_equity);
            self.finish_session(ending_equity, bar, open);
        }

        self.current_session_key = Some(session_key);
        self.intraday_halt_key = None;
        self.intraday_filled_orders = 0;
        self.session_start_equity = opening_equity;
        self.intraday_peak_equity = opening_equity;
        self.last_session_equity = Some(opening_equity);
    }

    fn finish_session(&mut self, ending_equity: f64, bar: usize, open: f64) {
        if ending_equity < self.session_start_equity - 1e-9 {
            self.consecutive_loss_days = self.consecutive_loss_days.saturating_add(1);
        } else {
            self.consecutive_loss_days = 0;
        }
        let strictest = self
            .risk_max_cons_loss_days_rules
            .iter()
            .min_by_key(|rule| rule.config.0)
            .map(|rule| (rule.config.0, rule.config.1.clone()));
        if let Some((limit, alert_message)) = strictest {
            if self.consecutive_loss_days >= limit && !self.risk_halted {
                self.risk_halted = true;
                self.pending.clear();
                self.persistent_exits.clear();
                self.deferred_named_exits.clear();
                self.force_close_all(bar, open, "risk.max_cons_loss_days", alert_message);
            }
        }
    }

    fn trading_blocked(&self) -> bool {
        self.risk_halted
            || self
                .current_session_key
                .is_some_and(|key| self.intraday_halt_key == Some(key))
    }

    /// A read-only view of one closed (`open == false`) or open (`open == true`)
    /// trade at `index`, or `None` if out of range. Closed indices are global
    /// and remain stable across retained-ledger eviction; open indices are
    /// zero-based within the current lot list. Backs the `strategy.*trades.*`
    /// accessors.
    pub fn trade_view(&self, index: usize, open: bool) -> Option<TradeView<'_>> {
        if open {
            let lot = self.lots.get(index)?;
            let dir = lot.qty.signum();
            Some(TradeView {
                entry_price: lot.entry_price,
                entry_notional: lot.entry_account_notional_per_unit * lot.qty.abs(),
                exit_price: f64::NAN,
                size: lot.qty,
                profit: lot.current_profit,
                // The opening commission share for the whole lot.
                commission: lot.entry_commission_per_unit * lot.qty.abs(),
                entry_bar: lot.entry_bar as u32,
                exit_bar: 0,
                max_runup: lot.max_runup,
                max_drawdown: lot.max_drawdown,
                entry_id: non_empty(&lot.entry_id),
                exit_id: None,
                entry_comment: lot.entry_comment.as_deref(),
                exit_comment: None,
                _dir: dir,
            })
        } else {
            // Retention is keyed to filled-order ordinals, so old-entry rows
            // can disappear between otherwise retained global trade indices.
            // Never translate through a contiguous offset; find the exact
            // stable global index requested by Pine code.
            let global_index = u32::try_from(index).ok()?;
            let t = self
                .trades
                .iter()
                .find(|trade| trade.index == Some(global_index))?;
            Some(TradeView {
                entry_price: t.entry_price,
                entry_notional: t.entry_notional.unwrap_or(f64::NAN),
                exit_price: t.exit_price,
                size: t.sign * t.qty,
                profit: t.pnl,
                commission: t.commission,
                entry_bar: t.entry_bar,
                exit_bar: t.exit_bar,
                max_runup: t.max_runup,
                max_drawdown: t.max_drawdown,
                entry_id: non_empty(&t.entry_id),
                exit_id: t.exit_id.as_deref(),
                entry_comment: t.entry_comment.as_deref(),
                exit_comment: t.comment.as_deref(),
                _dir: t.sign,
            })
        }
    }

    fn record_closed_trade(&mut self, mut trade: StrategyTrade) {
        trade.index = Some(self.closed_trades_total);
        // `execute_fill` has already applied this row's P&L to `realized`.
        // Stamping the full-run cumulative value lets report consumers bridge
        // gaps when an intervening old-entry trade is not retained.
        trade.cumulative_pnl = Some(self.realized);
        Self::record_report_trade(&mut self.report_all, &trade);
        if trade.sign >= 0.0 {
            Self::record_report_trade(&mut self.report_long, &trade);
        } else {
            Self::record_report_trade(&mut self.report_short, &trade);
        }
        self.report_pnl_samples.push((
            // Set unconditionally at the top of this function; the fallback is
            // unreachable and exists only so no panic path can enter the
            // artifact build (a reachable panic drags `core::fmt` vtables and
            // an element section into every module).
            trade.index.unwrap_or(u32::MAX),
            trade.sign,
            trade.pnl,
        ));
        self.closed_trades_total = self.closed_trades_total.saturating_add(1);
        if trade.pnl > 0.0 {
            self.gross_profit += trade.pnl;
            self.win_trades = self.win_trades.saturating_add(1);
        } else if trade.pnl < 0.0 {
            self.gross_loss += -trade.pnl;
            self.loss_trades = self.loss_trades.saturating_add(1);
        } else {
            self.even_trades = self.even_trades.saturating_add(1);
        }
        let basis = trade.entry_notional.unwrap_or(0.0);
        if basis.is_finite() && basis > f64::EPSILON {
            let percent = trade.pnl / basis * 100.0;
            self.trade_percent_sum += percent;
            if trade.pnl > 0.0 {
                self.winning_trade_percent_sum += percent;
            } else if trade.pnl < 0.0 {
                self.losing_trade_percent_sum += percent;
            }
        }
        let equity = self.initial_capital + self.realized;
        self.realized_peak = self.realized_peak.max(equity);
        self.realized_trough = self.realized_trough.min(equity);
        self.trade_retention.record(&trade);
        self.trades.push_back(trade);
    }

    /// Trade accessors return gross price excursions. The report subtracts
    /// the already-paid opening fee from favorable P&L and adds it to adverse P&L.
    /// Broker-produced rows always have an exact split; rows without one keep
    /// their recorded values rather than guessing a fee allocation.
    fn report_trade_excursions(trade: &StrategyTrade) -> (f64, f64) {
        match trade.commission_split {
            Some(split) => (
                if trade.max_runup.is_finite() && split.entry.is_finite() {
                    (trade.max_runup - split.entry).max(0.0)
                } else {
                    trade.max_runup - split.entry
                },
                trade.max_drawdown + split.entry,
            ),
            None => (trade.max_runup, trade.max_drawdown),
        }
    }

    fn record_report_trade(scope: &mut ReportScopeAccumulator, trade: &StrategyTrade) {
        let (report_runup, report_drawdown) = Self::report_trade_excursions(trade);
        let aggregate = &mut scope.aggregate;
        aggregate.closed_trades = aggregate.closed_trades.saturating_add(1);
        aggregate.net_profit += trade.pnl;
        aggregate.commission += trade.commission;
        let basis = trade
            .entry_notional
            .filter(|basis| basis.is_finite() && *basis > 0.0);
        let return_percent = basis.map(|basis| trade.pnl / basis * 100.0);
        if let Some(value) = return_percent.filter(|value| value.is_finite()) {
            aggregate.return_percent_sum += value;
            aggregate.return_percent_count = aggregate.return_percent_count.saturating_add(1);
        }

        // Aggregate average bars-held counts both the entry and exit bars.
        // Per-trade rows keep the non-inclusive `exit_bar - entry_bar` duration.
        let bars = u64::from(
            trade
                .exit_bar
                .saturating_sub(trade.entry_bar)
                .saturating_add(1),
        );
        aggregate.bars_held_sum = aggregate.bars_held_sum.saturating_add(bars);
        aggregate.bars_held_count = aggregate.bars_held_count.saturating_add(1);

        let highlight = || crate::StrategyReportTradeHighlight {
            // Indexed by `record_closed_trade` before aggregation; the
            // fallback is unreachable and exists only to keep the crate free
            // of panic paths for the artifact build.
            trade_index: trade.index.unwrap_or(u32::MAX),
            entry_id: trade.entry_id.clone(),
            sign: trade.sign,
            pnl: trade.pnl,
            return_percent,
        };
        if trade.pnl > 0.0 {
            aggregate.gross_profit += trade.pnl;
            aggregate.win_trades = aggregate.win_trades.saturating_add(1);
            aggregate.winning_bars_held_sum = aggregate.winning_bars_held_sum.saturating_add(bars);
            aggregate.winning_bars_held_count = aggregate.winning_bars_held_count.saturating_add(1);
            if let Some(value) = return_percent.filter(|value| value.is_finite()) {
                aggregate.winning_return_percent_sum += value;
                aggregate.winning_return_percent_count =
                    aggregate.winning_return_percent_count.saturating_add(1);
            }
            if aggregate
                .largest_winning_trade
                .as_ref()
                .is_none_or(|current| trade.pnl > current.pnl)
            {
                aggregate.largest_winning_trade = Some(highlight());
            }
        } else if trade.pnl < 0.0 {
            aggregate.gross_loss += -trade.pnl;
            aggregate.loss_trades = aggregate.loss_trades.saturating_add(1);
            aggregate.losing_bars_held_sum = aggregate.losing_bars_held_sum.saturating_add(bars);
            aggregate.losing_bars_held_count = aggregate.losing_bars_held_count.saturating_add(1);
            if let Some(value) = return_percent.filter(|value| value.is_finite()) {
                aggregate.losing_return_percent_sum += value;
                aggregate.losing_return_percent_count =
                    aggregate.losing_return_percent_count.saturating_add(1);
            }
            if aggregate
                .largest_losing_trade
                .as_ref()
                .is_none_or(|current| trade.pnl < current.pnl)
            {
                aggregate.largest_losing_trade = Some(highlight());
            }
        } else {
            aggregate.even_trades = aggregate.even_trades.saturating_add(1);
        }

        let record_excursion =
            |value: f64, sum: &mut f64, count: &mut u32, max: &mut Option<f64>| {
                if value.is_finite() && value >= 0.0 {
                    *sum += value;
                    *count = count.saturating_add(1);
                    *max = Some(max.map_or(value, |current| current.max(value)));
                }
            };
        record_excursion(
            report_runup,
            &mut aggregate.favorable_excursion_sum,
            &mut aggregate.favorable_excursion_count,
            &mut aggregate.favorable_excursion_max,
        );
        record_excursion(
            report_drawdown,
            &mut aggregate.adverse_excursion_sum,
            &mut aggregate.adverse_excursion_count,
            &mut aggregate.adverse_excursion_max,
        );
        if let Some(basis) = basis {
            record_excursion(
                report_runup / basis * 100.0,
                &mut aggregate.favorable_excursion_percent_sum,
                &mut aggregate.favorable_excursion_percent_count,
                &mut aggregate.favorable_excursion_percent_max,
            );
            record_excursion(
                report_drawdown / basis * 100.0,
                &mut aggregate.adverse_excursion_percent_sum,
                &mut aggregate.adverse_excursion_percent_count,
                &mut aggregate.adverse_excursion_percent_max,
            );
        }
    }

    /// Evict closed-trade detail whose opening or closing fill has left the
    /// latest-9,000-successful-order window. Full-run counters and aggregates
    /// were updated before detail entered this deque and are never pruned.
    ///
    /// This runs after every successful fill. Rescanning every retained row
    /// each time would make a long run quadratic in its fill count, so
    /// [`TradeRetention`] rescans only when an out-of-order row may have aged
    /// out.
    fn prune_closed_trade_detail(&mut self) {
        if self.successful_order_fills_total <= MAX_RETAINED_FILLED_ORDERS {
            return;
        }
        let first_retained_order = self
            .successful_order_fills_total
            .saturating_sub(MAX_RETAINED_FILLED_ORDERS);
        self.trade_retention
            .prune(&mut self.trades, first_retained_order);
    }

    fn first_retained_trade_index(&self) -> u32 {
        self.trades
            .front()
            .map_or(self.closed_trades_total, |trade| {
                // Broker-produced rows are always stamped. Falling back to the
                // total keeps this helper panic-free if an unstamped row is ever
                // injected by a future migration path.
                trade.index.unwrap_or(self.closed_trades_total)
            })
    }

    fn retained_trade_pnl_baseline(&self) -> f64 {
        self.trades.front().map_or(self.realized, |trade| {
            trade.cumulative_pnl.unwrap_or(self.realized) - trade.pnl
        })
    }

    fn metrics(&self, open_profit: f64) -> crate::strategy_metrics::StrategyMetrics {
        crate::strategy_metrics::StrategyMetrics {
            net_profit: self.realized,
            gross_profit: self.gross_profit,
            gross_loss: self.gross_loss,
            open_profit,
            closed_trades: self.closed_trades_total,
            win_trades: self.win_trades,
            loss_trades: self.loss_trades,
            even_trades: self.even_trades,
            avg_trade: if self.closed_trades_total == 0 {
                0.0
            } else {
                self.realized / self.closed_trades_total as f64
            },
            max_drawdown: self.max_drawdown,
            max_runup: self.max_runup,
        }
    }

    /// Resolve an order's quantity in contracts from an explicit `qty` or the
    /// strategy's `default_qty_*` settings, using `price` to convert cash/percent
    /// sizings. An explicit nonpositive/nonfinite quantity is a zero-sized order,
    /// not a request to fall back to the default. Returns a non-negative contract
    /// count (the caller applies the direction sign).
    fn resolve_qty(&self, explicit: Option<f64>, price: f64) -> f64 {
        if let Some(q) = explicit {
            return self.quantize_qty(q);
        }
        let price = if price.is_finite() && price != 0.0 {
            price
        } else {
            1.0
        };
        let qty = match self.default_qty_type.as_str() {
            "cash" => (self.default_qty_value
                / (price * self.pointvalue * self.current_currency_rate))
                .max(0.0),
            "percent_of_equity" => {
                let equity = Self::round_account_value(
                    self.initial_capital + self.realized + self.open_profit(price),
                );
                (equity * self.default_qty_value
                    / 100.0
                    / (price * self.pointvalue * self.current_currency_rate
                        + self.commission_for(1.0, price)))
                .max(0.0)
            }
            // "fixed" and anything else: a contract count.
            _ => self.default_qty_value.max(0.0),
        };
        self.quantize_qty(qty)
    }

    /// Public quantity estimator backing `strategy.default_entry_qty()`.
    pub fn default_entry_qty(&self, fill_price: f64) -> f64 {
        self.resolve_qty(None, fill_price)
    }

    fn quantize_qty(&self, qty: f64) -> f64 {
        floor_to_increment(qty, self.mincontract)
    }

    /// Commission charged for filling `qty` contracts at `price`.
    fn commission_for(&self, qty: f64, price: f64) -> f64 {
        let qty = qty.abs();
        match self.commission_type.as_str() {
            "percent" => {
                qty * price.abs()
                    * self.pointvalue
                    * self.current_currency_rate
                    * self.commission_value
                    / 100.0
            }
            // Fixed cash commissions are already denominated in the strategy's
            // account currency; applying symbol FX again would double-convert.
            "cash_per_contract" => qty * self.commission_value,
            "cash_per_order" => self.commission_value,
            _ => 0.0,
        }
    }

    /// Apply adverse slippage: a buy fill (`sign > 0`) pays more, a sell fill
    /// pays less, by `slippage_ticks * mintick`.
    fn apply_slippage(&self, price: f64, sign: f64) -> f64 {
        let adj = self.slippage_ticks * self.mintick;
        if sign > 0.0 {
            price + adj
        } else {
            price - adj
        }
    }

    /// Quantize an executable price to the symbol's minimum tick. The order
    /// fill simulator records fills at symbol precision even when input OHLC
    /// data may contain half-tick values.
    fn quantize_fill_price(&self, price: f64) -> f64 {
        libm::round(price / self.mintick) * self.mintick
    }

    /// Round a declared limit or stop price onto the tick grid for the
    /// order fill simulator: always toward the price that is
    /// harder to reach. A buy limit and a sell stop round down; a sell limit
    /// and a buy stop round up. The rule depends only on the order's side and
    /// kind, never on where the market is, so a stop already crossed still
    /// rounds the same way, and the recorded trade shows the rounded price.
    fn round_order_price(&self, price: f64, buy: bool, kind: OrderPriceKind) -> f64 {
        if !price.is_finite() || !self.mintick.is_finite() || self.mintick <= 0.0 {
            return price;
        }
        let ticks = price / self.mintick;
        let up = match kind {
            OrderPriceKind::Limit => !buy,
            OrderPriceKind::Stop => buy,
        };
        // A price already on the grid stays put: 0.3 / 0.1 is
        // 2.9999999999999996, not a whole tick away. The tolerance grows with the
        // tick count so it still covers the quotient's rounding error past
        // 2^24 ticks, where a fixed 1e-9 is below half a float step.
        let nearest = libm::round(ticks);
        let tolerance = 1e-9_f64.max(libm::fabs(ticks) * 16.0 * f64::EPSILON);
        if libm::fabs(ticks - nearest) <= tolerance {
            nearest * self.mintick
        } else if up {
            libm::ceil(ticks) * self.mintick
        } else {
            libm::floor(ticks) * self.mintick
        }
    }

    fn lot_profit_at(lot: &Lot, price: f64, pointvalue: f64, currency_rate: f64) -> f64 {
        Self::lot_price_move_at(lot, price, pointvalue, currency_rate)
            - lot.entry_commission_per_unit * lot.qty.abs()
    }

    fn lot_price_move_at(lot: &Lot, price: f64, pointvalue: f64, currency_rate: f64) -> f64 {
        (price - lot.entry_price) * lot.qty * pointvalue * currency_rate
    }

    fn mark_lot_prices(
        lot: &mut Lot,
        high: f64,
        low: f64,
        close: f64,
        pointvalue: f64,
        currency_rate: f64,
    ) {
        if close.is_finite() {
            lot.current_profit = Self::lot_profit_at(lot, close, pointvalue, currency_rate);
        }
        let (favorable, adverse) = if lot.qty >= 0.0 {
            (high, low)
        } else {
            (low, high)
        };
        if favorable.is_finite() {
            lot.max_runup = lot
                .max_runup
                .max(Self::lot_price_move_at(lot, favorable, pointvalue, currency_rate).max(0.0));
        }
        if adverse.is_finite() {
            lot.max_drawdown = lot
                .max_drawdown
                .max((-Self::lot_price_move_at(lot, adverse, pointvalue, currency_rate)).max(0.0));
        }
    }

    fn mark_open_lots(&mut self, high: f64, low: f64, close: f64) {
        let pointvalue = self.pointvalue;
        let currency_rate = self.current_currency_rate;
        // The whole position's P&L at this mark's adverse extreme, and the
        // entry fees its open lots have already paid.
        let mut adverse_move = 0.0;
        let mut open_entry_fees = 0.0;
        for lot in &mut self.lots {
            Self::mark_lot_prices(lot, high, low, close, pointvalue, currency_rate);
            let adverse = if lot.qty >= 0.0 { low } else { high };
            adverse_move += Self::lot_price_move_at(lot, adverse, pointvalue, currency_rate);
            open_entry_fees += lot.entry_commission_per_unit * lot.qty.abs();
        }
        self.update_account_drawdown(adverse_move, open_entry_fees);
        self.update_trade_anchored_equity_runups();
    }

    /// The strategy max drawdown: the highest closed-trade equity so far
    /// minus equity with every open trade marked at this mark's adverse
    /// extreme. Open trades count together, so pyramided entries that are
    /// all under water add up. With one trade open it equals the per-trade
    /// formula.
    fn update_account_drawdown(&mut self, adverse_move: f64, open_entry_fees: f64) {
        if !adverse_move.is_finite() || !open_entry_fees.is_finite() {
            return;
        }
        let trough = self.initial_capital + self.realized + adverse_move;
        // Script-visible drawdown is gross of the open trades' paid entry fees;
        // the report includes them.
        let script_drawdown = (self.realized_peak - trough).max(0.0);
        let drawdown = (self.realized_peak - trough + open_entry_fees).max(0.0);
        if script_drawdown.is_finite() {
            self.max_drawdown = self.max_drawdown.max(script_drawdown);
        }
        if drawdown.is_finite() {
            self.report_max_drawdown = self.report_max_drawdown.max(drawdown);
            if self.realized_peak > 0.0 {
                self.report_drawdown_percent = self
                    .report_drawdown_percent
                    .max(drawdown / self.realized_peak * 100.0);
            }
        }
    }

    /// Apply the intrabar run-up rule separately to every physical trade. Unrealized adverse movement never lowers the
    /// run-up anchor. Price excursions are already expressed in account
    /// currency by `mark_lot_prices`.
    fn update_trade_anchored_equity_runups(&mut self) {
        for lot in &self.lots {
            // Gross lot extrema back the script-visible accessors; run report extrema
            // independently include the original paid fee in account currency.
            let script_runup =
                (lot.equity_on_entry - lot.min_equity_before_entry + lot.max_runup).max(0.0);
            if script_runup.is_finite() {
                self.max_runup = self.max_runup.max(script_runup);
            }
            let entry_fee = lot.entry_commission_per_unit * lot.qty.abs();
            let report_favorable = (lot.max_runup - entry_fee).max(0.0);
            let runup =
                (lot.equity_on_entry - lot.min_equity_before_entry + report_favorable).max(0.0);
            if runup.is_finite() {
                self.report_max_runup = self.report_max_runup.max(runup);
            }
        }
    }

    fn update_percent_equity_path(&mut self, open: f64, high: f64, low: f64, close: f64) {
        let high_first = (high - open).abs() <= (open - low).abs();
        let path = if high_first {
            [open, high, low, close]
        } else {
            [open, low, high, close]
        };
        let mut previous = None;
        for price in path {
            // This loop only updates extrema. An identical consecutive mark
            // has no new effects; retain bit equality so signed zeros differ.
            if previous == Some(price.to_bits()) {
                continue;
            }
            previous = Some(price.to_bits());
            let equity = self.equity_at_price(price);
            if !equity.is_finite() {
                continue;
            }
            self.equity_peak = self.equity_peak.max(equity);
            self.equity_trough = self.equity_trough.min(equity);
            if self.equity_peak.abs() > f64::EPSILON {
                self.max_drawdown_percent = self
                    .max_drawdown_percent
                    .max((self.equity_peak - equity) / self.equity_peak.abs() * 100.0);
            }
            if self.equity_trough.abs() > f64::EPSILON {
                self.max_runup_percent = self
                    .max_runup_percent
                    .max((equity - self.equity_trough) / self.equity_trough.abs() * 100.0);
            }
        }
    }

    fn upsert_pending(&mut self, order: PendingOrder) {
        let existing_index = self.pending.iter().position(|candidate| {
            candidate.id == order.id
                && candidate.reduce_only == order.reduce_only
                && candidate.target_lot == order.target_lot
        });
        if let Some(index) = existing_index {
            let replaced_opening = self.pending[index].opening_placement_id;
            self.pending[index] = order;
            self.discard_deferred_opening_placement(replaced_opening);
        } else {
            self.pending.push(order);
        }
    }

    /// Start one main-script calculation. This boundary is deliberately
    /// separate for `calc_on_order_fills` recalculations, even when they occur
    /// at the same market tick: each calculation observes the broker book that
    /// exists when it begins and can modify generations it creates itself.
    pub fn begin_script_execution(&mut self) {
        self.opening_execution_id = self.opening_execution_id.saturating_add(1);
        self.opening_execution_long_occupancy = self.live_entry_occupancy(1.0);
        self.opening_execution_short_occupancy = self.live_entry_occupancy(-1.0);
    }

    fn live_entry_occupancy(&self, sign: f64) -> u32 {
        let lot_count = self
            .lots
            .iter()
            .filter(|lot| lot.qty.signum() == sign.signum())
            .count();
        let pending_count = self
            .pending
            .iter()
            .filter(|order| {
                order.is_entry && !order.reduce_only && order.sign.signum() == sign.signum()
            })
            .count();
        u32::try_from(lot_count.saturating_add(pending_count)).unwrap_or(u32::MAX)
    }

    fn execution_entry_occupancy(&self, sign: f64) -> u32 {
        if sign.signum() >= 0.0 {
            self.opening_execution_long_occupancy
        } else {
            self.opening_execution_short_occupancy
        }
    }

    fn current_execution_opening_index(&self, id: &str) -> Option<usize> {
        self.pending.iter().rposition(|order| {
            !order.reduce_only
                && order.opening_placement_id.is_some()
                && order.id == id
                && order.opening_execution_id == Some(self.opening_execution_id)
        })
    }

    fn latest_opening_index(&self, id: &str) -> Option<usize> {
        self.pending.iter().rposition(|order| {
            !order.reduce_only && order.opening_placement_id.is_some() && order.id == id
        })
    }

    /// Replace one physical opening generation. Direction-compatible
    /// modifications retain its stable placement identity, including any
    /// named exit that already bound to it. An opposite-direction call is a
    /// cancel-and-recreate operation and therefore receives the new order's
    /// identity and releases bindings to the cancelled generation.
    fn replace_opening(&mut self, index: usize, mut order: PendingOrder) {
        let replaced_placement = self.pending[index].opening_placement_id;
        if self.pending[index].sign.signum() == order.sign.signum() {
            order.opening_placement_id = replaced_placement;
        } else {
            self.discard_deferred_opening_placement(replaced_placement);
        }
        self.pending[index] = order;
    }

    fn discard_deferred_opening_placement(&mut self, placement: Option<u64>) {
        let Some(placement) = placement else {
            return;
        };
        for deferred in &mut self.deferred_named_exits {
            deferred
                .eligible_opening_placements
                .retain(|candidate| *candidate != placement);
        }
        self.deferred_named_exits
            .retain(|deferred| !deferred.eligible_opening_placements.is_empty());
    }

    fn prune_deferred_named_exits(&mut self) {
        let live: Vec<u64> = self
            .pending
            .iter()
            .filter_map(|order| order.opening_placement_id)
            .collect();
        for deferred in &mut self.deferred_named_exits {
            deferred
                .eligible_opening_placements
                .retain(|placement| live.contains(placement));
        }
        self.deferred_named_exits
            .retain(|deferred| !deferred.eligible_opening_placements.is_empty());
    }

    /// Quantity in `lot_id` not already claimed by an earlier
    /// `strategy.exit()` child. Exit reservations are deliberately distinct
    /// from market closes and risk liquidation orders, which do not reserve a
    /// portion of a trade while resting.
    fn unreserved_lot_qty(&self, lot_id: u64) -> f64 {
        let Some(lot) = self.lots.iter().find(|lot| lot.id == lot_id) else {
            return 0.0;
        };
        let reserved: f64 = self
            .pending
            .iter()
            .filter(|order| order.target_lot == Some(lot_id))
            .filter_map(|order| order.magnitude)
            .sum();
        (lot.qty.abs() - reserved).max(0.0)
    }

    /// Reconcile live child reservations after another order closes a trade
    /// (including FIFO attribution that may close a lot different from the
    /// exit command's `from_entry`). Earlier calls keep priority; later calls
    /// shrink or disappear when the remaining trade quantity cannot satisfy
    /// their reservations.
    fn reconcile_exit_reservations(&mut self) {
        let mut capacities: Vec<(u64, f64)> = self
            .lots
            .iter()
            .map(|lot| (lot.id, lot.qty.abs()))
            .collect();
        let mincontract = self.mincontract;
        for order in &mut self.pending {
            let Some(lot_id) = order.target_lot else {
                continue;
            };
            let Some((_, available)) = capacities.iter_mut().find(|(id, _)| *id == lot_id) else {
                order.magnitude = Some(0.0);
                continue;
            };
            let requested = order.magnitude.unwrap_or(0.0).min(*available);
            let reserved = if requested >= *available {
                *available
            } else {
                floor_to_increment(requested, mincontract)
            };
            order.magnitude = Some(reserved);
            *available = (*available - reserved).max(0.0);
        }
        self.pending.retain(|order| {
            order.target_lot.is_none() || order.magnitude.is_some_and(|qty| qty > 0.0)
        });
    }

    fn exit_child_for_lot(
        &self,
        template: &PersistentExit,
        lot: &Lot,
        reserved_qty: f64,
    ) -> Option<PendingOrder> {
        if reserved_qty <= 0.0 || lot.qty == 0.0 {
            return None;
        }
        let long = lot.qty > 0.0;
        // Exiting a long sells; exiting a short buys.
        let exit_buys = !long;
        let relative_limit = template
            .profit
            .filter(|value| value.is_finite() && *value >= 0.0)
            .map(|points| {
                if long {
                    lot.entry_price + points * self.mintick
                } else {
                    lot.entry_price - points * self.mintick
                }
            });
        let absolute_limit = template
            .limit
            .filter(|value| value.is_finite())
            .map(|value| self.round_order_price(value, exit_buys, OrderPriceKind::Limit));
        let limit = match (absolute_limit, relative_limit) {
            (Some(absolute), Some(relative)) => Some(if long {
                absolute.min(relative)
            } else {
                absolute.max(relative)
            }),
            (absolute, relative) => absolute.or(relative),
        };
        let relative_stop = template
            .loss
            .filter(|value| value.is_finite() && *value >= 0.0)
            .map(|points| {
                if long {
                    lot.entry_price - points * self.mintick
                } else {
                    lot.entry_price + points * self.mintick
                }
            });
        let absolute_stop = template
            .stop
            .filter(|value| value.is_finite())
            .map(|value| self.round_order_price(value, exit_buys, OrderPriceKind::Stop));
        let stop = match (absolute_stop, relative_stop) {
            (Some(absolute), Some(relative)) => Some(if long {
                absolute.max(relative)
            } else {
                absolute.min(relative)
            }),
            (absolute, relative) => absolute.or(relative),
        };
        let relative_trail = template
            .trail_points
            .filter(|value| value.is_finite() && *value >= 0.0)
            .map(|points| {
                if long {
                    lot.entry_price + points * self.mintick
                } else {
                    lot.entry_price - points * self.mintick
                }
            });
        let absolute_trail = template.trail_price.filter(|value| value.is_finite());
        let trail_activation = match (absolute_trail, relative_trail) {
            (Some(absolute), Some(relative)) => Some(if long {
                absolute.min(relative)
            } else {
                absolute.max(relative)
            }),
            (absolute, relative) => absolute.or(relative),
        };
        let trail_offset_ticks = template
            .trail_offset
            .filter(|value| value.is_finite() && *value >= 0.0);
        let trailing_complete = trail_activation.is_some() && trail_offset_ticks.is_some();
        if limit.is_none() && stop.is_none() && !trailing_complete {
            return None;
        }

        Some(PendingOrder {
            opening_placement_id: None,
            opening_execution_id: None,
            id: template.id.clone(),
            is_entry: false,
            sign: -lot.qty.signum(),
            magnitude: Some(reserved_qty),
            limit,
            stop,
            comment: template.metadata.comment.clone(),
            comment_profit: template.metadata.comment_profit.clone(),
            comment_loss: template.metadata.comment_loss.clone(),
            comment_trailing: template.metadata.comment_trailing.clone(),
            alert_message: template.metadata.alert_message.clone(),
            alert_profit: template.metadata.alert_profit.clone(),
            alert_loss: template.metadata.alert_loss.clone(),
            alert_trailing: template.metadata.alert_trailing.clone(),
            disable_alert: template.metadata.disable_alert,
            oca_name: template
                .metadata
                .oca_name
                .clone()
                .filter(|name| !name.is_empty()),
            // A bracket's take-profit/stop/trailing legs are OCA-reduce. This
            // child represents those mutually exclusive legs; explicit names
            // additionally connect different exit call ids.
            oca_type: OcaType::Reduce,
            reduce_only: true,
            target_entry: Some(lot.entry_id.clone()),
            target_lot: Some(lot.id),
            stop_limit_activated: false,
            trail_activation: trailing_complete.then_some(trail_activation).flatten(),
            trail_offset_ticks,
            trail_best: None,
            exit_order_name: None,
        })
    }

    /// Materialize exit children over the eligible open trades. `qty` and
    /// `qty_percent` size the call across that complete trade set, then the
    /// quantity is reserved FIFO per lot. Calls are processed in source order,
    /// so earlier children reduce what later calls can claim from each trade.
    fn add_exit_children<'a>(
        &mut self,
        template: &PersistentExit,
        lot_ids: impl IntoIterator<Item = &'a u64>,
    ) {
        let lot_ids: Vec<u64> = lot_ids.into_iter().copied().collect();
        let applicable_qty: f64 = lot_ids
            .iter()
            .filter_map(|lot_id| self.lots.iter().find(|lot| lot.id == *lot_id))
            .map(|lot| lot.qty.abs())
            .sum();
        let initial_qty: f64 = lot_ids
            .iter()
            .filter_map(|lot_id| self.lots.iter().find(|lot| lot.id == *lot_id))
            .map(|lot| lot.initial_qty)
            .sum();
        let requested = if let Some(qty) = template.qty {
            if qty.is_finite() && qty > 0.0 {
                qty
            } else {
                0.0
            }
        } else if let Some(percent) = template.qty_percent {
            if percent.is_finite() && percent > 0.0 {
                initial_qty * percent / 100.0
            } else {
                0.0
            }
        } else {
            applicable_qty
        };
        let mut remaining_request = if requested >= applicable_qty {
            applicable_qty
        } else {
            self.quantize_qty(requested)
        };

        for lot_id in lot_ids {
            if remaining_request <= 0.0 {
                break;
            }
            let Some(lot) = self.lots.iter().find(|lot| lot.id == lot_id) else {
                continue;
            };
            let available = self.unreserved_lot_qty(lot_id);
            if available <= 0.0 {
                continue;
            }
            let reserved = if remaining_request >= available {
                available
            } else {
                self.quantize_qty(remaining_request)
            };
            if let Some(order) = self.exit_child_for_lot(template, lot, reserved) {
                self.upsert_pending(order);
                remaining_request = (remaining_request - reserved).max(0.0);
            }
        }
    }

    fn attach_persistent_exits_to_lot(&mut self, lot_id: u64) {
        let templates = self.persistent_exits.clone();
        for template in templates {
            self.add_exit_children(&template, core::iter::once(&lot_id));
        }
    }

    fn attach_deferred_named_exits_to_lot(&mut self, lot_id: u64, placement: Option<u64>) {
        let Some(placement) = placement else {
            return;
        };
        let templates: Vec<PersistentExit> = self
            .deferred_named_exits
            .iter()
            .filter(|deferred| deferred.eligible_opening_placements.contains(&placement))
            .map(|deferred| deferred.template.clone())
            .collect();
        for template in templates {
            self.add_exit_children(&template, core::iter::once(&lot_id));
        }
    }

    fn comment_for_fill(order: &PendingOrder, kind: FillKind) -> Option<String> {
        let specialized = match kind {
            FillKind::Limit => order.comment_profit.as_ref(),
            FillKind::Stop => order.comment_loss.as_ref(),
            FillKind::Trailing => order.comment_trailing.as_ref(),
            FillKind::Market => None,
        };
        specialized.cloned().or_else(|| order.comment.clone())
    }

    fn alert_for_fill(order: &PendingOrder, kind: FillKind) -> Option<String> {
        let specialized = match kind {
            FillKind::Limit => order.alert_profit.as_ref(),
            FillKind::Stop => order.alert_loss.as_ref(),
            FillKind::Trailing => order.alert_trailing.as_ref(),
            FillKind::Market => None,
        };
        specialized
            .cloned()
            .or_else(|| order.alert_message.clone())
            .filter(|message| !message.is_empty())
    }

    fn record_order_fill_alert(&mut self, order: &PendingOrder, kind: FillKind, bar: usize) {
        if order.disable_alert {
            return;
        }
        if let Some(message) = Self::alert_for_fill(order, kind) {
            self.order_fill_alerts.push(OrderFillAlert {
                bar: bar as u32,
                message,
            });
        }
    }

    /// Drain explicit order-fill alert triggers for the host event stream.
    pub fn take_order_fill_alerts(&mut self) -> Vec<OrderFillAlert> {
        core::mem::take(&mut self.order_fill_alerts)
    }

    /// Number of same-tick immediate-close fills produced since the last VM
    /// execution boundary.
    pub fn take_immediate_fill_count(&mut self) -> usize {
        core::mem::take(&mut self.immediate_fill_count)
    }

    fn same_oca_group(filled: &PendingOrder, candidate: &PendingOrder) -> bool {
        filled.oca_type != OcaType::None
            && filled.oca_type == candidate.oca_type
            && filled.oca_name.is_some()
            && filled.oca_name == candidate.oca_name
            // One strategy.exit() call creates a child for every matching
            // trade. Those same-id children must all remain executable; OCA
            // only makes their alternative legs mutually exclusive (already
            // represented by each child) or connects *different* exit calls.
            && !(filled.id == candidate.id
                && filled.target_lot.is_some()
                && filled.target_lot != candidate.target_lot)
    }

    /// Apply a successful fill's OCA policy to orders that were not executable
    /// on the fill's tick. `reduce` subtracts the *executed* quantity and removes
    /// orders reduced to zero, while `cancel` removes every later-tick peer in
    /// the same `(name, type)` group. Peers that can execute on the same tick
    /// are left unaffected, so [`Broker::process_fills`] selects that batch
    /// before calling this helper.
    fn apply_oca_to_orders(
        &self,
        filled: &PendingOrder,
        filled_qty: f64,
        reference_price: f64,
        orders: &mut Vec<PendingOrder>,
    ) {
        if filled.oca_type == OcaType::None || filled.oca_name.is_none() {
            return;
        }
        let candidates = core::mem::take(orders);
        for mut candidate in candidates {
            if !Self::same_oca_group(filled, &candidate) {
                orders.push(candidate);
                continue;
            }
            match filled.oca_type {
                OcaType::None => orders.push(candidate),
                OcaType::Cancel => {}
                OcaType::Reduce => {
                    let current = self.resolve_qty(candidate.magnitude, reference_price);
                    let remaining = self.quantize_qty((current - filled_qty).max(0.0));
                    if remaining > 0.0 {
                        candidate.magnitude = Some(remaining);
                        orders.push(candidate);
                    }
                }
            }
        }
    }

    // -- order placement (called by the VM `Order` opcode) ------------------

    /// Enqueue a `strategy.entry` order. `qty_sign` is the direction sign
    /// (`+1` long / `-1` short); `explicit_qty` is the raw `qty` operand or
    /// `None`; `limit`/`stop` are optional price legs.
    #[allow(clippy::too_many_arguments)]
    pub fn place_entry(
        &mut self,
        id: String,
        qty_sign: f64,
        explicit_qty: Option<f64>,
        limit: Option<f64>,
        stop: Option<f64>,
        metadata: OrderMetadata,
    ) {
        if self.trading_blocked() {
            return;
        }
        let sign = qty_sign.signum();
        if !self.entry_direction_allowed(sign) {
            let size = self.position_size();
            if size != 0.0 && size.signum() != sign {
                self.place_close_order(
                    Some(id),
                    None,
                    None,
                    None,
                    OrderMetadata {
                        comment: metadata.comment,
                        alert_message: metadata.alert_message,
                        disable_alert: metadata.disable_alert,
                        ..OrderMetadata::default()
                    },
                    None,
                );
            }
            return;
        }
        let limit =
            limit.map(|value| self.round_order_price(value, sign > 0.0, OrderPriceKind::Limit));
        let stop =
            stop.map(|value| self.round_order_price(value, sign > 0.0, OrderPriceKind::Stop));
        let price_based = limit.is_some() || stop.is_some();
        // Percent allocations freeze quantity while the script sees its
        // pre-fill account and FX rate. Cash allocations resolve at fill.
        if explicit_qty.is_some_and(|q| !q.is_finite() || q <= 0.0) {
            return;
        }
        let magnitude = explicit_qty.or_else(|| {
            (self.default_qty_type == "percent_of_equity" && self.execution_price.is_finite())
                .then(|| self.resolve_qty(None, self.execution_price))
        });
        let opening_placement_id = self.next_opening_placement_id;
        self.next_opening_placement_id = self.next_opening_placement_id.saturating_add(1);
        let order = PendingOrder {
            opening_placement_id: Some(opening_placement_id),
            opening_execution_id: Some(self.opening_execution_id),
            id,
            is_entry: true,
            sign,
            magnitude,
            limit,
            stop,
            comment: metadata.comment,
            comment_profit: None,
            comment_loss: None,
            comment_trailing: None,
            alert_message: metadata.alert_message,
            alert_profit: None,
            alert_loss: None,
            alert_trailing: None,
            disable_alert: metadata.disable_alert,
            oca_name: metadata
                .oca_name
                .filter(|name| !name.is_empty() && metadata.oca_type != OcaType::None),
            oca_type: metadata.oca_type,
            reduce_only: false,
            target_entry: None,
            target_lot: None,
            stop_limit_activated: false,
            trail_activation: None,
            trail_offset_ticks: None,
            trail_best: None,
            exit_order_name: None,
        };

        // Repeated calls with one id in the same calculation modify that
        // calculation's physical generation (latest call wins).
        if let Some(index) = self.current_execution_opening_index(&order.id) {
            self.replace_opening(index, order);
            return;
        }

        let latest_same_id = self.latest_opening_index(&order.id);
        // `strategy.entry` and `strategy.order` share the opening-order id
        // namespace. With an exactly-one order generation, switching commands
        // modifies it. An opposite-direction same-id entry likewise cancels
        // and recreates the selected generation rather than coexisting.
        if let Some(index) = latest_same_id.filter(|index| {
            !self.pending[*index].is_entry
                || self.pending[*index].sign.signum() != order.sign.signum()
        }) {
            self.replace_opening(index, order);
            return;
        }

        let cap = self.pyramiding.max(1);
        let occupancy = if price_based {
            // Distinct price orders generated during one execution all see its
            // pre-execution snapshot. Prior-execution pending generations do
            // consume capacity, while calls accepted together may later fill
            // past the nominal cap.
            self.execution_entry_occupancy(sign)
        } else {
            // The same-tick exception excludes market entries.
            self.live_entry_occupancy(sign)
        };
        if occupancy < cap {
            self.pending.push(order);
        } else if let Some(index) = latest_same_id {
            // At capacity a same-id call modifies an existing generation. If
            // several physical generations share the id, the latest one is
            // selected.
            self.replace_opening(index, order);
        }
    }

    /// Enqueue a basic `strategy.order` (ignores pyramiding, can open or
    /// reverse without the entry cap).
    #[allow(clippy::too_many_arguments)]
    pub fn place_order(
        &mut self,
        id: String,
        qty_sign: f64,
        explicit_qty: Option<f64>,
        limit: Option<f64>,
        stop: Option<f64>,
        metadata: OrderMetadata,
    ) {
        if self.trading_blocked() {
            return;
        }
        if explicit_qty.is_some_and(|q| !q.is_finite() || q <= 0.0) {
            return;
        }
        let magnitude = explicit_qty;
        let buy = qty_sign > 0.0;
        let limit = limit.map(|value| self.round_order_price(value, buy, OrderPriceKind::Limit));
        let stop = stop.map(|value| self.round_order_price(value, buy, OrderPriceKind::Stop));
        let opening_placement_id = self.next_opening_placement_id;
        self.next_opening_placement_id = self.next_opening_placement_id.saturating_add(1);
        let order = PendingOrder {
            opening_placement_id: Some(opening_placement_id),
            opening_execution_id: Some(self.opening_execution_id),
            id,
            is_entry: false,
            sign: qty_sign.signum(),
            magnitude,
            limit,
            stop,
            comment: metadata.comment,
            comment_profit: None,
            comment_loss: None,
            comment_trailing: None,
            alert_message: metadata.alert_message,
            alert_profit: None,
            alert_loss: None,
            alert_trailing: None,
            disable_alert: metadata.disable_alert,
            oca_name: metadata
                .oca_name
                .filter(|name| !name.is_empty() && metadata.oca_type != OcaType::None),
            oca_type: metadata.oca_type,
            reduce_only: false,
            target_entry: None,
            target_lot: None,
            stop_limit_activated: false,
            trail_activation: None,
            trail_offset_ticks: None,
            trail_best: None,
            exit_order_name: None,
        };
        // `strategy.order` ignores pyramiding but still modifies an opening
        // generation with the same id. If several generations share the id,
        // the latest pending generation is modified consistently.
        let existing = self
            .current_execution_opening_index(&order.id)
            .or_else(|| self.latest_opening_index(&order.id));
        if let Some(index) = existing {
            self.replace_opening(index, order);
        } else {
            self.pending.push(order);
        }
    }

    /// Enqueue a market close of the whole open position (`strategy.close_all`)
    /// or a fraction of it. `qty`/`qty_percent` are optional; when both absent
    /// the whole position is closed.
    pub fn place_close(
        &mut self,
        entry_id: Option<String>,
        qty: Option<f64>,
        qty_percent: Option<f64>,
        metadata: OrderMetadata,
        immediate_tick: Option<(usize, f64)>,
    ) -> usize {
        self.place_close_order(
            entry_id.clone(),
            entry_id,
            qty,
            qty_percent,
            metadata,
            immediate_tick,
        )
    }

    /// Create a close whose broker order identity is independent from the
    /// entry selector it targets. Most explicit close commands use the same
    /// string for both. A direction-disallowed `strategy.entry()` is the
    /// exception: that call becomes a close of the whole opposite position, and
    /// the entry call's id is kept as that closing order's identity.
    fn place_close_order(
        &mut self,
        order_id: Option<String>,
        target_entry: Option<String>,
        qty: Option<f64>,
        qty_percent: Option<f64>,
        metadata: OrderMetadata,
        immediate_tick: Option<(usize, f64)>,
    ) -> usize {
        let size = self.close_target_size(target_entry.as_deref());
        if size == 0.0 {
            return 0;
        }
        // The closing order is opposite the position sign. An explicit zero is
        // a zero-sized close; it must not fall through to the default full size.
        let close_qty = if let Some(q) = qty {
            if !q.is_finite() || q <= 0.0 {
                return 0;
            }
            q.min(size.abs())
        } else if let Some(p) = qty_percent {
            if !p.is_finite() || p <= 0.0 {
                return 0;
            }
            (size.abs() * (p / 100.0)).min(size.abs())
        } else {
            size.abs()
        };
        // A full close consumes the actual remaining lots. Re-quantizing a
        // floating-point remainder can discard a whole contract step after
        // several partial fills.
        let close_qty = if close_qty >= size.abs() {
            size.abs()
        } else {
            self.quantize_qty(close_qty)
        };
        if close_qty <= 0.0 {
            return 0;
        }
        let sign = if size > 0.0 { -1.0 } else { 1.0 };
        // `strategy.close(id)` uses the entry id as its order id, and
        // `strategy.close_all()` has neither. A margin call or a
        // direction-disallowed entry names its own order and gets no name.
        let exit_order_name = match (order_id.as_deref(), target_entry.as_deref()) {
            // Built without `format!`, whose formatter would give the
            // runtime blob a function table.
            (Some(id), Some(entry)) if id == entry => {
                let mut name = String::from("Close entry(s) order ");
                name.push_str(entry);
                Some(name)
            }
            (None, None) => Some(String::from("Close position order")),
            _ => None,
        }
        // A closing order's comment replaces the default label.
        .filter(|_| metadata.comment.is_none());
        let order = PendingOrder {
            opening_placement_id: None,
            opening_execution_id: None,
            id: order_id.unwrap_or_default(),
            is_entry: false,
            sign,
            magnitude: Some(close_qty),
            limit: None,
            stop: None,
            comment: metadata.comment,
            comment_profit: None,
            comment_loss: None,
            comment_trailing: None,
            alert_message: metadata.alert_message,
            alert_profit: None,
            alert_loss: None,
            alert_trailing: None,
            disable_alert: metadata.disable_alert,
            oca_name: None,
            oca_type: OcaType::None,
            reduce_only: true,
            target_entry,
            target_lot: None,
            stop_limit_activated: false,
            trail_activation: None,
            trail_offset_ticks: None,
            trail_best: None,
            exit_order_name,
        };
        if let Some((bar, price)) = immediate_tick.filter(|(_, price)| price.is_finite()) {
            self.mark_open_lots(price, price, price);
            self.update_percent_equity_path(price, price, price, price);
            self.update_maximum_margin_used(price);
            let filled = self.execute_fill(&order, price, FillKind::Market, bar);
            if filled > 0.0 {
                self.record_order_fill_alert(&order, FillKind::Market, bar);
                self.note_script_fill(bar, price);
                self.immediate_fill_count = self.immediate_fill_count.saturating_add(1);
                if self.position_size() == 0.0 {
                    self.pending.retain(|candidate| !candidate.reduce_only);
                }
                return 1;
            }
            0
        } else {
            self.upsert_pending(order);
            0
        }
    }

    /// Enqueue a `strategy.exit` bracket: a reduce-only order carrying a
    /// take-profit (`profit` points or a `limit` price) and/or a stop-loss
    /// (`loss` points or a `stop` price). Absolute `limit`/`stop` take
    /// precedence over the point offsets.
    #[allow(clippy::too_many_arguments)]
    pub fn place_exit(
        &mut self,
        id: String,
        from_entry: Option<String>,
        qty: Option<f64>,
        qty_percent: Option<f64>,
        profit: Option<f64>,
        limit: Option<f64>,
        loss: Option<f64>,
        stop: Option<f64>,
        trail_price: Option<f64>,
        trail_points: Option<f64>,
        trail_offset: Option<f64>,
        metadata: ExitMetadata,
    ) {
        // Declared prices stay as written here; each exit child rounds them
        // once its side is known (see `exit_child_for_lot`).
        let from_entry = from_entry.filter(|entry| !entry.is_empty());
        let template = PersistentExit {
            id: id.clone(),
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
        };

        // A later call with the same exit id modifies the existing exit
        // children. Removing them first also releases their reservations so
        // the updated call can reclaim quantity in source order.
        self.pending
            .retain(|order| !(order.id == id && order.target_lot.is_some()));
        self.persistent_exits
            .retain(|persistent| persistent.id != id);
        self.deferred_named_exits
            .retain(|deferred| deferred.template.id != id);

        let eligible_opening_placements: Vec<u64> = from_entry
            .as_deref()
            .map(|entry| {
                self.pending
                    .iter()
                    .filter(|order| !order.reduce_only && order.id == entry)
                    .filter_map(|order| order.opening_placement_id)
                    .collect()
            })
            .unwrap_or_default();
        let lot_ids: Vec<u64> = self
            .lots
            .iter()
            .filter(|lot| {
                from_entry
                    .as_deref()
                    .is_none_or(|entry| lot.entry_id == entry)
            })
            .map(|lot| lot.id)
            .collect();
        if !lot_ids.is_empty() {
            self.add_exit_children(&template, &lot_ids);
        }
        if from_entry.is_none() {
            // A no-from-entry call remains active for later lots only after it
            // can identify a current position lifecycle (an open lot or a
            // pending opening order). A wholly flat/no-order call remains a
            // no-op and does not seed exits indefinitely into the future.
            let has_pending_open = self
                .pending
                .iter()
                .any(|order| !order.reduce_only && order.opening_placement_id.is_some());
            if !lot_ids.is_empty() || has_pending_open {
                self.persistent_exits.push(template);
            }
        } else if !eligible_opening_placements.is_empty() {
            self.deferred_named_exits.push(DeferredNamedExit {
                template,
                eligible_opening_placements,
            });
        }
    }

    /// Cancel pending orders with the given id (`strategy.cancel`).
    pub fn cancel(&mut self, id: &str) {
        self.pending.retain(|o| o.id != id);
        self.persistent_exits
            .retain(|persistent| persistent.id != id);
        self.deferred_named_exits
            .retain(|deferred| deferred.template.id != id);
        self.prune_deferred_named_exits();
    }

    /// Cancel every pending order (`strategy.cancel_all`).
    pub fn cancel_all(&mut self) {
        self.pending.clear();
        self.persistent_exits.clear();
        self.deferred_named_exits.clear();
    }

    /// The signed size that a close targets: the whole position for
    /// `close_all`, or the aggregate remaining quantity of lots whose entry id
    /// matches a named `strategy.close`.
    fn close_target_size(&self, entry_id: Option<&str>) -> f64 {
        match entry_id.filter(|id| !id.is_empty()) {
            None => self.position_size(),
            Some(id) => self
                .lots
                .iter()
                .filter(|lot| lot.entry_id == id)
                .map(|lot| lot.qty)
                .sum(),
        }
    }

    fn entry_direction_allowed(&self, qty_sign: f64) -> bool {
        self.allow_entry_rules
            .iter()
            .all(|rule| rule.config == 0 || f64::from(rule.config) == qty_sign.signum())
    }

    // -- the fill loop (called by the VM bar loop) --------------------------

    /// Process the pending order book against bar `bar`'s OHLC. Call this at the
    /// start of a bar (for orders placed on the previous bar) — or, under
    /// `process_orders_on_close`, at the end of the same bar the orders were
    /// placed on (`market_at_close = true`, so market fills use the close).
    /// Filled orders update the position and realize trades.
    pub fn process_fills(
        &mut self,
        bar: usize,
        open: f64,
        high: f64,
        low: f64,
        close: f64,
        market_at_close: bool,
    ) -> usize {
        let mut queued_fills = 0;
        if !market_at_close
            && self
                .pending_close_margin
                .is_some_and(|(queued, _)| queued < bar)
        {
            let (_, qty) = self
                .pending_close_margin
                .take()
                .expect("checked pending liquidation");
            let before = self.position_size().abs();
            let immediate_fills_before = self.immediate_fill_count;
            queued_fills = self.place_close_order(
                Some("Margin Call".into()),
                None,
                Some(qty),
                None,
                OrderMetadata {
                    comment: Some("Margin Call".into()),
                    disable_alert: true,
                    ..OrderMetadata::default()
                },
                Some((bar, open)),
            );
            // The caller accounts for this market-tick fill directly. Do not
            // schedule a second, script-immediate recalculation for it.
            self.immediate_fill_count = immediate_fills_before;
            let filled = before - self.position_size().abs();
            if filled > 0.0 {
                let volume = filled * open.abs() * self.pointvalue * self.current_currency_rate;
                self.margin_calls = self.margin_calls.saturating_add(1);
                self.total_liquidated += volume;
                self.largest_liquidation = self.largest_liquidation.max(volume);
            }
        }
        if self.pending.is_empty() {
            return queued_fills;
        }
        let open = self.quantize_fill_price(open);
        let high = self.quantize_fill_price(high);
        let low = self.quantize_fill_price(low);
        let close = self.quantize_fill_price(close);
        // Determine the complete executable batch before applying any fill's
        // side effects. OCA rules cannot cancel or reduce a peer that is
        // executable on the same available tick. `fill_price` also advances
        // stateful stop-limit/trailing orders, so retain each updated order in
        // exactly one of these collections.
        let orders = core::mem::take(&mut self.pending);
        let mut executable = Vec::new();
        let mut still_pending = Vec::new();
        for mut order in orders {
            match self.fill_price(&mut order, open, high, low, close, market_at_close) {
                Some((raw_price, kind)) => executable.push((order, raw_price, kind)),
                None => {
                    // Market orders always fill; a resting limit/stop that did
                    // not trigger stays live for the next bar.
                    still_pending.push(order);
                }
            }
        }

        let mut fills = queued_fills;
        for (order, raw_price, kind) in executable {
            // Existing trades reach the order's actual fill price before a
            // closing/reversal fill removes them. This enforces the closing-bar
            // path boundary without giving a newly opened lot
            // price history from before its own fill.
            self.mark_open_lots(raw_price, raw_price, raw_price);
            self.update_percent_equity_path(raw_price, raw_price, raw_price, raw_price);
            self.update_maximum_margin_used(raw_price);
            let mut executed_order = order.clone();
            executed_order.comment = Self::comment_for_fill(&order, kind);
            let position_sign_before = self.position_sign();
            let filled_qty = self.execute_fill(&executed_order, raw_price, kind, bar);
            self.discard_deferred_opening_placement(order.opening_placement_id);
            if filled_qty > 0.0 {
                fills += 1;
                self.record_order_fill_alert(&order, kind, bar);
                self.note_script_fill(bar, raw_price);
                // Only orders that were ineligible on this tick remain subject
                // to OCA cancellation/reduction. Every order in `executable`
                // keeps the quantity selected at the tick boundary.
                self.apply_oca_to_orders(&order, filled_qty, raw_price, &mut still_pending);
                let position_sign_after = self.position_sign();
                if position_sign_after == 0.0 {
                    // A persistent no-from-entry exit ends when its position
                    // closes. Already-executable peers remain in this tick's
                    // immutable batch.
                    self.persistent_exits.clear();
                    still_pending.retain(|candidate| !candidate.reduce_only);
                    self.pending.retain(|candidate| !candidate.reduce_only);
                } else if position_sign_before != 0.0 && position_sign_after != position_sign_before
                {
                    // Old-position children must not cross a reversal, but a
                    // named deferred child materialized for the new opening
                    // lot remains valid.
                    self.persistent_exits.clear();
                    still_pending.retain(|candidate| !candidate.reduce_only);
                }
            }
            if self.trading_blocked() {
                still_pending.clear();
                break;
            }
        }
        if self.trading_blocked() {
            self.pending.clear();
            self.persistent_exits.clear();
            self.deferred_named_exits.clear();
        } else {
            // `execute_fill` can materialize persistent exit children for a
            // newly opened lot. They are created on this tick and therefore
            // join the resting book for the *next* available tick.
            still_pending.append(&mut self.pending);
            self.pending = still_pending;
            self.reconcile_exit_reservations();
            self.prune_deferred_named_exits();
        }
        fills
    }

    /// The fill mode of `process_orders_on_close`.
    pub fn process_on_close(&self) -> bool {
        self.process_orders_on_close
    }

    pub fn calc_on_every_tick(&self) -> bool {
        self.calc_on_every_tick
    }

    pub fn calc_on_order_fills(&self) -> bool {
        self.calc_on_order_fills
    }

    pub fn calc_on_every_history_tick(&self) -> bool {
        self.calc_on_every_history_tick
    }

    /// Determine the raw (pre-slippage) fill price for `order` against a bar,
    /// or `None` if a resting limit/stop did not trigger this bar.
    fn fill_price(
        &self,
        order: &mut PendingOrder,
        open: f64,
        high: f64,
        low: f64,
        close: f64,
        market_at_close: bool,
    ) -> Option<(f64, FillKind)> {
        let buy = order.sign > 0.0;
        // Reference price for a plain market fill.
        let market = if market_at_close { close } else { open };
        // Orders created during the bar-close execution did not exist while
        // the bar traversed its high/low. Their optional close-tick fill
        // attempt may inspect only the closing tick, never earlier OHLC.
        let (open, high, low) = if market_at_close {
            (close, close, close)
        } else {
            (open, high, low)
        };
        if order.reduce_only && order.trail_activation.is_some() {
            return self.trailing_bracket_fill(order, open, high, low, close);
        }
        match (order.limit, order.stop) {
            (None, None) => Some((market, FillKind::Market)),
            (Some(limit), None) => self
                .limit_fill(buy, limit, open, high, low)
                .map(|price| (price, FillKind::Limit)),
            (None, Some(stop)) => self
                .stop_fill(buy, stop, open, high, low)
                .map(|price| (price, FillKind::Stop)),
            (Some(limit), Some(stop)) => {
                if !order.reduce_only {
                    return self.stop_limit_fill(
                        buy,
                        stop,
                        limit,
                        open,
                        high,
                        low,
                        close,
                        &mut order.stop_limit_activated,
                    );
                }
                // A fixed bracket leg already marketable at the segment open
                // executes before any later synthetic path crossing. This is
                // especially important after a gap through the stop when the
                // later high also reaches the take-profit.
                if self.limit_reached_at_price(buy, limit, open) {
                    return Some((
                        self.limit_execution_price(buy, limit, open),
                        FillKind::Limit,
                    ));
                }
                let stop_at_open = if buy { open >= stop } else { open <= stop };
                if stop_at_open {
                    return Some((open, FillKind::Stop));
                }
                // Both legs on one order/bar: walk the assumed intrabar path
                // from the open and take whichever triggers first.
                let up_first = (high - open).abs() <= (open - low).abs();
                let limit_hit = self.limit_fill(buy, limit, open, high, low);
                let stop_hit = self.stop_fill(buy, stop, open, high, low);
                match (limit_hit, stop_hit) {
                    (Some(l), Some(s)) => {
                        // Both would fill this bar; the path order decides.
                        // open→high→low: an up-move triggers the higher price
                        // first; open→low→high: the lower price first.
                        let price = if up_first {
                            if l.max(s) == l {
                                l
                            } else {
                                s
                            }
                        } else if l.min(s) == l {
                            l
                        } else {
                            s
                        };
                        Some((
                            price,
                            if price == l {
                                FillKind::Limit
                            } else {
                                FillKind::Stop
                            },
                        ))
                    }
                    (Some(l), None) => Some((l, FillKind::Limit)),
                    (None, Some(s)) => Some((s, FillKind::Stop)),
                    (None, None) => None,
                }
            }
        }
    }

    /// Walk the simulator's synthetic OHLC path for an exit bracket that
    /// includes a trailing leg.  The activation/best-price state is updated on
    /// favorable segments and retained on the order when no leg fills.
    fn trailing_bracket_fill(
        &self,
        order: &mut PendingOrder,
        open: f64,
        high: f64,
        low: f64,
        close: f64,
    ) -> Option<(f64, FillKind)> {
        let activation = order.trail_activation?;
        let offset = order.trail_offset_ticks? * self.mintick;
        if !activation.is_finite() || !offset.is_finite() || offset < 0.0 {
            return None;
        }
        // A sell reduce order exits a long position; a buy reduce order exits
        // a short position.
        let long_exit = order.sign < 0.0;
        let high_first = (high - open).abs() <= (open - low).abs();
        let path = if high_first {
            [open, high, low, close]
        } else {
            [open, low, high, close]
        };
        if order.trail_best.is_none()
            && ((long_exit && open >= activation) || (!long_exit && open <= activation))
        {
            order.trail_best = Some(open);
        }

        for segment in path.windows(2) {
            let from = segment[0];
            let to = segment[1];

            // An order activated on a prior segment/bar can gap through its
            // stop at the segment start.  Fixed legs use the same better/worse
            // market-at-open behavior as ordinary limit/stop orders.
            let trailing_stop = order.trail_best.map(|best| {
                if long_exit {
                    best - offset
                } else {
                    best + offset
                }
            });
            if long_exit {
                if let Some(limit) = order
                    .limit
                    .filter(|limit| self.limit_reached_at_price(false, *limit, from))
                {
                    return Some((
                        self.limit_execution_price(false, limit, from),
                        FillKind::Limit,
                    ));
                }
                let fixed = order.stop.filter(|stop| from <= *stop);
                let trailing = trailing_stop.filter(|stop| from <= *stop);
                match (fixed, trailing) {
                    (Some(fixed), Some(trailing)) => {
                        return Some((
                            from.min(fixed.max(trailing)),
                            if trailing >= fixed {
                                FillKind::Trailing
                            } else {
                                FillKind::Stop
                            },
                        ));
                    }
                    (Some(fixed), None) => return Some((from.min(fixed), FillKind::Stop)),
                    (None, Some(trailing)) => {
                        return Some((from.min(trailing), FillKind::Trailing));
                    }
                    (None, None) => {}
                }
            } else {
                if let Some(limit) = order
                    .limit
                    .filter(|limit| self.limit_reached_at_price(true, *limit, from))
                {
                    return Some((
                        self.limit_execution_price(true, limit, from),
                        FillKind::Limit,
                    ));
                }
                let fixed = order.stop.filter(|stop| from >= *stop);
                let trailing = trailing_stop.filter(|stop| from >= *stop);
                match (fixed, trailing) {
                    (Some(fixed), Some(trailing)) => {
                        return Some((
                            from.max(fixed.min(trailing)),
                            if trailing <= fixed {
                                FillKind::Trailing
                            } else {
                                FillKind::Stop
                            },
                        ));
                    }
                    (Some(fixed), None) => return Some((from.max(fixed), FillKind::Stop)),
                    (None, Some(trailing)) => {
                        return Some((from.max(trailing), FillKind::Trailing));
                    }
                    (None, None) => {}
                }
            }

            if to > from {
                // Upward segment: profit for a long, fixed/trailing loss for a
                // short. Pick the first crossed price (the lowest level).
                if long_exit {
                    if let Some(limit) = order.limit.filter(|limit| {
                        let trigger = self.limit_trigger_price(false, *limit);
                        trigger > from && trigger <= to
                    }) {
                        return Some((limit, FillKind::Limit));
                    }
                } else {
                    let mut first: Option<(f64, FillKind)> = order
                        .stop
                        .filter(|level| *level > from && *level <= to)
                        .map(|level| (level, FillKind::Stop));
                    if let Some(level) = trailing_stop.filter(|level| *level > from && *level <= to)
                    {
                        if first.is_none_or(|(candidate, _)| level < candidate) {
                            first = Some((level, FillKind::Trailing));
                        }
                    }
                    if first.is_some() {
                        return first;
                    }
                }
            } else if to < from {
                // Downward segment: fixed/trailing loss for a long, profit for
                // a short. Pick the first crossed price (the highest level).
                if long_exit {
                    let mut first: Option<(f64, FillKind)> = order
                        .stop
                        .filter(|level| *level < from && *level >= to)
                        .map(|level| (level, FillKind::Stop));
                    if let Some(level) = trailing_stop.filter(|level| *level < from && *level >= to)
                    {
                        if first.is_none_or(|(candidate, _)| level > candidate) {
                            first = Some((level, FillKind::Trailing));
                        }
                    }
                    if first.is_some() {
                        return first;
                    }
                } else if let Some(limit) = order.limit.filter(|limit| {
                    let trigger = self.limit_trigger_price(true, *limit);
                    trigger < from && trigger >= to
                }) {
                    return Some((limit, FillKind::Limit));
                }
            }

            // No exit leg fired. Advance trailing activation/ratchet state on
            // favorable movement. Activation reached mid-segment starts the
            // trail at that level, then the segment's remaining extreme can
            // immediately improve it.
            if long_exit && to > from {
                if order.trail_best.is_some() || (activation > from && activation <= to) {
                    order.trail_best = Some(order.trail_best.unwrap_or(activation).max(to));
                } else if from >= activation {
                    order.trail_best = Some(from.max(to));
                }
            } else if !long_exit && to < from {
                if order.trail_best.is_some() || (activation < from && activation >= to) {
                    order.trail_best = Some(order.trail_best.unwrap_or(activation).min(to));
                } else if from <= activation {
                    order.trail_best = Some(from.min(to));
                }
            }
        }
        None
    }

    /// A limit order's fill price, or `None` if the bar never reached it. A buy
    /// limit fills at or below `limit`; a sell limit at or above.
    fn limit_fill(&self, buy: bool, limit: f64, open: f64, high: f64, low: f64) -> Option<f64> {
        if buy {
            if low <= self.limit_trigger_price(true, limit) {
                Some(self.limit_execution_price(true, limit, open))
            } else {
                None
            }
        } else if high >= self.limit_trigger_price(false, limit) {
            Some(self.limit_execution_price(false, limit, open))
        } else {
            None
        }
    }

    /// Price that must trade before a limit is verified. The queue assumption
    /// changes eligibility, never the order's declared limit price.
    fn limit_trigger_price(&self, buy: bool, limit: f64) -> f64 {
        let verification = self.backtest_fill_limits_assumption as f64 * self.mintick;
        if buy {
            limit - verification
        } else {
            limit + verification
        }
    }

    fn limit_reached_at_price(&self, buy: bool, limit: f64, price: f64) -> bool {
        if buy {
            price <= self.limit_trigger_price(true, limit)
        } else {
            price >= self.limit_trigger_price(false, limit)
        }
    }

    fn limit_execution_price(&self, buy: bool, limit: f64, open: f64) -> f64 {
        if self.backtest_fill_limits_assumption > 0 {
            // Verification simulates time spent in the limit queue. Once the
            // queue is filled, the declared limit is recorded, not the deeper
            // verification price.
            limit
        } else if buy {
            open.min(limit)
        } else {
            open.max(limit)
        }
    }

    /// Stateful stop-limit activation for `strategy.entry/order(stop, limit)`.
    /// Historical bars are traversed using the synthetic OHLC path;
    /// only prices at or after stop activation may satisfy the limit.
    #[allow(clippy::too_many_arguments)]
    fn stop_limit_fill(
        &self,
        buy: bool,
        stop: f64,
        limit: f64,
        open: f64,
        high: f64,
        low: f64,
        close: f64,
        activated: &mut bool,
    ) -> Option<(f64, FillKind)> {
        if *activated {
            return self
                .limit_fill(buy, limit, open, high, low)
                .map(|price| (price, FillKind::Limit));
        }
        let high_first = (high - open).abs() <= (open - low).abs();
        let path = if high_first {
            [open, high, low, close]
        } else {
            [open, low, high, close]
        };
        let stop_reached = |price: f64| if buy { price >= stop } else { price <= stop };
        let limit_trigger = self.limit_trigger_price(buy, limit);
        let limit_reached = |price: f64| {
            if buy {
                price <= limit_trigger
            } else {
                price >= limit_trigger
            }
        };
        for segment in path.windows(2) {
            let from = segment[0];
            let to = segment[1];
            let mut active_from = from;
            if !*activated {
                if stop_reached(from) {
                    *activated = true;
                } else if (buy && from < stop && to >= stop) || (!buy && from > stop && to <= stop)
                {
                    *activated = true;
                    active_from = stop;
                }
            }
            if !*activated {
                continue;
            }
            if limit_reached(active_from) {
                return Some((
                    self.limit_execution_price(buy, limit, active_from),
                    FillKind::Limit,
                ));
            }
            if (buy && active_from > limit_trigger && to <= limit_trigger)
                || (!buy && active_from < limit_trigger && to >= limit_trigger)
            {
                return Some((limit, FillKind::Limit));
            }
        }
        None
    }

    /// A stop order's fill price, or `None` if the bar never triggered it. A
    /// buy stop triggers at or above `stop`; a sell stop at or below.
    fn stop_fill(&self, buy: bool, stop: f64, open: f64, high: f64, low: f64) -> Option<f64> {
        if buy {
            if high >= stop {
                Some(open.max(stop))
            } else {
                None
            }
        } else if low <= stop {
            Some(open.min(stop))
        } else {
            None
        }
    }

    /// Apply a triggered fill at `raw_price`: adjust for slippage, update the
    /// position (closing opposite lots and opening/reversing as needed), charge
    /// commission, and realize any closed quantity as a trade.
    fn execute_fill(
        &mut self,
        order: &PendingOrder,
        raw_price: f64,
        kind: FillKind,
        bar: usize,
    ) -> f64 {
        let sign = order.sign.signum();
        let slipped_price = if kind == FillKind::Limit {
            raw_price
        } else {
            self.apply_slippage(raw_price, sign)
        };
        let fill_price = self.quantize_fill_price(slipped_price);
        self.update_maximum_margin_used(fill_price);
        // Resolve the order magnitude (`None` => the strategy default, which is
        // price-dependent for cash/percent sizings).
        let closing_size = order
            .target_lot
            .and_then(|id| {
                self.lots
                    .iter()
                    .find(|lot| lot.id == id)
                    .map(|lot| lot.qty.abs())
            })
            .unwrap_or_else(|| self.close_target_size(order.target_entry.as_deref()).abs());
        let base_requested =
            if order.reduce_only && order.magnitude.is_some_and(|q| q >= closing_size) {
                closing_size
            } else {
                self.resolve_qty(order.magnitude, fill_price)
            };
        if base_requested <= 0.0 {
            return 0.0;
        }

        let position = self.position_size();
        // How much of this fill closes opposite lots vs. opens new exposure.
        let closing_capacity = if position != 0.0 && position.signum() != sign {
            position.abs()
        } else {
            0.0
        };
        // Build the close/open plan before mutating any lots. A reversing entry
        // closes the whole opposite position and opens its requested quantity;
        // `strategy.order` splits its requested quantity across the two legs.
        let close_qty = if closing_capacity > 0.0 {
            if order.is_entry {
                closing_capacity
            } else {
                base_requested.min(closing_capacity)
            }
        } else {
            0.0
        };
        let mut open_qty = if order.reduce_only {
            0.0
        } else if order.is_entry {
            base_requested
        } else {
            self.quantize_qty((base_requested - close_qty).max(0.0))
        };

        // `strategy.risk.max_position_size` applies to `strategy.entry()` only.
        // Resolve it before a reversal closes the old position: if the cap is
        // below one tradable contract, the entry is rejected as a whole instead
        // of turning it into an unintended close-only transaction.
        if order.is_entry {
            if let Some(cap) = self
                .max_position_size_rules
                .iter()
                .map(|rule| rule.config)
                .reduce(f64::min)
            {
                let same_dir_open: f64 = self
                    .lots
                    .iter()
                    .filter(|lot| lot.qty.signum() == sign)
                    .map(|lot| lot.qty.abs())
                    .sum();
                let headroom = (cap - same_dir_open).max(0.0);
                open_qty = self.quantize_qty(open_qty.min(headroom));
            }
            if closing_capacity > 0.0 && open_qty <= 0.0 {
                return 0.0;
            }
        }

        let market_entry = order.is_entry && order.limit.is_none() && order.stop.is_none();
        // Market entries first pass admission against the last script-visible
        // equity, rounded to four decimals as in percent sizing.
        // A gap can make a queued reversal unaffordable before either leg
        // executes, even when marking the old position at the fill would
        // provide enough funds. In that case the entire entry is rejected.
        if market_entry && open_qty > 0.0 && self.execution_price.is_finite() {
            let budget = Self::round_account_value(self.equity_at_price(self.execution_price));
            if !self.opening_margin_with_equity(
                open_qty,
                sign,
                fill_price,
                closing_capacity > 0.0,
                budget,
            ) {
                return 0.0;
            }
        }

        // An opening leg whose required margin exceeds available funds is
        // rejected. Existing exposure keeps its margin
        // reserved while pyramiding, whereas a reversal releases the old
        // position's margin before testing the new side. Market entries use
        // the pre-fill mark for this exact-funds check, independently of the
        // rounded admission budget above. A sub-cent sizing excess can discard
        // only the opening leg; an admitted gap may instead produce an immediate
        // margin call. Other orders retain their fill-price affordability check.
        if open_qty > 0.0
            && !self.opening_margin_is_affordable(
                open_qty,
                sign,
                if market_entry && self.execution_price.is_finite() {
                    self.execution_price
                } else {
                    fill_price
                },
                closing_capacity > 0.0,
            )
        {
            open_qty = 0.0;
        }

        let planned_fill_qty = close_qty + open_qty;
        if planned_fill_qty <= 0.0 {
            return 0.0;
        }
        // Tentatively use the next ordinal while applying this fill. The
        // counter advances only after at least one unit actually executes, so
        // a stale targeted order or rejected zero-size plan consumes no slot.
        // Every trade row and reversal-opening lot created below shares this
        // one ordinal because they are legs of one PendingOrder fill.
        let fill_order_ordinal = self.successful_order_fills_total;
        // Calculate commission once for the whole executed order, then allocate
        // it pro rata over every closed lot and any reversal-opening lot. This is
        // equivalent for percent/cash-per-contract and essential for
        // cash-per-order, which must be charged exactly once.
        let mut commission_qty_remaining = planned_fill_qty;
        // Round each executed order's commission to ten significant digits.
        // Keep the per-unit sizing estimate unrounded.
        let mut commission_remaining =
            Self::round_account_value(self.commission_for(planned_fill_qty, fill_price));
        let mut filled_qty = 0.0;

        // 1) Close opposite lots FIFO, realizing PnL.
        if close_qty > 0.0 {
            let mut to_close = close_qty;
            while to_close > 1e-12 && !self.lots.is_empty() {
                let lot_index = if self.close_entries_rule == "ANY" {
                    if let Some(target_lot) = order.target_lot {
                        let Some(index) = self.lots.iter().position(|lot| lot.id == target_lot)
                        else {
                            break;
                        };
                        index
                    } else if let Some(target) =
                        order.target_entry.as_deref().filter(|id| !id.is_empty())
                    {
                        let Some(index) = self.lots.iter().position(|lot| lot.entry_id == target)
                        else {
                            break;
                        };
                        index
                    } else {
                        0
                    }
                } else {
                    0
                };
                Self::mark_lot_prices(
                    &mut self.lots[lot_index],
                    fill_price,
                    fill_price,
                    fill_price,
                    self.pointvalue,
                    self.current_currency_rate,
                );
                // Snapshot the front lot, drop the borrow, then compute.
                let (
                    lot_qty,
                    dir,
                    entry_id,
                    entry_bar,
                    entry_price,
                    entry_order_ordinal,
                    entry_fx_rate,
                    entry_comm_per_unit,
                    entry_comment,
                    lot_max_runup,
                    lot_max_drawdown,
                ) = {
                    let lot = &self.lots[lot_index];
                    (
                        lot.qty.abs(),
                        lot.qty.signum(),
                        lot.entry_id.clone(),
                        lot.entry_bar,
                        lot.entry_price,
                        lot.entry_order_ordinal,
                        lot.entry_fx_rate,
                        lot.entry_commission_per_unit,
                        lot.entry_comment.clone(),
                        lot.max_runup,
                        lot.max_drawdown,
                    )
                };
                let closed = to_close.min(lot_qty);
                filled_qty += closed;
                let closed_ratio = closed / lot_qty;
                // PnL for a long lot: (exit - entry) * qty; short: (entry - exit) * qty.
                let gross = (fill_price - entry_price)
                    * closed
                    * dir
                    * self.pointvalue
                    * self.current_currency_rate;
                let exit_commission = if closed + 1e-12 >= commission_qty_remaining {
                    commission_remaining
                } else {
                    commission_remaining * closed / commission_qty_remaining
                };
                commission_qty_remaining = (commission_qty_remaining - closed).max(0.0);
                commission_remaining = (commission_remaining - exit_commission).max(0.0);
                // The pro-rated opening commission for the quantity being
                // closed. It was carried on the lot (NOT charged to `realized`
                // at open, where it lived in `open_profit` instead); on close it
                // moves into this trade's realized `pnl` alongside the exit
                // commission. Adding the full `pnl` to `realized` keeps
                // `realized == net_profit` (the sum of closed-trade pnl).
                let entry_commission = entry_comm_per_unit * closed;
                let pnl = gross - exit_commission - entry_commission;
                self.realized += pnl;
                self.record_closed_trade(StrategyTrade {
                    index: None,
                    entry_id,
                    entry_bar: entry_bar as u32,
                    entry_price,
                    exit_bar: bar as u32,
                    exit_price: fill_price,
                    qty: closed,
                    comment: order.comment.clone(),
                    pnl,
                    cumulative_pnl: None,
                    entry_notional: Some(
                        entry_price.abs() * self.pointvalue * entry_fx_rate * closed,
                    ),
                    // `dir` is the closed lot's sign (the trade's direction).
                    sign: dir,
                    // Total commission on the trade: the closed quantity's share
                    // of the opening commission plus the exit commission.
                    commission: entry_commission + exit_commission,
                    max_runup: lot_max_runup * closed_ratio,
                    max_drawdown: lot_max_drawdown * closed_ratio,
                    exit_id: (!order.id.is_empty()).then(|| order.id.clone()),
                    exit_order_name: order.exit_order_name.clone(),
                    entry_comment,
                    entry_order_ordinal,
                    exit_order_ordinal: fill_order_ordinal,
                    commission_split: Some(CommissionSplit {
                        entry: entry_commission,
                        exit: exit_commission,
                    }),
                });
                // Reduce or remove the lot.
                let new_qty = lot_qty - closed;
                // Summing then subtracting several lots can leave binary
                // roundoff even when the request covers the whole position.
                // Bound cleanup below a real tradable increment: a large
                // position must never make one remaining contract into dust.
                let qty_roundoff = (lot_qty * f64::EPSILON * 8.0)
                    .max(1e-12)
                    .min(self.mincontract * 0.5);
                if new_qty <= qty_roundoff {
                    self.lots.remove(lot_index);
                } else {
                    let remaining_ratio = new_qty / lot_qty;
                    let lot = &mut self.lots[lot_index];
                    lot.qty = new_qty * dir;
                    lot.current_profit *= remaining_ratio;
                    lot.max_runup *= remaining_ratio;
                    lot.max_drawdown *= remaining_ratio;
                }
                to_close -= closed;
            }
        }

        if position != 0.0 && self.lots.is_empty() {
            // The prior position has fully closed. This is the lifetime
            // boundary for a persistent no-from-entry exit call,
            // including when the same fill subsequently opens a reversal.
            self.persistent_exits.clear();
            self.pending.retain(|candidate| !candidate.reduce_only);
        }

        // 2) Open new exposure with the planned remainder — but a reduce-only
        // order (exit/close) never opens/flips past flat.
        if open_qty > 1e-12 && !order.reduce_only {
            filled_qty += open_qty;
            if self.lots.is_empty() {
                self.position_entry_name = Some(order.id.clone());
            }
            // All closing allocations have already been subtracted; the
            // opening lot receives the exact residual, avoiding rounding drift.
            let commission = commission_remaining;
            // The opening commission is NOT charged to `realized` here; the
            // per-unit share is stored on the lot and enters equity two ways
            // that must agree: (a) `open_profit` subtracts it while the lot is
            // open, so `strategy.openprofit`/equity dip from the entry bar;
            // and (b) it moves into the trade's realized `pnl` on close. Keeping
            // it out of `realized` at open makes `realized == net_profit`.
            let lot_id = self.next_lot_id;
            self.next_lot_id = self.next_lot_id.saturating_add(1);
            // Equity before this physical trade's opening is used. This
            // excludes the new entry commission, which remains
            // carried in the lot's open P&L, but includes any MTM on earlier
            // pyramided legs at the fill price.
            let equity_on_entry = self.equity_at_price(fill_price);
            let min_equity_before_entry = self.realized_trough;
            let entry_account_notional_per_unit =
                fill_price.abs() * self.pointvalue * self.current_currency_rate;
            if self.first_entry_bar.is_none() {
                self.first_entry_bar = Some(bar as u32);
                self.first_entry_price = Some(fill_price);
                self.first_entry_account_notional_per_unit = Some(entry_account_notional_per_unit);
            }
            self.lots.push(Lot {
                id: lot_id,
                entry_id: order.id.clone(),
                entry_bar: bar,
                entry_price: fill_price,
                equity_on_entry,
                min_equity_before_entry,
                entry_fx_rate: self.current_currency_rate,
                entry_account_notional_per_unit,
                entry_order_ordinal: fill_order_ordinal,
                entry_comment: order.comment.clone(),
                qty: open_qty * sign,
                initial_qty: open_qty,
                entry_commission_per_unit: commission / open_qty,
                current_profit: -commission,
                max_runup: 0.0,
                // Price-only excursions remain zero at entry for script-visible accessors.
                // Account/report projection charges the paid fee separately.
                max_drawdown: 0.0,
            });
            // The new trade joins the account drawdown at its fill price.
            let (pointvalue, currency_rate) = (self.pointvalue, self.current_currency_rate);
            let (adverse_move, open_entry_fees) =
                self.lots.iter().fold((0.0, 0.0), |(moved, fees), lot| {
                    (
                        moved + Self::lot_price_move_at(lot, fill_price, pointvalue, currency_rate),
                        fees + lot.entry_commission_per_unit * lot.qty.abs(),
                    )
                });
            self.update_account_drawdown(adverse_move, open_entry_fees);
            self.update_trade_anchored_equity_runups();
            self.attach_persistent_exits_to_lot(lot_id);
            self.attach_deferred_named_exits_to_lot(lot_id, order.opening_placement_id);
            self.max_trade_contracts_held = self.max_trade_contracts_held.max(open_qty);
            if sign > 0.0 {
                self.max_trade_contracts_held_long =
                    self.max_trade_contracts_held_long.max(open_qty);
            } else {
                self.max_trade_contracts_held_short =
                    self.max_trade_contracts_held_short.max(open_qty);
            }
            // The report metric is peak *position* size, not the largest
            // individual order. Pyramided fills therefore accumulate before
            // updating the directional high-water marks.
            let held = self.position_size().abs();
            self.max_contracts_held = self.max_contracts_held.max(held);
            if sign > 0.0 {
                self.max_contracts_held_long = self.max_contracts_held_long.max(held);
            } else {
                self.max_contracts_held_short = self.max_contracts_held_short.max(held);
            }
        }
        if self.lots.is_empty() {
            self.position_entry_name = None;
            self.persistent_exits.clear();
        }
        self.update_maximum_margin_used(fill_price);
        if filled_qty > 0.0 {
            self.successful_order_fills_total = self.successful_order_fills_total.saturating_add(1);
            self.prune_closed_trade_detail();
            // Include commissions and realized state transitions at the exact
            // fill in the separately defined peak/trough percentage surface.
            self.update_percent_equity_path(fill_price, fill_price, fill_price, fill_price);
        }
        filled_qty
    }

    fn force_close_all(
        &mut self,
        bar: usize,
        raw_price: f64,
        reason: &str,
        alert_message: Option<String>,
    ) {
        let size = self.position_size();
        if size == 0.0 || !raw_price.is_finite() {
            return;
        }
        let order = PendingOrder {
            opening_placement_id: None,
            opening_execution_id: None,
            id: reason.to_string(),
            is_entry: false,
            sign: -size.signum(),
            magnitude: Some(size.abs()),
            limit: None,
            stop: None,
            comment: None,
            comment_profit: None,
            comment_loss: None,
            comment_trailing: None,
            alert_message,
            alert_profit: None,
            alert_loss: None,
            alert_trailing: None,
            disable_alert: false,
            oca_name: None,
            oca_type: OcaType::None,
            reduce_only: true,
            target_entry: None,
            target_lot: None,
            stop_limit_activated: false,
            trail_activation: None,
            trail_offset_ticks: None,
            trail_best: None,
            exit_order_name: None,
        };
        self.mark_open_lots(raw_price, raw_price, raw_price);
        self.update_percent_equity_path(raw_price, raw_price, raw_price, raw_price);
        self.update_maximum_margin_used(raw_price);
        let filled = self.execute_fill(&order, raw_price, FillKind::Market, bar);
        if filled > 0.0 {
            self.record_order_fill_alert(&order, FillKind::Market, bar);
        }
    }

    fn note_script_fill(&mut self, bar: usize, raw_price: f64) {
        self.intraday_filled_orders = self.intraday_filled_orders.saturating_add(1);
        let strictest = self
            .risk_max_intraday_filled_orders_rules
            .iter()
            .min_by_key(|rule| rule.config.0)
            .map(|rule| (rule.config.0, rule.config.1.clone()));
        if let Some((limit, alert_message)) = strictest {
            if self.intraday_filled_orders >= limit {
                self.trigger_intraday_halt(
                    bar,
                    raw_price,
                    "risk.max_intraday_filled_orders",
                    alert_message,
                );
            }
        }
    }

    fn trigger_intraday_halt(
        &mut self,
        bar: usize,
        raw_price: f64,
        reason: &str,
        alert_message: Option<String>,
    ) {
        if self.risk_halted {
            return;
        }
        let Some(session_key) = self.current_session_key else {
            return;
        };
        if self.intraday_halt_key == Some(session_key) {
            return;
        }
        self.intraday_halt_key = Some(session_key);
        self.pending.clear();
        self.persistent_exits.clear();
        self.deferred_named_exits.clear();
        self.force_close_all(bar, raw_price, reason, alert_message);
    }

    fn enforce_max_drawdown(&mut self, bar: usize, close: f64) {
        let equity = self.initial_capital + self.realized + self.open_profit(close);
        if !equity.is_finite() {
            return;
        }
        if equity > self.drawdown_peak_equity {
            self.drawdown_peak_equity = equity;
        }
        if self.risk_halted {
            return;
        }
        let strictest = self
            .risk_max_drawdown_rules
            .iter()
            .map(|rule| {
                let threshold = match rule.config.1 {
                    RiskUnit::Cash => rule.config.0,
                    RiskUnit::PercentOfEquity => self.drawdown_peak_equity * rule.config.0 / 100.0,
                };
                (threshold, rule.config.2.clone())
            })
            .reduce(|current, candidate| {
                if candidate.0 < current.0 {
                    candidate
                } else {
                    current
                }
            });
        let Some((threshold, alert_message)) = strictest else {
            return;
        };
        if self.drawdown_peak_equity - equity >= threshold {
            self.risk_halted = true;
            self.pending.clear();
            self.persistent_exits.clear();
            self.deferred_named_exits.clear();
            self.force_close_all(bar, close, "risk.max_drawdown", alert_message);
        }
    }

    fn enforce_intraday_loss(&mut self, bar: usize, close: f64) {
        let equity = self.equity_at_price(close);
        if !equity.is_finite() {
            return;
        }
        if !self.intraday_peak_equity.is_finite() || equity > self.intraday_peak_equity {
            self.intraday_peak_equity = equity;
        }

        // The percent-of-equity form has one additional terminal rule: once
        // equity reaches zero or below, trading is disabled permanently,
        // including after the next risk session begins.
        // Evaluate this before the ordinary session-halt early return so a
        // previously triggered intraday halt can still become permanent.
        if equity <= 0.0 {
            let percent_alert = self
                .risk_max_intraday_loss_rules
                .iter()
                .filter(|rule| rule.config.1 == RiskUnit::PercentOfEquity)
                .min_by(|left, right| left.config.0.total_cmp(&right.config.0))
                .map(|rule| rule.config.2.clone());
            if let Some(alert_message) = percent_alert {
                if !self.risk_halted {
                    self.risk_halted = true;
                    self.pending.clear();
                    self.persistent_exits.clear();
                    self.deferred_named_exits.clear();
                    self.force_close_all(bar, close, "risk.max_intraday_loss", alert_message);
                }
                return;
            }
        }
        if self.trading_blocked() || !self.intraday_peak_equity.is_finite() {
            return;
        }
        let strictest = self
            .risk_max_intraday_loss_rules
            .iter()
            .map(|rule| {
                let threshold = match rule.config.1 {
                    RiskUnit::Cash => rule.config.0,
                    RiskUnit::PercentOfEquity => self.intraday_peak_equity * rule.config.0 / 100.0,
                };
                (threshold, rule.config.2.clone())
            })
            .reduce(|current, candidate| {
                if candidate.0 < current.0 {
                    candidate
                } else {
                    current
                }
            });
        let Some((threshold, alert_message)) = strictest else {
            return;
        };
        if self.intraday_peak_equity - equity >= threshold {
            self.trigger_intraday_halt(bar, close, "risk.max_intraday_loss", alert_message);
        }
    }

    fn equity_at_price(&self, price: f64) -> f64 {
        self.initial_capital + self.realized + self.open_profit(price)
    }

    // -- equity / outcome ---------------------------------------------------

    /// Unrealized profit of the open position marked to `price`, **including**
    /// the open lots' entry commission. `strategy.openprofit` is reduced
    /// by the commission paid to open the position, so the value is negative at
    /// entry (before any price move) by exactly that commission. Folding it in
    /// here makes the mark-to-market equity (`initial + realized + open_profit`)
    /// reflect the commission from the entry bar and keeps the two equity
    /// surfaces — the per-bar curve (via `realized`) and `strategy.equity` (via
    /// `net_profit`) — in agreement, since `realized == net_profit` on every bar.
    pub fn open_profit(&self, price: f64) -> f64 {
        if !price.is_finite() {
            return 0.0;
        }
        self.lots
            .iter()
            .map(|l| {
                (price - l.entry_price) * l.qty * self.pointvalue * self.current_currency_rate
                    - l.entry_commission_per_unit * l.qty.abs()
            })
            .sum()
    }

    fn open_trades_capital_held(&self) -> f64 {
        let margin_enabled = (self.margin_long.is_finite() && self.margin_long > 0.0)
            || (self.margin_short.is_finite() && self.margin_short > 0.0);
        if !margin_enabled {
            return f64::NAN;
        }
        if self.lots.is_empty() {
            return 0.0;
        }
        let mut held = 0.0;
        for lot in &self.lots {
            let margin = if lot.qty >= 0.0 {
                self.margin_long
            } else {
                self.margin_short
            };
            let margin = if margin.is_finite() && margin > 0.0 {
                margin
            } else {
                0.0
            };
            held += lot.entry_account_notional_per_unit * lot.qty.abs() * margin / 100.0;
        }
        held
    }

    /// Required margin at a mark price. Unlike
    /// `strategy.opentrades.capital_held`, the run report measures each
    /// bar at its close rather than retaining the lots' entry prices.
    fn margin_used_at(&self, price: f64) -> f64 {
        if !price.is_finite() {
            return 0.0;
        }
        let used: f64 = self
            .lots
            .iter()
            .map(|lot| {
                let margin = if lot.qty >= 0.0 {
                    self.margin_long
                } else {
                    self.margin_short
                };
                price.abs()
                    * lot.qty.abs()
                    * self.pointvalue
                    * self.current_currency_rate
                    * margin.max(0.0)
                    / 100.0
            })
            .sum();
        // Rust's empty floating-point sum is -0.0. Keep the public monetary
        // series canonically non-negative across serde/wasm boundaries.
        if used == 0.0 {
            0.0
        } else {
            used
        }
    }

    /// Whether `qty` units of a new position can be funded at `price` under
    /// the requested side's margin ratio. A non-positive/non-finite margin
    /// disables the affordability constraint for that direction.
    fn opening_margin_is_affordable(
        &self,
        qty: f64,
        sign: f64,
        price: f64,
        releases_existing_margin: bool,
    ) -> bool {
        self.opening_margin_with_equity(
            qty,
            sign,
            price,
            releases_existing_margin,
            self.equity_at_price(price),
        )
    }

    fn opening_margin_with_equity(
        &self,
        qty: f64,
        sign: f64,
        price: f64,
        releases_existing_margin: bool,
        equity: f64,
    ) -> bool {
        if qty <= 0.0 {
            return true;
        }
        let margin = if sign >= 0.0 {
            self.margin_long
        } else {
            self.margin_short
        };
        if !margin.is_finite() || margin <= 0.0 {
            return true;
        }

        let required =
            qty * price.abs() * self.pointvalue * self.current_currency_rate * margin / 100.0;
        if !required.is_finite() || !equity.is_finite() {
            return false;
        }
        let retained_margin = if releases_existing_margin {
            0.0
        } else {
            self.margin_used_at(price)
        };
        let available = equity - retained_margin;
        if !available.is_finite() {
            return false;
        }

        // Permit exact-boundary fills despite a few ULPs of multiplication
        // drift, but never turn the tolerance into a meaningful cash credit.
        let tolerance = required.abs().max(available.abs()).max(1.0) * 1e-12;
        required <= available + tolerance
    }

    fn update_maximum_margin_used(&mut self, price: f64) {
        let margin = self.margin_used_at(price);
        self.maximum_margin_used = self.maximum_margin_used.max(margin);
        if self.lots.is_empty() || !price.is_finite() {
            return;
        }
        self.ever_held_position = true;
    }

    /// Current margin-call price from the margin equation. A
    /// flat position, disabled margin, or an unattainable divisor reads `na`.
    fn margin_liquidation_price(&self, net_profit: f64) -> f64 {
        let size = self.position_size();
        if size == 0.0 || !self.pointvalue.is_finite() || self.pointvalue <= 0.0 {
            return f64::NAN;
        }
        let direction = size.signum();
        let margin_percent = if direction > 0.0 {
            self.margin_long
        } else {
            self.margin_short
        };
        if !margin_percent.is_finite() || margin_percent <= 0.0 {
            return f64::NAN;
        }
        let divisor = margin_percent / 100.0 - direction;
        if divisor.abs() <= f64::EPSILON {
            return f64::NAN;
        }
        let outstanding_entry_commission: f64 = self
            .lots
            .iter()
            .map(|lot| lot.entry_commission_per_unit * lot.qty.abs())
            .sum();
        let account_value_per_point = self.pointvalue * size.abs() * self.current_currency_rate;
        if !account_value_per_point.is_finite() || account_value_per_point <= 0.0 {
            return f64::NAN;
        }
        let raw = ((self.initial_capital + net_profit - outstanding_entry_commission)
            / account_value_per_point
            - direction * self.avg_price())
            / divisor;
        if !raw.is_finite() || raw < 0.0 || !self.mintick.is_finite() || self.mintick <= 0.0 {
            return f64::NAN;
        }
        if direction > 0.0 {
            libm::floor(raw / self.mintick) * self.mintick
        } else {
            libm::ceil(raw / self.mintick) * self.mintick
        }
    }

    // Monetary sizing budgets, margin requirements and paid commissions
    // round to ten significant digits. Round the computed decimal scale
    // directly, so the margin comparison uses the rounded value.
    fn round_account_value(value: f64) -> f64 {
        if !value.is_finite() || value <= 0.0 {
            return value;
        }
        let factor = libm::pow(10.0, 9.0 - libm::floor(libm::log10(value)));
        if !factor.is_finite() || factor == 0.0 {
            return value;
        }
        libm::round(value * factor) / factor
    }

    fn enforce_margin_call(&mut self, bar: usize, high: f64, low: f64) {
        let size = self.position_size();
        if size == 0.0 {
            return;
        }
        let (price, margin_percent) = if size > 0.0 {
            (low, self.margin_long)
        } else {
            (high, self.margin_short)
        };
        if !price.is_finite()
            || price <= 0.0
            || !margin_percent.is_finite()
            || margin_percent <= 0.0
        {
            return;
        }
        let ratio = margin_percent / 100.0;
        let equity = self.equity_at_price(price);
        let required = Self::round_account_value(
            size.abs() * price * self.pointvalue * self.current_currency_rate * ratio,
        );
        let available = equity - required;
        if !available.is_finite() || available >= 0.0 {
            return;
        }
        // A margin call liquidates four times the minimum cover quantity,
        // which reduces repeated margin calls.
        // The minimum cover is truncated to the symbol's contract
        // precision before applying the 4x liquidation cushion. Using f64's
        // integer `trunc()` here silently suppressed calls for fractional
        // symbols (for example, a 0.20-contract cover with mincontract 0.01).
        let minimum_cover = self.quantize_qty(
            -available / ratio / (price * self.pointvalue * self.current_currency_rate),
        );
        let qty = (if minimum_cover > 0.0 {
            minimum_cover * 4.0
        } else {
            1.0
        })
        .min(size.abs());
        if qty <= 0.0 || !qty.is_finite() {
            return;
        }
        let order = PendingOrder {
            opening_placement_id: None,
            opening_execution_id: None,
            id: "Margin Call".to_string(),
            is_entry: false,
            sign: -size.signum(),
            magnitude: Some(qty),
            limit: None,
            stop: None,
            comment: Some("Margin Call".to_string()),
            comment_profit: None,
            comment_loss: None,
            comment_trailing: None,
            alert_message: None,
            alert_profit: None,
            alert_loss: None,
            alert_trailing: None,
            disable_alert: true,
            oca_name: None,
            oca_type: OcaType::None,
            reduce_only: true,
            target_entry: None,
            target_lot: None,
            stop_limit_activated: false,
            trail_activation: None,
            trail_offset_ticks: None,
            trail_best: None,
            exit_order_name: None,
        };
        let liquidation_price = self.apply_slippage(price, order.sign.signum());
        self.mark_open_lots(price, price, price);
        self.update_percent_equity_path(price, price, price, price);
        self.update_maximum_margin_used(price);
        let filled = self.execute_fill(&order, price, FillKind::Market, bar);
        if filled > 0.0 {
            let liquidation_volume =
                filled * liquidation_price.abs() * self.pointvalue * self.current_currency_rate;
            self.margin_calls = self.margin_calls.saturating_add(1);
            self.total_liquidated += liquidation_volume;
            self.largest_liquidation = self.largest_liquidation.max(liquidation_volume);
        }
    }

    /// Record the `strategy.*` read-surface snapshot for the current execution
    /// point. The historical/realtime drivers pass only prices available
    /// since the preceding execution, so a newly opened lot never inherits an
    /// earlier high/low. `close` marks `strategy.openprofit`/`equity`.
    pub fn record_snapshot(&mut self, bar: usize, open: f64, high: f64, low: f64, close: f64) {
        self.observe_market(bar, open, high, low, close);
        self.capture_script_snapshot(bar);
    }

    /// Advance account risk and excursions at every available market vertex.
    /// Drivers capture script history separately immediately before execution.
    /// Every fill, account mark, excursion and risk operation is preserved.
    pub fn observe_market(&mut self, bar: usize, open: f64, high: f64, low: f64, close: f64) {
        let same_high = high.to_bits() == open.to_bits();
        let same_low = low.to_bits() == open.to_bits();
        let same_close = close.to_bits() == open.to_bits();
        let open = self.quantize_fill_price(open);
        let high = if same_high {
            open
        } else {
            self.quantize_fill_price(high)
        };
        let low = if same_low {
            open
        } else {
            self.quantize_fill_price(low)
        };
        let close = if same_close {
            open
        } else {
            self.quantize_fill_price(close)
        };
        self.execution_price = close;
        self.enforce_max_drawdown(bar, close);
        self.enforce_intraday_loss(bar, close);
        self.mark_open_lots(high, low, close);
        self.update_percent_equity_path(open, high, low, close);
        let mut previous = None;
        for price in [open, high, low, close] {
            if previous == Some(price.to_bits()) {
                continue;
            }
            previous = Some(price.to_bits());
            self.update_maximum_margin_used(price);
        }
        self.enforce_margin_call(bar, high, low);
    }

    /// Freeze script-visible account state after the latest market update.
    /// Capture once per execution, including fill-triggered recalculations.
    pub fn capture_script_snapshot(&mut self, bar: usize) {
        let close = self.execution_price;
        let size = self.position_size();
        let open_profit = self.open_profit(close);
        // Entry commission is charged immediately to net profit; openprofit
        // is the remaining lots' gross mark-to-market gain. The internal
        // metrics core keeps closed-trade P&L, so transfer the outstanding
        // fees between these two read surfaces without changing equity.
        let entry_fees: f64 = self
            .lots
            .iter()
            .map(|lot| lot.entry_commission_per_unit * lot.qty.abs())
            .sum();
        // Reuse the metrics core over the trades closed so far so the running
        // net/gross/drawdown/run-up carry the exact same arithmetic as the final
        // summary. `openprofit` enters equity but not the realized drawdown curve.
        let m = self.metrics(open_profit);
        let na_if = |cond: bool, v: f64| if cond { f64::NAN } else { v };
        let pct = |value: f64, basis: f64| {
            if basis.is_finite() && basis.abs() > f64::EPSILON {
                value / basis.abs() * 100.0
            } else {
                f64::NAN
            }
        };
        let realized_equity = self.initial_capital + m.net_profit;
        let snapshot = StrategySnapshot {
            position_size: size,
            position_avg_price: na_if(size == 0.0, self.avg_price()),
            open_profit: open_profit + entry_fees,
            net_profit: m.net_profit - entry_fees,
            gross_profit: m.gross_profit,
            gross_loss: m.gross_loss,
            equity: self.initial_capital + m.net_profit + open_profit,
            initial_capital: self.initial_capital,
            win_trades: m.win_trades as i64,
            loss_trades: m.loss_trades as i64,
            even_trades: m.even_trades as i64,
            closed_trades: m.closed_trades as i64,
            open_trades: self.lots.len() as i64,
            max_drawdown: m.max_drawdown,
            max_runup: m.max_runup,
            avg_trade: na_if(m.closed_trades == 0, m.avg_trade),
            avg_winning_trade: na_if(
                m.win_trades == 0,
                m.gross_profit / m.win_trades.max(1) as f64,
            ),
            avg_losing_trade: na_if(
                m.loss_trades == 0,
                m.gross_loss / m.loss_trades.max(1) as f64,
            ),
            max_contracts_held: self.max_trade_contracts_held,
            max_contracts_held_long: self.max_trade_contracts_held_long,
            max_contracts_held_short: self.max_trade_contracts_held_short,
            account_currency: self.account_currency.clone(),
            avg_losing_trade_percent: na_if(
                m.loss_trades == 0,
                self.losing_trade_percent_sum / m.loss_trades.max(1) as f64,
            ),
            avg_trade_percent: na_if(
                m.closed_trades == 0,
                self.trade_percent_sum / m.closed_trades.max(1) as f64,
            ),
            avg_winning_trade_percent: na_if(
                m.win_trades == 0,
                self.winning_trade_percent_sum / m.win_trades.max(1) as f64,
            ),
            closed_trades_first_index: self.first_retained_trade_index() as i64,
            gross_loss_percent: pct(m.gross_loss, self.initial_capital),
            gross_profit_percent: pct(m.gross_profit, self.initial_capital),
            margin_liquidation_price: self.margin_liquidation_price(m.net_profit),
            max_drawdown_percent: self.max_drawdown_percent,
            max_runup_percent: self.max_runup_percent,
            net_profit_percent: pct(m.net_profit - entry_fees, self.initial_capital),
            open_profit_percent: pct(open_profit + entry_fees, realized_equity - entry_fees),
            open_trades_capital_held: self.open_trades_capital_held(),
            position_entry_name: self.position_entry_name.clone(),
        };
        if self.snapshots.len() <= bar {
            self.snapshots.resize_with(bar + 1, || None);
        }
        self.snapshots[bar] = Some(snapshot);
    }

    /// Bytes per bar of the lanes [`Self::reserve_bar_lanes`] reserves.
    pub const BAR_LANE_BYTES: usize = core::mem::size_of::<Option<StrategySnapshot>>()
        + 2 * core::mem::size_of::<f64>()
        + core::mem::size_of::<Option<f64>>();

    /// Reserve the lanes that hold one entry per bar (strategy snapshots,
    /// equity, margin and currency rates) for a run of `bars` bars, so they
    /// never reallocate during it. A host with a bounded memory can then
    /// check the run's per-bar storage, `bars * BAR_LANE_BYTES`, before it
    /// starts, and a lane never asks for twice its size part-way through.
    pub fn reserve_bar_lanes(&mut self, bars: usize) {
        self.snapshots
            .reserve_exact(bars.saturating_sub(self.snapshots.len()));
        self.equity
            .reserve_exact(bars.saturating_sub(self.equity.len()));
        self.margin_used
            .reserve_exact(bars.saturating_sub(self.margin_used.len()));
        self.currency_rates
            .reserve_exact(bars.saturating_sub(self.currency_rates.len()));
    }

    /// Bytes held by the ledger vectors that grow with the number of trades
    /// rather than bars: the retained closed-trade rows and the full-run
    /// report P&L samples. Growing them can ask for up to twice this.
    pub fn trade_ledger_bytes(&self) -> usize {
        self.trades.capacity() * core::mem::size_of::<StrategyTrade>()
            + self.report_pnl_samples.capacity() * core::mem::size_of::<(u32, f64, f64)>()
    }

    /// The strategy snapshot for `index`, or `None` if that bar has not been
    /// recorded yet (an out-of-range history read maps to `na`).
    pub fn snapshot_at(&self, index: usize) -> Option<&StrategySnapshot> {
        self.snapshots.get(index).and_then(Option::as_ref)
    }

    /// Record the equity sample for a completed bar (`initial_capital +
    /// realized + open PnL` marked to the bar's close).
    pub fn record_equity(&mut self, bar: usize, close: f64) {
        let close = self.quantize_fill_price(close);
        self.enforce_max_drawdown(bar, close);
        self.enforce_intraday_loss(bar, close);
        self.mark_open_lots(close, close, close);
        let open = self.open_profit(close);
        let equity = self.initial_capital + self.realized + open;
        let margin_used = self.margin_used_at(close);
        let deficit = Self::round_account_value(margin_used) - equity;
        let size = self.position_size();
        let ratio = if size > 0.0 {
            self.margin_long
        } else {
            self.margin_short
        } / 100.0;
        if deficit > 1e-9 && ratio > 0.0 && size != 0.0 {
            let cover = self.quantize_qty(
                deficit / (close * self.pointvalue * self.current_currency_rate * ratio),
            );
            let qty = (if cover > 0.0 { cover * 4.0 } else { 1.0 }).min(size.abs());
            if qty > 0.0 {
                self.pending_close_margin = Some((bar, qty));
            }
        }
        self.update_maximum_margin_used(close);
        self.last_session_equity = Some(equity);
        if self.equity.len() <= bar {
            self.equity.resize(bar + 1, self.initial_capital);
        }
        self.equity[bar] = equity;
        if self.margin_used.len() <= bar {
            self.margin_used.resize(bar + 1, 0.0);
        }
        self.margin_used[bar] = margin_used;
    }

    fn open_entry_commission(&self) -> f64 {
        self.lots
            .iter()
            .map(|lot| lot.entry_commission_per_unit * lot.qty.abs())
            .sum()
    }

    fn record_report_open_lot(
        scope: &mut ReportScopeAccumulator,
        lot: &Lot,
        final_close: f64,
        pointvalue: f64,
        currency_rate: f64,
    ) {
        scope.aggregate.open_trades = scope.aggregate.open_trades.saturating_add(1);
        let open_profit = if final_close.is_finite() {
            (final_close - lot.entry_price) * lot.qty * pointvalue * currency_rate
                - lot.entry_commission_per_unit * lot.qty.abs()
        } else {
            -lot.entry_commission_per_unit * lot.qty.abs()
        };
        if open_profit.is_finite() {
            scope.aggregate.open_profit +=
                open_profit + lot.entry_commission_per_unit * lot.qty.abs();
        }
        // Closed rows already contain the pro-rated opening fee for quantities
        // that exited. Only the fee still carried by the live remainder is
        // added here, avoiding both pruning loss and double counting.
        let open_commission = lot.entry_commission_per_unit * lot.qty.abs();
        if open_commission.is_finite() {
            scope.aggregate.commission += open_commission;
            scope.aggregate.gross_loss += open_commission;
            scope.aggregate.net_profit -= open_commission;
        }
    }

    fn finalize_report_scope(
        mut scope: ReportScopeAccumulator,
        samples: &[(u32, f64, f64)],
        direction: Option<f64>,
    ) -> crate::StrategyReportScopeAggregate {
        let in_scope =
            |sign: f64| direction.is_none_or(|expected| sign.signum() == expected.signum());
        if scope.aggregate.closed_trades >= 2 {
            let values: Vec<f64> = samples
                .iter()
                .filter(|(_, sign, _)| in_scope(*sign))
                .map(|(_, _, pnl)| *pnl)
                .collect();
            if !values.is_empty() {
                let mean = values.iter().sum::<f64>() / values.len() as f64;
                let variance = values
                    .iter()
                    .map(|value| {
                        let delta = value - mean;
                        delta * delta
                    })
                    .sum::<f64>()
                    / values.len() as f64;
                let standard_deviation = libm::sqrt(variance);
                let lower_fence = mean - standard_deviation * 3.0;
                let upper_fence = mean + standard_deviation * 3.0;
                let mut indices = Vec::new();
                let mut total_pnl = 0.0;
                for (index, sign, pnl) in samples {
                    if in_scope(*sign) && (*pnl < lower_fence || *pnl > upper_fence) {
                        indices.push(*index);
                        total_pnl += *pnl;
                    }
                }
                scope.aggregate.outliers = crate::StrategyReportOutliers {
                    count: u32::try_from(indices.len()).unwrap_or(u32::MAX),
                    total_pnl,
                    lower_fence: Some(lower_fence),
                    upper_fence: Some(upper_fence),
                    trade_indices: indices,
                };
            }
        }
        scope.aggregate
    }

    fn report_aggregates(&self, final_close: f64) -> crate::StrategyReportAggregates {
        let mut all = self.report_all.clone();
        let mut long = self.report_long.clone();
        let mut short = self.report_short.clone();
        for lot in &self.lots {
            Self::record_report_open_lot(
                &mut all,
                lot,
                final_close,
                self.pointvalue,
                self.current_currency_rate,
            );
            let directional = if lot.qty >= 0.0 {
                &mut long
            } else {
                &mut short
            };
            Self::record_report_open_lot(
                directional,
                lot,
                final_close,
                self.pointvalue,
                self.current_currency_rate,
            );
        }
        crate::StrategyReportAggregates {
            all: Self::finalize_report_scope(all, &self.report_pnl_samples, None),
            long: Self::finalize_report_scope(long, &self.report_pnl_samples, Some(1.0)),
            short: Self::finalize_report_scope(short, &self.report_pnl_samples, Some(-1.0)),
        }
    }

    /// Consume the broker into report outcome pieces. Exported trade excursions
    /// include paid entry fees; the accessors read the separate gross values
    /// from the live broker before this consumed projection is constructed.
    pub fn into_outcome(mut self, final_close: f64) -> BrokerOutcome {
        let size = self.position_size();
        let avg = self.avg_price();
        let open_profit = self.open_profit(final_close);
        let first_retained_trade_index = self.first_retained_trade_index();
        let retained_trade_pnl_baseline = self.retained_trade_pnl_baseline();
        let report_aggregates = self.report_aggregates(final_close);
        let required_account_size = self
            .ever_held_position
            .then_some(self.maximum_margin_used + self.report_max_drawdown);
        // An aborting/uncommitted realtime bar can be synchronized without an
        // equity sample. Export only the committed, index-aligned prefix.
        self.currency_rates.truncate(self.equity.len());
        if self.currency_rates.len() < self.equity.len() {
            self.currency_rates.resize(self.equity.len(), None);
        }
        let currency_conversion_complete = !self.currencies_require_conversion()
            || self.currency_rates.iter().all(Option::is_some);
        let open_entry_commission = self.open_entry_commission();
        let mut account_metrics = self.metrics(open_profit);
        account_metrics.net_profit -= open_entry_commission;
        account_metrics.gross_loss += open_entry_commission;
        account_metrics.open_profit += open_entry_commission;
        account_metrics.max_drawdown = self.report_max_drawdown;
        account_metrics.max_runup = self.report_max_runup;
        account_metrics.avg_trade = if account_metrics.closed_trades > 0 {
            account_metrics.net_profit / f64::from(account_metrics.closed_trades)
        } else {
            0.0
        };
        BrokerOutcome {
            metrics: account_metrics,
            open_entry_commission,
            max_drawdown_percent: self.report_drawdown_percent,
            trades: self
                .trades
                .into_iter()
                .map(|mut trade| {
                    (trade.max_runup, trade.max_drawdown) = Self::report_trade_excursions(&trade);
                    trade
                })
                .collect(),
            first_retained_trade_index,
            retained_trade_pnl_baseline,
            first_entry_bar: self.first_entry_bar,
            first_entry_price: self.first_entry_price,
            first_entry_account_notional_per_unit: self.first_entry_account_notional_per_unit,
            account_currency: self.account_currency,
            symbol_currency: self.symbol_currency,
            currency_rates: self.currency_rates,
            currency_conversion_complete,
            position: StrategyPosition {
                size,
                avg_price: avg,
            },
            equity: self.equity,
            max_contracts_held: self.max_contracts_held,
            max_contracts_held_long: self.max_contracts_held_long,
            max_contracts_held_short: self.max_contracts_held_short,
            margin_used: self.margin_used,
            maximum_margin_used: self.maximum_margin_used,
            required_account_size,
            report_aggregates,
            margin_calls: self.margin_calls,
            total_liquidated: self.total_liquidated,
            largest_liquidation: self.largest_liquidation,
        }
    }

    /// A bounded report projection over retained trades and bar-close equity.
    /// This reads the completed broker without constructing the full outcome,
    /// collecting the trade deque, or computing extended report aggregates.
    /// It does not change execution, retention, or intrabar risk accounting.
    pub fn retained_objectives(&self) -> Option<RetainedObjectives> {
        let mut result = RetainedObjectives {
            initial_capital: self.initial_capital,
            net_profit: 0.0,
            gross_profit: 0.0,
            gross_loss: 0.0,
            closed_trades: self.trades.len(),
            total_commission: 0.0,
            final_equity: *self.equity.last()?,
            position_size: self.position_size(),
            max_drawdown: 0.0,
            first_retained_trade_index: self.first_retained_trade_index(),
        };
        for trade in &self.trades {
            if !trade.pnl.is_finite() || !trade.commission.is_finite() {
                return None;
            }
            result.net_profit += trade.pnl;
            result.gross_profit += trade.pnl.max(0.0);
            result.gross_loss += (-trade.pnl).max(0.0);
            result.total_commission += trade.commission;
        }
        let mut peak = self.initial_capital;
        for &equity in &self.equity {
            if !equity.is_finite() {
                return None;
            }
            peak = peak.max(equity);
            result.max_drawdown = result.max_drawdown.max(peak - equity);
        }
        [
            result.initial_capital,
            result.net_profit,
            result.gross_profit,
            result.gross_loss,
            result.total_commission,
            result.final_equity,
            result.position_size,
            result.max_drawdown,
        ]
        .iter()
        .all(|v| v.is_finite())
        .then_some(result)
    }

    /// Whether every committed bar resolved a symbol-to-account rate, identity
    /// included: the answer [`Broker::into_outcome`] exports, read without
    /// consuming the broker. A bar synchronized without an equity sample is
    /// not committed, and a committed bar without a rate is unresolved.
    fn currency_conversion_complete(&self) -> bool {
        !self.currencies_require_conversion()
            || (self.currency_rates.len() >= self.equity.len()
                && self
                    .currency_rates
                    .iter()
                    .take(self.equity.len())
                    .all(Option::is_some))
    }

    /// Commission charged across the whole run, as `[all, long, short]`: the
    /// `commission` of each scope of [`Broker::report_aggregates`], to the
    /// bit. Each scope starts from its closed trades' running total and adds
    /// the opening fee every open lot still carries, in lot order, exactly as
    /// `record_report_open_lot` does, without cloning a scope accumulator.
    fn report_commission(&self) -> [f64; 3] {
        let mut all = self.report_all.aggregate.commission;
        let mut long = self.report_long.aggregate.commission;
        let mut short = self.report_short.aggregate.commission;
        for lot in &self.lots {
            let open_commission = lot.entry_commission_per_unit * lot.qty.abs();
            if open_commission.is_finite() {
                all += open_commission;
                if lot.qty >= 0.0 {
                    long += open_commission;
                } else {
                    short += open_commission;
                }
            }
        }
        [all, long, short]
    }

    /// The compact closed-trade ledger of the completed run: full-run
    /// counters and trade-anchored intrabar drawdown beside the retained
    /// rows, marked at `final_close` exactly like [`Broker::into_outcome`].
    /// This borrows the completed broker without constructing the outcome,
    /// the per-bar lanes, or the report aggregates, and changes no execution,
    /// retention, or risk accounting. `None` when a value is not finite.
    pub fn trade_ledger(&self, final_close: f64) -> Option<TradeLedger<'_>> {
        let [commission_paid, commission_paid_long, commission_paid_short] =
            self.report_commission();
        let ledger = TradeLedger {
            initial_capital: self.initial_capital,
            mintick: self.mintick,
            point_value: self.pointvalue,
            slippage_ticks: self.slippage_ticks,
            account_currency: &self.account_currency,
            symbol_currency: &self.symbol_currency,
            currency_conversion_complete: self.currency_conversion_complete(),
            first_retained_trade_index: self.first_retained_trade_index(),
            closed_trades: self.closed_trades_total,
            first_entry_bar: self.first_entry_bar,
            first_entry_price: self.first_entry_price,
            first_entry_account_notional_per_unit: self.first_entry_account_notional_per_unit,
            net_profit: self.realized - self.open_entry_commission(),
            gross_profit: self.gross_profit,
            gross_loss: self.gross_loss + self.open_entry_commission(),
            open_profit: self.open_profit(final_close) + self.open_entry_commission(),
            open_entry_commission: self.open_entry_commission(),
            max_drawdown: self.report_max_drawdown,
            max_drawdown_percent: self.report_drawdown_percent,
            commission_paid,
            commission_paid_long,
            commission_paid_short,
            position: StrategyPosition {
                size: self.position_size(),
                avg_price: self.avg_price(),
            },
            trades: &self.trades,
        };
        [
            ledger.initial_capital,
            ledger.mintick,
            ledger.point_value,
            ledger.slippage_ticks,
            ledger.first_entry_price.unwrap_or(0.0),
            ledger.first_entry_account_notional_per_unit.unwrap_or(0.0),
            ledger.net_profit,
            ledger.gross_profit,
            ledger.gross_loss,
            ledger.open_profit,
            ledger.max_drawdown,
            ledger.max_drawdown_percent,
            ledger.open_entry_commission,
            ledger.commission_paid,
            ledger.commission_paid_long,
            ledger.commission_paid_short,
            ledger.position.size,
            ledger.position.avg_price,
        ]
        .iter()
        .all(|value| value.is_finite())
        .then_some(ledger)
    }

    // =======================================================================
    // AOT runtime surface. Additive: the interpreter
    // keeps driving `process_fills`/`record_snapshot`/`record_equity`
    // directly from `run_strategy_history_bar`, because it interleaves
    // script re-execution these entry points deliberately do not model
    // (`calc_on_order_fills` and `calc_on_every_history_tick` are structured
    // refusals in the native tier). The broker phases below are otherwise
    // call-for-call identical to the interpreter's sequencing.
    // =======================================================================

    /// Impose [`BrokerLimits`] on this broker's growable state. The interpreter
    /// never calls this (Pine semantics have no caps); the AOT
    /// runtime sets generous finite caps sized to its arena.
    pub fn set_limits(&mut self, limits: BrokerLimits) {
        self.limits = limits;
    }

    /// Check the configured [`BrokerLimits`] against the current book. The
    /// bar-walk entry points call this; it is public so an embedding can also
    /// poll after individual placements.
    pub fn check_limits(&self) -> Result<(), BrokerError> {
        let checks = [
            (
                "pending orders",
                self.pending.len(),
                self.limits.max_pending_orders,
            ),
            ("open lots", self.lots.len(), self.limits.max_open_lots),
            (
                "exit recipes",
                self.persistent_exits.len() + self.deferred_named_exits.len(),
                self.limits.max_exit_recipes,
            ),
        ];
        for (cap, actual, limit) in checks {
            if actual > limit {
                return Err(BrokerError::CapExceeded { cap, limit, actual });
            }
        }
        Ok(())
    }

    /// The bar-close posture lane value: the sign of the open position,
    /// `1`/`0`/`-1`. This is what a strategy standalone WebAssembly artifact writes to its
    /// designated lane each bar for `standalone_wasm_serialize`'s edge detection.
    pub fn posture(&self) -> i32 {
        let size = self.position_size();
        if size > 0.0 {
            1
        } else if size < 0.0 {
            -1
        } else {
            0
        }
    }

    /// The pre-script fill phase for one historical bar: the interpreter's
    /// 4-tick synthetic walk (`run_strategy_history_bar` in
    /// `pinewasm_vm`), minus the script executions it interleaves.
    ///
    /// The bar decomposes into the four ordered OHLC vertices — high first
    /// iff `|high - open| <= |open - low|` — and each adjacent pair forms a
    /// fill segment whose open is the previous vertex, so a crossed
    /// stop/limit order fills at its trigger price rather than the vertex
    /// extreme, and same-bar exit children join the book for the next
    /// segment of the same bar. The per-bar snapshot marks each vertex so
    /// open-lot excursions accumulate the intrabar path exactly as the
    /// interpreter records them.
    ///
    /// Call once per bar before the script body executes; orders the body
    /// places become eligible on the next bar's walk (or this bar's
    /// [`Broker::finish_bar`] close pass under `process_orders_on_close`).
    /// Returns the number of fills executed across the four segments.
    pub fn process_bar_ticks(
        &mut self,
        bar: usize,
        open: f64,
        high: f64,
        low: f64,
        close: f64,
    ) -> Result<usize, BrokerError> {
        self.check_limits()?;
        let high_first = (high - open).abs() <= (open - low).abs();
        let path = if high_first {
            [open, high, low, close]
        } else {
            [open, low, high, close]
        };
        let mut fills = 0usize;
        let mut previous_price = open;
        for price in path {
            // Historical bars model a continuous intrabar path between
            // adjacent OHLC vertices. The previous vertex is the segment
            // open; passing `price` for every field would incorrectly treat
            // each vertex as a gap and fill at the extreme.
            fills += self.process_fills(
                bar,
                previous_price,
                previous_price.max(price),
                previous_price.min(price),
                price,
                false,
            );
            // Each vertex is one available OHLC point: existing lots
            // accumulate the path vertex by vertex, while a lot opened on
            // this segment cannot inherit earlier extrema.
            self.observe_market(bar, price, price, price, price);
            previous_price = price;
        }
        self.capture_script_snapshot(bar);
        self.check_limits()?;
        Ok(fills)
    }

    /// The post-script phase for one historical bar: the
    /// `process_orders_on_close` close-tick pass (when the declaration set
    /// it) followed by the bar's equity/margin sample — the same sequencing
    /// the interpreter performs after the close-tick script execution.
    /// Returns the number of close-pass fills (always `0` when
    /// `process_orders_on_close` is off).
    pub fn finish_bar(&mut self, bar: usize, close: f64) -> Result<usize, BrokerError> {
        let fills = if self.process_orders_on_close {
            self.process_fills(bar, close, close, close, close, true)
        } else {
            0
        };
        self.record_equity(bar, close);
        self.check_limits()?;
        Ok(fills)
    }
}

/// A read-only, direction-signed view of one trade (closed or open), assembled
/// by [`Broker::trade_view`] for the `strategy.*trades.*` accessors. Prices,
/// size, profit, and commission are `f64` (`NaN` → the getter maps to `na`);
/// ids/comments borrow the trade's strings (`None` → `na`). `exit_*` fields are
/// meaningless for an open trade (`exit_price`/`max_*` are `NaN`, ids `None`).
pub struct TradeView<'a> {
    pub entry_price: f64,
    /// Account-currency entry notional for the represented quantity.
    pub entry_notional: f64,
    pub exit_price: f64,
    /// Signed traded size (positive long, negative short).
    pub size: f64,
    pub profit: f64,
    pub commission: f64,
    pub entry_bar: u32,
    pub exit_bar: u32,
    pub max_runup: f64,
    pub max_drawdown: f64,
    pub entry_id: Option<&'a str>,
    pub exit_id: Option<&'a str>,
    pub entry_comment: Option<&'a str>,
    pub exit_comment: Option<&'a str>,
    /// Direction sign (kept for completeness; `size` already carries it).
    _dir: f64,
}

/// `Some(s)` when `s` is non-empty, else `None` — trade ids default to the empty
/// string when an order carried no id, which reads as `na`.
fn non_empty(s: &str) -> Option<&str> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// The pieces the VM lifts out of a finished [`Broker`] to build a
/// `StrategyOutcome`. The metrics retain full-run incremental aggregates
/// even when the exported closed-trade detail has evicted rows whose boundary
/// fills left the retention window (`MAX_RETAINED_FILLED_ORDERS`), so realized profit is not
/// surfaced separately here.
pub struct BrokerOutcome {
    pub max_drawdown_percent: f64,
    pub open_entry_commission: f64,
    pub trades: Vec<StrategyTrade>,
    pub first_retained_trade_index: u32,
    pub retained_trade_pnl_baseline: f64,
    pub first_entry_bar: Option<u32>,
    pub first_entry_price: Option<f64>,
    pub first_entry_account_notional_per_unit: Option<f64>,
    pub account_currency: String,
    pub symbol_currency: String,
    pub currency_rates: Vec<Option<f64>>,
    pub currency_conversion_complete: bool,
    pub metrics: crate::strategy_metrics::StrategyMetrics,
    pub position: StrategyPosition,
    pub equity: Vec<f64>,
    /// Peak absolute aggregate position size during the run.
    pub max_contracts_held: f64,
    /// Peak aggregate long position size during the run.
    pub max_contracts_held_long: f64,
    /// Peak aggregate short position size (magnitude) during the run.
    pub max_contracts_held_short: f64,
    pub margin_used: Vec<f64>,
    pub maximum_margin_used: f64,
    pub required_account_size: Option<f64>,
    pub report_aggregates: crate::StrategyReportAggregates,
    pub margin_calls: u32,
    pub total_liquidated: f64,
    pub largest_liquidation: f64,
}

/// Retained-ledger objectives with bar-close (not intrabar) drawdown. Trade
/// totals cover the same retained rows as [`BrokerOutcome::trades`]; equity
/// and position cover the completed run. All fields are finite on success.
#[derive(Debug, Clone, PartialEq)]
pub struct RetainedObjectives {
    pub initial_capital: f64,
    pub net_profit: f64,
    pub gross_profit: f64,
    pub gross_loss: f64,
    pub closed_trades: usize,
    pub total_commission: f64,
    pub final_equity: f64,
    pub position_size: f64,
    pub max_drawdown: f64,
    pub first_retained_trade_index: u32,
}

/// The compact closed-trade ledger [`Broker::trade_ledger`] reads off a
/// completed run. Counters, profit and commission totals, open profit and
/// drawdown cover the whole run; [`TradeLedger::trades`] yields only the rows
/// the filled-order window retained. The ledger is therefore complete only
/// when the first retained index is zero and the row count equals
/// [`TradeLedger::closed_trades`]: an evicted row can sit between retained
/// ones. Every number is finite.
pub struct TradeLedger<'a> {
    pub initial_capital: f64,
    /// Symbol price increment every fill was quantized to.
    pub mintick: f64,
    /// Currency value of a one-point move for one contract.
    pub point_value: f64,
    /// Ticks of adverse slippage applied to market and stop fills.
    pub slippage_ticks: f64,
    pub account_currency: &'a str,
    pub symbol_currency: &'a str,
    pub currency_conversion_complete: bool,
    pub first_retained_trade_index: u32,
    /// Closed trades across the whole run, retained or not.
    pub closed_trades: u32,
    pub first_entry_bar: Option<u32>,
    pub first_entry_price: Option<f64>,
    pub first_entry_account_notional_per_unit: Option<f64>,
    pub net_profit: f64,
    pub gross_profit: f64,
    pub gross_loss: f64,
    /// Unrealized profit of the open position at the final close.
    pub open_profit: f64,
    /// Largest trade-anchored intrabar account drawdown, not the bar-close
    /// drawdown of [`RetainedObjectives`].
    pub max_drawdown: f64,
    /// Independent maximum of trade-anchored drawdown / pre-entry peak equity.
    pub max_drawdown_percent: f64,
    /// Already paid entry fees carried by the current open position.
    pub open_entry_commission: f64,
    /// Every commission the run was charged: the closed trades' commission,
    /// retained or not, plus the opening fee open lots still carry. This is
    /// the `commission` of [`crate::StrategyReportAggregates::all`], and it
    /// is already inside the profit fields: closed rows net their own
    /// commission and `open_profit` nets the open lots' opening fee, so
    /// subtracting it from either charges it twice. The retained rows'
    /// `commission` sum is the closed share alone.
    pub commission_paid: f64,
    /// [`TradeLedger::commission_paid`] for long trades and open long lots.
    pub commission_paid_long: f64,
    /// [`TradeLedger::commission_paid`] for short trades and open short lots.
    pub commission_paid_short: f64,
    pub position: StrategyPosition,
    trades: &'a VecDeque<StrategyTrade>,
}

impl<'a> TradeLedger<'a> {
    /// The retained rows in close order. A row is `None` when this broker
    /// did not produce it or one of its numbers is not finite.
    pub fn trades(&self) -> impl Iterator<Item = Option<LedgerTrade<'a>>> + 'a {
        let trades: &'a VecDeque<StrategyTrade> = self.trades;
        trades.iter().map(LedgerTrade::from_trade)
    }
}

/// One retained closed trade of a [`TradeLedger`]. `pnl` is
/// `(exit_price - entry_price) * qty * sign * point value * closing-bar
/// symbol-to-account rate - exit_commission - entry_commission`, evaluated
/// left to right, and `commission` is `entry_commission + exit_commission`.
pub struct LedgerTrade<'a> {
    /// Stable zero-based ordinal in the full run's closed-trade sequence.
    pub index: u32,
    pub entry_id: &'a str,
    /// The closing order's id, empty when it carried none.
    pub exit_id: &'a str,
    /// The closing order's selected comment, when present.
    pub comment: Option<&'a str>,
    /// Broker-authored name of a close or close-all order without a comment.
    pub exit_order_name: Option<&'a str>,
    pub sign: f64,
    pub qty: f64,
    pub entry_bar: u32,
    pub exit_bar: u32,
    pub entry_price: f64,
    pub exit_price: f64,
    pub pnl: f64,
    pub entry_notional: f64,
    /// The closed quantity's share of the opening fill's commission.
    pub entry_commission: f64,
    /// The closed quantity's share of the closing fill's commission.
    pub exit_commission: f64,
    pub commission: f64,
    /// Full-run cumulative realized profit immediately after this row closed.
    pub cumulative_pnl: f64,
    /// Report favorable excursion after the paid opening fee, clipped at zero.
    pub max_runup: f64,
    /// Report adverse excursion including the paid opening fee.
    pub max_drawdown: f64,
}

impl<'a> LedgerTrade<'a> {
    fn from_trade(trade: &'a StrategyTrade) -> Option<Self> {
        let split = trade.commission_split?;
        let (report_runup, report_drawdown) = Broker::report_trade_excursions(trade);
        let row = LedgerTrade {
            index: trade.index?,
            entry_id: &trade.entry_id,
            exit_id: trade.exit_id.as_deref().unwrap_or(""),
            comment: trade.comment.as_deref(),
            exit_order_name: trade.exit_order_name.as_deref(),
            sign: trade.sign,
            qty: trade.qty,
            entry_bar: trade.entry_bar,
            exit_bar: trade.exit_bar,
            entry_price: trade.entry_price,
            exit_price: trade.exit_price,
            pnl: trade.pnl,
            entry_notional: trade.entry_notional?,
            entry_commission: split.entry,
            exit_commission: split.exit,
            commission: trade.commission,
            cumulative_pnl: trade.cumulative_pnl?,
            max_runup: report_runup,
            max_drawdown: report_drawdown,
        };
        [
            row.sign,
            row.qty,
            row.entry_price,
            row.exit_price,
            row.pnl,
            row.entry_notional,
            row.entry_commission,
            row.exit_commission,
            row.commission,
            row.cumulative_pnl,
            row.max_runup,
            row.max_drawdown,
        ]
        .iter()
        .all(|value| value.is_finite())
        .then_some(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BrokerConfig;
    use alloc::string::ToString;

    fn config() -> BrokerConfig {
        BrokerConfig::default()
    }

    #[test]
    fn gross_script_excursions_are_separate_from_partial_report_projections() {
        for sign in [1.0, -1.0] {
            let cfg = BrokerConfig {
                initial_capital: 1000.0,
                process_orders_on_close: true,
                commission_type: "cash_per_order".into(),
                commission_value: 4.0,
                margin_long: 0.0,
                margin_short: 0.0,
                ..config()
            };
            let mut broker = Broker::new_with_symbol(&cfg, 0.01, 1.0, 1.0, "USD");
            script_bar(&mut broker, 0, (100.0, 100.0, 100.0, 100.0), |b| {
                b.place_entry(
                    "E".into(),
                    sign,
                    Some(2.0),
                    None,
                    None,
                    OrderMetadata::default(),
                );
            });
            script_bar(&mut broker, 1, (100.0, 101.0, 99.0, 100.0), |b| {
                b.place_close(
                    Some("E".into()),
                    Some(1.0),
                    None,
                    OrderMetadata::default(),
                    None,
                );
            });
            // Gross movement is one unit for the closed and remaining units.
            // Each carries two units of the four-unit entry fee. The report's
            // clipped favorable value is zero, from which gross cannot be recovered.
            for open in [false, true] {
                let view = broker.trade_view(0, open).unwrap();
                assert_eq!((view.max_runup, view.max_drawdown), (1.0, 1.0));
            }
            let partial = broker
                .trade_ledger(100.0)
                .unwrap()
                .trades()
                .next()
                .unwrap()
                .unwrap();
            assert_eq!((partial.max_runup, partial.max_drawdown), (0.0, 3.0));
            let close = if sign > 0.0 { 110.0 } else { 90.0 };
            script_bar(&mut broker, 2, (100.0, 120.0, 80.0, close), |b| {
                b.place_close(Some("E".into()), None, None, OrderMetadata::default(), None);
            });
            let second = broker.trade_view(1, false).unwrap();
            assert_eq!((second.max_runup, second.max_drawdown), (20.0, 20.0));
            // The account drawdown also carries the six already realized on
            // the partial exit (two of entry fee and four of exit fee):
            // 1000 down to 1000 - 6 - 20.
            let snapshot = broker.snapshot_at(2).unwrap();
            assert_eq!((snapshot.max_drawdown, snapshot.max_runup), (26.0, 20.0));
            let report = broker.clone().into_outcome(close);
            assert_eq!(
                (report.trades[0].max_runup, report.trades[0].max_drawdown),
                (0.0, 3.0)
            );
            assert_eq!(
                (report.trades[1].max_runup, report.trades[1].max_drawdown),
                (18.0, 22.0)
            );
            // The report's also counts the open unit's paid entry fee.
            assert_eq!(
                (report.metrics.max_drawdown, report.metrics.max_runup),
                (28.0, 18.0)
            );
            assert_eq!(report.required_account_size, Some(28.0));
            assert_eq!(broker.trade_ledger(close).unwrap().max_drawdown, 28.0);
            assert_eq!(report.report_aggregates.all.favorable_excursion_sum, 18.0);
            assert_eq!(report.report_aggregates.all.adverse_excursion_sum, 25.0);
            for (row, expected) in broker
                .trade_ledger(close)
                .unwrap()
                .trades()
                .zip(&report.trades)
            {
                let row = row.unwrap();
                assert_eq!(
                    (row.max_runup, row.max_drawdown),
                    (expected.max_runup, expected.max_drawdown)
                );
            }
            // Reporting consumes only a clone; the retained trade deque stays gross.
            assert_eq!(broker.trade_view(0, false).unwrap().max_runup, 1.0);
        }
    }

    #[test]
    fn account_drawdown_includes_paid_entry_commission_for_long_and_short_lots() {
        for sign in [1.0, -1.0] {
            let cfg = BrokerConfig {
                initial_capital: 1000.0,
                process_orders_on_close: true,
                commission_type: "cash_per_order".into(),
                commission_value: 2.0,
                margin_long: 0.0,
                margin_short: 0.0,
                ..config()
            };
            let mut broker = Broker::new_with_symbol(&cfg, 0.01, 1.0, 1.0, "USD");
            script_bar(&mut broker, 0, (100.0, 100.0, 100.0, 100.0), |b| {
                b.place_entry(
                    "E".into(),
                    sign,
                    Some(1.0),
                    None,
                    None,
                    OrderMetadata::default(),
                );
            });
            assert_eq!(broker.report_max_drawdown, 2.0, "entry fee is already paid");
            script_bar(&mut broker, 1, (100.0, 110.0, 90.0, 100.0), |_| {});
            assert_eq!(
                broker.report_max_drawdown, 12.0,
                "entry fee plus adverse price movement"
            );
            // The script-visible trade view remains gross; the run report includes fees.
            assert_eq!(broker.lots[0].max_drawdown, 10.0);
            assert_eq!(broker.trade_view(0, true).unwrap().max_drawdown, 10.0);
        }
    }

    #[test]
    fn account_drawdown_charges_percent_entry_commission_at_fill_value() {
        let cfg = BrokerConfig {
            initial_capital: 100_000.0,
            process_orders_on_close: true,
            commission_type: "percent".into(),
            commission_value: 0.1,
            margin_long: 0.0,
            ..config()
        };
        let mut broker = Broker::new_with_symbol(&cfg, 0.01, 1.0, 1.0, "USD");
        script_bar(
            &mut broker,
            0,
            (81_749.99, 81_749.99, 81_749.99, 81_749.99),
            |b| {
                b.place_entry(
                    "E".into(),
                    1.0,
                    Some(1.0),
                    None,
                    None,
                    OrderMetadata::default(),
                );
            },
        );
        script_bar(
            &mut broker,
            1,
            (81_749.99, 81_800.0, 81_534.02, 81_600.0),
            |_| {},
        );
        let expected = 215.97 + 81.74999;
        assert!((broker.report_max_drawdown - expected).abs() < 1e-8);
    }

    #[test]
    // Interpreting this 9,600-bar order walk under Miri with forced soft
    // floats exceeds the bounded runtime-safety budget on its own (the native
    // run takes about 120 ms; every other broker test is sub-millisecond).
    // The native and AddressSanitizer CI jobs run it unchanged, and the rest of
    // this suite keeps the retained-ledger kernels under Miri.
    #[cfg_attr(
        miri,
        ignore = "9,600-bar walk exceeds the bounded Miri budget; covered natively and under AddressSanitizer"
    )]
    fn compact_objectives_follow_retained_ledger_and_preserve_the_broker() {
        let cfg = BrokerConfig {
            initial_capital: 100_000.0,
            process_orders_on_close: true,
            commission_type: "cash_per_order".into(),
            commission_value: 0.125,
            ..config()
        };
        let mut broker = Broker::new_with_symbol(&cfg, 0.25, 1.0, 1.0, "USD");
        assert!(broker.retained_objectives().is_none()); // no completed equity
        for bar in 0..9600 {
            let price = if bar % 2 == 0 { 100.0 } else { 101.0 };
            broker.begin_session(bar as i64, bar, price);
            broker
                .process_bar_ticks(bar, price, price, price, price)
                .unwrap();
            if bar % 2 == 0 {
                broker.place_entry(
                    "L".into(),
                    1.0,
                    Some(1.0),
                    None,
                    None,
                    OrderMetadata::default(),
                );
            } else {
                broker.place_close(Some("L".into()), None, None, OrderMetadata::default(), None);
            }
            broker.finish_bar(bar, price).unwrap();
        }
        let first = broker.retained_objectives().unwrap();
        assert_eq!(broker.retained_objectives(), Some(first.clone()));
        let full = broker.into_outcome(101.0);
        assert!(first.first_retained_trade_index > 0);
        assert_eq!(
            first.first_retained_trade_index,
            full.first_retained_trade_index
        );
        assert_eq!(first.closed_trades, full.trades.len());
        assert!(first.closed_trades < full.metrics.closed_trades as usize);
        assert_eq!(
            first.net_profit,
            full.trades.iter().map(|t| t.pnl).sum::<f64>()
        );
        assert_eq!(
            first.total_commission,
            full.trades.iter().map(|t| t.commission).sum::<f64>()
        );
        assert_eq!(first.final_equity, *full.equity.last().unwrap());
        assert_eq!(first.position_size, full.position.size);
    }

    /// One bar under the AOT per-bar protocol: the pre-script fill walk, the
    /// script's placements, then the close pass and the equity sample.
    fn script_bar(
        broker: &mut Broker,
        bar: usize,
        (open, high, low, close): (f64, f64, f64, f64),
        place: impl FnOnce(&mut Broker),
    ) {
        broker.begin_session(bar as i64, bar, open);
        broker
            .process_bar_ticks(bar, open, high, low, close)
            .expect("no caps set");
        broker.begin_script_execution();
        place(broker);
        broker.finish_bar(bar, close).expect("no caps set");
    }

    #[test]
    fn declared_order_prices_round_toward_the_harder_fill() {
        let broker = Broker::new_with_symbol(&config(), 0.01, 1.0, 1.0, "USD");
        let round = |buy, kind| broker.round_order_price(100.005, buy, kind);
        assert_eq!(round(true, OrderPriceKind::Limit), 100.0);
        assert_eq!(round(false, OrderPriceKind::Limit), 100.01);
        assert_eq!(round(true, OrderPriceKind::Stop), 100.01);
        assert_eq!(round(false, OrderPriceKind::Stop), 100.0);
        // A price already on the grid stays put whichever way it rounds.
        for (buy, kind) in [
            (true, OrderPriceKind::Limit),
            (false, OrderPriceKind::Limit),
            (true, OrderPriceKind::Stop),
            (false, OrderPriceKind::Stop),
        ] {
            assert_eq!(broker.round_order_price(78610.45, buy, kind), 78610.45);
            // Past 2^24 ticks a fixed epsilon is below the quotient's error.
            // The price stays on its own tick, as a fill would record it.
            assert_eq!(
                broker.round_order_price(223702.36, buy, kind),
                broker.quantize_fill_price(223702.36)
            );
        }
        let fine = Broker::new_with_symbol(&config(), 0.001, 1.0, 1.0, "USD");
        for (buy, kind) in [
            (true, OrderPriceKind::Limit),
            (false, OrderPriceKind::Limit),
            (true, OrderPriceKind::Stop),
            (false, OrderPriceKind::Stop),
        ] {
            assert_eq!(
                fine.round_order_price(16777.314, buy, kind),
                fine.quantize_fill_price(16777.314)
            );
        }
    }

    #[test]
    fn a_commented_close_carries_no_order_name() {
        let mut broker = Broker::new_with_symbol(&config(), 0.01, 1.0, 1.0, "USD");
        let flat = (100.0, 100.0, 100.0, 100.0);
        script_bar(&mut broker, 0, flat, |b| {
            b.place_entry(
                "L".into(),
                1.0,
                Some(1.0),
                None,
                None,
                OrderMetadata::default(),
            );
        });
        script_bar(&mut broker, 1, flat, |b| {
            let metadata = OrderMetadata {
                comment: Some("Take it".into()),
                ..OrderMetadata::default()
            };
            b.place_close(Some("L".into()), None, None, metadata, None);
        });
        script_bar(&mut broker, 2, flat, |_| {});
        assert_eq!(broker.trades.len(), 1);
        assert_eq!(broker.trades[0].comment.as_deref(), Some("Take it"));
        assert_eq!(broker.trades[0].exit_order_name, None);
    }

    #[test]
    fn a_buy_limit_between_ticks_fills_at_the_lower_tick() {
        let mut broker = Broker::new_with_symbol(&config(), 0.01, 1.0, 1.0, "USD");
        script_bar(&mut broker, 0, (101.0, 101.0, 101.0, 101.0), |b| {
            b.place_entry(
                "L".into(),
                1.0,
                Some(1.0),
                Some(100.005),
                None,
                OrderMetadata::default(),
            );
        });
        assert_eq!(broker.pending[0].limit, Some(100.0));
        // Rounded to the nearest tick, 100.005 would fill a cent higher.
        script_bar(&mut broker, 1, (101.0, 101.0, 99.9, 100.5), |_| {});
        assert_eq!(broker.position_size(), 1.0);
        assert_eq!(broker.avg_price(), 100.0);
    }

    #[test]
    fn exit_stops_round_by_side_even_when_already_crossed() {
        // The market sits on either side of the stop when it is declared;
        // only the exit's side decides the rounding.
        for (sign, expected) in [(1.0, 126.29), (-1.0, 126.3)] {
            for price in [120.0, 130.0] {
                let mut broker = Broker::new_with_symbol(&config(), 0.01, 1.0, 1.0, "USD");
                script_bar(&mut broker, 0, (125.0, 125.0, 125.0, 125.0), |b| {
                    b.place_entry(
                        "E".into(),
                        sign,
                        Some(1.0),
                        None,
                        None,
                        OrderMetadata::default(),
                    );
                });
                script_bar(&mut broker, 1, (price, price, price, price), |b| {
                    b.place_exit(
                        "X".into(),
                        Some("E".into()),
                        None,
                        None,
                        None,
                        None,
                        None,
                        Some(126.295),
                        None,
                        None,
                        None,
                        ExitMetadata::default(),
                    );
                });
                let order = broker
                    .pending
                    .iter()
                    .find(|order| order.id == "X")
                    .expect("the exit stays pending");
                assert_eq!(order.stop, Some(expected), "sign {sign}, market {price}");
            }
        }
    }

    #[test]
    fn closing_commands_carry_exit_order_names() {
        let mut broker = Broker::new_with_symbol(&config(), 0.01, 1.0, 1.0, "USD");
        let flat = (100.0, 100.0, 100.0, 100.0);
        script_bar(&mut broker, 0, flat, |b| {
            b.place_entry(
                "L".into(),
                1.0,
                Some(1.0),
                None,
                None,
                OrderMetadata::default(),
            );
        });
        script_bar(&mut broker, 1, flat, |b| {
            b.place_close(Some("L".into()), None, None, OrderMetadata::default(), None);
        });
        script_bar(&mut broker, 2, flat, |b| {
            b.place_entry(
                "S".into(),
                -1.0,
                Some(1.0),
                None,
                None,
                OrderMetadata::default(),
            );
        });
        script_bar(&mut broker, 3, flat, |b| {
            b.place_close(None, None, None, OrderMetadata::default(), None);
        });
        script_bar(&mut broker, 4, flat, |b| {
            b.place_entry(
                "R".into(),
                1.0,
                Some(1.0),
                None,
                None,
                OrderMetadata::default(),
            );
        });
        script_bar(&mut broker, 5, flat, |b| {
            b.place_exit(
                "X".into(),
                Some("R".into()),
                None,
                None,
                None,
                Some(100.0),
                None,
                None,
                None,
                None,
                None,
                ExitMetadata::default(),
            );
        });
        script_bar(&mut broker, 6, flat, |_| {});
        let names: Vec<_> = broker
            .trades
            .iter()
            .map(|trade| (trade.exit_id.as_deref(), trade.exit_order_name.as_deref()))
            .collect();
        assert_eq!(
            names,
            [
                (Some("L"), Some("Close entry(s) order L")),
                (None, Some("Close position order")),
                (Some("X"), None),
            ]
        );
    }

    #[test]
    fn pyramided_trades_under_water_add_up_in_the_account_drawdown() {
        let cfg = BrokerConfig {
            pyramiding: 2,
            ..config()
        };
        let mut broker = Broker::new_with_symbol(&cfg, 0.01, 1.0, 1.0, "USD");
        let flat = (100.0, 100.0, 100.0, 100.0);
        for bar in 0..2 {
            script_bar(&mut broker, bar, flat, |b| {
                b.place_entry(
                    "L".into(),
                    1.0,
                    Some(1.0),
                    None,
                    None,
                    OrderMetadata::default(),
                );
            });
        }
        script_bar(&mut broker, 2, flat, |_| {});
        assert_eq!(broker.position_size(), 2.0);
        // Both units are ten under at the low: twenty, not ten per trade.
        script_bar(&mut broker, 3, (100.0, 100.0, 90.0, 95.0), |_| {});
        assert_eq!(broker.snapshot_at(3).unwrap().max_drawdown, 20.0);
    }

    #[test]
    fn trade_ledger_matches_the_outcome_and_keeps_both_commission_charges() {
        let cfg = BrokerConfig {
            initial_capital: 10_000.0,
            pyramiding: 2,
            commission_type: "cash_per_order".into(),
            commission_value: 1.25,
            slippage: 2,
            ..config()
        };
        let market = OrderMetadata::default;
        let mut broker = Broker::new_with_symbol(&cfg, 0.25, 2.0, 1.0, "USD");
        // A run without a completed bar is an empty ledger, not a refusal.
        let empty = broker.trade_ledger(f64::NAN).expect("empty ledger");
        assert_eq!((empty.closed_trades, empty.trades().count()), (0, 0));
        assert_eq!((empty.first_entry_bar, empty.open_profit), (None, 0.0));

        script_bar(&mut broker, 0, (100.0, 101.0, 99.0, 100.0), |b| {
            b.place_entry("A".into(), 1.0, Some(3.0), None, None, market());
        });
        script_bar(&mut broker, 1, (100.0, 102.0, 99.5, 101.0), |b| {
            b.place_entry("B".into(), 1.0, Some(1.0), None, None, market());
        });
        script_bar(&mut broker, 2, (101.0, 103.0, 100.5, 102.0), |b| {
            b.place_close(None, None, None, market(), None);
        });
        script_bar(&mut broker, 3, (104.0, 105.0, 103.0, 104.0), |b| {
            b.place_entry("S".into(), -1.0, Some(2.0), None, None, market());
        });
        script_bar(&mut broker, 4, (104.0, 104.5, 102.0, 103.0), |b| {
            b.place_entry("L".into(), 1.0, Some(4.0), None, None, market());
        });
        script_bar(&mut broker, 5, (102.0, 103.0, 101.0, 102.5), |b| {
            b.place_close(Some("L".into()), Some(1.0), None, market(), None);
        });
        script_bar(&mut broker, 6, (103.0, 104.0, 102.0, 103.5), |_| {});

        let final_close = 103.5;
        let ledger = broker.trade_ledger(final_close).expect("finite ledger");
        let rows: Vec<LedgerTrade<'_>> = ledger
            .trades()
            .collect::<Option<_>>()
            .expect("broker-produced rows");
        let outcome = broker.clone().into_outcome(final_close);

        assert_eq!(ledger.initial_capital, 10_000.0);
        assert_eq!(ledger.mintick, 0.25);
        assert_eq!(ledger.point_value, 2.0);
        assert_eq!(ledger.slippage_ticks, 2.0);
        assert_eq!(ledger.account_currency, outcome.account_currency);
        assert_eq!(ledger.symbol_currency, "USD");
        assert!(ledger.currency_conversion_complete);
        assert_eq!(
            ledger.first_retained_trade_index,
            outcome.first_retained_trade_index
        );
        assert_eq!(ledger.closed_trades, outcome.metrics.closed_trades);
        assert_eq!(ledger.first_entry_bar, Some(1));
        assert_eq!(ledger.first_entry_price, Some(100.5));
        assert_eq!(ledger.first_entry_account_notional_per_unit, Some(201.0));
        assert_eq!(ledger.first_entry_bar, outcome.first_entry_bar);
        assert_eq!(ledger.first_entry_price, outcome.first_entry_price);
        assert_eq!(
            ledger.first_entry_account_notional_per_unit,
            outcome.first_entry_account_notional_per_unit
        );
        assert_eq!(ledger.net_profit, outcome.metrics.net_profit);
        assert_eq!(ledger.gross_profit, outcome.metrics.gross_profit);
        assert_eq!(ledger.gross_loss, outcome.metrics.gross_loss);
        assert_eq!(ledger.open_profit, outcome.metrics.open_profit);
        assert_eq!(ledger.max_drawdown, outcome.metrics.max_drawdown);
        assert_eq!(ledger.position, outcome.position);
        assert_eq!(ledger.position.size, 3.0);
        assert!(ledger.open_profit != 0.0 && ledger.max_drawdown > 0.0);
        // The report's commission, scope by scope, to the bit.
        let report = &outcome.report_aggregates;
        for (ledger, report) in [
            (ledger.commission_paid, report.all.commission),
            (ledger.commission_paid_long, report.long.commission),
            (ledger.commission_paid_short, report.short.commission),
        ] {
            assert_eq!(ledger.to_bits(), report.to_bits());
        }

        assert_eq!(rows.len(), 4);
        assert_eq!(rows.len(), outcome.trades.len());
        let mut realized = 0.0;
        for (row, trade) in rows.iter().zip(&outcome.trades) {
            assert_eq!(Some(row.index), trade.index);
            assert_eq!(row.entry_id, trade.entry_id);
            assert_eq!(row.exit_id, trade.exit_id.as_deref().unwrap_or(""));
            assert_eq!(row.sign, trade.sign);
            assert_eq!(row.qty, trade.qty);
            assert_eq!(row.entry_bar, trade.entry_bar);
            assert_eq!(row.exit_bar, trade.exit_bar);
            assert_eq!(row.entry_price, trade.entry_price);
            assert_eq!(row.exit_price, trade.exit_price);
            assert_eq!(row.pnl, trade.pnl);
            assert_eq!(Some(row.entry_notional), trade.entry_notional);
            assert_eq!(row.commission, trade.commission);
            assert_eq!(Some(row.cumulative_pnl), trade.cumulative_pnl);
            assert_eq!(row.max_runup, trade.max_runup);
            assert_eq!(row.max_drawdown, trade.max_drawdown);

            // What a verifier recomputes from the row alone, in the broker's
            // operation order and at the identity currency rate, bit for bit.
            let gross =
                (row.exit_price - row.entry_price) * row.qty * row.sign * ledger.point_value * 1.0;
            assert_eq!(
                (gross - row.exit_commission - row.entry_commission).to_bits(),
                row.pnl.to_bits()
            );
            assert_eq!(
                (row.entry_commission + row.exit_commission).to_bits(),
                row.commission.to_bits()
            );
            assert_eq!(
                (row.entry_price.abs() * ledger.point_value * 1.0 * row.qty).to_bits(),
                row.entry_notional.to_bits()
            );
            realized += row.pnl;
            assert_eq!(realized.to_bits(), row.cumulative_pnl.to_bits());
        }
        assert_eq!(
            (realized - ledger.open_entry_commission).to_bits(),
            ledger.net_profit.to_bits()
        );

        // One closing order across two lots: its single charge is shared by
        // closed quantity, three of four contracts and then the remainder.
        assert_eq!(
            (rows[0].entry_id, rows[0].qty, rows[0].exit_bar),
            ("A", 3.0, 3)
        );
        assert_eq!(rows[0].entry_commission, 1.25 / 3.0 * 3.0);
        assert_eq!(rows[0].exit_commission, 0.9375);
        assert_eq!(
            (rows[1].entry_id, rows[1].qty, rows[1].exit_bar),
            ("B", 1.0, 3)
        );
        assert_eq!(rows[1].entry_commission, 1.25);
        assert_eq!(rows[1].exit_commission, 0.3125);
        assert_eq!(rows[1].pnl, 4.0 - 0.3125 - 1.25);
        // A reversing entry: its closing leg carries two of the order's six
        // contracts, and the lot it opens carries the rest.
        assert_eq!(
            (rows[2].entry_id, rows[2].sign, rows[2].exit_bar),
            ("S", -1.0, 5)
        );
        assert_eq!(rows[2].entry_commission, 1.25 / 2.0 * 2.0);
        assert_eq!(rows[2].exit_commission, 1.25 * 2.0 / 6.0);
        // A partial close takes one contract's share of that opening charge
        // and the whole charge of its own order.
        let opening = 1.25 - 1.25 * 2.0 / 6.0;
        assert_eq!(
            (rows[3].entry_id, rows[3].qty, rows[3].exit_bar),
            ("L", 1.0, 6)
        );
        assert_eq!(rows[3].entry_commission, opening / 4.0 * 1.0);
        assert_eq!(rows[3].exit_commission, 1.25);
        // The sum alone would not have told these apart.
        assert_ne!(rows[2].entry_commission, rows[2].exit_commission);

        // The other three of `L`'s contracts are still open and carry the
        // rest of that opening charge, which no closed row holds. Commission
        // paid is the rows' running total and then that: six orders at 1.25.
        let closed = rows.iter().fold(0.0, |sum, row| sum + row.commission);
        let carried = opening / 4.0 * 3.0;
        assert!(carried > 0.0);
        assert_eq!(ledger.commission_paid, closed + carried);
        assert!((ledger.commission_paid - 6.0 * 1.25).abs() < 1e-12);
        assert_eq!(
            ledger.commission_paid_long,
            rows[0].commission + rows[1].commission + rows[3].commission + carried
        );
        assert_eq!(ledger.commission_paid_short, rows[2].commission);

        // Reading the ledger changed nothing.
        let again = broker.trade_ledger(final_close).expect("finite ledger");
        assert_eq!(again.net_profit, ledger.net_profit);
        assert_eq!(again.commission_paid, ledger.commission_paid);
        assert_eq!(again.trades().count(), rows.len());
    }

    #[test]
    fn paid_opening_fees_are_realized_in_every_account_projection() {
        let cfg = BrokerConfig {
            initial_capital: 1_000.0,
            process_orders_on_close: true,
            commission_type: "cash_per_order".into(),
            commission_value: 2.0,
            ..config()
        };
        let mut broker = Broker::new_with_symbol(&cfg, 0.01, 1.0, 1.0, "USD");
        script_bar(&mut broker, 0, (100.0, 100.0, 100.0, 100.0), |b| {
            b.place_entry(
                "A".into(),
                1.0,
                Some(1.0),
                None,
                None,
                OrderMetadata::default(),
            );
        });
        script_bar(&mut broker, 1, (110.0, 110.0, 110.0, 110.0), |b| {
            b.place_close(None, None, None, OrderMetadata::default(), None);
        });
        script_bar(&mut broker, 2, (100.0, 100.0, 100.0, 100.0), |b| {
            b.place_entry(
                "B".into(),
                1.0,
                Some(2.0),
                None,
                None,
                OrderMetadata::default(),
            );
        });
        script_bar(&mut broker, 3, (105.0, 105.0, 105.0, 105.0), |_| {});
        let snapshot = broker.snapshot_at(3).unwrap();
        let ledger = broker.trade_ledger(105.0).unwrap();
        let outcome = broker.clone().into_outcome(105.0);
        // The closed winner earned 6. The still-open entry has already paid 2;
        // its 10 of price movement is unrealized. Transfer the paid fee only
        // once: equity remains 1,014 on all three surfaces.
        assert_eq!(outcome.trades[0].pnl, 6.0);
        // Preserve the existing script-visible gross-loss definition while
        // the report includes the already paid open entry fee.
        assert_eq!(
            (
                snapshot.net_profit,
                snapshot.gross_profit,
                snapshot.gross_loss,
                snapshot.open_profit
            ),
            (4.0, 6.0, 0.0, 10.0)
        );
        for (net, gain, loss, open) in [
            (
                ledger.net_profit,
                ledger.gross_profit,
                ledger.gross_loss,
                ledger.open_profit,
            ),
            (
                outcome.metrics.net_profit,
                outcome.metrics.gross_profit,
                outcome.metrics.gross_loss,
                outcome.metrics.open_profit,
            ),
            (
                outcome.report_aggregates.all.net_profit,
                outcome.report_aggregates.all.gross_profit,
                outcome.report_aggregates.all.gross_loss,
                outcome.report_aggregates.all.open_profit,
            ),
        ] {
            assert_eq!((net, gain, loss, open), (4.0, 6.0, 2.0, 10.0));
            assert_eq!(cfg.initial_capital + net + open, 1_014.0);
        }
        assert_eq!(ledger.open_entry_commission, 2.0);
        assert_eq!(outcome.open_entry_commission, 2.0);
        assert_eq!(ledger.commission_paid, 6.0);
        assert_eq!(
            outcome.metrics.gross_profit / outcome.metrics.gross_loss,
            3.0
        );
        assert_eq!(outcome.trades[0].max_runup, 8.0);
        assert_eq!(outcome.trades[0].max_drawdown, 2.0);
    }

    #[test]
    fn drawdown_percentage_has_its_own_trade_anchored_maximum() {
        let cfg = BrokerConfig {
            initial_capital: 1_000.0,
            margin_long: 0.0,
            process_orders_on_close: true,
            commission_type: "cash_per_order".into(),
            commission_value: 1.0,
            ..config()
        };
        let mut broker = Broker::new_with_symbol(&cfg, 0.01, 1.0, 1.0, "USD");
        script_bar(&mut broker, 0, (100.0, 100.0, 100.0, 100.0), |b| {
            b.place_entry(
                "A".into(),
                1.0,
                Some(1.0),
                None,
                None,
                OrderMetadata::default(),
            );
        });
        script_bar(&mut broker, 1, (100.0, 2_000.0, 50.0, 1_100.0), |b| {
            b.place_close(None, None, None, OrderMetadata::default(), None);
        });
        script_bar(&mut broker, 2, (1_000.0, 1_000.0, 1_000.0, 1_000.0), |b| {
            b.place_entry(
                "B".into(),
                1.0,
                Some(2.0),
                None,
                None,
                OrderMetadata::default(),
            );
        });
        script_bar(&mut broker, 3, (1_000.0, 1_000.0, 974.5, 975.0), |_| {});
        let snapshot = broker.snapshot_at(3).unwrap();
        let ledger = broker.trade_ledger(975.0).unwrap();
        let outcome = broker.clone().into_outcome(975.0);
        // First trade: (50 + 1) / 1,000 = 5.1%. Second trade: 52 / 1,998,
        // a larger cash loss but a smaller relative loss. The unrealized high
        // of 2,000 never becomes an account drawdown anchor.
        assert_eq!(outcome.metrics.max_drawdown, 52.0);
        assert!(snapshot.max_drawdown_percent > 5.1);
        for percentage in [ledger.max_drawdown_percent, outcome.max_drawdown_percent] {
            assert!((percentage - 5.1).abs() < 1e-12);
        }
    }

    #[test]
    fn trade_ledger_commission_is_the_report_commission_on_every_bar() {
        // Percent commission on prices that are not round: every charge is an
        // inexact product, so the order of the additions decides the last
        // bit. The run holds a long lot, closes part of it, reverses into a
        // short, pyramids a second short lot, closes part of the first, and
        // ends flat. After each bar the ledger's three totals are the
        // report's own.
        let cfg = BrokerConfig {
            initial_capital: 50_000.0,
            pyramiding: 3,
            commission_type: "percent".into(),
            commission_value: 0.075,
            ..config()
        };
        let market = OrderMetadata::default;
        let mut broker = Broker::new_with_symbol(&cfg, 0.01, 1.0, 1.0, "USD");
        // (open lots, of which long, closed trades) after each bar.
        let mut seen = Vec::new();
        for bar in 0..10 {
            let open = 97.13 + 1.37 * bar as f64;
            let close = open + 0.41;
            let prices = (open, open + 0.9, open - 0.7, close);
            script_bar(&mut broker, bar, prices, |b| match bar {
                0 => b.place_entry("L".into(), 1.0, Some(3.0), None, None, market()),
                1 => {
                    b.place_close(Some("L".into()), Some(1.0), None, market(), None);
                }
                2 => b.place_entry("S".into(), -1.0, Some(5.0), None, None, market()),
                3 => b.place_entry("S2".into(), -1.0, Some(2.0), None, None, market()),
                4 => {
                    b.place_close(Some("S".into()), Some(2.0), None, market(), None);
                }
                7 => {
                    b.place_close(None, None, None, market(), None);
                }
                _ => {}
            });
            let ledger = broker.trade_ledger(close).expect("finite ledger");
            let report = broker.clone().into_outcome(close).report_aggregates;
            for (ledger, report) in [
                (ledger.commission_paid, report.all.commission),
                (ledger.commission_paid_long, report.long.commission),
                (ledger.commission_paid_short, report.short.commission),
            ] {
                assert_eq!(ledger.to_bits(), report.to_bits(), "bar {bar}");
            }
            // What the closed rows hold, in each scope, in close order.
            let closed = |keep: fn(f64) -> bool| {
                ledger
                    .trades()
                    .map(|row| row.expect("broker-produced row"))
                    .filter(|row| keep(row.sign))
                    .fold(0.0, |sum, row| sum + row.commission)
            };
            let carried = [
                ledger.commission_paid - closed(|_| true),
                ledger.commission_paid_long - closed(|sign| sign > 0.0),
                ledger.commission_paid_short - closed(|sign| sign < 0.0),
            ];
            let lots = broker.lots.len();
            let long_lots = broker.lots.iter().filter(|lot| lot.qty > 0.0).count();
            if lots == 0 {
                // Flat: the rows are all of it, to the bit.
                assert_eq!(carried, [0.0; 3], "bar {bar}");
            } else {
                // An open lot's fee is in its own direction's total only.
                assert!(carried[0] > 0.0, "bar {bar}");
                assert_eq!(carried[1] > 0.0, long_lots > 0, "bar {bar}");
                assert_eq!(carried[2] > 0.0, long_lots < lots, "bar {bar}");
            }
            seen.push((lots, long_lots, ledger.closed_trades));
        }
        assert_eq!(
            seen,
            [
                // Nothing filled; the long lot; one of its contracts closed.
                (0, 0, 0),
                (1, 1, 0),
                (1, 1, 1),
                // Reversed short; a second short lot; part of the first closed.
                (1, 0, 2),
                (2, 0, 2),
                (2, 0, 3),
                (2, 0, 3),
                (2, 0, 3),
                // Both lots closed by one order.
                (0, 0, 5),
                (0, 0, 5),
            ]
        );
    }

    #[test]
    fn trade_ledger_adds_open_lot_commission_in_the_report_order() {
        // Three short lots stay open behind a closed long trade and a closed
        // contract of the first short lot. At these prices the percent
        // charges do not add up to the same bits in any other order, so the
        // totals pin the order itself: a scope's closed total first, then
        // each open lot's carried commission, first lot to last.
        let cfg = BrokerConfig {
            initial_capital: 50_000.0,
            pyramiding: 3,
            commission_type: "percent".into(),
            commission_value: 0.075,
            ..config()
        };
        let market = OrderMetadata::default;
        let mut broker = Broker::new_with_symbol(&cfg, 0.01, 1.0, 1.0, "USD");
        let open_of = |bar: usize| 107.91 + 1.37 * bar as f64;
        for bar in 0..7 {
            let open = open_of(bar);
            let prices = (open, open + 0.9, open - 0.7, open + 0.41);
            script_bar(&mut broker, bar, prices, |b| match bar {
                0 => b.place_entry("A".into(), 1.0, Some(2.0), None, None, market()),
                1 => {
                    b.place_close(Some("A".into()), None, None, market(), None);
                }
                2 => b.place_entry("S1".into(), -1.0, Some(3.0), None, None, market()),
                3 => b.place_entry("S2".into(), -1.0, Some(2.0), None, None, market()),
                4 => b.place_entry("S3".into(), -1.0, Some(4.0), None, None, market()),
                5 => {
                    b.place_close(Some("S1".into()), Some(1.0), None, market(), None);
                }
                _ => {}
            });
        }
        let close = open_of(6) + 0.41;
        assert_eq!(broker.closed_trades_total, 2);
        let carried: Vec<f64> = broker
            .lots
            .iter()
            .map(|lot| lot.entry_commission_per_unit * lot.qty.abs())
            .collect();
        assert_eq!(carried.len(), 3);
        assert!(broker.lots.iter().all(|lot| lot.qty < 0.0));

        let ledger = broker.trade_ledger(close).expect("finite ledger");
        let report = broker.clone().into_outcome(close).report_aggregates;
        for (closed, paid, reported) in [
            (
                broker.report_all.aggregate.commission,
                ledger.commission_paid,
                report.all.commission,
            ),
            (
                broker.report_short.aggregate.commission,
                ledger.commission_paid_short,
                report.short.commission,
            ),
        ] {
            let in_order = carried.iter().fold(closed, |sum, fee| sum + fee);
            let lots_first = closed + carried.iter().fold(0.0, |sum, fee| sum + fee);
            let last_lot_first = carried.iter().rev().fold(closed, |sum, fee| sum + fee);
            // The fixture tells the orders apart, or it pins nothing.
            assert_ne!(in_order.to_bits(), lots_first.to_bits());
            assert_ne!(in_order.to_bits(), last_lot_first.to_bits());
            assert_eq!(paid.to_bits(), in_order.to_bits());
            assert_eq!(paid.to_bits(), reported.to_bits());
        }
        // No long lot is open: the long total is the closed long trade's.
        let closed_long = broker.report_long.aggregate.commission;
        assert!(closed_long > 0.0);
        assert_eq!(ledger.commission_paid_long.to_bits(), closed_long.to_bits());
        assert_eq!(
            ledger.commission_paid_long.to_bits(),
            report.long.commission.to_bits()
        );
    }

    #[test]
    fn trade_ledger_refuses_values_it_cannot_vouch_for() {
        let cfg = BrokerConfig {
            process_orders_on_close: true,
            ..config()
        };
        let mut broker = Broker::new(&cfg, 0.01);
        script_bar(&mut broker, 0, (100.0, 100.0, 100.0, 100.0), |b| {
            b.place_entry(
                "L".into(),
                1.0,
                Some(1.0),
                None,
                None,
                OrderMetadata::default(),
            );
        });
        script_bar(&mut broker, 1, (101.0, 101.0, 101.0, 101.0), |b| {
            b.place_close(None, None, None, OrderMetadata::default(), None);
        });
        let produced = broker.trades.back().cloned().expect("one closed trade");
        assert!(broker
            .trade_ledger(101.0)
            .expect("finite ledger")
            .trades()
            .all(|row| row.is_some()));

        // A row this broker did not produce carries no split to report.
        let mut foreign = produced.clone();
        foreign.commission_split = None;
        // A produced row with a number that is not finite.
        let mut unbounded = produced.clone();
        unbounded.pnl = f64::NAN;
        let mut unsplit = produced;
        unsplit.commission_split = Some(CommissionSplit {
            entry: f64::INFINITY,
            exit: 0.0,
        });
        for row in [foreign, unbounded, unsplit] {
            broker.trades.push_back(row);
            let ledger = broker.trade_ledger(101.0).expect("finite header");
            assert!(ledger.trades().last().expect("the row").is_none());
            broker.trades.pop_back();
        }

        let mut header = broker.clone();
        header.realized = f64::INFINITY;
        assert!(header.trade_ledger(101.0).is_none());
        let mut entry = broker.clone();
        entry.first_entry_price = Some(f64::NAN);
        assert!(entry.trade_ledger(101.0).is_none());
        // Each commission total is held to the same rule on its own.
        for scope in 0..3 {
            let mut charged = broker.clone();
            let total = match scope {
                0 => &mut charged.report_all,
                1 => &mut charged.report_long,
                _ => &mut charged.report_short,
            };
            total.aggregate.commission = f64::INFINITY;
            assert!(charged.trade_ledger(101.0).is_none(), "scope {scope}");
        }
        assert!(broker.trade_ledger(101.0).is_some());
    }

    #[test]
    fn trade_ledger_reports_currency_conversion_like_the_outcome() {
        let converting = BrokerConfig {
            currency: "EUR".into(),
            ..config()
        };
        // (declaration, rate synchronized per bar, committed bars, complete)
        let cases: [(&BrokerConfig, &[Option<f64>], usize, bool); 5] = [
            (&config(), &[None, None], 2, true),
            (&converting, &[Some(0.9), Some(0.91)], 2, true),
            (&converting, &[Some(0.9), None], 2, false),
            // A synchronized bar that never committed an equity sample.
            (&converting, &[Some(0.9), None], 1, true),
            // A committed bar that was never synchronized.
            (&converting, &[Some(0.9)], 2, false),
        ];
        for (cfg, rates, committed, complete) in cases {
            let mut broker = Broker::new_with_symbol(cfg, 0.01, 1.0, 1.0, "USD");
            for (bar, rate) in rates.iter().enumerate() {
                broker.sync_currency_rate(bar, *rate);
            }
            for bar in 0..committed {
                broker.record_equity(bar, 100.0);
            }
            let ledger = broker.trade_ledger(100.0).expect("finite ledger");
            assert_eq!(ledger.currency_conversion_complete, complete);
            assert_eq!(
                ledger.currency_conversion_complete,
                broker
                    .clone()
                    .into_outcome(100.0)
                    .currency_conversion_complete
            );
        }
    }

    #[test]
    // The same bounded-budget exclusion as the retained-objectives walk above.
    #[cfg_attr(
        miri,
        ignore = "9,001-bar walk exceeds the bounded Miri budget; covered natively and under AddressSanitizer"
    )]
    fn trade_ledger_exposes_a_retention_gap_behind_a_zero_first_index() {
        // `A` opens with the run's first fill and stays open while `B`
        // round-trips beside it, so the first closed trade belongs to `B`.
        // Once 9,001 orders have filled, `A`'s opening fill is the only one
        // outside the retention window: its row is evicted from the middle
        // of the ledger while row 0 survives.
        let cfg = BrokerConfig {
            pyramiding: 2,
            close_entries_rule: "ANY".into(),
            process_orders_on_close: true,
            commission_type: "cash_per_order".into(),
            commission_value: 0.125,
            ..config()
        };
        let market = OrderMetadata::default;
        let mut broker = Broker::new_with_symbol(&cfg, 0.25, 1.0, 1.0, "");
        let drive = |broker: &mut Broker, bar: usize| {
            let price = if bar % 2 == 0 { 100.0 } else { 101.0 };
            script_bar(broker, bar, (price, price, price, price), |b| {
                if bar == 0 {
                    b.place_entry("A".into(), 1.0, Some(1.0), None, None, market());
                } else if bar % 2 == 1 {
                    b.place_entry("B".into(), 1.0, Some(1.0), None, None, market());
                } else {
                    b.place_close(Some("B".into()), None, None, market(), None);
                }
                if bar == 10 {
                    b.place_close(Some("A".into()), None, None, market(), None);
                }
            });
        };
        for bar in 0..9_000 {
            drive(&mut broker, bar);
        }
        assert_eq!(broker.successful_order_fills_total, 9_001);

        let ledger = broker.trade_ledger(101.0).expect("finite ledger");
        let rows: Vec<LedgerTrade<'_>> = ledger
            .trades()
            .collect::<Option<_>>()
            .expect("broker-produced rows");
        assert_eq!(ledger.first_retained_trade_index, 0);
        assert_eq!(ledger.closed_trades, 4_500);
        assert_eq!(rows.len(), 4_499);
        let indices: Vec<u32> = rows.iter().map(|row| row.index).collect();
        assert_eq!(indices[..6], [0, 1, 2, 3, 4, 6]);
        assert!(indices[5..].windows(2).all(|pair| pair[1] == pair[0] + 1));
        assert!(rows.iter().all(|row| row.entry_id == "B"));
        // Full-run totals still include the evicted row; the retained rows
        // alone do not add up to them.
        let last = rows.last().expect("retained rows");
        assert_eq!(
            ledger.net_profit,
            last.cumulative_pnl - ledger.open_entry_commission
        );
        assert_eq!(
            ledger.net_profit + ledger.open_entry_commission
                - rows.iter().map(|row| row.pnl).sum::<f64>(),
            -0.25
        );
        assert_eq!(ledger.position.size, 1.0);

        let outcome = broker.clone().into_outcome(101.0);
        assert_eq!(outcome.first_retained_trade_index, 0);
        assert_eq!(outcome.trades.len(), rows.len());
        assert_eq!(outcome.metrics.closed_trades, ledger.closed_trades);
        assert_eq!(outcome.metrics.net_profit, ledger.net_profit);

        // One more fill moves the window past row 0 as well.
        drive(&mut broker, 9_000);
        let ledger = broker.trade_ledger(100.0).expect("finite ledger");
        assert_eq!(ledger.first_retained_trade_index, 1);
        assert_eq!(ledger.closed_trades, 4_501);
        assert_eq!(ledger.trades().count(), 4_499);
    }

    /// A closed-trade row carrying only what retention reads.
    fn retention_row(
        index: u32,
        entry_order_ordinal: u64,
        exit_order_ordinal: u64,
    ) -> StrategyTrade {
        StrategyTrade {
            index: Some(index),
            entry_id: String::new(),
            entry_bar: 0,
            entry_price: 0.0,
            exit_bar: 0,
            exit_price: 0.0,
            qty: 1.0,
            comment: None,
            pnl: 0.0,
            cumulative_pnl: None,
            entry_notional: None,
            sign: 1.0,
            commission: 0.0,
            max_runup: 0.0,
            max_drawdown: 0.0,
            exit_id: None,
            exit_order_name: None,
            entry_comment: None,
            entry_order_ordinal,
            exit_order_ordinal,
            commission_split: None,
        }
    }

    #[test]
    fn trade_retention_evicts_exactly_what_a_full_rescan_evicts() {
        // Random fills over windows short enough for lots to outlive them, so
        // rows age out from the front, from the middle, and on arrival. Each
        // fill opens a lot or closes one to three, wholly or partly, in any
        // order or oldest first. After every fill the retained rows must be
        // the ones the old full rescan keeps, and closing oldest first must
        // never call for a full pass.
        struct Rng(u64);
        impl Rng {
            fn below(&mut self, n: u64) -> u64 {
                self.0 ^= self.0 >> 12;
                self.0 ^= self.0 << 25;
                self.0 ^= self.0 >> 27;
                self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) % n
            }
        }
        // Miri interprets a short run over small windows, which still evicts
        // from the front and the middle; the native run is long enough for
        // every window to be outlived many times.
        let (fills, max_window) = if cfg!(miri) {
            (96, 8)
        } else {
            (20_000, u64::MAX)
        };
        for (seed, window, oldest_first) in [
            (0x9e37_79b9_7f4a_7c15, 32, false),
            (0x5eed_5eed_5eed_5eed, 7, false),
            (0x0bad_cafe_f00d_d00d, 200, false),
            (0x1234_5678_9abc_def1, 32, true),
        ] {
            let window = window.min(max_window);
            let mut rng = Rng(seed);
            let mut retention = TradeRetention::new();
            let mut trades = VecDeque::new();
            // Every row recorded and not yet rescanned out, as ordinals.
            let mut rescanned: Vec<(u64, u64)> = Vec::new();
            let mut open: Vec<u64> = Vec::new();
            let mut index = 0;
            let mut middle_evictions = 0;
            for ordinal in 0..fills {
                if open.is_empty() || (open.len() < 8 && rng.below(2) == 0) {
                    open.push(ordinal);
                } else {
                    for _ in 0..=rng.below(3).min(open.len() as u64 - 1) {
                        let at = if oldest_first {
                            0
                        } else {
                            rng.below(open.len() as u64) as usize
                        };
                        let entry = if rng.below(4) == 0 {
                            open[at]
                        } else {
                            open.remove(at)
                        };
                        let row = retention_row(index, entry, ordinal);
                        index += 1;
                        retention.record(&row);
                        trades.push_back(row);
                        rescanned.push((entry, ordinal));
                    }
                }
                let filled = ordinal + 1;
                if filled > window {
                    let first = filled - window;
                    retention.prune(&mut trades, first);
                    let mut kept_ahead = false;
                    rescanned.retain(|&(entry, exit)| {
                        let kept = entry >= first && exit >= first;
                        middle_evictions += usize::from(kept_ahead && !kept);
                        kept_ahead |= kept;
                        kept
                    });
                }
                assert!(
                    trades
                        .iter()
                        .map(|row| (row.entry_order_ordinal, row.exit_order_ordinal))
                        .eq(rescanned.iter().copied()),
                    "seed {seed:#x}: fill {ordinal}"
                );
                if oldest_first {
                    assert_eq!(retention.out_of_order_floor, u64::MAX, "fill {ordinal}");
                }
            }
            assert_eq!(oldest_first, middle_evictions == 0, "seed {seed:#x}");
        }
    }

    #[test]
    // The same bounded-budget exclusion as the retention-gap walk above.
    #[cfg_attr(
        miri,
        ignore = "12,500-bar walk exceeds the bounded Miri budget; covered natively and under AddressSanitizer"
    )]
    fn retained_trades_match_a_full_rescan_after_every_bar() {
        // `B` round-trips beside three long-held lots for 12,505 filled
        // orders. `A`, opened by order 0 and closed on bar 4,000, ages out
        // from the middle of the ledger once 9,001 orders have filled. `C`,
        // held from bar 3,000 to 8,000, ages out from the middle some 3,000
        // orders later. `D`, held from bar 2,000 past 9,000 more orders, is
        // already outside the window when it closes on bar 11,500.
        let cfg = BrokerConfig {
            pyramiding: 4,
            close_entries_rule: "ANY".into(),
            process_orders_on_close: true,
            commission_type: "cash_per_order".into(),
            commission_value: 0.125,
            ..config()
        };
        let market = OrderMetadata::default;
        let mut broker = Broker::new_with_symbol(&cfg, 0.25, 1.0, 1.0, "");
        // The old rule over every row the broker has shown, as (index,
        // opening ordinal, closing ordinal).
        let mut rescanned: VecDeque<(u32, u64, u64)> = VecDeque::new();
        let mut next_index = 0;
        let mut never_retained = Vec::new();
        let mut middle_evictions = Vec::new();
        for bar in 0..12_500 {
            let price = if bar % 2 == 0 { 100.0 } else { 101.0 };
            script_bar(&mut broker, bar, (price, price, price, price), |b| {
                if bar % 2 == 1 {
                    b.place_entry("B".into(), 1.0, Some(1.0), None, None, market());
                } else if bar > 0 {
                    b.place_close(Some("B".into()), None, None, market(), None);
                }
                let opening = match bar {
                    0 => Some("A"),
                    2_000 => Some("D"),
                    3_000 => Some("C"),
                    _ => None,
                };
                if let Some(id) = opening {
                    b.place_entry(id.into(), 1.0, Some(1.0), None, None, market());
                }
                let closing = match bar {
                    4_000 => Some("A"),
                    8_000 => Some("C"),
                    11_500 => Some("D"),
                    _ => None,
                };
                if let Some(id) = closing {
                    b.place_close(Some(id.into()), None, None, market(), None);
                }
            });
            // This bar's new rows are at the back; a row pruned on arrival
            // leaves a gap in the indices.
            let new = broker
                .trades
                .iter()
                .rev()
                .take_while(|trade| trade.index >= Some(next_index))
                .count();
            for trade in broker.trades.range(broker.trades.len() - new..) {
                let index = trade.index.expect("broker-produced row");
                never_retained.extend(next_index..index);
                rescanned.push_back((index, trade.entry_order_ordinal, trade.exit_order_ordinal));
                next_index = index + 1;
            }
            let first = broker
                .successful_order_fills_total
                .saturating_sub(MAX_RETAINED_FILLED_ORDERS);
            let mut kept_ahead = false;
            rescanned.retain(|&(index, entry, exit)| {
                let kept = entry >= first && exit >= first;
                if kept_ahead && !kept {
                    middle_evictions.push(index);
                }
                kept_ahead |= kept;
                kept
            });
            assert!(
                broker
                    .trades
                    .iter()
                    .map(|trade| trade.index)
                    .eq(rescanned.iter().map(|&(index, ..)| Some(index))),
                "bar {bar}"
            );
        }
        never_retained.extend(next_index..broker.closed_trades_total);
        assert_eq!(broker.successful_order_fills_total, 12_505);
        // Bars 2 to 4,000 close `B` 2,000 times before `A` closes, bars 4,002
        // to 8,000 2,000 more before `C`, and bars 8,002 to 11,500 another
        // 1,750 before `D`.
        assert_eq!(middle_evictions, [2_000, 4_001]);
        assert_eq!(never_retained, [5_752]);

        let first_retained = rescanned.front().map(|&(index, ..)| index);
        let ledger = broker.trade_ledger(101.0).expect("finite ledger");
        assert_eq!(ledger.closed_trades, 6_252);
        assert_eq!(Some(ledger.first_retained_trade_index), first_retained);
        assert!(ledger
            .trades()
            .map(|row| row.expect("broker-produced row").index)
            .eq(rescanned.iter().map(|&(index, ..)| index)));
        let outcome = broker.into_outcome(101.0);
        assert_eq!(Some(outcome.first_retained_trade_index), first_retained);
        assert!(outcome
            .trades
            .iter()
            .map(|trade| trade.index)
            .eq(rescanned.iter().map(|&(index, ..)| Some(index))));
    }

    /// Drive one bar exactly the way `run_strategy_history_bar` sequences the
    /// broker phases (fills + snapshot per vertex, close pass, equity), so the
    /// walk entry points can be asserted equivalent to the interpreter's
    /// hand-rolled sequence.
    fn drive_bar_vm_style(broker: &mut Broker, bar: usize, o: f64, h: f64, l: f64, c: f64) {
        broker.begin_session(bar as i64, bar, o);
        let high_first = (h - o).abs() <= (o - l).abs();
        let path = if high_first {
            [o, h, l, c]
        } else {
            [o, l, h, c]
        };
        let mut prev = o;
        for price in path {
            broker.process_fills(bar, prev, prev.max(price), prev.min(price), price, false);
            broker.record_snapshot(bar, price, price, price, price);
            prev = price;
        }
        if broker.process_on_close() {
            broker.process_fills(bar, c, c, c, c, true);
        }
        broker.record_equity(bar, c);
    }

    /// Drive one bar through the AOT-facing entry points.
    fn drive_bar_walk(broker: &mut Broker, bar: usize, o: f64, h: f64, l: f64, c: f64) {
        broker.begin_session(bar as i64, bar, o);
        broker
            .process_bar_ticks(bar, o, h, l, c)
            .expect("no caps set");
        broker.finish_bar(bar, c).expect("no caps set");
    }

    fn drive_script_bar(broker: &mut Broker, bar: usize, o: f64, h: f64, l: f64, c: f64) {
        broker.begin_session(bar as i64, bar, o);
        broker.begin_script_execution();
        broker
            .process_bar_ticks(bar, o, h, l, c)
            .expect("no caps set");
        broker.finish_bar(bar, c).expect("no caps set");
    }

    fn pyramided_stop_run(
        sign: f64,
        stop: f64,
        last_bar: (f64, f64, f64, f64),
        competing_stop: bool,
    ) -> f64 {
        let cfg = BrokerConfig {
            margin_long: 0.0,
            margin_short: 0.0,
            pyramiding: 2,
            ..config()
        };
        let mut broker = Broker::new(&cfg, 1.0);

        broker.begin_session(0, 0, 100.0);
        broker.begin_script_execution();
        broker
            .process_bar_ticks(0, 100.0, 100.0, 100.0, 100.0)
            .unwrap();
        broker.place_entry(
            "L".into(),
            sign,
            Some(1.0),
            None,
            None,
            OrderMetadata::default(),
        );
        broker.place_exit(
            "x".into(),
            None,
            None,
            None,
            None,
            None,
            None,
            Some(stop),
            None,
            None,
            None,
            ExitMetadata::default(),
        );
        broker.finish_bar(0, 100.0).unwrap();

        drive_script_bar(&mut broker, 1, 100.0, 101.0, 99.0, 100.0);
        assert_eq!(broker.position_size(), sign);
        broker.place_entry(
            "L".into(),
            sign,
            Some(1.0),
            None,
            None,
            OrderMetadata::default(),
        );
        broker.begin_session(2, 2, 100.0);
        broker.begin_script_execution();
        broker
            .process_bar_ticks(2, 100.0, 101.0, 99.0, 100.0)
            .unwrap();
        assert_eq!(broker.position_size(), sign * 2.0);
        if competing_stop {
            broker.place_order(
                "competing".into(),
                -1.0,
                Some(2.0),
                None,
                Some(90.0),
                OrderMetadata::default(),
            );
        }
        broker.finish_bar(2, 100.0).unwrap();

        drive_script_bar(
            &mut broker,
            3,
            last_bar.0,
            last_bar.1,
            last_bar.2,
            last_bar.3,
        );
        let position_after_stop = broker.position_size();
        assert!(
            broker.persistent_exits.is_empty(),
            "flattening must end the persistent exit lifecycle"
        );
        assert!(
            !broker.pending.iter().any(|order| order.reduce_only),
            "flattening must remove remaining reduce-only children"
        );

        // A later bar must not execute stale exit children or change the
        // post-stop position.
        drive_script_bar(&mut broker, 4, 85.0, 86.0, 84.0, 85.0);
        assert_eq!(broker.position_size(), position_after_stop);
        assert!(!broker.pending.iter().any(|order| order.reduce_only));
        position_after_stop
    }

    #[test]
    fn flat_position_sign_is_zero_for_negative_zero_and_nonzero_signs_are_preserved() {
        let broker = Broker::new(&config(), 1.0);
        let flat = broker.position_size();
        assert_eq!(flat, 0.0);
        assert!(flat.is_sign_negative());
        assert_eq!(flat.signum(), -1.0);
        assert_eq!(broker.position_sign(), 0.0);
        assert_eq!(Broker::position_sign_for(0.0_f64), 0.0);
        assert_eq!(Broker::position_sign_for(-0.0_f64), 0.0);
        assert_eq!(Broker::position_sign_for(1.25), 1.0);
        assert_eq!(Broker::position_sign_for(-1.25), -1.0);
    }

    #[test]
    fn persistent_stop_protects_both_pyramided_lots_after_flat_entry() {
        assert_eq!(
            pyramided_stop_run(1.0, 90.0, (100.0, 100.0, 80.0, 85.0), false),
            0.0,
            "long stop should close both lots"
        );
        assert_eq!(
            pyramided_stop_run(-1.0, 110.0, (100.0, 120.0, 100.0, 115.0), false),
            0.0,
            "short stop should close both lots"
        );
    }

    #[test]
    fn executable_competing_stop_remains_in_the_immutable_fill_batch() {
        assert_eq!(
            pyramided_stop_run(1.0, 90.0, (100.0, 100.0, 80.0, 85.0), true),
            -2.0,
            "the competing sell stop executes after both exit children flatten"
        );
    }

    fn run_persistent_late_exit_cleanup(
        sign: f64,
        first_stop: f64,
        late_stop: f64,
        flatten_sign: f64,
        stop_bar: (f64, f64, f64, f64),
        later_bar: (f64, f64, f64, f64),
    ) {
        let cfg = BrokerConfig {
            margin_long: 0.0,
            margin_short: 0.0,
            pyramiding: 2,
            ..config()
        };
        let mut broker = Broker::new(&cfg, 1.0);
        broker.begin_session(0, 0, 100.0);
        broker.begin_script_execution();
        broker
            .process_bar_ticks(0, 100.0, 100.0, 100.0, 100.0)
            .unwrap();
        broker.place_entry(
            "A".into(),
            sign,
            Some(2.0),
            None,
            None,
            OrderMetadata::default(),
        );
        for (id, stop) in [("first", first_stop), ("late", late_stop)] {
            broker.place_exit(
                id.into(),
                None,
                None,
                Some(50.0),
                None,
                None,
                None,
                Some(stop),
                None,
                None,
                None,
                ExitMetadata::default(),
            );
        }
        broker.finish_bar(0, 100.0).unwrap();

        drive_script_bar(&mut broker, 1, 100.0, 101.0, 99.0, 100.0);
        broker.place_entry(
            "B".into(),
            sign,
            Some(2.0),
            None,
            None,
            OrderMetadata::default(),
        );
        drive_script_bar(&mut broker, 2, 100.0, 101.0, 99.0, 100.0);
        assert_eq!(broker.position_size(), sign * 4.0);
        assert_eq!(
            broker
                .pending
                .iter()
                .filter(|order| order.id == "late")
                .count(),
            2,
            "the untriggered late exit has one child for each lot"
        );
        // Retain an unscoped reduce-only peer as well. Real lot-targeted exit
        // children are also removed by reservation reconciliation once their
        // lots disappear, so this pending-batch peer makes the sign-branch
        // cleanup observable independently of that later reconciliation.
        let mut late_peer = broker
            .pending
            .iter()
            .find(|order| order.id == "late")
            .expect("late template has a child")
            .clone();
        late_peer.id = "late-unscoped".into();
        late_peer.target_entry = None;
        late_peer.target_lot = None;
        broker.pending.push(late_peer);

        // The market order flattens at the open while both stop templates are
        // untriggered. They enter `still_pending`; the flat transition must
        // remove their children before they can survive this fill batch.
        broker.place_order(
            "flatten".into(),
            flatten_sign,
            Some(4.0),
            None,
            None,
            OrderMetadata::default(),
        );
        drive_script_bar(
            &mut broker,
            3,
            stop_bar.0,
            stop_bar.1,
            stop_bar.2,
            stop_bar.3,
        );
        assert_eq!(broker.position_size(), 0.0);
        assert!(broker.persistent_exits.is_empty());
        assert!(
            !broker.pending.iter().any(|order| order.reduce_only),
            "the untriggered reduce-only child must be removed during flatten cleanup"
        );
        assert!(!broker.pending.iter().any(|order| order.id == "late"));
        assert!(!broker
            .pending
            .iter()
            .any(|order| order.id == "late-unscoped"));

        // The old late stop would trigger on this bar if it had survived.
        drive_script_bar(
            &mut broker,
            4,
            later_bar.0,
            later_bar.1,
            later_bar.2,
            later_bar.3,
        );
        assert_eq!(broker.position_size(), 0.0);
        assert!(!broker.pending.iter().any(|order| order.id == "late"));
        assert!(!broker
            .pending
            .iter()
            .any(|order| order.id == "late-unscoped"));
    }

    #[test]
    fn flatten_removes_untriggered_persistent_exit_children_from_pending_batch() {
        run_persistent_late_exit_cleanup(
            1.0,
            90.0,
            70.0,
            -1.0,
            (100.0, 110.0, 100.0, 105.0),
            (80.0, 80.0, 60.0, 75.0),
        );
    }

    #[test]
    fn short_flatten_removes_untriggered_persistent_exit_children() {
        run_persistent_late_exit_cleanup(
            -1.0,
            110.0,
            130.0,
            1.0,
            (100.0, 109.0, 100.0, 105.0),
            (120.0, 140.0, 120.0, 135.0),
        );
    }

    #[test]
    fn executed_order_commissions_use_ten_significant_digits() {
        let cfg = BrokerConfig {
            initial_capital: 100_000.0,
            process_orders_on_close: true,
            commission_type: "percent".into(),
            commission_value: 0.075,
            ..config()
        };
        let mut broker = Broker::new_with_symbol(&cfg, 0.00001, 1.0, 0.0001, "USD");
        drive_bar_walk(&mut broker, 0, 12.3456, 12.3456, 12.3456, 12.3456);
        broker.place_entry(
            "L".into(),
            1.0,
            Some(1234.5678),
            None,
            None,
            OrderMetadata::default(),
        );
        broker.finish_bar(0, 12.3456).unwrap();
        broker.place_close(
            Some("L".into()),
            None,
            None,
            OrderMetadata::default(),
            Some((1, 13.5791)),
        );
        assert_eq!(broker.trades.len(), 1);
        // Independently rounded decimal products qty * price * 0.00075.
        assert!((broker.trades[0].commission - (11.43111017 + 12.57323971)).abs() < 1e-11);
    }

    #[test]
    fn script_account_charges_entry_fees_once_through_partial_and_full_closes() {
        let cfg = BrokerConfig {
            initial_capital: 1000.0,
            process_orders_on_close: true,
            commission_type: "percent".into(),
            commission_value: 1.0,
            ..config()
        };
        let mut broker = Broker::new_with_symbol(&cfg, 1.0, 1.0, 0.5, "USD");
        drive_bar_walk(&mut broker, 0, 100.0, 100.0, 100.0, 100.0);
        broker.place_entry(
            "L".into(),
            1.0,
            Some(1.0),
            None,
            None,
            OrderMetadata::default(),
        );
        broker.finish_bar(0, 100.0).unwrap();
        broker.record_snapshot(1, 110.0, 110.0, 110.0, 110.0);
        let before = broker.snapshot_at(1).unwrap();
        assert_eq!(before.net_profit, -1.0);
        assert_eq!(before.open_profit, 10.0);
        assert_eq!(before.equity, 1009.0);
        assert_eq!(before.net_profit_percent, -0.1);
        assert!((before.open_profit_percent - 1000.0 / 999.0).abs() < 1e-12);
        broker.place_close(
            Some("L".into()),
            Some(0.5),
            None,
            OrderMetadata::default(),
            Some((1, 110.0)),
        );
        broker.record_snapshot(1, 110.0, 110.0, 110.0, 110.0);
        let partial = broker.snapshot_at(1).unwrap();
        assert!((partial.net_profit - 3.45).abs() < 1e-10);
        assert_eq!(partial.open_profit, 5.0);
        assert!((partial.equity - 1008.45).abs() < 1e-10);
        broker.place_close(
            Some("L".into()),
            None,
            None,
            OrderMetadata::default(),
            Some((2, 120.0)),
        );
        broker.record_snapshot(2, 120.0, 120.0, 120.0, 120.0);
        let full = broker.snapshot_at(2).unwrap();
        assert!((full.net_profit - 12.85).abs() < 1e-10);
        assert_eq!(full.open_profit, 0.0);
        assert!((full.equity - 1012.85).abs() < 1e-10);
    }

    #[test]
    fn account_rounding_uses_the_computed_decimal_half() {
        // The scaled product is a half even if its binary input is just below.
        assert_eq!(Broker::round_account_value(12.345678905), 12.34567891);
        assert_eq!(Broker::round_account_value(12345.678906), 12345.67891);
        assert_eq!(Broker::round_account_value(0.0), 0.0);
        assert!(Broker::round_account_value(f64::NAN).is_nan());
    }

    #[test]
    fn margin_comparison_uses_significant_digits_at_different_account_scales() {
        for scale in [0.1, 1.0, 10000.0] {
            let cfg = BrokerConfig {
                initial_capital: 100.00000001 * scale,
                process_orders_on_close: true,
                ..config()
            };
            let mut broker = Broker::new_with_symbol(&cfg, 0.00000001, 1.0, 0.01, "USD");
            drive_bar_walk(&mut broker, 0, 100.0, 100.0, 100.0, 100.0);
            broker.place_entry(
                "L".into(),
                1.0,
                Some(scale),
                None,
                None,
                OrderMetadata::default(),
            );
            broker.finish_bar(0, 100.0).unwrap();
            assert!(broker.trades.is_empty());
            broker.record_snapshot(1, 103.12345676, 103.12345676, 103.12345676, 103.12345676);
            assert_eq!(broker.trades.len(), 1, "scale {scale}");
            assert_eq!(broker.trades[0].qty, scale.min(1.0));
        }
    }

    #[test]
    fn percent_sizing_and_margin_agree_at_a_multimillion_account_scale() {
        // Four fixed decimal places size one increment too low and suppress
        // the margin call.
        let cfg = BrokerConfig {
            initial_capital: 2_718_497.28478718,
            process_orders_on_close: true,
            default_qty_type: "percent_of_equity".into(),
            default_qty_value: 100.0,
            commission_type: "percent".into(),
            commission_value: 0.05,
            ..config()
        };
        let mut broker = Broker::new_with_symbol(&cfg, 0.01, 1.0, 0.000001, "USD");
        script_bar(&mut broker, 0, (5413.0, 5413.0, 5413.0, 5413.0), |b| {
            b.place_entry("L".into(), 1.0, None, None, None, OrderMetadata::default());
        });
        assert_eq!(broker.position_size(), 501.965401);
        assert!(broker.trades.is_empty());
        script_bar(&mut broker, 1, (5413.0, 5452.82, 5220.12, 5300.0), |_| {});
        assert_eq!(broker.trades.len(), 1);
        assert_eq!(broker.trades[0].exit_id.as_deref(), Some("Margin Call"));
        assert_eq!(broker.trades[0].qty, 1.0);
        assert_eq!(broker.trades[0].exit_price, 5413.0);
        assert!((broker.trades[0].pnl + 5.413).abs() < 1e-8);
    }

    #[test]
    fn margin_halfway_rounding_liquidates_at_the_matching_price_vertex() {
        // At this close the required margin is 1,000,028.7875.
        // A residual-based half correction delays this to a later bar high.
        let cfg = BrokerConfig {
            initial_capital: 292_138.40211647,
            process_orders_on_close: true,
            commission_type: "percent".into(),
            commission_value: 0.05,
            ..config()
        };
        let mut broker = Broker::new_with_symbol(&cfg, 0.01, 1.0, 0.000001, "USD");
        script_bar(&mut broker, 0, (912.45, 912.45, 912.45, 912.45), |b| {
            b.place_entry(
                "L".into(),
                1.0,
                Some(320.009212),
                None,
                None,
                OrderMetadata::default(),
            );
        });
        script_bar(&mut broker, 1, (3100.0, 3125.0, 3100.0, 3125.0), |_| {});
        assert_eq!(broker.trades.len(), 1);
        assert_eq!(broker.trades[0].exit_id.as_deref(), Some("Margin Call"));
        assert_eq!(broker.trades[0].qty, 1.0);
        assert_eq!(broker.trades[0].exit_price, 3125.0);
    }

    #[test]
    fn close_margin_deficit_keeps_its_quantity_until_the_next_open() {
        let cfg = BrokerConfig {
            initial_capital: 1000.0,
            process_orders_on_close: true,
            commission_type: "percent".into(),
            commission_value: 1.0,
            calc_on_order_fills: true,
            ..config()
        };
        let mut broker = Broker::new_with_symbol(&cfg, 1.0, 1.0, 0.01, "USD");
        drive_bar_walk(&mut broker, 0, 100.0, 100.0, 100.0, 100.0);
        broker.place_entry(
            "L".into(),
            1.0,
            Some(10.0),
            None,
            None,
            OrderMetadata::default(),
        );
        broker.finish_bar(0, 100.0).unwrap();
        assert!(
            broker.trades.is_empty(),
            "no available tick remains after the close fill"
        );
        broker.begin_session(1, 1, 110.0);
        assert_eq!(
            broker
                .process_bar_ticks(1, 110.0, 110.0, 110.0, 110.0)
                .unwrap(),
            1
        );
        assert_eq!(broker.trades.len(), 1);
        assert_eq!(broker.trades[0].qty, 0.4);
        assert_eq!(broker.trades[0].exit_price, 110.0);
        assert_eq!(broker.margin_calls, 1);
        assert_eq!(
            broker.take_immediate_fill_count(),
            0,
            "one market fill must not schedule a duplicate immediate recalculation"
        );
    }

    #[test]
    fn percent_quantity_uses_rounded_equity_and_the_entry_commission() {
        // Ten significant digits retain sub-cent equity at this account scale.
        for (capital, expected) in [(1000.000049, 330.033019), (999.999951, 330.032987)] {
            let cfg = BrokerConfig {
                initial_capital: capital,
                default_qty_type: "percent_of_equity".into(),
                default_qty_value: 100.0,
                commission_type: "percent".into(),
                commission_value: 1.0,
                ..config()
            };
            let broker = Broker::new_with_symbol(&cfg, 0.01, 1.0, 0.000001, "USD");
            // floor((capital / (3 + 3 * 0.01)) / 0.000001) * 0.000001.
            assert!((broker.default_entry_qty(3.0) - expected).abs() < 1e-10);
        }
    }

    #[test]
    fn percent_quantity_freezes_at_placement_but_cash_waits_for_fill() {
        for (kind, value, expected) in [("percent_of_equity", 10.0, 10.0), ("cash", 100.0, 5.0)] {
            let cfg = BrokerConfig {
                initial_capital: 1000.0,
                default_qty_type: kind.into(),
                default_qty_value: value,
                ..config()
            };
            let mut broker = Broker::new_with_symbol(&cfg, 1.0, 1.0, 1.0, "USD");
            drive_bar_walk(&mut broker, 0, 10.0, 10.0, 10.0, 10.0);
            broker.place_entry("L".into(), 1.0, None, None, None, OrderMetadata::default());
            drive_bar_walk(&mut broker, 1, 20.0, 20.0, 20.0, 20.0);
            assert_eq!(broker.position_size(), expected, "{kind}");
        }
    }

    #[test]
    fn resting_exit_prices_round_away_from_the_placement_price() {
        let cfg = BrokerConfig {
            initial_capital: 1000.0,
            process_orders_on_close: true,
            ..config()
        };
        let mut broker = Broker::new_with_symbol(&cfg, 0.25, 1.0, 1.0, "USD");
        drive_bar_walk(&mut broker, 0, 100.0, 100.0, 100.0, 100.0);
        broker.place_entry(
            "L".into(),
            1.0,
            Some(1.0),
            None,
            None,
            OrderMetadata::default(),
        );
        broker.finish_bar(0, 100.0).unwrap();
        broker.place_exit(
            "X".into(),
            Some("L".into()),
            None,
            None,
            None,
            Some(100.10),
            None,
            Some(99.90),
            None,
            None,
            None,
            ExitMetadata::default(),
        );
        drive_bar_walk(&mut broker, 1, 100.0, 100.0, 100.0, 100.0);
        assert_eq!(broker.position_size(), 1.0);
        drive_bar_walk(&mut broker, 2, 100.0, 100.25, 100.0, 100.25);
        assert_eq!(broker.position_size(), 0.0);
        assert_eq!(broker.trades[0].exit_price, 100.25);
    }

    #[test]
    fn complete_close_removes_roundoff_from_summing_multiple_lots() {
        for sign in [-1.0, 1.0] {
            let cfg = BrokerConfig {
                initial_capital: 1_000_000.0,
                process_orders_on_close: true,
                pyramiding: 2,
                ..config()
            };
            let mut broker = Broker::new_with_symbol(&cfg, 1.0, 1.0, 0.000001, "USD");
            drive_bar_walk(&mut broker, 0, 1.0, 1.0, 1.0, 1.0);
            for (id, qty) in [("A", 52047.324538), ("B", 22348.317134)] {
                broker.place_entry(
                    id.into(),
                    sign,
                    Some(qty),
                    None,
                    None,
                    OrderMetadata::default(),
                );
            }
            broker.finish_bar(0, 1.0).unwrap();
            assert_eq!(broker.lots.len(), 2);
            broker.place_close(None, None, None, OrderMetadata::default(), Some((1, 1.0)));
            assert_eq!(broker.position_size(), 0.0, "sign {sign}");
            assert_eq!(broker.trades.len(), 2);
        }
    }

    #[test]
    fn full_close_consumes_binary_remainders_after_partial_fills() {
        // Synthetic decimal quantities exercise subtraction around contract steps.
        for (initial, partial) in [(2585.750213, 2311.523603), (17680.104198, 13398.09084)] {
            let cfg = BrokerConfig {
                initial_capital: 1_000_000.0,
                process_orders_on_close: true,
                ..config()
            };
            let mut broker = Broker::new_with_symbol(&cfg, 1.0, 1.0, 0.000001, "USD");
            broker.begin_session(0, 0, 5.0);
            broker.process_bar_ticks(0, 5.0, 5.0, 5.0, 5.0).unwrap();
            broker.place_entry(
                "L".into(),
                1.0,
                Some(initial),
                None,
                None,
                OrderMetadata::default(),
            );
            broker.finish_bar(0, 5.0).unwrap();
            broker.place_close(
                Some("L".into()),
                Some(partial),
                None,
                OrderMetadata::default(),
                Some((0, 5.0)),
            );
            assert!(broker.position_size() > 0.0);
            broker.place_close(
                Some("L".into()),
                None,
                None,
                OrderMetadata::default(),
                Some((1, 5.0)),
            );
            assert_eq!(broker.position_size(), 0.0);
            assert_eq!(broker.posture(), 0);
            broker.place_entry(
                "L".into(),
                1.0,
                Some(100.0),
                None,
                None,
                OrderMetadata::default(),
            );
            broker.finish_bar(2, 5.0).unwrap();
            assert_eq!(broker.position_size(), 100.0);
        }
    }

    #[test]
    fn full_exit_reserves_the_exact_remainder_after_two_partial_fills() {
        let cfg = BrokerConfig {
            initial_capital: 1_000_000.0,
            process_orders_on_close: true,
            ..config()
        };
        let mut broker = Broker::new_with_symbol(&cfg, 1.0, 1.0, 0.000001, "USD");
        broker.begin_session(0, 0, 100.0);
        broker
            .process_bar_ticks(0, 100.0, 100.0, 100.0, 100.0)
            .unwrap();
        broker.place_entry(
            "S".into(),
            -1.0,
            Some(17.651292),
            None,
            None,
            OrderMetadata::default(),
        );
        broker.finish_bar(0, 100.0).unwrap();
        for qty in [12.310361, 4.465170] {
            broker.place_close(
                Some("S".into()),
                Some(qty),
                None,
                OrderMetadata::default(),
                Some((0, 100.0)),
            );
        }
        broker.place_exit(
            "exit".into(),
            Some("S".into()),
            None,
            None,
            None,
            Some(95.0),
            None,
            None,
            None,
            None,
            None,
            ExitMetadata::default(),
        );
        broker
            .process_bar_ticks(1, 99.0, 100.0, 94.0, 95.0)
            .unwrap();
        assert_eq!(broker.position_size(), 0.0);
        assert_eq!(broker.trades.len(), 3);
    }

    #[test]
    fn partial_close_keeps_a_real_contract_on_a_large_position() {
        // A position-scaled dust tolerance would incorrectly erase this unit.
        let cfg = BrokerConfig {
            initial_capital: 1e17,
            process_orders_on_close: true,
            ..config()
        };
        let mut broker = Broker::new_with_symbol(&cfg, 1.0, 1.0, 1.0, "USD");
        broker.begin_session(0, 0, 1.0);
        broker.process_bar_ticks(0, 1.0, 1.0, 1.0, 1.0).unwrap();
        broker.place_entry(
            "L".into(),
            1.0,
            Some(1e15),
            None,
            None,
            OrderMetadata::default(),
        );
        broker.finish_bar(0, 1.0).unwrap();
        assert_eq!(broker.position_size(), 1e15);
        broker.place_close(
            Some("L".into()),
            Some(1e15 - 1.0),
            None,
            OrderMetadata::default(),
            Some((1, 1.0)),
        );
        assert_eq!(broker.position_size(), 1.0);
    }

    #[test]
    fn flat_price_vertices_still_fill_orders_and_charge_each_side() {
        let cfg = BrokerConfig {
            initial_capital: 1000.0,
            commission_type: "percent".into(),
            commission_value: 1.0,
            margin_long: 100.0,
            ..config()
        };
        let mut broker = Broker::new_with_symbol(&cfg, 0.01, 1.0, 1.0, "USD");
        broker.begin_session(0, 0, 10.004);
        assert_eq!(
            broker
                .process_bar_ticks(0, 10.004, 10.004, 10.004, 10.004)
                .unwrap(),
            0
        );
        broker.place_entry(
            "L".into(),
            1.0,
            Some(2.0),
            None,
            None,
            OrderMetadata::default(),
        );
        broker.finish_bar(0, 10.004).unwrap();

        broker.begin_session(1, 1, 10.004);
        assert_eq!(
            broker
                .process_bar_ticks(1, 10.004, 10.004, 10.004, 10.004)
                .unwrap(),
            1
        );
        let snapshot = broker.snapshot_at(1).unwrap();
        assert_eq!(snapshot.position_size, 2.0);
        assert_eq!(snapshot.position_avg_price, 10.0);
        assert_eq!(snapshot.net_profit, -0.2);
        assert_eq!(snapshot.equity, 999.8);
        broker.place_close(Some("L".into()), None, None, OrderMetadata::default(), None);
        broker.finish_bar(1, 10.004).unwrap();

        broker.begin_session(2, 2, 10.004);
        assert_eq!(
            broker
                .process_bar_ticks(2, 10.004, 10.004, 10.004, 10.004)
                .unwrap(),
            1
        );
        broker.finish_bar(2, 10.004).unwrap();
        let outcome = broker.into_outcome(10.0);
        assert_eq!(outcome.trades.len(), 1);
        assert_eq!(outcome.trades[0].entry_price, 10.0);
        assert_eq!(outcome.trades[0].exit_price, 10.0);
        assert_eq!(outcome.trades[0].commission, 0.4);
        assert_eq!(outcome.position.size, 0.0);
        assert_eq!(outcome.maximum_margin_used, 20.0);
        assert_eq!(outcome.equity, [1000.0, 999.8, 999.6]);
    }

    #[test]
    fn resting_limit_fills_intrabar_at_trigger_price() {
        // Buy limit 95 on a 100 -> 106 -> 94 -> 104 bar: the second segment
        // (106 -> 94) crosses the limit, so the fill is at the trigger price,
        // not the bar low and not the close.
        let mut broker = Broker::new(&config(), 0.01);
        broker.begin_session(0, 0, 100.0);
        broker
            .process_bar_ticks(0, 100.0, 101.0, 99.0, 100.0)
            .unwrap();
        broker.place_entry(
            "L".to_string(),
            1.0,
            None,
            Some(95.0),
            None,
            OrderMetadata::default(),
        );
        broker.finish_bar(0, 100.0).unwrap();
        assert_eq!(broker.posture(), 0);

        broker.begin_session(1, 1, 100.0);
        let fills = broker
            .process_bar_ticks(1, 100.0, 106.0, 94.0, 104.0)
            .unwrap();
        assert_eq!(fills, 1);
        assert_eq!(broker.posture(), 1);
        assert_eq!(broker.position_size(), 1.0);
        assert_eq!(broker.avg_price(), 95.0);
    }

    #[test]
    fn bar_walk_matches_interpreter_sequencing() {
        // Two brokers, identical orders: one driven through the AOT entry
        // points, one through the interpreter's hand-rolled phase sequence.
        // Their outcomes must be identical field for field.
        let bars: [(f64, f64, f64, f64); 4] = [
            (100.0, 101.0, 99.0, 100.5),
            (100.5, 107.0, 95.0, 103.0),
            (103.0, 104.0, 96.0, 97.0),
            (97.0, 99.5, 92.0, 98.0),
        ];
        let mut walk = Broker::new(&config(), 0.01);
        let mut vm = Broker::new(&config(), 0.01);
        for (bar, (o, h, l, c)) in bars.iter().copied().enumerate() {
            drive_bar_walk(&mut walk, bar, o, h, l, c);
            drive_bar_vm_style(&mut vm, bar, o, h, l, c);
            // Compare every script-visible field, including NaN and strings.
            assert_eq!(
                alloc::format!("{:?}", walk.snapshot_at(bar)),
                alloc::format!("{:?}", vm.snapshot_at(bar))
            );
            // Same script on both: bar 0 places a stop entry, bar 2 a
            // stop-loss exit (fills on bar 3's walk at its trigger).
            if bar == 0 {
                for broker in [&mut walk, &mut vm] {
                    broker.begin_script_execution();
                    broker.place_entry(
                        "e".to_string(),
                        1.0,
                        Some(2.0),
                        None,
                        Some(105.0),
                        OrderMetadata::default(),
                    );
                }
            }
            if bar == 2 {
                for broker in [&mut walk, &mut vm] {
                    broker.begin_script_execution();
                    broker.place_exit(
                        "x".to_string(),
                        Some("e".to_string()),
                        None,
                        None,
                        None,
                        None,
                        None,
                        Some(93.0),
                        None,
                        None,
                        None,
                        ExitMetadata::default(),
                    );
                }
            }
        }
        assert_eq!(walk.posture(), vm.posture());
        let walk_outcome = walk.into_outcome(98.0);
        let vm_outcome = vm.into_outcome(98.0);
        assert_eq!(walk_outcome.equity, vm_outcome.equity);
        assert_eq!(walk_outcome.trades, vm_outcome.trades);
        assert_eq!(walk_outcome.position, vm_outcome.position);
        assert_eq!(walk_outcome.margin_used, vm_outcome.margin_used);
        // The stop entry filled at its trigger and the stop exit at its own.
        assert_eq!(walk_outcome.trades.len(), 1);
        assert_eq!(walk_outcome.trades[0].entry_price, 105.0);
        assert_eq!(walk_outcome.trades[0].exit_price, 93.0);
    }

    #[test]
    fn finish_bar_runs_the_close_pass_only_under_process_orders_on_close() {
        let mut on_close = config();
        on_close.process_orders_on_close = true;
        let mut broker = Broker::new(&on_close, 0.01);
        broker.begin_session(0, 0, 100.0);
        broker
            .process_bar_ticks(0, 100.0, 102.0, 99.0, 101.0)
            .unwrap();
        broker.begin_script_execution();
        broker.place_entry(
            "e".to_string(),
            1.0,
            None,
            None,
            None,
            OrderMetadata::default(),
        );
        // Same-bar close fill at the close price.
        assert_eq!(broker.finish_bar(0, 101.0).unwrap(), 1);
        assert_eq!(broker.posture(), 1);
        assert_eq!(broker.avg_price(), 101.0);

        let mut broker = Broker::new(&config(), 0.01);
        broker.begin_session(0, 0, 100.0);
        broker
            .process_bar_ticks(0, 100.0, 102.0, 99.0, 101.0)
            .unwrap();
        broker.begin_script_execution();
        broker.place_entry(
            "e".to_string(),
            1.0,
            None,
            None,
            None,
            OrderMetadata::default(),
        );
        // Default eligibility: nothing fills until the next bar's open tick.
        assert_eq!(broker.finish_bar(0, 101.0).unwrap(), 0);
        assert_eq!(broker.posture(), 0);
        broker.begin_session(1, 1, 103.0);
        assert_eq!(
            broker
                .process_bar_ticks(1, 103.0, 104.0, 102.0, 103.5)
                .unwrap(),
            1
        );
        assert_eq!(broker.avg_price(), 103.0);
    }

    #[test]
    fn cap_breach_surfaces_as_error_not_panic() {
        let mut broker = Broker::new(&config(), 0.01);
        broker.set_limits(BrokerLimits {
            max_pending_orders: 1,
            ..BrokerLimits::default()
        });
        broker.begin_session(0, 0, 100.0);
        broker
            .process_bar_ticks(0, 100.0, 101.0, 99.0, 100.0)
            .unwrap();
        broker.begin_script_execution();
        for id in ["a", "b"] {
            broker.place_entry(
                id.to_string(),
                1.0,
                None,
                Some(90.0),
                None,
                OrderMetadata::default(),
            );
        }
        assert_eq!(
            broker.finish_bar(0, 100.0),
            Err(BrokerError::CapExceeded {
                cap: "pending orders",
                limit: 1,
                actual: 2
            })
        );
    }

    #[test]
    fn posture_tracks_position_sign_through_a_reversal() {
        let mut broker = Broker::new(&config(), 0.01);
        drive_bar_walk(&mut broker, 0, 100.0, 101.0, 99.0, 100.0);
        assert_eq!(broker.posture(), 0);
        broker.begin_script_execution();
        broker.place_entry(
            "long".to_string(),
            1.0,
            None,
            None,
            None,
            OrderMetadata::default(),
        );
        drive_bar_walk(&mut broker, 1, 100.0, 102.0, 99.0, 101.0);
        assert_eq!(broker.posture(), 1);
        broker.begin_script_execution();
        broker.place_entry(
            "short".to_string(),
            -1.0,
            None,
            None,
            None,
            OrderMetadata::default(),
        );
        drive_bar_walk(&mut broker, 2, 101.0, 103.0, 100.0, 102.0);
        assert_eq!(broker.posture(), -1);
    }

    #[test]
    fn successful_order_ordinals_advance_once_for_multi_lot_and_reversal_fills() {
        let mut config = config();
        config.close_entries_rule = "ANY".to_string();

        // One closing PendingOrder can produce several closed-trade rows. Every
        // row must share that order's one ordinal rather than advancing per lot.
        let mut multi_lot = Broker::new_with_symbol(&config, 0.01, 1.0, 1.0, "");
        for id in ["A", "B"] {
            multi_lot.place_order(
                id.to_string(),
                1.0,
                Some(1.0),
                None,
                None,
                OrderMetadata::default(),
            );
            assert_eq!(multi_lot.process_fills(0, 10.0, 10.0, 10.0, 10.0, false), 1);
        }
        multi_lot.place_close(None, None, None, OrderMetadata::default(), None);
        assert_eq!(multi_lot.process_fills(1, 12.0, 12.0, 12.0, 12.0, false), 1);
        let multi_lot = multi_lot.into_outcome(12.0);
        assert_eq!(multi_lot.trades.len(), 2);
        assert_eq!(multi_lot.trades[0].entry_order_ordinal, 0);
        assert_eq!(multi_lot.trades[1].entry_order_ordinal, 1);
        assert!(multi_lot
            .trades
            .iter()
            .all(|trade| trade.exit_order_ordinal == 2));

        // A reversing entry's closing and opening legs likewise share one
        // ordinal: the second trade's entry is the first trade's exit.
        let mut reversal = Broker::new_with_symbol(&config, 0.01, 1.0, 1.0, "");
        reversal.place_entry(
            "L".to_string(),
            1.0,
            Some(1.0),
            None,
            None,
            OrderMetadata::default(),
        );
        assert_eq!(reversal.process_fills(0, 10.0, 10.0, 10.0, 10.0, false), 1);
        reversal.place_entry(
            "R".to_string(),
            -1.0,
            Some(1.0),
            None,
            None,
            OrderMetadata::default(),
        );
        assert_eq!(reversal.process_fills(1, 11.0, 11.0, 11.0, 11.0, false), 1);
        reversal.place_close(None, None, None, OrderMetadata::default(), None);
        assert_eq!(reversal.process_fills(2, 9.0, 9.0, 9.0, 9.0, false), 1);
        let reversal = reversal.into_outcome(9.0);
        assert_eq!(reversal.trades.len(), 2);
        assert_eq!(reversal.trades[0].entry_order_ordinal, 0);
        assert_eq!(reversal.trades[0].exit_order_ordinal, 1);
        assert_eq!(reversal.trades[1].entry_order_ordinal, 1);
        assert_eq!(reversal.trades[1].exit_order_ordinal, 2);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn strategy_trade_wire_shape_is_camel_case_without_ordinals() {
        let trade = StrategyTrade {
            index: Some(3),
            entry_id: "e".to_string(),
            entry_bar: 1,
            entry_price: 100.0,
            exit_bar: 2,
            exit_price: 101.0,
            qty: 1.0,
            comment: None,
            pnl: 1.0,
            cumulative_pnl: Some(1.0),
            entry_notional: Some(100.0),
            sign: 1.0,
            commission: 0.0,
            max_runup: 1.0,
            max_drawdown: 0.0,
            exit_id: None,
            exit_order_name: Some("Close position order".to_string()),
            entry_comment: None,
            entry_order_ordinal: 7,
            exit_order_ordinal: 8,
            commission_split: Some(CommissionSplit {
                entry: 0.25,
                exit: 0.5,
            }),
        };
        let json = serde_json::to_string(&trade).expect("serializes");
        // The whole row, so a broker-internal field can never reach the wire.
        assert_eq!(
            json,
            concat!(
                r#"{"index":3,"entryId":"e","entryBar":1,"entryPrice":100.0,"exitBar":2,"#,
                r#""exitPrice":101.0,"qty":1.0,"pnl":1.0,"cumulativePnl":1.0,"#,
                r#""entryNotional":100.0,"sign":1.0,"commission":0.0,"maxRunup":1.0,"#,
                r#""maxDrawdown":0.0,"exitOrderName":"Close position order"}"#
            )
        );
        assert!(json.contains("\"entryId\""));
        assert!(json.contains("\"entryBar\""));
        assert!(json.contains("\"cumulativePnl\""));
        assert!(!json.contains("ordinal"));
        assert!(!json.contains("entry_id"));
        // A decoded row carries no broker-internal state, so it can never
        // enter the compact ledger as if this broker had produced it.
        let decoded: StrategyTrade = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(decoded.commission_split, None);
        assert_eq!(decoded.entry_order_ordinal, 0);
        assert!(LedgerTrade::from_trade(&decoded).is_none());
        assert!(LedgerTrade::from_trade(&trade).is_some());
    }
}
