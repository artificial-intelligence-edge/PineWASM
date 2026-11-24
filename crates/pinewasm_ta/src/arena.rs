//! Fixed-capacity ring buffers over a caller-supplied cell array.
//!
//! A `VecDeque<Option<f64>>` per ring would need allocation, which a
//! `no_std` artifact cannot do. Every ring here is instead a
//! `(base, cap, len)` triple indexing one flat `&mut [f64]`, so a whole
//! program's engine state is a single contiguous region the emitter sizes at
//! compile time (every capacity comes from a baked const param).
//!
//! `na` is `f64::NAN`. That is not a sentinel collision: the read boundary
//! normalizes every non-finite value to na before it reaches the engine
//! (`Value::from_f64`), so a stored cell is either finite or absent.

use crate::state::f64_op;
#[cfg(any(test, feature = "aeneas"))]
use crate::state::{generic_op, FloatOps};

/// A window of `cap` cells, oldest to newest, holding the last `len` pushes.
///
/// `Copy` and pointer-free: a [`Ring`] is part of a [`crate::State`], which
/// lives in the same flat region as the cells it indexes.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Ring {
    /// First cell index in the engine's cell array.
    base: u32,
    cap: u32,
    /// Cells written so far, saturating at `cap`.
    len: u32,
    /// Index of the oldest cell within `base .. base + cap`.
    head: u32,
    /// Missing values among the cells written so far. Stored with the ring
    /// so copying or restoring engine state also restores its readiness.
    missing: u32,
}

/// `Ring::push`'s body over an operation dictionary: `f64_op`
/// for the shipped method, `generic_op` for the extracted generic twin.
macro_rules! ring_push_fn {
    ($op:ident, $name:ident $(, $g:ident: $bound:path)?; $f:ty) => {
        /// Overwrite the oldest cell once full, exactly like a `VecDeque`
        /// `pop_front`-then-`push_back` pair.
        pub(crate) fn $name $(<$g: $bound>)? (&mut self, cells: &mut [$f], value: $f) {
            let slot = self.push_slot();
            let mut absent = true;
            let mut replaced_absent = true;
            if let Some(cell) = cells.get_mut(slot as usize) {
                replaced_absent = $op!(is_nan *cell);
                *cell = value;
                absent = $op!(is_nan value);
            }
            self.advance(absent, replaced_absent);
        }
    };
}

/// `Ring::window_ready`'s body over any cell type (it reads no cell value).
macro_rules! ring_window_ready_fn {
    ($name:ident $(, $g:ident)?; $f:ty) => {
        /// Whether the full window is readable: `cap` cells written and none of
        /// them na. This is the window rule — one na anywhere in
        /// the window makes the whole window unavailable.
        ///
        /// Each push updates the missing count, avoiding a second full-window
        /// scan before the indicator calculation. The storage bounds check is
        /// retained so a truncated region still fails closed.
        pub(crate) fn $name $(<$g>)? (self, cells: &[$f]) -> bool {
            self.full()
                && self.missing == 0
                && cells
                    .get(self.base as usize..(self.base + self.cap) as usize)
                    .is_some()
        }
    };
}

/// `Ring::get`'s body over an operation dictionary: `f64_op`
/// for the shipped method, `generic_op` for the extracted generic twin.
macro_rules! ring_get_fn {
    ($op:ident, $name:ident $(, $g:ident: $bound:path)?; $f:ty) => {
        /// Cell `i` counting from the oldest (0) to the newest (`len - 1`);
        /// `NAN` when out of range, which reads as na everywhere.
        #[inline]
        pub(crate) fn $name $(<$g: $bound>)? (self, cells: &[$f], i: u32) -> $f {
            let Some(slot) = self.slot_of(i) else {
                return $op!(nan);
            };
            match cells.get(slot as usize) {
                Some(&v) => v,
                None => $op!(nan),
            }
        }
    };
}

/// `Ring::last_window_ready`'s body over an operation dictionary;
/// `$get` is the ring read the dictionary pairs with.
macro_rules! ring_last_window_ready_fn {
    ($op:ident, $name:ident, $get:ident $(, $g:ident: $bound:path)?; $f:ty) => {
        /// [`Ring::window_ready`] for the newest `k` cells (`ta.hma`'s inner
        /// half-length window).
        pub(crate) fn $name $(<$g: $bound>)? (self, cells: &[$f], k: u32) -> bool {
            if self.len < k {
                return false;
            }
            let start = self.len - k;
            let mut i = start;
            while i < self.len {
                if $op!(is_nan self.$get(cells, i)) {
                    return false;
                }
                i += 1;
            }
            true
        }
    };
}

/// `Ring::last`'s body over an operation dictionary; `$get`
/// is the ring read the dictionary pairs with.
macro_rules! ring_last_fn {
    ($op:ident, $name:ident, $get:ident $(, $g:ident: $bound:path)?; $f:ty) => {
        /// The newest cell, or na.
        pub(crate) fn $name $(<$g: $bound>)? (self, cells: &[$f]) -> $f {
            if self.len == 0 {
                $op!(nan)
            } else {
                self.$get(cells, self.len - 1)
            }
        }
    };
}

impl Ring {
    /// A ring of `cap` cells (at least one) starting at `base`.
    pub(crate) fn new(base: u32, cap: u32) -> Ring {
        Ring {
            base,
            cap: if cap == 0 { 1 } else { cap },
            len: 0,
            head: 0,
            missing: 0,
        }
    }

    pub(crate) fn cap(self) -> u32 {
        self.cap
    }

    pub(crate) fn len(self) -> u32 {
        self.len
    }

    pub(crate) fn full(self) -> bool {
        self.len == self.cap
    }

    /// Wrap `head + i` into the ring without a division.
    ///
    /// `head < cap` and `i < cap`, so the sum is below `2 * cap` and one
    /// conditional subtract is exact. This is not micro-optimization for its
    /// own sake: window functions rescan their whole window every bar, so a
    /// `%` here would be an integer division per cell per bar.
    #[inline]
    fn wrap(self, i: u32) -> u32 {
        let raw = self.head + i;
        if raw >= self.cap {
            raw - self.cap
        } else {
            raw
        }
    }

    ring_push_fn!(f64_op, push; f64);

    /// The cell the next [`Ring::push`] writes: the integer half of `push`,
    /// kept free of `f64` so it can be extracted on its own.
    #[inline]
    fn push_slot(self) -> u32 {
        self.base + self.wrap(self.len)
    }

    /// The bookkeeping half of [`Ring::push`] once the cell is written:
    /// update the missing count, then grow `len` or, once full, rotate
    /// `head` past the overwritten oldest cell.
    #[inline]
    fn advance(&mut self, absent: bool, replaced_absent: bool) {
        self.track_missing(absent, replaced_absent);
        if self.len == self.cap {
            self.head = if self.head + 1 == self.cap {
                0
            } else {
                self.head + 1
            };
        } else {
            self.len += 1;
        }
    }

    /// Account for the incoming cell and, once full, the overwritten cell.
    /// Saturation also keeps malformed short storage from reaching a panic.
    fn track_missing(&mut self, absent: bool, replaced_absent: bool) {
        self.missing = self
            .missing
            .saturating_sub(u32::from(self.full() && replaced_absent))
            .saturating_add(u32::from(absent));
    }

    ring_get_fn!(f64_op, get; f64);

    /// The cell holding element `i` (oldest is 0), or `None` when `i` is
    /// not below `len`: the integer half of [`Ring::get`].
    #[inline]
    fn slot_of(self, i: u32) -> Option<u32> {
        if i >= self.len {
            None
        } else {
            Some(self.base + self.wrap(i))
        }
    }

    ring_last_fn!(f64_op, last, get; f64);

    ring_window_ready_fn!(window_ready; f64);

    ring_last_window_ready_fn!(f64_op, last_window_ready, get; f64);
}

/// The extracted generic twins of the `f64` ring methods.
#[cfg(any(test, feature = "aeneas"))]
impl Ring {
    ring_push_fn!(generic_op, push_generic, F: FloatOps; F);
    ring_window_ready_fn!(window_ready_generic, F; F);
    ring_get_fn!(generic_op, get_generic, F: FloatOps; F);
    ring_last_window_ready_fn!(generic_op, last_window_ready_generic, get_generic,
        F: FloatOps; F);
    ring_last_fn!(generic_op, last_generic, get_generic, F: FloatOps; F);
}

/// Free-function extraction roots, because the extraction tool cannot name
/// inherent methods directly. They reach the integer halves of [`Ring`]
/// only, never an `f64` path. Compiled only with `--features aeneas`.
#[cfg(feature = "aeneas")]
pub mod aeneas_roots {
    use super::Ring;

    /// Root for `Ring::new`.
    pub fn ring_new(base: u32, cap: u32) -> Ring {
        Ring::new(base, cap)
    }

    /// Root for `Ring::full`.
    pub fn ring_full(r: Ring) -> bool {
        r.full()
    }

    /// Root for `Ring::wrap`.
    pub fn ring_wrap(r: Ring, i: u32) -> u32 {
        r.wrap(i)
    }

    /// Root for `Ring::push_slot`.
    pub fn ring_push_slot(r: Ring) -> u32 {
        r.push_slot()
    }

    /// Root for `Ring::slot_of`.
    pub fn ring_slot_of(r: Ring, i: u32) -> Option<u32> {
        r.slot_of(i)
    }

    /// Root for `Ring::track_missing`.
    pub fn ring_track_missing(mut r: Ring, absent: bool, replaced_absent: bool) -> Ring {
        r.track_missing(absent, replaced_absent);
        r
    }

    /// Root for `Ring::advance`.
    pub fn ring_advance(mut r: Ring, absent: bool, replaced_absent: bool) -> Ring {
        r.advance(absent, replaced_absent);
        r
    }
}

#[cfg(feature = "aeneas")]
pub mod aeneas_roots_ta_arena2;

/// `ExtremumWindow::value`'s body over an operation dictionary.
macro_rules! extremum_value_fn {
    ($op:ident, $name:ident $(, $g:ident: $bound:path)?; $f:ty) => {
        fn $name $(<$g: $bound>)? (&self, cells: &[$f], slot: u32) -> $f {
            cells
                .get(self.cell_of(slot) as usize)
                .copied()
                .unwrap_or($op!(nan))
        }
    };
}

/// `ExtremumWindow::append`'s body over an operation dictionary; `$value`
/// is the matching `value` method.
macro_rules! extremum_append_fn {
    ($op:ident, $name:ident, $value:ident $(, $g:ident: $bound:path)?; $f:ty) => {
        fn $name $(<$g: $bound>)? (&mut self, cells: &[$f], slot: u32, value: $f, highest: bool) {
            if $op!(is_nan value) {
                return;
            }
            let dominates = |last| {
                if highest {
                    $op!(ge value, last)
                } else {
                    $op!(le value, last)
                }
            };
            if self.truncated {
                // Omitted candidates are newer and weaker than the retained
                // tail. Only a value dominating that tail can discard them.
                // Free slots after partial expiry must not bypass this gap.
                if !dominates(self.$value(cells, self.back())) {
                    return;
                }
                self.truncated = false;
            }
            while self.len > 0 {
                // Equal candidates favor the most recent call. Zero-valued
                // scalar extrema are replaced by the arm's `$fallback` (the
                // engine: `+0.0`).
                if dominates(self.$value(cells, self.back())) {
                    self.pop_back();
                } else {
                    break;
                }
            }
            self.push_back(slot);
        }
    };
}

/// `ExtremumWindow::push`'s body over an operation dictionary; `$ring_push`,
/// `$append` and `$value` are the matching methods.
macro_rules! extremum_push_fn {
    ($op:ident, $name:ident, $ring_push:ident, $append:ident, $value:ident
        $(, $g:ident: $bound:path)?; $f:ty) => {
        pub(crate) fn $name $(<$g: $bound>)? (&mut self, cells: &mut [$f], value: $f, highest: bool) {
            let end = self.ring.base.saturating_add(self.ring.cap);
            if cells.get(self.ring.base as usize..end as usize).is_none() {
                return;
            }
            let slot = self.ring.wrap(self.ring.len);
            // Expire the candidate before its underlying value is overwritten.
            self.expire(slot);
            self.ring.$ring_push(cells, value);
            if self.restart_if_drained() {
                for i in 0..self.ring.len {
                    let slot = self.ring.wrap(i);
                    self.$append(cells, slot, self.$value(cells, slot), highest);
                }
            } else {
                self.$append(cells, slot, value, highest);
            }
        }
    };
}

/// `ExtremumWindow::best`'s body over an operation dictionary.
macro_rules! extremum_best_fn {
    ($op:ident, $name:ident, $window_ready:ident, $value:ident $(, $g:ident: $bound:path)?; $f:ty) => {
        pub(crate) fn $name $(<$g: $bound>)? (&self, cells: &[$f]) -> $f {
            if self.len == 0 || !self.ring.$window_ready(cells) {
                return $op!(nan);
            }
            self.$value(cells, self.candidate(0))
        }
    };
}

/// Scalar extrema require a full bar window and a present current source,
/// but skip earlier missing cells. Offset extrema retain the strict window.
macro_rules! extremum_best_present_fn {
    ($op:ident, $name:ident, $value:ident $(, $g:ident: $bound:path)?; $f:ty) => {
        pub(crate) fn $name $(<$g: $bound>)? (&self, cells: &[$f]) -> $f {
            let end = self.ring.base.saturating_add(self.ring.cap);
            if self.len == 0
                || !self.ring.full()
                || cells.get(self.ring.base as usize..end as usize).is_none()
            {
                return $op!(nan);
            }
            let newest = self.ring.base + self.ring.wrap(self.ring.len - 1);
            match cells.get(newest as usize) {
                Some(value) if !$op!(is_nan *value) => self.$value(cells, self.candidate(0)),
                _ => $op!(nan),
            }
        }
    };
}

/// `ExtremumWindow::offset`'s body over an operation dictionary.
macro_rules! extremum_offset_fn {
    ($op:ident, $name:ident $(, $g:ident: $bound:path)?; $f:ty) => {
        pub(crate) fn $name $(<$g: $bound>)? (&self) -> $f {
            $op!(neg $op!(from_u32 self.offset_distance()))
        }
    };
}

/// The engine's `State::WindowExtremum` arm (`ta.highest`, `ta.lowest`,
/// `ta.highestbars`, `ta.lowestbars`) over an operation dictionary. `$bars`
/// selects the offset output; `$fallback` is what the arm returns on a
/// zero extremum (the engine passes `+0.0`; evaluated only in that branch). `$best` checks offsets' strict window, while
/// `$best_present` implements scalar extrema's missing-value rule.
macro_rules! window_extremum_arm {
    ($op:ident, $window:expr, $cells:expr, $x:expr, $highest:expr, $bars:expr, $fallback:expr,
        $push:ident, $best:ident, $best_present:ident, $offset:ident) => {{
        let highest = $highest;
        $window.$push($cells, $x, highest);
        let best = if $bars {
            $window.$best($cells)
        } else {
            $window.$best_present($cells)
        };
        if $op!(is_nan best) {
            $op!(nan)
        } else if $bars {
            $window.$offset()
        } else if $op!(eq_zero best) {
            // The cache holds one candidate per value, so the sign of its
            // zero is an accident of the window's order. The caller supplies
            // the zero extremum (the engine: always `+0.0`).
            $fallback
        } else {
            $op!(norm best)
        }
    }};
}
pub(crate) use window_extremum_arm;

/// A window plus a bounded monotonic cache of candidate cell indices. The
/// cache fits inside the existing reserved state stride; the window still
/// needs exactly `cap` cells. If there are more candidates than fit, retain
/// the oldest (strongest) prefix and rebuild when that prefix expires.
///
/// Indices name physical ring slots, rather than absolute bar numbers: they
/// cannot overflow on long runs, and conditional calls advance only this
/// site's clock. Copying the state and cells also copies the entire deque.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExtremumWindow {
    pub(crate) ring: Ring,
    candidates: [u32; 24],
    head: u8,
    len: u8,
    truncated: bool,
}

impl ExtremumWindow {
    pub(crate) fn new(base: u32, cap: u32) -> Self {
        Self {
            ring: Ring::new(base, cap),
            candidates: [0; 24],
            head: 0,
            len: 0,
            truncated: false,
        }
    }

    /// Physical index of queue entry `i` (0 is the front) in `candidates`.
    ///
    /// `head < 24` and `i <= 24`, so one conditional subtract is exact.
    #[inline]
    fn queue_slot(&self, i: u8) -> usize {
        let raw = usize::from(self.head) + usize::from(i);
        if raw >= self.candidates.len() {
            raw - self.candidates.len()
        } else {
            raw
        }
    }

    #[inline]
    fn candidate(&self, i: u8) -> u32 {
        self.candidates[self.queue_slot(i)]
    }

    /// The newest (weakest) candidate's ring slot. Requires `len > 0`.
    #[inline]
    fn back(&self) -> u32 {
        self.candidate(self.len - 1)
    }

    /// Drop the newest candidate. Requires `len > 0`.
    #[inline]
    fn pop_back(&mut self) {
        self.len -= 1;
    }

    /// Enqueue `slot` behind the retained candidates, or mark the cache
    /// truncated once all 24 entries are in use.
    #[inline]
    fn push_back(&mut self, slot: u32) {
        if usize::from(self.len) < self.candidates.len() {
            self.candidates[self.queue_slot(self.len)] = slot;
            self.len += 1;
        } else {
            self.truncated = true;
        }
    }

    /// Drop the front candidate if it names `slot`, the ring slot the next
    /// push is about to overwrite.
    #[inline]
    fn expire(&mut self, slot: u32) {
        if self.len > 0 && self.candidate(0) == slot {
            self.head = self.queue_slot(1) as u8;
            self.len -= 1;
        }
    }

    /// Reset an empty truncated cache for a full rebuild; `true` when the
    /// caller must rescan the window.
    #[inline]
    fn restart_if_drained(&mut self) -> bool {
        if self.len == 0 && self.truncated {
            self.head = 0;
            self.truncated = false;
            true
        } else {
            false
        }
    }

    /// The cell index backing ring slot `slot`.
    #[inline]
    fn cell_of(&self, slot: u32) -> u32 {
        self.ring.base + slot
    }

    extremum_value_fn!(f64_op, value; f64);
    extremum_append_fn!(f64_op, append, value; f64);
    extremum_push_fn!(f64_op, push, push, append, value; f64);
    extremum_best_fn!(f64_op, best, window_ready, value; f64);
    extremum_best_present_fn!(f64_op, best_present, value; f64);
    extremum_offset_fn!(f64_op, offset; f64);

    /// Bars from the front candidate to the newest cell, walking forward
    /// around the ring: the integer half of [`ExtremumWindow::offset`].
    /// Requires `ring.len > 0` and `len > 0`.
    #[inline]
    fn offset_distance(&self) -> u32 {
        let newest = self.ring.wrap(self.ring.len - 1);
        let best = self.candidate(0);
        if newest >= best {
            newest - best
        } else {
            newest + self.ring.cap - best
        }
    }
}

/// The extracted generic twins of the `f64` methods of [`ExtremumWindow`]
/// and of the engine's `State::WindowExtremum` arm.
#[cfg(any(test, feature = "aeneas"))]
impl ExtremumWindow {
    extremum_value_fn!(generic_op, value_generic, F: FloatOps; F);
    extremum_append_fn!(generic_op, append_generic, value_generic, F: FloatOps; F);
    extremum_push_fn!(generic_op, push_generic, push_generic, append_generic, value_generic,
        F: FloatOps; F);
    extremum_best_fn!(generic_op, best_generic, window_ready_generic, value_generic,
        F: FloatOps; F);
    extremum_best_present_fn!(generic_op, best_present_generic, value_generic,
        F: FloatOps; F);
    extremum_offset_fn!(generic_op, offset_generic, F: FloatOps; F);

    /// `window_extremum_arm!` over `generic_op`: the engine arm with the
    /// zero-extremum fold passed in as `zero_fold`.
    pub(crate) fn advance_generic<F: FloatOps>(
        &mut self,
        cells: &mut [F],
        x: F,
        highest: bool,
        bars: bool,
        zero_fold: F,
    ) -> F {
        window_extremum_arm!(
            generic_op,
            self,
            cells,
            x,
            highest,
            bars,
            zero_fold,
            push_generic,
            best_generic,
            best_present_generic,
            offset_generic
        )
    }
}

/// A ring of `(a, b)` pairs — `ta.vwma`, `ta.correlation`, `ta.stoch` and
/// `ta.mfi` all buffer two series at once. A pair is absent (`na`) when
/// either side is.
/// `PairRing::push`'s body over an operation dictionary:
/// `f64_op` for the shipped method, `generic_op` for the extracted twin.
macro_rules! pair_push_fn {
    ($op:ident, $name:ident $(, $g:ident: $bound:path)?; $f:ty) => {
        /// Push `(a, b)`, or the absent pair when either side is na.
        pub(crate) fn $name $(<$g: $bound>)? (&mut self, cells: &mut [$f], a: $f, b: $f) {
            let (a, b) = if $op!(is_nan a) || $op!(is_nan b) {
                ($op!(nan), $op!(nan))
            } else {
                (a, b)
            };
            let slot = self.push_slot();
            let mut absent = true;
            let mut replaced_absent = true;
            if let Some(cell) = cells.get_mut(slot as usize) {
                replaced_absent = $op!(is_nan *cell);
                *cell = a;
                absent = $op!(is_nan a);
            }
            if let Some(cell) = cells.get_mut(slot as usize + 1) {
                *cell = b;
            }
            self.inner.advance(absent, replaced_absent);
        }
    };
}

/// `PairRing::get`'s body over an operation dictionary.
macro_rules! pair_get_fn {
    ($op:ident, $name:ident $(, $g:ident: $bound:path)?; $f:ty) => {
        /// Pair `i` from the oldest; `(NAN, NAN)` when out of range or absent.
        #[inline]
        pub(crate) fn $name $(<$g: $bound>)? (self, cells: &[$f], i: u32) -> ($f, $f) {
            let Some(slot) = self.slot_of(i) else {
                return ($op!(nan), $op!(nan));
            };
            let a = cells.get(slot as usize).copied().unwrap_or($op!(nan));
            let b = cells.get(slot as usize + 1).copied().unwrap_or($op!(nan));
            (a, b)
        }
    };
}

/// `PairRing::ready`'s body over any cell type (it reads no cell value).
macro_rules! pair_ready_fn {
    ($name:ident $(, $g:ident)?; $f:ty) => {
        /// `cap` pairs written and none absent.
        pub(crate) fn $name $(<$g>)? (self, cells: &[$f]) -> bool {
            self.inner.full()
                && self.inner.missing == 0
                && cells
                    .get(self.inner.base as usize..self.span_end() as usize)
                    .is_some()
        }
    };
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PairRing {
    inner: Ring,
}

impl PairRing {
    /// `cap` pairs, occupying `2 * cap` cells from `base`.
    pub(crate) fn new(base: u32, cap: u32) -> PairRing {
        PairRing {
            inner: Ring::new(base, cap),
        }
    }

    pub(crate) fn cap(self) -> u32 {
        self.inner.cap
    }

    /// Reuse a freshly reset pair footprint for two independent windows.
    /// Existing pair-ring kernels and their interleaved layout stay intact;
    /// this policy adapter is only valid before any pair has been pushed.
    pub(crate) fn independent_windows(self) -> Option<(Ring, Ring)> {
        if self.inner.len != 0 {
            return None;
        }
        let second = self.inner.base.checked_add(self.inner.cap)?;
        second.checked_add(self.inner.cap)?;
        Some((
            Ring::new(self.inner.base, self.inner.cap),
            Ring::new(second, self.inner.cap),
        ))
    }

    pair_push_fn!(f64_op, push; f64);

    /// The first of the two cells the next [`PairRing::push`] writes.
    #[inline]
    fn push_slot(self) -> u32 {
        self.inner.base + 2 * self.inner.wrap(self.inner.len)
    }

    /// The first cell of pair `i` (oldest is 0), or `None` when `i` is not
    /// below `len`: the integer half of [`PairRing::get`].
    #[inline]
    fn slot_of(self, i: u32) -> Option<u32> {
        if i >= self.inner.len {
            None
        } else {
            Some(self.inner.base + 2 * self.inner.wrap(i))
        }
    }

    /// One past the last cell the ring occupies.
    #[inline]
    fn span_end(self) -> u32 {
        self.inner.base + 2 * self.inner.cap
    }

    pair_get_fn!(f64_op, get; f64);

    pair_ready_fn!(ready; f64);
}

/// The extracted generic twins of the `f64` pair-ring methods
/// (`ta.stoch`).
#[cfg(any(test, feature = "aeneas"))]
impl PairRing {
    pair_push_fn!(generic_op, push_generic, F: FloatOps; F);
    pair_get_fn!(generic_op, get_generic, F: FloatOps; F);
    pair_ready_fn!(ready_generic, F; F);
}

/// `sort_total`'s body over an operation dictionary: `f64_op`
/// for the shipped function, `generic_op` for the extracted generic twin.
/// `$sift` is the `sift_down` the dictionary pairs with.
macro_rules! sort_total_fn {
    ($op:ident, $f:ty, $sift:ident, $(#[$m:meta])* $vis:vis fn $name:ident [$($g:tt)*]) => {
        $(#[$m])*
        $vis fn $name<$($g)*>(slice: &mut [$f]) {
            let n = slice.len();
            if n < 2 {
                return;
            }
            let mut start = n / 2;
            while start > 0 {
                start -= 1;
                $sift(slice, start, n);
            }
            let mut end = n;
            while end > 1 {
                end -= 1;
                slice.swap(0, end);
                $sift(slice, 0, end);
            }
        }
    };
}

/// `sift_down`'s body over an operation dictionary: the heapsort sift under
/// `total_cmp` (`$op!(total_gt a, b)` is `a.total_cmp(&b).is_gt()`).
macro_rules! sift_down_fn {
    ($op:ident, $f:ty, $(#[$m:meta])* $vis:vis fn $name:ident [$($g:tt)*]) => {
        $(#[$m])*
        $vis fn $name<$($g)*>(slice: &mut [$f], mut root: usize, end: usize) {
            loop {
                let child = root * 2 + 1;
                if child >= end {
                    return;
                }
                let (Some(&left), Some(&parent)) = (slice.get(child), slice.get(root)) else {
                    return;
                };
                let mut swap = root;
                let mut best = parent;
                if $op!(total_gt left, best) {
                    swap = child;
                    best = left;
                }
                // Bound check first, then the read: the same value as reading
                // first (`get` is pure), in the shape Aeneas can join (the
                // read-first form trips its
                // loop-join collapse).
                if child + 1 < end {
                    if let Some(&right) = slice.get(child + 1) {
                        if $op!(total_gt right, best) {
                            swap = child + 1;
                        }
                    }
                }
                if swap == root {
                    return;
                }
                slice.swap(root, swap);
                root = swap;
            }
        }
    };
}

sort_total_fn!(
    f64_op,
    f64,
    sift_down,
    /// Sort `slice` ascending under `f64::total_cmp`, without allocating.
    ///
    /// Heapsort rather than `slice::sort_unstable_by`: that one can panic when
    /// handed an inconsistent comparator, and a reachable panic in the artifact
    /// build drags `core::fmt`'s formatting vtables (and with them an element
    /// section the runtime splice rejects) into every module. The result is
    /// identical either way — `total_cmp` is a total order and the windows
    /// reaching here are na-free, so elements that compare equal are also
    /// bit-equal and their relative order cannot be observed.
    pub(crate) fn sort_total[]
);

sift_down_fn!(f64_op, f64, fn sift_down[]);

#[cfg(any(test, feature = "aeneas"))]
sort_total_fn!(
    generic_op,
    F,
    sift_down_generic,
    /// `sort_total_fn!` over `generic_op`.
    pub(crate) fn sort_total_generic[F: FloatOps]
);

#[cfg(any(test, feature = "aeneas"))]
sift_down_fn!(
    generic_op,
    F,
    /// `sift_down_fn!` over `generic_op`.
    fn sift_down_generic[F: FloatOps]
);

/// `tie_block`'s body over an operation dictionary:
/// `total_lt a, b` is `a.total_cmp(&b).is_lt()`, `fne a, b` is `a != b`.
macro_rules! tie_block_fn {
    ($op:ident, $f:ty, $(#[$m:meta])* $vis:vis fn $name:ident [$($g:tt)*]) => {
        $(#[$m])*
        $vis fn $name<$($g)*>(sorted: &[$f], x: $f) -> (usize, usize) {
            let (mut lo, mut hi) = (0usize, sorted.len());
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                match sorted.get(mid) {
                    Some(&v) if $op!(total_lt v, x) => lo = mid + 1,
                    _ => hi = mid,
                }
            }
            let first = lo;
            let mut count = 0usize;
            let mut i = first;
            // Bound check first, then the read: the same loop as
            // `while let Some(&v) = sorted.get(i)` (`get` is `Some` exactly
            // below `len`), in the shape Aeneas can join.
            while i < sorted.len() {
                if let Some(&v) = sorted.get(i) {
                    if $op!(fne v, x) {
                        break;
                    }
                }
                count += 1;
                i += 1;
            }
            (first, count)
        }
    };
}

tie_block_fn!(
    f64_op,
    f64,
    /// Index of the first element of `sorted` equal to `x`, and how many equal
    /// elements follow it — the tie block `ta.rci`'s average rank is built from.
    /// `sorted` must be ascending under `total_cmp` and contain `x`.
    pub(crate) fn tie_block[]
);

#[cfg(any(test, feature = "aeneas"))]
tie_block_fn!(
    generic_op,
    F,
    /// `tie_block_fn!` over `generic_op`.
    pub(crate) fn tie_block_generic[F: FloatOps]
);

#[cfg(test)]
mod tests {
    extern crate std;

    use super::{ExtremumWindow, PairRing, Ring};
    use crate::state::f64_op;
    use std::{format, vec, vec::Vec};

    /// The engine's `norm` (non-finite becomes na), for the arm expansion.
    fn norm(v: f64) -> f64 {
        if v.is_finite() {
            v
        } else {
            f64::NAN
        }
    }

    fn inputs() -> [f64; 512] {
        let mut values = [0.0; 512];
        for (i, value) in values.iter_mut().enumerate() {
            *value = match i % 71 {
                0 | 1 | 3 | 20 => f64::NAN,
                21 => -0.0,
                22 => f64::INFINITY,
                _ => i as f64 / 7.0,
            };
        }
        values
    }

    #[test]
    fn readiness_matches_history_through_wrap_missing_values_and_restore() {
        let values = inputs();
        for cap in [1, 2, 3, 22, 30] {
            let mut ring = Ring::new(3, cap);
            let mut cells = [42.0; 64];
            let mut snapshot = (ring, cells);
            for replay in 0..2 {
                let start = if replay == 0 { 0 } else { 256 };
                if replay == 1 {
                    (ring, cells) = snapshot;
                }
                for (i, &value) in values.iter().enumerate().skip(start) {
                    if i == 256 && replay == 0 {
                        snapshot = (ring, cells);
                    }
                    ring.push(&mut cells, value);
                    let len = (i + 1).min(cap as usize);
                    let expected = &values[i + 1 - len..=i];
                    assert_eq!(
                        ring.window_ready(&cells),
                        len == cap as usize && expected.iter().all(|x| !x.is_nan()),
                        "cap={cap}, bar={i}, replay={replay}"
                    );
                    for (j, &x) in expected.iter().enumerate() {
                        assert_eq!(ring.get(&cells, j as u32).to_bits(), x.to_bits());
                    }
                    for k in 0..=cap {
                        let suffix_ready = len >= k as usize
                            && expected[len.saturating_sub(k as usize)..]
                                .iter()
                                .all(|x| !x.is_nan());
                        assert_eq!(ring.last_window_ready(&cells, k), suffix_ready);
                    }
                }
            }
        }
    }

    #[test]
    fn pair_readiness_tracks_missing_values_on_either_side() {
        let a = inputs();
        let b = inputs().map(|x| x + 1.0);
        for cap in [1, 2, 3, 22, 30] {
            let mut ring = PairRing::new(2, cap);
            let mut cells = [42.0; 64];
            for i in 0..a.len() - 9 {
                ring.push(&mut cells, a[i], b[i + 9]);
                let len = (i + 1).min(cap as usize);
                let start = i + 1 - len;
                assert_eq!(
                    ring.ready(&cells),
                    len == cap as usize
                        && (start..=i).all(|j| !a[j].is_nan() && !b[j + 9].is_nan()),
                    "cap={cap}, bar={i}"
                );
            }
        }
    }

    /// A plain version of `Ring::push`, kept in tests to check the
    /// integer/`f64` split against.
    fn reference_push(ring: &mut Ring, cells: &mut [f64], value: f64) {
        let slot = ring.base + ring.wrap(ring.len);
        let mut absent = true;
        let mut replaced_absent = true;
        if let Some(cell) = cells.get_mut(slot as usize) {
            replaced_absent = cell.is_nan();
            *cell = value;
            absent = value.is_nan();
        }
        ring.track_missing(absent, replaced_absent);
        if ring.len == ring.cap {
            ring.head = if ring.head + 1 == ring.cap {
                0
            } else {
                ring.head + 1
            };
        } else {
            ring.len += 1;
        }
    }

    /// A plain version of `Ring::get`, kept in tests.
    fn reference_get(ring: Ring, cells: &[f64], i: u32) -> f64 {
        if i >= ring.len {
            return f64::NAN;
        }
        match cells.get((ring.base + ring.wrap(i)) as usize) {
            Some(&v) => v,
            None => f64::NAN,
        }
    }

    fn bits(cells: &[f64]) -> Vec<u64> {
        cells.iter().map(|x| x.to_bits()).collect()
    }

    #[test]
    fn split_push_and_get_match_the_reference() {
        // Deterministic LCG sweep over capacities (including the 0 -> 1
        // clamp), bases, and storage lengths that are short, exact, and
        // truncated mid-window, with na, -0.0, and infinities mixed in.
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            seed >> 33
        };
        for cap in [0u32, 1, 2, 3, 5, 8, 17, 31] {
            for base in [0u32, 1, 4, 9] {
                let exact = (base + cap.max(1)) as usize;
                for storage in [0, 1, exact.saturating_sub(1), exact, exact + 3] {
                    let mut new_ring = Ring::new(base, cap);
                    let mut old_ring = new_ring;
                    let mut new_cells = vec![7.5; storage];
                    let mut old_cells = new_cells.clone();
                    for step in 0..200 {
                        let value = match next() % 9 {
                            0 | 1 => f64::NAN,
                            2 => -0.0,
                            3 => f64::INFINITY,
                            _ => (next() % 1000) as f64 / 8.0,
                        };
                        new_ring.push(&mut new_cells, value);
                        reference_push(&mut old_ring, &mut old_cells, value);
                        let ctx = format!("cap={cap} base={base} storage={storage} step={step}");
                        assert_eq!(new_ring, old_ring, "{ctx}");
                        assert_eq!(bits(&new_cells), bits(&old_cells), "{ctx}");
                        for i in 0..cap.max(1) + 3 {
                            assert_eq!(
                                new_ring.get(&new_cells, i).to_bits(),
                                reference_get(old_ring, &old_cells, i).to_bits(),
                                "{ctx} i={i}"
                            );
                        }
                        assert_eq!(
                            new_ring.last(&new_cells).to_bits(),
                            old_ring.last(&old_cells).to_bits()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn slot_helpers_agree_with_the_reference_indexing() {
        for cap in [1u32, 2, 7] {
            let mut ring = Ring::new(5, cap);
            let mut cells = [0.0; 16];
            for _ in 0..3 * cap {
                assert_eq!(ring.push_slot(), ring.base + ring.wrap(ring.len));
                for i in 0..cap + 2 {
                    let expected = (i < ring.len).then(|| ring.base + ring.wrap(i));
                    assert_eq!(ring.slot_of(i), expected);
                }
                ring.push(&mut cells, 1.0);
            }
        }
    }

    #[test]
    fn truncated_storage_does_not_become_ready() {
        let mut ring = Ring::new(1, 3);
        let mut pair = PairRing::new(1, 2);
        let mut cells = [0.0; 3];
        for _ in 0..20 {
            ring.push(&mut cells, 1.0);
            assert!(!ring.window_ready(&cells));
        }
        for _ in 0..20 {
            pair.push(&mut cells, 1.0, 2.0);
            assert!(!pair.ready(&cells));
        }
    }

    /// A plain version of the `ExtremumWindow` queue logic, written as free
    /// functions over the same fields, to check the split implementation against.
    mod reference_extremum {
        use super::super::ExtremumWindow;

        fn queue_slot(w: &ExtremumWindow, i: u8) -> usize {
            let raw = usize::from(w.head) + usize::from(i);
            if raw >= w.candidates.len() {
                raw - w.candidates.len()
            } else {
                raw
            }
        }

        fn candidate(w: &ExtremumWindow, i: u8) -> u32 {
            w.candidates[queue_slot(w, i)]
        }

        fn value(w: &ExtremumWindow, cells: &[f64], slot: u32) -> f64 {
            cells
                .get((w.ring.base + slot) as usize)
                .copied()
                .unwrap_or(f64::NAN)
        }

        fn append(w: &mut ExtremumWindow, cells: &[f64], slot: u32, value_in: f64, highest: bool) {
            if value_in.is_nan() {
                return;
            }
            let dominates = |last| {
                if highest {
                    value_in >= last
                } else {
                    value_in <= last
                }
            };
            if w.truncated {
                if !dominates(value(w, cells, candidate(w, w.len - 1))) {
                    return;
                }
                w.truncated = false;
            }
            while w.len > 0 {
                if dominates(value(w, cells, candidate(w, w.len - 1))) {
                    w.len -= 1;
                } else {
                    break;
                }
            }
            if usize::from(w.len) < w.candidates.len() {
                let q = queue_slot(w, w.len);
                w.candidates[q] = slot;
                w.len += 1;
            } else {
                w.truncated = true;
            }
        }

        pub(super) fn push(w: &mut ExtremumWindow, cells: &mut [f64], v: f64, highest: bool) {
            let end = w.ring.base.saturating_add(w.ring.cap);
            if cells.get(w.ring.base as usize..end as usize).is_none() {
                return;
            }
            let slot = w.ring.wrap(w.ring.len);
            if w.len > 0 && candidate(w, 0) == slot {
                w.head = queue_slot(w, 1) as u8;
                w.len -= 1;
            }
            w.ring.push(cells, v);
            if w.len == 0 && w.truncated {
                w.head = 0;
                w.truncated = false;
                for i in 0..w.ring.len {
                    let slot = w.ring.wrap(i);
                    append(w, cells, slot, value(w, cells, slot), highest);
                }
            } else {
                append(w, cells, slot, v, highest);
            }
        }

        pub(super) fn best(w: &ExtremumWindow, cells: &[f64]) -> f64 {
            if w.len == 0 || !w.ring.window_ready(cells) {
                return f64::NAN;
            }
            value(w, cells, candidate(w, 0))
        }

        pub(super) fn best_present(w: &ExtremumWindow, cells: &[f64]) -> f64 {
            let end = w.ring.base.saturating_add(w.ring.cap);
            if w.len == 0
                || !w.ring.full()
                || cells.get(w.ring.base as usize..end as usize).is_none()
                || w.ring.last(cells).is_nan()
            {
                return f64::NAN;
            }
            value(w, cells, candidate(w, 0))
        }

        /// The scalar missing-value rule plus the original offset and signed
        /// zero behavior, with `window_value` given as `fold`.
        pub(super) fn arm(
            w: &ExtremumWindow,
            cells: &[f64],
            _highest: bool,
            bars: bool,
            fold: f64,
        ) -> f64 {
            let best = if bars {
                best(w, cells)
            } else {
                best_present(w, cells)
            };
            if best.is_nan() {
                f64::NAN
            } else if bars {
                offset(w)
            } else if best == 0.0 {
                fold
            } else {
                super::norm(best)
            }
        }

        pub(super) fn offset(w: &ExtremumWindow) -> f64 {
            let newest = w.ring.wrap(w.ring.len - 1);
            let best = candidate(w, 0);
            let distance = if newest >= best {
                newest - best
            } else {
                newest + w.ring.cap - best
            };
            -f64::from(distance)
        }
    }

    /// A plain version of `PairRing::push` and `PairRing::get`, kept in tests.
    fn reference_pair_push(p: &mut PairRing, cells: &mut [f64], a: f64, b: f64) {
        let (a, b) = if a.is_nan() || b.is_nan() {
            (f64::NAN, f64::NAN)
        } else {
            (a, b)
        };
        let cap = p.inner.cap;
        let slot = p.inner.base + 2 * p.inner.wrap(p.inner.len);
        let mut absent = true;
        let mut replaced_absent = true;
        if let Some(cell) = cells.get_mut(slot as usize) {
            replaced_absent = cell.is_nan();
            *cell = a;
            absent = a.is_nan();
        }
        if let Some(cell) = cells.get_mut(slot as usize + 1) {
            *cell = b;
        }
        p.inner.track_missing(absent, replaced_absent);
        if p.inner.len == cap {
            p.inner.head = if p.inner.head + 1 == cap {
                0
            } else {
                p.inner.head + 1
            };
        } else {
            p.inner.len += 1;
        }
    }

    fn reference_pair_get(p: PairRing, cells: &[f64], i: u32) -> (f64, f64) {
        if i >= p.inner.len {
            return (f64::NAN, f64::NAN);
        }
        let slot = p.inner.base + 2 * p.inner.wrap(i);
        let a = cells.get(slot as usize).copied().unwrap_or(f64::NAN);
        let b = cells.get(slot as usize + 1).copied().unwrap_or(f64::NAN);
        (a, b)
    }

    fn reference_pair_ready(p: PairRing, cells: &[f64]) -> bool {
        p.inner.full()
            && p.inner.missing == 0
            && cells
                .get(p.inner.base as usize..(p.inner.base + 2 * p.inner.cap) as usize)
                .is_some()
    }

    fn lcg(mut seed: u64) -> impl FnMut() -> u64 {
        move || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            seed >> 33
        }
    }

    #[test]
    fn split_extremum_window_matches_the_reference() {
        // Monotone runs overflow the 24-entry cache and force truncation and
        // rebuilds; short storage exercises the early return; small value
        // alphabets force ties; na, -0.0 and infinities are mixed in.
        let mut next = lcg(0x2545_f491_4f6c_dd1d);
        let (mut saw_truncated, mut saw_rebuild) = (false, false);
        for cap in [0u32, 1, 2, 3, 5, 23, 24, 25, 40, 64] {
            for base in [0u32, 3] {
                let exact = (base + cap.max(1)) as usize;
                for storage in [exact.saturating_sub(1), exact, exact + 2] {
                    for highest in [false, true] {
                        for mode in 0..4u64 {
                            let mut new_w = ExtremumWindow::new(base, cap);
                            let mut old_w = new_w;
                            let mut arm_w = new_w;
                            let mut gen_w = new_w;
                            let mut new_cells = vec![3.25; storage];
                            let mut old_cells = new_cells.clone();
                            let mut arm_cells = new_cells.clone();
                            let mut gen_cells = new_cells.clone();
                            let mut trend = 0.0f64;
                            for step in 0..400 {
                                let r = next();
                                let value = match mode {
                                    // Long monotone runs in both directions.
                                    0 => {
                                        trend += if (step / 57) % 2 == 0 { 1.0 } else { -1.0 };
                                        trend
                                    }
                                    // Tiny alphabet: many ties.
                                    1 => (r % 3) as f64,
                                    // Mixed specials.
                                    2 => match r % 11 {
                                        0 => f64::NAN,
                                        1 => -0.0,
                                        2 => 0.0,
                                        3 => f64::INFINITY,
                                        4 => f64::NEG_INFINITY,
                                        _ => (next() % 1000) as f64 / 8.0 - 60.0,
                                    },
                                    // Noisy trend with occasional na.
                                    _ => {
                                        trend += (r % 7) as f64 - 2.5;
                                        if r % 29 == 0 {
                                            f64::NAN
                                        } else {
                                            trend
                                        }
                                    }
                                };
                                // A rebuild follows when expiry drains a truncated cache.
                                let drained = storage >= exact
                                    && new_w.truncated
                                    && new_w.len == 1
                                    && new_w.candidate(0) == new_w.ring.wrap(new_w.ring.len);
                                new_w.push(&mut new_cells, value, highest);
                                saw_truncated |= new_w.truncated;
                                saw_rebuild |= drained;
                                reference_extremum::push(
                                    &mut old_w,
                                    &mut old_cells,
                                    value,
                                    highest,
                                );
                                let ctx = format!(
                                    "cap={cap} base={base} storage={storage} highest={highest} mode={mode} step={step}"
                                );
                                assert_eq!(new_w, old_w, "{ctx}");
                                assert_eq!(bits(&new_cells), bits(&old_cells), "{ctx}");
                                let best = new_w.best(&new_cells);
                                assert_eq!(
                                    best.to_bits(),
                                    reference_extremum::best(&old_w, &old_cells).to_bits(),
                                    "{ctx}"
                                );
                                if !best.is_nan() {
                                    assert_eq!(
                                        new_w.offset().to_bits(),
                                        reference_extremum::offset(&old_w).to_bits(),
                                        "{ctx}"
                                    );
                                }
                                // The engine arm: the plain arm on the plain
                                // window, the shipped `f64_op` expansion, and the generic
                                // twin at `f64` agree bit for bit.
                                let bars = r % 2 == 0;
                                let fold = 12_345.5;
                                let want = reference_extremum::arm(
                                    &old_w, &old_cells, highest, bars, fold,
                                );
                                let shipped = window_extremum_arm!(
                                    f64_op,
                                    arm_w,
                                    &mut arm_cells,
                                    value,
                                    highest,
                                    bars,
                                    fold,
                                    push,
                                    best,
                                    best_present,
                                    offset
                                );
                                let generic = gen_w.advance_generic(
                                    &mut gen_cells,
                                    value,
                                    highest,
                                    bars,
                                    fold,
                                );
                                assert_eq!(arm_w, old_w, "{ctx}");
                                assert_eq!(gen_w, old_w, "{ctx}");
                                assert_eq!(bits(&arm_cells), bits(&old_cells), "{ctx}");
                                assert_eq!(bits(&gen_cells), bits(&old_cells), "{ctx}");
                                assert_eq!(shipped.to_bits(), want.to_bits(), "{ctx}");
                                assert_eq!(generic.to_bits(), want.to_bits(), "{ctx}");
                            }
                        }
                    }
                }
            }
        }
        assert!(saw_truncated && saw_rebuild);
    }

    #[test]
    fn extremum_queue_helpers_keep_indices_in_bounds() {
        let mut next = lcg(7);
        for cap in [1u32, 24, 30] {
            let mut w = ExtremumWindow::new(0, cap);
            let mut cells = vec![0.0; cap as usize];
            for _ in 0..600 {
                let value = (next() % 50) as f64;
                w.push(&mut cells, value, next() % 2 == 0);
                assert!(usize::from(w.head) < w.candidates.len());
                assert!(usize::from(w.len) <= w.candidates.len());
                assert!(!w.truncated || w.len > 0);
                for i in 0..w.len {
                    assert!(w.queue_slot(i) < w.candidates.len());
                    assert!(w.candidate(i) < cap);
                }
            }
        }
    }

    #[test]
    fn split_pair_ring_matches_the_reference() {
        let mut next = lcg(0x9e37_79b9_7f4a_7c15);
        for cap in [0u32, 1, 2, 3, 5, 8, 17] {
            for base in [0u32, 1, 4] {
                let exact = (base + 2 * cap.max(1)) as usize;
                for storage in [0, 1, exact.saturating_sub(2), exact - 1, exact, exact + 3] {
                    let mut new_p = PairRing::new(base, cap);
                    let mut old_p = new_p;
                    let mut new_cells = vec![7.5; storage];
                    let mut old_cells = new_cells.clone();
                    for step in 0..200 {
                        let mut sample = || match next() % 9 {
                            0 => f64::NAN,
                            1 => -0.0,
                            2 => f64::INFINITY,
                            _ => (next() % 1000) as f64 / 8.0,
                        };
                        let (a, b) = (sample(), sample());
                        new_p.push(&mut new_cells, a, b);
                        reference_pair_push(&mut old_p, &mut old_cells, a, b);
                        let ctx = format!("cap={cap} base={base} storage={storage} step={step}");
                        assert_eq!(new_p, old_p, "{ctx}");
                        assert_eq!(bits(&new_cells), bits(&old_cells), "{ctx}");
                        assert_eq!(
                            new_p.ready(&new_cells),
                            reference_pair_ready(old_p, &old_cells),
                            "{ctx}"
                        );
                        for i in 0..cap.max(1) + 3 {
                            let (x, y) = new_p.get(&new_cells, i);
                            let (u, v) = reference_pair_get(old_p, &old_cells, i);
                            assert_eq!(
                                (x.to_bits(), y.to_bits()),
                                (u.to_bits(), v.to_bits()),
                                "{ctx} i={i}"
                            );
                            assert_eq!(
                                new_p.slot_of(i),
                                (i < old_p.inner.len).then(|| base + 2 * old_p.inner.wrap(i)),
                                "{ctx} i={i}"
                            );
                        }
                    }
                }
            }
        }
    }
}
