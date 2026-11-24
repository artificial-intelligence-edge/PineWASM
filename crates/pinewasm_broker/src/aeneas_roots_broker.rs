//! Charon/Aeneas extraction roots for the broker kernel twins in `num.rs`.
//! Compiled only with `--features aeneas`; every item is a
//! free function because `--start-from` cannot name inherent methods at
//! the pinned Charon revision. Each root calls the generic twin of one shipped kernel;
//! the twins are generic over `BrokerNum`, and no root instantiates them at
//! `f64`.

use crate::num::{
    compute_generic, fifo_close_generic, fill_plan_generic, position_sign_generic,
    record_counters_generic, sign_boundary_generic, BrokerNum, ClosedQty, Counters, MetricsOf,
    PlanOrder, PnlRow, QtyCtx, QtyLot,
};
use alloc::vec::Vec;

/// Root for the three-way position sign (`process_fills`).
pub fn broker_position_sign<Q: BrokerNum>(size: Q) -> Q {
    position_sign_generic(size)
}

/// Root for `process_fills`' persistent-exit lifetime branch.
pub fn broker_sign_boundary<Q: BrokerNum>(before: Q, after: Q) -> u8 {
    sign_boundary_generic(before, after)
}

/// Root for `execute_fill`'s close/open plan.
pub fn broker_fill_plan<Q: BrokerNum>(
    ctx: &QtyCtx<Q>,
    order: PlanOrder,
    position: Q,
    sign: Q,
    base_requested: Q,
) -> (Q, Q, Q) {
    fill_plan_generic(ctx, order, position, sign, base_requested)
}

/// Root for `execute_fill`'s FIFO closing loop.
pub fn broker_fifo_close<Q: BrokerNum>(
    ctx: &QtyCtx<Q>,
    lots: &mut Vec<QtyLot<Q>>,
    close_qty: Q,
    closed_rows: &mut Vec<ClosedQty<Q>>,
) -> Q {
    fifo_close_generic(ctx, lots, close_qty, closed_rows)
}

/// Root for `record_closed_trade`'s incremental counters.
pub fn broker_record_counters<Q: BrokerNum>(acc: &mut Counters<Q>, pnl: Q) {
    record_counters_generic(acc, pnl);
}

/// Root for `strategy_metrics::compute`, the in-repo fold.
pub fn broker_compute_metrics<Q: BrokerNum>(
    trades: &[PnlRow<Q>],
    initial_capital: Q,
    open_profit: Q,
) -> MetricsOf<Q> {
    compute_generic(trades, initial_capital, open_profit)
}
