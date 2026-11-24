//! Per-call-site engine state, and the region layout that holds it.
//!
//! Every variant is `Copy` and pointer-free: ring buffers are `(base, cap,
//! len)` triples into one flat cell array. That is what lets the same state
//! machine run in the interpreter (over a `Vec`) and inside an emitted
//! artifact (over a compile-time-sized slice of linear memory) without a
//! second implementation.
//!
//! `Option<f64>` fields from the interpreter's state shapes are stored as
//! plain `f64` with `NAN` for `None`. That is exact rather than merely
//! convenient: each such field is only ever assigned a value that already
//! passed a finiteness check, so na and a real value can never collide. The
//! one place the argument does not hold is [`Smoother::value`], which can
//! reach ±∞ (and, for `length == 1` with an infinite predecessor, NaN), so
//! that one carries an explicit `seeded` flag.
//!
//! The smoothing kernels are written once as macros over an operation table:
//! `f64_op!` expands them for the shipped `f64` code, and `generic_op!` for the
//! extracted generic twin.

use crate::arena::{ExtremumWindow, PairRing, Ring};
use crate::{Decl, TaFunc};

/// Const-param ceiling on a window length, matching the interpreter's
/// defensive clamp and `TaParamRule::Length`'s upper bound.
const MAX_LENGTH: i64 = 100_000;

// A short contiguous scan is competitive with candidate bookkeeping.
// The bounded inline cache uses no additional cell storage at any length.
// This is a performance choice, not a semantic boundary.
const EXTREMUM_QUEUE_MIN_LENGTH: u32 = 32;

/// The operation table's `canon_zero`: a zero of either sign becomes `+0.0`,
/// anything else (NaN included) is returned as it is.
///
/// This is the rule for window extrema: `ta.highest`, `ta.lowest` and the
/// folded extremes inside `ta.stoch`, `ta.wpr` and `ta.range` never return
/// `-0.0`. The folds use plain `f64::max`/`f64::min` per cell
/// ([`fmax_raw`]/[`fmin_raw`]), whose result depends on the target only in
/// the sign of a zero, so one compare after the fold makes it the same on
/// every target. It is deliberately not the IEEE `minimum` of the window
/// (which is `-0.0` when a `-0.0` is present): deciding that would need a
/// second pass over the window whenever the extreme is zero.
#[inline]
pub(crate) fn canon_zero(a: f64) -> f64 {
    #[cfg(test)]
    {
        if tie_policy::plain_max_min() {
            return a;
        }
    }
    if a == 0.0 {
        0.0
    } else {
        a
    }
}

/// The operation table's `max_raw`: plain `f64::max`. The sign of a zero result
/// is the target's choice when one operand is `+0.0` and the other `-0.0`
/// (native arm64 and the wasm runtime differ), so this is only for sites
/// where that sign cannot reach an output: the window folds, whose zero
/// extreme is canonicalized afterwards ([`canon_zero`]), and `ta.rsi`,
/// `ta.cmo` and `ta.wad`, whose outputs do not depend on it.
#[inline]
pub(crate) fn fmax_raw(a: f64, b: f64) -> f64 {
    #[cfg(test)]
    {
        tie_policy::platform_max(a, b)
    }
    #[cfg(not(test))]
    {
        a.max(b)
    }
}

/// The operation table's `min_raw`: plain `f64::min` (see [`fmax_raw`]).
#[inline]
pub(crate) fn fmin_raw(a: f64, b: f64) -> f64 {
    #[cfg(test)]
    {
        tie_policy::platform_min(a, b)
    }
    #[cfg(not(test))]
    {
        a.min(b)
    }
}

/// Deterministic `max` of two `f64` values, the operation table's `max`.
///
/// `f64::max` leaves the `+0.0` vs `-0.0` tie to the target, and the native
/// and wasm builds of this crate picked different zeros. This is IEEE
/// 754-2019 `maximumNumber`, decided on every input: the larger operand;
/// `+0.0` on a signed-zero tie; with a NaN operand the other operand (the
/// right one when both are NaN). The result is always one of the two
/// operand bit patterns.
///
/// The definition is [`fmax_full`], which uses comparisons only. The first
/// two lines are a shortcut that changes nothing: `f64::max` is free to
/// pick either zero on the tie and may return a NaN when an operand is a
/// NaN, but any result `m` that is neither a zero nor a NaN is exactly
/// `fmax_full(a, b)` (the larger operand, or the one that is not NaN).
/// Every zero or NaN result is recomputed by the definition.
///
/// The shortcut costs a compare and a predicted branch per call. That is
/// fine once per bar (`ta.max`, `ta.sar`, the true range, the `ta.vwap`
/// variance) and too much once per window cell, so the window folds use
/// [`fmax_raw`] per cell and [`canon_zero`] on the result instead.
#[inline]
pub(crate) fn fmax_det(a: f64, b: f64) -> f64 {
    #[cfg(test)]
    {
        if tie_policy::plain_max_min() {
            return fmax_raw(a, b);
        }
    }
    let m = fmax_raw(a, b);
    if m.abs() > 0.0 {
        m
    } else {
        fmax_full(a, b)
    }
}

/// [`fmax_det`] without the shortcut, on every input.
#[inline]
#[allow(clippy::neg_cmp_op_on_partial_ord)]
pub(crate) fn fmax_full(a: f64, b: f64) -> f64 {
    if a == b {
        // Equal values: the same pattern twice, or the two zeros. `a` is
        // right when `b` is negative, and `b` is right otherwise, so `+0.0`
        // wins.
        if b.is_sign_negative() {
            a
        } else {
            b
        }
    } else if !(a <= b) && !a.is_nan() {
        // `a > b`, or `b` is NaN and `a` is not.
        a
    } else {
        b
    }
}

/// Deterministic `min` of two `f64` values, the operation table's `min`: the
/// mirror image of [`fmax_det`]. The smaller operand; `-0.0` on a
/// signed-zero tie; with a NaN operand the other operand (the right one
/// when both are NaN). The definition is [`fmin_full`]; the first two lines
/// are the same shortcut.
#[inline]
pub(crate) fn fmin_det(a: f64, b: f64) -> f64 {
    #[cfg(test)]
    {
        if tie_policy::plain_max_min() {
            return fmin_raw(a, b);
        }
    }
    let m = fmin_raw(a, b);
    if m.abs() > 0.0 {
        m
    } else {
        fmin_full(a, b)
    }
}

/// [`fmin_det`] without the shortcut, on every input.
#[inline]
#[allow(clippy::neg_cmp_op_on_partial_ord)]
pub(crate) fn fmin_full(a: f64, b: f64) -> f64 {
    if a == b {
        // As in `fmax_full`, mirrored: `-0.0` wins.
        if b.is_sign_negative() {
            b
        } else {
            a
        }
    } else if !(a >= b) && !a.is_nan() {
        // `a < b`, or `b` is NaN and `a` is not.
        a
    } else {
        b
    }
}

/// Test seam under the `max`/`min` helpers. It exists only under
/// `cfg(test)`; a non-test build contains no trace of it. Thread-local, so
/// parallel tests do not see each other.
///
/// * [`set_platform`] replaces `f64::max`/`f64::min` themselves (inside
///   [`fmax_raw`]/[`fmin_raw`], hence also inside the shortcut of
///   [`fmax_det`]/[`fmin_det`]) by a model of another target's tie-break.
///   The engine's outputs must not depend on it.
/// * [`set_plain_max_min`] turns the deterministic helpers back into plain
///   `f64::max`/`f64::min` and [`canon_zero`] into the identity. Every
///   signed-zero decision goes through one of the two, so with the switch on,
///   max/min use plain `f64::max`/`f64::min` and `canon_zero` is the
///   identity. (The engine's `long_window_zero` reads the same flag and runs
///   the ordered zero fold of the long windows again.)
/// * [`set_ieee`] makes every `max`/`min` deterministic, the raw ones
///   included: the definition that the shipped mixture of raw and deterministic
///   sites must equal.
/// * [`mixed_ties`] counts the calls that saw `+0.0` against `-0.0`.
#[cfg(test)]
pub(crate) mod tie_policy {
    extern crate std;
    use core::cell::Cell;

    /// What `f64::max`/`f64::min` do on a tie.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum Policy {
        /// This host's `f64::max`/`f64::min`.
        HostF64,
        /// The left operand wins every tie (`libm`'s `fmax`/`fmin`).
        LeftWins,
        /// The right operand wins every tie.
        RightWins,
        /// Each tie is broken by a pseudo-random bit. A compiler may treat
        /// `max`/`min` as commutative and swap operands per call site (the
        /// wasm runtime blob did not follow one rule), so this stands for
        /// every mixture of the two rules above.
        Scrambled,
    }

    std::thread_local! {
        static PLATFORM: Cell<Policy> = const { Cell::new(Policy::HostF64) };
        static LEGACY: Cell<bool> = const { Cell::new(false) };
        static IEEE: Cell<bool> = const { Cell::new(false) };
        static MIXED: Cell<u64> = const { Cell::new(0) };
        static COIN: Cell<u64> = const { Cell::new(0x9e37_79b9_7f4a_7c15) };
    }

    /// Select the modelled target for this thread; returns the previous one.
    pub(crate) fn set_platform(policy: Policy) -> Policy {
        PLATFORM.with(|p| p.replace(policy))
    }

    /// Switch the plain `max`/`min` behaviour on or off; returns the previous
    /// setting.
    pub(crate) fn set_plain_max_min(on: bool) -> bool {
        LEGACY.with(|l| l.replace(on))
    }

    /// Make every `max`/`min`, the raw ones included, the deterministic
    /// definition: the engine with IEEE `maximum`/`minimum` at every site.
    /// Returns the previous setting.
    pub(crate) fn set_ieee(on: bool) -> bool {
        IEEE.with(|l| l.replace(on))
    }

    pub(crate) fn plain_max_min() -> bool {
        LEGACY.with(Cell::get)
    }

    /// How many `max`/`min` calls on this thread had one operand `+0.0` and
    /// the other `-0.0`.
    pub(crate) fn mixed_ties() -> u64 {
        MIXED.with(Cell::get)
    }

    /// The next pseudo-random bit (xorshift64).
    fn coin() -> bool {
        COIN.with(|c| {
            let mut x = c.get();
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            c.set(x);
            x & 1 == 0
        })
    }

    fn left_max(a: f64, b: f64) -> f64 {
        if a.is_nan() || a < b {
            b
        } else {
            a
        }
    }

    fn left_min(a: f64, b: f64) -> f64 {
        if a.is_nan() || b < a {
            b
        } else {
            a
        }
    }

    fn note(a: f64, b: f64) -> Policy {
        if a == b && a.is_sign_negative() != b.is_sign_negative() {
            MIXED.with(|m| m.set(m.get() + 1));
        }
        PLATFORM.with(Cell::get)
    }

    pub(super) fn platform_max(a: f64, b: f64) -> f64 {
        let policy = note(a, b);
        if IEEE.with(Cell::get) {
            return super::fmax_full(a, b);
        }
        match policy {
            Policy::HostF64 => a.max(b),
            Policy::LeftWins => left_max(a, b),
            Policy::RightWins => left_max(b, a),
            Policy::Scrambled if coin() => left_max(a, b),
            Policy::Scrambled => left_max(b, a),
        }
    }

    pub(super) fn platform_min(a: f64, b: f64) -> f64 {
        let policy = note(a, b);
        if IEEE.with(Cell::get) {
            return super::fmin_full(a, b);
        }
        match policy {
            Policy::HostF64 => a.min(b),
            Policy::LeftWins => left_min(a, b),
            Policy::RightWins => left_min(b, a),
            Policy::Scrambled if coin() => left_min(a, b),
            Policy::Scrambled => left_min(b, a),
        }
    }
}

/// The `f64` operation table of the smoothing kernels: each
/// operation is the primitive `f64` operator or literal, so an expansion
/// with `f64_op` is token-for-token the hand-written `f64` code. The two
/// exceptions are `max` and `min`, which are [`fmax_det`]/[`fmin_det`]
/// (`f64::max`/`f64::min` with the signed-zero tie decided); the plain
/// `f64::max`/`f64::min` are `max_raw`/`min_raw`. `present`
/// and `norm` resolve at the expansion site (the engine's helpers).
macro_rules! f64_op {
    (one) => {
        1.0
    };
    (two) => {
        2.0
    };
    (nan) => {
        f64::NAN
    };
    (from_u32 $n:expr) => {
        f64::from($n)
    };
    (add $a:expr, $b:expr) => {
        $a + $b
    };
    (add_assign $p:expr, $b:expr) => {
        $p += $b
    };
    (sub $a:expr, $b:expr) => {
        $a - $b
    };
    (mul $a:expr, $b:expr) => {
        $a * $b
    };
    (div $a:expr, $b:expr) => {
        $a / $b
    };
    (present $a:expr) => {
        present($a)
    };
    (norm $a:expr) => {
        norm($a)
    };
    (is_nan $a:expr) => {
        $a.is_nan()
    };
    (ge $a:expr, $b:expr) => {
        $a >= $b
    };
    (le $a:expr, $b:expr) => {
        $a <= $b
    };
    (neg $a:expr) => {
        -$a
    };
    (eq_zero $a:expr) => {
        $a == 0.0
    };
    (max_min $a:expr) => {
        f64::MIN.max($a)
    };
    (min_max $a:expr) => {
        f64::MAX.min($a)
    };
    (zero) => {
        0.0
    };
    (hundred) => {
        100.0
    };
    (max $a:expr, $b:expr) => {
        $crate::state::fmax_det($a, $b)
    };
    (abs $a:expr) => {
        $a.abs()
    };
    (finite $a:expr) => {
        finite($a)
    };
    (gt $a:expr, $b:expr) => {
        $a > $b
    };
    (lt $a:expr, $b:expr) => {
        $a < $b
    };
    (min $a:expr, $b:expr) => {
        $crate::state::fmin_det($a, $b)
    };
    (canon_zero $a:expr) => {
        $crate::state::canon_zero($a)
    };
    (max_raw $a:expr, $b:expr) => {
        $crate::state::fmax_raw($a, $b)
    };
    (min_raw $a:expr, $b:expr) => {
        $crate::state::fmin_raw($a, $b)
    };
    (bool_out $b:expr) => {
        bool_out($b)
    };
    (neg_one) => {
        -1.0
    };
    (feq $a:expr, $b:expr) => {
        $a == $b
    };
    (is_finite $a:expr) => {
        $a.is_finite()
    };
    (from_i64 $n:expr) => {
        $n as f64
    };
    (three) => {
        3.0
    };
    (sqrt $a:expr) => {
        libm::sqrt($a)
    };
    (six) => {
        6.0
    };
    (floor $a:expr) => {
        libm::floor($a)
    };
    (exp $a:expr) => {
        pinewasm_num::raw::exp($a)
    };
    (midpoint $a:expr, $b:expr) => {
        f64::midpoint($a, $b)
    };
    (total_gt $a:expr, $b:expr) => {
        $a.total_cmp(&$b).is_gt()
    };
    (total_lt $a:expr, $b:expr) => {
        $a.total_cmp(&$b).is_lt()
    };
    (fne $a:expr, $b:expr) => {
        $a != $b
    };
    (lowest) => {
        f64::MIN
    };
    (highest) => {
        f64::MAX
    };
    (from_usize $n:expr) => {
        $n as f64
    };
    (ceil $a:expr) => {
        libm::ceil($a)
    };
    (to_usize $a:expr) => {
        $a as usize
    };
    (div_assign $p:expr, $b:expr) => {
        $p /= $b
    };
    (lambert) => {
        0.015
    };
    (paren $a:expr) => {
        ($a)
    };
}
pub(crate) use f64_op;

/// The generic operation table: every operation is a [`FloatOps`] method of the
/// type parameter `F` at the expansion site; `present x = !x.is_nan()` and
/// `norm v = if v.is_finite() { v } else { NAN }` (the engine's helpers).
#[cfg(any(test, feature = "aeneas"))]
macro_rules! generic_op {
    (canon_zero $a:expr) => {{
        let v = $a;
        if F::eq_zero(v) {
            F::zero()
        } else {
            v
        }
    }};
    // `max_raw`/`min_raw` are the deterministic `F::max`/`F::min` here:
    // `FloatOps` has no model of a target-dependent `f64::max`. That is
    // sound for the sites that use them. The plain `f64` result and the
    // deterministic one differ at most in the sign of a zero, and each such
    // site either removes that sign afterwards (`canon_zero` on the window
    // folds) or has outputs that do not depend on it (`ta.rsi`, `ta.cmo`,
    // `ta.wad`, the uncanonicalized operand of `ta.stoch`/`ta.wpr`/
    // `ta.range`), which the engine differential checks under every
    // tie-break model.
    (max_raw $a:expr, $b:expr) => {
        F::max($a, $b)
    };
    (min_raw $a:expr, $b:expr) => {
        F::min($a, $b)
    };
    (one) => {
        F::one()
    };
    (two) => {
        F::two()
    };
    (nan) => {
        F::nan()
    };
    (from_u32 $n:expr) => {
        F::from_u32($n)
    };
    (add $a:expr, $b:expr) => {
        F::add($a, $b)
    };
    (add_assign $p:expr, $b:expr) => {
        $p = F::add($p, $b)
    };
    (sub $a:expr, $b:expr) => {
        F::sub($a, $b)
    };
    (mul $a:expr, $b:expr) => {
        F::mul($a, $b)
    };
    (div $a:expr, $b:expr) => {
        F::div($a, $b)
    };
    (present $a:expr) => {
        !F::is_nan($a)
    };
    (norm $a:expr) => {{
        let v = $a;
        if F::is_finite(v) {
            v
        } else {
            F::nan()
        }
    }};
    (is_nan $a:expr) => {
        F::is_nan($a)
    };
    (ge $a:expr, $b:expr) => {
        F::ge($a, $b)
    };
    (le $a:expr, $b:expr) => {
        F::le($a, $b)
    };
    (neg $a:expr) => {
        F::neg($a)
    };
    (eq_zero $a:expr) => {
        F::eq_zero($a)
    };
    (max_min $a:expr) => {
        F::max_min($a)
    };
    (min_max $a:expr) => {
        F::min_max($a)
    };
    (zero) => {
        F::zero()
    };
    (hundred) => {
        F::hundred()
    };
    (max $a:expr, $b:expr) => {
        F::max($a, $b)
    };
    (abs $a:expr) => {
        F::abs($a)
    };
    (finite $a:expr) => {{
        let v = $a;
        if F::is_finite(v) {
            v
        } else {
            F::nan()
        }
    }};
    (gt $a:expr, $b:expr) => {
        F::gt($a, $b)
    };
    (lt $a:expr, $b:expr) => {
        F::lt($a, $b)
    };
    (min $a:expr, $b:expr) => {
        F::min($a, $b)
    };
    (bool_out $b:expr) => {
        if $b {
            F::one()
        } else {
            F::zero()
        }
    };
    (neg_one) => {
        F::neg_one()
    };
    (feq $a:expr, $b:expr) => {
        F::feq($a, $b)
    };
    (is_finite $a:expr) => {
        F::is_finite($a)
    };
    (from_i64 $n:expr) => {
        F::from_i64($n)
    };
    (three) => {
        F::three()
    };
    (sqrt $a:expr) => {
        F::sqrt($a)
    };
    (six) => {
        F::six()
    };
    (floor $a:expr) => {
        F::floor($a)
    };
    (exp $a:expr) => {
        F::exp($a)
    };
    (midpoint $a:expr, $b:expr) => {
        F::midpoint($a, $b)
    };
    (total_gt $a:expr, $b:expr) => {
        F::total_gt($a, $b)
    };
    (total_lt $a:expr, $b:expr) => {
        F::total_lt($a, $b)
    };
    (fne $a:expr, $b:expr) => {
        !F::feq($a, $b)
    };
    (lowest) => {
        F::lowest()
    };
    (highest) => {
        F::highest()
    };
    (from_usize $n:expr) => {
        F::from_usize($n)
    };
    (ceil $a:expr) => {
        F::ceil($a)
    };
    (to_usize $a:expr) => {
        F::to_usize($a)
    };
    (div_assign $p:expr, $b:expr) => {
        $p = F::div($p, $b)
    };
    (lambert) => {
        F::lambert()
    };
    (paren $a:expr) => {
        $a
    };
}
#[cfg(any(test, feature = "aeneas"))]
pub(crate) use generic_op;

/// `Smoother::step`'s body over an operation table (`f64_op` for the
/// shipped method, `generic_op` for the extracted generic twin).
macro_rules! smoother_step_fn {
    ($op:ident, $f:ty, $name:ident) => {
        /// Advance with a non-na input; returns the current output (finite
        /// once seeded, `NAN` while still accumulating).
        pub fn $name(&mut self, x: $f, length: u32, alpha: $f) -> $f {
            if self.seeded {
                self.value = $op!(add $op!(mul alpha, x), $op!(mul $op!(sub $op!(one), alpha), self.value));
            } else {
                $op!(add_assign self.sum, x);
                self.count += 1;
                if self.count >= length {
                    self.value = $op!(div self.sum, $op!(from_u32 length));
                    self.seeded = true;
                }
            }
            if self.seeded {
                self.value
            } else {
                $op!(nan)
            }
        }
    };
}

/// The engine's `State::Smooth` arm (`ta.ema`/`ta.rma`/`ta.atr`) after the
/// alpha: a present input steps and passes the result through `norm`
/// (non-finite becomes na); an na input yields na and leaves the state
/// untouched. `$step` is the smoother method the operation table pairs with.
macro_rules! smooth_advance {
    ($op:ident, $s:expr, $step:ident, $x:expr, $length:expr, $alpha:expr) => {
        if $op!(present $x) {
            $op!(norm $s.$step($x, $length, $alpha))
        } else {
            // na input: na output this bar, state untouched.
            $op!(nan)
        }
    };
}
pub(crate) use smooth_advance;

/// The RMA alpha, `1.0 / f64::from(length)` (`ta.rma`, `ta.atr`).
macro_rules! rma_alpha {
    ($op:ident, $length:expr) => {
        $op!(div $op!(one), $op!(from_u32 $length))
    };
}
pub(crate) use rma_alpha;

/// The ema alpha, `2.0 / (f64::from(length) + 1.0)`.
macro_rules! ema_alpha {
    ($op:ident, $length:expr) => {
        $op!(div $op!(two), $op!(add $op!(from_u32 $length), $op!(one)))
    };
}
pub(crate) use ema_alpha;

/// Seed-then-recurse accumulator shared by `ta.ema`, `ta.rma`, and the
/// smoothing inside `ta.rsi`/`ta.atr`. The engine uses `Smoother<f64>` (the
/// default); the type parameter exists for the extracted generic twin.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Smoother<F = f64> {
    sum: F,
    value: F,
    count: u32,
    seeded: bool,
}

impl Smoother<f64> {
    smoother_step_fn!(f64_op, f64, step);
}

/// The binary64 operations the smoothing kernels evaluate: the literals `1.0`, `2.0`,
/// `NAN`, `f64::from(u32)`, the four arithmetic operators, `is_nan` and
/// `is_finite`. It exists only for the extracted generic twins of the
/// kernels; the shipped code expands the same kernel macros with `f64_op`.
/// `f64` is the only instance, and each method is its `f64_op` operation.
#[cfg(any(test, feature = "aeneas"))]
pub trait FloatOps: Copy {
    /// `1.0`.
    fn one() -> Self;
    /// `2.0`.
    fn two() -> Self;
    /// `f64::NAN`.
    fn nan() -> Self;
    /// `f64::from(n)`.
    fn from_u32(n: u32) -> Self;
    /// `self + rhs`.
    fn add(self, rhs: Self) -> Self;
    /// `self - rhs`.
    fn sub(self, rhs: Self) -> Self;
    /// `self * rhs`.
    fn mul(self, rhs: Self) -> Self;
    /// `self / rhs`.
    fn div(self, rhs: Self) -> Self;
    /// `self.is_nan()`.
    fn is_nan(self) -> bool;
    /// `self.is_finite()`.
    fn is_finite(self) -> bool;
    /// `self >= rhs` (false when either side is NaN).
    fn ge(self, rhs: Self) -> bool;
    /// `self <= rhs` (false when either side is NaN).
    fn le(self, rhs: Self) -> bool;
    /// `-self`.
    fn neg(self) -> Self;
    /// `self == 0.0`.
    fn eq_zero(self) -> bool;
    /// `f64::MIN.max(self)`.
    fn max_min(self) -> Self;
    /// `f64::MAX.min(self)`.
    fn min_max(self) -> Self;
    /// `0.0`.
    fn zero() -> Self;
    /// `100.0`.
    fn hundred() -> Self;
    /// `fmax_det(self, rhs)`: the larger operand, `+0.0` on a signed-zero
    /// tie, the other argument when one side is NaN (`rhs` when both are).
    fn max(self, rhs: Self) -> Self;
    /// `self.abs()`.
    fn abs(self) -> Self;
    /// `self > rhs` (false when either side is NaN).
    fn gt(self, rhs: Self) -> bool;
    /// `self < rhs` (false when either side is NaN).
    fn lt(self, rhs: Self) -> bool;
    /// `fmin_det(self, rhs)`: the smaller operand, `-0.0` on a signed-zero
    /// tie, the other argument when one side is NaN (`rhs` when both are).
    fn min(self, rhs: Self) -> Self;
    /// `-1.0`.
    fn neg_one() -> Self;
    /// `self == rhs` (false when either side is NaN; `-0.0 == 0.0`).
    fn feq(self, rhs: Self) -> bool;
    /// `n as f64`.
    fn from_i64(n: i64) -> Self;
    /// `3.0`.
    fn three() -> Self;
    /// `libm::sqrt(self)` (IEEE-754 correctly rounded square root).
    fn sqrt(self) -> Self;
    /// `6.0`.
    fn six() -> Self;
    /// `libm::floor(self)` (exact: round toward negative infinity).
    fn floor(self) -> Self;
    /// `pinewasm_num::raw::exp(self)` (the `exp` of `pinewasm_num`, which the
    /// artifact and the interpreter share; not correctly rounded).
    fn exp(self) -> Self;
    /// `f64::midpoint(self, rhs)` (`core`'s overflow-free `(a + b) / 2`).
    fn midpoint(self, rhs: Self) -> Self;
    /// `self.total_cmp(&rhs).is_gt()` (IEEE-754 totalOrder, exact on bits).
    fn total_gt(self, rhs: Self) -> bool;
    /// `self.total_cmp(&rhs).is_lt()` (IEEE-754 totalOrder, exact on bits).
    fn total_lt(self, rhs: Self) -> bool;
    /// `f64::MIN` (the most negative finite value).
    fn lowest() -> Self;
    /// `f64::MAX` (the largest finite value).
    fn highest() -> Self;
    /// `n as f64` for `n: usize` (round to nearest even; exact below 2^53).
    fn from_usize(n: usize) -> Self;
    /// `libm::ceil(self)` (exact: round toward positive infinity).
    fn ceil(self) -> Self;
    /// `self as usize`: truncation toward zero, saturating at `0` and
    /// `usize::MAX`; NaN gives `0` (exact on bits for a given `usize` width).
    fn to_usize(self) -> usize;
    /// `0.015` (the constant of `ta.cci`; a literal, exact on bits).
    fn lambert() -> Self;
}

/// The `f64` primitives behind `impl FloatOps for f64`, one per method, each
/// the `f64_op` operation. The extraction keeps this module opaque (float
/// literals and float arithmetic do not translate); each function is
/// specified as its IEEE-754 binary64 operation on the bit pattern.
#[cfg(any(test, feature = "aeneas"))]
mod f64_ops {
    pub(super) fn one() -> f64 {
        f64_op!(one)
    }
    pub(super) fn two() -> f64 {
        f64_op!(two)
    }
    pub(super) fn nan() -> f64 {
        f64_op!(nan)
    }
    pub(super) fn from_u32(n: u32) -> f64 {
        f64_op!(from_u32 n)
    }
    pub(super) fn add(a: f64, b: f64) -> f64 {
        f64_op!(add a, b)
    }
    pub(super) fn sub(a: f64, b: f64) -> f64 {
        f64_op!(sub a, b)
    }
    pub(super) fn mul(a: f64, b: f64) -> f64 {
        f64_op!(mul a, b)
    }
    pub(super) fn div(a: f64, b: f64) -> f64 {
        f64_op!(div a, b)
    }
    pub(super) fn is_nan(a: f64) -> bool {
        a.is_nan()
    }
    pub(super) fn is_finite(a: f64) -> bool {
        a.is_finite()
    }
    pub(super) fn ge(a: f64, b: f64) -> bool {
        f64_op!(ge a, b)
    }
    pub(super) fn le(a: f64, b: f64) -> bool {
        f64_op!(le a, b)
    }
    pub(super) fn neg(a: f64) -> f64 {
        f64_op!(neg a)
    }
    pub(super) fn eq_zero(a: f64) -> bool {
        f64_op!(eq_zero a)
    }
    pub(super) fn max_min(a: f64) -> f64 {
        f64_op!(max_min a)
    }
    pub(super) fn min_max(a: f64) -> f64 {
        f64_op!(min_max a)
    }
    pub(super) fn zero() -> f64 {
        f64_op!(zero)
    }
    pub(super) fn hundred() -> f64 {
        f64_op!(hundred)
    }
    pub(super) fn max(a: f64, b: f64) -> f64 {
        f64_op!(max a, b)
    }
    pub(super) fn abs(a: f64) -> f64 {
        f64_op!(abs a)
    }
    pub(super) fn gt(a: f64, b: f64) -> bool {
        f64_op!(gt a, b)
    }
    pub(super) fn lt(a: f64, b: f64) -> bool {
        f64_op!(lt a, b)
    }
    pub(super) fn min(a: f64, b: f64) -> f64 {
        f64_op!(min a, b)
    }
    pub(super) fn neg_one() -> f64 {
        f64_op!(neg_one)
    }
    pub(super) fn feq(a: f64, b: f64) -> bool {
        f64_op!(feq a, b)
    }
    pub(super) fn from_i64(n: i64) -> f64 {
        f64_op!(from_i64 n)
    }
    pub(super) fn three() -> f64 {
        f64_op!(three)
    }
    pub(super) fn sqrt(a: f64) -> f64 {
        f64_op!(sqrt a)
    }
    pub(super) fn six() -> f64 {
        f64_op!(six)
    }
    pub(super) fn floor(a: f64) -> f64 {
        f64_op!(floor a)
    }
    pub(super) fn exp(a: f64) -> f64 {
        f64_op!(exp a)
    }
    pub(super) fn midpoint(a: f64, b: f64) -> f64 {
        f64_op!(midpoint a, b)
    }
    pub(super) fn total_gt(a: f64, b: f64) -> bool {
        f64_op!(total_gt a, b)
    }
    pub(super) fn total_lt(a: f64, b: f64) -> bool {
        f64_op!(total_lt a, b)
    }
    pub(super) fn lowest() -> f64 {
        f64_op!(lowest)
    }
    pub(super) fn highest() -> f64 {
        f64_op!(highest)
    }
    pub(super) fn from_usize(n: usize) -> f64 {
        f64_op!(from_usize n)
    }
    pub(super) fn ceil(a: f64) -> f64 {
        f64_op!(ceil a)
    }
    pub(super) fn to_usize(a: f64) -> usize {
        f64_op!(to_usize a)
    }
    pub(super) fn lambert() -> f64 {
        f64_op!(lambert)
    }
}

#[cfg(any(test, feature = "aeneas"))]
impl FloatOps for f64 {
    fn one() -> f64 {
        f64_ops::one()
    }
    fn two() -> f64 {
        f64_ops::two()
    }
    fn nan() -> f64 {
        f64_ops::nan()
    }
    fn from_u32(n: u32) -> f64 {
        f64_ops::from_u32(n)
    }
    fn add(self, rhs: f64) -> f64 {
        f64_ops::add(self, rhs)
    }
    fn sub(self, rhs: f64) -> f64 {
        f64_ops::sub(self, rhs)
    }
    fn mul(self, rhs: f64) -> f64 {
        f64_ops::mul(self, rhs)
    }
    fn div(self, rhs: f64) -> f64 {
        f64_ops::div(self, rhs)
    }
    fn is_nan(self) -> bool {
        f64_ops::is_nan(self)
    }
    fn is_finite(self) -> bool {
        f64_ops::is_finite(self)
    }
    fn ge(self, rhs: f64) -> bool {
        f64_ops::ge(self, rhs)
    }
    fn le(self, rhs: f64) -> bool {
        f64_ops::le(self, rhs)
    }
    fn neg(self) -> f64 {
        f64_ops::neg(self)
    }
    fn eq_zero(self) -> bool {
        f64_ops::eq_zero(self)
    }
    fn max_min(self) -> f64 {
        f64_ops::max_min(self)
    }
    fn min_max(self) -> f64 {
        f64_ops::min_max(self)
    }
    fn zero() -> f64 {
        f64_ops::zero()
    }
    fn hundred() -> f64 {
        f64_ops::hundred()
    }
    fn max(self, rhs: f64) -> f64 {
        f64_ops::max(self, rhs)
    }
    fn abs(self) -> f64 {
        f64_ops::abs(self)
    }
    fn gt(self, rhs: f64) -> bool {
        f64_ops::gt(self, rhs)
    }
    fn lt(self, rhs: f64) -> bool {
        f64_ops::lt(self, rhs)
    }
    fn min(self, rhs: f64) -> f64 {
        f64_ops::min(self, rhs)
    }
    fn neg_one() -> f64 {
        f64_ops::neg_one()
    }
    fn feq(self, rhs: f64) -> bool {
        f64_ops::feq(self, rhs)
    }
    fn from_i64(n: i64) -> f64 {
        f64_ops::from_i64(n)
    }
    fn three() -> f64 {
        f64_ops::three()
    }
    fn sqrt(self) -> f64 {
        f64_ops::sqrt(self)
    }
    fn six() -> f64 {
        f64_ops::six()
    }
    fn floor(self) -> f64 {
        f64_ops::floor(self)
    }
    fn exp(self) -> f64 {
        f64_ops::exp(self)
    }
    fn midpoint(self, rhs: f64) -> f64 {
        f64_ops::midpoint(self, rhs)
    }
    fn total_gt(self, rhs: f64) -> bool {
        f64_ops::total_gt(self, rhs)
    }
    fn total_lt(self, rhs: f64) -> bool {
        f64_ops::total_lt(self, rhs)
    }
    fn lowest() -> f64 {
        f64_ops::lowest()
    }
    fn highest() -> f64 {
        f64_ops::highest()
    }
    fn from_usize(n: usize) -> f64 {
        f64_ops::from_usize(n)
    }
    fn ceil(self) -> f64 {
        f64_ops::ceil(self)
    }
    fn to_usize(self) -> usize {
        f64_ops::to_usize(self)
    }
    fn lambert() -> f64 {
        f64_ops::lambert()
    }
}

/// The extracted generic twins: the same kernel macros expanded with
/// `generic_op`.
#[cfg(any(test, feature = "aeneas"))]
impl<F: FloatOps> Smoother<F> {
    smoother_step_fn!(generic_op, F, step_generic);

    /// `smooth_advance!` over `generic_op`: the engine's smoothing arm.
    pub(crate) fn advance_generic(&mut self, x: F, length: u32, alpha: F) -> F {
        smooth_advance!(generic_op, self, step_generic, x, length, alpha)
    }
}

/// `rma_alpha!` over `generic_op`.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn rma_alpha_generic<F: FloatOps>(length: u32) -> F {
    rma_alpha!(generic_op, length)
}

/// `ema_alpha!` over `generic_op`.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn ema_alpha_generic<F: FloatOps>(length: u32) -> F {
    ema_alpha!(generic_op, length)
}

/// `ta.sar` recursion state: the running SAR (`result`), the extreme point
/// (`max_min`), the acceleration factor, the trend side, and the last two
/// bars' highs/lows plus the last close. `bars` counts executed advances.
/// The engine uses `Sar<f64>` (the default); the type parameter exists for
/// the extracted generic twin of the `ta.sar` arm.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Sar<F = f64> {
    pub(crate) bars: u64,
    pub(crate) result: F,
    pub(crate) max_min: F,
    pub(crate) accel: F,
    pub(crate) is_below: bool,
    pub(crate) prev1_high: F,
    pub(crate) prev1_low: F,
    pub(crate) prev2_high: F,
    pub(crate) prev2_low: F,
    pub(crate) prev_close: F,
}

/// Per-call-site engine state, one per `TaStateDecl`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum State {
    /// `fixnan`: the last non-na value seen.
    Fixnan { last: f64 },
    /// Pure window functions over one numeric source.
    Window { ring: Ring },
    /// `ta.vwma`: (source, volume) pairs.
    WindowPair { ring: PairRing },
    /// Production `ta.vwma`: independent non-na weighted/volume windows.
    /// The extracted model still constructs `WindowPair`;
    /// `Engine::reset` applies the current production policy.
    VwmaIndependent { weighted: Ring, volumes: Ring },
    /// `ta.hma`: the source window plus the intermediate
    /// `2*wma(len/2) - wma(len)` series feeding the outer `wma(sqrt(len))`.
    Hma { src: Ring, inter: Ring },
    /// `ta.ema` / `ta.rma` / `ta.atr`.
    Smooth { s: Smoother },
    /// `ta.rsi`: previous source plus gain/loss smoothing.
    Rsi {
        prev: f64,
        up: Smoother,
        down: Smoother,
    },
    /// `ta.tr` and the other computed-from-data functions: no state.
    Stateless,
    /// `ta.cum`: the running total.
    Cum { total: f64 },
    /// `ta.max` / `ta.min`: the all-time running extremum.
    Extremum { value: f64 },
    /// `ta.crossover` / `ta.crossunder` / `ta.cross`: previous operands.
    Cross {
        prev_a: f64,
        prev_b: f64,
        seen: bool,
    },
    /// `ta.barssince`: bars since the condition was last true
    /// (`i64::MIN` = never).
    BarsSince { since: i64 },
    /// `ta.valuewhen`: the last `occurrence + 1` captured values.
    ValueWhen { vals: Ring },
    /// `ta.correlation`: (source1, source2) pairs.
    Corr { ring: PairRing },
    /// `ta.stoch`: (high, low) operand pairs.
    Stoch { ring: PairRing },
    /// `ta.mfi`: the previous source plus (up-flow, down-flow) pairs.
    Mfi { prev: f64, ring: PairRing },
    /// `ta.wpr`: windows of market-data highs and lows.
    Wpr { highs: Ring, lows: Ring },
    /// `ta.obv`: running total plus the previous close.
    Obv { total: f64, prev_close: f64 },
    /// `ta.accdist`: running total of `clv * volume`.
    AccDist { total: f64 },
    /// `ta.nvi` / `ta.pvi`: the index value plus the previous close/volume.
    VolumeIndex {
        value: f64,
        prev_close: f64,
        prev_volume: f64,
    },
    /// `ta.pvt`: running total plus the previous close.
    Pvt { total: f64, prev_close: f64 },
    /// `ta.wad`: running total plus the previous close.
    Wad { total: f64, prev_close: f64 },
    /// `ta.vwap`: cumulative price*volume, price²*volume, and volume sums.
    Vwap {
        pv: f64,
        p2v: f64,
        vol: f64,
        active: bool,
    },
    /// `ta.sar`: the parabolic stop-and-reverse recursion.
    Sar { s: Sar },
    /// `ta.tsi`: previous source plus the four ema smoothers.
    Tsi {
        prev: f64,
        pc_long: Smoother,
        pc_short: Smoother,
        abs_long: Smoother,
        abs_short: Smoother,
    },
    /// `ta.kcw` / `ta.kc`: emas of the source and of the bar range.
    Keltner {
        basis: Smoother,
        range_ema: Smoother,
    },
    /// `ta.pivothigh` / `ta.pivotlow`.
    Pivot { ring: Ring },
    /// `ta.macd`: fast/slow source emas plus the signal ema.
    Macd {
        fast: Smoother,
        slow: Smoother,
        signal: Smoother,
    },
    /// `ta.bb`: a source window (basis and population stdev share it).
    Bb { ring: Ring },
    /// `ta.dmi`: the directional movement index.
    Dmi {
        prev_high: f64,
        prev_low: f64,
        tr: Smoother,
        plus_dm: Smoother,
        minus_dm: Smoother,
        adx: Smoother,
    },
    /// `ta.supertrend`: the ATR smoother plus the trailing-band recursion.
    Supertrend {
        atr: Smoother,
        bands_seeded: bool,
        prev_upper: f64,
        prev_lower: f64,
        prev_supertrend: f64,
        prev_direction: f64,
    },
    /// `timeframe.change`: the previous bar's bucket id (`i64::MIN` = none).
    TimeframeChange { prev_bucket: i64 },
    /// Defense in depth: a declaration the validator would have rejected
    /// (a slot-based function in the engine table). Never advanceable.
    Invalid,
    /// Highest/lowest and their offsets, with a bounded candidate deque.
    /// Append new states so existing hot state discriminants remain stable.
    WindowExtremum { window: ExtremumWindow },
}

/// A declaration's window length: const param 0, defensively clamped.
pub(crate) fn length_of(decl: &Decl) -> u32 {
    decl.param(0).unwrap_or(1).clamp(1, MAX_LENGTH) as u32
}

fn param_clamped(decl: &Decl, i: usize, default: i64) -> u32 {
    decl.param(i).unwrap_or(default).clamp(0, MAX_LENGTH) as u32
}

/// `round(sqrt(len))`, the outer `ta.hma` window (at least 1).
///
/// Computed in integers so [`Layout::plan`] (which reaches this through
/// [`cells_for`]) stays free of `f64` for Aeneas extraction. It is the same
/// value the float form `libm::round(libm::sqrt(len as f64))` gives for
/// every `u32`: `lo = floor(sqrt(len))` by bisection (`lo <= 65535`, so no
/// product overflows `u64`), and since `len` is an integer, `sqrt(len)` is
/// never exactly `lo + 1/2`, so it rounds up iff `len > lo * lo + lo`. The
/// float square root is correctly rounded and stays at least `1e-6` away
/// from any half-integer below `2^16`, so it cannot round differently.
pub(crate) fn hma_outer(len: u32) -> u32 {
    let n = u64::from(len);
    let mut lo = 0u64;
    let mut hi = 65_536u64;
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if mid * mid <= n {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let r = if n > lo * lo + lo { lo + 1 } else { lo };
    if r == 0 {
        1
    } else {
        r as u32
    }
}

/// Cells one declaration's rings occupy in the shared cell array.
pub fn cells_for(decl: &Decl) -> u32 {
    // Not named `func`: Aeneas would print that binder where it shadows the
    // `func` module in the extracted `func::TaFunc` patterns.
    let Some(ta_func) = decl.func() else {
        return 0;
    };
    let length = length_of(decl);
    match ta_func {
        TaFunc::MathSum
        | TaFunc::Wma
        | TaFunc::Alma
        | TaFunc::Stdev
        | TaFunc::Dev
        | TaFunc::Variance
        | TaFunc::Linreg
        | TaFunc::Cci
        | TaFunc::Cog
        | TaFunc::Median
        | TaFunc::MedianInt
        | TaFunc::Mode
        | TaFunc::ModeInt
        | TaFunc::PercentileLinearInterpolation
        | TaFunc::PercentileNearestRank
        | TaFunc::Range
        | TaFunc::RangeInt
        | TaFunc::Rci
        | TaFunc::Bbw
        | TaFunc::Bb => length,
        TaFunc::Mom
        | TaFunc::Roc
        | TaFunc::Rising
        | TaFunc::Falling
        | TaFunc::Cmo
        | TaFunc::PercentRank => length.saturating_add(1),
        TaFunc::Highest | TaFunc::Lowest | TaFunc::HighestBars | TaFunc::LowestBars => length,
        TaFunc::Swma => 4,
        TaFunc::Vwma | TaFunc::Correlation | TaFunc::Stoch | TaFunc::Mfi => {
            length.saturating_mul(2)
        }
        TaFunc::Hma => {
            let len = length.max(2);
            len.saturating_add(hma_outer(len))
        }
        TaFunc::Wpr => length.saturating_mul(2),
        TaFunc::ValueWhen
        | TaFunc::ValueWhenInt
        | TaFunc::ValueWhenBool
        | TaFunc::ValueWhenColor => {
            (decl.param(0).unwrap_or(0).clamp(0, MAX_LENGTH) as u32).saturating_add(1)
        }
        TaFunc::PivotHigh | TaFunc::PivotLow => param_clamped(decl, 0, 1)
            .saturating_add(param_clamped(decl, 1, 1))
            .saturating_add(1),
        _ => 0,
    }
}

/// Scratch cells one declaration needs for a sorted copy of its window
/// (`ta.median`, `ta.mode`, the percentiles, `ta.rci`). The region is shared
/// across sites, so the engine reserves the maximum rather than the sum.
pub fn scratch_for(decl: &Decl) -> u32 {
    match decl.func() {
        Some(
            TaFunc::Median
            | TaFunc::MedianInt
            | TaFunc::Mode
            | TaFunc::ModeInt
            | TaFunc::PercentileLinearInterpolation
            | TaFunc::PercentileNearestRank
            | TaFunc::Rci,
        ) => length_of(decl),
        _ => 0,
    }
}

/// `(cells_for, scratch_for)` of one declaration, or `(0, 0)` for none: the
/// identity of both of [`Layout::plan`]'s folds. A separate function so the
/// plan loop carries no borrow across iterations (Aeneas extraction).
#[inline]
fn decl_footprint(decl: Option<&Decl>) -> (u32, u32) {
    match decl {
        Some(decl) => (cells_for(decl), scratch_for(decl)),
        None => (0, 0),
    }
}

/// Bytes an engine region reserves per [`State`].
///
/// A published upper bound rather than `size_of::<State>()`: the emitter is a
/// host-side build and the artifact's copy of the engine is a wasm one, and
/// nothing in the language promises those two agree on an enum's layout. Both
/// sides step by this constant between regions, while the state slice itself
/// uses whatever the real (never larger) stride is.
pub const STATE_BYTES: u32 = 128;
const _: () = assert!(
    core::mem::size_of::<State>() as u32 <= STATE_BYTES,
    "State outgrew the reserved stride; raise STATE_BYTES"
);

/// Sizes of the three regions a program's engine needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Layout {
    /// [`State`] entries, one per declaration.
    pub states: u32,
    /// Cells shared by every ring.
    pub cells: u32,
    /// Cells reserved for the shared sorting scratch.
    pub scratch: u32,
}

/// One indexed footprint read for the scalar layout fold.
trait LayoutFootprints {
    fn footprint(&self, index: usize) -> (u32, u32);
}

struct DeclFootprints<'a>(&'a [Decl]);

impl LayoutFootprints for DeclFootprints<'_> {
    fn footprint(&self, index: usize) -> (u32, u32) {
        decl_footprint(self.0.get(index))
    }
}

/// The production layout fold, independent of declaration representation.
/// Reads remain ordered even after the cell sum has saturated.
fn plan_footprints<P: LayoutFootprints>(table: &P, count: usize) -> Layout {
    let mut cells = 0u32;
    let mut scratch = 0u32;
    let mut i = 0usize;
    while i < count {
        let (c, s) = table.footprint(i);
        cells = cells.saturating_add(c);
        scratch = scratch.max(s);
        i += 1;
    }
    Layout {
        states: count as u32,
        cells,
        scratch,
    }
}

impl Layout {
    /// Plan the regions for `decls`. Every size comes from baked const
    /// params, so an artifact can reserve exactly this much at compile time
    /// and never allocate at run time.
    pub fn plan(decls: &[Decl]) -> Layout {
        plan_footprints(&DeclFootprints(decls), decls.len())
    }

    /// Total f64 cells (rings plus scratch).
    pub fn total_cells(self) -> u32 {
        self.cells.saturating_add(self.scratch)
    }

    /// Bytes the whole engine region occupies: the validated declaration
    /// copies, the states, then the cells and the shared scratch.
    ///
    /// The emitter reserves exactly this much linear memory and the artifact's
    /// engine lays the same regions out inside it, both from this one
    /// function — the layout cannot drift between the two sides.
    pub fn region_bytes(self) -> u32 {
        self.states
            .saturating_mul(crate::Decl::SIZE as u32 + STATE_BYTES)
            .saturating_add(self.total_cells().saturating_mul(8))
    }

    /// Offsets of the three regions after the declaration copies, relative to
    /// the region base: `(states, cells, scratch)`.
    pub fn offsets(self) -> (u32, u32, u32) {
        let states = self.states.saturating_mul(crate::Decl::SIZE as u32);
        let cells = states.saturating_add(self.states.saturating_mul(STATE_BYTES));
        let scratch = cells.saturating_add(self.cells.saturating_mul(8));
        (states, cells, scratch)
    }
}

/// Free-function extraction roots for Charon (`--start-from` cannot name
/// inherent methods at the pinned Charon revision). Compiled only with
/// `--features aeneas`.
#[cfg(feature = "aeneas")]
pub mod aeneas_roots {
    use super::Layout;
    use crate::Decl;

    /// Root for `Layout::plan`.
    pub fn layout_plan(decls: &[Decl]) -> Layout {
        Layout::plan(decls)
    }

    /// Root for `Layout::total_cells`.
    pub fn layout_total_cells(l: Layout) -> u32 {
        l.total_cells()
    }

    /// Root for `Layout::region_bytes`.
    pub fn layout_region_bytes(l: Layout) -> u32 {
        l.region_bytes()
    }

    /// Root for `Layout::offsets`.
    pub fn layout_offsets(l: Layout) -> (u32, u32, u32) {
        l.offsets()
    }
}

/// The cell-bearing part of one site's initial [`State`]: which rings it
/// builds and where, with no `f64` in sight.
///
/// A site's initial state is [`init_shape`] followed by [`state_of`]. The
/// split keeps the base/capacity arithmetic (the part a reset has to get
/// right for the sites' cells to stay disjoint) inside the subset Aeneas
/// extracts; `state_of` only moves these rings into their [`State`] variant
/// and fills in the float fields' initial values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shape {
    /// [`State::Invalid`].
    Invalid,
    /// [`State::Window`].
    Window { ring: Ring },
    /// [`State::WindowExtremum`].
    WindowExtremum { window: ExtremumWindow },
    /// [`State::WindowPair`].
    WindowPair { ring: PairRing },
    /// [`State::Hma`].
    Hma { src: Ring, inter: Ring },
    /// [`State::ValueWhen`].
    ValueWhen { vals: Ring },
    /// [`State::Corr`].
    Corr { ring: PairRing },
    /// [`State::Stoch`].
    Stoch { ring: PairRing },
    /// [`State::Mfi`].
    Mfi { ring: PairRing },
    /// [`State::Wpr`].
    Wpr { highs: Ring, lows: Ring },
    /// [`State::Pivot`].
    Pivot { ring: Ring },
    /// [`State::Bb`].
    Bb { ring: Ring },
    /// A state that owns no cells; [`scalar_state`] builds it.
    Scalar { ta_func: TaFunc },
}

/// The rings one declaration's initial state takes from `base` on (see
/// [`Shape`]). Malformed declarations produce [`Shape::Invalid`], hence
/// [`State::Invalid`] (the validator rejects them earlier; executing one is a
/// structured error).
pub(crate) fn init_shape(decl: &Decl, base: u32) -> Shape {
    // Not named `func`: see `cells_for`.
    let Some(ta_func) = decl.func() else {
        return Shape::Invalid;
    };
    let length = length_of(decl);
    match ta_func {
        TaFunc::Change | TaFunc::Sma => Shape::Invalid,
        TaFunc::MathSum
        | TaFunc::Wma
        | TaFunc::Alma
        | TaFunc::Stdev
        | TaFunc::Dev
        | TaFunc::Variance
        | TaFunc::Linreg
        | TaFunc::Cci
        | TaFunc::Cog
        | TaFunc::Median
        | TaFunc::MedianInt
        | TaFunc::Mode
        | TaFunc::ModeInt
        | TaFunc::PercentileLinearInterpolation
        | TaFunc::PercentileNearestRank
        | TaFunc::Range
        | TaFunc::RangeInt
        | TaFunc::Rci => Shape::Window {
            ring: Ring::new(base, length),
        },
        TaFunc::Highest | TaFunc::Lowest | TaFunc::HighestBars | TaFunc::LowestBars => {
            if length >= EXTREMUM_QUEUE_MIN_LENGTH {
                Shape::WindowExtremum {
                    window: ExtremumWindow::new(base, length),
                }
            } else {
                Shape::Window {
                    ring: Ring::new(base, length),
                }
            }
        }
        // mom/roc need the value `length` bars back plus the current one;
        // percentrank/rising/falling/cmo the same shape.
        TaFunc::Mom
        | TaFunc::Roc
        | TaFunc::Rising
        | TaFunc::Falling
        | TaFunc::Cmo
        | TaFunc::PercentRank => Shape::Window {
            ring: Ring::new(base, length.saturating_add(1)),
        },
        TaFunc::Swma => Shape::Window {
            ring: Ring::new(base, 4),
        },
        TaFunc::Bbw => Shape::Window {
            ring: Ring::new(base, length),
        },
        TaFunc::Vwma => Shape::WindowPair {
            ring: PairRing::new(base, length),
        },
        TaFunc::Hma => {
            let len = length.max(2);
            Shape::Hma {
                src: Ring::new(base, len),
                inter: Ring::new(base + len, hma_outer(len)),
            }
        }
        TaFunc::ValueWhen
        | TaFunc::ValueWhenInt
        | TaFunc::ValueWhenBool
        | TaFunc::ValueWhenColor => Shape::ValueWhen {
            vals: Ring::new(
                base,
                (decl.param(0).unwrap_or(0).clamp(0, MAX_LENGTH) as u32).saturating_add(1),
            ),
        },
        TaFunc::Correlation => Shape::Corr {
            ring: PairRing::new(base, length),
        },
        TaFunc::Stoch => Shape::Stoch {
            ring: PairRing::new(base, length),
        },
        TaFunc::Mfi => Shape::Mfi {
            ring: PairRing::new(base, length),
        },
        TaFunc::Wpr => Shape::Wpr {
            highs: Ring::new(base, length),
            lows: Ring::new(base + length, length),
        },
        TaFunc::PivotHigh | TaFunc::PivotLow => Shape::Pivot {
            ring: Ring::new(
                base,
                param_clamped(decl, 0, 1)
                    .saturating_add(param_clamped(decl, 1, 1))
                    .saturating_add(1),
            ),
        },
        TaFunc::Bb => Shape::Bb {
            ring: Ring::new(base, length),
        },
        _ => Shape::Scalar { ta_func },
    }
}

/// `0.0`. A named constant rather than a literal only because Aeneas cannot
/// translate float literals; the extraction keeps it opaque and models it by
/// its bit pattern.
const ZERO: f64 = 0.0;
/// `1.0` (see [`ZERO`]).
const ONE: f64 = 1.0;

/// The initial state of a function that owns no cells ([`Shape::Scalar`]).
/// A cell-owning or malformed function here is [`State::Invalid`]; no
/// [`Shape`] carries one.
fn scalar_state(ta_func: TaFunc) -> State {
    // Not named `func`: see `cells_for`.
    let zero = Smoother::default();
    match ta_func {
        TaFunc::Fixnan | TaFunc::FixnanInt => State::Fixnan { last: f64::NAN },
        TaFunc::Ema | TaFunc::Rma | TaFunc::Atr => State::Smooth { s: zero },
        TaFunc::Rsi => State::Rsi {
            prev: f64::NAN,
            up: zero,
            down: zero,
        },
        TaFunc::Tr | TaFunc::Wvad | TaFunc::Iii => State::Stateless,
        TaFunc::Cum => State::Cum { total: ZERO },
        TaFunc::Max | TaFunc::Min => State::Extremum { value: f64::NAN },
        TaFunc::Crossover | TaFunc::Crossunder | TaFunc::Cross => State::Cross {
            prev_a: f64::NAN,
            prev_b: f64::NAN,
            seen: false,
        },
        TaFunc::BarsSince => State::BarsSince { since: i64::MIN },
        TaFunc::Obv => State::Obv {
            total: ZERO,
            prev_close: f64::NAN,
        },
        TaFunc::AccDist => State::AccDist { total: ZERO },
        TaFunc::Nvi | TaFunc::Pvi => State::VolumeIndex {
            value: ONE,
            prev_close: f64::NAN,
            prev_volume: f64::NAN,
        },
        TaFunc::Pvt => State::Pvt {
            total: ZERO,
            prev_close: f64::NAN,
        },
        TaFunc::Wad => State::Wad {
            total: ZERO,
            prev_close: f64::NAN,
        },
        TaFunc::Vwap | TaFunc::VwapSource | TaFunc::VwapAnchored | TaFunc::VwapBands => {
            State::Vwap {
                pv: ZERO,
                p2v: ZERO,
                vol: ZERO,
                active: matches!(ta_func, TaFunc::Vwap | TaFunc::VwapSource),
            }
        }
        TaFunc::Sar => State::Sar { s: Sar::default() },
        TaFunc::Tsi => State::Tsi {
            prev: f64::NAN,
            pc_long: zero,
            pc_short: zero,
            abs_long: zero,
            abs_short: zero,
        },
        TaFunc::Kcw | TaFunc::Kc => State::Keltner {
            basis: zero,
            range_ema: zero,
        },
        TaFunc::Macd => State::Macd {
            fast: zero,
            slow: zero,
            signal: zero,
        },
        TaFunc::Dmi => State::Dmi {
            prev_high: f64::NAN,
            prev_low: f64::NAN,
            tr: zero,
            plus_dm: zero,
            minus_dm: zero,
            adx: zero,
        },
        TaFunc::Supertrend => State::Supertrend {
            atr: zero,
            bands_seeded: false,
            prev_upper: ZERO,
            prev_lower: ZERO,
            prev_supertrend: ZERO,
            prev_direction: ZERO,
        },
        TaFunc::TimeframeChange => State::TimeframeChange {
            prev_bucket: i64::MIN,
        },
        _ => State::Invalid,
    }
}

/// Materialize a [`Shape`]: move its rings into the matching [`State`].
fn state_of(shape: Shape) -> State {
    match shape {
        Shape::Invalid => State::Invalid,
        Shape::Window { ring } => State::Window { ring },
        Shape::WindowExtremum { window } => State::WindowExtremum { window },
        Shape::WindowPair { ring } => State::WindowPair { ring },
        Shape::Hma { src, inter } => State::Hma { src, inter },
        Shape::ValueWhen { vals } => State::ValueWhen { vals },
        Shape::Corr { ring } => State::Corr { ring },
        Shape::Stoch { ring } => State::Stoch { ring },
        Shape::Mfi { ring } => State::Mfi {
            prev: f64::NAN,
            ring,
        },
        Shape::Wpr { highs, lows } => State::Wpr { highs, lows },
        Shape::Pivot { ring } => State::Pivot { ring },
        Shape::Bb { ring } => State::Bb { ring },
        Shape::Scalar { ta_func } => scalar_state(ta_func),
    }
}

/// One step of a reset: the site's [`Shape`] at `base`, and the base of the
/// next site. `None` (never passed by [`reset_sites`], which stays in bounds)
/// takes no cells. An `Option` parameter so the reset loop carries no borrow
/// across iterations (Aeneas extraction).
#[inline]
pub(crate) fn site_shape(decl: Option<&Decl>, base: u32) -> (Shape, u32) {
    match decl {
        Some(decl) => (init_shape(decl, base), base.saturating_add(cells_for(decl))),
        None => (Shape::Invalid, base),
    }
}

/// Where [`reset_sites`] delivers each site's [`Shape`].
pub trait ShapeSink {
    /// Record site `index`'s initial shape.
    fn put(&mut self, index: usize, shape: Shape);
}

/// The engine's state region as a [`ShapeSink`]: each shape becomes that
/// site's initial [`State`]; a site past the end of the region is skipped.
impl ShapeSink for [State] {
    #[inline]
    fn put(&mut self, index: usize, shape: Shape) {
        if let Some(slot) = self.get_mut(index) {
            *slot = state_of(shape);
        }
    }
}

/// The site loop of `Engine::reset`: assign every declaration its base
/// (the saturating prefix sum of [`cells_for`]) and hand its [`Shape`] to
/// `sink`. Generic over the sink so this exact loop, free of `f64`, is what
/// Aeneas extracts; an indexed loop for the same reason.
pub(crate) fn reset_sites<S: ShapeSink + ?Sized>(decls: &[Decl], sink: &mut S) {
    let mut base = 0u32;
    let mut i = 0usize;
    while i < decls.len() {
        let (shape, next) = site_shape(decls.get(i), base);
        sink.put(i, shape);
        base = next;
        i += 1;
    }
}

/// The cell half of a reset: every cell reads na.
#[inline]
pub(crate) fn fill_absent(cells: &mut [f64]) {
    for cell in cells.iter_mut() {
        *cell = f64::NAN;
    }
}

/// `Engine::reset` over its regions: every site's initial state (a site past
/// the end of `states` is skipped, and still advances the bases), then every
/// cell na. A free function over the slices so the whole reset, not only its
/// site loop, is what Aeneas extracts.
#[inline]
pub(crate) fn reset_region(decls: &[Decl], states: &mut [State], cells: &mut [f64]) {
    reset_sites(decls, states);
    fill_absent(cells);
}

/// Extraction-only marker, kept opaque (`--opaque`). Aeneas translates `f64`
/// to a type that its support library does not define; an opaque type makes
/// it emit a declaration for `F64` instead, which the support code defines
/// (by the IEEE-754 bit pattern). Outside the roots module because an opaque
/// item is not a translated root.
#[cfg(feature = "aeneas")]
#[allow(dead_code)] // referenced only by the extraction root `ta_f64_model`
pub struct F64Model;

/// Extraction roots for the rest of the reset: the state
/// materialization (`state_of`, `scalar_state`), the state region's
/// `ShapeSink`, the NaN fill and `reset_region`, the body of `Engine::reset`.
/// Compiled only with `--features aeneas`; `state` is private, so the roots
/// are reachable only as Charon `--start-from` paths.
#[cfg(feature = "aeneas")]
#[allow(dead_code)]
pub mod aeneas_roots_ta_state {
    use super::{Shape, ShapeSink, State};
    use crate::{Decl, TaFunc};

    /// Root that makes [`super::F64Model`] part of the extraction.
    pub fn ta_f64_model(_model: &super::F64Model) {}

    /// Root for `state::scalar_state`.
    pub fn ta_scalar_state(ta_func: TaFunc) -> State {
        super::scalar_state(ta_func)
    }

    /// Root for `state::state_of`.
    pub fn ta_state_of(shape: Shape) -> State {
        super::state_of(shape)
    }

    /// Root for the state region's `ShapeSink::put`.
    pub fn ta_put(states: &mut [State], index: usize, shape: Shape) {
        states.put(index, shape);
    }

    /// Root for `state::fill_absent`.
    pub fn ta_fill_absent(cells: &mut [f64]) {
        super::fill_absent(cells);
    }

    /// Root for the generic `Smoother::step_generic`.
    pub fn ta_smoother_step<F: super::FloatOps>(
        s: &mut super::Smoother<F>,
        x: F,
        length: u32,
        alpha: F,
    ) -> F {
        s.step_generic(x, length, alpha)
    }

    /// Root for the generic `Smoother::advance_generic`.
    pub fn ta_smoother_advance<F: super::FloatOps>(
        s: &mut super::Smoother<F>,
        x: F,
        length: u32,
        alpha: F,
    ) -> F {
        s.advance_generic(x, length, alpha)
    }

    /// Root for `state::rma_alpha_generic`.
    pub fn ta_rma_alpha<F: super::FloatOps>(length: u32) -> F {
        super::rma_alpha_generic(length)
    }

    /// Root for `state::ema_alpha_generic`.
    pub fn ta_ema_alpha<F: super::FloatOps>(length: u32) -> F {
        super::ema_alpha_generic(length)
    }

    /// Root for the `f64` instance: the `ta.rma` arm at `f64`,
    /// `advance(x, length, rma_alpha(length))`.
    pub fn ta_rma_advance_f64(s: &mut super::Smoother<f64>, x: f64, length: u32) -> f64 {
        s.advance_generic(x, length, super::rma_alpha_generic(length))
    }

    /// Root for `PairRing::push`.
    pub fn ta_pair_push(ring: &mut crate::arena::PairRing, cells: &mut [f64], a: f64, b: f64) {
        ring.push(cells, a, b);
    }

    /// Root for `PairRing::get`.
    pub fn ta_pair_get(ring: crate::arena::PairRing, cells: &[f64], i: u32) -> (f64, f64) {
        ring.get(cells, i)
    }

    /// Root for `PairRing::ready`.
    pub fn ta_pair_ready(ring: crate::arena::PairRing, cells: &[f64]) -> bool {
        ring.ready(cells)
    }

    /// Root for `state::reset_region`, the body of `Engine::reset`.
    pub fn ta_reset_region(decls: &[Decl], states: &mut [State], cells: &mut [f64]) {
        super::reset_region(decls, states, cells);
    }
}

#[cfg(test)]
mod layout_footprints_tests {
    extern crate std;

    use super::{decl_footprint, plan_footprints, DeclFootprints, Layout, LayoutFootprints};
    use crate::{Decl, TaFunc, MAX_PARAMS};
    use core::cell::RefCell;
    use std::panic::{catch_unwind, panic_any, AssertUnwindSafe};
    use std::{vec, vec::Vec};

    // Plain indexed Layout::plan body, for comparison in the tests.
    fn reference_plan(decls: &[Decl]) -> Layout {
        let mut cells = 0u32;
        let mut scratch = 0u32;
        // An indexed loop rather than an iterator so the plan stays inside
        // the subset Aeneas extracts; `get` keeps it free of a panic path.
        let mut i = 0usize;
        while i < decls.len() {
            let (c, s) = decl_footprint(decls.get(i));
            cells = cells.saturating_add(c);
            scratch = scratch.max(s);
            i += 1;
        }
        Layout {
            states: decls.len() as u32,
            cells,
            scratch,
        }
    }

    struct Recording<'a> {
        rows: &'a [(u32, u32)],
        calls: RefCell<Vec<usize>>,
        fail_at: Option<usize>,
        by_call: bool,
    }

    impl<'a> Recording<'a> {
        fn new(rows: &'a [(u32, u32)]) -> Self {
            Self {
                rows,
                calls: RefCell::new(Vec::new()),
                fail_at: None,
                by_call: false,
            }
        }
    }

    impl LayoutFootprints for Recording<'_> {
        fn footprint(&self, index: usize) -> (u32, u32) {
            let call = self.calls.borrow().len();
            self.calls.borrow_mut().push(index);
            if self.fail_at == Some(index) {
                panic_any(index);
            }
            let row = if self.by_call { call } else { index };
            self.rows.get(row).copied().unwrap_or_default()
        }
    }

    #[test]
    fn concrete_all_tags_and_raw_params_match_frozen_plan() {
        let params = [
            [0; MAX_PARAMS],
            [1; MAX_PARAMS],
            [-1; MAX_PARAMS],
            [i64::MIN, i64::MAX, 2, 100_000],
            [99_999, 100_000, 100_001, 1_000_000],
            [i64::MAX; MAX_PARAMS],
            // Raw encodings of -0, a quiet NaN, infinity, and a NaN payload.
            [
                i64::MIN,
                0x7ff8_0000_0000_0000,
                0x7ff0_0000_0000_0000,
                0x7ff8_1234_5678_9abc,
            ],
        ];
        let mut all = Vec::new();
        for func in TaFunc::ALL {
            for values in params {
                for width in 0..=7u16 {
                    // Widths above MAX_PARAMS are malformed internal records,
                    // not values claimed to be returned by Decl::new.
                    let decl = Decl {
                        tag: func.tag(),
                        nparams: width,
                        _pad: 0xa5a5_5a5a,
                        params: values,
                    };
                    assert_eq!(Layout::plan(&[decl]), reference_plan(&[decl]));
                    if usize::from(width) <= MAX_PARAMS {
                        let valid = Decl::new(func, &values[..usize::from(width)]).unwrap();
                        assert_eq!(Layout::plan(&[valid]), reference_plan(&[valid]));
                    }
                    all.push(decl);
                }
            }
        }
        all.push(Decl {
            tag: u16::MAX,
            nparams: u16::MAX,
            _pad: u32::MAX,
            params: [i64::MAX; MAX_PARAMS],
        });
        assert_eq!(Layout::plan(&all), reference_plan(&all));
        all.reverse();
        assert_eq!(Layout::plan(&all), reference_plan(&all));
    }

    #[test]
    fn concrete_mixed_prefixes_and_saturation_match_frozen_plan() {
        let declarations = [
            Decl::new(TaFunc::Vwma, &[100_000]).unwrap(),
            Decl::new(TaFunc::Median, &[100_000]).unwrap(),
            Decl::new(TaFunc::Sma, &[i64::MAX]).unwrap(),
            Decl::new(TaFunc::Hma, &[i64::MIN]).unwrap(),
        ];
        let mut rows = Vec::new();
        for index in 0..25_000 {
            rows.push(declarations[index % declarations.len()]);
        }
        for count in [0, 1, 2, 3, 4, 5, 31, 128, rows.len()] {
            let before: Vec<_> = rows[..count].iter().map(Decl::to_le_bytes).collect();
            assert_eq!(Layout::plan(&rows[..count]), reference_plan(&rows[..count]));
            assert_eq!(
                rows[..count]
                    .iter()
                    .map(Decl::to_le_bytes)
                    .collect::<Vec<_>>(),
                before
            );
        }
        let saturated = vec![declarations[0]; 25_000];
        let layout = Layout::plan(&saturated);
        assert_eq!(layout, reference_plan(&saturated));
        assert_eq!(layout.cells, u32::MAX);
    }

    #[test]
    fn scalar_prefix_grid_matches_wide_sum_and_independent_maximum() {
        let values = [0, 1, 2, 99_999, 100_000, u32::MAX - 1, u32::MAX];
        for a in values {
            for b in values {
                for c in values {
                    let rows = [(a, b), (b, c), (c, a), (0, 0)];
                    for count in 0..=6 {
                        let table = Recording::new(&rows);
                        let selected = &rows[..count.min(rows.len())];
                        let wide_sum: u64 = selected.iter().map(|row| u64::from(row.0)).sum();
                        let expected = Layout {
                            states: count as u32,
                            cells: wide_sum.min(u64::from(u32::MAX)) as u32,
                            scratch: selected.iter().map(|row| row.1).max().unwrap_or(0),
                        };
                        assert_eq!(plan_footprints(&table, count), expected);
                        assert_eq!(*table.calls.borrow(), (0..count).collect::<Vec<_>>());
                    }
                }
            }
        }
    }

    #[test]
    fn stateful_reads_continue_once_in_order_after_saturation() {
        let rows = [(u32::MAX, 2), (1, 5), (0, 3), (7, 11)];
        let mut table = Recording::new(&rows);
        table.by_call = true;
        assert_eq!(
            plan_footprints(&table, rows.len()),
            Layout {
                states: 4,
                cells: u32::MAX,
                scratch: 11,
            }
        );
        assert_eq!(*table.calls.borrow(), [0, 1, 2, 3]);
    }

    #[test]
    fn callback_panics_preserve_exact_visited_prefix_including_max_count() {
        let rows = [(u32::MAX, 1), (1, 2), (0, 3), (0, 4)];
        for count in [4, usize::MAX] {
            for fail_at in 0..rows.len() {
                let mut table = Recording::new(&rows);
                table.fail_at = Some(fail_at);
                let panic = catch_unwind(AssertUnwindSafe(|| plan_footprints(&table, count)))
                    .expect_err("the selected callback must panic");
                assert_eq!(panic.downcast_ref::<usize>(), Some(&fail_at));
                assert_eq!(*table.calls.borrow(), (0..=fail_at).collect::<Vec<_>>());
            }
        }
        // MAX count above is stopped at a real callback panic. It does not
        // claim that this finite test completed a MAX-length scan.
    }

    #[test]
    fn borrowed_adapter_preserves_missing_rows_and_explicit_count() {
        let rows = [
            Decl::new(TaFunc::Median, &[17]).unwrap(),
            Decl::new(TaFunc::Vwma, &[9]).unwrap(),
        ];
        let table = DeclFootprints(&rows);
        assert_eq!(table.footprint(0), decl_footprint(rows.first()));
        assert_eq!(table.footprint(1), decl_footprint(rows.get(1)));
        assert_eq!(table.footprint(rows.len()), (0, 0));
        assert_eq!(table.footprint(usize::MAX), (0, 0));
        let expected = reference_plan(&rows);
        assert_eq!(plan_footprints(&table, 2), expected);
        assert_eq!(
            plan_footprints(&table, 5),
            Layout {
                states: 5,
                ..expected
            }
        );
        let mut unused = Recording::new(&[]);
        unused.fail_at = Some(0);
        assert_eq!(plan_footprints(&unused, 0), Layout::default());
        assert!(unused.calls.borrow().is_empty());
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::{
        cells_for, init_shape, length_of, param_clamped, scratch_for, site_shape, state_of, Layout,
        Sar, Shape, Smoother, State, EXTREMUM_QUEUE_MIN_LENGTH, MAX_LENGTH,
    };
    use crate::arena::{ExtremumWindow, PairRing, Ring};
    use crate::{Decl, TaFunc};
    use std::{format, string::String, vec, vec::Vec};

    /// Float form of [`super::hma_outer`], used as the expected value.
    fn reference_hma_outer(len: u32) -> u32 {
        let r = libm::round(libm::sqrt(f64::from(len))) as u32;
        if r == 0 {
            1
        } else {
            r
        }
    }

    #[test]
    fn integer_hma_outer_matches_the_float_reference() {
        // Every length a declaration can produce (`len` is clamped to
        // 2..=100_000) and well beyond it.
        for len in 0..=1u32 << 20 {
            assert_eq!(super::hma_outer(len), reference_hma_outer(len), "len={len}");
        }
        // Every rounding boundary in the whole u32 domain: around each
        // perfect square and each `r*r + r` / `r*r + r + 1` switch point.
        for r in 0u64..=65_536 {
            for n in [
                (r * r).saturating_sub(1),
                r * r,
                r * r + 1,
                r * r + r,
                r * r + r + 1,
            ] {
                if let Ok(len) = u32::try_from(n) {
                    assert_eq!(super::hma_outer(len), reference_hma_outer(len), "len={len}");
                }
            }
        }
        assert_eq!(super::hma_outer(u32::MAX), reference_hma_outer(u32::MAX));
    }

    /// Iterator form of [`Layout::plan`], used as the expected value.
    fn reference_plan(decls: &[Decl]) -> Layout {
        let mut cells = 0u32;
        let mut scratch = 0u32;
        for decl in decls {
            cells = cells.saturating_add(cells_for(decl));
            scratch = scratch.max(scratch_for(decl));
        }
        Layout {
            states: decls.len() as u32,
            cells,
            scratch,
        }
    }

    fn decls_with(params: &[i64]) -> Vec<Decl> {
        let mut out = Vec::new();
        for func in TaFunc::ALL {
            // Every arity up to the widest signature, so defaults apply too.
            for n in 0..=params.len().min(crate::MAX_PARAMS) {
                out.extend(Decl::new(func, &params[..n]));
            }
        }
        out
    }

    #[test]
    fn indexed_plan_matches_the_iterator_reference() {
        let param_sets: [&[i64]; 7] = [
            &[],
            &[1, 1, 1, 1],
            &[14, 3, 6, 0],
            &[0, -5, i64::MIN, i64::MAX],
            &[100_000, 100_000, 100_000, 100_000],
            &[i64::MAX, 2, 3, 4],
            &[99_999, 1, 7, 100_001],
        ];
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for params in param_sets {
            let decls = decls_with(params);
            for len in 0..decls.len().min(64) {
                assert_eq!(Layout::plan(&decls[..len]), reference_plan(&decls[..len]));
            }
            assert_eq!(Layout::plan(&decls), reference_plan(&decls));
            // Deterministic shuffled sub-sequences, including repeats.
            for _ in 0..64 {
                let mut picked = Vec::new();
                for _ in 0..(seed % 97) as usize {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    picked.push(decls[(seed % decls.len() as u64) as usize]);
                }
                let layout = Layout::plan(&picked);
                let reference = reference_plan(&picked);
                assert_eq!(layout, reference);
                assert_eq!(layout.total_cells(), reference.total_cells());
                assert_eq!(layout.region_bytes(), reference.region_bytes());
                assert_eq!(layout.offsets(), reference.offsets());
            }
        }
    }

    #[test]
    fn plan_saturates_like_the_reference() {
        // Enough maximal windows to saturate the u32 cell count.
        let big = Decl::new(TaFunc::Vwma, &[100_000]).unwrap();
        let decls = vec![big; 25_000];
        let layout = Layout::plan(&decls);
        assert_eq!(layout, reference_plan(&decls));
        assert_eq!(layout.cells, u32::MAX);
        assert_eq!(layout.region_bytes(), u32::MAX);
    }

    /// Direct per-site match used as the expected value: one match building
    /// the whole [`State`], rings and float fields together.
    #[allow(clippy::too_many_lines)]
    fn reference_init(decl: &Decl, base: u32) -> State {
        let Some(func) = decl.func() else {
            return State::Invalid;
        };
        let length = length_of(decl);
        let zero = Smoother::default();
        match func {
            TaFunc::Change | TaFunc::Sma => State::Invalid,
            TaFunc::Fixnan | TaFunc::FixnanInt => State::Fixnan { last: f64::NAN },
            TaFunc::MathSum
            | TaFunc::Wma
            | TaFunc::Alma
            | TaFunc::Stdev
            | TaFunc::Dev
            | TaFunc::Variance
            | TaFunc::Linreg
            | TaFunc::Cci
            | TaFunc::Cog
            | TaFunc::Median
            | TaFunc::MedianInt
            | TaFunc::Mode
            | TaFunc::ModeInt
            | TaFunc::PercentileLinearInterpolation
            | TaFunc::PercentileNearestRank
            | TaFunc::Range
            | TaFunc::RangeInt
            | TaFunc::Rci => State::Window {
                ring: Ring::new(base, length),
            },
            TaFunc::Highest | TaFunc::Lowest | TaFunc::HighestBars | TaFunc::LowestBars => {
                if length >= EXTREMUM_QUEUE_MIN_LENGTH {
                    State::WindowExtremum {
                        window: ExtremumWindow::new(base, length),
                    }
                } else {
                    State::Window {
                        ring: Ring::new(base, length),
                    }
                }
            }
            TaFunc::Mom
            | TaFunc::Roc
            | TaFunc::Rising
            | TaFunc::Falling
            | TaFunc::Cmo
            | TaFunc::PercentRank => State::Window {
                ring: Ring::new(base, length.saturating_add(1)),
            },
            TaFunc::Swma => State::Window {
                ring: Ring::new(base, 4),
            },
            TaFunc::Bbw => State::Window {
                ring: Ring::new(base, length),
            },
            TaFunc::Vwma => State::WindowPair {
                ring: PairRing::new(base, length),
            },
            TaFunc::Hma => {
                let len = length.max(2);
                State::Hma {
                    src: Ring::new(base, len),
                    inter: Ring::new(base + len, super::hma_outer(len)),
                }
            }
            TaFunc::Ema | TaFunc::Rma | TaFunc::Atr => State::Smooth { s: zero },
            TaFunc::Rsi => State::Rsi {
                prev: f64::NAN,
                up: zero,
                down: zero,
            },
            TaFunc::Tr | TaFunc::Wvad | TaFunc::Iii => State::Stateless,
            TaFunc::Cum => State::Cum { total: 0.0 },
            TaFunc::Max | TaFunc::Min => State::Extremum { value: f64::NAN },
            TaFunc::Crossover | TaFunc::Crossunder | TaFunc::Cross => State::Cross {
                prev_a: f64::NAN,
                prev_b: f64::NAN,
                seen: false,
            },
            TaFunc::BarsSince => State::BarsSince { since: i64::MIN },
            TaFunc::ValueWhen
            | TaFunc::ValueWhenInt
            | TaFunc::ValueWhenBool
            | TaFunc::ValueWhenColor => State::ValueWhen {
                vals: Ring::new(
                    base,
                    (decl.param(0).unwrap_or(0).clamp(0, MAX_LENGTH) as u32).saturating_add(1),
                ),
            },
            TaFunc::Correlation => State::Corr {
                ring: PairRing::new(base, length),
            },
            TaFunc::Stoch => State::Stoch {
                ring: PairRing::new(base, length),
            },
            TaFunc::Mfi => State::Mfi {
                prev: f64::NAN,
                ring: PairRing::new(base, length),
            },
            TaFunc::Wpr => State::Wpr {
                highs: Ring::new(base, length),
                lows: Ring::new(base + length, length),
            },
            TaFunc::Obv => State::Obv {
                total: 0.0,
                prev_close: f64::NAN,
            },
            TaFunc::AccDist => State::AccDist { total: 0.0 },
            TaFunc::Nvi | TaFunc::Pvi => State::VolumeIndex {
                value: 1.0,
                prev_close: f64::NAN,
                prev_volume: f64::NAN,
            },
            TaFunc::Pvt => State::Pvt {
                total: 0.0,
                prev_close: f64::NAN,
            },
            TaFunc::Wad => State::Wad {
                total: 0.0,
                prev_close: f64::NAN,
            },
            TaFunc::Vwap | TaFunc::VwapSource | TaFunc::VwapAnchored | TaFunc::VwapBands => {
                State::Vwap {
                    pv: 0.0,
                    p2v: 0.0,
                    vol: 0.0,
                    active: matches!(func, TaFunc::Vwap | TaFunc::VwapSource),
                }
            }
            TaFunc::Sar => State::Sar { s: Sar::default() },
            TaFunc::Tsi => State::Tsi {
                prev: f64::NAN,
                pc_long: zero,
                pc_short: zero,
                abs_long: zero,
                abs_short: zero,
            },
            TaFunc::Kcw | TaFunc::Kc => State::Keltner {
                basis: zero,
                range_ema: zero,
            },
            TaFunc::PivotHigh | TaFunc::PivotLow => State::Pivot {
                ring: Ring::new(
                    base,
                    param_clamped(decl, 0, 1)
                        .saturating_add(param_clamped(decl, 1, 1))
                        .saturating_add(1),
                ),
            },
            TaFunc::Macd => State::Macd {
                fast: zero,
                slow: zero,
                signal: zero,
            },
            TaFunc::Bb => State::Bb {
                ring: Ring::new(base, length),
            },
            TaFunc::Dmi => State::Dmi {
                prev_high: f64::NAN,
                prev_low: f64::NAN,
                tr: zero,
                plus_dm: zero,
                minus_dm: zero,
                adx: zero,
            },
            TaFunc::Supertrend => State::Supertrend {
                atr: zero,
                bands_seeded: false,
                prev_upper: 0.0,
                prev_lower: 0.0,
                prev_supertrend: 0.0,
                prev_direction: 0.0,
            },
            TaFunc::TimeframeChange => State::TimeframeChange {
                prev_bucket: i64::MIN,
            },
        }
    }

    /// Direct loop form of `Engine::reset` over the three regions, used as the
    /// expected value.
    fn reference_reset(decls: &[Decl], states: &mut [State], cells: &mut [f64]) {
        let mut base = 0u32;
        for (i, decl) in decls.iter().enumerate() {
            if let Some(slot) = states.get_mut(i) {
                *slot = reference_init(decl, base);
            }
            base = base.saturating_add(cells_for(decl));
        }
        for cell in cells.iter_mut() {
            *cell = f64::NAN;
        }
    }

    /// States hold NaN fields, so `PartialEq` cannot compare them; `Debug`
    /// prints every field (NaN as `NaN`), which is exact for these values.
    fn show(states: &[State]) -> Vec<String> {
        states.iter().map(|s| format!("{s:?}")).collect()
    }

    const PARAM_SETS: [&[i64]; 8] = [
        &[],
        &[1, 1, 1, 1],
        &[14, 3, 6, 0],
        &[31, 32, 33, 2],
        &[0, -5, i64::MIN, i64::MAX],
        &[100_000, 100_000, 100_000, 100_000],
        &[i64::MAX, 2, 3, 4],
        &[99_999, 1, 7, 100_001],
    ];

    #[test]
    fn shape_then_state_matches_the_reference_init() {
        // `Hma`/`Wpr` add `base + len` unchecked, so stay below
        // the largest footprint's overflow point.
        let bases = [0, 1, 7, 12_345, 1 << 31, u32::MAX - 300_001];
        for params in PARAM_SETS {
            for decl in decls_with(params) {
                for base in bases {
                    assert_eq!(
                        format!("{:?}", state_of(init_shape(&decl, base))),
                        format!("{:?}", reference_init(&decl, base)),
                        "{decl:?} at {base}"
                    );
                    let (shape, next) = site_shape(Some(&decl), base);
                    assert_eq!(shape, init_shape(&decl, base));
                    assert_eq!(next, base.saturating_add(cells_for(&decl)));
                }
            }
        }
        // A tag no variant claims is invalid on both paths.
        let bad = Decl {
            tag: u16::MAX,
            ..Decl::new(TaFunc::Sma, &[3]).unwrap()
        };
        assert_eq!(bad.func(), None);
        assert_eq!(init_shape(&bad, 5), Shape::Invalid);
        assert_eq!(site_shape(Some(&bad), 5), (Shape::Invalid, 5));
        assert_eq!(site_shape(None, 9), (Shape::Invalid, 9));
        assert!(matches!(reference_init(&bad, 5), State::Invalid));
    }

    #[test]
    fn reset_matches_the_reference_reset() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let check = |decls: &[Decl]| {
            // Short, exact, and long state/cell regions: a short region
            // skips the tail sites and still advances the bases.
            for extra in [-2i64, 0, 3] {
                let n = (decls.len() as i64 + extra).max(0) as usize;
                let cells_n = (Layout::plan(decls).cells as usize).min(1 << 16) + 5;
                let mut states = vec![State::Stateless; n];
                let mut cells = vec![0.5f64; cells_n];
                let mut ref_states = states.clone();
                let mut ref_cells = cells.clone();
                // Check the extracted reset kernel.
                // Engine::reset also builds independent VWMA windows
                // (separately exercised).
                super::reset_region(decls, &mut states, &mut cells);
                reference_reset(decls, &mut ref_states, &mut ref_cells);
                assert_eq!(show(&states), show(&ref_states));
                let bits = |c: &[f64]| c.iter().map(|v| v.to_bits()).collect::<Vec<_>>();
                assert_eq!(bits(&cells), bits(&ref_cells));
            }
        };
        for params in PARAM_SETS {
            let decls = decls_with(params);
            for len in 0..decls.len().min(48) {
                check(&decls[..len]);
            }
            check(&decls);
            for _ in 0..32 {
                let mut picked = Vec::new();
                for _ in 0..(seed % 61) as usize {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    picked.push(decls[(seed % decls.len() as u64) as usize]);
                }
                check(&picked);
            }
        }
        // Saturating bases: past `u32::MAX` cells every later site is based
        // at `u32::MAX` on both paths (only cell-free sites, so no ring's
        // `base + len` overflows).
        let big = Decl::new(TaFunc::Vwma, &[100_000]).unwrap();
        let mut decls = vec![big; 21_475];
        decls.push(Decl::new(TaFunc::Ema, &[3]).unwrap());
        decls.push(Decl::new(TaFunc::Cum, &[]).unwrap());
        check(&decls);
    }
    /// Plain non-generic `Smoother`, used as the expected value.
    #[derive(Debug, Clone, Copy, Default, PartialEq)]
    struct RefSmoother {
        sum: f64,
        value: f64,
        count: u32,
        seeded: bool,
    }

    impl RefSmoother {
        fn step(&mut self, x: f64, length: u32, alpha: f64) -> f64 {
            if self.seeded {
                self.value = alpha * x + (1.0 - alpha) * self.value;
            } else {
                self.sum += x;
                self.count += 1;
                if self.count >= length {
                    self.value = self.sum / f64::from(length);
                    self.seeded = true;
                }
            }
            if self.seeded {
                self.value
            } else {
                f64::NAN
            }
        }
    }

    /// Expected value of the engine arm: `if present(x) { norm(s.step(..)) } else { NAN }`.
    fn ref_advance(s: &mut RefSmoother, x: f64, length: u32, alpha: f64) -> f64 {
        if !x.is_nan() {
            let v = s.step(x, length, alpha);
            if v.is_finite() {
                v
            } else {
                f64::NAN
            }
        } else {
            f64::NAN
        }
    }

    /// `fmax_det`/`fmin_det` against the `f64::max`/`f64::min` they
    /// replace, over the full square of special bit patterns and over
    /// random pairs.
    ///
    /// * Neither operand NaN and not a `+0.0`/`-0.0` pair: bit-identical to
    ///   `f64::max`/`f64::min`.
    /// * Exactly one operand NaN: the other operand, bit for bit. That is
    ///   what `f64::max`/`f64::min` document; a target may instead return a
    ///   NaN when the NaN is signaling, so the `f64::max` result is only
    ///   required to be that operand or a NaN.
    /// * Both NaN: the right operand (the `f64::max` result is some NaN).
    /// * `+0.0` against `-0.0`, either order: `max` is `+0.0`, `min` is
    ///   `-0.0` (the `f64::max` result is a zero of either sign).
    ///
    /// Also: the helpers equal their comparison-only definitions
    /// (`fmax_full`/`fmin_full`), so the `f64::max` shortcut changes
    /// nothing; the result is always one of the two operand patterns, both are
    /// commutative on non-NaN operands and associative on non-NaN triples
    /// (so a window fold does not depend on the cell order), and
    /// `min(a, b) == -max(-a, -b)`.
    #[test]
    fn fmax_fmin_det_match_f64_except_on_signed_zero_ties() {
        use super::{fmax_det, fmax_full, fmin_det, fmin_full};
        const SIGN: u64 = 1 << 63;
        let mut specials: Vec<u64> = vec![
            0,
            SIGN,
            1,
            SIGN | 1,
            f64::MIN_POSITIVE.to_bits(),
            (-f64::MIN_POSITIVE).to_bits(),
            1.0f64.to_bits(),
            (-1.0f64).to_bits(),
            2.5f64.to_bits(),
            (-2.5f64).to_bits(),
            f64::MAX.to_bits(),
            f64::MIN.to_bits(),
            f64::INFINITY.to_bits(),
            f64::NEG_INFINITY.to_bits(),
            f64::NAN.to_bits(),
            (-f64::NAN).to_bits(),
            // Quiet NaNs with payloads, then signaling NaNs of both signs.
            0x7ff8_0000_0000_0abc,
            0xfff8_0000_0000_1234,
            0x7ff0_0000_0000_0001,
            0xfff0_0000_0000_0001,
            0x7ff4_0000_0000_0000,
        ];
        // Neighbours of each special, one ulp either side.
        for p in specials.clone() {
            specials.push(p.wrapping_add(1));
            specials.push(p.wrapping_sub(1));
        }
        let mut pairs: Vec<(u64, u64)> = Vec::new();
        for &a in &specials {
            for &b in &specials {
                pairs.push((a, b));
            }
        }
        let mut state = 0x243f_6a88_85a3_08d3u64;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            state
        };
        for i in 0..200_000usize {
            let a = next();
            let b = match i % 4 {
                0 => next(),
                // Equal magnitudes, same and opposite signs.
                1 => a,
                2 => a ^ SIGN,
                _ => specials[(next() % specials.len() as u64) as usize],
            };
            pairs.push((a, b));
        }
        let (mut mixed, mut one_nan, mut both_nan, mut plain, mut equal) = (0, 0, 0, 0, 0);
        for &(a, b) in &pairs {
            let (x, y) = (f64::from_bits(a), f64::from_bits(b));
            let (mx, mn) = (fmax_det(x, y).to_bits(), fmin_det(x, y).to_bits());
            let (old_max, old_min) = (x.max(y), x.min(y));
            let ctx = std::format!("{a:#x} {b:#x}");
            assert!(mx == a || mx == b, "{ctx}");
            assert!(mn == a || mn == b, "{ctx}");
            // The shortcut changes nothing: the helpers are their
            // comparison-only definitions.
            assert_eq!(mx, fmax_full(x, y).to_bits(), "{ctx}");
            assert_eq!(mn, fmin_full(x, y).to_bits(), "{ctx}");
            if x.is_nan() && y.is_nan() {
                both_nan += 1;
                assert_eq!((mx, mn), (b, b), "{ctx}");
                assert!(old_max.is_nan() && old_min.is_nan(), "{ctx}");
            } else if x.is_nan() || y.is_nan() {
                one_nan += 1;
                let other = if x.is_nan() { b } else { a };
                assert_eq!((mx, mn), (other, other), "{ctx}");
                assert!(old_max.to_bits() == other || old_max.is_nan(), "{ctx}");
                assert!(old_min.to_bits() == other || old_min.is_nan(), "{ctx}");
                // A quiet NaN operand: `f64::max`/`f64::min` return the
                // other operand on every target.
                let nan = if x.is_nan() { a } else { b };
                if nan & (1 << 51) != 0 {
                    assert_eq!((old_max.to_bits(), old_min.to_bits()), (other, other));
                }
            } else if a | b == SIGN && a != b {
                mixed += 1;
                assert_eq!((mx, mn), (0, SIGN), "{ctx}");
                assert!(old_max == 0.0 && old_min == 0.0, "{ctx}");
            } else {
                plain += 1;
                equal += usize::from(x == y);
                assert_eq!((mx, mn), (old_max.to_bits(), old_min.to_bits()), "{ctx}");
            }
            if !x.is_nan() && !y.is_nan() {
                assert_eq!(mx, fmax_det(y, x).to_bits(), "{ctx}");
                assert_eq!(mn, fmin_det(y, x).to_bits(), "{ctx}");
                // One is the max and the other the min.
                assert!((mx, mn) == (a, b) || (mx, mn) == (b, a), "{ctx}");
            }
            assert_eq!(mn, (-fmax_det(-x, -y)).to_bits(), "{ctx}");
        }
        assert!(mixed >= 2 && one_nan > 1000 && both_nan > 100 && plain > 100_000);
        // The shortcut is sound whatever the target's `f64::max`/`f64::min`
        // do on the tie: under each modelled tie-break the helpers are
        // still their comparison-only definitions.
        {
            use super::tie_policy::{set_platform, Policy};
            for policy in [Policy::LeftWins, Policy::RightWins, Policy::Scrambled] {
                let before = set_platform(policy);
                for &(a, b) in &pairs {
                    let (x, y) = (f64::from_bits(a), f64::from_bits(b));
                    assert_eq!(
                        fmax_det(x, y).to_bits(),
                        fmax_full(x, y).to_bits(),
                        "{policy:?} {a:#x} {b:#x}"
                    );
                    assert_eq!(
                        fmin_det(x, y).to_bits(),
                        fmin_full(x, y).to_bits(),
                        "{policy:?} {a:#x} {b:#x}"
                    );
                }
                set_platform(before);
            }
        }
        assert!(equal > 10_000, "{equal}");
        // Associativity on non-NaN triples of specials: a fold's result is
        // independent of the order of the cells.
        let finite: Vec<f64> = specials
            .iter()
            .map(|&p| f64::from_bits(p))
            .filter(|v| !v.is_nan())
            .collect();
        for &x in &finite {
            for &y in &finite {
                for &z in &finite {
                    assert_eq!(
                        fmax_det(fmax_det(x, y), z).to_bits(),
                        fmax_det(x, fmax_det(y, z)).to_bits(),
                        "{x:?} {y:?} {z:?}"
                    );
                    assert_eq!(
                        fmin_det(fmin_det(x, y), z).to_bits(),
                        fmin_det(x, fmin_det(y, z)).to_bits(),
                        "{x:?} {y:?} {z:?}"
                    );
                }
            }
        }
    }

    /// Differential check: the generic `Smoother<f64>` (`step`, `advance`,
    /// `rma_alpha`, `ema_alpha`) is bit-identical to the direct `f64` code on
    /// every length class and on random, na, ±inf, huge and subnormal inputs,
    /// for both alphas and for arbitrary alphas. The bit-level models of the
    /// comparison/sign/clamp entries (`key`, `ge`, `le`, `neg`, `eqZero`,
    /// `maxMin`, `minMax`) agree with the `f64` operations of
    /// `impl FloatOps for f64` on special and random bit patterns.
    #[test]
    fn float_ops_bit_models_match_f64() {
        use super::FloatOps;
        const SIGN: u64 = 1 << 63;
        fn is_nan(b: u64) -> bool {
            (b >> 52) & 0x7ff == 0x7ff && b & ((1 << 52) - 1) != 0
        }
        fn key(b: u64) -> i128 {
            if b < SIGN {
                i128::from(b)
            } else {
                -i128::from(b - SIGN)
            }
        }
        fn ge(a: u64, b: u64) -> bool {
            !is_nan(a) && !is_nan(b) && key(b) <= key(a)
        }
        fn le(a: u64, b: u64) -> bool {
            !is_nan(a) && !is_nan(b) && key(a) <= key(b)
        }
        fn neg(a: u64) -> u64 {
            if a < SIGN {
                a + SIGN
            } else {
                a - SIGN
            }
        }
        fn eq_zero(a: u64) -> bool {
            a % SIGN == 0
        }
        let min_bits = f64::MIN.to_bits();
        let max_bits = f64::MAX.to_bits();
        assert_eq!(min_bits, 0xffef_ffff_ffff_ffff);
        assert_eq!(max_bits, 0x7fef_ffff_ffff_ffff);
        fn max_min(a: u64) -> u64 {
            let m = f64::MIN.to_bits();
            if is_nan(a) || key(a) < key(m) {
                m
            } else {
                a
            }
        }
        fn min_max(a: u64) -> u64 {
            let m = f64::MAX.to_bits();
            if is_nan(a) || key(m) < key(a) {
                m
            } else {
                a
            }
        }
        fn abs(a: u64) -> u64 {
            a % SIGN
        }
        fn gt(a: u64, b: u64) -> bool {
            !is_nan(a) && !is_nan(b) && key(b) < key(a)
        }
        fn lt(a: u64, b: u64) -> bool {
            !is_nan(a) && !is_nan(b) && key(a) < key(b)
        }
        /// Bit model of `min`: NaN yields the other side; otherwise the
        /// smaller `key`; on a `key` tie `a` when `a` is `-0.0`, else `b`
        /// (so `-0.0` wins the signed-zero tie, IEEE `minimumNumber`).
        fn min(a: u64, b: u64) -> u64 {
            if is_nan(a) {
                b
            } else if is_nan(b) || key(a) < key(b) {
                a
            } else if key(b) < key(a) || a != SIGN {
                b
            } else {
                a
            }
        }
        /// Bit model of `max`: NaN yields the other side; otherwise the
        /// larger `key`; on a `key` tie (equal patterns, or `±0.0`) `a` when
        /// `a` is `+0.0`, else `b` (so `+0.0` wins the signed-zero tie).
        fn max(a: u64, b: u64) -> u64 {
            if is_nan(a) {
                b
            } else if is_nan(b) || key(b) < key(a) {
                a
            } else if key(a) < key(b) || a != 0 {
                b
            } else {
                a
            }
        }
        /// `x as usize` on the bit pattern: NaN and every negative pattern
        /// give 0; otherwise the magnitude truncated toward zero, saturating
        /// at `usize::MAX` (infinity included).
        fn to_usize(a: u64) -> usize {
            let exp = (a >> 52) & 0x7ff;
            if is_nan(a) || a >= SIGN {
                return 0;
            }
            if exp == 0x7ff {
                return usize::MAX;
            }
            let frac = a & ((1 << 52) - 1);
            let mant = if exp == 0 { frac } else { frac | (1 << 52) };
            // value = mant * 2^(exp - 1075), with exp = 0 read as 1.
            let shift = exp.max(1) as i64 - 1075;
            let v: u128 = if shift >= 0 {
                if shift >= 64 {
                    u128::MAX
                } else {
                    u128::from(mant) << shift
                }
            } else if -shift >= 64 {
                0
            } else {
                u128::from(mant >> (-shift))
            };
            usize::try_from(v).unwrap_or(usize::MAX)
        }
        for (v, want) in [
            (0.0f64, 0usize),
            (-0.0, 0),
            (0.999_999, 0),
            (1.0, 1),
            (1.5, 1),
            (-1.5, 0),
            (4_503_599_627_370_497.0, 4_503_599_627_370_497),
            (1e300, usize::MAX),
            (f64::INFINITY, usize::MAX),
            (f64::NEG_INFINITY, 0),
            (f64::NAN, 0),
        ] {
            assert_eq!(FloatOps::to_usize(v), want, "{v}");
            assert_eq!(to_usize(v.to_bits()), want, "{v}");
        }
        assert_eq!(FloatOps::ceil(-0.5f64).to_bits(), (-0.0f64).to_bits());
        assert_eq!(FloatOps::ceil(0.25f64), 1.0);
        let mut pats: Vec<u64> = vec![
            0,
            SIGN,
            1,
            SIGN | 1,
            f64::INFINITY.to_bits(),
            f64::NEG_INFINITY.to_bits(),
            f64::NAN.to_bits(),
            (-f64::NAN).to_bits(),
            0x7ff0_0000_0000_0001,
            min_bits,
            max_bits,
            f64::MIN_POSITIVE.to_bits(),
            1.0f64.to_bits(),
            (-1.0f64).to_bits(),
        ];
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..200_000 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            pats.push(state);
            // Also nearby exponents/fractions of the specials.
            pats.push(state & 0xfff0_0000_0000_000f);
        }
        let n = pats.len();
        for (i, &a) in pats.iter().enumerate() {
            let b = pats[(i * 7 + 3) % n];
            let (x, y) = (f64::from_bits(a), f64::from_bits(b));
            assert_eq!(is_nan(a), FloatOps::is_nan(x), "{a:#x}");
            assert_eq!(ge(a, b), FloatOps::ge(x, y), "{a:#x} {b:#x}");
            assert_eq!(le(a, b), FloatOps::le(x, y), "{a:#x} {b:#x}");
            assert_eq!(neg(a), FloatOps::neg(x).to_bits(), "{a:#x}");
            assert_eq!(eq_zero(a), FloatOps::eq_zero(x), "{a:#x}");
            // `sqrt` is not an exact bit model: it is the correctly rounded
            // IEEE-754 square root, here checked against
            // `std`'s, which is also correctly rounded.
            let root = FloatOps::sqrt(x);
            let std_root = x.sqrt();
            assert!(
                root.to_bits() == std_root.to_bits() || (root.is_nan() && std_root.is_nan()),
                "{a:#x}"
            );
            // `floor` is exact; `std`'s is the same IEEE-754 roundToIntegral.
            let fl = FloatOps::floor(x);
            assert!(
                fl.to_bits() == x.floor().to_bits() || (fl.is_nan() && x.is_nan()),
                "{a:#x}"
            );
            // `ceil` is exact; `std`'s is the same IEEE-754 roundToIntegral.
            let ce = FloatOps::ceil(x);
            assert!(
                ce.to_bits() == x.ceil().to_bits() || (ce.is_nan() && x.is_nan()),
                "{a:#x}"
            );
            // `to_usize` (`as usize`) against the bit-level truncating,
            // saturating conversion.
            assert_eq!(to_usize(a), FloatOps::to_usize(x), "{a:#x}");
            // `total_gt` against the totalOrder key on the bit pattern
            // (flip the magnitude bits of a negative pattern, compare signed).
            let total_key = |p: u64| (p ^ ((((p as i64) >> 63) as u64) >> 1)) as i64;
            assert_eq!(
                total_key(a) > total_key(b),
                FloatOps::total_gt(x, y),
                "{a:#x} {b:#x}"
            );
            // `total_lt` against the same totalOrder key.
            assert_eq!(
                total_key(a) < total_key(b),
                FloatOps::total_lt(x, y),
                "{a:#x} {b:#x}"
            );
            // `fne` (`a != b` in the shipped operation table) is `!feq` in the
            // generic one.
            assert_eq!(x != y, !FloatOps::feq(x, y), "{a:#x} {b:#x}");
            // `from_usize` is the IEEE-754 integer conversion (round to
            // nearest even), the same as `u64`'s on a 64-bit target.
            assert_eq!(
                <f64 as FloatOps>::from_usize(a as usize).to_bits(),
                (a as f64).to_bits(),
                "{a:#x}"
            );
            // `midpoint` against `core`'s algorithm, restated.
            let mid = FloatOps::midpoint(x, y);
            #[allow(clippy::manual_midpoint)]
            let mid_model = {
                let lo = f64::MIN_POSITIVE * 2.0;
                let hi = f64::MAX / 2.0;
                if x.abs() <= hi && y.abs() <= hi {
                    (x + y) / 2.0
                } else if x.abs() < lo {
                    x + y / 2.0
                } else if y.abs() < lo {
                    x / 2.0 + y
                } else {
                    x / 2.0 + y / 2.0
                }
            };
            assert!(
                mid.to_bits() == mid_model.to_bits() || (mid.is_nan() && mid_model.is_nan()),
                "{a:#x} {b:#x}"
            );
            // `exp` matches `pinewasm_num::raw::exp`.
            let e = FloatOps::exp(x);
            let e_ref = pinewasm_num::raw::exp(x);
            assert!(
                e.to_bits() == e_ref.to_bits() || (e.is_nan() && e_ref.is_nan()),
                "{a:#x}"
            );
            let max_actual = FloatOps::max_min(x);
            let min_actual = FloatOps::min_max(x);
            if is_nan(a) {
                // Rust permits the platform to propagate a signaling NaN
                // through min/max instead of choosing the finite operand.
                assert!(
                    max_actual.to_bits() == max_min(a) || max_actual.is_nan(),
                    "{a:#x}"
                );
                assert!(
                    min_actual.to_bits() == min_max(a) || min_actual.is_nan(),
                    "{a:#x}"
                );
            } else {
                assert_eq!(max_min(a), max_actual.to_bits(), "{a:#x}");
                assert_eq!(min_max(a), min_actual.to_bits(), "{a:#x}");
            }
            assert_eq!(abs(a), FloatOps::abs(x).to_bits(), "{a:#x}");
            // `max` is `fmax_det`: exact on every pair, NaN operands and
            // the signed-zero tie included (no slack).
            let m = FloatOps::max(x, y);
            assert_eq!(max(a, b), m.to_bits(), "{a:#x} {b:#x}");
            assert_eq!(gt(a, b), FloatOps::gt(x, y), "{a:#x} {b:#x}");
            let feq = !is_nan(a) && !is_nan(b) && key(a) == key(b);
            assert_eq!(feq, FloatOps::feq(x, y), "{a:#x} {b:#x}");
            assert_eq!(lt(a, b), FloatOps::lt(x, y), "{a:#x} {b:#x}");
            // `min` is `fmin_det`: exact on every pair, as for `max`.
            let mn = FloatOps::min(x, y);
            assert_eq!(min(a, b), mn.to_bits(), "{a:#x} {b:#x}");
            // `max(x, 0.0)` and `max(-x, 0.0)` (the rsi gain/loss inputs),
            // and the two signed-zero ties in both orders.
            assert_eq!(FloatOps::max(x, 0.0).to_bits(), max(a, 0), "{a:#x}");
            assert_eq!(FloatOps::max(-x, 0.0).to_bits(), max(neg(a), 0), "{a:#x}");
        }
        for (a, b) in [(0, SIGN), (SIGN, 0), (0, 0), (SIGN, SIGN)] {
            let (x, y) = (f64::from_bits(a), f64::from_bits(b));
            assert_eq!(FloatOps::max(x, y).to_bits(), max(a, b), "{a:#x} {b:#x}");
            assert_eq!(FloatOps::min(x, y).to_bits(), min(a, b), "{a:#x} {b:#x}");
            assert_eq!(max(a, b), a & b, "{a:#x} {b:#x}");
            assert_eq!(min(a, b), a | b, "{a:#x} {b:#x}");
        }
        assert_eq!(<f64 as FloatOps>::zero().to_bits(), 0);
        assert_eq!(
            <f64 as FloatOps>::neg_one().to_bits(),
            0xbff0_0000_0000_0000
        );
        assert_eq!(
            <f64 as FloatOps>::hundred().to_bits(),
            0x4059_0000_0000_0000
        );
        assert_eq!(<f64 as FloatOps>::three().to_bits(), 0x4008_0000_0000_0000);
        // Known correctly rounded roots, independent of any `sqrt` in scope.
        for (x, root) in [
            (2.0f64, 0x3ff6_a09e_667f_3bcd_u64),
            (4.0, 0x4000_0000_0000_0000),
            (0.0, 0),
            (-0.0, 0x8000_0000_0000_0000),
            (f64::INFINITY, 0x7ff0_0000_0000_0000),
        ] {
            assert_eq!(FloatOps::sqrt(x).to_bits(), root, "{x}");
        }
        assert!(FloatOps::sqrt(-1.0f64).is_nan());
        assert!(FloatOps::sqrt(f64::NEG_INFINITY).is_nan());
        assert_eq!(<f64 as FloatOps>::six().to_bits(), 0x4018_0000_0000_0000);
        // `lambert` is the correctly rounded binary64 of 0.015 = 3/200:
        // with biased exponent 0x3f8 the value is `m / 2^59`, and `m` is the
        // nearest integer to `3·2^59/200` (|200·m − 3·2^59| < 100).
        let lambert = <f64 as FloatOps>::lambert().to_bits();
        assert_eq!(lambert, 0x3f8e_b851_eb85_1eb8);
        assert_eq!(lambert >> 52, 0x3f8);
        let m = u128::from((lambert & ((1 << 52) - 1)) | (1 << 52));
        assert!((200 * m).abs_diff(3 << 59) < 100);
        // `paren` is grouping only (`(a)` in the shipped operation table, `a` in
        // the generic one): the identity on every bit pattern, NaN payloads
        // included.
        {
            type F = f64;
            for v in [
                0.0f64,
                -0.0,
                1.5,
                5e-324,
                f64::MAX,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NAN,
                -f64::NAN,
                f64::from_bits(0x7ff0_0000_0000_0001),
            ] {
                let generic: F = generic_op!(paren v);
                assert_eq!(f64_op!(paren v).to_bits(), v.to_bits(), "{v}");
                assert_eq!(generic.to_bits(), v.to_bits(), "{v}");
            }
        }
        for (x, fl) in [
            (2.5f64, 2.0f64),
            (-2.5, -3.0),
            (-0.0, -0.0),
            (0.5, 0.0),
            (-0.5, -1.0),
        ] {
            assert_eq!(FloatOps::floor(x).to_bits(), fl.to_bits(), "{x}");
        }
        assert_eq!(FloatOps::exp(0.0f64).to_bits(), 1.0f64.to_bits());
        assert_eq!(FloatOps::exp(f64::NEG_INFINITY).to_bits(), 0);
        assert_eq!(FloatOps::exp(f64::INFINITY), f64::INFINITY);
        assert!(FloatOps::exp(f64::NAN).is_nan());
        assert_eq!(FloatOps::midpoint(f64::MAX, f64::MAX), f64::MAX);
        assert_eq!(FloatOps::midpoint(1.0f64, 2.0), 1.5);
        assert!(FloatOps::total_gt(0.0f64, -0.0));
        assert!(!FloatOps::total_gt(-0.0f64, 0.0));
        assert!(FloatOps::total_gt(f64::NAN, f64::INFINITY));
        assert!(FloatOps::total_gt(f64::NEG_INFINITY, -f64::NAN));
        assert!(FloatOps::total_lt(-0.0f64, 0.0));
        assert!(!FloatOps::total_lt(0.0f64, -0.0));
        assert!(FloatOps::total_lt(f64::INFINITY, f64::NAN));
        assert!(!FloatOps::total_lt(f64::NAN, f64::NAN));
        assert_eq!(<f64 as FloatOps>::lowest().to_bits(), 0xffef_ffff_ffff_ffff);
        assert_eq!(
            <f64 as FloatOps>::highest().to_bits(),
            0x7fef_ffff_ffff_ffff
        );
        for (n, bits) in [
            (0usize, 0u64),
            (1, 0x3ff0_0000_0000_0000),
            (3, 0x4008_0000_0000_0000),
            ((1 << 53) + 1, 0x4340_0000_0000_0000),
            ((1 << 53) + 3, 0x4340_0000_0000_0002),
            (usize::MAX, 0x43f0_0000_0000_0000),
        ] {
            assert_eq!(<f64 as FloatOps>::from_usize(n).to_bits(), bits, "{n}");
        }
    }

    /// Bit equality, except that any two NaNs agree: Rust leaves the sign and payload of a NaN
    /// produced by arithmetic unspecified, and LLVM may commute `fadd`/`fmul`, so which NaN
    /// operand propagates (or the x86 default NaN `0xfff8...`) can differ between the inlined
    /// shipped expansion and the out-of-line generic twin.
    fn same_f64(a: f64, b: f64) -> bool {
        a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
    }

    #[test]
    fn generic_smoother_matches_the_reference() {
        let specials = [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            0.0,
            -0.0,
            f64::MAX,
            f64::MIN,
            f64::MIN_POSITIVE,
            5e-324,
            1.0,
            -1.0,
        ];
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for length in [1u32, 2, 3, 5, 14, 100, 100_000, u32::MAX] {
            let r: f64 = super::rma_alpha_generic(length);
            let e: f64 = super::ema_alpha_generic(length);
            assert_eq!(r.to_bits(), (1.0 / f64::from(length)).to_bits());
            assert_eq!(e.to_bits(), (2.0 / (f64::from(length) + 1.0)).to_bits());
            let fixed_alpha = f64::from_bits(next());
            for alpha in [r, e, 0.5, fixed_alpha] {
                for mode in 0..4 {
                    let mut new = Smoother::<f64>::default();
                    let mut old = RefSmoother::default();
                    let mut new_step = Smoother::<f64>::default();
                    let mut old_step = RefSmoother::default();
                    for i in 0..400 {
                        let raw = next();
                        let x = match mode {
                            0 => (raw % 2001) as f64 - 1000.0,
                            1 => specials[(raw % specials.len() as u64) as usize],
                            2 => f64::from_bits(raw),
                            _ if i % 3 == 0 => f64::NAN,
                            _ => (raw % 97) as f64 * 0.25,
                        };
                        let a = new.advance_generic(x, length, alpha);
                        let b = ref_advance(&mut old, x, length, alpha);
                        assert!(same_f64(a, b), "advance len {length} i {i}");
                        assert!(same_f64(new.sum, old.sum));
                        assert!(same_f64(new.value, old.value));
                        assert_eq!((new.count, new.seeded), (old.count, old.seeded));
                        if length < 1000 {
                            // The shipped `f64_op` expansion and the generic
                            // twin at `f64`, against the plain restatement.
                            let mut twin = new_step;
                            let t = twin.step_generic(x, length, alpha);
                            let c = new_step.step(x, length, alpha);
                            assert!(same_f64(t, c));
                            assert!(same_f64(twin.sum, new_step.sum));
                            assert!(same_f64(twin.value, new_step.value));
                            assert_eq!(
                                (twin.count, twin.seeded),
                                (new_step.count, new_step.seeded)
                            );
                            let d = old_step.step(x, length, alpha);
                            assert!(same_f64(c, d), "step len {length} i {i}");
                            assert!(same_f64(new_step.value, old_step.value));
                        }
                    }
                }
            }
        }
    }
}
