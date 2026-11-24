//! Generic twins of six broker kernels, for Charon/Aeneas extraction.
//!
//! This module is compiled only for tests and the `aeneas` feature. Shipped
//! builds compile none of it, and the shipped broker (`broker.rs`,
//! `strategy_metrics.rs`) does not refer to it.
//!
//! Each twin is a plain generic function over [`BrokerNum`] that restates one
//! shipped `f64` kernel operation for operation:
//!
//! * [`position_sign_generic`]: `Broker::position_sign_for`, the three-way
//!   position sign read by `process_fills`.
//! * [`sign_boundary_generic`]: `process_fills`' persistent-exit lifetime
//!   branch.
//! * [`fill_plan_generic`]: `execute_fill`'s close/open plan.
//! * [`fifo_close_generic`]: the quantity projection of `execute_fill`'s FIFO
//!   closing loop, for `close_entries_rule = "FIFO"` (lot index `0`). Trade
//!   rows, P&L, commission and excursions are not modelled.
//! * [`record_counters_generic`]: `record_closed_trade`'s incremental
//!   counters.
//! * [`compute_generic`]: `strategy_metrics::compute`, the reference fold.
//!
//! Correspondence with the shipped code is by test, not by construction: the
//! twins are separate source. The tests below run each twin at
//! `impl BrokerNum for f64` against the shipped broker through its public API
//! where the kernel is reachable (FIFO close and plan, counters, `compute`,
//! quantization), against verbatim copies of the shipped code, and against
//! literal expected values (the sign, the FIFO cleanup constants).
//!
//! Numeric contract: the broker's contract is the shipped binary64 operation
//! order. The Lean package proves the twins at an exact-integer instance of
//! [`BrokerNum`] only. Those theorems do not transfer to the `f64` instance
//! (FIFO roundoff cleanup and pro-rata commission have no integer
//! counterpart), and nothing here makes a decimal or error-bound claim about
//! `f64`.

/// The numeric operations the broker kernels use. `impl BrokerNum for f64`
/// is the shipped reading (each method is the primitive `f64` expression the
/// shipped kernel uses); the Lean package supplies an exact-integer instance.
pub trait BrokerNum: Copy {
    fn zero() -> Self;
    fn from_u32(n: u32) -> Self;
    fn add(a: Self, b: Self) -> Self;
    fn sub(a: Self, b: Self) -> Self;
    fn div(a: Self, b: Self) -> Self;
    fn neg(a: Self) -> Self;
    fn abs(a: Self) -> Self;
    fn signum(a: Self) -> Self;
    fn min(a: Self, b: Self) -> Self;
    fn max0(a: Self) -> Self;
    fn mul(a: Self, b: Self) -> Self;
    fn gt_zero(a: Self) -> bool;
    fn lt_zero(a: Self) -> bool;
    fn eq_zero(a: Self) -> bool;
    fn eq(a: Self, b: Self) -> bool;
    fn lt(a: Self, b: Self) -> bool;
    fn le(a: Self, b: Self) -> bool;
    /// The FIFO loop's "something left to close" test (`> 1e-12` in `f64`).
    fn above_eps(a: Self) -> bool;
    /// The FIFO loop's dust bound for a lot of `lot_qty` at increment
    /// `mincontract`: a remainder at or below it removes the lot.
    fn roundoff(lot_qty: Self, mincontract: Self) -> Self;
    /// `floor_to_increment(qty, increment)`: quantity quantization.
    fn quantize(qty: Self, increment: Self) -> Self;
}

/// The `f64` primitives behind `impl BrokerNum for f64`, one per method: the
/// expressions the shipped kernels use.
mod f64_ops {
    pub(super) fn zero() -> f64 {
        0.0
    }
    pub(super) fn from_u32(n: u32) -> f64 {
        n as f64
    }
    pub(super) fn add(a: f64, b: f64) -> f64 {
        a + b
    }
    pub(super) fn sub(a: f64, b: f64) -> f64 {
        a - b
    }
    pub(super) fn div(a: f64, b: f64) -> f64 {
        a / b
    }
    pub(super) fn neg(a: f64) -> f64 {
        -a
    }
    pub(super) fn abs(a: f64) -> f64 {
        a.abs()
    }
    pub(super) fn signum(a: f64) -> f64 {
        a.signum()
    }
    pub(super) fn min(a: f64, b: f64) -> f64 {
        a.min(b)
    }
    pub(super) fn max0(a: f64) -> f64 {
        a.max(0.0)
    }
    pub(super) fn mul(a: f64, b: f64) -> f64 {
        a * b
    }
    pub(super) fn gt_zero(a: f64) -> bool {
        a > 0.0
    }
    pub(super) fn lt_zero(a: f64) -> bool {
        a < 0.0
    }
    #[allow(clippy::float_cmp)]
    pub(super) fn eq_zero(a: f64) -> bool {
        a == 0.0
    }
    #[allow(clippy::float_cmp)]
    pub(super) fn eq(a: f64, b: f64) -> bool {
        a == b
    }
    pub(super) fn lt(a: f64, b: f64) -> bool {
        a < b
    }
    pub(super) fn le(a: f64, b: f64) -> bool {
        a <= b
    }
    pub(super) fn above_eps(a: f64) -> bool {
        a > 1e-12
    }
    pub(super) fn roundoff(lot_qty: f64, mincontract: f64) -> f64 {
        (lot_qty * f64::EPSILON * 8.0)
            .max(1e-12)
            .min(mincontract * 0.5)
    }
    /// A verbatim copy of `broker.rs`'s private `floor_to_increment`.
    pub(super) fn quantize(qty: f64, increment: f64) -> f64 {
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
}

impl BrokerNum for f64 {
    fn zero() -> Self {
        f64_ops::zero()
    }
    fn from_u32(n: u32) -> Self {
        f64_ops::from_u32(n)
    }
    fn add(a: Self, b: Self) -> Self {
        f64_ops::add(a, b)
    }
    fn sub(a: Self, b: Self) -> Self {
        f64_ops::sub(a, b)
    }
    fn div(a: Self, b: Self) -> Self {
        f64_ops::div(a, b)
    }
    fn neg(a: Self) -> Self {
        f64_ops::neg(a)
    }
    fn abs(a: Self) -> Self {
        f64_ops::abs(a)
    }
    fn signum(a: Self) -> Self {
        f64_ops::signum(a)
    }
    fn min(a: Self, b: Self) -> Self {
        f64_ops::min(a, b)
    }
    fn max0(a: Self) -> Self {
        f64_ops::max0(a)
    }
    fn mul(a: Self, b: Self) -> Self {
        f64_ops::mul(a, b)
    }
    fn gt_zero(a: Self) -> bool {
        f64_ops::gt_zero(a)
    }
    fn lt_zero(a: Self) -> bool {
        f64_ops::lt_zero(a)
    }
    fn eq_zero(a: Self) -> bool {
        f64_ops::eq_zero(a)
    }
    fn eq(a: Self, b: Self) -> bool {
        f64_ops::eq(a, b)
    }
    fn lt(a: Self, b: Self) -> bool {
        f64_ops::lt(a, b)
    }
    fn le(a: Self, b: Self) -> bool {
        f64_ops::le(a, b)
    }
    fn above_eps(a: Self) -> bool {
        f64_ops::above_eps(a)
    }
    fn roundoff(lot_qty: Self, mincontract: Self) -> Self {
        f64_ops::roundoff(lot_qty, mincontract)
    }
    fn quantize(qty: Self, increment: Self) -> Self {
        f64_ops::quantize(qty, increment)
    }
}

// ---------------------------------------------------------------------------
// Generic twins
// ---------------------------------------------------------------------------

pub use twins::*;

mod twins {
    use super::BrokerNum;
    use alloc::vec::Vec;

    /// Three-way sign of a signed position size: zero when flat,
    /// otherwise `signum`. The twin of `Broker::position_sign_for`.
    pub fn position_sign_generic<Q: BrokerNum>(size: Q) -> Q {
        if Q::eq_zero(size) {
            Q::zero()
        } else {
            Q::signum(size)
        }
    }

    /// `process_fills`' persistent-exit lifetime branch on the three-way signs
    /// read before and after one fill: `0` keeps everything, `1` is the flat
    /// boundary, `2` is a reversal.
    pub fn sign_boundary_generic<Q: BrokerNum>(before: Q, after: Q) -> u8 {
        let mut boundary = 0u8;
        if Q::eq_zero(after) {
            boundary = 1;
        } else if !Q::eq_zero(before) && !Q::eq(after, before) {
            boundary = 2;
        }
        boundary
    }

    /// The order fields the plan reads.
    #[derive(Debug, Clone, Copy)]
    pub struct PlanOrder {
        pub is_entry: bool,
        pub reduce_only: bool,
    }

    /// The broker configuration the plan and the FIFO loop read.
    #[derive(Debug, Clone, Copy)]
    pub struct QtyCtx<Q> {
        pub mincontract: Q,
    }

    impl<Q: BrokerNum> QtyCtx<Q> {
        /// `Broker::quantize_qty`.
        pub fn quantize_qty(&self, qty: Q) -> Q {
            Q::quantize(qty, self.mincontract)
        }
    }

    /// `execute_fill`'s close/open plan against the signed `position`, for an
    /// order of direction `sign` (`+1`/`-1`) and requested size
    /// `base_requested`: `(closing_capacity, close_qty, open_qty)`.
    #[allow(unused_mut)]
    pub fn fill_plan_generic<Q: BrokerNum>(
        ctx: &QtyCtx<Q>,
        order: PlanOrder,
        position: Q,
        sign: Q,
        base_requested: Q,
    ) -> (Q, Q, Q) {
        let closing_capacity = if !Q::eq_zero(position) && !Q::eq(Q::signum(position), sign) {
            Q::abs(position)
        } else {
            Q::zero()
        };
        let close_qty = if Q::gt_zero(closing_capacity) {
            if order.is_entry {
                closing_capacity
            } else {
                Q::min(base_requested, closing_capacity)
            }
        } else {
            Q::zero()
        };
        let mut open_qty = if order.reduce_only {
            Q::zero()
        } else if order.is_entry {
            base_requested
        } else {
            ctx.quantize_qty(Q::max0(Q::sub(base_requested, close_qty)))
        };
        (closing_capacity, close_qty, open_qty)
    }

    /// One open lot as the FIFO loop sees it: its identity and signed
    /// remaining quantity.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct QtyLot<Q> {
        pub id: u64,
        pub qty: Q,
    }

    /// One closed-trade row as the FIFO loop sees it: the closed lot's
    /// identity and the quantity closed.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct ClosedQty<Q> {
        pub lot: u64,
        pub qty: Q,
    }

    /// `execute_fill`'s FIFO closing loop under `close_entries_rule = "FIFO"`,
    /// projected to quantities: closes up to
    /// `close_qty` from the front of `lots`, appends one row per closed lot
    /// to `closed`, and returns the total closed quantity.
    pub fn fifo_close_generic<Q: BrokerNum>(
        ctx: &QtyCtx<Q>,
        lots: &mut Vec<QtyLot<Q>>,
        close_qty: Q,
        closed_rows: &mut Vec<ClosedQty<Q>>,
    ) -> Q {
        let mut filled_qty = Q::zero();
        if Q::gt_zero(close_qty) {
            let mut to_close = close_qty;
            while Q::above_eps(to_close) && !lots.is_empty() {
                let lot_index = 0;
                let (lot_qty, dir) = {
                    let lot = &lots[lot_index];
                    (Q::abs(lot.qty), Q::signum(lot.qty))
                };
                let closed = Q::min(to_close, lot_qty);
                filled_qty = Q::add(filled_qty, closed);
                closed_rows.push(ClosedQty {
                    lot: lots[lot_index].id,
                    qty: closed,
                });
                // Always clear a remainder that is indistinguishable from
                // roundoff even when the request covers the whole position.
                let new_qty = Q::sub(lot_qty, closed);
                let qty_roundoff = Q::roundoff(lot_qty, ctx.mincontract);
                if Q::le(new_qty, qty_roundoff) {
                    lots.remove(lot_index);
                } else {
                    lots[lot_index].qty = Q::mul(new_qty, dir);
                }
                to_close = Q::sub(to_close, closed);
            }
        }
        filled_qty
    }

    /// The counters `record_closed_trade` updates.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct Counters<Q> {
        pub closed_trades_total: u32,
        pub gross_profit: Q,
        pub gross_loss: Q,
        pub win_trades: u32,
        pub loss_trades: u32,
        pub even_trades: u32,
    }

    /// `record_closed_trade`'s incremental counters for one closed trade of
    /// profit `pnl`, in the report's branch order (win, loss, even).
    pub fn record_counters_generic<Q: BrokerNum>(acc: &mut Counters<Q>, pnl: Q) {
        acc.closed_trades_total = acc.closed_trades_total.saturating_add(1);
        if Q::gt_zero(pnl) {
            acc.gross_profit = Q::add(acc.gross_profit, pnl);
            acc.win_trades = acc.win_trades.saturating_add(1);
        } else if Q::lt_zero(pnl) {
            acc.gross_loss = Q::add(acc.gross_loss, Q::neg(pnl));
            acc.loss_trades = acc.loss_trades.saturating_add(1);
        } else {
            acc.even_trades = acc.even_trades.saturating_add(1);
        }
    }

    /// A closed trade as `compute` sees it.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct PnlRow<Q> {
        pub pnl: Q,
    }

    /// `StrategyMetrics` over `Q`.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct MetricsOf<Q> {
        pub net_profit: Q,
        pub gross_profit: Q,
        pub gross_loss: Q,
        pub open_profit: Q,
        pub closed_trades: u32,
        pub win_trades: u32,
        pub loss_trades: u32,
        pub even_trades: u32,
        pub avg_trade: Q,
        pub max_drawdown: Q,
        pub max_runup: Q,
    }

    /// Compute the [`MetricsOf`] for a run from its closed `trades`, the
    /// account `initial_capital`, and the final-bar `open_profit`. The
    /// twin of `strategy_metrics::compute`.
    pub fn compute_generic<Q: BrokerNum>(
        trades: &[PnlRow<Q>],
        initial_capital: Q,
        open_profit: Q,
    ) -> MetricsOf<Q> {
        let mut gross_profit = Q::zero();
        let mut gross_loss = Q::zero();
        let mut win_trades = 0u32;
        let mut loss_trades = 0u32;
        let mut even_trades = 0u32;

        // Realized equity curve: peak/trough tracking over cumulative
        // realized profit, starting from the account's initial capital.
        let mut realized = initial_capital;
        let mut peak = initial_capital;
        let mut trough = initial_capital;
        let mut max_drawdown = Q::zero();
        let mut max_runup = Q::zero();

        for trade in trades {
            let pnl = trade.pnl;
            if Q::gt_zero(pnl) {
                gross_profit = Q::add(gross_profit, pnl);
                win_trades += 1;
            } else if Q::lt_zero(pnl) {
                gross_loss = Q::add(gross_loss, Q::neg(pnl));
                loss_trades += 1;
            } else {
                even_trades += 1;
            }

            realized = Q::add(realized, pnl);
            if Q::lt(peak, realized) {
                peak = realized;
            }
            if Q::lt(realized, trough) {
                trough = realized;
            }
            let drawdown = Q::sub(peak, realized);
            if Q::lt(max_drawdown, drawdown) {
                max_drawdown = drawdown;
            }
            let runup = Q::sub(realized, trough);
            if Q::lt(max_runup, runup) {
                max_runup = runup;
            }
        }

        let net_profit = Q::sub(gross_profit, gross_loss);
        let closed_trades = win_trades + loss_trades + even_trades;
        let avg_trade = if closed_trades == 0 {
            Q::zero()
        } else {
            Q::div(net_profit, Q::from_u32(closed_trades))
        };

        MetricsOf {
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
}

#[cfg(test)]
mod tests {
    use super::f64_ops::quantize as floor_to_increment;
    use super::*;
    use crate::{Broker, BrokerConfig, OrderMetadata};
    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;

    /// Random-case count: the full count natively, a small one under Miri,
    /// whose soft-float interpreter is orders of magnitude slower.
    fn cases(native: usize) -> usize {
        if cfg!(miri) {
            native.min(64)
        } else {
            native
        }
    }

    /// Deterministic xorshift64* stream for the differential tests.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        /// A quantity-like value: small integers, decimals, specials.
        fn qty(&mut self) -> f64 {
            match self.below(8) {
                0 => 0.0,
                1 => -0.0,
                2 => self.below(10) as f64,
                3 => self.below(1000) as f64 / 100.0,
                4 => 0.1 * self.below(40) as f64,
                5 => f64::from_bits(self.next() >> 2),
                6 => 1e-13 * self.below(3) as f64,
                _ => self.below(7) as f64 * 0.25,
            }
        }
        fn signed(&mut self) -> f64 {
            let q = self.qty();
            if self.below(2) == 0 {
                q
            } else {
                -q
            }
        }
    }

    fn same(a: f64, b: f64) -> bool {
        a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
    }

    /// `Broker::position_sign_for`, verbatim (it is private to `broker.rs`).
    #[allow(clippy::float_cmp)]
    fn reference_position_sign(size: f64) -> f64 {
        if size == 0.0 {
            0.0
        } else {
            size.signum()
        }
    }

    #[test]
    fn position_sign_twin_matches_the_reference() {
        // Literal pins first, so the reference copy itself is checked: both
        // zeros read as `+0.0`, and `signum` passes NaN through.
        for (x, want) in [
            (0.0, 0.0),
            (-0.0, 0.0),
            (1.0, 1.0),
            (-1.0, -1.0),
            (f64::MIN_POSITIVE, 1.0),
            (-f64::MIN_POSITIVE, -1.0),
            (f64::INFINITY, 1.0),
            (f64::NEG_INFINITY, -1.0),
            (1e-300, 1.0),
        ] {
            assert_eq!(reference_position_sign(x).to_bits(), f64::to_bits(want));
            assert_eq!(
                position_sign_generic::<f64>(x).to_bits(),
                f64::to_bits(want)
            );
        }
        assert!(reference_position_sign(f64::NAN).is_nan());
        assert!(position_sign_generic::<f64>(f64::NAN).is_nan());
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..cases(100_000) {
            let x = rng.signed();
            let want = reference_position_sign(x);
            assert!(same(position_sign_generic::<f64>(x), want));
        }
    }

    /// The `process_fills` branch conditions, verbatim.
    #[allow(clippy::float_cmp)]
    fn reference_boundary(before: f64, after: f64) -> u8 {
        let position_sign_before = before;
        let position_sign_after = after;
        if position_sign_after == 0.0 {
            1
        } else if position_sign_before != 0.0 && position_sign_after != position_sign_before {
            2
        } else {
            0
        }
    }

    #[test]
    fn sign_boundary_twin_matches_the_reference() {
        let signs = [-1.0, -0.0, 0.0, 1.0, f64::NAN];
        for before in signs {
            for after in signs {
                assert_eq!(
                    sign_boundary_generic::<f64>(before, after),
                    reference_boundary(before, after)
                );
            }
        }
    }

    /// The `execute_fill` plan, verbatim (with `quantize_qty`
    /// inlined as `floor_to_increment(.., mincontract)`).
    #[allow(clippy::float_cmp)]
    fn reference_plan(
        mincontract: f64,
        is_entry: bool,
        reduce_only: bool,
        position: f64,
        sign: f64,
        base_requested: f64,
    ) -> (f64, f64, f64) {
        let closing_capacity = if position != 0.0 && position.signum() != sign {
            position.abs()
        } else {
            0.0
        };
        let close_qty = if closing_capacity > 0.0 {
            if is_entry {
                closing_capacity
            } else {
                base_requested.min(closing_capacity)
            }
        } else {
            0.0
        };
        let open_qty = if reduce_only {
            0.0
        } else if is_entry {
            base_requested
        } else {
            floor_to_increment((base_requested - close_qty).max(0.0), mincontract)
        };
        (closing_capacity, close_qty, open_qty)
    }

    #[test]
    fn fill_plan_twin_matches_the_reference() {
        let mut rng = Rng(0x1234_5678_9abc_def1);
        for _ in 0..cases(200_000) {
            let mincontract = [1.0, 0.1, 0.01, 0.25, 0.0][rng.below(5) as usize];
            let order = PlanOrder {
                is_entry: rng.below(2) == 0,
                reduce_only: rng.below(3) == 0,
            };
            let position = rng.signed();
            let sign = if rng.below(2) == 0 { 1.0 } else { -1.0 };
            let base = rng.qty();
            let (a, b, c) =
                fill_plan_generic::<f64>(&QtyCtx { mincontract }, order, position, sign, base);
            let (x, y, z) = reference_plan(
                mincontract,
                order.is_entry,
                order.reduce_only,
                position,
                sign,
                base,
            );
            assert!(same(a, x) && same(b, y) && same(c, z));
        }
    }

    /// The shipped FIFO loop's quantity projection, verbatim, for
    /// `close_entries_rule = "FIFO"`.
    fn reference_fifo(
        mincontract: f64,
        lots: &mut Vec<QtyLot<f64>>,
        close_qty: f64,
        rows: &mut Vec<ClosedQty<f64>>,
    ) -> f64 {
        let mut filled_qty = 0.0;
        if close_qty > 0.0 {
            let mut to_close = close_qty;
            while to_close > 1e-12 && !lots.is_empty() {
                let lot_index = 0;
                let (lot_qty, dir) = {
                    let lot = &lots[lot_index];
                    (lot.qty.abs(), lot.qty.signum())
                };
                let closed = to_close.min(lot_qty);
                filled_qty += closed;
                rows.push(ClosedQty {
                    lot: lots[lot_index].id,
                    qty: closed,
                });
                let new_qty = lot_qty - closed;
                let qty_roundoff = (lot_qty * f64::EPSILON * 8.0)
                    .max(1e-12)
                    .min(mincontract * 0.5);
                if new_qty <= qty_roundoff {
                    lots.remove(lot_index);
                } else {
                    lots[lot_index].qty = new_qty * dir;
                }
                to_close -= closed;
            }
        }
        filled_qty
    }

    #[test]
    fn fifo_close_twin_matches_the_reference() {
        let mut rng = Rng(0x0bad_cafe_f00d_d00d);
        for _ in 0..cases(50_000) {
            let mincontract = [1.0, 0.1, 0.01, 0.25][rng.below(4) as usize];
            let dir = if rng.below(2) == 0 { 1.0 } else { -1.0 };
            let n = rng.below(6);
            let lots: Vec<QtyLot<f64>> = (0..n)
                .map(|id| QtyLot {
                    id,
                    qty: dir * rng.qty().abs(),
                })
                .collect();
            let close = rng.qty();
            let (mut a, mut b) = (lots.clone(), lots);
            let (mut ra, mut rb) = (Vec::new(), Vec::new());
            let fa = fifo_close_generic::<f64>(&QtyCtx { mincontract }, &mut a, close, &mut ra);
            let fb = reference_fifo(mincontract, &mut b, close, &mut rb);
            assert!(same(fa, fb));
            assert_eq!(a.len(), b.len());
            for (x, y) in a.iter().zip(&b) {
                assert!(x.id == y.id && same(x.qty, y.qty));
            }
            assert_eq!(ra.len(), rb.len());
            for (x, y) in ra.iter().zip(&rb) {
                assert!(x.lot == y.lot && same(x.qty, y.qty));
            }
        }
    }

    /// One long lot of `lot`, closed by `lot - remainder` at `mincontract`,
    /// through the `f64` twin. Returns the surviving lot quantities.
    fn fifo_remainder(lot: f64, remainder: f64, mincontract: f64) -> Vec<f64> {
        let close = lot - remainder;
        // The cases below pick remainders that `lot - close` reproduces exactly.
        assert_eq!((lot - close).to_bits(), remainder.to_bits());
        let mut lots = alloc::vec![QtyLot { id: 0, qty: lot }];
        let mut rows = Vec::new();
        let filled =
            fifo_close_generic::<f64>(&QtyCtx { mincontract }, &mut lots, close, &mut rows);
        assert_eq!(filled.to_bits(), close.to_bits());
        assert_eq!(rows, [ClosedQty { lot: 0, qty: close }]);
        lots.iter().map(|lot| lot.qty).collect()
    }

    /// The FIFO dust bound `(lot * EPSILON * 8).max(1e-12).min(mincontract / 2)`
    /// and its inclusive comparison, pinned by literal outcomes, so a
    /// change to the `f64` instance's constants cannot hide behind a reference
    /// copy that changed with it.
    #[test]
    #[allow(clippy::float_cmp)]
    fn fifo_roundoff_cleanup_constants_are_pinned() {
        // Exact powers of two. `powi` is not correctly rounded by contract,
        // and Miri perturbs it on purpose.
        let two = |e: i64| f64::from_bits(((1023 + e) as u64) << 52);
        // Absolute floor 1e-12: a small lot's relative bound (1.8e-15) is
        // below it. A remainder in (1e-12, 1e-9] is a real remainder and
        // stays; one at or below 1e-12 is dust and goes.
        assert_eq!(fifo_remainder(1.0, two(-31), 1.0), [two(-31)]); // 4.7e-10
        assert_eq!(fifo_remainder(1.0, two(-33), 1.0), [two(-33)]); // 1.2e-10
        assert_eq!(fifo_remainder(1.0, two(-39), 1.0), [two(-39)]); // 1.8e-12
        assert_eq!(fifo_remainder(1.0, two(-40), 1.0), [0.0; 0]); // 9.1e-13
                                                                  // Relative bound, factor 8: for a lot of 2^20 it is exactly 2^-29
                                                                  // (2^-30 with factor 4). A remainder strictly between the two is dust.
        let lot = two(20);
        assert_eq!(fifo_remainder(lot, 1.5 * two(-30), 1.0), [0.0; 0]);
        // Inclusive comparison: a remainder exactly at the bound is dust; the
        // next representable remainder above it stays.
        assert_eq!(fifo_remainder(lot, two(-29), 1.0), [0.0; 0]);
        let above = two(-29) + two(-33);
        assert_eq!(fifo_remainder(lot, above, 1.0), [above]);
        // Half-`mincontract` cap: with `mincontract = 2^-31` the bound drops
        // to 2^-32, so the same 1.5 * 2^-30 remainder is real and stays, and
        // a remainder exactly at the cap is dust.
        assert_eq!(
            fifo_remainder(lot, 1.5 * two(-30), two(-31)),
            [1.5 * two(-30)]
        );
        assert_eq!(fifo_remainder(lot, two(-32), two(-31)), [0.0; 0]);
    }

    /// `strategy_metrics::compute`, verbatim, over bare P&L.
    fn reference_compute(pnls: &[f64], initial_capital: f64) -> MetricsOf<f64> {
        let mut gross_profit = 0.0;
        let mut gross_loss = 0.0;
        let mut win_trades = 0u32;
        let mut loss_trades = 0u32;
        let mut even_trades = 0u32;
        let mut realized = initial_capital;
        let mut peak = initial_capital;
        let mut trough = initial_capital;
        let mut max_drawdown = 0.0;
        let mut max_runup = 0.0;
        for &pnl in pnls {
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
        MetricsOf {
            net_profit,
            gross_profit,
            gross_loss,
            open_profit: 0.0,
            closed_trades,
            win_trades,
            loss_trades,
            even_trades,
            avg_trade,
            max_drawdown,
            max_runup,
        }
    }

    fn same_metrics(a: &MetricsOf<f64>, b: &MetricsOf<f64>) -> bool {
        same(a.net_profit, b.net_profit)
            && same(a.gross_profit, b.gross_profit)
            && same(a.gross_loss, b.gross_loss)
            && same(a.open_profit, b.open_profit)
            && a.closed_trades == b.closed_trades
            && a.win_trades == b.win_trades
            && a.loss_trades == b.loss_trades
            && a.even_trades == b.even_trades
            && same(a.avg_trade, b.avg_trade)
            && same(a.max_drawdown, b.max_drawdown)
            && same(a.max_runup, b.max_runup)
    }

    #[test]
    fn compute_and_counters_twins_match_the_reference_fold() {
        let mut rng = Rng(0x5eed_5eed_5eed_5eed);
        for _ in 0..cases(20_000) {
            let n = rng.below(12) as usize;
            let pnls: Vec<f64> = (0..n).map(|_| rng.signed()).collect();
            let capital = rng.qty() * 1000.0;
            let reference = reference_compute(&pnls, capital);
            let rows: Vec<PnlRow<f64>> = pnls.iter().map(|&pnl| PnlRow { pnl }).collect();
            assert!(same_metrics(
                &compute_generic::<f64>(&rows, capital, 0.0),
                &reference
            ));
            let trades: Vec<crate::StrategyTrade> = pnls
                .iter()
                .map(|&pnl| crate::StrategyTrade {
                    index: None,
                    entry_id: alloc::string::String::new(),
                    entry_bar: 0,
                    entry_price: 100.0,
                    exit_bar: 1,
                    exit_price: 100.0,
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
                })
                .collect();
            let shipped = crate::strategy_metrics::compute(&trades, capital, 0.0);
            let shipped = MetricsOf {
                net_profit: shipped.net_profit,
                gross_profit: shipped.gross_profit,
                gross_loss: shipped.gross_loss,
                open_profit: shipped.open_profit,
                closed_trades: shipped.closed_trades,
                win_trades: shipped.win_trades,
                loss_trades: shipped.loss_trades,
                even_trades: shipped.even_trades,
                avg_trade: shipped.avg_trade,
                max_drawdown: shipped.max_drawdown,
                max_runup: shipped.max_runup,
            };
            assert!(same_metrics(&shipped, &reference));
            // The incremental counters agree with the fold's counters.
            let mut acc = Counters {
                closed_trades_total: 0,
                gross_profit: 0.0,
                gross_loss: 0.0,
                win_trades: 0,
                loss_trades: 0,
                even_trades: 0,
            };
            for &pnl in &pnls {
                record_counters_generic::<f64>(&mut acc, pnl);
            }
            assert_eq!(acc.closed_trades_total, reference.closed_trades);
            assert_eq!(
                (acc.win_trades, acc.loss_trades, acc.even_trades),
                (
                    reference.win_trades,
                    reference.loss_trades,
                    reference.even_trades
                )
            );
            assert!(same(acc.gross_profit, reference.gross_profit));
            assert!(same(acc.gross_loss, reference.gross_loss));
        }
    }

    // -- The twins against the shipped broker, through its public API --------

    fn shipped_bar(broker: &mut Broker, bar: usize, o: f64, h: f64, l: f64, c: f64) {
        broker.begin_session(bar as i64, bar, o);
        broker.begin_script_execution();
        broker
            .process_bar_ticks(bar, o, h, l, c)
            .expect("no caps set");
    }

    fn shipped_order(broker: &mut Broker, id: String, direction: f64, qty: f64) {
        broker.place_order(
            id,
            direction,
            Some(qty),
            None,
            None,
            OrderMetadata::default(),
        );
    }

    fn shipped_config() -> BrokerConfig {
        BrokerConfig {
            margin_long: 0.0,
            margin_short: 0.0,
            pyramiding: 10,
            ..BrokerConfig::default()
        }
    }

    /// The shipped broker's open lots, in order: (entry id, signed quantity).
    fn shipped_lots(broker: &Broker) -> Vec<(String, f64)> {
        (0..)
            .map_while(|i| broker.trade_view(i, true))
            .map(|view| (String::from(view.entry_id.expect("named lot")), view.size))
            .collect()
    }

    /// The shipped broker's closed trades, in order: (entry id, signed size).
    fn shipped_closed(broker: &Broker) -> Vec<(String, f64)> {
        (0..)
            .map_while(|i| broker.trade_view(i, false))
            .map(|view| (String::from(view.entry_id.expect("named lot")), view.size))
            .collect()
    }

    /// Open `entries` as separate long lots, then sell `close` with one
    /// `strategy.order`, on the shipped broker; run the plan and FIFO twins
    /// on the lots read before the close; compare lots and closed trades bit
    /// for bit.
    fn check_fifo_against_shipped(entries: &[f64], close: f64, mincontract: f64) {
        let mut broker = Broker::new_with_symbol(&shipped_config(), 0.01, 1.0, mincontract, "USD");
        shipped_bar(&mut broker, 0, 100.0, 100.0, 100.0, 100.0);
        for (i, &qty) in entries.iter().enumerate() {
            shipped_order(&mut broker, format!("o{i}"), 1.0, qty);
        }
        broker.finish_bar(0, 100.0).expect("no caps set");
        shipped_bar(&mut broker, 1, 100.0, 101.0, 99.0, 100.0);
        let before = shipped_lots(&broker);
        let position = broker.position_size();
        shipped_order(&mut broker, String::from("c"), -1.0, close);
        broker.finish_bar(1, 100.0).expect("no caps set");
        assert!(shipped_closed(&broker).is_empty());
        shipped_bar(&mut broker, 2, 100.0, 101.0, 99.0, 100.0);

        let ctx = QtyCtx { mincontract };
        let order = PlanOrder {
            is_entry: false,
            reduce_only: false,
        };
        let base = floor_to_increment(close, mincontract);
        let (_, close_qty, open_qty) = fill_plan_generic(&ctx, order, position, -1.0, base);
        let mut lots: Vec<QtyLot<f64>> = before
            .iter()
            .enumerate()
            .map(|(id, (_, qty))| QtyLot {
                id: id as u64,
                qty: *qty,
            })
            .collect();
        let mut rows = Vec::new();
        fifo_close_generic(&ctx, &mut lots, close_qty, &mut rows);

        // The surviving old lots are the twin's lots, in order; a flip
        // appends one short lot of the plan's opening quantity after them.
        let after = shipped_lots(&broker);
        let opened = usize::from(open_qty > 0.0);
        assert_eq!(after.len(), lots.len() + opened, "{entries:?} {close}");
        for ((entry_id, qty), twin) in after.iter().zip(&lots) {
            assert_eq!(entry_id, &before[twin.id as usize].0);
            assert_eq!(qty.to_bits(), twin.qty.to_bits(), "{entries:?} {close}");
        }
        if opened == 1 {
            let (entry_id, qty) = after.last().expect("the flipped lot");
            assert_eq!(entry_id, "c");
            assert_eq!(qty.to_bits(), (-open_qty).to_bits(), "{entries:?} {close}");
        }
        let closed = shipped_closed(&broker);
        assert_eq!(closed.len(), rows.len(), "{entries:?} {close}");
        for ((entry_id, size), row) in closed.iter().zip(&rows) {
            assert_eq!(entry_id, &before[row.lot as usize].0);
            assert_eq!(size.to_bits(), row.qty.to_bits(), "{entries:?} {close}");
        }
    }

    #[test]
    fn fifo_and_plan_twins_match_the_shipped_broker() {
        let fixed: &[(&[f64], f64)] = &[
            (&[0.3, 0.7, 1.1], 1.5),
            (&[0.1, 0.2], 0.3),
            (&[0.1, 0.2, 0.3], 0.6),
            (&[1.0, 2.0, 3.0], 2.0),
            (&[0.333, 0.333, 0.334], 1.0),
            (&[0.5], 2.25),
            (&[0.25, 0.25, 0.25, 0.25], 0.75),
            (&[0.1, 0.1, 0.1], 0.3),
            (&[0.7, 0.1, 0.2], 1.0),
        ];
        for &(entries, close) in fixed {
            check_fifo_against_shipped(entries, close, 0.001);
        }
        // Adversarial: decimal lots whose sums do not round-trip in binary64,
        // closes that land on, just under and beyond lot boundaries, and
        // several increments.
        let mut rng = Rng(0xf1f0_c105_e5ee_d001);
        for _ in 0..cases(3_000) {
            let mincontract = [0.001, 0.01, 0.1, 1.0][rng.below(4) as usize];
            let n = 1 + rng.below(5) as usize;
            let entries: Vec<f64> = (0..n)
                .map(|_| (1 + rng.below(40)) as f64 * mincontract)
                .collect();
            let total: f64 = entries.iter().sum();
            let extra = (1 + rng.below(40)) as f64 * mincontract;
            let close = match rng.below(5) {
                0 => total,
                1 => entries[0],
                2 => entries.iter().take(1 + rng.below(n as u64) as usize).sum(),
                3 => total + extra,
                _ => extra,
            };
            check_fifo_against_shipped(&entries, close, mincontract);
        }
    }

    #[test]
    fn quantize_copy_matches_the_shipped_order_sizing() {
        // `f64_ops::quantize` is a copy of the private `floor_to_increment`.
        // The shipped broker sizes an explicit `strategy.order` quantity with
        // that function, so the filled position is its observable value.
        let mut rng = Rng(0x0fa7_71c3_0000_0001);
        let mut checked = 0;
        for i in 0..cases(4_000) {
            let mincontract = [0.001, 0.01, 0.1, 0.25, 1.0, 3.0][rng.below(6) as usize];
            let steps = rng.below(2_000) as f64;
            let qty = match i % 4 {
                // Decimal boundaries such as `0.3 / 0.1`.
                0 => steps * mincontract,
                1 => rng.below(200_000) as f64 / 1000.0,
                2 => steps * mincontract + mincontract * 0.999_999,
                _ => f64::from_bits((steps * mincontract).to_bits().saturating_sub(1)),
            };
            let want = floor_to_increment(qty, mincontract);
            if !qty.is_finite() || want <= 0.0 {
                continue;
            }
            let mut broker =
                Broker::new_with_symbol(&shipped_config(), 0.01, 1.0, mincontract, "USD");
            shipped_bar(&mut broker, 0, 100.0, 100.0, 100.0, 100.0);
            shipped_order(&mut broker, String::from("o"), 1.0, qty);
            broker.finish_bar(0, 100.0).expect("no caps set");
            shipped_bar(&mut broker, 1, 100.0, 101.0, 99.0, 100.0);
            assert_eq!(
                broker.position_size().to_bits(),
                want.to_bits(),
                "{qty} at {mincontract}"
            );
            checked += 1;
        }
        assert!(checked >= cases(4_000) / 2);
    }

    #[test]
    fn counters_twin_matches_the_shipped_report() {
        // Round trips at varying exit prices give winning, losing and even
        // trades; the shipped snapshot's counters are `record_closed_trade`'s.
        let mut rng = Rng(0xc0c0_17e5_5eed_0003);
        for _ in 0..cases(300) {
            let mut broker = Broker::new_with_symbol(&shipped_config(), 0.01, 1.0, 0.001, "USD");
            let trips = 1 + rng.below(8) as usize;
            let mut bar = 0;
            shipped_bar(&mut broker, bar, 100.0, 100.0, 100.0, 100.0);
            for trip in 0..trips {
                let qty = (1 + rng.below(30)) as f64 * 0.1;
                let exit = [100.0, 100.0, 101.37, 98.21, 103.5, 99.99][rng.below(6) as usize];
                shipped_order(&mut broker, format!("e{trip}"), 1.0, qty);
                broker.finish_bar(bar, 100.0).expect("no caps set");
                bar += 1;
                shipped_bar(&mut broker, bar, 100.0, 100.0, 100.0, exit);
                shipped_order(&mut broker, format!("x{trip}"), -1.0, qty);
                broker.finish_bar(bar, exit).expect("no caps set");
                bar += 1;
                shipped_bar(&mut broker, bar, exit, exit, exit, 100.0);
                assert_eq!(broker.position_size(), 0.0);
            }
            broker.record_snapshot(bar, 100.0, 100.0, 100.0, 100.0);
            let snapshot = broker.snapshot_at(bar).expect("recorded");
            let mut twin = Counters {
                closed_trades_total: 0,
                gross_profit: 0.0,
                gross_loss: 0.0,
                win_trades: 0,
                loss_trades: 0,
                even_trades: 0,
            };
            let mut closed = 0;
            while let Some(view) = broker.trade_view(closed, false) {
                record_counters_generic::<f64>(&mut twin, view.profit);
                closed += 1;
            }
            assert_eq!(closed, trips);
            assert_eq!(snapshot.closed_trades, i64::from(twin.closed_trades_total));
            assert_eq!(snapshot.win_trades, i64::from(twin.win_trades));
            assert_eq!(snapshot.loss_trades, i64::from(twin.loss_trades));
            assert_eq!(snapshot.even_trades, i64::from(twin.even_trades));
            assert_eq!(snapshot.gross_profit.to_bits(), twin.gross_profit.to_bits());
            assert_eq!(snapshot.gross_loss.to_bits(), twin.gross_loss.to_bits());
        }
    }
}
