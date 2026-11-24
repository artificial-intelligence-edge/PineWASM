//! Free-function extraction roots for the integer halves of
//! [`ExtremumWindow`] (its candidate queue) and [`PairRing`] (its pair
//! indexing). Charon cannot name inherent methods with `--start-from`, so
//! each root forwards to one method. The integer roots reach no `f64` path;
//! the `*_generic` roots reach the generic twins of the `f64` methods,
//! and `extremum_advance_f64` the `f64` instance of
//! `FloatOps`. Compiled only with `--features aeneas`.

use super::{ExtremumWindow, PairRing, Ring};
use crate::state::FloatOps;

/// Root for `ExtremumWindow::new`.
pub fn extremum_new(base: u32, cap: u32) -> ExtremumWindow {
    ExtremumWindow::new(base, cap)
}

/// Root for `ExtremumWindow::queue_slot`.
pub fn extremum_queue_slot(w: ExtremumWindow, i: u8) -> usize {
    w.queue_slot(i)
}

/// Root for `ExtremumWindow::candidate`.
pub fn extremum_candidate(w: ExtremumWindow, i: u8) -> u32 {
    w.candidate(i)
}

/// Root for `ExtremumWindow::back`.
pub fn extremum_back(w: ExtremumWindow) -> u32 {
    w.back()
}

/// Root for `ExtremumWindow::pop_back`.
pub fn extremum_pop_back(mut w: ExtremumWindow) -> ExtremumWindow {
    w.pop_back();
    w
}

/// Root for `ExtremumWindow::push_back`.
pub fn extremum_push_back(mut w: ExtremumWindow, slot: u32) -> ExtremumWindow {
    w.push_back(slot);
    w
}

/// Root for `ExtremumWindow::expire`.
pub fn extremum_expire(mut w: ExtremumWindow, slot: u32) -> ExtremumWindow {
    w.expire(slot);
    w
}

/// Root for `ExtremumWindow::restart_if_drained`.
pub fn extremum_restart_if_drained(mut w: ExtremumWindow) -> (ExtremumWindow, bool) {
    let rescan = w.restart_if_drained();
    (w, rescan)
}

/// Root for `ExtremumWindow::cell_of`.
pub fn extremum_cell_of(w: ExtremumWindow, slot: u32) -> u32 {
    w.cell_of(slot)
}

/// Root for `ExtremumWindow::offset_distance`.
pub fn extremum_offset_distance(w: ExtremumWindow) -> u32 {
    w.offset_distance()
}

/// Root for `PairRing::new`.
pub fn pair_ring_new(base: u32, cap: u32) -> PairRing {
    PairRing::new(base, cap)
}

/// Root for `PairRing::cap`.
pub fn pair_ring_cap(p: PairRing) -> u32 {
    p.cap()
}

/// Root for `PairRing::push_slot`.
pub fn pair_ring_push_slot(p: PairRing) -> u32 {
    p.push_slot()
}

/// Root for `PairRing::slot_of`.
pub fn pair_ring_slot_of(p: PairRing, i: u32) -> Option<u32> {
    p.slot_of(i)
}

/// Root for `PairRing::span_end`.
pub fn pair_ring_span_end(p: PairRing) -> u32 {
    p.span_end()
}

/// Root for the bookkeeping half of `PairRing::push`, which is
/// `Ring::advance` on the inner ring.
pub fn pair_ring_advance(mut p: PairRing, absent: bool, replaced_absent: bool) -> PairRing {
    p.inner.advance(absent, replaced_absent);
    p
}

/// Root for the generic twin `Ring::push_generic`.
pub fn ring_push_generic<F: FloatOps>(r: &mut Ring, cells: &mut [F], value: F) {
    r.push_generic(cells, value);
}

/// Root for the generic twin `Ring::window_ready_generic`.
pub fn ring_window_ready_generic<F>(r: Ring, cells: &[F]) -> bool {
    r.window_ready_generic(cells)
}

/// Root for the generic twin `ExtremumWindow::value_generic`.
pub fn extremum_value_generic<F: FloatOps>(w: &ExtremumWindow, cells: &[F], slot: u32) -> F {
    w.value_generic(cells, slot)
}

/// Root for the generic twin `ExtremumWindow::append_generic`.
pub fn extremum_append_generic<F: FloatOps>(
    w: &mut ExtremumWindow,
    cells: &[F],
    slot: u32,
    value: F,
    highest: bool,
) {
    w.append_generic(cells, slot, value, highest);
}

/// Root for the generic twin `ExtremumWindow::push_generic`.
pub fn extremum_push_generic<F: FloatOps>(
    w: &mut ExtremumWindow,
    cells: &mut [F],
    value: F,
    highest: bool,
) {
    w.push_generic(cells, value, highest);
}

/// Root for the generic twin `ExtremumWindow::best_generic`.
pub fn extremum_best_generic<F: FloatOps>(w: &ExtremumWindow, cells: &[F]) -> F {
    w.best_generic(cells)
}

/// Root for the generic twin `ExtremumWindow::offset_generic`.
pub fn extremum_offset_generic<F: FloatOps>(w: &ExtremumWindow) -> F {
    w.offset_generic()
}

/// Root for the generic twin of the engine's `State::WindowExtremum` arm.
pub fn extremum_advance_generic<F: FloatOps>(
    w: &mut ExtremumWindow,
    cells: &mut [F],
    x: F,
    highest: bool,
    bars: bool,
    zero_fold: F,
) -> F {
    w.advance_generic(cells, x, highest, bars, zero_fold)
}

/// Root for the `f64` instance: the engine arm's generic twin at `f64`.
pub fn extremum_advance_f64(
    w: &mut ExtremumWindow,
    cells: &mut [f64],
    x: f64,
    highest: bool,
    bars: bool,
    zero_fold: f64,
) -> f64 {
    w.advance_generic(cells, x, highest, bars, zero_fold)
}
