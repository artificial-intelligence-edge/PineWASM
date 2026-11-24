//! The Pine Script® v6 strategy broker, shared by the interpreter and every
//! emitted strategy artifact.
//!
//! # Why this is its own crate
//!
//! Native strategy lowering needs the broker *inside* the emitted module, and
//! a second implementation compiled into artifacts would drift from the
//! interpreter one fill at a time, and the drift would surface as differing
//! results between the interpreter and the artifact. So the broker moved
//! here — the exact argument that moved the `ta.*` engine into `pinewasm_ta`.
//! Keeping the broker in this shared crate ensures both sides call it:
//!
//! * `pinewasm_vm` drives it from the interpreter bar loop and its 4-tick
//!   historical decomposition (`run_strategy_history_bar`).
//! * `pinewasm_aot`'s runtime blob compiles it to WebAssembly behind a
//!   `broker` export family, driving the same state machine per emitted bar.
//!
//! The crate uses `core`/`alloc` types and routes `floor`, `round`, `ceil`, and
//! `sqrt` through `libm`, allowing the same implementation to run in the
//! native VM and the `no_std` WebAssembly runtime.
//!
//! # `no_std`, panics, and allocation
//!
//! The crate is `no_std + alloc`. Program-input failures are handled by
//! dropping or clamping out-of-range and nonsensical orders (PineWASM's input
//! policy), using benign values for internal invariant fallbacks, and
//! returning [`BrokerError`] from the bar-walk entry points when optional caps
//! are exceeded. A panic is a defect rather than a supported outcome. A
//! reachable panic in the artifact build would drag `core::fmt` vtables and an
//! element section into every module, which the runtime-blob splice rejects.
//!
//! Allocation is deterministic and input-bounded: the order book, open-lot
//! ledger, and exit recipes grow only as far as the script's own placements
//! (see [`BrokerLimits`] for the caps an embedding can impose), the
//! closed-trade deque is pruned to PineWASM's 9,000-successful-order retention
//! window, and the per-bar lanes (`equity`, `margin_used`, snapshots,
//! FX rates) are `O(bars)`. Identical inputs produce identical state and
//! identical outputs — there is no clock, no randomness, and no
//! host-dependent float path.
//!
//! # The per-bar protocol (AOT tier)
//!
//! The interpreter interleaves script execution with the fill walk to realize
//! `calc_on_order_fills` and `calc_on_every_history_tick`. The AOT tier
//! compiles neither (they are structured refusals), so its per-bar sequence
//! collapses to:
//!
//! ```text
//! broker.begin_session(key, bar, open);          // risk-session bookkeeping
//! broker.process_bar_ticks(bar, o, h, l, c)?;    // pre-script 4-tick fill walk
//! /* script body executes once, placing orders via place_* */
//! broker.finish_bar(bar, close)?;                // process_orders_on_close pass
//!                                                // + the bar's equity sample
//! let lane = broker.posture();                   // sign(position) ∈ {1, 0, -1}
//! ```
//!
//! [`Broker::process_bar_ticks`] follows `run_strategy_history_bar`'s
//! broker phases: the bar decomposes into the four ordered OHLC
//! vertices (high first iff `|high-open| <= |open-low|`), each adjacent pair
//! forms a fill segment whose open is the previous vertex (so crossed
//! stop/limit orders fill at their trigger price, not the extreme), and the
//! per-bar snapshot marks each vertex so trade excursions accumulate the
//! intrabar path. [`Broker::finish_bar`] runs the close-tick pass when
//! `process_orders_on_close` is set — the same `market_at_close` call the VM
//! makes after the script body — then records the bar's equity sample.
//!
//! # Order placement mirrors `CsilInstr::Order`
//!
//! `strategy.*` order calls are a dedicated CSIL opcode with fixed positional
//! operand layouts (`OrderKind::param_names` in `pinewasm_hir`). The
//! placement surface here mirrors those layouts one-to-one so both tiers
//! decode the same slots into the same call:
//!
//! | `OrderKind`                 | broker call |
//! |-----------------------------|-------------|
//! | `Entry` / `Order`           | [`Broker::place_entry`] / [`Broker::place_order`] — `(id, sign, qty, limit, stop)` + [`OrderMetadata`] (`oca_name`, `oca_type`, `comment`, `alert_message`, `disable_alert`) |
//! | `Exit`                      | [`Broker::place_exit`] — `(id, from_entry, qty, qty_percent, profit, limit, loss, stop, trail_price, trail_points, trail_offset)` + [`ExitMetadata`] |
//! | `Close` / `CloseAll`        | [`Broker::place_close`] (`CloseAll` passes `id = None`) |
//! | `Cancel` / `CancelAll`      | [`Broker::cancel`] / [`Broker::cancel_all`] |
//! | `RiskMaxPositionSize`       | [`Broker::set_max_position_size_at_site`] |
//! | `RiskAllowEntryIn`          | [`Broker::set_allow_entry_in_at_site`] |
//! | `RiskMaxDrawdown`           | [`Broker::set_max_drawdown_at_site`] |
//! | `RiskMaxIntradayLoss`       | [`Broker::set_max_intraday_loss_at_site`] |
//! | `RiskMaxIntradayFilledOrders` | [`Broker::set_max_intraday_filled_orders_at_site`] |
//! | `RiskMaxConsLossDays`       | [`Broker::set_max_cons_loss_days_at_site`] |
//!
//! The snapshot read surface backing the ~35 `strategy.*` builtins
//! (`position_size`, `equity`, `netprofit`, …) is [`StrategySnapshot`],
//! recorded per bar and read via [`Broker::snapshot_at`].
//!
//! # Configuration
//!
//! The broker is configured from the `strategy(...)` declaration through the
//! [`StrategyProperties`] trait. `pinewasm_hir::StrategyInfo` implements
//! it (so the interpreter's construction sites are untouched), and the AOT
//! runtime implements it over the config region the emitter serializes from
//! the same `StrategyInfo`. [`BrokerConfig`] is the crate's own plain-data
//! implementation for embeddings that carry no HIR.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

#[cfg(feature = "aeneas")]
pub mod aeneas_roots_broker;
mod broker;
#[cfg(any(test, feature = "aeneas"))]
mod num;
pub mod strategy_metrics;

pub use broker::*;

use alloc::string::String;
use alloc::vec::Vec;

/// The `strategy(...)` declaration surface the broker consumes.
///
/// Exactly the fields [`Broker::new_with_symbol`] reads — the broker's
/// honored parameters, not the full declaration (display concerns like
/// `overlay` or `max_lines_count` never reach the broker).
/// `pinewasm_hir::StrategyInfo` implements this trait, and
/// [`BrokerConfig`] is a plain-data implementation for hosts without HIR.
pub trait StrategyProperties {
    /// `initial_capital` — starting account equity.
    fn initial_capital(&self) -> f64;
    /// `default_qty_type` — `"fixed"`, `"cash"`, or `"percent_of_equity"`.
    fn default_qty_type(&self) -> &str;
    /// `default_qty_value` — default order size in `default_qty_type` units.
    fn default_qty_value(&self) -> f64;
    /// `commission_type` — `"percent"`, `"cash_per_contract"`, or
    /// `"cash_per_order"`.
    fn commission_type(&self) -> &str;
    /// `commission_value` — commission amount in `commission_type` units.
    fn commission_value(&self) -> f64;
    /// `slippage` — ticks of adverse slippage on market/stop fills.
    fn slippage(&self) -> u32;
    /// `pyramiding` — maximum concurrent same-direction entries (0 acts as 1).
    fn pyramiding(&self) -> u32;
    /// `process_orders_on_close` — same-bar close-tick order eligibility.
    fn process_orders_on_close(&self) -> bool;
    /// `calc_on_every_tick` — recalculate on each realtime update.
    fn calc_on_every_tick(&self) -> bool;
    /// `calc_on_order_fills` — recalculate after broker fills.
    fn calc_on_order_fills(&self) -> bool;
    /// The host-side "recalculate on every historical tick" override.
    fn calc_on_every_history_tick(&self) -> bool;
    /// `backtest_fill_limits_assumption` — extra verification ticks for limits.
    fn backtest_fill_limits_assumption(&self) -> u32;
    /// Exit allocation rule: `"FIFO"` (default) or `"ANY"`.
    fn close_entries_rule(&self) -> &str;
    /// `margin_long` — long margin percent.
    fn margin_long(&self) -> f64;
    /// `margin_short` — short margin percent.
    fn margin_short(&self) -> f64;
    /// `currency` — account currency token; empty/`"NONE"` means unset.
    fn currency(&self) -> &str;
}

/// Plain-data [`StrategyProperties`] for embeddings that carry no HIR — the
/// AOT runtime deserializes the emitter's config region into exactly this.
/// Field semantics and defaults match `pinewasm_hir::StrategyInfo`.
#[derive(Debug, Clone, PartialEq)]
pub struct BrokerConfig {
    pub initial_capital: f64,
    pub default_qty_type: String,
    pub default_qty_value: f64,
    pub commission_type: String,
    pub commission_value: f64,
    pub slippage: u32,
    pub pyramiding: u32,
    pub process_orders_on_close: bool,
    pub calc_on_every_tick: bool,
    pub calc_on_order_fills: bool,
    pub calc_on_every_history_tick: bool,
    pub backtest_fill_limits_assumption: u32,
    pub close_entries_rule: String,
    pub margin_long: f64,
    pub margin_short: f64,
    pub currency: String,
}

impl Default for BrokerConfig {
    /// The defaults of a bare `strategy("s")` declaration.
    fn default() -> Self {
        BrokerConfig {
            initial_capital: 1_000_000.0,
            default_qty_type: String::from("fixed"),
            default_qty_value: 1.0,
            commission_type: String::from("percent"),
            commission_value: 0.0,
            slippage: 0,
            pyramiding: 1,
            process_orders_on_close: false,
            calc_on_every_tick: false,
            calc_on_order_fills: false,
            calc_on_every_history_tick: false,
            backtest_fill_limits_assumption: 0,
            close_entries_rule: String::from("FIFO"),
            margin_long: 100.0,
            margin_short: 100.0,
            currency: String::from("NONE"),
        }
    }
}

impl StrategyProperties for BrokerConfig {
    fn initial_capital(&self) -> f64 {
        self.initial_capital
    }
    fn default_qty_type(&self) -> &str {
        &self.default_qty_type
    }
    fn default_qty_value(&self) -> f64 {
        self.default_qty_value
    }
    fn commission_type(&self) -> &str {
        &self.commission_type
    }
    fn commission_value(&self) -> f64 {
        self.commission_value
    }
    fn slippage(&self) -> u32 {
        self.slippage
    }
    fn pyramiding(&self) -> u32 {
        self.pyramiding
    }
    fn process_orders_on_close(&self) -> bool {
        self.process_orders_on_close
    }
    fn calc_on_every_tick(&self) -> bool {
        self.calc_on_every_tick
    }
    fn calc_on_order_fills(&self) -> bool {
        self.calc_on_order_fills
    }
    fn calc_on_every_history_tick(&self) -> bool {
        self.calc_on_every_history_tick
    }
    fn backtest_fill_limits_assumption(&self) -> u32 {
        self.backtest_fill_limits_assumption
    }
    fn close_entries_rule(&self) -> &str {
        &self.close_entries_rule
    }
    fn margin_long(&self) -> f64 {
        self.margin_long
    }
    fn margin_short(&self) -> f64 {
        self.margin_short
    }
    fn currency(&self) -> &str {
        &self.currency
    }
}

/// Optional bounds an embedding imposes on the broker's growable state. The
/// interpreter runs unbounded; the AOT runtime
/// sets generous finite caps so a pathological script surfaces a named
/// structured error instead of exhausting the module's arena.
///
/// Caps are checked at the bar-walk entry points ([`Broker::process_bar_ticks`]
/// and [`Broker::finish_bar`]), not at placement: placement stays infallible
/// (invalid orders are dropped/clamped, PineWASM's input policy), and a
/// breach reports the first bar on which the book exceeded its bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrokerLimits {
    /// Maximum simultaneously pending (placed-but-unfilled) orders.
    pub max_pending_orders: usize,
    /// Maximum simultaneously open position lots.
    pub max_open_lots: usize,
    /// Maximum live exit recipes (persistent + deferred named exits).
    pub max_exit_recipes: usize,
}

impl Default for BrokerLimits {
    /// Unlimited, as in the interpreter.
    fn default() -> Self {
        BrokerLimits {
            max_pending_orders: usize::MAX,
            max_open_lots: usize::MAX,
            max_exit_recipes: usize::MAX,
        }
    }
}

/// A structured broker failure. A panic is a defect rather than part of the
/// broker contract; embeddings that set [`BrokerLimits`] receive cap breaches
/// as values and map them onto their own error surface (the AOT runtime raises
/// its `"Indicator failure: "` class naming the cap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerError {
    /// A [`BrokerLimits`] bound was exceeded.
    CapExceeded {
        /// Which cap: `"pending orders"`, `"open lots"`, or `"exit recipes"`.
        cap: &'static str,
        /// The configured bound.
        limit: usize,
        /// The size that exceeded it.
        actual: usize,
    },
}

impl core::fmt::Display for BrokerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BrokerError::CapExceeded { cap, limit, actual } => {
                write!(f, "broker cap exceeded: {cap} ({actual} > {limit})")
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for BrokerError {}

/// One closed trade in a strategy run: a matched entry/exit pair with
/// the realized profit. `entry_bar`/`exit_bar` are history bar indices;
/// `qty` is the number of contracts closed; `pnl` is realized profit net of
/// commission and slippage. Documented in docs/browser.md.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct StrategyTrade {
    /// Stable zero-based ordinal in the full run's closed-trade sequence. Rows
    /// retained from PineWASM's filled-order window can have gaps between indices.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub index: Option<u32>,
    /// Script-provided id of the opening order.
    pub entry_id: String,
    /// Zero-based bar on which the opening fill occurred.
    pub entry_bar: u32,
    /// Opening fill price before account-currency conversion.
    pub entry_price: f64,
    /// Zero-based bar on which the closing fill occurred.
    pub exit_bar: u32,
    /// Closing fill price before account-currency conversion.
    pub exit_price: f64,
    /// Absolute number of contracts closed by this trade.
    pub qty: f64,
    /// The comment attached to the closing order, when any.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub comment: Option<String>,
    /// Realized account-currency profit after commission and slippage.
    pub pnl: f64,
    /// Full-run cumulative realized P&L immediately after this row closed.
    /// This remains exact across gaps in retained detail.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub cumulative_pnl: Option<f64>,
    /// Account-currency notional of the closed quantity at its entry price and
    /// entry-bar FX rate. Broker-produced rows always populate it; `None`
    /// supports outcomes serialized before currency-aware trade detail.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub entry_notional: Option<f64>,
    /// Direction sign of the trade: `+1.0` long, `-1.0` short. The signed
    /// traded size is `sign * qty` (`strategy.closedtrades.size`).
    #[cfg_attr(feature = "serde", serde(default))]
    pub sign: f64,
    /// Total commission charged on this trade (its pro-rated entry share plus
    /// the exit commission) — `strategy.closedtrades.commission`.
    #[cfg_attr(feature = "serde", serde(default))]
    pub commission: f64,
    /// The largest favorable excursion reached during this trade.
    #[cfg_attr(feature = "serde", serde(default))]
    pub max_runup: f64,
    /// The largest adverse excursion reached during this trade.
    #[cfg_attr(feature = "serde", serde(default))]
    pub max_drawdown: f64,
    /// The closing order's id — `strategy.closedtrades.exit_id`.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub exit_id: Option<String>,
    /// The name of a closing order that has no id of its own:
    /// `"Close entry(s) order <id>"` for `strategy.close`,
    /// `"Close position order"` for `strategy.close_all`. Absent when the
    /// closing order has a comment; the comment is used as the name instead.
    /// Not visible to Pine.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub exit_order_name: Option<String>,
    /// The opening order's comment — `strategy.closedtrades.entry_comment`.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub entry_comment: Option<String>,
    // Broker-internal retention stamp for the successful opening fill. This is
    // deliberately absent from the serialized result and wire contract.
    #[cfg_attr(feature = "serde", serde(skip))]
    entry_order_ordinal: u64,
    // Broker-internal retention stamp for the successful closing fill.
    #[cfg_attr(feature = "serde", serde(skip))]
    exit_order_ordinal: u64,
    // Broker-internal addends of `commission`, exactly as realized P&L
    // subtracted them. Their rounded sum cannot be split back into the same
    // bits, so the compact trade ledger reads them here. `None` on a row this
    // broker did not produce. Deliberately absent from the serialized result and
    // wire contract, like the ordinals above.
    #[cfg_attr(feature = "serde", serde(skip))]
    commission_split: Option<CommissionSplit>,
}

/// The two charges [`StrategyTrade::commission`] sums: the closed quantity's
/// share of the opening fill's commission and its share of the closing fill's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CommissionSplit {
    pub(crate) entry: f64,
    pub(crate) exit: f64,
}

/// The open position at the end of a strategy run. `size` is signed:
/// positive for long, negative for short, `0.0` when flat. `avg_price` is the
/// average entry price of the open position (meaningless when flat).
/// Documented in docs/browser.md.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct StrategyPosition {
    /// Signed contract quantity: positive long, negative short, zero flat.
    pub size: f64,
    /// Quantity-weighted average entry price; undefined in economic terms when
    /// [`StrategyPosition::size`] is zero.
    pub avg_price: f64,
}

/// Retention-independent identity/value for a largest winning or losing trade
/// in one direction scope.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct StrategyReportTradeHighlight {
    /// Stable zero-based ordinal in the full run's closed-trade sequence.
    pub trade_index: u32,
    /// Script-provided id of the highlighted trade's opening order.
    pub entry_id: String,
    /// Direction sign: `1.0` for long or `-1.0` for short.
    pub sign: f64,
    /// Realized account-currency profit of the highlighted trade.
    pub pnl: f64,
    /// Profit divided by entry notional as a percentage, when that notional
    /// was finite and positive.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub return_percent: Option<f64>,
}

/// Exact full-run three-standard-deviation outlier summary. The P&L sample is
/// retained internally by the broker only for this calculation; old trade
/// accessor rows remain evicted according to PineWASM's 9,000-successful-order
/// window.
#[derive(Debug, Clone, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct StrategyReportOutliers {
    /// Number of trades outside the recorded three-standard-deviation fences.
    pub count: u32,
    /// Sum of realized profit for the outlier trades.
    pub total_pnl: f64,
    /// Mean profit minus three population standard deviations, when defined.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub lower_fence: Option<f64>,
    /// Mean profit plus three population standard deviations, when defined.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub upper_fence: Option<f64>,
    /// Stable full-run trade ordinals classified as outliers.
    #[cfg_attr(feature = "serde", serde(default))]
    pub trade_indices: Vec<u32>,
}

/// Full-period inputs for one of the all, long or short scopes.
/// Sums and explicit denominators preserve exact averages even when
/// closed-trade detail has been trimmed.
#[derive(Debug, Clone, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct StrategyReportScopeAggregate {
    /// Total closed trades in this direction scope.
    pub closed_trades: u32,
    /// Open entry lots remaining at run end in this direction scope.
    pub open_trades: u32,
    /// Closed trades with positive realized profit.
    pub win_trades: u32,
    /// Closed trades with negative realized profit.
    pub loss_trades: u32,
    /// Closed trades with exactly zero realized profit.
    pub even_trades: u32,
    /// Sum of realized account-currency profit for closed trades.
    pub net_profit: f64,
    /// Account-currency profit of positions still open at run end.
    pub open_profit: f64,
    /// Sum of positive realized trade profits.
    pub gross_profit: f64,
    /// Absolute sum of negative realized trade profits.
    pub gross_loss: f64,
    /// Every executed transaction fee attributable to this direction,
    /// including the entry fee carried by lots still open at run end.
    pub commission: f64,
    /// Sum of defined closed-trade returns, in percentage points.
    pub return_percent_sum: f64,
    /// Number of trades contributing to [`StrategyReportScopeAggregate::return_percent_sum`].
    pub return_percent_count: u32,
    /// Sum of defined percentage returns for winning trades.
    pub winning_return_percent_sum: f64,
    /// Number of winning trades with a defined percentage return.
    pub winning_return_percent_count: u32,
    /// Sum of defined percentage returns for losing trades.
    pub losing_return_percent_sum: f64,
    /// Number of losing trades with a defined percentage return.
    pub losing_return_percent_count: u32,
    /// Sum of inclusive entry-through-exit bar counts across closed trades.
    pub bars_held_sum: u64,
    /// Number of closed trades contributing to [`StrategyReportScopeAggregate::bars_held_sum`].
    pub bars_held_count: u32,
    /// Sum of inclusive bar counts across winning trades.
    pub winning_bars_held_sum: u64,
    /// Number of winning trades contributing to `winning_bars_held_sum`.
    pub winning_bars_held_count: u32,
    /// Sum of inclusive bar counts across losing trades.
    pub losing_bars_held_sum: u64,
    /// Number of losing trades contributing to `losing_bars_held_sum`.
    pub losing_bars_held_count: u32,
    /// Sum of finite non-negative maximum favorable excursions.
    pub favorable_excursion_sum: f64,
    /// Number of trades contributing to `favorable_excursion_sum`.
    pub favorable_excursion_count: u32,
    /// Largest finite favorable excursion, when any was recorded.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub favorable_excursion_max: Option<f64>,
    /// Sum of favorable excursions divided by positive entry notional, in
    /// percentage points.
    pub favorable_excursion_percent_sum: f64,
    /// Number of trades contributing to `favorable_excursion_percent_sum`.
    pub favorable_excursion_percent_count: u32,
    /// Largest favorable-excursion percentage, when defined.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub favorable_excursion_percent_max: Option<f64>,
    /// Sum of finite non-negative maximum adverse excursions.
    pub adverse_excursion_sum: f64,
    /// Number of trades contributing to `adverse_excursion_sum`.
    pub adverse_excursion_count: u32,
    /// Largest finite adverse excursion, when any was recorded.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub adverse_excursion_max: Option<f64>,
    /// Sum of adverse excursions divided by positive entry notional, in
    /// percentage points.
    pub adverse_excursion_percent_sum: f64,
    /// Number of trades contributing to `adverse_excursion_percent_sum`.
    pub adverse_excursion_percent_count: u32,
    /// Largest adverse-excursion percentage, when defined.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub adverse_excursion_percent_max: Option<f64>,
    /// Winning trade with the largest realized profit, when any exists.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub largest_winning_trade: Option<StrategyReportTradeHighlight>,
    /// Losing trade with the most-negative realized profit, when any exists.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub largest_losing_trade: Option<StrategyReportTradeHighlight>,
    /// Three-standard-deviation P&L outlier summary for this direction scope.
    #[cfg_attr(feature = "serde", serde(default))]
    pub outliers: StrategyReportOutliers,
}

/// Exact full-run aggregates split into the all, long and short scopes.
#[derive(Debug, Clone, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct StrategyReportAggregates {
    /// Values across long and short trades together.
    pub all: StrategyReportScopeAggregate,
    /// Values for long trades and open long lots.
    pub long: StrategyReportScopeAggregate,
    /// Values for short trades and open short lots.
    pub short: StrategyReportScopeAggregate,
}
