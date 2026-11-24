//! The built-in catalog: every supported name a source program can call or read
//! without declaring it.
//!
//! The IR node definitions (`HirExpr`, `HirStmt`, `HirProgram`) live in
//! `lib.rs`. Everything here is re-exported from the crate root, so
//! `pinewasm_hir::BuiltinSeries` and friends resolve from there.

use crate::{Qualifier, Type};
use serde::{Deserialize, Serialize};

/// Pine's nominal type name for the `strategy.long` / `strategy.short`
/// constants and the `direction` parameter of entry/order commands.
pub const STRATEGY_DIRECTION_TYPE: &str = "strategy_direction";

/// A signed order direction (`strategy.long` / `strategy.short`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderDirection {
    Long,
    Short,
}

impl OrderDirection {
    /// `+1.0` for long, `-1.0` for short — the sign a qty is multiplied by.
    pub fn sign(self) -> f64 {
        match self {
            OrderDirection::Long => 1.0,
            OrderDirection::Short => -1.0,
        }
    }
}

/// The built-in market/context series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BuiltinSeries {
    Open,
    High,
    Low,
    Close,
    Volume,
    /// 0-based bar counter, typed `series int`.
    BarIndex,
    /// `hl2` — `(high + low) / 2`, `series float`.
    Hl2,
    /// `hlc3` — `(high + low + close) / 3`, `series float`.
    Hlc3,
    /// `ohlc4` — `(open + high + low + close) / 4`, `series float`.
    Ohlc4,
    /// `hlcc4` — `(high + low + close + close) / 4` (double-weighted close),
    /// `series float`.
    Hlcc4,
    /// `time` — the bar's open time in UNIX epoch milliseconds, `series int`.
    Time,
    /// `time_close` — the bar's close time in UNIX epoch milliseconds,
    /// computed as `time + timeframe.in_seconds * 1000`, `series int`.
    TimeClose,
    /// `timenow` — the current time. Deterministic in this VM: it equals the
    /// last bar's close time on every bar (see docs/runtime.md). `series int`.
    Timenow,
    /// `last_bar_index` — the `bar_index` of the last bar in the dataset,
    /// `series int` (same value on every bar in historical execution).
    LastBarIndex,
    /// `last_bar_time` — the `time` of the last bar in the dataset,
    /// `series int` (same value on every bar in historical execution).
    LastBarTime,
    /// `dayofweek` — day of week of the bar's open time in the exchange
    /// timezone, `series int`. 1 = Sunday … 7 = Saturday.
    DayOfWeek,
    /// `dayofmonth` — day of month (1..=31), `series int`.
    DayOfMonth,
    /// `month` — month (1..=12), `series int`.
    Month,
    /// `year` — full year (e.g. 2020), `series int`.
    Year,
    /// `hour` — hour of day (0..=23) in the exchange timezone, `series int`.
    Hour,
    /// `minute` — minute of hour (0..=59), `series int`.
    Minute,
    /// `second` — second of minute (0..=59), `series int`.
    Second,
    /// `weekofyear` — ISO 8601 week-of-year (1..=53) of the bar's open time in
    /// the exchange timezone, `series int`.
    WeekOfYear,
    // ---- barstate.* — `series bool` predicates wired to the driver ----
    /// `barstate.isfirst` — true only on the very first bar (`bar_index == 0`).
    BarstateIsfirst,
    /// `barstate.islast` — true on the last bar of the run (the last historical
    /// bar, or the realtime bar once realtime execution is active).
    BarstateIslast,
    /// `barstate.ishistory` — true on a historical (committed) bar; false on a
    /// realtime bar's executions.
    BarstateIshistory,
    /// `barstate.isrealtime` — true while executing the realtime bar; false on
    /// every historical bar.
    BarstateIsrealtime,
    /// `barstate.isconfirmed` — true on the closing execution of a bar: every
    /// historical bar (each runs once and commits) and the realtime bar's
    /// `is_bar_close` tick.
    BarstateIsconfirmed,
    /// `barstate.isnew` — true on the first execution of a bar: every
    /// historical bar and the realtime bar's opening tick.
    BarstateIsnew,
    /// `barstate.islastconfirmedhistory` — true on the last historical bar when
    /// realtime execution is active (`islast && ishistory` on the last committed
    /// bar); false when there is no realtime segment.
    BarstateIslastconfirmedhistory,
    // ---- strategy.* read surface — current broker-state getters, valid
    // only in a `strategy()` script (E0241). Each reads a per-bar snapshot the
    // broker records at the start of the bar (after prior-bar fills), so history
    // (`strategy.position_size[1]`) reads the prior bar's snapshot.
    /// `strategy.position_size` — signed size of the open position, `series float`.
    StrategyPositionSize,
    /// `strategy.position_avg_price` — average entry price of the open position
    /// (`na` when flat), `series float`.
    StrategyPositionAvgPrice,
    /// `strategy.openprofit` — unrealized profit of the open position marked to
    /// the current close, `series float`.
    StrategyOpenProfit,
    /// `strategy.netprofit` — cumulative realized net profit, `series float`.
    StrategyNetProfit,
    /// `strategy.grossprofit` — cumulative realized winning-trade profit (>= 0).
    StrategyGrossProfit,
    /// `strategy.grossloss` — cumulative realized losing-trade loss magnitude (>= 0).
    StrategyGrossLoss,
    /// `strategy.equity` — `initial_capital + netprofit + openprofit`, `series float`.
    StrategyEquity,
    /// `strategy.initial_capital` — the starting account equity, `series float`
    /// (constant across the run).
    StrategyInitialCapital,
    /// `strategy.wintrades` — count of closed winning trades, `series int`.
    StrategyWinTrades,
    /// `strategy.losstrades` — count of closed losing trades, `series int`.
    StrategyLossTrades,
    /// `strategy.eventrades` — count of closed break-even trades, `series int`.
    StrategyEvenTrades,
    /// `strategy.closedtrades` — total closed trades, `series int`.
    StrategyClosedTrades,
    /// `strategy.opentrades` — count of currently open lots, `series int`.
    StrategyOpenTrades,
    /// `strategy.max_drawdown` — largest realized-equity peak-to-trough drop so
    /// far (>= 0), `series float`.
    StrategyMaxDrawdown,
    /// `strategy.max_runup` — largest realized-equity trough-to-peak rise so far
    /// (>= 0), `series float`.
    StrategyMaxRunup,
    /// `strategy.avg_trade` — `netprofit / closedtrades` (`na` when none), `series float`.
    StrategyAvgTrade,
    /// `strategy.avg_winning_trade` — `grossprofit / wintrades` (`na` when none).
    StrategyAvgWinningTrade,
    /// `strategy.avg_losing_trade` — `grossloss / losstrades` (`na` when none).
    StrategyAvgLosingTrade,
    /// `strategy.max_contracts_held_all` — maximum units held in any single
    /// trade so far, regardless of direction, `series float`.
    StrategyMaxContractsHeldAll,
    /// `strategy.max_contracts_held_long` — maximum units held in any single
    /// long trade so far (>= 0), `series float`.
    StrategyMaxContractsHeldLong,
    /// `strategy.max_contracts_held_short` — maximum units held in any single
    /// short trade so far (magnitude, >= 0), `series float`.
    StrategyMaxContractsHeldShort,
    /// Account currency selected by the strategy declaration (`simple string`).
    StrategyAccountCurrency,
    /// Average losing-trade return percentage (`series float`).
    StrategyAvgLosingTradePercent,
    /// Average return percentage across all closed trades (`series float`).
    StrategyAvgTradePercent,
    /// Average winning-trade return percentage (`series float`).
    StrategyAvgWinningTradePercent,
    /// Global index of the oldest retained closed trade (`series int`).
    StrategyClosedTradesFirstIndex,
    /// Gross losing P/L as a percentage of initial capital (`series float`).
    StrategyGrossLossPercent,
    /// Gross winning P/L as a percentage of initial capital (`series float`).
    StrategyGrossProfitPercent,
    /// Price at which current equity reaches the margin requirement (`series float`).
    StrategyMarginLiquidationPrice,
    /// Maximum equity drawdown percentage (`series float`).
    StrategyMaxDrawdownPercent,
    /// Maximum equity run-up percentage (`series float`).
    StrategyMaxRunupPercent,
    /// Net realized P/L as a percentage of initial capital (`series float`).
    StrategyNetProfitPercent,
    /// Current open P/L as a percentage of realized equity (`series float`).
    StrategyOpenProfitPercent,
    /// Capital reserved by all open trades (`series float`).
    StrategyOpenTradesCapitalHeld,
    /// ID of the order that initially opened the current position (`series string`).
    StrategyPositionEntryName,

    // ---- timeframe.* — chart-timeframe context accessors. Each is a
    // per-run constant derived from the chart timeframe string (default "1D");
    // it holds the same value on every bar. Modeled as builtin series so a
    // host-driven timeframe flows in through `MarketData::timeframe` without a
    // new opcode. `timeframe.change` is the one genuinely per-bar member. ----
    /// `timeframe.period` — the chart timeframe string (e.g. "1D"), `string`.
    TimeframePeriod,
    /// `timeframe.multiplier` — the numeric part of the timeframe (e.g. 1 for
    /// "1D", 5 for "5", 30 for "30S"), `int`.
    TimeframeMultiplier,
    /// `timeframe.main_period` — the chart's main timeframe string. In this
    /// headless VM (no secondary contexts) this equals `timeframe.period`.
    TimeframeMainPeriod,
    /// `timeframe.isdwm` — true when the timeframe is days/weeks/months, `bool`.
    TimeframeIsdwm,
    /// `timeframe.isdaily` — true when the timeframe is in days, `bool`.
    TimeframeIsdaily,
    /// `timeframe.isweekly` — true when the timeframe is in weeks, `bool`.
    TimeframeIsweekly,
    /// `timeframe.ismonthly` — true when the timeframe is in months, `bool`.
    TimeframeIsmonthly,
    /// `timeframe.isintraday` — true for seconds/minutes/hours timeframes, `bool`.
    TimeframeIsintraday,
    /// `timeframe.isminutes` — true when the timeframe is in minutes, `bool`.
    TimeframeIsminutes,
    /// `timeframe.isseconds` — true when the timeframe is in seconds, `bool`.
    TimeframeIsseconds,
    /// `timeframe.isticks` — true when the timeframe is tick-based, `bool`.
    /// This VM has no tick timeframes, so it is always false.
    TimeframeIsticks,
    /// `timeframe.change(tf)` is a function (see the analyzer), not a variable;
    /// no series variant is needed.

    // ---- syminfo.* — host-or-default symbol metadata. Each reads a
    // `MarketData::syminfo` field (defaults are listed per field) and is a per-run
    // constant. ----
    /// `syminfo.ticker` — the symbol name without the exchange prefix, `string`.
    SyminfoTicker,
    /// `syminfo.tickerid` — the fully qualified symbol identifier, `string`.
    SyminfoTickerid,
    /// `syminfo.basecurrency` — the symbol's base currency, `string` (default "").
    SyminfoBasecurrency,
    /// `syminfo.currency` — the symbol's quote currency, `string` (default "").
    SyminfoCurrency,
    /// `syminfo.description` — the symbol's description, `string` (default "").
    SyminfoDescription,
    /// `syminfo.country` — the symbol's country code, `string` (default "").
    SyminfoCountry,
    /// `syminfo.industry` — the symbol's industry, `string` (default "").
    SyminfoIndustry,
    /// `syminfo.sector` — the symbol's sector, `string` (default "").
    SyminfoSector,
    /// `syminfo.type` — the market type ("stock"/"crypto"/...), `string`.
    SyminfoType,
    /// `syminfo.root` — the root symbol (futures), `string` (default "").
    SyminfoRoot,
    /// `syminfo.prefix` — the exchange prefix, `string` (default "").
    SyminfoPrefix,
    /// `syminfo.session` — the session specification, `string` (default "").
    SyminfoSession,
    /// `syminfo.timezone` — the exchange timezone, `string` (default "UTC").
    SyminfoTimezone,
    /// `syminfo.volumetype` — the volume type ("base"/"quote"/...), `string`.
    SyminfoVolumetype,
    /// `syminfo.main_tickerid` — the main ticker identifier, `string` (default "").
    SyminfoMainTickerid,
    /// `syminfo.mintick` — the minimum price increment, `float`.
    SyminfoMintick,
    /// `syminfo.minmove` — the minimum price move (integer numerator), `int`.
    SyminfoMinmove,
    /// `syminfo.pricescale` — the price scale (10^decimals), `int`.
    SyminfoPricescale,
    /// `syminfo.pointvalue` — the point value of one contract, `float`.
    SyminfoPointvalue,
    /// `syminfo.mincontract` — the minimum tradable contract/quantity, `float`.
    SyminfoMincontract,

    // ---- session.* — session-position predicates. ----
    /// `session.isfirstbar` — true on the first bar of the session, `bool`.
    /// This headless VM has one continuous session, so it is true on bar 0.
    SessionIsfirstbar,
    /// `session.isfirstbar_regular` — like `isfirstbar` for the regular
    /// session; true on bar 0 here, `bool`.
    SessionIsfirstbarRegular,
    /// `session.islastbar` — true on the last bar of the session, `bool`.
    /// True on the final bar of the run here.
    SessionIslastbar,
    /// `session.islastbar_regular` — like `islastbar` for the regular session;
    /// true on the final bar here, `bool`.
    SessionIslastbarRegular,
    /// `session.ismarket` — true during the regular market session, `bool`.
    /// This VM models a single always-regular session, so it is always true.
    SessionIsmarket,
    /// `session.ispremarket` — true during the pre-market session, `bool`.
    /// Always false here (no extended sessions).
    SessionIspremarket,
    /// `session.ispostmarket` — true during the post-market session, `bool`.
    /// Always false here (no extended sessions).
    SessionIspostmarket,
    /// `time_tradingday` — the UNIX time (epoch ms) of the start of the trading
    /// day the current bar belongs to, `int`. Host-or-default from the time axis.
    TimeTradingday,

    // ---- Host-or-default series. Each reads an OPTIONAL per-bar host
    // series on `MarketData` (default `na`); no real tick/viewport feed is
    // involved — data sourcing stays a host concern. ----
    /// `bid` — the highest price an active buyer will pay, `series float`.
    /// Reads the optional host `bid` series; `na` by default.
    Bid,
    /// `ask` — the lowest price an active seller will accept, `series float`.
    /// Like [`BuiltinSeries::Bid`]: reads the optional host `ask` series, `na`
    /// by default.
    Ask,
    /// `syminfo.expiration_date` — a host-provided UNIX timestamp (epoch ms)
    /// for the start of the last day of a non-continuous futures contract.
    /// PineWASM currently models this as `series int`; the host is responsible
    /// for applicability and omission produces `na`.
    SyminfoExpirationDate,
    // ---- SYMINFO fundamentals (host-or-default, `na` by default). Company
    // fundamental / analyst data the headless VM reads from an OPTIONAL host
    // `SymInfo` field; no real-time data feed is involved. Field
    // names follow the Pine Script naming convention. ----
    /// `syminfo.employees` — the number of employees the company has,
    /// `series int`. Host-or-default `na`.
    SyminfoEmployees,
    /// `syminfo.shareholders` — the number of shareholders the company has,
    /// `series int`. Host-or-default `na`.
    SyminfoShareholders,
    /// `syminfo.shares_outstanding_float` — the total shares outstanding
    /// excluding restricted shares, `series float`. Host-or-default `na`.
    SyminfoSharesOutstandingFloat,
    /// `syminfo.shares_outstanding_total` — the total shares outstanding
    /// including restricted shares, `simple int`. Host-or-default `na`.
    SyminfoSharesOutstandingTotal,
    /// `syminfo.recommendations_buy` — analysts giving a "Buy" rating,
    /// `series int`. Host-or-default `na`.
    SyminfoRecommendationsBuy,
    /// `syminfo.recommendations_buy_strong` — analysts giving a "Strong Buy"
    /// rating, `series int`. Host-or-default `na`.
    SyminfoRecommendationsBuyStrong,
    /// `syminfo.recommendations_date` — start date (epoch ms) of the last set
    /// of recommendations, `series int`. Host-or-default `na`.
    SyminfoRecommendationsDate,
    /// `syminfo.recommendations_hold` — analysts giving a "Hold" rating,
    /// `series int`. Host-or-default `na`.
    SyminfoRecommendationsHold,
    /// `syminfo.recommendations_sell` — analysts giving a "Sell" rating,
    /// `series int`. Host-or-default `na`.
    SyminfoRecommendationsSell,
    /// `syminfo.recommendations_sell_strong` — analysts giving a "Strong Sell"
    /// rating, `series int`. Host-or-default `na`.
    SyminfoRecommendationsSellStrong,
    /// `syminfo.recommendations_total` — total number of recommendations,
    /// `series int`. Host-or-default `na`.
    SyminfoRecommendationsTotal,
    /// `syminfo.target_price_average` — average of the last yearly analyst
    /// price targets, `series float`. Host-or-default `na`.
    SyminfoTargetPriceAverage,
    /// `syminfo.target_price_date` — start date (epoch ms) of the last price
    /// target prediction, `series int`. Host-or-default `na`.
    SyminfoTargetPriceDate,
    /// `syminfo.target_price_estimates` — number of price target predictions,
    /// `series int`. Host-or-default `na`.
    SyminfoTargetPriceEstimates,
    /// `syminfo.target_price_high` — the last highest yearly analyst price
    /// target, `series float`. Host-or-default `na`.
    SyminfoTargetPriceHigh,
    /// `syminfo.target_price_low` — the last lowest yearly analyst price
    /// target, `series float`. Host-or-default `na`.
    SyminfoTargetPriceLow,
    /// `syminfo.target_price_median` — median of the last yearly analyst price
    /// targets, `series float`. Host-or-default `na`.
    SyminfoTargetPriceMedian,
    /// `chart.left_visible_bar_time` — the open time (epoch ms) of the leftmost
    /// visible bar, `series int`. The headless VM has no viewport, so it reads
    /// the optional host value, `na` by default.
    ChartLeftVisibleBarTime,
    /// `chart.right_visible_bar_time` — the open time (epoch ms) of the
    /// rightmost visible bar, `series int`. Host-or-default `na` (no viewport).
    ChartRightVisibleBarTime,

    // ---- dividends.future_* / earnings.future_* (host-or-default upcoming-
    // events feed). These are built-in series holding the next scheduled
    // dividend / earnings values. The headless VM reads an OPTIONAL host
    // `MarketData::upcoming_events` field, `na` by default; no real-time corporate-
    // actions feed is involved. Names follow the Pine Script naming convention. ----
    /// `dividends.future_amount` — the amount of the upcoming dividend per
    /// share, `series float`. Host-or-default `na`.
    DividendsFutureAmount,
    /// `dividends.future_ex_date` — the ex-dividend date (epoch ms) of the
    /// upcoming dividend, `series int`. Host-or-default `na`.
    DividendsFutureExDate,
    /// `dividends.future_pay_date` — the payment date (epoch ms) of the
    /// upcoming dividend, `series int`. Host-or-default `na`.
    DividendsFuturePayDate,
    /// `earnings.future_eps` — the estimated EPS of the upcoming earnings
    /// report, `series float`. Host-or-default `na`.
    EarningsFutureEps,
    /// `earnings.future_period_end_time` — the end time (epoch ms) of the
    /// fiscal period for the upcoming earnings report, `series int`.
    /// Host-or-default `na`.
    EarningsFuturePeriodEndTime,
    /// `earnings.future_revenue` — the estimated revenue of the upcoming
    /// earnings report, `series float`. Host-or-default `na`.
    EarningsFutureRevenue,
    /// `earnings.future_time` — the announcement time (epoch ms) of the
    /// upcoming earnings report, `series int`. Host-or-default `na`.
    EarningsFutureTime,
    /// International Securities Identification Number; empty when unavailable.
    SyminfoIsin,
    /// `syminfo.current_contract` — the ticker identifier of the contract a
    /// continuous futures symbol currently follows, `simple string`; `na`
    /// when the host supplies none.
    SyminfoCurrentContract,
}

impl BuiltinSeries {
    /// Whether Pine Script allows this builtin as an `input.source` default or
    /// host selection. The public surface is intentionally narrower than the
    /// full numeric-series registry: volume, time, bar index, strategy state,
    /// and context series are not source-input choices.
    pub fn is_input_source(self) -> bool {
        matches!(
            self,
            BuiltinSeries::Open
                | BuiltinSeries::High
                | BuiltinSeries::Low
                | BuiltinSeries::Close
                | BuiltinSeries::Hl2
                | BuiltinSeries::Hlc3
                | BuiltinSeries::Ohlc4
                | BuiltinSeries::Hlcc4
        )
    }

    /// Canonical Pine source name for this built-in series.
    pub fn name(self) -> &'static str {
        match self {
            BuiltinSeries::Open => "open",
            BuiltinSeries::High => "high",
            BuiltinSeries::Low => "low",
            BuiltinSeries::Close => "close",
            BuiltinSeries::Volume => "volume",
            BuiltinSeries::BarIndex => "bar_index",
            BuiltinSeries::Hl2 => "hl2",
            BuiltinSeries::Hlc3 => "hlc3",
            BuiltinSeries::Ohlc4 => "ohlc4",
            BuiltinSeries::Hlcc4 => "hlcc4",
            BuiltinSeries::Time => "time",
            BuiltinSeries::TimeClose => "time_close",
            BuiltinSeries::Timenow => "timenow",
            BuiltinSeries::LastBarIndex => "last_bar_index",
            BuiltinSeries::LastBarTime => "last_bar_time",
            BuiltinSeries::DayOfWeek => "dayofweek",
            BuiltinSeries::DayOfMonth => "dayofmonth",
            BuiltinSeries::Month => "month",
            BuiltinSeries::Year => "year",
            BuiltinSeries::Hour => "hour",
            BuiltinSeries::Minute => "minute",
            BuiltinSeries::Second => "second",
            BuiltinSeries::WeekOfYear => "weekofyear",
            BuiltinSeries::BarstateIsfirst => "barstate.isfirst",
            BuiltinSeries::BarstateIslast => "barstate.islast",
            BuiltinSeries::BarstateIshistory => "barstate.ishistory",
            BuiltinSeries::BarstateIsrealtime => "barstate.isrealtime",
            BuiltinSeries::BarstateIsconfirmed => "barstate.isconfirmed",
            BuiltinSeries::BarstateIsnew => "barstate.isnew",
            BuiltinSeries::BarstateIslastconfirmedhistory => "barstate.islastconfirmedhistory",
            BuiltinSeries::StrategyPositionSize => "strategy.position_size",
            BuiltinSeries::StrategyPositionAvgPrice => "strategy.position_avg_price",
            BuiltinSeries::StrategyOpenProfit => "strategy.openprofit",
            BuiltinSeries::StrategyNetProfit => "strategy.netprofit",
            BuiltinSeries::StrategyGrossProfit => "strategy.grossprofit",
            BuiltinSeries::StrategyGrossLoss => "strategy.grossloss",
            BuiltinSeries::StrategyEquity => "strategy.equity",
            BuiltinSeries::StrategyInitialCapital => "strategy.initial_capital",
            BuiltinSeries::StrategyWinTrades => "strategy.wintrades",
            BuiltinSeries::StrategyLossTrades => "strategy.losstrades",
            BuiltinSeries::StrategyEvenTrades => "strategy.eventrades",
            BuiltinSeries::StrategyClosedTrades => "strategy.closedtrades",
            BuiltinSeries::StrategyOpenTrades => "strategy.opentrades",
            BuiltinSeries::StrategyMaxDrawdown => "strategy.max_drawdown",
            BuiltinSeries::StrategyMaxRunup => "strategy.max_runup",
            BuiltinSeries::StrategyAvgTrade => "strategy.avg_trade",
            BuiltinSeries::StrategyAvgWinningTrade => "strategy.avg_winning_trade",
            BuiltinSeries::StrategyAvgLosingTrade => "strategy.avg_losing_trade",
            BuiltinSeries::StrategyMaxContractsHeldAll => "strategy.max_contracts_held_all",
            BuiltinSeries::StrategyMaxContractsHeldLong => "strategy.max_contracts_held_long",
            BuiltinSeries::StrategyMaxContractsHeldShort => "strategy.max_contracts_held_short",
            BuiltinSeries::StrategyAccountCurrency => "strategy.account_currency",
            BuiltinSeries::StrategyAvgLosingTradePercent => "strategy.avg_losing_trade_percent",
            BuiltinSeries::StrategyAvgTradePercent => "strategy.avg_trade_percent",
            BuiltinSeries::StrategyAvgWinningTradePercent => "strategy.avg_winning_trade_percent",
            BuiltinSeries::StrategyClosedTradesFirstIndex => "strategy.closedtrades.first_index",
            BuiltinSeries::StrategyGrossLossPercent => "strategy.grossloss_percent",
            BuiltinSeries::StrategyGrossProfitPercent => "strategy.grossprofit_percent",
            BuiltinSeries::StrategyMarginLiquidationPrice => "strategy.margin_liquidation_price",
            BuiltinSeries::StrategyMaxDrawdownPercent => "strategy.max_drawdown_percent",
            BuiltinSeries::StrategyMaxRunupPercent => "strategy.max_runup_percent",
            BuiltinSeries::StrategyNetProfitPercent => "strategy.netprofit_percent",
            BuiltinSeries::StrategyOpenProfitPercent => "strategy.openprofit_percent",
            BuiltinSeries::StrategyOpenTradesCapitalHeld => "strategy.opentrades.capital_held",
            BuiltinSeries::StrategyPositionEntryName => "strategy.position_entry_name",
            BuiltinSeries::TimeframePeriod => "timeframe.period",
            BuiltinSeries::TimeframeMultiplier => "timeframe.multiplier",
            BuiltinSeries::TimeframeMainPeriod => "timeframe.main_period",
            BuiltinSeries::TimeframeIsdwm => "timeframe.isdwm",
            BuiltinSeries::TimeframeIsdaily => "timeframe.isdaily",
            BuiltinSeries::TimeframeIsweekly => "timeframe.isweekly",
            BuiltinSeries::TimeframeIsmonthly => "timeframe.ismonthly",
            BuiltinSeries::TimeframeIsintraday => "timeframe.isintraday",
            BuiltinSeries::TimeframeIsminutes => "timeframe.isminutes",
            BuiltinSeries::TimeframeIsseconds => "timeframe.isseconds",
            BuiltinSeries::TimeframeIsticks => "timeframe.isticks",
            BuiltinSeries::SyminfoTicker => "syminfo.ticker",
            BuiltinSeries::SyminfoTickerid => "syminfo.tickerid",
            BuiltinSeries::SyminfoBasecurrency => "syminfo.basecurrency",
            BuiltinSeries::SyminfoCurrency => "syminfo.currency",
            BuiltinSeries::SyminfoDescription => "syminfo.description",
            BuiltinSeries::SyminfoCountry => "syminfo.country",
            BuiltinSeries::SyminfoIndustry => "syminfo.industry",
            BuiltinSeries::SyminfoSector => "syminfo.sector",
            BuiltinSeries::SyminfoType => "syminfo.type",
            BuiltinSeries::SyminfoRoot => "syminfo.root",
            BuiltinSeries::SyminfoPrefix => "syminfo.prefix",
            BuiltinSeries::SyminfoSession => "syminfo.session",
            BuiltinSeries::SyminfoTimezone => "syminfo.timezone",
            BuiltinSeries::SyminfoVolumetype => "syminfo.volumetype",
            BuiltinSeries::SyminfoMainTickerid => "syminfo.main_tickerid",
            BuiltinSeries::SyminfoIsin => "syminfo.isin",
            BuiltinSeries::SyminfoCurrentContract => "syminfo.current_contract",
            BuiltinSeries::SyminfoMintick => "syminfo.mintick",
            BuiltinSeries::SyminfoMinmove => "syminfo.minmove",
            BuiltinSeries::SyminfoPricescale => "syminfo.pricescale",
            BuiltinSeries::SyminfoPointvalue => "syminfo.pointvalue",
            BuiltinSeries::SyminfoMincontract => "syminfo.mincontract",
            BuiltinSeries::SessionIsfirstbar => "session.isfirstbar",
            BuiltinSeries::SessionIsfirstbarRegular => "session.isfirstbar_regular",
            BuiltinSeries::SessionIslastbar => "session.islastbar",
            BuiltinSeries::SessionIslastbarRegular => "session.islastbar_regular",
            BuiltinSeries::SessionIsmarket => "session.ismarket",
            BuiltinSeries::SessionIspremarket => "session.ispremarket",
            BuiltinSeries::SessionIspostmarket => "session.ispostmarket",
            BuiltinSeries::TimeTradingday => "time_tradingday",
            BuiltinSeries::Bid => "bid",
            BuiltinSeries::Ask => "ask",
            BuiltinSeries::SyminfoExpirationDate => "syminfo.expiration_date",
            BuiltinSeries::SyminfoEmployees => "syminfo.employees",
            BuiltinSeries::SyminfoShareholders => "syminfo.shareholders",
            BuiltinSeries::SyminfoSharesOutstandingFloat => "syminfo.shares_outstanding_float",
            BuiltinSeries::SyminfoSharesOutstandingTotal => "syminfo.shares_outstanding_total",
            BuiltinSeries::SyminfoRecommendationsBuy => "syminfo.recommendations_buy",
            BuiltinSeries::SyminfoRecommendationsBuyStrong => "syminfo.recommendations_buy_strong",
            BuiltinSeries::SyminfoRecommendationsDate => "syminfo.recommendations_date",
            BuiltinSeries::SyminfoRecommendationsHold => "syminfo.recommendations_hold",
            BuiltinSeries::SyminfoRecommendationsSell => "syminfo.recommendations_sell",
            BuiltinSeries::SyminfoRecommendationsSellStrong => {
                "syminfo.recommendations_sell_strong"
            }
            BuiltinSeries::SyminfoRecommendationsTotal => "syminfo.recommendations_total",
            BuiltinSeries::SyminfoTargetPriceAverage => "syminfo.target_price_average",
            BuiltinSeries::SyminfoTargetPriceDate => "syminfo.target_price_date",
            BuiltinSeries::SyminfoTargetPriceEstimates => "syminfo.target_price_estimates",
            BuiltinSeries::SyminfoTargetPriceHigh => "syminfo.target_price_high",
            BuiltinSeries::SyminfoTargetPriceLow => "syminfo.target_price_low",
            BuiltinSeries::SyminfoTargetPriceMedian => "syminfo.target_price_median",
            BuiltinSeries::ChartLeftVisibleBarTime => "chart.left_visible_bar_time",
            BuiltinSeries::ChartRightVisibleBarTime => "chart.right_visible_bar_time",
            BuiltinSeries::DividendsFutureAmount => "dividends.future_amount",
            BuiltinSeries::DividendsFutureExDate => "dividends.future_ex_date",
            BuiltinSeries::DividendsFuturePayDate => "dividends.future_pay_date",
            BuiltinSeries::EarningsFutureEps => "earnings.future_eps",
            BuiltinSeries::EarningsFuturePeriodEndTime => "earnings.future_period_end_time",
            BuiltinSeries::EarningsFutureRevenue => "earnings.future_revenue",
            BuiltinSeries::EarningsFutureTime => "earnings.future_time",
        }
    }

    /// Whether this is a `strategy.*` broker-state getter. These are the
    /// only series that require a `strategy()` declaration (E0241) and that read
    /// from the broker's per-bar snapshot rather than the market data.
    pub fn is_strategy(self) -> bool {
        matches!(
            self,
            BuiltinSeries::StrategyPositionSize
                | BuiltinSeries::StrategyPositionAvgPrice
                | BuiltinSeries::StrategyOpenProfit
                | BuiltinSeries::StrategyNetProfit
                | BuiltinSeries::StrategyGrossProfit
                | BuiltinSeries::StrategyGrossLoss
                | BuiltinSeries::StrategyEquity
                | BuiltinSeries::StrategyInitialCapital
                | BuiltinSeries::StrategyWinTrades
                | BuiltinSeries::StrategyLossTrades
                | BuiltinSeries::StrategyEvenTrades
                | BuiltinSeries::StrategyClosedTrades
                | BuiltinSeries::StrategyOpenTrades
                | BuiltinSeries::StrategyMaxDrawdown
                | BuiltinSeries::StrategyMaxRunup
                | BuiltinSeries::StrategyAvgTrade
                | BuiltinSeries::StrategyAvgWinningTrade
                | BuiltinSeries::StrategyAvgLosingTrade
                | BuiltinSeries::StrategyMaxContractsHeldAll
                | BuiltinSeries::StrategyMaxContractsHeldLong
                | BuiltinSeries::StrategyMaxContractsHeldShort
                | BuiltinSeries::StrategyAccountCurrency
                | BuiltinSeries::StrategyAvgLosingTradePercent
                | BuiltinSeries::StrategyAvgTradePercent
                | BuiltinSeries::StrategyAvgWinningTradePercent
                | BuiltinSeries::StrategyClosedTradesFirstIndex
                | BuiltinSeries::StrategyGrossLossPercent
                | BuiltinSeries::StrategyGrossProfitPercent
                | BuiltinSeries::StrategyMarginLiquidationPrice
                | BuiltinSeries::StrategyMaxDrawdownPercent
                | BuiltinSeries::StrategyMaxRunupPercent
                | BuiltinSeries::StrategyNetProfitPercent
                | BuiltinSeries::StrategyOpenProfitPercent
                | BuiltinSeries::StrategyOpenTradesCapitalHeld
                | BuiltinSeries::StrategyPositionEntryName
        )
    }

    /// Whether the series is typed `series bool` — the `barstate.*` predicates.
    /// Together with [`BuiltinSeries::is_int`] this partitions the builtin
    /// series into the three scalar types (bool / int / float) with a single
    /// source of truth for the resolver and the CSIL validator.
    pub fn is_bool(self) -> bool {
        matches!(
            self,
            BuiltinSeries::BarstateIsfirst
                | BuiltinSeries::BarstateIslast
                | BuiltinSeries::BarstateIshistory
                | BuiltinSeries::BarstateIsrealtime
                | BuiltinSeries::BarstateIsconfirmed
                | BuiltinSeries::BarstateIsnew
                | BuiltinSeries::BarstateIslastconfirmedhistory
                // timeframe.* predicates
                | BuiltinSeries::TimeframeIsdwm
                | BuiltinSeries::TimeframeIsdaily
                | BuiltinSeries::TimeframeIsweekly
                | BuiltinSeries::TimeframeIsmonthly
                | BuiltinSeries::TimeframeIsintraday
                | BuiltinSeries::TimeframeIsminutes
                | BuiltinSeries::TimeframeIsseconds
                | BuiltinSeries::TimeframeIsticks
                // session.* predicates
                | BuiltinSeries::SessionIsfirstbar
                | BuiltinSeries::SessionIsfirstbarRegular
                | BuiltinSeries::SessionIslastbar
                | BuiltinSeries::SessionIslastbarRegular
                | BuiltinSeries::SessionIsmarket
                | BuiltinSeries::SessionIspremarket
                | BuiltinSeries::SessionIspostmarket
        )
    }

    /// Whether the series is typed `string` — the `timeframe.period`-style and
    /// `syminfo.*` string context accessors. These read a per-run
    /// constant from the chart timeframe / symbol metadata and hold the same
    /// value on every bar.
    pub fn is_string(self) -> bool {
        matches!(
            self,
            BuiltinSeries::StrategyAccountCurrency
                | BuiltinSeries::StrategyPositionEntryName
                | BuiltinSeries::TimeframePeriod
                | BuiltinSeries::TimeframeMainPeriod
                | BuiltinSeries::SyminfoTicker
                | BuiltinSeries::SyminfoTickerid
                | BuiltinSeries::SyminfoBasecurrency
                | BuiltinSeries::SyminfoCurrency
                | BuiltinSeries::SyminfoDescription
                | BuiltinSeries::SyminfoCountry
                | BuiltinSeries::SyminfoIndustry
                | BuiltinSeries::SyminfoSector
                | BuiltinSeries::SyminfoType
                | BuiltinSeries::SyminfoRoot
                | BuiltinSeries::SyminfoPrefix
                | BuiltinSeries::SyminfoSession
                | BuiltinSeries::SyminfoTimezone
                | BuiltinSeries::SyminfoVolumetype
                | BuiltinSeries::SyminfoMainTickerid
                | BuiltinSeries::SyminfoIsin
                | BuiltinSeries::SyminfoCurrentContract
        )
    }

    /// Chart viewport bounds are inputs to a chart execution.
    pub fn is_input(self) -> bool {
        matches!(
            self,
            Self::ChartLeftVisibleBarTime | Self::ChartRightVisibleBarTime
        )
    }

    /// Whether this builtin is bar-invariant and therefore `simple` rather
    /// than `series` in Pine's qualifier lattice. A history read still
    /// promotes the expression to `series` in semantic analysis.
    pub fn is_simple(self) -> bool {
        matches!(
            self,
            BuiltinSeries::StrategyAccountCurrency
                | BuiltinSeries::SyminfoTicker
                | BuiltinSeries::SyminfoTickerid
                | BuiltinSeries::SyminfoMincontract
                | BuiltinSeries::SyminfoMainTickerid
                | BuiltinSeries::SyminfoIsin
                | BuiltinSeries::SyminfoCurrentContract
                | BuiltinSeries::SyminfoExpirationDate
                | BuiltinSeries::SyminfoMinmove
                | BuiltinSeries::SyminfoPricescale
                | BuiltinSeries::SyminfoRoot
                | BuiltinSeries::SyminfoPrefix
                | BuiltinSeries::SyminfoMintick
                | BuiltinSeries::SyminfoPointvalue
                | BuiltinSeries::SyminfoSession
                | BuiltinSeries::SyminfoTimezone
                | BuiltinSeries::SyminfoDescription
                | BuiltinSeries::SyminfoCurrency
                | BuiltinSeries::SyminfoBasecurrency
                | BuiltinSeries::SyminfoType
                | BuiltinSeries::SyminfoSector
                | BuiltinSeries::SyminfoIndustry
                | BuiltinSeries::SyminfoCountry
                | BuiltinSeries::SyminfoVolumetype
                | BuiltinSeries::SyminfoEmployees
                | BuiltinSeries::SyminfoShareholders
                | BuiltinSeries::SyminfoSharesOutstandingFloat
                | BuiltinSeries::SyminfoSharesOutstandingTotal
                | BuiltinSeries::TimeframePeriod
                | BuiltinSeries::TimeframeMainPeriod
                | BuiltinSeries::TimeframeMultiplier
                | BuiltinSeries::TimeframeIsdwm
                | BuiltinSeries::TimeframeIsdaily
                | BuiltinSeries::TimeframeIsweekly
                | BuiltinSeries::TimeframeIsmonthly
                | BuiltinSeries::TimeframeIsintraday
                | BuiltinSeries::TimeframeIsminutes
                | BuiltinSeries::TimeframeIsseconds
                | BuiltinSeries::TimeframeIsticks
        )
    }

    /// Whether the series is typed `series int` (`bar_index` and the
    /// epoch-millisecond / bar-index time builtins) rather than
    /// `series float` (OHLCV and the derived averages). Drives both the
    /// resolver's type assignment and the CSIL validator's abstract typing so
    /// the int/float distinction has a single source of truth.
    pub fn is_int(self) -> bool {
        matches!(
            self,
            BuiltinSeries::BarIndex
                | BuiltinSeries::Time
                | BuiltinSeries::TimeClose
                | BuiltinSeries::Timenow
                | BuiltinSeries::LastBarIndex
                | BuiltinSeries::LastBarTime
                | BuiltinSeries::DayOfWeek
                | BuiltinSeries::DayOfMonth
                | BuiltinSeries::Month
                | BuiltinSeries::Year
                | BuiltinSeries::Hour
                | BuiltinSeries::Minute
                | BuiltinSeries::Second
                | BuiltinSeries::WeekOfYear
                // Strategy trade-count getters are `series int`; the monetary /
                // size getters are `series float`.
                | BuiltinSeries::StrategyWinTrades
                | BuiltinSeries::StrategyLossTrades
                | BuiltinSeries::StrategyEvenTrades
                | BuiltinSeries::StrategyClosedTrades
                | BuiltinSeries::StrategyOpenTrades
                | BuiltinSeries::StrategyClosedTradesFirstIndex
                // timeframe.* / syminfo.* / session.* int accessors
                | BuiltinSeries::TimeframeMultiplier
                | BuiltinSeries::SyminfoMinmove
                | BuiltinSeries::SyminfoPricescale
                | BuiltinSeries::TimeTradingday
                // Epoch-ms / expiration int accessors
                | BuiltinSeries::SyminfoExpirationDate
                | BuiltinSeries::ChartLeftVisibleBarTime
                | BuiltinSeries::ChartRightVisibleBarTime
                // SYMINFO fundamental integer counts and epoch-ms dates. The
                // share-count and target-price fields below are `series float`
                // (see the resolver's Float default), not listed here.
                | BuiltinSeries::SyminfoEmployees
                | BuiltinSeries::SyminfoShareholders
                | BuiltinSeries::SyminfoSharesOutstandingTotal
                | BuiltinSeries::SyminfoRecommendationsBuy
                | BuiltinSeries::SyminfoRecommendationsBuyStrong
                | BuiltinSeries::SyminfoRecommendationsDate
                | BuiltinSeries::SyminfoRecommendationsHold
                | BuiltinSeries::SyminfoRecommendationsSell
                | BuiltinSeries::SyminfoRecommendationsSellStrong
                | BuiltinSeries::SyminfoRecommendationsTotal
                | BuiltinSeries::SyminfoTargetPriceDate
                // Upcoming-events epoch-ms dates / times are `series int`; the
                // `future_amount`/`future_eps`/`future_revenue` values below are
                // `series float` (see the resolver's Float default).
                | BuiltinSeries::DividendsFutureExDate
                | BuiltinSeries::DividendsFuturePayDate
                | BuiltinSeries::EarningsFuturePeriodEndTime
                | BuiltinSeries::EarningsFutureTime
        )
    }

    /// The type a read of this builtin produces. `bar_index` and the
    /// epoch/bar-index time builtins are `int`, the `barstate.*` predicates are
    /// `bool`, the context names are `string`, and the OHLCV series and the
    /// derived averages (`hl2`, ...) are `float`. The resolver, default-argument
    /// materialization and language tooling all type builtin reads with this.
    pub fn value_type(self) -> Type {
        if self.is_bool() {
            Type::Bool
        } else if self.is_int() {
            Type::Int
        } else if self.is_string() {
            Type::String
        } else {
            Type::Float
        }
    }

    /// The qualifier of a current-bar read (history offset 0). A history read
    /// always promotes the expression to `series`.
    pub fn current_read_qualifier(self) -> Qualifier {
        if self.is_input() {
            Qualifier::Input
        } else if self.is_simple() {
            Qualifier::Simple
        } else {
            Qualifier::Series
        }
    }

    /// Whether this series is a plain OHLCV/derived **price** series — the only
    /// sources the higher-timeframe secondary context can resample for
    /// `request.security`/`request.security_lower_tf`. This is the exact
    /// set the VM's `htf_source_value` evaluates; the analyzer uses it to reject
    /// every other source (e.g. `time`, `syminfo.mintick`, `bid`,
    /// `dividends.future_amount`) with `E0244`/`E0246` instead of silently
    /// resampling them to `na`. Keep it in sync with `htf_source_value`.
    ///
    /// Note this is NARROWER than `!is_int() && !is_bool() && !is_string() &&
    /// !is_strategy()`: many non-price series are also `series float` (host-or-
    /// default fundamentals, `bid`/`ask`, the upcoming-event feed) yet are not
    /// valid higher-timeframe sources.
    pub fn is_price_source(self) -> bool {
        matches!(
            self,
            BuiltinSeries::Open
                | BuiltinSeries::High
                | BuiltinSeries::Low
                | BuiltinSeries::Close
                | BuiltinSeries::Volume
                | BuiltinSeries::Hl2
                | BuiltinSeries::Hlc3
                | BuiltinSeries::Ohlc4
                | BuiltinSeries::Hlcc4
        )
    }

    /// Complete built-in-series registry in the compiler-defined iteration
    /// order.
    pub const ALL: [BuiltinSeries; 135] = [
        BuiltinSeries::Open,
        BuiltinSeries::High,
        BuiltinSeries::Low,
        BuiltinSeries::Close,
        BuiltinSeries::Volume,
        BuiltinSeries::BarIndex,
        BuiltinSeries::Hl2,
        BuiltinSeries::Hlc3,
        BuiltinSeries::Ohlc4,
        BuiltinSeries::Hlcc4,
        BuiltinSeries::Time,
        BuiltinSeries::TimeClose,
        BuiltinSeries::Timenow,
        BuiltinSeries::LastBarIndex,
        BuiltinSeries::LastBarTime,
        BuiltinSeries::DayOfWeek,
        BuiltinSeries::DayOfMonth,
        BuiltinSeries::Month,
        BuiltinSeries::Year,
        BuiltinSeries::Hour,
        BuiltinSeries::Minute,
        BuiltinSeries::Second,
        BuiltinSeries::WeekOfYear,
        BuiltinSeries::BarstateIsfirst,
        BuiltinSeries::BarstateIslast,
        BuiltinSeries::BarstateIshistory,
        BuiltinSeries::BarstateIsrealtime,
        BuiltinSeries::BarstateIsconfirmed,
        BuiltinSeries::BarstateIsnew,
        BuiltinSeries::BarstateIslastconfirmedhistory,
        BuiltinSeries::StrategyPositionSize,
        BuiltinSeries::StrategyPositionAvgPrice,
        BuiltinSeries::StrategyOpenProfit,
        BuiltinSeries::StrategyNetProfit,
        BuiltinSeries::StrategyGrossProfit,
        BuiltinSeries::StrategyGrossLoss,
        BuiltinSeries::StrategyEquity,
        BuiltinSeries::StrategyInitialCapital,
        BuiltinSeries::StrategyWinTrades,
        BuiltinSeries::StrategyLossTrades,
        BuiltinSeries::StrategyEvenTrades,
        BuiltinSeries::StrategyClosedTrades,
        BuiltinSeries::StrategyOpenTrades,
        BuiltinSeries::StrategyMaxDrawdown,
        BuiltinSeries::StrategyMaxRunup,
        BuiltinSeries::StrategyAvgTrade,
        BuiltinSeries::StrategyAvgWinningTrade,
        BuiltinSeries::StrategyAvgLosingTrade,
        BuiltinSeries::StrategyMaxContractsHeldAll,
        BuiltinSeries::StrategyMaxContractsHeldLong,
        BuiltinSeries::StrategyMaxContractsHeldShort,
        BuiltinSeries::StrategyAccountCurrency,
        BuiltinSeries::StrategyAvgLosingTradePercent,
        BuiltinSeries::StrategyAvgTradePercent,
        BuiltinSeries::StrategyAvgWinningTradePercent,
        BuiltinSeries::StrategyClosedTradesFirstIndex,
        BuiltinSeries::StrategyGrossLossPercent,
        BuiltinSeries::StrategyGrossProfitPercent,
        BuiltinSeries::StrategyMarginLiquidationPrice,
        BuiltinSeries::StrategyMaxDrawdownPercent,
        BuiltinSeries::StrategyMaxRunupPercent,
        BuiltinSeries::StrategyNetProfitPercent,
        BuiltinSeries::StrategyOpenProfitPercent,
        BuiltinSeries::StrategyOpenTradesCapitalHeld,
        BuiltinSeries::StrategyPositionEntryName,
        BuiltinSeries::TimeframePeriod,
        BuiltinSeries::TimeframeMultiplier,
        BuiltinSeries::TimeframeMainPeriod,
        BuiltinSeries::TimeframeIsdwm,
        BuiltinSeries::TimeframeIsdaily,
        BuiltinSeries::TimeframeIsweekly,
        BuiltinSeries::TimeframeIsmonthly,
        BuiltinSeries::TimeframeIsintraday,
        BuiltinSeries::TimeframeIsminutes,
        BuiltinSeries::TimeframeIsseconds,
        BuiltinSeries::TimeframeIsticks,
        BuiltinSeries::SyminfoTicker,
        BuiltinSeries::SyminfoTickerid,
        BuiltinSeries::SyminfoBasecurrency,
        BuiltinSeries::SyminfoCurrency,
        BuiltinSeries::SyminfoDescription,
        BuiltinSeries::SyminfoCountry,
        BuiltinSeries::SyminfoIndustry,
        BuiltinSeries::SyminfoSector,
        BuiltinSeries::SyminfoType,
        BuiltinSeries::SyminfoRoot,
        BuiltinSeries::SyminfoPrefix,
        BuiltinSeries::SyminfoSession,
        BuiltinSeries::SyminfoTimezone,
        BuiltinSeries::SyminfoVolumetype,
        BuiltinSeries::SyminfoMainTickerid,
        BuiltinSeries::SyminfoMintick,
        BuiltinSeries::SyminfoMinmove,
        BuiltinSeries::SyminfoPricescale,
        BuiltinSeries::SyminfoPointvalue,
        BuiltinSeries::SyminfoMincontract,
        BuiltinSeries::SessionIsfirstbar,
        BuiltinSeries::SessionIsfirstbarRegular,
        BuiltinSeries::SessionIslastbar,
        BuiltinSeries::SessionIslastbarRegular,
        BuiltinSeries::SessionIsmarket,
        BuiltinSeries::SessionIspremarket,
        BuiltinSeries::SessionIspostmarket,
        BuiltinSeries::TimeTradingday,
        BuiltinSeries::Bid,
        BuiltinSeries::Ask,
        BuiltinSeries::SyminfoExpirationDate,
        BuiltinSeries::SyminfoEmployees,
        BuiltinSeries::SyminfoShareholders,
        BuiltinSeries::SyminfoSharesOutstandingFloat,
        BuiltinSeries::SyminfoSharesOutstandingTotal,
        BuiltinSeries::SyminfoRecommendationsBuy,
        BuiltinSeries::SyminfoRecommendationsBuyStrong,
        BuiltinSeries::SyminfoRecommendationsDate,
        BuiltinSeries::SyminfoRecommendationsHold,
        BuiltinSeries::SyminfoRecommendationsSell,
        BuiltinSeries::SyminfoRecommendationsSellStrong,
        BuiltinSeries::SyminfoRecommendationsTotal,
        BuiltinSeries::SyminfoTargetPriceAverage,
        BuiltinSeries::SyminfoTargetPriceDate,
        BuiltinSeries::SyminfoTargetPriceEstimates,
        BuiltinSeries::SyminfoTargetPriceHigh,
        BuiltinSeries::SyminfoTargetPriceLow,
        BuiltinSeries::SyminfoTargetPriceMedian,
        BuiltinSeries::ChartLeftVisibleBarTime,
        BuiltinSeries::ChartRightVisibleBarTime,
        BuiltinSeries::DividendsFutureAmount,
        BuiltinSeries::DividendsFutureExDate,
        BuiltinSeries::DividendsFuturePayDate,
        BuiltinSeries::EarningsFutureEps,
        BuiltinSeries::EarningsFuturePeriodEndTime,
        BuiltinSeries::EarningsFutureRevenue,
        BuiltinSeries::EarningsFutureTime,
        BuiltinSeries::SyminfoIsin,
        BuiltinSeries::SyminfoCurrentContract,
    ];
}

/// The engine-backed stateful builtin tags, re-exported from
/// [`pinewasm_ta`] — the crate that also owns the engine executing
/// them, so the front end, the CSIL validator, the reference interpreter,
/// and the copy compiled into every artifact all key off one definition.
pub use pinewasm_ta::{TaFunc, TaOperand, TaParamRule, TaResult};

/// One engine-backed stateful call site: the function tag plus its
/// compile-time const parameters (window lengths, `ta.alma`'s offset/sigma
/// as `f64` bit patterns, `ta.tr`'s `handle_na` as 0/1, ...). Declared once
/// per syntactic call site, in syntactic order; the site's `CALL_TA { state }`
/// instruction indexes this table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaStateDecl {
    pub func: TaFunc,
    /// Const parameters. Semantically these are plain `i64`, but some entries
    /// are `f64` bit patterns (`FloatBits`/`Percent` rules — e.g. `ta.bb`'s
    /// mult, `ta.alma`'s offset/sigma). A bit pattern like `2.0f64.to_bits()`
    /// is `2^62`, far above JavaScript's `Number.MAX_SAFE_INTEGER` (`2^53`),
    /// so serializing it as a JSON number breaks JS hosts (`serde_wasm_bindgen`
    /// rejects it outright). The wire form is therefore an array of decimal
    /// strings; deserialization also accepts bare numbers so older
    /// serialized programs still load. The values are opaque to hosts (the
    /// program manifest documents only `inputs`/`metadata` as host-readable).
    #[serde(with = "ta_params_serde")]
    pub params: Vec<i64>,
}

/// A run-constant numeric input used by a TA call. Integer parameters carry
/// their value directly; float parameters carry exact `f64` bit patterns. The
/// instruction/declaration retains its default until a fresh run resolves the
/// host input and specializes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaInputBinding {
    /// Index of the [`crate::InputDecl`] whose value supplies the parameter.
    pub input: u32,
    /// Whether to negate the resolved numeric value before checking its rule.
    pub negated: bool,
    /// Instruction slot or state parameter specialized for the fresh run.
    pub target: TaInputTarget,
}

/// Slot ids survive lowering unchanged; engine parameters use declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaInputTarget {
    /// Window length encoded by a simple-moving-average instruction.
    Sma {
        /// Series slot containing the moving average's state.
        slot: u32,
    },
    /// Window length encoded by a change instruction.
    Change {
        /// Series slot containing the change operation's state.
        slot: u32,
    },
    /// Numeric parameter stored in a stateful technical-analysis declaration.
    State {
        /// Index of the technical-analysis state declaration.
        state: u32,
        /// Index of the declaration parameter to specialize.
        param: u32,
    },
}

/// Wire codec for [`TaStateDecl::params`]: emit each `i64` as a decimal string
/// (JS-number-safe for `f64`-bit-pattern params), and on read accept either a
/// string or a bare number. See the field doc for why.
mod ta_params_serde {
    use serde::de::{Deserializer, Error, SeqAccess, Visitor};
    use serde::ser::{SerializeSeq, Serializer};
    use std::fmt;

    pub fn serialize<S: Serializer>(params: &[i64], serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(params.len()))?;
        for p in params {
            seq.serialize_element(&p.to_string())?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<i64>, D::Error> {
        struct ParamsVisitor;

        impl<'de> Visitor<'de> for ParamsVisitor {
            type Value = Vec<i64>;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a sequence of i64 params as decimal strings (or bare numbers)")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<i64>, A::Error> {
                // Each element is a decimal string (current form) or a bare
                // integer.
                #[derive(serde::Deserialize)]
                #[serde(untagged)]
                enum Param {
                    Int(i64),
                    Str(String),
                }

                let mut out = Vec::new();
                while let Some(p) = seq.next_element::<Param>()? {
                    out.push(match p {
                        Param::Int(v) => v,
                        Param::Str(s) => s.parse::<i64>().map_err(A::Error::custom)?,
                    });
                }
                Ok(out)
            }
        }

        deserializer.deserialize_seq(ParamsVisitor)
    }
}

/// Pure (stateless) built-in functions: same arguments in, same value out,
/// no hidden slots, no bar-to-bar state. Backed by the single
/// `CALL_PURE { func, argc }` opcode; the VM dispatches on this tag.
///
/// All variants are na-propagating (any `na` argument produces an `na`
/// result) except [`PureFunc::Na`] and [`PureFunc::Nz`], whose whole
/// purpose is to inspect/replace `na`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PureFunc {
    /// `na(x)` — true iff `x` is `na`; never `na` itself.
    Na,
    /// `nz(x, replacement)` — `x` with `na` replaced. At the CSIL level the
    /// replacement operand is always present: the compiler materializes the
    /// typed default (`0`, `0.0`, `false`) when the source form omits it.
    Nz,
    /// `math.abs(x)` — int → int (checked; `abs(i64::MIN)` is na), float → float.
    MathAbs,
    /// `math.sign(x)` — -1/0/1 preserving the numeric type.
    MathSign,
    MathSqrt,
    MathPow,
    MathExp,
    /// Natural (base-e) logarithm.
    MathLog,
    MathLog10,
    MathSin,
    MathCos,
    MathTan,
    MathAsin,
    MathAcos,
    MathAtan,
    /// `math.floor(x)` — int result (identity on int input).
    MathFloor,
    /// `math.ceil(x)` — int result (identity on int input).
    MathCeil,
    /// `math.round(x)` — int result, half away from zero;
    /// `math.round(x, precision)` — float result.
    MathRound,
    /// Variadic (2..=10); int when every argument is int.
    MathMin,
    /// Variadic (2..=10); int when every argument is int.
    MathMax,
    /// Variadic (2..=10); always float.
    MathAvg,
    MathToDegrees,
    MathToRadians,
    /// `math.random` — at the CSIL level `min` and `max` are
    /// always present (the compiler materializes the 0.0/1.0 defaults).
    MathRandom,
    /// `math.round_to_mintick(x)` — rounds to a multiple of the market
    /// data's `mintick`.
    MathRoundToMintick,

    // ---- str.* namespace -------------------------------------------------
    // All are na-propagating: any `na` argument yields `na`, including the
    // predicate and int-returning functions.
    /// `str.tostring(value)` / `str.tostring(value, format)` — value is
    /// int/float/bool/string; the optional second argument is a const format
    /// string. Returns a string.
    StrTostring,
    /// `str.tonumber(string)` — parse a numeric string to a float, or `na`
    /// when it does not match the numeric grammar.
    StrTonumber,
    /// `str.format(formatString, arg0, ...)` — `{N}` placeholder formatting.
    /// The format string is a const; args are int/float/bool/string.
    StrFormat,
    /// `str.length(string)` — number of Unicode code points (not bytes).
    StrLength,
    /// `str.substring(source, begin)` / `str.substring(source, begin, end)` —
    /// code-point slice, `begin` inclusive, `end` exclusive (default: to the
    /// end of the string).
    StrSubstring,
    /// `str.replace` — replace the nth non-overlapping occurrence of `target`;
    /// the compiler supplies the occurrence default when it is omitted.
    StrReplace,
    /// `str.replace_all(source, target, replacement)` — replace every
    /// non-overlapping occurrence.
    StrReplaceAll,
    /// `str.contains(source, str)` — bool.
    StrContains,
    /// `str.startswith(source, str)` — bool.
    StrStartswith,
    /// `str.endswith(source, str)` — bool.
    StrEndswith,
    /// `str.pos(source, str)` — code-point index of the first occurrence, or
    /// -1 if absent (int).
    StrPos,
    /// `str.upper(source)` — ASCII uppercase (non-ASCII letters unchanged).
    StrUpper,
    /// `str.lower(source)` — ASCII lowercase (non-ASCII letters unchanged).
    StrLower,
    /// `str.trim(source)` — strip leading/trailing spaces, tabs, and newlines.
    StrTrim,
    /// `str.repeat(source, repeat)` / `str.repeat(source, repeat, separator)`
    /// — repeat `repeat` times joined by `separator` (default empty).
    StrRepeat,
    /// `str.match(string, pattern)` — the first regex-matched substring, or
    /// `""` when it does not match. Invalid/evaluation-failing patterns are
    /// structured runtime errors.
    StrMatch,
    /// `str.format_time` — format an epoch-ms `time` int into a date/time
    /// string. The 2-argument form (no `timezone`) uses `syminfo.timezone` at
    /// runtime. Supports the common tokens (`yyyy`/`MM`/`dd`/`HH`/`mm`/`ss`);
    /// unsupported tokens pass through literally.
    StrFormatTime,

    // ---- color.* namespace -----------------------------------------------
    // Runtime color constructors/accessors. All produce or consume the
    // distinct `Color` value type (`Value::Color { r, g, b, t }`, transparency
    // `t` in `0..=100`). na-propagating on their color argument.
    /// `color.new(color, transp)` — the color with transparency replaced by
    /// `transp` (`0..=100`, clamped). Returns a color.
    ColorNew,
    /// `color.rgb` — a color from 0..=255 RGB
    /// channels (clamped) and 0..=100 transparency (clamped). Returns a color.
    ColorRgb,
    /// `color.from_gradient(value, bottom_value, top_value, bottom_color,
    /// top_color)` — linear interpolation between the two colors (all four
    /// RGBA channels) by `value`'s position in `[bottom_value, top_value]`
    /// (clamped to the endpoints outside the range). Returns a color.
    ColorFromGradient,
    /// Integer accessor kept for serialized programs; new source uses `ColorRFloat`.
    ColorR,
    /// Integer accessor kept for serialized programs; new source uses `ColorGFloat`.
    ColorG,
    /// Integer accessor kept for serialized programs; new source uses `ColorBFloat`.
    ColorB,
    /// Integer accessor kept for serialized programs; new source uses `ColorTFloat`.
    ColorT,
    /// `color.r(color)` — red channel as a float (`0..=255`).
    ColorRFloat,
    /// `color.g(color)` — green channel as a float (`0..=255`).
    ColorGFloat,
    /// `color.b(color)` — blue channel as a float (`0..=255`).
    ColorBFloat,
    /// `color.t(color)` — transparency as a float (`0..=100`).
    ColorTFloat,

    // ---- strategy trade accessors ----------------------------------------
    // Each reads one field of a closed or open trade at a runtime int index off
    // the broker's retained trade list. Only valid in a `strategy()` script
    // (E0241). An out-of-range index (or a mismatched account state) reads `na`;
    // never a panic. The [`TradeAccessor`] payload names the field.
    /// `strategy.closedtrades.<field>(trade_num)` — reads a closed trade.
    StrategyClosedTrade(TradeAccessor),
    /// `strategy.opentrades.<field>(trade_num)` — reads an open trade.
    StrategyOpenTrade(TradeAccessor),

    // ---- account currency conversions ------------------------------------
    /// `strategy.convert_to_account(value)` — convert a value expressed in the
    /// symbol's currency to the account currency. Identity unless the host
    /// supplies a rate (mirrors `request.currency_rate`). Only valid in a
    /// `strategy()` script (E0241).
    StrategyConvertToAccount,
    /// `strategy.convert_to_symbol(value)` — the inverse conversion.
    StrategyConvertToSymbol,
    /// `strategy.default_entry_qty(fill_price)` — estimate the declaration's
    /// default order quantity in contracts at a hypothetical fill price.
    StrategyDefaultEntryQty,

    // ---- timeframe.* functions -------------------------------------------
    /// `timeframe.in_seconds(timeframe)` — the length of a timeframe string
    /// in seconds. Returns `int`. An omitted argument reads the chart timeframe.
    TimeframeInSeconds,
    /// `timeframe.from_seconds(seconds)` — the canonical timeframe string
    /// for a length in seconds. Returns `string`.
    TimeframeFromSeconds,
    /// `time` on a requested timeframe — the opening timestamp of the bar on
    /// that timeframe after applying the
    /// chart-timeframe and requested-timeframe offsets, then filtering by the
    /// optional time-based session. The analyzer materializes all optional
    /// operands so the CSIL call shape is fixed.
    TimeOpen,
    /// `time_close` — the closing timestamp of the bar on the requested
    /// timeframe. Same fixed CSIL shape as [`PureFunc::TimeOpen`].
    TimeClose,

    // ---- timestamp() -----------------------------------------------------
    /// `timestamp` — the
    /// UNIX time in **milliseconds** of the given UTC calendar date/time,
    /// computed by civil-date math. Returns `int`. The leading-`timezone`
    /// overload is a separate variant [`PureFunc::TimestampTz`]; the compiler
    /// picks the variant from the first argument's type at the call site, or
    /// from the argument names in a named call. All
    /// six numeric arguments are always present at the CSIL level (the compiler
    /// materializes the `hour`/`minute`/`second` defaults).
    Timestamp,
    /// `timestamp` with a leading timezone argument, as [`PureFunc::Timestamp`]
    /// but the leading `timezone` string shifts the
    /// wall-clock fields into that fixed-offset zone before converting to
    /// UNIX ms. Returns `int`. At the CSIL level all seven arguments are present
    /// (the compiler materializes the `hour`/`minute`/`second` defaults). Only fixed-offset zones (`"UTC-5"`,
    /// `"GMT+05:30"`) resolve; IANA names fall back to UTC.
    TimestampTz,

    // ---- type-cast functions ---------------------------------------------
    // The scalar type-cast builtins. Each is na-propagating (a `na`
    // argument yields `na`) except `CastBool`, which maps `na` to `false`.
    /// `int(x)` — truncate a float toward zero to an int; `na → na`. An int
    /// argument passes through.
    CastInt,
    /// `float(x)` — widen an int to a float; `na → na`. A float passes through.
    CastFloat,
    /// `bool(x)` — `false` when `x` is `na`, `false`, or a numeric `0`; `true`
    /// otherwise. A bool passes through unchanged. (Note: NOT na-propagating.)
    CastBool,
    /// `color(x)` — identity on a color; `na → na`.
    CastColor,

    // ---- weekofyear() ----------------------------------------------------
    /// `weekofyear` — the ISO 8601
    /// week-of-year (1..=53) for a UNIX-ms `time`, decomposed in the given
    /// fixed-offset timezone (default `syminfo.timezone`, applied by the VM
    /// when omitted). Returns `int`; `na → na`. The optional `timezone` is
    /// present at the CSIL level only when supplied (no default materialization),
    /// mirroring `str.format_time`.
    WeekOfYear,

    /// A calendar-component function form (`year`, `month`, `dayofmonth`,
    /// `dayofweek`, `hour`, `minute`, `second`). Each decomposes a UNIX-ms
    /// `time` in the given fixed-offset timezone (default `syminfo.timezone`,
    /// applied by the VM when omitted) and returns the named component as `int`; `na → na`.
    /// The [`CalendarField`] payload names which component, so the whole family
    /// shares a single `PureFunc` variant — mirroring `WeekOfYear`. The bare
    /// `year`/`month`/... *variables* are separate `BuiltinSeries`.
    Calendar(CalendarField),

    // ---- ticker.* interface functions ------------------------------------
    // Each returns a **deterministic canonical ticker-id string** built from
    // its arguments (a canonical form; real symbology resolution is a host
    // concern). All arguments are `simple`/const strings
    // or numbers; na propagates to na.
    /// A `ticker.*` interface function. The [`TickerFunc`] payload names which
    /// one, so the whole family shares a single `PureFunc` variant.
    Ticker(TickerFunc),
    /// `syminfo.prefix(symbol)`: the exchange prefix of a ticker id (the part
    /// before the first `:`), or `na` when the id carries none. The bare
    /// `syminfo.prefix` variable is unrelated and resolves as a constant.
    SyminfoPrefix,
    /// `syminfo.ticker(symbol)`: the symbol name of a ticker id (the part
    /// after the first `:`), or the whole id when it carries no prefix.
    SyminfoTicker,
}

/// Which `ticker.*` interface function a [`PureFunc::Ticker`] is. Each folds
/// its arguments into a deterministic canonical ticker-id string; the exact
/// argument surface per function is enforced by the registry signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TickerFunc {
    /// `ticker.new` — a canonical ticker id built from the prefix, the ticker
    /// and the optional modifiers.
    New,
    /// `ticker.modify` — a canonical ticker id derived from an existing id and
    /// the optional modifiers.
    Modify,
    /// `ticker.heikinashi(symbol)`.
    Heikinashi,
    /// `ticker.kagi` — the Kagi form of `symbol`, selected by its reversal or
    /// param and style arguments.
    Kagi,
    /// `ticker.linebreak(symbol, number_of_lines)`.
    Linebreak,
    /// `ticker.pointfigure(symbol, source, style, param, reversal)`.
    Pointfigure,
    /// `ticker.renko` — the Renko form of `symbol`, with optional wick and
    /// source arguments.
    Renko,
    /// `ticker.standard` — the standard form of `symbol`; the symbol is optional.
    Standard,
    /// `ticker.inherit(from_tickerid, symbol)`.
    Inherit,
}

impl TickerFunc {
    /// The builtin name (`ticker.heikinashi`, ...).
    pub fn name(self) -> &'static str {
        match self {
            TickerFunc::New => "ticker.new",
            TickerFunc::Modify => "ticker.modify",
            TickerFunc::Heikinashi => "ticker.heikinashi",
            TickerFunc::Kagi => "ticker.kagi",
            TickerFunc::Linebreak => "ticker.linebreak",
            TickerFunc::Pointfigure => "ticker.pointfigure",
            TickerFunc::Renko => "ticker.renko",
            TickerFunc::Standard => "ticker.standard",
            TickerFunc::Inherit => "ticker.inherit",
        }
    }

    /// The canonical prefix token used to build the deterministic ticker-id
    /// string (`heikinashi:<symbol>`, ...). `New`/`Modify`/`Inherit` have their
    /// own construction and do not use this.
    pub fn canonical_prefix(self) -> &'static str {
        match self {
            TickerFunc::New => "new",
            TickerFunc::Modify => "modify",
            TickerFunc::Heikinashi => "heikinashi",
            TickerFunc::Kagi => "kagi",
            TickerFunc::Linebreak => "linebreak",
            TickerFunc::Pointfigure => "pointfigure",
            TickerFunc::Renko => "renko",
            TickerFunc::Standard => "standard",
            TickerFunc::Inherit => "inherit",
        }
    }
}

/// Which calendar component a [`PureFunc::Calendar`] function form extracts from
/// a decomposed UNIX-ms time. A single `Copy` tag so the seven calendar function
/// forms share one `PureFunc` variant. The `weekofyear` function form is kept
/// separate as [`PureFunc::WeekOfYear`] (it returns an ISO week, not a raw civil
/// component).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CalendarField {
    /// `year`.
    Year,
    /// `month` — 1..=12.
    Month,
    /// `dayofmonth` — 1..=31.
    DayOfMonth,
    /// `dayofweek` — 1 = Sunday .. 7 = Saturday.
    DayOfWeek,
    /// `hour` — 0..=23.
    Hour,
    /// `minute` — 0..=59.
    Minute,
    /// `second` — 0..=59.
    Second,
}

impl CalendarField {
    /// The builtin function name (`year`, `month`, ...).
    pub fn name(self) -> &'static str {
        match self {
            CalendarField::Year => "year",
            CalendarField::Month => "month",
            CalendarField::DayOfMonth => "dayofmonth",
            CalendarField::DayOfWeek => "dayofweek",
            CalendarField::Hour => "hour",
            CalendarField::Minute => "minute",
            CalendarField::Second => "second",
        }
    }
}

/// Which field a [`PureFunc::StrategyClosedTrade`]/[`PureFunc::StrategyOpenTrade`]
/// accessor reads off a trade. A single `Copy` tag so the accessor family shares
/// two `PureFunc` variants instead of ~26. Fields absent for open trades (exit_*)
/// are simply not registered as `strategy.opentrades.*` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TradeAccessor {
    /// Entry fill price (`series float`).
    EntryPrice,
    /// Exit fill price (`series float`; closed trades only).
    ExitPrice,
    /// Signed traded quantity: positive for a long trade, negative for a short.
    Size,
    /// Realized profit net of commission (`series float`).
    Profit,
    /// Realized profit as a percentage of entry notional (`series float`).
    ProfitPercent,
    /// Total commission paid on the trade (entry + exit share).
    Commission,
    /// Chart bar index of the entry fill (`series int`).
    EntryBarIndex,
    /// Chart bar index of the exit fill (`series int`; closed trades only).
    ExitBarIndex,
    /// UNIX time (ms) of the entry bar (`series int`).
    EntryTime,
    /// UNIX time (ms) of the exit bar (`series int`; closed trades only).
    ExitTime,
    /// Maximum favorable excursion of the trade (`series float`).
    MaxRunup,
    /// Maximum favorable excursion as a percentage of entry notional.
    MaxRunupPercent,
    /// Maximum adverse excursion of the trade (`series float`).
    MaxDrawdown,
    /// Maximum adverse excursion as a percentage of entry notional.
    MaxDrawdownPercent,
    /// The entry order id (`series string`).
    EntryId,
    /// The exit order id (`series string`; closed trades only).
    ExitId,
    /// The entry order comment (`series string`).
    EntryComment,
    /// The exit order comment (`series string`; closed trades only).
    ExitComment,
}

impl PureFunc {
    /// Whether this serialized accessor returns a float result. The four
    /// earlier integer opcodes remain readable unchanged.
    pub fn color_channel_is_float(self) -> bool {
        matches!(
            self,
            Self::ColorRFloat | Self::ColorGFloat | Self::ColorBFloat | Self::ColorTFloat
        )
    }

    pub fn name(self) -> &'static str {
        match self {
            PureFunc::Na => "na",
            PureFunc::Nz => "nz",
            PureFunc::MathAbs => "math.abs",
            PureFunc::MathSign => "math.sign",
            PureFunc::MathSqrt => "math.sqrt",
            PureFunc::MathPow => "math.pow",
            PureFunc::MathExp => "math.exp",
            PureFunc::MathLog => "math.log",
            PureFunc::MathLog10 => "math.log10",
            PureFunc::MathSin => "math.sin",
            PureFunc::MathCos => "math.cos",
            PureFunc::MathTan => "math.tan",
            PureFunc::MathAsin => "math.asin",
            PureFunc::MathAcos => "math.acos",
            PureFunc::MathAtan => "math.atan",
            PureFunc::MathFloor => "math.floor",
            PureFunc::MathCeil => "math.ceil",
            PureFunc::MathRound => "math.round",
            PureFunc::MathMin => "math.min",
            PureFunc::MathMax => "math.max",
            PureFunc::MathAvg => "math.avg",
            PureFunc::MathToDegrees => "math.todegrees",
            PureFunc::MathToRadians => "math.toradians",
            PureFunc::MathRandom => "math.random",
            PureFunc::MathRoundToMintick => "math.round_to_mintick",
            PureFunc::StrTostring => "str.tostring",
            PureFunc::StrTonumber => "str.tonumber",
            PureFunc::StrFormat => "str.format",
            PureFunc::StrLength => "str.length",
            PureFunc::StrSubstring => "str.substring",
            PureFunc::StrReplace => "str.replace",
            PureFunc::StrReplaceAll => "str.replace_all",
            PureFunc::StrContains => "str.contains",
            PureFunc::StrStartswith => "str.startswith",
            PureFunc::StrEndswith => "str.endswith",
            PureFunc::StrPos => "str.pos",
            PureFunc::StrUpper => "str.upper",
            PureFunc::StrLower => "str.lower",
            PureFunc::StrTrim => "str.trim",
            PureFunc::StrRepeat => "str.repeat",
            PureFunc::StrMatch => "str.match",
            PureFunc::StrFormatTime => "str.format_time",
            PureFunc::ColorNew => "color.new",
            PureFunc::ColorRgb => "color.rgb",
            PureFunc::ColorFromGradient => "color.from_gradient",
            PureFunc::ColorR | PureFunc::ColorRFloat => "color.r",
            PureFunc::ColorG | PureFunc::ColorGFloat => "color.g",
            PureFunc::ColorB | PureFunc::ColorBFloat => "color.b",
            PureFunc::ColorT | PureFunc::ColorTFloat => "color.t",
            PureFunc::StrategyClosedTrade(field) => field.closed_name(),
            PureFunc::StrategyOpenTrade(field) => field.open_name(),
            PureFunc::StrategyConvertToAccount => "strategy.convert_to_account",
            PureFunc::StrategyConvertToSymbol => "strategy.convert_to_symbol",
            PureFunc::StrategyDefaultEntryQty => "strategy.default_entry_qty",
            PureFunc::TimeframeInSeconds => "timeframe.in_seconds",
            PureFunc::TimeframeFromSeconds => "timeframe.from_seconds",
            PureFunc::TimeOpen => "time",
            PureFunc::TimeClose => "time_close",
            PureFunc::Timestamp | PureFunc::TimestampTz => "timestamp",
            PureFunc::CastInt => "int",
            PureFunc::CastFloat => "float",
            PureFunc::CastBool => "bool",
            PureFunc::CastColor => "color",
            PureFunc::WeekOfYear => "weekofyear",
            PureFunc::Calendar(f) => f.name(),
            PureFunc::Ticker(f) => f.name(),
            PureFunc::SyminfoPrefix => "syminfo.prefix",
            PureFunc::SyminfoTicker => "syminfo.ticker",
        }
    }

    /// Inclusive `argc` bounds at the **CSIL level** (after the compiler has
    /// materialized optional-argument defaults — see the variant docs; the
    /// surface signatures in the registry may accept fewer). The validator
    /// enforces these bounds structurally.
    pub fn argc_bounds(self) -> (u8, u8) {
        match self {
            PureFunc::Na => (1, 1),
            // The compiler always materializes nz's replacement.
            PureFunc::Nz => (2, 2),
            PureFunc::MathRound => (1, 2),
            PureFunc::MathMin | PureFunc::MathMax | PureFunc::MathAvg => (2, 10),
            PureFunc::MathPow => (2, 2),
            // min and max are always materialized; the seed stays optional.
            PureFunc::MathRandom => (2, 3),
            PureFunc::MathAbs
            | PureFunc::MathSign
            | PureFunc::MathSqrt
            | PureFunc::MathExp
            | PureFunc::MathLog
            | PureFunc::MathLog10
            | PureFunc::MathSin
            | PureFunc::MathCos
            | PureFunc::MathTan
            | PureFunc::MathAsin
            | PureFunc::MathAcos
            | PureFunc::MathAtan
            | PureFunc::MathFloor
            | PureFunc::MathCeil
            | PureFunc::MathToDegrees
            | PureFunc::MathToRadians
            | PureFunc::MathRoundToMintick => (1, 1),
            // str.* namespace.
            PureFunc::StrTonumber
            | PureFunc::StrLength
            | PureFunc::StrUpper
            | PureFunc::StrLower
            | PureFunc::StrTrim
            // The `syminfo.*` ticker-id splitters take one id string.
            | PureFunc::SyminfoPrefix
            | PureFunc::SyminfoTicker => (1, 1),
            // tostring: (value) or (value, format).
            PureFunc::StrTostring => (1, 2),
            // substring: (source, begin) or (source, begin, end).
            PureFunc::StrSubstring => (2, 3),
            PureFunc::StrContains
            | PureFunc::StrStartswith
            | PureFunc::StrEndswith
            | PureFunc::StrPos
            | PureFunc::StrMatch => (2, 2),
            // repeat: (source, repeat) or (source, repeat, separator).
            PureFunc::StrRepeat => (2, 3),
            // format_time: (time), (time, format), or (time, format, timezone).
            // The VM applies the `format` and `syminfo.timezone` defaults when
            // the respective arguments are omitted (no default materialization).
            PureFunc::StrFormatTime => (1, 3),
            // replace: the compiler materializes the occurrence default, so
            // the CSIL form is always (source, target, replacement, occurrence).
            PureFunc::StrReplace => (4, 4),
            PureFunc::StrReplaceAll => (3, 3),
            // format: a const format string plus 0..=N substitution args. The
            // upper bound mirrors the checker's cap on placeholders.
            PureFunc::StrFormat => (1, STR_FORMAT_MAX_ARGS),
            // color.* namespace.
            // new(color, transp): the transp is required (no default), so the
            // CSIL form is always two arguments.
            PureFunc::ColorNew => (2, 2),
            // rgb(red, green, blue, transp): the compiler materializes the
            // transp = 0 default, so the CSIL form is always four arguments.
            PureFunc::ColorRgb => (4, 4),
            // from_gradient(value, bottom_value, top_value, bottom_color,
            // top_color).
            PureFunc::ColorFromGradient => (5, 5),
            // r/g/b/t(color): a single color argument.
            PureFunc::ColorR | PureFunc::ColorG | PureFunc::ColorB | PureFunc::ColorT
            | PureFunc::ColorRFloat | PureFunc::ColorGFloat | PureFunc::ColorBFloat | PureFunc::ColorTFloat => (1, 1),
            // Trade accessors take a single int `trade_num`; conversions take a
            // single numeric `value`.
            PureFunc::StrategyClosedTrade(_)
            | PureFunc::StrategyOpenTrade(_)
            | PureFunc::StrategyConvertToAccount
            | PureFunc::StrategyConvertToSymbol
            | PureFunc::StrategyDefaultEntryQty => (1, 1),
            // timeframe.in_seconds(tf) / timeframe.from_seconds(seconds).
            PureFunc::TimeframeInSeconds | PureFunc::TimeframeFromSeconds => (1, 1),
            // time/time_close: the compiler materializes session="", timezone="",
            // bars_back=0, and timeframe_bars_back=0 after the required
            // timeframe string.
            PureFunc::TimeOpen | PureFunc::TimeClose => (5, 5),
            // timestamp: the compiler materializes the hour/minute/second
            // defaults, so the CSIL form is always six (UTC overload) or seven
            // (leading-timezone overload) numeric arguments.
            PureFunc::Timestamp => (6, 6),
            PureFunc::TimestampTz => (7, 7),
            // Type casts take a single argument.
            PureFunc::CastInt | PureFunc::CastFloat | PureFunc::CastBool | PureFunc::CastColor => {
                (1, 1)
            }
            // weekofyear(time) or weekofyear(time, timezone). The timezone is
            // not materialized (the VM applies the syminfo default), so the CSIL
            // form is 1 or 2 arguments.
            PureFunc::WeekOfYear => (1, 2),
            // year/month/dayofmonth/dayofweek/hour/minute/second(time[, timezone]).
            // Same shape as weekofyear: the timezone default is applied by the
            // VM (not materialized), so the CSIL form is 1 or 2 arguments.
            PureFunc::Calendar(_) => (1, 2),
            // ticker.* interface functions. Optional trailing arguments are
            // NOT materialized (the VM stringifies whatever it is given), so
            // the CSIL argc range mirrors the surface signature min..=max.
            PureFunc::Ticker(f) => match f {
                TickerFunc::Heikinashi => (1, 1),
                TickerFunc::Standard => (0, 1),
                TickerFunc::Linebreak | TickerFunc::Inherit => (2, 2),
                TickerFunc::Kagi => (2, 3),
                TickerFunc::New => (2, 6),
                TickerFunc::Modify => (1, 5),
                TickerFunc::Renko => (3, 5),
                TickerFunc::Pointfigure => (5, 5),
            },
        }
    }

    /// Whether this pure builtin reads broker state and so requires a
    /// `strategy()` declaration (E0241), like the `strategy.*` series getters.
    pub fn is_strategy(self) -> bool {
        matches!(
            self,
            PureFunc::StrategyClosedTrade(_)
                | PureFunc::StrategyOpenTrade(_)
                | PureFunc::StrategyConvertToAccount
                | PureFunc::StrategyConvertToSymbol
                | PureFunc::StrategyDefaultEntryQty
        )
    }
}

impl TradeAccessor {
    /// The `strategy.closedtrades.*` builtin name for this field.
    pub fn closed_name(self) -> &'static str {
        match self {
            TradeAccessor::EntryPrice => "strategy.closedtrades.entry_price",
            TradeAccessor::ExitPrice => "strategy.closedtrades.exit_price",
            TradeAccessor::Size => "strategy.closedtrades.size",
            TradeAccessor::Profit => "strategy.closedtrades.profit",
            TradeAccessor::ProfitPercent => "strategy.closedtrades.profit_percent",
            TradeAccessor::Commission => "strategy.closedtrades.commission",
            TradeAccessor::EntryBarIndex => "strategy.closedtrades.entry_bar_index",
            TradeAccessor::ExitBarIndex => "strategy.closedtrades.exit_bar_index",
            TradeAccessor::EntryTime => "strategy.closedtrades.entry_time",
            TradeAccessor::ExitTime => "strategy.closedtrades.exit_time",
            TradeAccessor::MaxRunup => "strategy.closedtrades.max_runup",
            TradeAccessor::MaxRunupPercent => "strategy.closedtrades.max_runup_percent",
            TradeAccessor::MaxDrawdown => "strategy.closedtrades.max_drawdown",
            TradeAccessor::MaxDrawdownPercent => "strategy.closedtrades.max_drawdown_percent",
            TradeAccessor::EntryId => "strategy.closedtrades.entry_id",
            TradeAccessor::ExitId => "strategy.closedtrades.exit_id",
            TradeAccessor::EntryComment => "strategy.closedtrades.entry_comment",
            TradeAccessor::ExitComment => "strategy.closedtrades.exit_comment",
        }
    }

    /// The `strategy.opentrades.*` builtin name for this field. Exit-only fields
    /// have no open-trade form and are never registered under these names.
    pub fn open_name(self) -> &'static str {
        match self {
            TradeAccessor::EntryPrice => "strategy.opentrades.entry_price",
            TradeAccessor::Size => "strategy.opentrades.size",
            TradeAccessor::Profit => "strategy.opentrades.profit",
            TradeAccessor::ProfitPercent => "strategy.opentrades.profit_percent",
            TradeAccessor::Commission => "strategy.opentrades.commission",
            TradeAccessor::EntryBarIndex => "strategy.opentrades.entry_bar_index",
            TradeAccessor::EntryTime => "strategy.opentrades.entry_time",
            TradeAccessor::MaxRunup => "strategy.opentrades.max_runup",
            TradeAccessor::MaxRunupPercent => "strategy.opentrades.max_runup_percent",
            TradeAccessor::MaxDrawdown => "strategy.opentrades.max_drawdown",
            TradeAccessor::MaxDrawdownPercent => "strategy.opentrades.max_drawdown_percent",
            TradeAccessor::EntryId => "strategy.opentrades.entry_id",
            TradeAccessor::EntryComment => "strategy.opentrades.entry_comment",
            // Exit-only fields: not exposed on open trades.
            TradeAccessor::ExitPrice
            | TradeAccessor::ExitBarIndex
            | TradeAccessor::ExitTime
            | TradeAccessor::ExitId
            | TradeAccessor::ExitComment => "strategy.opentrades.<invalid>",
        }
    }

    /// The HIR result type of this accessor (`string` ids/comments, `int` bar
    /// indices/times, `float` prices/sizes/profits).
    pub fn result_type(self) -> Type {
        match self {
            TradeAccessor::EntryId
            | TradeAccessor::ExitId
            | TradeAccessor::EntryComment
            | TradeAccessor::ExitComment => Type::String,
            TradeAccessor::EntryBarIndex
            | TradeAccessor::ExitBarIndex
            | TradeAccessor::EntryTime
            | TradeAccessor::ExitTime => Type::Int,
            TradeAccessor::EntryPrice
            | TradeAccessor::ExitPrice
            | TradeAccessor::Size
            | TradeAccessor::Profit
            | TradeAccessor::ProfitPercent
            | TradeAccessor::Commission
            | TradeAccessor::MaxRunup
            | TradeAccessor::MaxRunupPercent
            | TradeAccessor::MaxDrawdown
            | TradeAccessor::MaxDrawdownPercent => Type::Float,
        }
    }
}

/// Inclusive upper bound on `str.format` arguments at the CSIL level (the
/// format string plus up to [`STR_FORMAT_MAX_ARGS`] − 1 substitution
/// values). Keeps `argc` (a `u8`) and the validator's structural check
/// well-defined; far above any realistic placeholder count.
pub const STR_FORMAT_MAX_ARGS: u8 = 32;

/// Default `format` of `str.format_time`: an ISO 8601 pattern. The
/// interpreter applies it when the argument is omitted at the
/// end of the call; the analyzer supplies it when a named `timezone` skips it.
pub const STR_FORMAT_TIME_DEFAULT_FORMAT: &str = "yyyy-MM-dd'T'HH:mm:ssZ";

/// Inclusive upper bound on `array.from` arguments at the CSIL level. Keeps
/// `argc` (a `u8`) well-defined for the variadic constructor; far above any
/// realistic literal-array length.
pub const ARRAY_FROM_MAX_ARGS: u8 = u8::MAX;

// ---------------------------------------------------------------------------
// Collection functions
// ---------------------------------------------------------------------------

/// The abstract element-type category a collection function's argument or
/// result takes, for the CSIL validator's stack simulation. This is coarser
/// than [`Type`]: the validator only tracks value categories, not the exact
/// element type of an `array<T>` (a `Ref` is opaque at the CSIL level), so the
/// checker owns strict element typing and the validator provides structural
/// defense in depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionAbstract {
    /// A collection handle (`Value::Ref`).
    Ref,
    /// A scalar element value (any element type; the checker has already
    /// verified it matches the collection's element type).
    Elem,
    /// An `int` index / size / count argument.
    Int,
    /// A `string` argument (`array.join`'s separator).
    Str,
    /// The result is a `bool` (`array.includes`).
    Bool,
    /// The result is a `float` regardless of element type — the numeric
    /// aggregates (`array.sum`, `array.avg`, `array.stdev`, …) return float
    /// even for an `array<int>` receiver.
    Float,
    /// No value (void result: mutators like `array.push`/`array.set`).
    Void,
}

/// A reference-typed collection builtin (`array.*`, `map.*`, `matrix.*`). Dispatched through
/// the generic `CALL_COLLECTION { func, argc }` opcode, mirroring the
/// [`PureFunc`] / `CALL_PURE` pattern. The registry, MIR/CSIL lowering, CSIL
/// validator, and VM all key off this tag; adding a function is a table entry
/// plus a VM arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CollectionFunc {
    // ---- typed constructors ----------------------------------------------
    /// `array.new_float` — a fresh `array<float>`.
    ArrayNewFloat,
    ArrayNewInt,
    ArrayNewBool,
    ArrayNewString,
    ArrayNewColor,
    /// `array.new<MyType>` — a fresh `array<MyType>` of
    /// object references (collections-of-UDTs). The emitted call carries
    /// `(size, initial, type_tag)` where `type_tag` encodes the element's
    /// `type_id` (see the MIR heap's `ElemType`).
    ArrayNewObject,
    /// `array.from(arg0, arg1, ...)` — element type inferred from the args
    /// (all must share one scalar type). At least one argument.
    ArrayFrom,
    /// `str.split(source, separator)` — split `source` on each occurrence of
    /// `separator` into a fresh `array<string>`. Lives in the `str.*` surface
    /// but returns a heap `array<string>`, so it rides the `CALL_COLLECTION`
    /// machinery like a constructor (allocates; no receiver handle). An empty
    /// separator splits into individual characters.
    StrSplit,

    // ---- operations (receiver is the first argument) ---------------------
    /// `array.push(id, value)` — append; void.
    ArrayPush,
    /// `array.pop(id)` — remove and return the last element.
    ArrayPop,
    /// `array.get(id, index)` — element at `index` (bounds-checked).
    ArrayGet,
    /// `array.set(id, index, value)` — overwrite `index`; void (bounds-checked).
    ArraySet,
    /// `array.size(id)` — element count (int).
    ArraySize,
    /// `array.clear(id)` — remove all elements; void.
    ArrayClear,
    /// `array.shift(id)` — remove and return the first element.
    ArrayShift,
    /// `array.unshift(id, value)` — prepend; void.
    ArrayUnshift,
    /// `array.insert(id, index, value)` — insert before `index`; void.
    ArrayInsert,
    /// `array.remove(id, index)` — remove and return the element at `index`.
    ArrayRemove,
    /// `array.includes(id, value)` — membership test (bool).
    ArrayIncludes,
    /// `array.indexof(id, value)` — first index of `value`, or -1 (int).
    ArrayIndexOf,
    /// `array.lastindexof(id, value)` — last index of `value`, or -1 (int).
    ArrayLastIndexOf,
    /// `array.reverse(id)` — reverse in place; void.
    ArrayReverse,
    /// `array.first(id)` — the first element (error if empty).
    ArrayFirst,
    /// `array.last(id)` — the last element (error if empty).
    ArrayLast,
    /// `array.concat(id1, id2)` — append `id2`'s elements to `id1`; returns
    /// `id1` (the mutated receiver handle).
    ArrayConcat,
    /// `array.slice(id, index_from, index_to)` — a shallow copy of the range
    /// `[from, to)` as a new `array<T>`.
    ArraySlice,
    /// `array.copy(id)` — a shallow copy as a new `array<T>`.
    ArrayCopy,
    /// `array.fill(id, value)` / `array.fill(id, value, index_from, index_to)`
    /// — set the range (default: whole array) to `value`; void.
    ArrayFill,
    /// `array.join` — join an `array<int|float|string>` into one
    /// string.
    ArrayJoin,

    // ---- numeric aggregates ----------------------------------------------
    // Each reduces a numeric `array<int|float>` to a scalar (float), skipping
    // `na` elements; an empty array (or one with only `na` elements) yields
    // `na`. The rule is described in docs/ir.md.
    /// `array.sum(id)` — sum of the (non-`na`) elements → float.
    ArraySum,
    /// `array.avg(id)` — arithmetic mean of the non-`na` elements → float.
    ArrayAvg,
    /// `array.min(id)` — smallest non-`na` element → float.
    ArrayMin,
    /// `array.max(id)` — largest non-`na` element → float.
    ArrayMax,
    /// `array.median(id)` — median of the non-`na` elements → float.
    ArrayMedian,
    /// `array.range(id)` — `max - min` of the non-`na` elements → float.
    ArrayRange,
    /// `array.mode(id)` — most frequent element; on a tie the smallest such
    /// value → float.
    ArrayMode,
    /// `array.variance(id)` — population variance (denominator `n`) of the
    /// non-`na` elements → float.
    ArrayVariance,
    /// `array.stdev(id)` — population standard deviation (`sqrt(variance)`) →
    /// float.
    ArrayStdev,
    /// `array.covariance(id1, id2, biased)` — covariance of two equal-length
    /// numeric arrays → float. `biased` (materialized bool, default `true`)
    /// selects the population (`/n`) vs sample (`/(n-1)`) estimator.
    ArrayCovariance,

    // ---- boolean reductions ----------------------------------------------
    // Neither takes a callback: `array.every(id)` and `array.some(id)` reduce
    // the array's own elements. An element is "true" when it is a `true` bool
    // or a non-zero, non-`na` number (`na`/zero count as false). On an empty
    // array `every` returns `true` and `some` returns `false`; the na/empty
    // rules are described in docs/ir.md.
    /// `array.every(id)` — `true` iff **every** element is true. → bool.
    ArrayEvery,
    /// `array.some(id)` — `true` iff **at least one** element is true. → bool.
    ArraySome,

    // ---- percentile / rank family ----------------------------------------
    /// `array.percentrank(id, index)` — the percentile rank of the element at
    /// `index`: the percentage of the *other* elements that are `<=` it → float.
    ArrayPercentRank,
    /// `array.percentile_nearest_rank(id, percentage)` — the value at the given
    /// percentile using the nearest-rank method → float.
    ArrayPercentileNearestRank,
    /// `array.percentile_linear_interpolation(id, percentage)` — the value at
    /// the given percentile, linearly interpolating between the two nearest
    /// ranks → float.
    ArrayPercentileLinearInterpolation,

    // ---- binary search ---------------------------------------------------
    // All three require the receiver array to be sorted ascending (a
    // precondition the caller must satisfy; unsorted input yields an
    // unspecified index). Results are plain `int` indices.
    /// `array.binary_search(id, val)` — index of `val` in the sorted array, or
    /// `-1` if absent (the documented not-found convention).
    ArrayBinarySearch,
    /// `array.binary_search_leftmost(id, val)` — if present, the leftmost index
    /// equal to `val`; otherwise the index of the first element to the left of
    /// where `val` would belong (clamped into range). Always a valid index for
    /// a non-empty array.
    ArrayBinarySearchLeftmost,
    /// `array.binary_search_rightmost(id, val)` — if present, the rightmost
    /// index equal to `val`; otherwise the index of the first element to the
    /// right of where `val` would belong (clamped into range). Always a valid
    /// index for a non-empty array.
    ArrayBinarySearchRightmost,

    // ---- numeric transforms ----------------------------------------------
    /// `array.abs(id)` — a new `array<float>` of the elementwise absolute
    /// values (`na` elements stay `na`).
    ArrayAbs,
    /// `array.standardize(id)` — a new `array<float>` of `(x - mean) / stdev`
    /// (`na` elements stay `na`).
    ArrayStandardize,

    // ---- ordering --------------------------------------------------------
    /// `array.sort` — sort a numeric array in
    /// place; void. `na` elements sort to the end.
    ArraySort,
    /// `array.sort_indices` — a new `array<int>`
    /// of indices that would sort the array (stable).
    ArraySortIndices,

    // ---- map.* -----------------------------------------------------------
    // Maps are insertion-ordered key→value dictionaries. The typed
    // constructor is a single tag: `map.new(...)` lowers to two `ConstInt` type
    // tags (key, val) that the VM reads to build the object. For the static
    // key/value types the checker keys off the registry name
    // (`map.new_<key>_<val>`, the parser's rewrite of a scalar `map.new<K, V>`)
    // or, for a user-defined value type, the call's type arguments.
    /// `map.new_<key>_<val>()` — a fresh empty `map<K, V>`. Lowered args are
    /// two int type tags `(key_ty, val_ty)`.
    MapNew,
    /// `map.put(id, key, value)` — insert/overwrite; returns the previous value
    /// for `key` (or `na` if absent).
    MapPut,
    /// `map.get(id, key)` — the value for `key`, or `na` if absent.
    MapGet,
    /// `map.contains(id, key)` — membership test (bool).
    MapContains,
    /// `map.remove(id, key)` — remove `key`; returns the removed value (or `na`).
    MapRemove,
    /// `map.size(id)` — entry count (int).
    MapSize,
    /// `map.clear(id)` — remove all entries; void.
    MapClear,
    /// `map.keys(id)` — a new `array<K>` of the keys in insertion order.
    MapKeys,
    /// `map.values(id)` — a new `array<V>` of the values in insertion order.
    MapValues,
    /// `map.copy(id)` — a shallow copy as a new `map<K, V>`.
    MapCopy,
    /// `map.put_all(dest, src)` — copy every entry of `src` into `dest`
    /// (overwriting on key collision); void.
    MapPutAll,

    // ---- matrix.* --------------------------------------------------------
    // Matrices are `rows × columns`, row-major, reference-typed like arrays and
    // maps. The typed constructor is a single tag: `matrix.new_<T>(rows,
    // columns, initial_value)` lowers to a trailing `ConstInt` element-type tag
    // the VM reads to build the object. For the static element type the checker
    // keys off the registry name (the parser's rewrite of a scalar
    // `matrix.new<T>`) or, for a user-defined element type, the call's type
    // argument. All other operations take the matrix handle as their first
    // argument, mirroring `array.*` / `map.*`.
    /// `matrix.new_<T>(rows, columns, initial_value)` — a fresh
    /// `rows × columns` matrix filled with `initial_value`. Lowered args are
    /// `(rows, columns, initial_value, elem_ty_tag)`.
    MatrixNew,
    /// `matrix.get(id, row, column)` — the cell at `(row, column)`
    /// (bounds-checked).
    MatrixGet,
    /// `matrix.set(id, row, column, value)` — overwrite the cell; void
    /// (bounds-checked).
    MatrixSet,
    /// `matrix.rows(id)` — row count (int).
    MatrixRows,
    /// `matrix.columns(id)` — column count (int).
    MatrixColumns,
    /// `matrix.elements_count(id)` — `rows * columns` (int).
    MatrixElementsCount,
    /// `matrix.fill(id, value, from_row, to_row, from_column, to_column)` —
    /// set a rectangular region to `value`; void. The range args are always
    /// materialized by the checker (`to_*` default to the matrix dimensions,
    /// carried as sentinel `-1`).
    MatrixFill,
    /// `matrix.copy(id)` — a deep copy as a new matrix.
    MatrixCopy,
    /// `matrix.add_row(id, row, values)` — insert a new row (from an
    /// `array<T>`) before index `row`; void. An omitted (`na`) `row` appends
    /// and an omitted (`na`) `values` fills the row with `na`.
    MatrixAddRow,
    /// `matrix.add_col(id, column, values)` — insert a new column (from an
    /// `array<T>`) before index `column`; void.
    MatrixAddCol,
    /// `matrix.remove_row(id, row)` — remove the row at `row` and return its
    /// cells as a fresh `array<T>`.
    MatrixRemoveRow,
    /// `matrix.remove_col(id, column)` — remove the column at `column` and
    /// return its cells as a fresh `array<T>`.
    MatrixRemoveCol,
    /// `matrix.swap_rows(id, row1, row2)` — swap two rows; void.
    MatrixSwapRows,
    /// `matrix.swap_columns(id, column1, column2)` — swap two columns; void.
    MatrixSwapColumns,
    /// `matrix.reverse(id)` — reverse the element order (both axes); void.
    MatrixReverse,
    /// `matrix.transpose(id)` — a new transposed matrix.
    MatrixTranspose,
    /// `matrix.reshape(id, rows, columns)` — reinterpret the elements
    /// (row-major) at a new shape with the same element count; void.
    MatrixReshape,
    /// `matrix.submatrix(id, from_row, to_row, from_column, to_column)` — a new
    /// matrix of the given rectangular region (`to_*` exclusive).
    MatrixSubmatrix,
    /// `matrix.concat(id1, id2)` — append `id2`'s rows to `id1` (same column
    /// count); returns `id1`.
    MatrixConcat,
    /// `matrix.sum(id1, id2)` — elementwise `id1 + id2` (matrix) or `id1 +
    /// scalar`; a new matrix.
    MatrixSum,
    /// `matrix.diff(id1, id2)` — elementwise `id1 - id2` (matrix) or `id1 -
    /// scalar`; a new matrix.
    MatrixDiff,
    /// `matrix.mult(id1, id2)` — matrix product `id1 * id2` (matrix, with a
    /// dimension check) or `id1 * scalar`; a new matrix.
    MatrixMult,
    /// `matrix.det(id)` — determinant via LU with partial pivoting → float.
    MatrixDet,
    /// `matrix.inv(id)` — inverse via LU with partial pivoting → a new matrix
    /// (`na`-filled and a runtime error if singular).
    MatrixInv,
    /// `matrix.trace(id)` — sum of the main-diagonal cells → float.
    MatrixTrace,
    /// `matrix.pow(id, power)` — integer matrix power → a new matrix.
    MatrixPow,
    /// `matrix.kron(id1, id2)` — Kronecker product → a new matrix.
    MatrixKron,
    /// `matrix.avg(id)` — mean of the (non-`na`) cells → float.
    MatrixAvg,
    /// `matrix.min(id)` — smallest non-`na` cell → float.
    MatrixMin,
    /// `matrix.max(id)` — largest non-`na` cell → float.
    MatrixMax,
    /// `matrix.median(id)` — median of the non-`na` cells → float.
    MatrixMedian,
    /// `matrix.mode(id)` — most frequent cell (smallest on a tie) → float.
    MatrixMode,
    /// `matrix.is_square(id)` — `rows == columns` (bool).
    MatrixIsSquare,
    /// `matrix.is_identity(id)` — square with 1 on the diagonal, 0 elsewhere.
    MatrixIsIdentity,
    /// `matrix.is_diagonal(id)` — square with all off-diagonal cells 0.
    MatrixIsDiagonal,
    /// `matrix.is_symmetric(id)` — square with `m[i][j] == m[j][i]`.
    MatrixIsSymmetric,
    /// `matrix.is_antisymmetric(id)` — square with `m[i][j] == -m[j][i]`.
    MatrixIsAntisymmetric,
    /// `matrix.is_triangular(id)` — square and upper- or lower-triangular.
    MatrixIsTriangular,
    /// `matrix.is_stochastic(id)` — every row sums to 1 (within tolerance).
    MatrixIsStochastic,
    /// `matrix.is_binary(id)` — every cell is 0 or 1.
    MatrixIsBinary,
    /// `matrix.is_zero(id)` — every cell is 0.
    MatrixIsZero,
    /// `matrix.is_antidiagonal(id)` — square with all off-anti-diagonal cells 0.
    MatrixIsAntidiagonal,
    /// `matrix.rank(id)` — numerical rank via Gaussian elimination with partial
    /// pivoting → int.
    MatrixRank,
    /// `matrix.row(id, row)` — the given row's cells as a fresh `array<T>`.
    MatrixRow,
    /// `matrix.col(id, column)` — the given column's cells as a fresh
    /// `array<T>`.
    MatrixCol,
    /// `matrix.eigenvalues(id)` — the eigenvalues of a symmetric matrix as a
    /// fresh `array<float>` (descending; via cyclic Jacobi rotations).
    MatrixEigenvalues,
    /// `matrix.eigenvectors(id)` — the eigenvectors of a symmetric matrix as a
    /// new matrix (columns are unit eigenvectors, matched to
    /// `matrix.eigenvalues`'s descending order).
    MatrixEigenvectors,
    /// `matrix.pinv(id)` — the Moore-Penrose pseudo-inverse of a full-rank
    /// matrix as a new matrix (normal-equations form; reuses the LU inverse).
    MatrixPinv,
    /// `matrix.sort(id, column, order)` — reorder the receiver's rows in place.
    MatrixSort,
    /// Compiler-owned capability guard for the ordered, stateful scan.
    /// Its result is target-chosen: false is always permitted. The false
    /// arm defines the program; the true arm must be its exact refinement.
    /// Validation cannot prove this producer obligation. Pre-lowered CSIL from
    /// a different build is not valid input for this standalone export; re-lower
    /// from source.
    /// Not a Pine builtin or an observable Pine value.
    InternalDistanceScanEligible,
    /// Compiler-internal ordered scan over numeric arrays; returns the final
    /// threshold and mutates the two existing arrays. Metered inside the VM.
    InternalDistanceScan,
}

impl CollectionFunc {
    pub fn name(self) -> &'static str {
        match self {
            CollectionFunc::InternalDistanceScanEligible => "internal.distance_scan_eligible",
            CollectionFunc::InternalDistanceScan => "internal.distance_scan",
            CollectionFunc::ArrayNewFloat => "array.new_float",
            CollectionFunc::ArrayNewInt => "array.new_int",
            CollectionFunc::ArrayNewBool => "array.new_bool",
            CollectionFunc::ArrayNewString => "array.new_string",
            CollectionFunc::ArrayNewColor => "array.new_color",
            CollectionFunc::ArrayNewObject => "array.new_object",
            CollectionFunc::ArrayFrom => "array.from",
            CollectionFunc::StrSplit => "str.split",
            CollectionFunc::ArrayPush => "array.push",
            CollectionFunc::ArrayPop => "array.pop",
            CollectionFunc::ArrayGet => "array.get",
            CollectionFunc::ArraySet => "array.set",
            CollectionFunc::ArraySize => "array.size",
            CollectionFunc::ArrayClear => "array.clear",
            CollectionFunc::ArrayShift => "array.shift",
            CollectionFunc::ArrayUnshift => "array.unshift",
            CollectionFunc::ArrayInsert => "array.insert",
            CollectionFunc::ArrayRemove => "array.remove",
            CollectionFunc::ArrayIncludes => "array.includes",
            CollectionFunc::ArrayIndexOf => "array.indexof",
            CollectionFunc::ArrayLastIndexOf => "array.lastindexof",
            CollectionFunc::ArrayReverse => "array.reverse",
            CollectionFunc::ArrayFirst => "array.first",
            CollectionFunc::ArrayLast => "array.last",
            CollectionFunc::ArrayConcat => "array.concat",
            CollectionFunc::ArraySlice => "array.slice",
            CollectionFunc::ArrayCopy => "array.copy",
            CollectionFunc::ArrayFill => "array.fill",
            CollectionFunc::ArrayJoin => "array.join",
            CollectionFunc::ArraySum => "array.sum",
            CollectionFunc::ArrayAvg => "array.avg",
            CollectionFunc::ArrayMin => "array.min",
            CollectionFunc::ArrayMax => "array.max",
            CollectionFunc::ArrayMedian => "array.median",
            CollectionFunc::ArrayRange => "array.range",
            CollectionFunc::ArrayMode => "array.mode",
            CollectionFunc::ArrayVariance => "array.variance",
            CollectionFunc::ArrayStdev => "array.stdev",
            CollectionFunc::ArrayCovariance => "array.covariance",
            CollectionFunc::ArrayEvery => "array.every",
            CollectionFunc::ArraySome => "array.some",
            CollectionFunc::ArrayPercentRank => "array.percentrank",
            CollectionFunc::ArrayPercentileNearestRank => "array.percentile_nearest_rank",
            CollectionFunc::ArrayPercentileLinearInterpolation => {
                "array.percentile_linear_interpolation"
            }
            CollectionFunc::ArrayBinarySearch => "array.binary_search",
            CollectionFunc::ArrayBinarySearchLeftmost => "array.binary_search_leftmost",
            CollectionFunc::ArrayBinarySearchRightmost => "array.binary_search_rightmost",
            CollectionFunc::ArrayAbs => "array.abs",
            CollectionFunc::ArrayStandardize => "array.standardize",
            CollectionFunc::ArraySort => "array.sort",
            CollectionFunc::ArraySortIndices => "array.sort_indices",
            CollectionFunc::MapNew => "map.new",
            CollectionFunc::MapPut => "map.put",
            CollectionFunc::MapGet => "map.get",
            CollectionFunc::MapContains => "map.contains",
            CollectionFunc::MapRemove => "map.remove",
            CollectionFunc::MapSize => "map.size",
            CollectionFunc::MapClear => "map.clear",
            CollectionFunc::MapKeys => "map.keys",
            CollectionFunc::MapValues => "map.values",
            CollectionFunc::MapCopy => "map.copy",
            CollectionFunc::MapPutAll => "map.put_all",
            CollectionFunc::MatrixNew => "matrix.new",
            CollectionFunc::MatrixGet => "matrix.get",
            CollectionFunc::MatrixSet => "matrix.set",
            CollectionFunc::MatrixRows => "matrix.rows",
            CollectionFunc::MatrixColumns => "matrix.columns",
            CollectionFunc::MatrixElementsCount => "matrix.elements_count",
            CollectionFunc::MatrixFill => "matrix.fill",
            CollectionFunc::MatrixCopy => "matrix.copy",
            CollectionFunc::MatrixAddRow => "matrix.add_row",
            CollectionFunc::MatrixAddCol => "matrix.add_col",
            CollectionFunc::MatrixRemoveRow => "matrix.remove_row",
            CollectionFunc::MatrixRemoveCol => "matrix.remove_col",
            CollectionFunc::MatrixSwapRows => "matrix.swap_rows",
            CollectionFunc::MatrixSwapColumns => "matrix.swap_columns",
            CollectionFunc::MatrixReverse => "matrix.reverse",
            CollectionFunc::MatrixTranspose => "matrix.transpose",
            CollectionFunc::MatrixReshape => "matrix.reshape",
            CollectionFunc::MatrixSubmatrix => "matrix.submatrix",
            CollectionFunc::MatrixConcat => "matrix.concat",
            CollectionFunc::MatrixSum => "matrix.sum",
            CollectionFunc::MatrixDiff => "matrix.diff",
            CollectionFunc::MatrixMult => "matrix.mult",
            CollectionFunc::MatrixDet => "matrix.det",
            CollectionFunc::MatrixInv => "matrix.inv",
            CollectionFunc::MatrixTrace => "matrix.trace",
            CollectionFunc::MatrixPow => "matrix.pow",
            CollectionFunc::MatrixKron => "matrix.kron",
            CollectionFunc::MatrixAvg => "matrix.avg",
            CollectionFunc::MatrixMin => "matrix.min",
            CollectionFunc::MatrixMax => "matrix.max",
            CollectionFunc::MatrixMedian => "matrix.median",
            CollectionFunc::MatrixMode => "matrix.mode",
            CollectionFunc::MatrixIsSquare => "matrix.is_square",
            CollectionFunc::MatrixIsIdentity => "matrix.is_identity",
            CollectionFunc::MatrixIsDiagonal => "matrix.is_diagonal",
            CollectionFunc::MatrixIsSymmetric => "matrix.is_symmetric",
            CollectionFunc::MatrixIsAntisymmetric => "matrix.is_antisymmetric",
            CollectionFunc::MatrixIsTriangular => "matrix.is_triangular",
            CollectionFunc::MatrixIsStochastic => "matrix.is_stochastic",
            CollectionFunc::MatrixIsBinary => "matrix.is_binary",
            CollectionFunc::MatrixIsZero => "matrix.is_zero",
            CollectionFunc::MatrixIsAntidiagonal => "matrix.is_antidiagonal",
            CollectionFunc::MatrixRank => "matrix.rank",
            CollectionFunc::MatrixRow => "matrix.row",
            CollectionFunc::MatrixCol => "matrix.col",
            CollectionFunc::MatrixEigenvalues => "matrix.eigenvalues",
            CollectionFunc::MatrixEigenvectors => "matrix.eigenvectors",
            CollectionFunc::MatrixPinv => "matrix.pinv",
            CollectionFunc::MatrixSort => "matrix.sort",
        }
    }

    /// Argument labels and the required count for each collection function.
    ///
    /// Collection calls use receiver-dependent typing in the semantic
    /// analyzer, but their arity and argument labels are fixed here. The
    /// builtin registry and editor-surface generator read this table so that
    /// `array.get`, `map.get` and `matrix.get` keep separate entries.
    pub fn surface_params(self) -> (&'static [&'static str], usize) {
        use CollectionFunc as F;
        match self {
            F::InternalDistanceScanEligible | F::InternalDistanceScan => (&[], 0),
            F::ArrayNewFloat
            | F::ArrayNewInt
            | F::ArrayNewBool
            | F::ArrayNewString
            | F::ArrayNewColor
            | F::ArrayNewObject => (&["size", "initial_value"], 0),
            F::ArrayFrom => (&["arg0", "..."], 1),
            F::StrSplit => (&["string", "separator"], 2),
            F::ArrayPush | F::ArrayUnshift => (&["id", "value"], 2),
            F::ArrayPop
            | F::ArraySize
            | F::ArrayClear
            | F::ArrayShift
            | F::ArrayReverse
            | F::ArrayFirst
            | F::ArrayLast
            | F::ArrayCopy
            | F::ArraySum
            | F::ArrayAvg
            | F::ArrayMedian
            | F::ArrayRange
            | F::ArrayMode
            | F::ArrayAbs
            | F::ArrayStandardize
            | F::ArrayEvery
            | F::ArraySome => (&["id"], 1),
            F::ArrayGet | F::ArrayRemove => (&["id", "index"], 2),
            F::ArraySet | F::ArrayInsert => (&["id", "index", "value"], 3),
            F::ArrayIncludes | F::ArrayIndexOf | F::ArrayLastIndexOf => (&["id", "value"], 2),
            F::ArrayBinarySearch | F::ArrayBinarySearchLeftmost | F::ArrayBinarySearchRightmost => {
                (&["id", "val"], 2)
            }
            F::ArrayConcat => (&["id1", "id2"], 2),
            F::ArraySlice => (&["id", "index_from", "index_to"], 3),
            F::ArrayFill => (&["id", "value", "index_from", "index_to"], 2),
            F::ArrayJoin => (&["id", "separator"], 1),
            F::ArrayMin | F::ArrayMax => (&["id", "nth"], 1),
            F::ArrayVariance | F::ArrayStdev => (&["id", "biased"], 1),
            F::ArrayCovariance => (&["id1", "id2", "biased"], 2),
            F::ArrayPercentRank => (&["id", "index"], 2),
            F::ArrayPercentileNearestRank | F::ArrayPercentileLinearInterpolation => {
                (&["id", "percentage"], 2)
            }
            F::ArraySort | F::ArraySortIndices => (&["id", "order"], 1),

            F::MapNew => (&[], 0),
            F::MapPut => (&["id", "key", "value"], 3),
            F::MapGet | F::MapContains | F::MapRemove => (&["id", "key"], 2),
            F::MapSize | F::MapClear | F::MapKeys | F::MapValues | F::MapCopy => (&["id"], 1),
            F::MapPutAll => (&["id", "id2"], 2),

            F::MatrixNew => (&["rows", "columns", "initial_value"], 0),
            F::MatrixGet => (&["id", "row", "column"], 3),
            F::MatrixSet => (&["id", "row", "column", "value"], 4),
            F::MatrixRows
            | F::MatrixColumns
            | F::MatrixElementsCount
            | F::MatrixCopy
            | F::MatrixReverse
            | F::MatrixTranspose
            | F::MatrixDet
            | F::MatrixInv
            | F::MatrixTrace
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
            | F::MatrixEigenvalues
            | F::MatrixEigenvectors
            | F::MatrixPinv => (&["id"], 1),
            F::MatrixFill => (
                &[
                    "id",
                    "value",
                    "from_row",
                    "to_row",
                    "from_column",
                    "to_column",
                ],
                2,
            ),
            F::MatrixAddRow => (&["id", "row", "array_id"], 1),
            F::MatrixAddCol => (&["id", "column", "array_id"], 1),
            F::MatrixRemoveRow => (&["id", "row"], 2),
            F::MatrixRemoveCol => (&["id", "column"], 2),
            F::MatrixSwapRows => (&["id", "row1", "row2"], 3),
            F::MatrixSwapColumns => (&["id", "column1", "column2"], 3),
            F::MatrixReshape => (&["id", "rows", "columns"], 3),
            F::MatrixSubmatrix => (&["id", "from_row", "to_row", "from_column", "to_column"], 5),
            F::MatrixConcat | F::MatrixSum | F::MatrixDiff | F::MatrixMult | F::MatrixKron => {
                (&["id1", "id2"], 2)
            }
            F::MatrixPow => (&["id", "power"], 2),
            F::MatrixRow => (&["id", "row"], 2),
            F::MatrixCol => (&["id", "column"], 2),
            F::MatrixSort => (&["id", "column", "order", "sort_field"], 1),
        }
    }

    /// Inclusive `argc` bounds at the **CSIL level** (after the compiler has
    /// materialized optional-argument defaults). The validator enforces these
    /// structurally.
    pub fn argc_bounds(self) -> (u8, u8) {
        match self {
            CollectionFunc::InternalDistanceScanEligible => (12, 12),
            CollectionFunc::InternalDistanceScan => (10, 10),
            // Constructors: (size, initial) both materialized.
            CollectionFunc::ArrayNewFloat
            | CollectionFunc::ArrayNewInt
            | CollectionFunc::ArrayNewBool
            | CollectionFunc::ArrayNewString
            | CollectionFunc::ArrayNewColor => (2, 2),
            // Object constructor: (size, initial, type_tag).
            CollectionFunc::ArrayNewObject => (3, 3),
            // from: at least one element.
            CollectionFunc::ArrayFrom => (1, ARRAY_FROM_MAX_ARGS),
            // str.split: (source, separator).
            CollectionFunc::StrSplit => (2, 2),
            // (id) unary operations.
            CollectionFunc::ArrayPop
            | CollectionFunc::ArraySize
            | CollectionFunc::ArrayClear
            | CollectionFunc::ArrayShift
            | CollectionFunc::ArrayReverse
            | CollectionFunc::ArrayFirst
            | CollectionFunc::ArrayLast
            | CollectionFunc::ArrayCopy
            // Aggregates and transforms are unary `(id)`.
            | CollectionFunc::ArraySum
            | CollectionFunc::ArrayAvg
            | CollectionFunc::ArrayMedian
            | CollectionFunc::ArrayRange
            | CollectionFunc::ArrayMode
            | CollectionFunc::ArrayAbs
            | CollectionFunc::ArrayStandardize
            // Boolean reductions are unary `(id)`.
            | CollectionFunc::ArrayEvery
            | CollectionFunc::ArraySome => (1, 1),
            // min / max: (id, nth) and variance / stdev: (id, biased): the
            // second argument's default is materialized by the checker, so CSIL
            // argc is always 2.
            CollectionFunc::ArrayMin
            | CollectionFunc::ArrayMax
            | CollectionFunc::ArrayVariance
            | CollectionFunc::ArrayStdev => (2, 2),
            // sort / sort_indices: (id, order) — the order default is
            // materialized by the checker, so CSIL argc is always 2.
            CollectionFunc::ArraySort | CollectionFunc::ArraySortIndices => (2, 2),
            // percentrank / percentile family: (id, x) where x is an index or a
            // percentage.
            CollectionFunc::ArrayPercentRank
            | CollectionFunc::ArrayPercentileNearestRank
            | CollectionFunc::ArrayPercentileLinearInterpolation => (2, 2),
            // binary search variants: (id, val).
            CollectionFunc::ArrayBinarySearch
            | CollectionFunc::ArrayBinarySearchLeftmost
            | CollectionFunc::ArrayBinarySearchRightmost => (2, 2),
            // covariance: (id1, id2, biased) — the `biased` default (true) is
            // materialized by the checker, so CSIL argc is always 3.
            CollectionFunc::ArrayCovariance => (3, 3),
            // (id, x) binary operations.
            CollectionFunc::ArrayPush
            | CollectionFunc::ArrayGet
            | CollectionFunc::ArrayUnshift
            | CollectionFunc::ArrayRemove
            | CollectionFunc::ArrayIncludes
            | CollectionFunc::ArrayIndexOf
            | CollectionFunc::ArrayLastIndexOf
            | CollectionFunc::ArrayConcat => (2, 2),
            // (id, index, value) ternary operations.
            CollectionFunc::ArraySet | CollectionFunc::ArrayInsert => (3, 3),
            // slice: (id, from, to) — both indices materialized.
            CollectionFunc::ArraySlice => (3, 3),
            // fill: (id, value[, from[, to]]). The VM resolves an omitted
            // end from the already-evaluated receiver's length.
            CollectionFunc::ArrayFill => (2, 4),
            // join: (id) or (id, separator).
            CollectionFunc::ArrayJoin => (1, 2),
            // map.new: two materialized int type tags (key, val).
            CollectionFunc::MapNew => (2, 2),
            // (id) unary map operations.
            CollectionFunc::MapSize
            | CollectionFunc::MapClear
            | CollectionFunc::MapKeys
            | CollectionFunc::MapValues
            | CollectionFunc::MapCopy => (1, 1),
            // (id, key) / (id, other-map) binary map operations.
            CollectionFunc::MapGet
            | CollectionFunc::MapContains
            | CollectionFunc::MapRemove
            | CollectionFunc::MapPutAll => (2, 2),
            // (id, key, value) ternary.
            CollectionFunc::MapPut => (3, 3),

            // ---- matrix.* -------------------------------------------------
            // matrix.new_<T>: (rows, columns, initial_value, elem_ty_tag).
            CollectionFunc::MatrixNew => (4, 4),
            // (id) unary matrix operations.
            CollectionFunc::MatrixRows
            | CollectionFunc::MatrixColumns
            | CollectionFunc::MatrixElementsCount
            | CollectionFunc::MatrixCopy
            | CollectionFunc::MatrixReverse
            | CollectionFunc::MatrixTranspose
            | CollectionFunc::MatrixDet
            | CollectionFunc::MatrixInv
            | CollectionFunc::MatrixTrace
            | CollectionFunc::MatrixAvg
            | CollectionFunc::MatrixMin
            | CollectionFunc::MatrixMax
            | CollectionFunc::MatrixMedian
            | CollectionFunc::MatrixMode
            | CollectionFunc::MatrixIsSquare
            | CollectionFunc::MatrixIsIdentity
            | CollectionFunc::MatrixIsDiagonal
            | CollectionFunc::MatrixIsSymmetric
            | CollectionFunc::MatrixIsAntisymmetric
            | CollectionFunc::MatrixIsTriangular
            | CollectionFunc::MatrixIsStochastic
            | CollectionFunc::MatrixIsBinary
            | CollectionFunc::MatrixIsZero
            | CollectionFunc::MatrixIsAntidiagonal
            | CollectionFunc::MatrixEigenvalues
            | CollectionFunc::MatrixEigenvectors
            | CollectionFunc::MatrixPinv
            | CollectionFunc::MatrixRank => (1, 1),
            // matrix.sort: (id, column, order) — the column/order defaults are
            // materialized by the checker, so CSIL argc is always 3.
            CollectionFunc::MatrixSort => (4, 4),
            // (id, x) binary matrix operations.
            CollectionFunc::MatrixRemoveRow
            | CollectionFunc::MatrixRemoveCol
            | CollectionFunc::MatrixConcat
            | CollectionFunc::MatrixSum
            | CollectionFunc::MatrixDiff
            | CollectionFunc::MatrixMult
            | CollectionFunc::MatrixPow
            | CollectionFunc::MatrixKron
            | CollectionFunc::MatrixRow
            | CollectionFunc::MatrixCol => (2, 2),
            // (id, a, b) ternary matrix operations.
            CollectionFunc::MatrixGet
            | CollectionFunc::MatrixSwapRows
            | CollectionFunc::MatrixSwapColumns
            | CollectionFunc::MatrixReshape
            | CollectionFunc::MatrixAddRow
            | CollectionFunc::MatrixAddCol => (3, 3),
            // (id, row, column, value).
            CollectionFunc::MatrixSet => (4, 4),
            // submatrix: (id, from_row, to_row, from_column, to_column).
            CollectionFunc::MatrixSubmatrix => (5, 5),
            // fill: (id, value, from_row, to_row, from_column, to_column) — all
            // materialized (defaults carried as sentinels).
            CollectionFunc::MatrixFill => (6, 6),
        }
    }

    /// Metering cost class: `O(n)` functions (that touch every element) cost
    /// their element count; `O(1)` functions cost 1 unit. The VM charges the
    /// actual element count for the `O(n)` group; this reports whether the
    /// function is in that group (see docs/ir.md).
    pub fn is_linear_cost(self) -> bool {
        matches!(
            self,
            CollectionFunc::ArrayFrom
                | CollectionFunc::StrSplit
                | CollectionFunc::ArrayNewObject
                | CollectionFunc::ArrayNewFloat
                | CollectionFunc::ArrayNewInt
                | CollectionFunc::ArrayNewBool
                | CollectionFunc::ArrayNewString
                | CollectionFunc::ArrayNewColor
                | CollectionFunc::ArrayClear
                | CollectionFunc::ArrayShift
                | CollectionFunc::ArrayUnshift
                | CollectionFunc::ArrayInsert
                | CollectionFunc::ArrayRemove
                | CollectionFunc::ArrayReverse
                | CollectionFunc::ArrayConcat
                | CollectionFunc::ArraySlice
                | CollectionFunc::ArrayCopy
                | CollectionFunc::ArrayFill
                | CollectionFunc::ArrayIncludes
                | CollectionFunc::ArrayIndexOf
                | CollectionFunc::ArrayLastIndexOf
                | CollectionFunc::ArrayJoin
                // Aggregates / transforms / sorts all scan every element.
                | CollectionFunc::ArraySum
                | CollectionFunc::ArrayAvg
                | CollectionFunc::ArrayMin
                | CollectionFunc::ArrayMax
                | CollectionFunc::ArrayMedian
                | CollectionFunc::ArrayRange
                | CollectionFunc::ArrayMode
                | CollectionFunc::ArrayVariance
                | CollectionFunc::ArrayStdev
                | CollectionFunc::ArrayCovariance
                // Boolean reductions scan every element.
                | CollectionFunc::ArrayEvery
                | CollectionFunc::ArraySome
                | CollectionFunc::ArrayPercentRank
                | CollectionFunc::ArrayPercentileNearestRank
                | CollectionFunc::ArrayPercentileLinearInterpolation
                | CollectionFunc::ArrayBinarySearch
                | CollectionFunc::ArrayBinarySearchLeftmost
                | CollectionFunc::ArrayBinarySearchRightmost
                | CollectionFunc::ArrayAbs
                | CollectionFunc::ArrayStandardize
                | CollectionFunc::ArraySort
                | CollectionFunc::ArraySortIndices
                // Map operations that touch every entry.
                | CollectionFunc::MapClear
                | CollectionFunc::MapKeys
                | CollectionFunc::MapValues
                | CollectionFunc::MapCopy
                | CollectionFunc::MapPutAll
                // Vec-backed lookups scan entries (honest metering).
                | CollectionFunc::MapPut
                | CollectionFunc::MapGet
                | CollectionFunc::MapContains
                | CollectionFunc::MapRemove
                // Every matrix operation touches at least the receiver's cell
                // grid (construction, copy, elementwise, LU, aggregates,
                // predicates); metered by cell count. `matrix.get`/`set` are
                // O(1) but folded in for a uniform matrix cost class.
                | CollectionFunc::MatrixNew
                | CollectionFunc::MatrixGet
                | CollectionFunc::MatrixSet
                | CollectionFunc::MatrixRows
                | CollectionFunc::MatrixColumns
                | CollectionFunc::MatrixElementsCount
                | CollectionFunc::MatrixFill
                | CollectionFunc::MatrixCopy
                | CollectionFunc::MatrixAddRow
                | CollectionFunc::MatrixAddCol
                | CollectionFunc::MatrixRemoveRow
                | CollectionFunc::MatrixRemoveCol
                | CollectionFunc::MatrixSwapRows
                | CollectionFunc::MatrixSwapColumns
                | CollectionFunc::MatrixReverse
                | CollectionFunc::MatrixTranspose
                | CollectionFunc::MatrixReshape
                | CollectionFunc::MatrixSubmatrix
                | CollectionFunc::MatrixConcat
                | CollectionFunc::MatrixSum
                | CollectionFunc::MatrixDiff
                | CollectionFunc::MatrixMult
                | CollectionFunc::MatrixDet
                | CollectionFunc::MatrixInv
                | CollectionFunc::MatrixTrace
                | CollectionFunc::MatrixPow
                | CollectionFunc::MatrixKron
                | CollectionFunc::MatrixAvg
                | CollectionFunc::MatrixMin
                | CollectionFunc::MatrixMax
                | CollectionFunc::MatrixMedian
                | CollectionFunc::MatrixMode
                | CollectionFunc::MatrixIsSquare
                | CollectionFunc::MatrixIsIdentity
                | CollectionFunc::MatrixIsDiagonal
                | CollectionFunc::MatrixIsSymmetric
                | CollectionFunc::MatrixIsAntisymmetric
                | CollectionFunc::MatrixIsTriangular
                | CollectionFunc::MatrixIsStochastic
                | CollectionFunc::MatrixIsBinary
                | CollectionFunc::MatrixIsZero
                | CollectionFunc::MatrixIsAntidiagonal
                | CollectionFunc::MatrixRank
                | CollectionFunc::MatrixRow
                | CollectionFunc::MatrixCol
                | CollectionFunc::MatrixEigenvalues
                | CollectionFunc::MatrixEigenvectors
                | CollectionFunc::MatrixPinv
                | CollectionFunc::MatrixSort
        )
    }

    /// The abstract operand categories this call pops, **top of stack first**
    /// (the last argument is on top). `argc` disambiguates the optional-arg
    /// forms (`array.fill`, `array.join`).
    pub fn operand_reqs(self, argc: u8) -> Vec<CollectionAbstract> {
        use CollectionAbstract::{Bool, Elem, Float, Int, Ref, Str};
        match self {
            // start, end, count, query object, feature-array object, labels,
            // distances, predictions, neighbor count, initial threshold.
            // Numeric selectors use Elem so fractional int values are NOT
            // truncated before the guard can decline them. The guard adds
            // two literal field indices and dereferences their receivers safely.
            CollectionFunc::InternalDistanceScanEligible => {
                vec![
                    Int, Int, Float, Elem, Ref, Ref, Ref, Ref, Ref, Elem, Elem, Elem,
                ]
            }
            CollectionFunc::InternalDistanceScan => {
                vec![Float, Elem, Ref, Ref, Ref, Ref, Ref, Elem, Elem, Elem]
            }
            // Constructors: (size:int, initial:elem). Top is `initial`.
            CollectionFunc::ArrayNewFloat
            | CollectionFunc::ArrayNewInt
            | CollectionFunc::ArrayNewBool
            | CollectionFunc::ArrayNewString
            | CollectionFunc::ArrayNewColor => vec![Elem, Int],
            // Object constructor: (size:int, initial:elem, type_tag:int). Top
            // is `type_tag`.
            CollectionFunc::ArrayNewObject => vec![Int, Elem, Int],
            // from: all elements (any count).
            CollectionFunc::ArrayFrom => vec![Elem; argc as usize],
            // str.split: (source:string, separator:string). Top is separator.
            CollectionFunc::StrSplit => vec![Str, Str],
            // Unary (id).
            CollectionFunc::ArrayPop
            | CollectionFunc::ArraySize
            | CollectionFunc::ArrayClear
            | CollectionFunc::ArrayShift
            | CollectionFunc::ArrayReverse
            | CollectionFunc::ArrayFirst
            | CollectionFunc::ArrayLast
            | CollectionFunc::ArrayCopy
            | CollectionFunc::ArraySum
            | CollectionFunc::ArrayAvg
            | CollectionFunc::ArrayMedian
            | CollectionFunc::ArrayRange
            | CollectionFunc::ArrayMode
            | CollectionFunc::ArrayAbs
            | CollectionFunc::ArrayStandardize
            | CollectionFunc::ArrayEvery
            | CollectionFunc::ArraySome => vec![Ref],
            // min / max: (id, nth:int). Top is `nth`.
            CollectionFunc::ArrayMin | CollectionFunc::ArrayMax => vec![Int, Ref],
            // variance / stdev: (id, biased:bool). Top is `biased`.
            CollectionFunc::ArrayVariance | CollectionFunc::ArrayStdev => vec![Bool, Ref],
            // sort / sort_indices: (id, order:int). Top is `order`.
            CollectionFunc::ArraySort | CollectionFunc::ArraySortIndices => vec![Int, Ref],
            // percentrank: (id, index:int). Top is index.
            CollectionFunc::ArrayPercentRank => vec![Int, Ref],
            // percentile family: (id, percentage:float). Top is percentage.
            CollectionFunc::ArrayPercentileNearestRank
            | CollectionFunc::ArrayPercentileLinearInterpolation => vec![Float, Ref],
            // binary search variants: (id, val:elem). Top is val.
            CollectionFunc::ArrayBinarySearch
            | CollectionFunc::ArrayBinarySearchLeftmost
            | CollectionFunc::ArrayBinarySearchRightmost => vec![Elem, Ref],
            // covariance: (id1:ref, id2:ref, biased:bool). Top is biased.
            CollectionFunc::ArrayCovariance => vec![Bool, Ref, Ref],
            // (id, value:elem). Top is value.
            CollectionFunc::ArrayPush
            | CollectionFunc::ArrayUnshift
            | CollectionFunc::ArrayIncludes
            | CollectionFunc::ArrayIndexOf
            | CollectionFunc::ArrayLastIndexOf => vec![Elem, Ref],
            // (id, index:int). Top is index.
            CollectionFunc::ArrayGet | CollectionFunc::ArrayRemove => vec![Int, Ref],
            // (id, ref). Top is the second collection.
            CollectionFunc::ArrayConcat => vec![Ref, Ref],
            // (id, index:int, value:elem). Top is value.
            CollectionFunc::ArraySet | CollectionFunc::ArrayInsert => vec![Elem, Int, Ref],
            // slice: (id, from:int, to:int). Top is `to`.
            CollectionFunc::ArraySlice => vec![Int, Int, Ref],
            // fill: (id, value[, from[, to]]).
            CollectionFunc::ArrayFill => match argc {
                4 => vec![Int, Int, Elem, Ref],
                3 => vec![Int, Elem, Ref],
                _ => vec![Elem, Ref],
            },
            // join: (id) or (id, separator:string). Top is separator.
            CollectionFunc::ArrayJoin => {
                if argc == 2 {
                    vec![Str, Ref]
                } else {
                    vec![Ref]
                }
            }
            // map.new: two int type tags (key, val). Top is `val`.
            CollectionFunc::MapNew => vec![Int, Int],
            // (id) unary map operations.
            CollectionFunc::MapSize
            | CollectionFunc::MapClear
            | CollectionFunc::MapKeys
            | CollectionFunc::MapValues
            | CollectionFunc::MapCopy => vec![Ref],
            // (id, key:elem). Top is key.
            CollectionFunc::MapGet | CollectionFunc::MapContains | CollectionFunc::MapRemove => {
                vec![Elem, Ref]
            }
            // (id, other-map:ref). Top is the second map.
            CollectionFunc::MapPutAll => vec![Ref, Ref],
            // (id, key:elem, value:elem). Top is value.
            CollectionFunc::MapPut => vec![Elem, Elem, Ref],

            // ---- matrix.* -------------------------------------------------
            // matrix.new_<T>: (rows:int, columns:int, initial_value:elem,
            // tag:int). Top is the tag.
            CollectionFunc::MatrixNew => vec![Int, Elem, Int, Int],
            // (id) unary matrix operations.
            CollectionFunc::MatrixRows
            | CollectionFunc::MatrixColumns
            | CollectionFunc::MatrixElementsCount
            | CollectionFunc::MatrixCopy
            | CollectionFunc::MatrixReverse
            | CollectionFunc::MatrixTranspose
            | CollectionFunc::MatrixDet
            | CollectionFunc::MatrixInv
            | CollectionFunc::MatrixTrace
            | CollectionFunc::MatrixAvg
            | CollectionFunc::MatrixMin
            | CollectionFunc::MatrixMax
            | CollectionFunc::MatrixMedian
            | CollectionFunc::MatrixMode
            | CollectionFunc::MatrixIsSquare
            | CollectionFunc::MatrixIsIdentity
            | CollectionFunc::MatrixIsDiagonal
            | CollectionFunc::MatrixIsSymmetric
            | CollectionFunc::MatrixIsAntisymmetric
            | CollectionFunc::MatrixIsTriangular
            | CollectionFunc::MatrixIsStochastic
            | CollectionFunc::MatrixIsBinary
            | CollectionFunc::MatrixIsZero
            | CollectionFunc::MatrixIsAntidiagonal
            | CollectionFunc::MatrixEigenvalues
            | CollectionFunc::MatrixEigenvectors
            | CollectionFunc::MatrixPinv
            | CollectionFunc::MatrixRank => vec![Ref],
            // (id, index:int). Top is index.
            CollectionFunc::MatrixRemoveRow
            | CollectionFunc::MatrixRemoveCol
            | CollectionFunc::MatrixRow
            | CollectionFunc::MatrixCol => vec![Int, Ref],
            // sort: (id, column:int, order:int). Top is order.
            CollectionFunc::MatrixSort => vec![Int, Int, Int, Ref],
            // (id, other:ref). Top is the second matrix.
            CollectionFunc::MatrixConcat | CollectionFunc::MatrixKron => vec![Ref, Ref],
            // (id, matrix-or-scalar). Top is the second operand (Elem accepts a
            // Ref or a scalar at the CSIL level; the checker disambiguates).
            CollectionFunc::MatrixSum | CollectionFunc::MatrixDiff | CollectionFunc::MatrixMult => {
                vec![Elem, Ref]
            }
            // (id, power:int). Top is power.
            CollectionFunc::MatrixPow => vec![Int, Ref],
            // (id, row:int, column:int). Top is column.
            CollectionFunc::MatrixGet => vec![Int, Int, Ref],
            // (id, a:int, b:int). Top is `b`.
            CollectionFunc::MatrixSwapRows
            | CollectionFunc::MatrixSwapColumns
            | CollectionFunc::MatrixReshape => vec![Int, Int, Ref],
            // (id, index:int, values:ref array). Top is the array.
            CollectionFunc::MatrixAddRow | CollectionFunc::MatrixAddCol => vec![Ref, Int, Ref],
            // (id, row:int, column:int, value:elem). Top is value.
            CollectionFunc::MatrixSet => vec![Elem, Int, Int, Ref],
            // submatrix: (id, from_row, to_row, from_column, to_column). Top is
            // to_column.
            CollectionFunc::MatrixSubmatrix => vec![Int, Int, Int, Int, Ref],
            // fill: (id, value:elem, from_row, to_row, from_column,
            // to_column). Top is to_column.
            CollectionFunc::MatrixFill => vec![Int, Int, Int, Int, Elem, Ref],
        }
    }

    /// The abstract result category (what this call pushes, if anything).
    pub fn result(self) -> CollectionAbstract {
        use CollectionAbstract::{Bool, Elem, Float, Int, Ref, Str, Void};
        match self {
            CollectionFunc::InternalDistanceScanEligible => Bool,
            CollectionFunc::InternalDistanceScan => Float,
            // Constructors and shallow copies push a new handle.
            CollectionFunc::ArrayNewFloat
            | CollectionFunc::ArrayNewInt
            | CollectionFunc::ArrayNewBool
            | CollectionFunc::ArrayNewString
            | CollectionFunc::ArrayNewColor
            | CollectionFunc::ArrayNewObject
            | CollectionFunc::ArrayFrom
            | CollectionFunc::StrSplit
            | CollectionFunc::ArraySlice
            | CollectionFunc::ArrayCopy
            // concat returns the (mutated) receiver handle.
            | CollectionFunc::ArrayConcat
            // abs / standardize / sort_indices build a new array handle.
            | CollectionFunc::ArrayAbs
            | CollectionFunc::ArrayStandardize
            | CollectionFunc::ArraySortIndices
            // Map constructors, key/value array projections, and copy.
            | CollectionFunc::MapNew
            | CollectionFunc::MapKeys
            | CollectionFunc::MapValues
            | CollectionFunc::MapCopy => Ref,
            // Numeric aggregates always return float.
            CollectionFunc::ArraySum
            | CollectionFunc::ArrayAvg
            | CollectionFunc::ArrayMin
            | CollectionFunc::ArrayMax
            | CollectionFunc::ArrayMedian
            | CollectionFunc::ArrayRange
            | CollectionFunc::ArrayMode
            | CollectionFunc::ArrayVariance
            | CollectionFunc::ArrayStdev
            | CollectionFunc::ArrayCovariance
            | CollectionFunc::ArrayPercentRank
            | CollectionFunc::ArrayPercentileNearestRank
            | CollectionFunc::ArrayPercentileLinearInterpolation => Float,
            // Element-returning accessors.
            CollectionFunc::ArrayPop
            | CollectionFunc::ArrayShift
            | CollectionFunc::ArrayRemove
            | CollectionFunc::ArrayGet
            | CollectionFunc::ArrayFirst
            | CollectionFunc::ArrayLast
            // map.get / map.put / map.remove return a value element (or na).
            | CollectionFunc::MapGet
            | CollectionFunc::MapPut
            | CollectionFunc::MapRemove => Elem,
            CollectionFunc::ArraySize
            | CollectionFunc::ArrayIndexOf
            | CollectionFunc::ArrayLastIndexOf
            | CollectionFunc::ArrayBinarySearch
            | CollectionFunc::ArrayBinarySearchLeftmost
            | CollectionFunc::ArrayBinarySearchRightmost
            | CollectionFunc::MapSize => Int,
            CollectionFunc::ArrayIncludes
            | CollectionFunc::ArrayEvery
            | CollectionFunc::ArraySome
            | CollectionFunc::MapContains => Bool,
            CollectionFunc::ArrayJoin => Str,
            // Void mutators.
            CollectionFunc::ArrayPush
            | CollectionFunc::ArraySet
            | CollectionFunc::ArrayClear
            | CollectionFunc::ArrayUnshift
            | CollectionFunc::ArrayInsert
            | CollectionFunc::ArrayReverse
            | CollectionFunc::ArrayFill
            | CollectionFunc::ArraySort
            | CollectionFunc::MatrixSort
            | CollectionFunc::MapClear
            | CollectionFunc::MapPutAll => Void,

            // ---- matrix.* -------------------------------------------------
            // New handles: construction, copy, elementwise/linear-algebra
            // results, transpose/submatrix, and concat (the mutated receiver).
            CollectionFunc::MatrixNew
            | CollectionFunc::MatrixCopy
            | CollectionFunc::MatrixTranspose
            | CollectionFunc::MatrixSubmatrix
            | CollectionFunc::MatrixSum
            | CollectionFunc::MatrixDiff
            | CollectionFunc::MatrixMult
            | CollectionFunc::MatrixInv
            | CollectionFunc::MatrixPow
            | CollectionFunc::MatrixKron
            | CollectionFunc::MatrixConcat
            // row / col and explicit-index removals produce a fresh array<T>
            // handle;
            // eigenvalues projects a fresh array<float>; eigenvectors / pinv
            // build a fresh matrix handle.
            | CollectionFunc::MatrixRow
            | CollectionFunc::MatrixCol
            | CollectionFunc::MatrixRemoveRow
            | CollectionFunc::MatrixRemoveCol
            | CollectionFunc::MatrixEigenvalues
            | CollectionFunc::MatrixEigenvectors
            | CollectionFunc::MatrixPinv => Ref,
            // Scalar float reductions.
            CollectionFunc::MatrixDet
            | CollectionFunc::MatrixTrace
            | CollectionFunc::MatrixAvg
            | CollectionFunc::MatrixMin
            | CollectionFunc::MatrixMax
            | CollectionFunc::MatrixMedian
            | CollectionFunc::MatrixMode => Float,
            // Int shape queries.
            CollectionFunc::MatrixRows
            | CollectionFunc::MatrixColumns
            | CollectionFunc::MatrixElementsCount
            | CollectionFunc::MatrixRank => Int,
            // Cell accessor returns the element type.
            CollectionFunc::MatrixGet => Elem,
            // Predicates.
            CollectionFunc::MatrixIsSquare
            | CollectionFunc::MatrixIsIdentity
            | CollectionFunc::MatrixIsDiagonal
            | CollectionFunc::MatrixIsSymmetric
            | CollectionFunc::MatrixIsAntisymmetric
            | CollectionFunc::MatrixIsTriangular
            | CollectionFunc::MatrixIsStochastic
            | CollectionFunc::MatrixIsBinary
            | CollectionFunc::MatrixIsZero
            | CollectionFunc::MatrixIsAntidiagonal => Bool,
            // Void mutators.
            CollectionFunc::MatrixSet
            | CollectionFunc::MatrixFill
            | CollectionFunc::MatrixAddRow
            | CollectionFunc::MatrixAddCol
            | CollectionFunc::MatrixSwapRows
            | CollectionFunc::MatrixSwapColumns
            | CollectionFunc::MatrixReverse
            | CollectionFunc::MatrixReshape => Void,
        }
    }
}
