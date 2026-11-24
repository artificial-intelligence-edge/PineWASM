//! Broker performance metrics.
//!
//! Pure, side-effect-free computation of the `strategy.*` summary metrics from
//! a list of closed [`StrategyTrade`]s, the account `initial_capital`, and the
//! per-bar unrealized open profit. This is the exact arithmetic every `strategy.*`
//! state getter returns; the getter builtins that expose these values to a
//! script (`strategy.netprofit`, `strategy.wintrades`, …) read the broker's
//! per-bar snapshot, which is built from [`StrategyMetrics`].
//!
//! These are PineWASM's implemented strategy metric semantics. Each definition
//! below is pinned by a value-asserting unit test in this module; details not
//! settled by the Pine Script® language documentation remain project
//! assumptions.
//!
//! * `grossprofit` — sum of the profits of winning trades (profit > 0). Positive.
//! * `grossloss` — the **absolute value** of the sum of the profits of losing
//!   trades (profit < 0). Reported as a non-negative number.
//! * `netprofit` — `grossprofit - grossloss` (equivalently the sum of every
//!   closed trade's profit).
//! * `wintrades` / `losstrades` / `eventrades` — counts of closed trades with
//!   profit `> 0` / `< 0` / `== 0`.
//! * `closedtrades` — total number of closed trades.
//! * `avg_trade` — `netprofit / closedtrades`, or `0.0` when there are no closed
//!   trades (the getter surfaces `na` in that case; the raw metric stays `0.0`).
//! * `equity` — `initial_capital + netprofit + openprofit`. The final-bar value.
//! * `max_drawdown` — the largest peak-to-trough drop of the running realized
//!   equity curve (`initial_capital + cumulative realized profit`), as a
//!   non-negative number. `openprofit` does not enter the realized curve.
//! * `max_runup` — the largest trough-to-peak rise of that same realized equity
//!   curve, as a non-negative number.

use crate::StrategyTrade;

/// The computed `strategy.*` summary metrics for one run. Field names mirror the
/// `strategy.*` getter names. All monetary values are in account currency; counts are
/// exact. `avg_trade` is `0.0` (not `na`) when `closed_trades == 0` — the getter
/// layer maps the empty-trades case to `na`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StrategyMetrics {
    /// `strategy.netprofit` — sum of every closed trade's profit.
    pub net_profit: f64,
    /// `strategy.grossprofit` — sum of winning-trade profits (>= 0).
    pub gross_profit: f64,
    /// `strategy.grossloss` — absolute value of the summed losing-trade profits (>= 0).
    pub gross_loss: f64,
    /// `strategy.openprofit` — unrealized profit of the open position at the final bar.
    pub open_profit: f64,
    /// `strategy.closedtrades` — total number of closed trades.
    pub closed_trades: u32,
    /// `strategy.wintrades` — closed trades with profit > 0.
    pub win_trades: u32,
    /// `strategy.losstrades` — closed trades with profit < 0.
    pub loss_trades: u32,
    /// `strategy.eventrades` — closed trades with profit == 0.
    pub even_trades: u32,
    /// `strategy.avg_trade` — `net_profit / closed_trades` (0.0 when none).
    pub avg_trade: f64,
    /// `strategy.max_drawdown` — largest peak-to-trough drop of the realized
    /// equity curve (>= 0).
    pub max_drawdown: f64,
    /// `strategy.max_runup` — largest trough-to-peak rise of the realized
    /// equity curve (>= 0).
    pub max_runup: f64,
}

/// Compute the [`StrategyMetrics`] for a run from its closed `trades`, the account
/// `initial_capital`, and the final-bar `open_profit`.
///
/// Trades are consumed in `trades` order, which the broker produces in close
/// order (oldest first); the drawdown/run-up curve walks them in that order.
pub fn compute(
    trades: &[StrategyTrade],
    initial_capital: f64,
    open_profit: f64,
) -> StrategyMetrics {
    let mut gross_profit = 0.0;
    let mut gross_loss = 0.0;
    let mut win_trades = 0u32;
    let mut loss_trades = 0u32;
    let mut even_trades = 0u32;

    // Realized equity curve: peak/trough tracking over cumulative realized
    // profit, starting from the account's initial capital (the flat baseline).
    let mut realized = initial_capital;
    let mut peak = initial_capital;
    let mut trough = initial_capital;
    let mut max_drawdown = 0.0;
    let mut max_runup = 0.0;

    for trade in trades {
        let pnl = trade.pnl;
        if pnl > 0.0 {
            gross_profit += pnl;
            win_trades += 1;
        } else if pnl < 0.0 {
            gross_loss += -pnl;
            loss_trades += 1;
        } else {
            even_trades += 1;
        }

        realized += pnl;
        // Drawdown is tracked against the running maximum (peak) of the curve;
        // run-up against the running minimum (trough). The two extremes are
        // independent — a new peak never erases the trough, and vice versa — so
        // the whole-run worst drop and best rise are each tracked.
        if realized > peak {
            peak = realized;
        }
        if realized < trough {
            trough = realized;
        }
        let drawdown = peak - realized;
        if drawdown > max_drawdown {
            max_drawdown = drawdown;
        }
        let runup = realized - trough;
        if runup > max_runup {
            max_runup = runup;
        }
    }

    let net_profit = gross_profit - gross_loss;
    let closed_trades = win_trades + loss_trades + even_trades;
    let avg_trade = if closed_trades == 0 {
        0.0
    } else {
        net_profit / closed_trades as f64
    };

    StrategyMetrics {
        net_profit,
        gross_profit,
        gross_loss,
        open_profit,
        closed_trades,
        win_trades,
        loss_trades,
        even_trades,
        avg_trade,
        max_drawdown,
        max_runup,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    fn trade(pnl: f64) -> StrategyTrade {
        StrategyTrade {
            index: None,
            entry_id: "e".to_string(),
            entry_bar: 0,
            entry_price: 100.0,
            exit_bar: 1,
            exit_price: 100.0 + pnl,
            qty: 1.0,
            comment: None,
            pnl,
            cumulative_pnl: None,
            entry_notional: None,
            sign: 1.0,
            commission: 0.0,
            max_runup: 0.0,
            max_drawdown: 0.0,
            exit_id: None,
            exit_order_name: None,
            entry_comment: None,
            entry_order_ordinal: 0,
            exit_order_ordinal: 1,
            commission_split: None,
        }
    }

    #[test]
    fn empty_trade_list_is_all_zero() {
        let m = compute(&[], 10_000.0, 0.0);
        assert_eq!(m.net_profit, 0.0);
        assert_eq!(m.gross_profit, 0.0);
        assert_eq!(m.gross_loss, 0.0);
        assert_eq!(m.closed_trades, 0);
        assert_eq!(m.win_trades, 0);
        assert_eq!(m.loss_trades, 0);
        assert_eq!(m.even_trades, 0);
        assert_eq!(m.avg_trade, 0.0);
        assert_eq!(m.max_drawdown, 0.0);
        assert_eq!(m.max_runup, 0.0);
        assert_eq!(m.open_profit, 0.0);
    }

    #[test]
    fn gross_and_net_hand_computed() {
        // +30, -10, +5, -25  => gross_profit 35, gross_loss 35, net 0.
        let trades = [trade(30.0), trade(-10.0), trade(5.0), trade(-25.0)];
        let m = compute(&trades, 1_000.0, 0.0);
        assert_eq!(m.gross_profit, 35.0);
        assert_eq!(m.gross_loss, 35.0);
        assert_eq!(m.net_profit, 0.0);
    }

    #[test]
    fn win_loss_even_counts() {
        let trades = [
            trade(12.0),
            trade(-3.0),
            trade(0.0),
            trade(4.0),
            trade(0.0),
            trade(-8.0),
        ];
        let m = compute(&trades, 500.0, 0.0);
        assert_eq!(m.win_trades, 2);
        assert_eq!(m.loss_trades, 2);
        assert_eq!(m.even_trades, 2);
        assert_eq!(m.closed_trades, 6);
    }

    #[test]
    fn avg_trade_is_net_over_closed() {
        // net = 40 - 10 = 30 over 3 trades => 10.0.
        let trades = [trade(40.0), trade(-10.0), trade(0.0)];
        let m = compute(&trades, 0.0, 0.0);
        assert_eq!(m.net_profit, 30.0);
        assert_eq!(m.closed_trades, 3);
        assert_eq!(m.avg_trade, 10.0);
    }

    #[test]
    fn max_drawdown_hand_computed() {
        // Realized curve from 1000: +100 ->1100 (peak), -300 ->800,
        // -50 ->750 (trough, dd from peak 1100 = 350), +200 ->950.
        // Largest peak-to-trough drop is 1100 - 750 = 350.
        let trades = [trade(100.0), trade(-300.0), trade(-50.0), trade(200.0)];
        let m = compute(&trades, 1_000.0, 0.0);
        assert_eq!(m.max_drawdown, 350.0);
    }

    #[test]
    fn max_runup_hand_computed() {
        // Realized curve from 1000: -200 ->800 (trough), +500 ->1300,
        // +100 ->1400 (peak). Largest trough-to-peak rise is 1400 - 800 = 600.
        let trades = [trade(-200.0), trade(500.0), trade(100.0)];
        let m = compute(&trades, 1_000.0, 0.0);
        assert_eq!(m.max_runup, 600.0);
    }

    #[test]
    fn drawdown_measured_from_running_peak_not_start() {
        // Curve climbs to a new peak before falling: +500 ->1500 (peak),
        // -400 ->1100. Drawdown is 1500 - 1100 = 400, not measured from 1000.
        let trades = [trade(500.0), trade(-400.0)];
        let m = compute(&trades, 1_000.0, 0.0);
        assert_eq!(m.max_drawdown, 400.0);
        // And a monotonically rising curve has zero drawdown.
        let rising = [trade(10.0), trade(20.0), trade(30.0)];
        assert_eq!(compute(&rising, 0.0, 0.0).max_drawdown, 0.0);
    }

    #[test]
    fn open_profit_passthrough_and_final_equity() {
        // Equity = initial + net + open. net = 50, open = 12.5.
        let trades = [trade(50.0)];
        let m = compute(&trades, 10_000.0, 12.5);
        assert_eq!(m.open_profit, 12.5);
        let equity = 10_000.0 + m.net_profit + m.open_profit;
        assert_eq!(equity, 10_062.5);
    }
}
