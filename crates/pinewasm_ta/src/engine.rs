//! The advance rules: one bar of one call site.
//!
//! This module implements the interpreter's advance rules, with
//! `Option<f64>` replaced by `f64` + `NAN`, `Vec`/`VecDeque` windows replaced
//! by [`crate::arena`] rings, and market-data reads routed through [`Lanes`].
//! Nothing else moved: the arithmetic, the accumulation order, and the
//! warm-up/na rules are the same expressions in the same sequence, because
//! the compiled artifact and the interpreter have to agree in every bit.
//!
//! Warm-up/na rules (unchanged from `docs/language-support.md`):
//! - window functions return na until `length` values have been received at
//!   the site, and an na input inside the window poisons it — except
//!   `ta.mom`/`ta.roc`, which only need the two endpoint values;
//! - recursive functions seed with the mean of the first `length` non-na
//!   inputs, then recurse; an na input yields na that bar but does **not**
//!   reset the state.

use crate::arena::{sort_total, tie_block, window_extremum_arm, Ring};
#[cfg(any(test, feature = "aeneas"))]
use crate::arena::{sort_total_generic, tie_block_generic, PairRing};
#[cfg(any(test, feature = "aeneas"))]
use crate::state::Sar;
use crate::state::{ema_alpha, f64_op, length_of, rma_alpha, smooth_advance, Smoother, State};
#[cfg(any(test, feature = "aeneas"))]
use crate::state::{ema_alpha_generic, generic_op, FloatOps};
use crate::{Decl, TaFunc};

/// Market data the engine reads directly (the functions with no source
/// operand: `ta.tr`, `ta.obv`, `ta.sar`, ...).
///
/// The interpreter implements this over `MarketData`; an emitted artifact
/// implements it over its own builtin lanes. Out-of-range bars read as na,
/// which is also how a data hole reads, so neither side needs bounds logic.
pub trait Lanes {
    fn open(&self, bar: u32) -> f64;
    fn high(&self, bar: u32) -> f64;
    fn low(&self, bar: u32) -> f64;
    fn close(&self, bar: u32) -> f64;
    fn volume(&self, bar: u32) -> f64;
    /// Bar open time in epoch milliseconds (`MarketData::time_at`).
    fn time_ms(&self, bar: u32) -> i64;
}

/// One `CALL_TA` result: the pushed value plus the extra elements a
/// tuple-returning function produces. na is `NAN` throughout; int and bool
/// results ride the same lane (`0.0`/`1.0` for bools, exact integers for
/// `ta.barssince` and the `*bars` family), which is what the compiled ABI
/// has always done.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Out {
    pub value: f64,
    pub extra: [f64; 2],
}

impl Out {
    fn scalar(value: f64) -> Out {
        Out {
            value,
            extra: [f64::NAN; 2],
        }
    }
}

/// `Value::from_f64`'s rule: anything non-finite is na.
#[inline]
fn norm(v: f64) -> f64 {
    if v.is_finite() {
        v
    } else {
        f64::NAN
    }
}

/// A finite market-data read (`None` for NaN/±Inf holes) as na-or-value.
#[inline]
fn finite(v: f64) -> f64 {
    norm(v)
}

#[inline]
fn present(v: f64) -> bool {
    !v.is_nan()
}

#[inline]
fn bool_out(b: bool) -> f64 {
    if b {
        1.0
    } else {
        0.0
    }
}

/// `true_range`'s body over an operation dictionary: `f64_op`
/// for the shipped function, `generic_op` for the extracted twin. The market
/// reads are macro arguments: the shipped function reads `high`/`low` at `bar`
/// and, only when `bar != 0`, `close` at `bar - 1`; the twin takes the four
/// raw reads as values (`first` is `bar == 0`).
macro_rules! true_range_fn {
    (
        $op:ident, $f:ty, $(#[$m:meta])* $vis:vis fn $name:ident
        [$($g:tt)*] ($($p:ident: $t:ty),*)
        { high: $high:expr, low: $low:expr, first: $first:expr, prev_close: $pc:expr }
    ) => {
        $(#[$m])*
        $vis fn $name<$($g)*>($($p: $t,)* handle_na: bool) -> $f {
            let high = $op!(finite $high);
            let low = $op!(finite $low);
            if $op!(is_nan high) || $op!(is_nan low) {
                return $op!(nan);
            }
            let hl = $op!(sub high, low);
            let prev_close = if $first {
                $op!(nan)
            } else {
                $op!(finite $pc)
            };
            if $op!(present prev_close) {
                $op!(max
                    $op!(max hl, $op!(abs $op!(sub high, prev_close))),
                    $op!(abs $op!(sub low, prev_close)))
            } else if handle_na {
                hl
            } else {
                $op!(nan)
            }
        }
    };
}

true_range_fn!(
    f64_op,
    f64,
    /// True range at `bar`: `max(high - low, |high - close[1]|, |low - close[1]|)`.
    /// With no previous close (`bar` 0 or a non-finite hole), `handle_na` selects
    /// `high - low` or na. na when high/low are data holes.
    fn true_range[L: Lanes](lanes: &L, bar: u32)
    { high: lanes.high(bar), low: lanes.low(bar), first: bar == 0, prev_close: lanes.close(bar - 1) }
);

#[cfg(any(test, feature = "aeneas"))]
true_range_fn!(
    generic_op,
    F,
    /// `true_range_fn!` over `generic_op`: the true range from the raw reads
    /// `high(bar)`, `low(bar)`, `close(bar - 1)` (ignored when `first`).
    pub(crate) fn true_range_generic[F: FloatOps](h: F, l: F, first: bool, pc: F)
    { high: h, low: l, first: first, prev_close: pc }
);

/// The engine's `State::Smooth` arm (`ta.ema`/`ta.rma`/`ta.atr`) after
/// `length`: the input and alpha by function (atr smooths the true range,
/// `handle_na = true` form, with the rma alpha; no source operand), then
/// `smooth_advance!`. `$src` is the source operand and `$tr` the true range,
/// each evaluated only in its arm.
macro_rules! smooth_arm {
    ($op:ident, $s:expr, $step:ident, $func:expr, $length:expr, $src:expr, $tr:expr) => {{
        let (x, alpha) = match $func {
            // atr smooths the true range (handle_na = true form) with
            // the rma alpha; no source operand.
            TaFunc::Atr => ($tr, rma_alpha!($op, $length)),
            TaFunc::Rma => ($src, rma_alpha!($op, $length)),
            _ => ($src, ema_alpha!($op, $length)),
        };
        // `if present(x) { norm(s.step(x, length, alpha)) } else { NAN }`;
        // the generic twin is `Smoother::advance_generic`.
        smooth_advance!($op, $s, $step, x, $length, alpha)
    }};
}

/// The engine's `State::Rsi` arm after `length`: the rma alpha, then a
/// present input replaces `prev`; with a present previous value the change is
/// split into a gain `max(change, 0)` and a loss `max(-change, 0)`, each
/// smoothed, and the output is `norm(100 * gain / (gain + loss))` once both
/// are seeded. An na input yields na and leaves the state untouched.
macro_rules! rsi_arm {
    ($op:ident, $prev:expr, $up:expr, $down:expr, $step:ident, $x:expr, $length:expr) => {{
        let alpha = $op!(div $op!(one), $op!(from_u32 $length));
        let x = $x;
        if $op!(present x) {
            let p = core::mem::replace($prev, x);
            if $op!(present p) {
                let change = $op!(sub x, p);
                let gain = $up.$step($op!(max_raw change, $op!(zero)), $length, alpha);
                let loss = $down.$step($op!(max_raw $op!(neg change), $op!(zero)), $length, alpha);
                if $op!(present gain) && $op!(present loss) {
                    // 100 * up / (up + down); flat windows (0/0)
                    // normalize to na.
                    $op!(norm $op!(div $op!(mul $op!(hundred), gain), $op!(add gain, loss)))
                } else {
                    $op!(nan)
                }
            } else {
                $op!(nan)
            }
        } else {
            $op!(nan)
        }
    }};
}

/// `smooth_arm!` over `generic_op`: the extracted `State::Smooth` arm. `src`
/// is `arg(0)`; `high`, `low`, `first`, `prev_close` are the true range's raw
/// reads (used only for `ta.atr`). The tag parameter is not named `func`
/// (it would shadow the extracted module `func`; see `cells_for`).
#[cfg(any(test, feature = "aeneas"))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn smooth_arm_generic<F: FloatOps>(
    s: &mut Smoother<F>,
    ta_func: TaFunc,
    length: u32,
    src: F,
    high: F,
    low: F,
    first: bool,
    prev_close: F,
) -> F {
    smooth_arm!(
        generic_op,
        s,
        step_generic,
        ta_func,
        length,
        src,
        true_range_generic(high, low, first, prev_close, true)
    )
}

/// `rsi_arm!` over `generic_op`: the extracted `State::Rsi` arm.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn rsi_arm_generic<F: FloatOps>(
    prev: &mut F,
    up: &mut Smoother<F>,
    down: &mut Smoother<F>,
    x: F,
    length: u32,
) -> F {
    rsi_arm!(generic_op, prev, up, down, step_generic, x, length)
}

/// A tuple-returning arm's result in the shipped engine: return it from
/// `advance_state` as an [`Out`] (the output hook of the
/// tuple-arm macros; the generic twins use `out_tuple!`).
macro_rules! out_return {
    ($value:expr, $a:expr, $b:expr) => {
        return Some(Out {
            value: $value,
            extra: [$a, $b],
        })
    };
    // `$value` with both extra outputs na.
    ($value:expr; na) => {
        return Some(Out {
            value: $value,
            extra: [f64::NAN; 2],
        })
    };
}

/// The generic twins' output hook: the three values as a tuple.
#[cfg(any(test, feature = "aeneas"))]
macro_rules! out_tuple {
    ($value:expr, $a:expr, $b:expr) => {
        ($value, $a, $b)
    };
    ($value:expr; na) => {
        ($value, F::nan(), F::nan())
    };
}

/// The generic twins' output hook for arms with an early output: return the
/// three values as a tuple (`out_return!`'s twin).
#[cfg(any(test, feature = "aeneas"))]
macro_rules! out_return_tuple {
    ($value:expr, $a:expr, $b:expr) => {
        return ($value, $a, $b)
    };
    ($value:expr; na) => {
        return ($value, F::nan(), F::nan())
    };
}

/// `step_if`'s body over an operation dictionary: advance a smoother only
/// for a present input (an na input leaves the recursion untouched and
/// yields na). `$step` is the smoother method the dictionary pairs with.
macro_rules! step_if_fn {
    (
        $op:ident, $f:ty, $step:ident, $(#[$m:meta])* $vis:vis fn $name:ident
        [$($g:tt)*] ($sm:ty)
    ) => {
        $(#[$m])*
        $vis fn $name<$($g)*>(s: &mut $sm, x: $f, length: u32, alpha: $f) -> $f {
            if $op!(present x) {
                s.$step(x, length, alpha)
            } else {
                $op!(nan)
            }
        }
    };
}

/// `bar_span`'s body over an operation dictionary. The shipped function reads
/// the true range and `high`/`low` at `bar` (each only in its branch); the
/// twin takes the raw reads as values.
macro_rules! bar_span_fn {
    (
        $op:ident, $f:ty, $(#[$m:meta])* $vis:vis fn $name:ident
        [$($g:tt)*] ($($p:ident: $t:ty),*)
        { tr: $tr:expr, high: $high:expr, low: $low:expr }
    ) => {
        $(#[$m])*
        $vis fn $name<$($g)*>($($p: $t,)* use_true_range: bool) -> $f {
            if use_true_range {
                $tr
            } else {
                let h = $op!(finite $high);
                let l = $op!(finite $low);
                if $op!(present h) && $op!(present l) {
                    $op!(sub h, l)
                } else {
                    $op!(nan)
                }
            }
        }
    };
}

/// The `ta.tr` arm (`State::Stateless`, the default case) after `handle_na`:
/// the normalized true range.
macro_rules! tr_arm {
    ($op:ident, $tr:expr) => {
        $op!(norm $tr)
    };
}

/// The `State::Fixnan` arm: remember a present input; output the last
/// present input (normalized), na until one exists.
macro_rules! fixnan_arm {
    ($op:ident, $last:expr, $x:expr) => {{
        let x = $x;
        if $op!(present x) {
            *$last = x;
        }
        $op!(norm *$last)
    }};
}

/// The `State::Cum` arm: a present input is added to the running total and
/// the (normalized) total is the output; an na input yields na and leaves
/// the total untouched.
macro_rules! cum_arm {
    ($op:ident, $total:expr, $x:expr) => {{
        let x = $x;
        if $op!(present x) {
            $op!(add_assign *$total, x);
            $op!(norm *$total)
        } else {
            // na input: na output, running total untouched.
            $op!(nan)
        }
    }};
}

/// The `State::Extremum` arm (`ta.max`/`ta.min`): an na input is ignored;
/// the first present input starts the running extremum. `$value` is the
/// state place, `$recv` its current value (`*value`).
macro_rules! extremum_arm {
    ($op:ident, $value:expr, $recv:expr, $x:expr, $is_max:expr) => {{
        let x = $x;
        if $op!(present x) {
            *$value = if !$op!(present *$value) {
                x
            } else if $is_max {
                $op!(max $recv, x)
            } else {
                $op!(min $recv, x)
            };
        }
        $op!(norm *$value)
    }};
}

/// The `State::Cross` arm (`ta.crossover`/`ta.crossunder`/`ta.cross`): a
/// cross needs a previous bar and all four values present; the operands
/// always become the previous values.
macro_rules! cross_arm {
    ($op:ident, $func:expr, $prev_a:expr, $prev_b:expr, $seen:expr, $a:expr, $b:expr) => {{
        let a = $a;
        let b = $b;
        let crossed = if *$seen
            && $op!(present a)
            && $op!(present b)
            && $op!(present *$prev_a)
            && $op!(present *$prev_b)
        {
            let (pa, pb) = (*$prev_a, *$prev_b);
            match $func {
                TaFunc::Crossover => $op!(gt a, b) && $op!(le pa, pb),
                TaFunc::Crossunder => $op!(lt a, b) && $op!(ge pa, pb),
                _ => ($op!(gt a, b) && $op!(le pa, pb)) || ($op!(lt a, b) && $op!(ge pa, pb)),
            }
        } else {
            // First bar, or any na among the four values: no cross.
            false
        };
        *$prev_a = a;
        *$prev_b = b;
        *$seen = true;
        $op!(bool_out crossed)
    }};
}

/// The `State::Macd` arm after the three lengths: `macdLine = ema(source,
/// fast) − ema(source, slow)`; the signal is an ema of the macd line, so it
/// only advances once the macd line exists. `$alpha` is the ema alpha
/// function, `$out` the output hook.
macro_rules! macd_arm {
    (
        $op:ident, $out:ident, $step:ident, $alpha:path,
        $fast:expr, $slow:expr, $signal:expr, $fast_len:expr, $slow_len:expr, $sig_len:expr,
        $x:expr
    ) => {{
        let fast_len = $fast_len;
        let slow_len = $slow_len;
        let sig_len = $sig_len;
        let x = $x;
        let macd = if $op!(present x) {
            let f = $fast.$step(x, fast_len, $alpha(fast_len));
            let s = $slow.$step(x, slow_len, $alpha(slow_len));
            if $op!(present f) && $op!(present s) {
                $op!(sub f, s)
            } else {
                $op!(nan)
            }
        } else {
            $op!(nan)
        };
        let sig = if $op!(present macd) {
            $signal.$step(macd, sig_len, $alpha(sig_len))
        } else {
            $op!(nan)
        };
        let hist = if $op!(present macd) && $op!(present sig) {
            $op!(sub macd, sig)
        } else {
            $op!(nan)
        };
        $out!($op!(norm macd), $op!(norm sig), $op!(norm hist))
    }};
}

/// The `State::Keltner` arm of `ta.kc`: the ema of the source (`middle`) and
/// of the bar range, each advanced only on a present input; the bands are
/// `middle ± range * mult`. `$use_tr` names the flag the `$span` operand
/// reads.
macro_rules! kc_arm {
    (
        $op:ident, $out:ident, $step_if:ident, $alpha:path, $basis:expr, $range_ema:expr,
        $length:expr, $mult:expr, $use_tr:ident = $use_tr_e:expr, $span:expr, $src:expr
    ) => {{
        let length = $length;
        let mult = $mult;
        let $use_tr = $use_tr_e;
        let alpha = $alpha(length);
        let span = $span;
        let middle = $step_if($basis, $src, length, alpha);
        let range = $step_if($range_ema, span, length, alpha);
        let (upper, lower) = if $op!(present middle) && $op!(present range) {
            (
                $op!(add middle, $op!(mul range, mult)),
                $op!(sub middle, $op!(mul range, mult)),
            )
        } else {
            ($op!(nan), $op!(nan))
        };
        $out!($op!(norm middle), $op!(norm upper), $op!(norm lower))
    }};
}

/// The `State::Keltner` arm of `ta.kcw`: as `kc_arm!` (the alpha inline),
/// then the band width as a percent of the basis. This percent-valued width
/// helper is kept for the test suite. The production ratio policy is
/// `kcw_ratio` below.
#[cfg(any(test, feature = "aeneas"))]
macro_rules! kcw_arm {
    (
        $op:ident, $step_if:ident, $basis:expr, $range_ema:expr,
        $length:expr, $mult:expr, $use_tr:ident = $use_tr_e:expr, $span:expr, $src:expr
    ) => {{
        let length = $length;
        let mult = $mult;
        let $use_tr = $use_tr_e;
        let alpha = ema_alpha!($op, length);
        let span = $span;
        let basis_out = $step_if($basis, $src, length, alpha);
        let range_out = $step_if($range_ema, span, length, alpha);
        if $op!(present basis_out) && $op!(present range_out) {
            // ((basis + r*mult) - (basis - r*mult)) / basis * 100 — width
            // as a percent of basis.
            $op!(norm $op!(mul $op!(div $op!(mul $op!(mul $op!(two), mult), range_out), basis_out), $op!(hundred)))
        } else {
            $op!(nan)
        }
    }};
}

/// Keltner channel width is a ratio, `(upper - lower) / basis`.
/// The percent-valued helper above is kept unchanged for the tests; this
/// function is the production rule.
fn kcw_ratio(
    basis: &mut Smoother,
    range_ema: &mut Smoother,
    length: u32,
    mult: f64,
    src: f64,
    span: f64,
) -> f64 {
    let alpha = ema_alpha(length);
    let basis_out = step_if(basis, src, length, alpha);
    let range_out = step_if(range_ema, span, length, alpha);
    if present(basis_out) && present(range_out) {
        norm(2.0 * mult * range_out / basis_out)
    } else {
        f64::NAN
    }
}

/// `tr_arm!` over `generic_op`: the extracted `ta.tr` arm.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn tr_arm_generic<F: FloatOps>(h: F, l: F, first: bool, pc: F, handle_na: bool) -> F {
    tr_arm!(generic_op, true_range_generic(h, l, first, pc, handle_na))
}

/// `fixnan_arm!` over `generic_op`.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn fixnan_arm_generic<F: FloatOps>(last: &mut F, x: F) -> F {
    fixnan_arm!(generic_op, last, x)
}

/// `cum_arm!` over `generic_op`.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn cum_arm_generic<F: FloatOps>(total: &mut F, x: F) -> F {
    cum_arm!(generic_op, total, x)
}

/// `extremum_arm!` over `generic_op` (`is_max` is `func == TaFunc::Max`).
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn extremum_arm_generic<F: FloatOps>(value: &mut F, x: F, is_max: bool) -> F {
    extremum_arm!(generic_op, value, *value, x, is_max)
}

/// `cross_arm!` over `generic_op`.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn cross_arm_generic<F: FloatOps>(
    ta_func: TaFunc,
    prev_a: &mut F,
    prev_b: &mut F,
    seen: &mut bool,
    a: F,
    b: F,
) -> F {
    cross_arm!(generic_op, ta_func, prev_a, prev_b, seen, a, b)
}

/// `macd_arm!` over `generic_op`: `(macd, signal, hist)`.
#[cfg(any(test, feature = "aeneas"))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn macd_arm_generic<F: FloatOps>(
    fast: &mut Smoother<F>,
    slow: &mut Smoother<F>,
    signal: &mut Smoother<F>,
    fast_len: u32,
    slow_len: u32,
    sig_len: u32,
    x: F,
) -> (F, F, F) {
    macd_arm!(
        generic_op,
        out_tuple,
        step_generic,
        ema_alpha_generic::<F>,
        fast,
        slow,
        signal,
        fast_len,
        slow_len,
        sig_len,
        x
    )
}

/// `kc_arm!` over `generic_op`: `(middle, upper, lower)`. `high`, `low`,
/// `first`, `prev_close` are the bar range's raw reads.
#[cfg(any(test, feature = "aeneas"))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn kc_arm_generic<F: FloatOps>(
    basis: &mut Smoother<F>,
    range_ema: &mut Smoother<F>,
    length: u32,
    mult: F,
    use_true_range: bool,
    src: F,
    high: F,
    low: F,
    first: bool,
    prev_close: F,
) -> (F, F, F) {
    kc_arm!(
        generic_op,
        out_tuple,
        step_if_generic,
        ema_alpha_generic::<F>,
        basis,
        range_ema,
        length,
        mult,
        use_tr = use_true_range,
        bar_span_generic(high, low, first, prev_close, use_tr),
        src
    )
}

/// `kcw_arm!` over `generic_op`.
#[cfg(any(test, feature = "aeneas"))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn kcw_arm_generic<F: FloatOps>(
    basis: &mut Smoother<F>,
    range_ema: &mut Smoother<F>,
    length: u32,
    mult: F,
    use_true_range: bool,
    src: F,
    high: F,
    low: F,
    first: bool,
    prev_close: F,
) -> F {
    kcw_arm!(
        generic_op,
        step_if_generic,
        basis,
        range_ema,
        length,
        mult,
        use_tr = use_true_range,
        bar_span_generic(high, low, first, prev_close, use_tr),
        src
    )
}

/// The `State::TimeframeChange` arm (`timeframe.change(timeframe)`) before
/// its output: the bar's time is bucketed by the timeframe length, `$changed`
/// is bound to whether the bucket differs from the previous bar's
/// (`i64::MIN` = none, so the first bar is a change), and the bucket is
/// stored. `$prev_bucket` is the state place, `$tf_param` the baked const
/// param (`Option<i64>`, timeframe seconds), `$time_ms` the raw
/// `time_ms(bar)` read; the caller emits `$op!(bool_out $changed)`.
///
/// It is a statement macro (invoked with `;`) rather than an expression
/// block so the shipped arm expands to exactly the scalar block: an
/// expression macro would be wrapped by rustfmt in a second, nested block.
macro_rules! timeframe_change_step {
    ($prev_bucket:expr, $tf_param:expr, $time_ms:expr, $changed:ident) => {
        // The baked const param is the timeframe length in seconds (>= 1);
        // clamp defensively so the divisor is never zero.
        let tf_secs = $tf_param.unwrap_or(1).max(1);
        let tf_ms = tf_secs.saturating_mul(1000);
        let bucket = $time_ms.div_euclid(tf_ms);
        let $changed = *$prev_bucket != bucket;
        // No `;`: the invocation's own `;` ends this statement, so the
        // expansion has no empty statement.
        *$prev_bucket = bucket
    };
}

/// The `State::BarsSince` arm after the condition: a true condition restarts
/// the count, otherwise a started count advances (saturating); na until the
/// condition has been true once (`i64::MIN` = never).
macro_rules! barssince_arm {
    ($op:ident, $since:expr, $cond:expr) => {{
        if $cond {
            *$since = 0;
        } else if *$since != i64::MIN {
            *$since = $since.saturating_add(1);
        }
        if *$since == i64::MIN {
            $op!(nan)
        } else {
            $op!(from_i64 * $since)
        }
    }};
}

/// The `State::Obv` arm: `cum(sign(change(close)) * volume)`; an na term is
/// na this bar without touching the total; the close always becomes the
/// previous close. `$close`/`$volume` are the raw lane reads.
macro_rules! obv_arm {
    ($op:ident, $total:expr, $prev_close:expr, $close:expr, $volume:expr) => {{
        let close = $op!(finite $close);
        let volume = $op!(finite $volume);
        // cum(sign(change(close)) * volume): an na term is na this bar
        // without touching the total (the ta.cum rule).
        let value = if $op!(present close) && $op!(present *$prev_close) && $op!(present volume) {
            let sign = if $op!(gt close, *$prev_close) {
                $op!(one)
            } else if $op!(lt close, *$prev_close) {
                $op!(neg_one)
            } else {
                $op!(zero)
            };
            $op!(add_assign *$total, $op!(mul sign, volume));
            $op!(norm *$total)
        } else {
            $op!(nan)
        };
        *$prev_close = close;
        value
    }};
}

/// The `State::AccDist` arm: cum of `clv * volume` (a flat bar contributes
/// `0`); a data hole is na and leaves the total.
macro_rules! accdist_arm {
    ($op:ident, $total:expr, $high:expr, $low:expr, $close:expr, $volume:expr) => {{
        let h = $op!(finite $high);
        let l = $op!(finite $low);
        let c = $op!(finite $close);
        let v = $op!(finite $volume);
        if $op!(present h) && $op!(present l) && $op!(present c) && $op!(present v) {
            // cum of clv * volume; a flat bar contributes 0.
            let term = if $op!(feq h, l) {
                $op!(zero)
            } else {
                $op!(mul $op!(div $op!(sub $op!(sub $op!(mul $op!(two), c), l), h), $op!(sub h, l)), v)
            };
            $op!(add_assign *$total, term);
            $op!(norm *$total)
        } else {
            // Data hole: na this bar, total untouched.
            $op!(nan)
        }
    }};
}

/// The `State::Pvt` arm: cum of `change(close) / close[1] * volume`; a
/// non-finite term (zero previous close) is na and leaves the total.
macro_rules! pvt_arm {
    ($op:ident, $total:expr, $prev_close:expr, $close:expr, $volume:expr) => {{
        let close = $op!(finite $close);
        let volume = $op!(finite $volume);
        // cum(change(close) / close[1] * volume); a zero previous close
        // divides to a non-finite term → na.
        let value = if $op!(present close) && $op!(present *$prev_close) && $op!(present volume) {
            let term = $op!(mul $op!(div $op!(sub close, *$prev_close), *$prev_close), volume);
            if $op!(is_finite term) {
                $op!(add_assign *$total, term);
                $op!(norm *$total)
            } else {
                $op!(nan)
            }
        } else {
            $op!(nan)
        };
        *$prev_close = close;
        value
    }};
}

/// The `State::Wad` arm: `gain = mom > 0 ? close - min(low, close[1]) : mom
/// < 0 ? close - max(high, close[1]) : 0` (an na momentum takes the final
/// else branch); an na gain (a hole in the needed high/low) is na and leaves
/// the total. `$high`/`$low`/`$close` are the raw lane reads.
macro_rules! wad_arm {
    ($op:ident, $total:expr, $prev_close:expr, $high:expr, $low:expr, $close:expr) => {{
        let high = $op!(finite $high);
        let low = $op!(finite $low);
        let close = $op!(finite $close);
        // gain = mom > 0 ? close - min(low, close[1])
        //      : mom < 0 ? close - max(high, close[1]) : 0,
        // where an **na** momentum takes the final else branch.
        let gain = if $op!(present close) && $op!(present *$prev_close) {
            let p = *$prev_close;
            if $op!(gt close, p) {
                if $op!(present low) {
                    $op!(sub close, $op!(min_raw low, p))
                } else {
                    $op!(nan)
                }
            } else if $op!(lt close, p) {
                if $op!(present high) {
                    $op!(sub close, $op!(max_raw high, p))
                } else {
                    $op!(nan)
                }
            } else {
                $op!(zero)
            }
        } else {
            $op!(zero)
        };
        let value = if $op!(present gain) {
            $op!(add_assign *$total, gain);
            $op!(norm *$total)
        } else {
            // A hole in the needed high/low: na, total untouched.
            $op!(nan)
        };
        *$prev_close = close;
        value
    }};
}

/// `barssince_arm!` over `generic_op` (`c` is `cond(0)`).
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn barssince_arm_generic<F: FloatOps>(since: &mut i64, c: bool) -> F {
    barssince_arm!(generic_op, since, c)
}

/// `timeframe_change_step!` then `generic_op!(bool_out …)`: the extracted
/// `timeframe.change` arm (`tf_param` is `decl.param(0)`, `time_ms` the raw
/// `time_ms(bar)` read).
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn timeframe_change_arm_generic<F: FloatOps>(
    prev_bucket: &mut i64,
    tf_param: Option<i64>,
    time_ms: i64,
) -> F {
    timeframe_change_step!(prev_bucket, tf_param, time_ms, changed);
    generic_op!(bool_out changed)
}

/// `obv_arm!` over `generic_op` (`close`, `volume`: the raw lane reads).
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn obv_arm_generic<F: FloatOps>(
    total: &mut F,
    prev_close: &mut F,
    close: F,
    volume: F,
) -> F {
    obv_arm!(generic_op, total, prev_close, close, volume)
}

/// `accdist_arm!` over `generic_op`.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn accdist_arm_generic<F: FloatOps>(
    total: &mut F,
    high: F,
    low: F,
    close: F,
    volume: F,
) -> F {
    accdist_arm!(generic_op, total, high, low, close, volume)
}

/// `pvt_arm!` over `generic_op`.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn pvt_arm_generic<F: FloatOps>(
    total: &mut F,
    prev_close: &mut F,
    close: F,
    volume: F,
) -> F {
    pvt_arm!(generic_op, total, prev_close, close, volume)
}

/// `wad_arm!` over `generic_op`.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn wad_arm_generic<F: FloatOps>(
    total: &mut F,
    prev_close: &mut F,
    high: F,
    low: F,
    close: F,
) -> F {
    wad_arm!(generic_op, total, prev_close, high, low, close)
}

/// `window_sum`'s body over an operation dictionary: the plain window sum,
/// oldest to newest. `$get` is the ring read the dictionary pairs with.
macro_rules! window_sum_fn {
    ($op:ident, $f:ty, $get:ident, $(#[$m:meta])* $vis:vis fn $name:ident [$($g:tt)*]) => {
        $(#[$m])*
        $vis fn $name<$($g)*>(ring: Ring, cells: &[$f]) -> $f {
            let mut sum = $op!(zero);
            let mut i = 0u32;
            while i < ring.cap() {
                $op!(add_assign sum, ring.$get(cells, i));
                i += 1;
            }
            sum
        }
    };
}

/// The `ta.stdev` / `ta.variance` / `ta.dev` case of `window_value` once the
/// window is ready (`$count` cells, `$n = f64::from($count)`): the mean, then
/// the mean absolute deviation (`$is_dev`), or the sum of squared deviations
/// over `n` (`$biased`) or `n - 1`, as the variance (`$is_variance`) or its
/// square root. `$get`/`$sum` are the ring read and window sum the
/// dictionary pairs with.
macro_rules! stdev_arm {
    (
        $op:ident, $get:ident, $sum:ident, $ring:expr, $cells:expr, $count:expr, $n:expr,
        $is_dev:expr, $is_variance:expr, $biased:expr
    ) => {{
        let mean = $op!(div $sum($ring, $cells), $n);
        if $is_dev {
            let mut abs_dev = $op!(zero);
            let mut i = 0u32;
            while i < $count {
                $op!(add_assign abs_dev, $op!(abs $op!(sub $ring.$get($cells, i), mean)));
                i += 1;
            }
            return $op!(norm $op!(div abs_dev, $n));
        }
        let biased = $biased;
        let mut ssd = $op!(zero);
        let mut i = 0u32;
        while i < $count {
            let d = $op!(sub $ring.$get($cells, i), mean);
            $op!(add_assign ssd, $op!(mul d, d));
            i += 1;
        }
        // Unbiased variance of a single sample divides by zero and
        // normalizes to na.
        let variance = $op!(div ssd, if biased { $n } else { $op!(sub $n, $op!(one)) });
        if $is_variance {
            $op!(norm variance)
        } else {
            $op!(norm $op!(sqrt variance))
        }
    }};
}

/// The `ta.bbw` case of `window_value` once the window is ready:
/// `((basis + dev) - (basis - dev)) / basis * 100` with basis = sma, dev =
/// mult * stdev (population).
macro_rules! bbw_arm {
    (
        $op:ident, $get:ident, $sum:ident, $ring:expr, $cells:expr, $count:expr, $n:expr,
        $mult:expr
    ) => {{
        let mult = $mult;
        let basis = $op!(div $sum($ring, $cells), $n);
        let mut ssd = $op!(zero);
        let mut i = 0u32;
        while i < $count {
            let d = $op!(sub $ring.$get($cells, i), basis);
            $op!(add_assign ssd, $op!(mul d, d));
            i += 1;
        }
        let stdev = $op!(sqrt $op!(div ssd, $n));
        $op!(norm $op!(mul $op!(div $op!(mul $op!(mul $op!(two), mult), stdev), basis), $op!(hundred)))
    }};
}

/// The `State::Bb` arm (`ta.bb`): push the source; na until the window is
/// ready; then the window mean and `mean ± mult * stdev` (population).
/// `$out` is the output hook (both of its forms return).
macro_rules! bb_arm {
    (
        $op:ident, $out:ident, $push:ident, $ready:ident, $get:ident, $sum:ident,
        $ring:expr, $cells:expr, $x:expr, $length:expr, $mult:expr
    ) => {{
        $ring.$push($cells, $x);
        let length = $length;
        let mult = $mult;
        if !$ring.$ready($cells) {
            $out!($op!(nan); na);
        }
        let n = $op!(from_u32 length);
        let mean = $op!(div $sum(*$ring, $cells), n);
        // Population (biased) standard deviation.
        let mut ssd = $op!(zero);
        let mut i = 0u32;
        while i < $ring.cap() {
            let d = $op!(sub $ring.$get($cells, i), mean);
            $op!(add_assign ssd, $op!(mul d, d));
            i += 1;
        }
        let dev = $op!(mul mult, $op!(sqrt $op!(div ssd, n)));
        $out!($op!(norm mean), $op!(norm $op!(add mean, dev)), $op!(norm $op!(sub mean, dev)))
    }};
}

/// `vwap_source`'s body over an operation dictionary: the source argument
/// for the tags that take one (`$use_src`), else hlc3 of the bar (na on a
/// data hole). The shipped function reads the lanes as operands; the twin
/// takes the raw reads as values.
macro_rules! vwap_source_fn {
    (
        $op:ident, $f:ty, $(#[$m:meta])* $vis:vis fn $name:ident
        [$($g:tt)*] ($($p:ident: $t:ty),*)
        { use_src: $use_src:expr, src: $src:expr, high: $high:expr, low: $low:expr, close: $close:expr }
    ) => {
        $(#[$m])*
        $vis fn $name<$($g)*>($($p: $t),*) -> $f {
            if $use_src {
                $src
            } else {
                let h = $op!(finite $high);
                let l = $op!(finite $low);
                let c = $op!(finite $close);
                if $op!(present h) && $op!(present l) && $op!(present c) {
                    // Bare ta.vwap is cumulative from bar 0 over hlc3.
                    $op!(div $op!(add $op!(add h, l), c), $op!(three))
                } else {
                    $op!(nan)
                }
            }
        }
    };
}

/// `advance_vwap_segment`'s body over an operation dictionary: an anchor
/// restarts the segment; before the first segment starts (anchored forms)
/// the output is na; a present `(source, volume)` pair accumulates `pv`,
/// `p2v`, `vol`; the mean is `pv / vol` once the volume is positive.
macro_rules! vwap_segment_fn {
    ($op:ident, $f:ty, $(#[$m:meta])* $vis:vis fn $name:ident [$($g:tt)*]) => {
        $(#[$m])*
        $vis fn $name<$($g)*>(
            pv: &mut $f,
            p2v: &mut $f,
            vol: &mut $f,
            active: &mut bool,
            source: $f,
            volume: $f,
            anchor: bool,
        ) -> ($f, $f) {
            if anchor {
                *pv = $op!(zero);
                *p2v = $op!(zero);
                *vol = $op!(zero);
                *active = true;
            }
            if !*active {
                return ($op!(nan), $op!(nan));
            }
            if $op!(present source) && $op!(present volume) {
                $op!(add_assign *pv, $op!(mul source, volume));
                $op!(add_assign *p2v, $op!(mul $op!(mul source, source), volume));
                $op!(add_assign *vol, volume);
                if $op!(gt *vol, $op!(zero)) {
                    let mean = $op!(div *pv, *vol);
                    ($op!(norm mean), mean)
                } else {
                    ($op!(nan), $op!(nan))
                }
            } else {
                ($op!(nan), $op!(nan))
            }
        }
    };
}

/// The `State::Vwap` arm of `ta.vwap`, `ta.vwap(source)` and
/// `ta.vwap(source, anchor)`: advance the segment with the source and the
/// (finite) bar volume; the output is the pushed value. `$volume` is the raw
/// lane read.
macro_rules! vwap_arm {
    (
        $op:ident, $segment:ident, $pv:expr, $p2v:expr, $vol:expr, $active:expr,
        $source:expr, $volume:expr, $anchor:expr
    ) => {{
        let source = $source;
        let anchor = $anchor;
        $segment(
            $pv,
            $p2v,
            $vol,
            $active,
            source,
            $op!(finite $volume),
            anchor,
        )
        .0
    }};
}

/// The `State::Vwap` arm of `ta.vwap(source, anchor, stdev_mult)`: as
/// `vwap_arm!`, plus the bands `mean ± sqrt(max(p2v / vol - mean², 0)) *
/// mult` once the mean and the multiplier are present.
macro_rules! vwap_bands_arm {
    (
        $op:ident, $out:ident, $segment:ident, $pv:expr, $p2v:expr, $vol:expr, $active:expr,
        $source:expr, $volume:expr, $anchor:expr, $mult:expr
    ) => {{
        let source = $source;
        let anchor = $anchor;
        let (vwap, mean) = $segment(
            $pv,
            $p2v,
            $vol,
            $active,
            source,
            $op!(finite $volume),
            anchor,
        );
        let mult = $mult;
        if $op!(present mean) && $op!(present mult) {
            // Deterministic `max`: with a negative volume sum the difference
            // can be `-0.0`, and the sign of the zero variance reaches the
            // bands through `sqrt` and `mean ± dev`.
            let variance = $op!(max $op!(sub $op!(div *$p2v, *$vol), $op!(mul mean, mean)), $op!(zero));
            let dev = $op!(mul $op!(sqrt variance), mult);
            $out!(vwap, $op!(norm $op!(add mean, dev)), $op!(norm $op!(sub mean, dev)));
        }
        $out!(vwap; na)
    }};
}

/// `stdev_arm!` over `generic_op`, with the `State::Window` push and the
/// readiness check of `window_value` in front: the extracted `ta.stdev`
/// (`is_dev = false`, `is_variance = false`), `ta.variance`
/// (`is_variance = true`) and `ta.dev` (`is_dev = true`) arm.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn stdev_family_arm_generic<F: FloatOps>(
    ring: &mut Ring,
    cells: &mut [F],
    x: F,
    is_dev: bool,
    is_variance: bool,
    biased: bool,
) -> F {
    ring.push_generic(cells, x);
    let ring = *ring;
    let cells: &[F] = cells;
    if !ring.window_ready_generic(cells) {
        return F::nan();
    }
    let count = ring.cap();
    let n = F::from_u32(count);
    stdev_arm!(
        generic_op,
        get_generic,
        window_sum_generic,
        ring,
        cells,
        count,
        n,
        is_dev,
        is_variance,
        biased
    )
}

/// `bbw_arm!` over `generic_op`, with the `State::Window` push and the
/// readiness check of `window_value` in front: the extracted `ta.bbw` arm.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn bbw_arm_generic<F: FloatOps>(ring: &mut Ring, cells: &mut [F], x: F, mult: F) -> F {
    ring.push_generic(cells, x);
    let ring = *ring;
    let cells: &[F] = cells;
    if !ring.window_ready_generic(cells) {
        return F::nan();
    }
    let count = ring.cap();
    let n = F::from_u32(count);
    bbw_arm!(
        generic_op,
        get_generic,
        window_sum_generic,
        ring,
        cells,
        count,
        n,
        mult
    )
}

/// `bb_arm!` over `generic_op`: `(basis, upper, lower)`.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn bb_arm_generic<F: FloatOps>(
    ring: &mut Ring,
    cells: &mut [F],
    x: F,
    length: u32,
    mult: F,
) -> (F, F, F) {
    bb_arm!(
        generic_op,
        out_return_tuple,
        push_generic,
        window_ready_generic,
        get_generic,
        window_sum_generic,
        ring,
        cells,
        x,
        length,
        mult
    )
}

/// `vwap_arm!` over `generic_op` with the source from `vwap_source_generic`
/// (`use_src`, `src`, and the raw reads `high`, `low`, `close`, `volume`).
#[cfg(any(test, feature = "aeneas"))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn vwap_arm_generic<F: FloatOps>(
    pv: &mut F,
    p2v: &mut F,
    vol: &mut F,
    active: &mut bool,
    use_src: bool,
    src: F,
    high: F,
    low: F,
    close: F,
    volume: F,
    anchor: bool,
) -> F {
    vwap_arm!(
        generic_op,
        advance_vwap_segment_generic,
        pv,
        p2v,
        vol,
        active,
        vwap_source_generic(use_src, src, high, low, close),
        volume,
        anchor
    )
}

/// `vwap_bands_arm!` over `generic_op`: `(vwap, upper, lower)`. The source is
/// `src` (`vwap_source` of the bands tag is its source argument).
#[cfg(any(test, feature = "aeneas"))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn vwap_bands_arm_generic<F: FloatOps>(
    pv: &mut F,
    p2v: &mut F,
    vol: &mut F,
    active: &mut bool,
    src: F,
    volume: F,
    anchor: bool,
    mult: F,
) -> (F, F, F) {
    vwap_bands_arm!(
        generic_op,
        out_return_tuple,
        advance_vwap_segment_generic,
        pv,
        p2v,
        vol,
        active,
        src,
        volume,
        anchor,
        mult
    )
}

/// `at`'s body over an operation dictionary: element `i` of a slice, or na
/// when out of range.
macro_rules! at_fn {
    ($op:ident, $f:ty, $(#[$m:meta])* $vis:vis fn $name:ident [$($g:tt)*]) => {
        $(#[$m])*
        $vis fn $name<$($g)*>(slice: &[$f], i: usize) -> $f {
            match slice.get(i) {
                Some(&v) => v,
                None => $op!(nan),
            }
        }
    };
}

/// `sorted_window`'s body over a cell type: `$get` and `$sort` are the ring
/// read and the `total_cmp` heapsort the dictionary pairs with.
macro_rules! sorted_window_fn {
    ($f:ty, $get:ident, $sort:ident, $(#[$m:meta])* $vis:vis fn $name:ident [$($g:tt)*]) => {
        $(#[$m])*
        $vis fn $name<$($g)*>(ring: Ring, cells: &[$f], scratch: &mut [$f]) -> usize {
            let n = ring.cap() as usize;
            if scratch.len() < n {
                return 0;
            }
            let mut i = 0u32;
            while (i as usize) < n {
                if let Some(slot) = scratch.get_mut(i as usize) {
                    *slot = ring.$get(cells, i);
                }
                i += 1;
            }
            if let Some(head) = scratch.get_mut(..n) {
                $sort(head);
            }
            n
        }
    };
}

/// The `State::ValueWhen` arm (`ta.valuewhen`, all four source types): a
/// true condition captures the source; the output is the oldest capture once
/// the ring holds `occurrence + 1` of them. `$push`/`$get` are the ring
/// methods the dictionary pairs with.
macro_rules! valuewhen_arm {
    ($op:ident, $push:ident, $get:ident, $vals:expr, $cells:expr, $cond:expr, $x:expr) => {{
        if $cond {
            $vals.$push($cells, $x);
        }
        // The ring holds `occurrence + 1` captures; once full its oldest
        // cell is the requested occurrence.
        if $vals.full() {
            $op!(norm $vals.$get($cells, 0))
        } else {
            $op!(nan)
        }
    }};
}

/// The `State::Pivot` arm (`ta.pivothigh` / `ta.pivotlow`): push the source;
/// once the window is ready, the cell `$left` from the oldest is the output
/// when strictly beyond every other cell (`$is_high` picks the side).
macro_rules! pivot_arm {
    (
        $op:ident, $push:ident, $ready:ident, $get:ident, $ring:expr, $cells:expr, $x:expr,
        $left:expr, $is_high:expr
    ) => {{
        $ring.$push($cells, $x);
        let left = $left;
        if $ring.$ready($cells) {
            // The candidate sits `rightbars` cells back; it is a pivot
            // when **strictly** beyond every neighbor on both sides.
            let center = $ring.$get($cells, left);
            let mut is_pivot = true;
            let mut i = 0u32;
            while i < $ring.cap() {
                if i != left {
                    let x = $ring.$get($cells, i);
                    let ok = if $is_high {
                        $op!(gt center, x)
                    } else {
                        $op!(lt center, x)
                    };
                    if !ok {
                        is_pivot = false;
                        break;
                    }
                }
                i += 1;
            }
            if is_pivot {
                $op!(norm center)
            } else {
                $op!(nan)
            }
        } else {
            // Warm-up, or an na cell anywhere in the window.
            $op!(nan)
        }
    }};
}

/// The `ta.median` case of `window_value` once the window is ready: sort a
/// copy into the scratch region; the middle value, or the midpoint of the
/// two middle values for an even length. `$sorted`/`$at` are the sorted copy
/// and the slice read the dictionary pairs with.
macro_rules! median_arm {
    ($op:ident, $sorted:ident, $at:ident, $ring:expr, $cells:expr, $scratch:expr) => {{
        // Average of the two middle values for even lengths.
        let len = $sorted($ring, $cells, $scratch);
        if len == 0 {
            return $op!(nan);
        }
        let mid = len / 2;
        if len % 2 == 1 {
            $op!(norm $at($scratch, mid))
        } else {
            $op!(norm $op!(midpoint $at($scratch, mid - 1), $at($scratch, mid)))
        }
    }};
}

/// The `ta.linreg` case of `window_value` once the window is ready (`$count`
/// cells, `$n = f64::from($count)`): the least-squares line over x = 0
/// (oldest) .. n-1, evaluated at `n - 1 - offset`. `$offset` is the raw
/// integer parameter.
macro_rules! linreg_arm {
    ($op:ident, $get:ident, $ring:expr, $cells:expr, $count:expr, $n:expr, $offset:expr) => {{
        // Least squares over x = 0 (oldest) .. n-1 (newest), evaluated at
        // x = length - 1 - offset.
        let offset = $op!(from_i64 $offset);
        let sum_x = $op!(div $op!(mul $n, $op!(sub $n, $op!(one))), $op!(two));
        let sum_xx = $op!(div
            $op!(mul $op!(mul $op!(sub $n, $op!(one)), $n), $op!(sub $op!(mul $op!(two), $n), $op!(one))),
            $op!(six));
        let mut sum_y = $op!(zero);
        let mut sum_xy = $op!(zero);
        let mut i = 0u32;
        while i < $count {
            let y = $ring.$get($cells, i);
            $op!(add_assign sum_y, y);
            $op!(add_assign sum_xy, $op!(mul $op!(from_u32 i), y));
            i += 1;
        }
        // length 1 has an undefined slope (0/0) and normalizes to na.
        let slope = $op!(div
            $op!(sub $op!(mul $n, sum_xy), $op!(mul sum_x, sum_y)),
            $op!(sub $op!(mul $n, sum_xx), $op!(mul sum_x, sum_x)));
        let intercept = $op!(div $op!(sub sum_y, $op!(mul slope, sum_x)), $n);
        $op!(norm $op!(add intercept, $op!(mul slope, $op!(sub $op!(sub $n, $op!(one)), offset))))
    }};
}

/// The `ta.alma` case of `window_value` once the window is ready: Gaussian
/// weights centred at `offset * (n - 1)` (floored when `$floor`) with width
/// `n / sigma`, through the crate's `exp` function.
macro_rules! alma_arm {
    (
        $op:ident, $get:ident, $ring:expr, $cells:expr, $count:expr, $n:expr,
        $offset:expr, $sigma:expr, $floor:expr
    ) => {{
        // m = offset * (length - 1), s = length / sigma,
        // w_i = exp(-(i - m)^2 / (2 s^2)) with i = 0 the oldest bar.
        let offset = $offset;
        let sigma = $sigma;
        // `floor = true` truncates the weighting center to a whole bar.
        let floor = $floor;
        let m = if floor {
            $op!(floor $op!(mul offset, $op!(sub $n, $op!(one))))
        } else {
            $op!(mul offset, $op!(sub $n, $op!(one)))
        };
        let s = $op!(div $n, sigma);
        let mut weighted = $op!(zero);
        let mut norm_sum = $op!(zero);
        let mut i = 0u32;
        while i < $count {
            let d = $op!(sub $op!(from_u32 i), m);
            // The Gaussian kernel uses the crate's `exp`, like every other
            // transcendental a script can reach: the compiled artifact and
            // the interpreter must agree in the last ulp.
            let w = $op!(exp $op!(div $op!(neg $op!(mul d, d)), $op!(mul $op!(mul $op!(two), s), s)));
            $op!(add_assign weighted, $op!(mul w, $ring.$get($cells, i)));
            $op!(add_assign norm_sum, w);
            i += 1;
        }
        $op!(norm $op!(div weighted, norm_sum))
    }};
}

/// `valuewhen_arm!` over `generic_op`: the extracted `ta.valuewhen` arm
/// (`cond` is the condition, `x` the source read only when it holds).
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn valuewhen_arm_generic<F: FloatOps>(
    vals: &mut Ring,
    cells: &mut [F],
    cond: bool,
    x: F,
) -> F {
    valuewhen_arm!(generic_op, push_generic, get_generic, vals, cells, cond, x)
}

/// `pivot_arm!` over `generic_op`: the extracted `ta.pivothigh` (`is_high`)
/// and `ta.pivotlow` arm; `left` is the clamped leftbars parameter.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn pivot_arm_generic<F: FloatOps>(
    ring: &mut Ring,
    cells: &mut [F],
    x: F,
    left: u32,
    is_high: bool,
) -> F {
    pivot_arm!(
        generic_op,
        push_generic,
        window_ready_generic,
        get_generic,
        ring,
        cells,
        x,
        left,
        is_high
    )
}

/// `median_arm!` over `generic_op`, with the `State::Window` push and the
/// readiness check of `window_value` in front: the extracted `ta.median`
/// arm (both source types).
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn median_arm_generic<F: FloatOps>(
    ring: &mut Ring,
    cells: &mut [F],
    scratch: &mut [F],
    x: F,
) -> F {
    ring.push_generic(cells, x);
    let ring = *ring;
    let cells: &[F] = cells;
    if !ring.window_ready_generic(cells) {
        return F::nan();
    }
    median_arm!(
        generic_op,
        sorted_window_generic,
        at_generic,
        ring,
        cells,
        scratch
    )
}

/// `linreg_arm!` over `generic_op`, with the `State::Window` push and the
/// readiness check of `window_value` in front: the extracted `ta.linreg` arm
/// (`offset` is the raw integer parameter).
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn linreg_arm_generic<F: FloatOps>(
    ring: &mut Ring,
    cells: &mut [F],
    x: F,
    offset: i64,
) -> F {
    ring.push_generic(cells, x);
    let ring = *ring;
    let cells: &[F] = cells;
    if !ring.window_ready_generic(cells) {
        return F::nan();
    }
    let count = ring.cap();
    let n = F::from_u32(count);
    linreg_arm!(generic_op, get_generic, ring, cells, count, n, offset)
}

/// `alma_arm!` over `generic_op`, with the `State::Window` push and the
/// readiness check of `window_value` in front: the extracted `ta.alma` arm
/// (`offset`, `sigma` the parameter floats, `floor` the floor flag).
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn alma_arm_generic<F: FloatOps>(
    ring: &mut Ring,
    cells: &mut [F],
    x: F,
    offset: F,
    sigma: F,
    floor: bool,
) -> F {
    ring.push_generic(cells, x);
    let ring = *ring;
    let cells: &[F] = cells;
    if !ring.window_ready_generic(cells) {
        return F::nan();
    }
    let count = ring.cap();
    let n = F::from_u32(count);
    alma_arm!(
        generic_op,
        get_generic,
        ring,
        cells,
        count,
        n,
        offset,
        sigma,
        floor
    )
}

/// A scalar arm's early result in the shipped engine: return it from
/// `advance_state` as [`Out::scalar`] (the early-output hook
/// of the scalar-arm macros; the generic twins use `value_return!`).
macro_rules! out_scalar_return {
    ($value:expr) => {
        return Some(Out::scalar($value))
    };
}

/// The generic twins' early-output hook for scalar arms: return the value.
#[cfg(any(test, feature = "aeneas"))]
macro_rules! value_return {
    ($value:expr) => {
        return $value
    };
}

/// The `State::Stoch` arm (`ta.stoch(source, high, low, length)`): push the
/// (high, low) pair; once the window is ready and the source present,
/// `100 * (source - lowest low) / (highest high - lowest low)`.
/// `$push`/`$ready`/`$get` are the pair-ring methods the dictionary pairs
/// with.
macro_rules! stoch_arm {
    (
        $op:ident, $push:ident, $ready:ident, $get:ident, $ring:expr, $cells:expr,
        $source:expr, $high:expr, $low:expr
    ) => {{
        let source = $source;
        $ring.$push($cells, $high, $low);
        if $op!(present source) && $ring.$ready($cells) {
            let mut hh = $op!(lowest);
            let mut ll = $op!(highest);
            let mut i = 0u32;
            while i < $ring.cap() {
                let (h, l) = $ring.$get($cells, i);
                hh = $op!(max_raw hh, h);
                ll = $op!(min_raw ll, l);
                i += 1;
            }
            // Window extrema never return `-0.0`: a zero lowest low is
            // `+0.0` on every target. (A zero `hh` needs no such step: it
            // only enters `hh - ll`, which is `-ll` when `ll` is not zero
            // and a zero, so the quotient is na, when `ll` is zero too.)
            let ll = $op!(canon_zero ll);
            // A flat window (hh == ll) divides by zero → na.
            $op!(norm $op!(div $op!(mul $op!(hundred), $op!(sub source, ll)), $op!(sub hh, ll)))
        } else {
            $op!(nan)
        }
    }};
}

/// The `State::Sar` arm (`ta.sar(start, inc, max)`): the parabolic
/// stop-and-reverse over the market data. `$high`/`$low`/`$close` are the
/// raw reads (the arm applies `finite`); a data hole returns na through
/// `$ret` with the recursion frozen.
macro_rules! sar_arm {
    (
        $op:ident, $ret:ident, $s:expr, $start:expr, $inc:expr, $max:expr,
        $high:expr, $low:expr, $close:expr
    ) => {{
        let start = $start;
        let inc = $inc;
        let max = $max;
        let high = $op!(finite $high);
        let low = $op!(finite $low);
        let close = $op!(finite $close);
        if !$op!(present high) || !$op!(present low) || !$op!(present close) {
            // Data hole: na, recursion frozen.
            $ret!($op!(nan));
        }
        // Parabolic stop-and-reverse; `s.bars` counts executed
        // advances (the site's own bar_index).
        let value = if $s.bars == 0 {
            $op!(nan)
        } else {
            let mut is_first_trend_bar = false;
            if $s.bars == 1 {
                if $op!(gt close, $s.prev_close) {
                    $s.is_below = true;
                    $s.max_min = high;
                    $s.result = $s.prev1_low;
                } else {
                    $s.is_below = false;
                    $s.max_min = low;
                    $s.result = $s.prev1_high;
                }
                is_first_trend_bar = true;
                $s.accel = start;
            }
            $op!(add_assign $s.result, $op!(mul $s.accel, $op!(sub $s.max_min, $s.result)));
            if $s.is_below {
                if $op!(gt $s.result, low) {
                    is_first_trend_bar = true;
                    $s.is_below = false;
                    $s.result = $op!(max high, $s.max_min);
                    $s.max_min = low;
                    $s.accel = start;
                }
            } else if $op!(lt $s.result, high) {
                is_first_trend_bar = true;
                $s.is_below = true;
                $s.result = $op!(min low, $s.max_min);
                $s.max_min = high;
                $s.accel = start;
            }
            if !is_first_trend_bar {
                if $s.is_below {
                    if $op!(gt high, $s.max_min) {
                        $s.max_min = high;
                        $s.accel = $op!(min $op!(add $s.accel, inc), max);
                    }
                } else if $op!(lt low, $s.max_min) {
                    $s.max_min = low;
                    $s.accel = $op!(min $op!(add $s.accel, inc), max);
                }
            }
            if $s.is_below {
                $s.result = $op!(min $s.result, $s.prev1_low);
                if $s.bars > 1 {
                    $s.result = $op!(min $s.result, $s.prev2_low);
                }
            } else {
                $s.result = $op!(max $s.result, $s.prev1_high);
                if $s.bars > 1 {
                    $s.result = $op!(max $s.result, $s.prev2_high);
                }
            }
            $op!(norm $s.result)
        };
        $s.prev2_high = $s.prev1_high;
        $s.prev2_low = $s.prev1_low;
        $s.prev1_high = high;
        $s.prev1_low = low;
        $s.prev_close = close;
        $s.bars += 1;
        value
    }};
}

/// The `State::Tsi` arm (`ta.tsi(source, short, long)`): the double-smoothed
/// momentum over the double-smoothed absolute momentum (ema alphas of the
/// clamped lengths). An na input yields na and leaves the state untouched.
/// `$step` is the smoother method the dictionary pairs with.
macro_rules! tsi_arm {
    (
        $op:ident, $step:ident, $prev:expr, $pc_long:expr, $pc_short:expr, $abs_long:expr,
        $abs_short:expr, $x:expr, $short:expr, $long:expr
    ) => {{
        let short = $short;
        let long = $long;
        let alpha_short = ema_alpha!($op, short);
        let alpha_long = ema_alpha!($op, long);
        let x = $x;
        if $op!(present x) {
            let p = core::mem::replace($prev, x);
            if $op!(present p) {
                let mom = $op!(sub x, p);
                let long_pc = $pc_long.$step(mom, long, alpha_long);
                let smoothed_pc = if $op!(present long_pc) {
                    $pc_short.$step(long_pc, short, alpha_short)
                } else {
                    $op!(nan)
                };
                let long_abs = $abs_long.$step($op!(abs mom), long, alpha_long);
                let smoothed_abs = if $op!(present long_abs) {
                    $abs_short.$step(long_abs, short, alpha_short)
                } else {
                    $op!(nan)
                };
                if $op!(present smoothed_pc) && $op!(present smoothed_abs) {
                    // In [-1, 1]; an all-flat history (0/0) → na.
                    $op!(norm $op!(div smoothed_pc, smoothed_abs))
                } else {
                    $op!(nan)
                }
            } else {
                $op!(nan)
            }
        } else {
            // na input: na output, state untouched.
            $op!(nan)
        }
    }};
}

/// The `State::Supertrend` arm (`ta.supertrend(factor, atrPeriod)`):
/// `(supertrend, direction)`. `$tr` is the true range (`handle_na = true`
/// form), `$high`/`$low`/`$close` the raw reads at `bar`, `$first` is
/// `bar == 0` and `$pc` the raw previous close (read only when not
/// `$first`). `$ret` returns the all-na output early; `$out` produces the
/// final output.
macro_rules! supertrend_arm {
    (
        $op:ident, $ret:ident, $out:ident, $step:ident, $atr:expr, $bands_seeded:expr,
        $prev_upper:expr, $prev_lower:expr, $prev_supertrend:expr, $prev_direction:expr,
        $atr_period:expr, $factor:expr, $tr:expr, $high:expr, $low:expr, $close:expr,
        $first:expr, $pc:expr
    ) => {{
        let atr_period = $atr_period;
        let factor = $factor;
        // atr = ta.atr(atr_period): the rma of the true range with
        // the handle_na = true form. An na true range freezes the state.
        let tr_now = $tr;
        let atr_now = if $op!(present tr_now) {
            $atr.$step(tr_now, atr_period, rma_alpha!($op, atr_period))
        } else {
            $op!(nan)
        };
        let high = $op!(finite $high);
        let low = $op!(finite $low);
        let close = $op!(finite $close);
        if !($op!(present atr_now) && $op!(present high) && $op!(present low) && $op!(present close)) {
            $ret!($op!(nan); na);
        }
        let src = $op!(midpoint high, low);
        let upper_basic = $op!(add src, $op!(mul factor, atr_now));
        let lower_basic = $op!(sub src, $op!(mul factor, atr_now));
        // prevUpperBand/prevLowerBand default to the current basic on the
        // first valid bar.
        let (prev_up, prev_lo) = if *$bands_seeded {
            (*$prev_upper, *$prev_lower)
        } else {
            (upper_basic, lower_basic)
        };
        let prev_close = if $first {
            $op!(nan)
        } else {
            $op!(finite $pc)
        };
        let upper_band = if $op!(present prev_close)
            && !($op!(lt upper_basic, prev_up) || $op!(gt prev_close, prev_up))
        {
            prev_up
        } else {
            upper_basic
        };
        let lower_band = if $op!(present prev_close)
            && !($op!(gt lower_basic, prev_lo) || $op!(lt prev_close, prev_lo))
        {
            prev_lo
        } else {
            lower_basic
        };
        // direction: +1 downtrend, −1 uptrend. On the first valid bar
        // start at +1 (down).
        let direction = if !*$bands_seeded {
            $op!(one)
        } else if $op!(feq *$prev_supertrend, *$prev_upper) {
            if $op!(gt close, upper_band) {
                $op!(neg_one)
            } else {
                $op!(one)
            }
        } else if $op!(lt close, lower_band) {
            $op!(one)
        } else {
            $op!(neg_one)
        };
        let supertrend = if $op!(feq direction, $op!(neg_one)) {
            lower_band
        } else {
            upper_band
        };
        *$bands_seeded = true;
        *$prev_upper = upper_band;
        *$prev_lower = lower_band;
        *$prev_supertrend = supertrend;
        *$prev_direction = direction;
        $out!($op!(norm supertrend), $op!(norm direction), $op!(nan))
    }};
}

/// The `ta.range` case of `window_value` once the window is ready (`$count`
/// cells): the window's maximum minus its minimum.
macro_rules! range_arm {
    ($op:ident, $get:ident, $ring:expr, $cells:expr, $count:expr) => {{
        let mut max = $op!(lowest);
        let mut min = $op!(highest);
        let mut i = 0u32;
        while i < $count {
            let x = $ring.$get($cells, i);
            max = $op!(max_raw max, x);
            min = $op!(min_raw min, x);
            i += 1;
        }
        // Window extrema never return `-0.0`: a zero maximum is `+0.0` on
        // every target. (That fixes the difference whatever the sign of a
        // zero minimum: `+0.0 - ±0.0` is `+0.0`, and with a non-zero
        // maximum the difference is the maximum.)
        let max = $op!(canon_zero max);
        $op!(norm $op!(sub max, min))
    }};
}

/// `ta.rci`'s tie-block lookup: the first index and the size of `$x`'s tie
/// block in the sorted prefix `$scratch[..$len]` (`(0, 1)` if the prefix is
/// out of range, which a planned layout never is).
macro_rules! tie_at {
    ($tie:ident, $scratch:expr, $len:expr, $x:expr) => {
        match $scratch.get(..$len) {
            Some(sorted) => $tie(sorted, $x),
            None => (0, 1),
        }
    };
}

/// `rci_arm!`'s lookup hook in the generic twin: a call to
/// [`tie_at_generic`] (`$tie` is implied).
#[cfg(any(test, feature = "aeneas"))]
macro_rules! tie_at_call {
    ($tie:ident, $scratch:expr, $len:expr, $x:expr) => {
        tie_at_generic($scratch, $len, $x)
    };
}

/// The `ta.rci` case of `window_value` once the window is ready (`$count`
/// cells, `$n = f64::from($count)`): Spearman's rank correlation between the
/// source and the bar index, scaled to [-100, 100]. `$sorted`/`$tie` are
/// the sorted scratch copy and the tie-block search the dictionary pairs
/// with; `$tie_at` is the tie-block lookup in the sorted prefix
/// (`tie_at!` inline in the shipped expansion, a call to `tie_at_generic`
/// in the twin, because the extractable form cannot join the
/// `Option<&[F]>` match inside the loop).
macro_rules! rci_arm {
    (
        $op:ident, $sorted:ident, $tie:ident, $get:ident, $ring:expr, $cells:expr,
        $scratch:expr, $tie_at:ident, $count:expr, $n:expr
    ) => {{
        // Spearman's rank correlation between the source and the bar
        // index, scaled to [-100, 100]. Price ties take average ranks, so
        // the coefficient is Pearson on (position, rank).
        let len = $sorted($ring, $cells, $scratch);
        if len == 0 {
            return $op!(nan);
        }
        let mean = $op!(div $op!(sub $n, $op!(one)), $op!(two)); // both rank sets average this + 1
        let (mut cov, mut var_x, mut var_y) = ($op!(zero), $op!(zero), $op!(zero));
        let mut j = 0u32;
        while j < $count {
            let x = $ring.$get($cells, j);
            let (first, ties) = $tie_at!($tie, $scratch, len, x);
            // Ranks are 1-based; a tie block shares the average of the
            // ranks it spans.
            let rank = $op!(add
                $op!(add $op!(from_usize first), $op!(div $op!(from_usize ties.max(1) - 1), $op!(two))),
                $op!(one));
            let dx = $op!(sub $op!(from_u32 j), mean);
            let dy = $op!(sub $op!(sub rank, $op!(one)), mean);
            $op!(add_assign cov, $op!(mul dx, dy));
            $op!(add_assign var_x, $op!(mul dx, dx));
            $op!(add_assign var_y, $op!(mul dy, dy));
            j += 1;
        }
        // A flat window has zero rank variance (0/0) → na.
        $op!(norm $op!(div $op!(mul $op!(hundred), cov), $op!(mul $op!(sqrt var_x), $op!(sqrt var_y))))
    }};
}

/// `stoch_arm!` over `generic_op`: the extracted `ta.stoch` arm (`source`
/// is `arg(0)`, `high`/`low` the pushed operands `arg(1)`/`arg(2)`).
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn stoch_arm_generic<F: FloatOps>(
    ring: &mut PairRing,
    cells: &mut [F],
    source: F,
    high: F,
    low: F,
) -> F {
    stoch_arm!(
        generic_op,
        push_generic,
        ready_generic,
        get_generic,
        ring,
        cells,
        source,
        high,
        low
    )
}

/// `sar_arm!` over `generic_op`: the extracted `ta.sar` arm (`start`, `inc`,
/// `max` the parameter floats; `high`, `low`, `close` the raw reads at the
/// bar).
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn sar_arm_generic<F: FloatOps>(
    s: &mut Sar<F>,
    start: F,
    inc: F,
    max: F,
    high: F,
    low: F,
    close: F,
) -> F {
    sar_arm!(
        generic_op,
        value_return,
        s,
        start,
        inc,
        max,
        high,
        low,
        close
    )
}

/// `tsi_arm!` over `generic_op`: the extracted `ta.tsi` arm (`short`, `long`
/// the clamped lengths).
#[cfg(any(test, feature = "aeneas"))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn tsi_arm_generic<F: FloatOps>(
    prev: &mut F,
    pc_long: &mut Smoother<F>,
    pc_short: &mut Smoother<F>,
    abs_long: &mut Smoother<F>,
    abs_short: &mut Smoother<F>,
    x: F,
    short: u32,
    long: u32,
) -> F {
    tsi_arm!(
        generic_op,
        step_generic,
        prev,
        pc_long,
        pc_short,
        abs_long,
        abs_short,
        x,
        short,
        long
    )
}

/// `supertrend_arm!` over `generic_op`: the extracted `ta.supertrend` arm,
/// `(supertrend, direction, na)`. `high`, `low`, `close` are the raw reads
/// at the bar, `prev_close` the raw `close(bar - 1)` (ignored when `first`).
#[cfg(any(test, feature = "aeneas"))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn supertrend_arm_generic<F: FloatOps>(
    atr: &mut Smoother<F>,
    bands_seeded: &mut bool,
    prev_upper: &mut F,
    prev_lower: &mut F,
    prev_supertrend: &mut F,
    prev_direction: &mut F,
    atr_period: u32,
    factor: F,
    high: F,
    low: F,
    close: F,
    first: bool,
    prev_close: F,
) -> (F, F, F) {
    supertrend_arm!(
        generic_op,
        out_return_tuple,
        out_tuple,
        step_generic,
        atr,
        bands_seeded,
        prev_upper,
        prev_lower,
        prev_supertrend,
        prev_direction,
        atr_period,
        factor,
        true_range_generic(high, low, first, prev_close, true),
        high,
        low,
        close,
        first,
        prev_close
    )
}

/// `range_arm!` over `generic_op`, with the `State::Window` push and the
/// readiness check of `window_value` in front: the extracted `ta.range` arm
/// (both source types).
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn range_arm_generic<F: FloatOps>(ring: &mut Ring, cells: &mut [F], x: F) -> F {
    ring.push_generic(cells, x);
    let ring = *ring;
    let cells: &[F] = cells;
    if !ring.window_ready_generic(cells) {
        return F::nan();
    }
    let count = ring.cap();
    range_arm!(generic_op, get_generic, ring, cells, count)
}

/// `rci_arm!` over `generic_op`, with the `State::Window` push and the
/// readiness check of `window_value` in front: the extracted `ta.rci` arm.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn rci_arm_generic<F: FloatOps>(
    ring: &mut Ring,
    cells: &mut [F],
    scratch: &mut [F],
    x: F,
) -> F {
    ring.push_generic(cells, x);
    let ring = *ring;
    let cells: &[F] = cells;
    if !ring.window_ready_generic(cells) {
        return F::nan();
    }
    let count = ring.cap();
    let n = F::from_u32(count);
    rci_arm!(
        generic_op,
        sorted_window_generic,
        tie_block_generic,
        get_generic,
        ring,
        cells,
        scratch,
        tie_at_call,
        count,
        n
    )
}

/// `tie_at!` over the generic tie-block search: the lookup `rci_arm!`'s
/// twin calls once per cell.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn tie_at_generic<F: FloatOps>(scratch: &[F], len: usize, x: F) -> (usize, usize) {
    tie_at!(tie_block_generic, scratch, len, x)
}

/// `weighted_mean_last`'s body over an operation dictionary: the linearly
/// weighted mean of the newest `k` cells (oldest weight 1, newest weight
/// `k`). `$get` is the ring read the dictionary pairs with.
macro_rules! weighted_mean_last_fn {
    ($op:ident, $f:ty, $get:ident, $(#[$m:meta])* $vis:vis fn $name:ident [$($g:tt)*]) => {
        $(#[$m])*
        $vis fn $name<$($g)*>(ring: Ring, cells: &[$f], k: u32) -> $f {
            let n = $op!(from_u32 k);
            let mut sum = $op!(zero);
            let start = ring.len() - k;
            let mut i = 0u32;
            while i < k {
                $op!(add_assign sum, $op!(mul $op!(add $op!(from_u32 i), $op!(one)), ring.$get(cells, start + i)));
                i += 1;
            }
            $op!(div sum, $op!(div $op!(mul n, $op!(add n, $op!(one))), $op!(two)))
        }
    };
}

weighted_mean_last_fn!(
    f64_op,
    f64,
    get,
    /// Linearly weighted mean of the newest `k` cells (oldest weight 1, newest
    /// weight `k`), the shared core of `ta.wma` and `ta.hma`.
    fn weighted_mean_last[]
);

#[cfg(any(test, feature = "aeneas"))]
weighted_mean_last_fn!(
    generic_op,
    F,
    get_generic,
    /// `weighted_mean_last_fn!` over `generic_op`.
    pub(crate) fn weighted_mean_last_generic[F: FloatOps]
);

/// A shipped arm's final tuple result: an [`Out`] (the output
/// constructor hook; the generic twins use `out_tuple!`).
macro_rules! out_value {
    ($value:expr, $a:expr, $b:expr) => {
        Out {
            value: $value,
            extra: [$a, $b],
        }
    };
    // `$value` with both extra outputs na.
    ($value:expr; na) => {
        Out {
            value: $value,
            extra: [f64::NAN; 2],
        }
    };
}

/// A shipped arm's built [`Out`], returned from `advance_state` (the generic
/// twins use `value_return!`).
macro_rules! out_some_return {
    ($out:expr) => {
        return Some($out)
    };
}

/// The `State::WindowPair` arm (`ta.vwma(source, length)`): push the
/// (source, volume) pair; once the window is ready, the volume-weighted mean
/// `Σ source·volume / Σ volume`. `$volume` is the raw volume read (the arm
/// applies `finite`); `$push`/`$ready`/`$get` are the pair-ring methods the
/// dictionary pairs with.
macro_rules! vwma_arm {
    (
        $op:ident, $push:ident, $ready:ident, $get:ident, $ring:expr, $cells:expr,
        $x:expr, $volume:expr
    ) => {{
        $ring.$push($cells, $x, $op!(finite $volume));
        if $ring.$ready($cells) {
            let mut weighted = $op!(zero);
            let mut volumes = $op!(zero);
            let mut i = 0u32;
            while i < $ring.cap() {
                let (s, v) = $ring.$get($cells, i);
                $op!(add_assign weighted, $op!(mul s, v));
                $op!(add_assign volumes, v);
                i += 1;
            }
            $op!(norm $op!(div weighted, volumes))
        } else {
            $op!(nan)
        }
    }};
}

/// `ta.vwma` is the ratio of two independent SMAs.
/// A missing source suppresses its weighted product, not the volume input.
fn vwma_independent(
    weighted: &mut Ring,
    volumes: &mut Ring,
    cells: &mut [f64],
    src: f64,
    volume: f64,
) -> f64 {
    let volume = finite(volume);
    let product = norm(src * volume);
    if present(product) {
        weighted.push(cells, product);
    }
    if present(volume) {
        volumes.push(cells, volume);
    }
    if weighted.window_ready(cells) && volumes.window_ready(cells) {
        let numerator = norm(window_sum(*weighted, cells) / f64::from(weighted.cap()));
        let denominator = norm(window_sum(*volumes, cells) / f64::from(volumes.cap()));
        norm(numerator / denominator)
    } else {
        f64::NAN
    }
}

/// The `State::Hma` arm (`ta.hma(source, length)`): push the source; the
/// inner series `2·wma(source, length/2) − wma(source, length)` (na unless
/// both windows are ready and the value finite) is pushed to the second
/// ring, whose full-window wma is the output. `$push`/`$ready`/`$last_ready`
/// are the ring methods and `$wml` the weighted mean the dictionary pairs
/// with.
macro_rules! hma_arm {
    (
        $op:ident, $push:ident, $ready:ident, $last_ready:ident, $wml:ident, $src:expr,
        $inter:expr, $cells:expr, $x:expr
    ) => {{
        $src.$push($cells, $x);
        let half = ($src.cap() / 2).max(1);
        let inter_value = if $src.$last_ready($cells, half) && $src.$ready($cells) {
            let v = $op!(sub
                $op!(mul $op!(two), $wml(*$src, $cells, half)),
                $wml(*$src, $cells, $src.cap()));
            if $op!(is_finite v) {
                v
            } else {
                $op!(nan)
            }
        } else {
            $op!(nan)
        };
        $inter.$push($cells, inter_value);
        if $inter.$ready($cells) {
            $op!(norm $wml(*$inter, $cells, $inter.cap()))
        } else {
            $op!(nan)
        }
    }};
}

/// The `State::Dmi` arm (`ta.dmi(diLength, adxSmoothing)`):
/// `(+DI, −DI, ADX)` over the rma (alpha `1 / di_len`). `$high`/`$low`
/// are the raw reads (the arm applies `finite`) and `$tr_now` the true range
/// (`handle_na = false` form), evaluated only when both bars are present.
/// `$mk` builds the output, `$ret` returns it; `$step` is the smoother
/// method the dictionary pairs with.
macro_rules! dmi_arm {
    (
        $op:ident, $mk:ident, $ret:ident, $step:ident, $prev_high:expr, $prev_low:expr,
        $tr:expr, $plus_dm:expr, $minus_dm:expr, $adx:expr, $di_len:expr, $adx_len:expr,
        $high:expr, $low:expr, $tr_now:expr
    ) => {{
        let di_len = $di_len;
        let adx_len = $adx_len;
        let alpha = rma_alpha!($op, di_len); // the rma smoothing.
        let high = $op!(finite $high);
        let low = $op!(finite $low);
        let mut out = $mk!($op!(nan); na);
        if $op!(present high)
            && $op!(present low)
            && $op!(present *$prev_high)
            && $op!(present *$prev_low)
        {
            let up_move = $op!(sub high, *$prev_high);
            let down_move = $op!(sub *$prev_low, low);
            let plus = if $op!(gt up_move, down_move) && $op!(gt up_move, $op!(zero)) {
                up_move
            } else {
                $op!(zero)
            };
            let minus = if $op!(gt down_move, up_move) && $op!(gt down_move, $op!(zero)) {
                down_move
            } else {
                $op!(zero)
            };
            let tr_v = $tr_now;
            if $op!(present tr_v) {
                let str_ = $tr.$step(tr_v, di_len, alpha);
                let splus = $plus_dm.$step(plus, di_len, alpha);
                let sminus = $minus_dm.$step(minus, di_len, alpha);
                if $op!(present str_)
                    && $op!(present splus)
                    && $op!(present sminus)
                    && $op!(gt str_, $op!(zero))
                {
                    let plus_di = $op!(div $op!(mul $op!(hundred), splus), str_);
                    let minus_di = $op!(div $op!(mul $op!(hundred), sminus), str_);
                    let di_sum = $op!(add plus_di, minus_di);
                    // A zero DI sum makes DX na (leaves ADX untouched,
                    // like an na smoother input).
                    let adx_v = if $op!(gt di_sum, $op!(zero)) {
                        let dx = $op!(div
                            $op!(mul $op!(hundred), $op!(abs $op!(sub plus_di, minus_di))),
                            di_sum);
                        $adx.$step(dx, adx_len, alpha)
                    } else {
                        $op!(nan)
                    };
                    out = $mk!($op!(norm plus_di), $op!(norm minus_di), $op!(norm adx_v));
                }
            }
        }
        *$prev_high = high;
        *$prev_low = low;
        $ret!(out);
    }};
}

/// The `ta.rising` / `ta.falling` case of `window_value` after the push:
/// bool-valued, plain false (never na) during warm-up or with an na in the
/// window; otherwise whether the newest cell is above (`$rising`) or below
/// every earlier one. Returns from the enclosing function. `$ready`/`$get`
/// are the ring methods the dictionary pairs with.
macro_rules! rising_arm {
    ($op:ident, $ready:ident, $get:ident, $ring:expr, $cells:expr, $rising:expr) => {{
        if !$ring.$ready($cells) {
            return $op!(zero);
        }
        let last = $ring.cap() - 1;
        let current = $ring.$get($cells, last);
        let mut ok = true;
        let mut i = 0u32;
        while i < last {
            let prev = $ring.$get($cells, i);
            let good = if $rising {
                $op!(gt current, prev)
            } else {
                $op!(lt current, prev)
            };
            if !good {
                ok = false;
                break;
            }
            i += 1;
        }
        return $op!(bool_out ok);
    }};
}

/// The `ta.mode` case of `window_value` once the window is ready: the most
/// frequent value of the sorted scratch copy; ties resolve to the smallest.
/// `$sorted`/`$at` are the sorted copy and the scratch read the dictionary
/// pairs with.
macro_rules! mode_arm {
    ($op:ident, $sorted:ident, $at:ident, $ring:expr, $cells:expr, $scratch:expr) => {{
        // The most frequent value; ties resolve to the smallest.
        let len = $sorted($ring, $cells, $scratch);
        if len == 0 {
            return $op!(nan);
        }
        let first = $at($scratch, 0);
        let (mut best, mut best_len) = (first, 0usize);
        let (mut run_value, mut run_len) = (first, 0usize);
        let mut i = 0usize;
        while i < len {
            let x = $at($scratch, i);
            if $op!(feq x, run_value) {
                run_len += 1;
            } else {
                run_value = x;
                run_len = 1;
            }
            if run_len > best_len {
                best = run_value;
                best_len = run_len;
            }
            i += 1;
        }
        $op!(norm best)
    }};
}

/// The `ta.percentile_nearest_rank` case of `window_value` once the window
/// is ready: the smallest window value with at least `$p`% of the data at or
/// below it; percentage 0 selects the smallest value. `$p` is the percentage
/// float; `$sorted`/`$at` as for `mode_arm!`.
macro_rules! percentile_nearest_rank_arm {
    (
        $op:ident, $sorted:ident, $at:ident, $ring:expr, $cells:expr, $scratch:expr,
        $p:expr
    ) => {{
        // Nearest rank: the smallest window value with at least p% of the
        // data at or below it; percentage 0 selects the smallest value.
        let p = $p;
        let len = $sorted($ring, $cells, $scratch);
        if len == 0 {
            return $op!(nan);
        }
        let rank = $op!(to_usize $op!(ceil $op!(mul $op!(div p, $op!(hundred)), $op!(from_usize len))));
        $op!(norm $at($scratch, rank.max(1) - 1))
    }};
}

/// The `ta.mom` / `ta.roc` case of `window_value` after the push: only the
/// window's endpoints are read, so na cells in between do not poison. `$mom`
/// selects `newest − oldest` over `100·(newest − oldest)/oldest`. Returns
/// from the enclosing function. `$get`/`$last` are the ring reads the
/// dictionary pairs with.
macro_rules! mom_roc_arm {
    ($op:ident, $get:ident, $last:ident, $ring:expr, $cells:expr, $mom:expr) => {{
        if !$ring.full() {
            return $op!(nan);
        }
        let oldest = $ring.$get($cells, 0);
        let newest = $ring.$last($cells);
        if !$op!(present oldest) || !$op!(present newest) {
            return $op!(nan);
        }
        return if $mom {
            $op!(norm $op!(sub newest, oldest))
        } else {
            $op!(norm $op!(div $op!(mul $op!(hundred), $op!(sub newest, oldest)), oldest))
        };
    }};
}

/// The `ta.wma` case of `window_value` once the window is ready: the
/// linearly weighted mean of the whole window. `$wml` is the weighted mean
/// the dictionary pairs with.
macro_rules! wma_arm {
    ($op:ident, $wml:ident, $ring:expr, $cells:expr, $count:expr) => {
        $op!(norm $wml($ring, $cells, $count))
    };
}

/// The `ta.cci` case of `window_value` once the window is ready:
/// `(source − sma) / (0.015 · mean absolute deviation)`; a flat window
/// divides by 0 → na. `$get`/`$last`/`$window_sum` are the ring reads and
/// window sum the dictionary pairs with; `$count`/`$n` the window length as
/// `u32` and float.
macro_rules! cci_arm {
    (
        $op:ident, $get:ident, $last:ident, $window_sum:ident, $ring:expr, $cells:expr,
        $count:expr, $n:expr
    ) => {{
        // (source - sma) / (0.015 * dev); a flat window divides by 0 → na.
        let mean = $op!(div $window_sum($ring, $cells), $n);
        let mut dev = $op!(zero);
        let mut i = 0u32;
        while i < $count {
            $op!(add_assign dev, $op!(abs $op!(sub $ring.$get($cells, i), mean)));
            i += 1;
        }
        $op!(div_assign dev, $n);
        $op!(norm $op!(div $op!(sub $ring.$last($cells), mean), $op!(mul $op!(lambert), dev)))
    }};
}

/// The `ta.cmo` case of `window_value` once the window is ready:
/// `100·(Σup − Σdown)/(Σup + Σdown)` over the window's bar-to-bar changes;
/// all-flat (0/0) → na. `$get` is the ring read the dictionary pairs with.
macro_rules! cmo_arm {
    ($op:ident, $get:ident, $ring:expr, $cells:expr, $count:expr) => {{
        // 100 * (sum_up - sum_down) / (sum_up + sum_down) over the
        // window's `length` bar-to-bar changes; all-flat (0/0) → na.
        let (mut up, mut down) = ($op!(zero), $op!(zero));
        let mut i = 1u32;
        while i < $count {
            let change = $op!(sub $ring.$get($cells, i), $ring.$get($cells, i - 1));
            $op!(add_assign up, $op!(max_raw change, $op!(zero)));
            $op!(add_assign down, $op!(max_raw $op!(neg change), $op!(zero)));
            i += 1;
        }
        $op!(norm $op!(div $op!(mul $op!(hundred), $op!(sub up, down)), $op!(add up, down)))
    }};
}

/// The `ta.percentile_linear_interpolation` case of `window_value` once the
/// window is ready: linear interpolation between the two nearest ranks over
/// rank positions `0 ..= n−1` (the inclusive method). `$p` is the percentage
/// float; `$sorted`/`$at` as for `mode_arm!`.
macro_rules! percentile_linear_interpolation_arm {
    (
        $op:ident, $sorted:ident, $at:ident, $ring:expr, $cells:expr, $scratch:expr,
        $p:expr
    ) => {{
        // Linear interpolation between the two nearest ranks over rank
        // positions 0 ..= n-1 (the inclusive method).
        let p = $p;
        let len = $sorted($ring, $cells, $scratch);
        if len == 0 {
            return $op!(nan);
        }
        let rank = $op!(mul $op!(div p, $op!(hundred)), $op!(from_usize (len - 1)));
        let lo = $op!(to_usize $op!(floor rank));
        let hi = $op!(to_usize $op!(ceil rank));
        let frac = $op!(sub rank, $op!(from_usize lo));
        $op!(norm $op!(add
            $at($scratch, lo),
            $op!(mul frac, $op!(sub $at($scratch, hi), $at($scratch, lo)))))
    }};
}

/// The `State::Corr` arm (`ta.correlation(source1, source2, length)`): push
/// the pair; once the window is ready, the Pearson correlation computed
/// mean-first. `$push`/`$ready`/`$get` are the pair-ring methods the
/// dictionary pairs with.
macro_rules! correlation_arm {
    (
        $op:ident, $push:ident, $ready:ident, $get:ident, $ring:expr, $cells:expr,
        $a:expr, $b:expr
    ) => {{
        $ring.$push($cells, $a, $b);
        if $ring.$ready($cells) {
            // Pearson correlation, computed mean-first to limit float
            // cancellation. A zero variance on either side is 0/0 → na.
            let n = $op!(from_u32 $ring.cap());
            let (mut mean_a, mut mean_b) = ($op!(zero), $op!(zero));
            let mut i = 0u32;
            while i < $ring.cap() {
                let (a, b) = $ring.$get($cells, i);
                $op!(add_assign mean_a, a);
                $op!(add_assign mean_b, b);
                i += 1;
            }
            $op!(div_assign mean_a, n);
            $op!(div_assign mean_b, n);
            let (mut cov, mut var_a, mut var_b) = ($op!(zero), $op!(zero), $op!(zero));
            i = 0;
            while i < $ring.cap() {
                let (a, b) = $ring.$get($cells, i);
                $op!(add_assign cov, $op!(mul $op!(sub a, mean_a), $op!(sub b, mean_b)));
                $op!(add_assign var_a, $op!(mul $op!(sub a, mean_a), $op!(sub a, mean_a)));
                $op!(add_assign var_b, $op!(mul $op!(sub b, mean_b), $op!(sub b, mean_b)));
                i += 1;
            }
            $op!(norm $op!(div cov, $op!(mul $op!(sqrt var_a), $op!(sqrt var_b))))
        } else {
            $op!(nan)
        }
    }};
}

/// The `State::Wpr` arm (`ta.wpr(length)`): push the high and low; once both
/// windows are ready and the close present,
/// `100·(close − highest)/(highest − lowest)`. `$high`/`$low`/`$close` are
/// the raw reads (the arm applies `finite`); `$push`/`$ready`/`$get` are the
/// ring methods the dictionary pairs with.
macro_rules! wpr_arm {
    (
        $op:ident, $push:ident, $ready:ident, $get:ident, $highs:expr, $lows:expr,
        $cells:expr, $high:expr, $low:expr, $close:expr
    ) => {{
        $highs.$push($cells, $op!(finite $high));
        $lows.$push($cells, $op!(finite $low));
        let close = $op!(finite $close);
        if $highs.$ready($cells) && $lows.$ready($cells) && $op!(present close) {
            let mut hh = $op!(lowest);
            let mut ll = $op!(highest);
            let mut i = 0u32;
            while i < $highs.cap() {
                hh = $op!(max_raw hh, $highs.$get($cells, i));
                i += 1;
            }
            i = 0;
            while i < $lows.cap() {
                ll = $op!(min_raw ll, $lows.$get($cells, i));
                i += 1;
            }
            // Window extrema never return `-0.0`: a zero highest high is
            // `+0.0` on every target. (A zero `ll` needs no such step: it
            // only enters `hh - ll`, which is `hh` when `hh` is not zero
            // and a zero, so the quotient is na, when `hh` is zero too.)
            let hh = $op!(canon_zero hh);
            // 100 * (close - highest(high)) / (highest - lowest).
            $op!(norm $op!(div $op!(mul $op!(hundred), $op!(sub close, hh)), $op!(sub hh, ll)))
        } else {
            $op!(nan)
        }
    }};
}

/// The `ta.math_sum` case of `window_value` once the window is ready.
/// `$window_sum` is the window sum the dictionary pairs with.
macro_rules! math_sum_arm {
    ($op:ident, $window_sum:ident, $ring:expr, $cells:expr) => {
        $op!(norm $window_sum($ring, $cells))
    };
}

/// The `ta.swma` case of `window_value` once the (four-cell) window is
/// ready: weights 1, 2, 2, 1 over 6. `$at`/`$get` are the slice and ring
/// reads the dictionary pairs with.
macro_rules! swma_arm {
    ($op:ident, $at:ident, $get:ident, $ring:expr, $cells:expr, $count:expr) => {{
        // x[3]*1/6 + x[2]*2/6 + x[1]*2/6 + x[0]*1/6 (oldest → newest).
        let weights = [$op!(one), $op!(two), $op!(two), $op!(one)];
        let mut sum = $op!(zero);
        let mut i = 0u32;
        while i < $count && (i as usize) < weights.len() {
            $op!(add_assign sum, $op!(mul $at(&weights, i as usize), $ring.$get($cells, i)));
            i += 1;
        }
        $op!(norm $op!(div sum, $op!(six)))
    }};
}

/// The `ta.cog` case of `window_value` once the window is ready:
/// `−Σ source[i]·(i+1) / Σ source[i]` with `i = 0` the newest bar; a zero
/// window sum divides by zero → na. `$get`/`$window_sum` are the ring read
/// and window sum the dictionary pairs with.
macro_rules! cog_arm {
    ($op:ident, $get:ident, $window_sum:ident, $ring:expr, $cells:expr, $count:expr) => {{
        // -Σ source[i]·(i+1) / Σ source[i], where i = 0 is the newest bar;
        // a zero window sum divides by zero → na.
        let sum = $window_sum($ring, $cells);
        let mut num = $op!(zero);
        let mut j = 0u32;
        while j < $count {
            // Window is oldest → newest; the newest cell gets weight 1.
            $op!(add_assign num, $op!(mul $ring.$get($cells, j), $op!(from_u32 $count - j)));
            j += 1;
        }
        $op!(norm $op!(div $op!(neg num), sum))
    }};
}

/// The `ta.percentrank` case of `window_value` once the window is ready:
/// the percent of the `length` previous values `<=` the current one (the
/// window holds `length + 1` cells). `$get` is the ring read the dictionary
/// pairs with.
macro_rules! percent_rank_arm {
    ($op:ident, $get:ident, $ring:expr, $cells:expr, $count:expr) => {{
        // Percent of the `length` previous values <= the current value
        // (the window holds length + 1 cells).
        let last = $count - 1;
        let current = $ring.$get($cells, last);
        let mut hits = 0u32;
        let mut i = 0u32;
        while i < last {
            if $op!(le $ring.$get($cells, i), current) {
                hits += 1;
            }
            i += 1;
        }
        $op!(norm $op!(div $op!(mul $op!(hundred), $op!(from_u32 hits)), $op!(from_u32 last)))
    }};
}

/// The `ta.wvad` case of the `State::Stateless` arm:
/// `(close − open)/(high − low)·volume` per bar, na when any read is.
/// `$o`/`$h`/`$l`/`$c`/`$v` are the raw reads (the arm applies `finite`).
macro_rules! wvad_arm {
    ($op:ident, $o:expr, $h:expr, $l:expr, $c:expr, $v:expr) => {{
        let o = $op!(finite $o);
        let h = $op!(finite $h);
        let l = $op!(finite $l);
        let c = $op!(finite $c);
        let v = $op!(finite $v);
        if $op!(present o) && $op!(present h) && $op!(present l) && $op!(present c) && $op!(present v)
        {
            $op!(norm $op!(mul $op!(div $op!(sub c, o), $op!(sub h, l)), v))
        } else {
            $op!(nan)
        }
    }};
}

/// The `ta.iii` case of the `State::Stateless` arm:
/// `(2·close − high − low)/((high − low)·volume)` per bar, na when any read
/// is. `$h`/`$l`/`$c`/`$v` are the raw reads (the arm applies `finite`).
macro_rules! iii_arm {
    ($op:ident, $h:expr, $l:expr, $c:expr, $v:expr) => {{
        let h = $op!(finite $h);
        let l = $op!(finite $l);
        let c = $op!(finite $c);
        let v = $op!(finite $v);
        if $op!(present h) && $op!(present l) && $op!(present c) && $op!(present v) {
            $op!(norm $op!(div
                $op!(sub $op!(sub $op!(mul $op!(two), c), h), l),
                $op!(mul $op!(sub h, l), v)))
        } else {
            $op!(nan)
        }
    }};
}

/// The `State::Mfi` arm (`ta.mfi(source, length)`): the bar's up/down money
/// flows from the source change, pushed as a pair; once the window is
/// ready, `100 − 100/(1 + Σup/Σdown)`. `$prev` is the previous source
/// (place), `$source` the source operand, `$volume` the raw volume read (the
/// arm applies `finite`); `$push`/`$ready`/`$get` are the pair-ring methods
/// the dictionary pairs with.
macro_rules! mfi_arm {
    (
        $op:ident, $push:ident, $ready:ident, $get:ident, $prev:expr, $ring:expr, $cells:expr,
        $source:expr, $volume:expr
    ) => {{
        let source = $source;
        let volume = $op!(finite $volume);
        let (up, down) = if $op!(present source) && $op!(present volume) {
            // Money flows, split by the sign of the source change:
            // `volume * (change <= 0 ? 0 : src)` and
            // `volume * (change >= 0 ? 0 : src)`; an **na** change (first
            // bar, or after an na source) takes both else branches,
            // contributing `volume * src` to both flows.
            let change = if $op!(present *$prev) {
                $op!(sub source, *$prev)
            } else {
                $op!(nan)
            };
            let up = if $op!(present change) && $op!(le change, $op!(zero)) {
                $op!(zero)
            } else {
                source
            };
            let down = if $op!(present change) && $op!(ge change, $op!(zero)) {
                $op!(zero)
            } else {
                source
            };
            ($op!(mul volume, up), $op!(mul volume, down))
        } else {
            ($op!(nan), $op!(nan))
        };
        *$prev = source;
        $ring.$push($cells, up, down);
        if $ring.$ready($cells) {
            let (mut upper, mut lower) = ($op!(zero), $op!(zero));
            let mut i = 0u32;
            while i < $ring.cap() {
                let (u, d) = $ring.$get($cells, i);
                $op!(add_assign upper, u);
                $op!(add_assign lower, d);
                i += 1;
            }
            // lower == 0 → upper/lower = inf → mfi = 100; 0/0 → na.
            $op!(norm $op!(sub
                $op!(hundred),
                $op!(paren $op!(div $op!(hundred), $op!(add $op!(one), $op!(div upper, lower))))))
        } else {
            $op!(nan)
        }
    }};
}

/// The `State::VolumeIndex` arm (`ta.nvi` / `ta.pvi`): starting at 1, a
/// volume decrease (nvi, `$func` = `TaFunc::Nvi`) or increase (pvi) versus
/// the previous volume applies the close's rate of change; zero or na closes
/// carry the value. `$value`/`$prev_close`/`$prev_volume` are the state
/// places; `$close`/`$volume` the raw reads (the arm applies `finite`).
macro_rules! volume_index_arm {
    (
        $op:ident, $func:expr, $value:expr, $prev_close:expr, $prev_volume:expr, $close:expr,
        $volume:expr
    ) => {{
        let close = $op!(finite $close);
        let volume = $op!(finite $volume);
        // Starts at 1.0. The value is carried unchanged when either close is
        // zero or na; otherwise a volume decrease (nvi) or increase (pvi)
        // against the previous volume applies the close's rate of change.
        if $op!(present close)
            && $op!(present *$prev_close)
            && $op!(fne close, $op!(zero))
            && $op!(fne *$prev_close, $op!(zero))
        {
            let prev_vol = if $op!(present *$prev_volume) {
                *$prev_volume
            } else {
                $op!(zero)
            };
            let trigger = $op!(present volume)
                && match $func {
                    TaFunc::Nvi => $op!(lt volume, prev_vol),
                    TaFunc::Pvi => $op!(gt volume, prev_vol),
                    _ => false,
                };
            if trigger {
                let next = $op!(add
                    *$value,
                    $op!(mul
                        $op!(paren $op!(div $op!(sub close, *$prev_close), *$prev_close)),
                        *$value));
                if $op!(is_finite next) {
                    *$value = next;
                }
            }
        }
        *$prev_close = close;
        *$prev_volume = volume;
        $op!(norm *$value)
    }};
}

/// `mom_roc_arm!` over `generic_op`, with the `State::Window` push in front:
/// the extracted `ta.mom` (`mom`) / `ta.roc` arm.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn mom_roc_arm_generic<F: FloatOps>(
    ring: &mut Ring,
    cells: &mut [F],
    x: F,
    mom: bool,
) -> F {
    ring.push_generic(cells, x);
    let ring = *ring;
    let cells: &[F] = cells;
    mom_roc_arm!(generic_op, get_generic, last_generic, ring, cells, mom)
}

/// `wma_arm!` over `generic_op`, with the `State::Window` push and the
/// readiness check of `window_value` in front: the extracted `ta.wma` arm.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn wma_arm_generic<F: FloatOps>(ring: &mut Ring, cells: &mut [F], x: F) -> F {
    ring.push_generic(cells, x);
    let ring = *ring;
    let cells: &[F] = cells;
    if !ring.window_ready_generic(cells) {
        return F::nan();
    }
    wma_arm!(
        generic_op,
        weighted_mean_last_generic,
        ring,
        cells,
        ring.cap()
    )
}

/// `cci_arm!` over `generic_op`, with the `State::Window` push and the
/// readiness check of `window_value` in front: the extracted `ta.cci` arm.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn cci_arm_generic<F: FloatOps>(ring: &mut Ring, cells: &mut [F], x: F) -> F {
    ring.push_generic(cells, x);
    let ring = *ring;
    let cells: &[F] = cells;
    if !ring.window_ready_generic(cells) {
        return F::nan();
    }
    let count = ring.cap();
    let n = F::from_u32(count);
    cci_arm!(
        generic_op,
        get_generic,
        last_generic,
        window_sum_generic,
        ring,
        cells,
        count,
        n
    )
}

/// `cmo_arm!` over `generic_op`, with the `State::Window` push and the
/// readiness check of `window_value` in front: the extracted `ta.cmo` arm.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn cmo_arm_generic<F: FloatOps>(ring: &mut Ring, cells: &mut [F], x: F) -> F {
    ring.push_generic(cells, x);
    let ring = *ring;
    let cells: &[F] = cells;
    if !ring.window_ready_generic(cells) {
        return F::nan();
    }
    cmo_arm!(generic_op, get_generic, ring, cells, ring.cap())
}

/// `percentile_linear_interpolation_arm!` over `generic_op`, with the
/// `State::Window` push and the readiness check of `window_value` in front:
/// the extracted `ta.percentile_linear_interpolation` arm (`p` the
/// percentage).
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn percentile_linear_interpolation_arm_generic<F: FloatOps>(
    ring: &mut Ring,
    cells: &mut [F],
    scratch: &mut [F],
    x: F,
    p: F,
) -> F {
    ring.push_generic(cells, x);
    let ring = *ring;
    let cells: &[F] = cells;
    if !ring.window_ready_generic(cells) {
        return F::nan();
    }
    percentile_linear_interpolation_arm!(
        generic_op,
        sorted_window_generic,
        at_generic,
        ring,
        cells,
        scratch,
        p
    )
}

/// `correlation_arm!` over `generic_op`: the extracted `ta.correlation` arm
/// (`a`, `b` are `arg(0)`, `arg(1)`).
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn correlation_arm_generic<F: FloatOps>(
    ring: &mut PairRing,
    cells: &mut [F],
    a: F,
    b: F,
) -> F {
    correlation_arm!(
        generic_op,
        push_generic,
        ready_generic,
        get_generic,
        ring,
        cells,
        a,
        b
    )
}

/// `wpr_arm!` over `generic_op`: the extracted `ta.wpr` arm (`high`, `low`,
/// `close` the raw reads at the bar).
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn wpr_arm_generic<F: FloatOps>(
    highs: &mut Ring,
    lows: &mut Ring,
    cells: &mut [F],
    high: F,
    low: F,
    close: F,
) -> F {
    wpr_arm!(
        generic_op,
        push_generic,
        window_ready_generic,
        get_generic,
        highs,
        lows,
        cells,
        high,
        low,
        close
    )
}

/// `math_sum_arm!` over `generic_op`, with the `State::Window` push and the
/// readiness check of `window_value` in front: the extracted `ta.math_sum`
/// arm.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn math_sum_arm_generic<F: FloatOps>(ring: &mut Ring, cells: &mut [F], x: F) -> F {
    ring.push_generic(cells, x);
    let ring = *ring;
    let cells: &[F] = cells;
    if !ring.window_ready_generic(cells) {
        return F::nan();
    }
    math_sum_arm!(generic_op, window_sum_generic, ring, cells)
}

/// `swma_arm!` over `generic_op`, with the `State::Window` push and the
/// readiness check of `window_value` in front: the extracted `ta.swma` arm.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn swma_arm_generic<F: FloatOps>(ring: &mut Ring, cells: &mut [F], x: F) -> F {
    ring.push_generic(cells, x);
    let ring = *ring;
    let cells: &[F] = cells;
    if !ring.window_ready_generic(cells) {
        return F::nan();
    }
    swma_arm!(generic_op, at_generic, get_generic, ring, cells, ring.cap())
}

/// `cog_arm!` over `generic_op`, with the `State::Window` push and the
/// readiness check of `window_value` in front: the extracted `ta.cog` arm.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn cog_arm_generic<F: FloatOps>(ring: &mut Ring, cells: &mut [F], x: F) -> F {
    ring.push_generic(cells, x);
    let ring = *ring;
    let cells: &[F] = cells;
    if !ring.window_ready_generic(cells) {
        return F::nan();
    }
    cog_arm!(
        generic_op,
        get_generic,
        window_sum_generic,
        ring,
        cells,
        ring.cap()
    )
}

/// `percent_rank_arm!` over `generic_op`, with the `State::Window` push and
/// the readiness check of `window_value` in front: the extracted
/// `ta.percentrank` arm.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn percent_rank_arm_generic<F: FloatOps>(ring: &mut Ring, cells: &mut [F], x: F) -> F {
    ring.push_generic(cells, x);
    let ring = *ring;
    let cells: &[F] = cells;
    if !ring.window_ready_generic(cells) {
        return F::nan();
    }
    percent_rank_arm!(generic_op, get_generic, ring, cells, ring.cap())
}

/// `wvad_arm!` over `generic_op`: the extracted `ta.wvad` arm (the raw
/// `open`/`high`/`low`/`close`/`volume` reads at the bar).
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn wvad_arm_generic<F: FloatOps>(o: F, h: F, l: F, c: F, v: F) -> F {
    wvad_arm!(generic_op, o, h, l, c, v)
}

/// `iii_arm!` over `generic_op`: the extracted `ta.iii` arm (the raw
/// `high`/`low`/`close`/`volume` reads at the bar).
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn iii_arm_generic<F: FloatOps>(h: F, l: F, c: F, v: F) -> F {
    iii_arm!(generic_op, h, l, c, v)
}

/// `mfi_arm!` over `generic_op`: the extracted `ta.mfi` arm (`source` is
/// `arg(0)`, `volume` the raw `volume(bar)` read).
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn mfi_arm_generic<F: FloatOps>(
    prev: &mut F,
    ring: &mut PairRing,
    cells: &mut [F],
    source: F,
    volume: F,
) -> F {
    mfi_arm!(
        generic_op,
        push_generic,
        ready_generic,
        get_generic,
        prev,
        ring,
        cells,
        source,
        volume
    )
}

/// `volume_index_arm!` over `generic_op`: the extracted `ta.nvi` / `ta.pvi`
/// arm (`close`/`volume` the raw reads at the bar).
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn volume_index_arm_generic<F: FloatOps>(
    ta_func: TaFunc,
    value: &mut F,
    prev_close: &mut F,
    prev_volume: &mut F,
    close: F,
    volume: F,
) -> F {
    volume_index_arm!(
        generic_op,
        ta_func,
        value,
        prev_close,
        prev_volume,
        close,
        volume
    )
}

/// `vwma_arm!` over `generic_op`: the extracted `ta.vwma` arm (`x` is
/// `arg(0)`, `volume` the raw `volume(bar)` read).
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn vwma_arm_generic<F: FloatOps>(
    ring: &mut PairRing,
    cells: &mut [F],
    x: F,
    volume: F,
) -> F {
    vwma_arm!(
        generic_op,
        push_generic,
        ready_generic,
        get_generic,
        ring,
        cells,
        x,
        volume
    )
}

/// `hma_arm!` over `generic_op`: the extracted `ta.hma` arm.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn hma_arm_generic<F: FloatOps>(
    src: &mut Ring,
    inter: &mut Ring,
    cells: &mut [F],
    x: F,
) -> F {
    hma_arm!(
        generic_op,
        push_generic,
        window_ready_generic,
        last_window_ready_generic,
        weighted_mean_last_generic,
        src,
        inter,
        cells,
        x
    )
}

/// `dmi_arm!` over `generic_op`: the extracted `ta.dmi` arm,
/// `(+DI, −DI, ADX)`. `di_len`, `adx_len` are the clamped lengths; `high`,
/// `low` the raw reads at the bar and `prev_close` the raw `close(bar - 1)`
/// (ignored when `first`).
#[cfg(any(test, feature = "aeneas"))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn dmi_arm_generic<F: FloatOps>(
    prev_high: &mut F,
    prev_low: &mut F,
    tr: &mut Smoother<F>,
    plus_dm: &mut Smoother<F>,
    minus_dm: &mut Smoother<F>,
    adx: &mut Smoother<F>,
    di_len: u32,
    adx_len: u32,
    high: F,
    low: F,
    first: bool,
    prev_close: F,
) -> (F, F, F) {
    dmi_arm!(
        generic_op,
        out_tuple,
        value_return,
        step_generic,
        prev_high,
        prev_low,
        tr,
        plus_dm,
        minus_dm,
        adx,
        di_len,
        adx_len,
        high,
        low,
        true_range_generic(high, low, first, prev_close, false)
    )
}

/// `rising_arm!` over `generic_op`, with the `State::Window` push in front:
/// the extracted `ta.rising` (`rising`) / `ta.falling` arm.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn rising_arm_generic<F: FloatOps>(
    ring: &mut Ring,
    cells: &mut [F],
    x: F,
    rising: bool,
) -> F {
    ring.push_generic(cells, x);
    let ring = *ring;
    let cells: &[F] = cells;
    rising_arm!(
        generic_op,
        window_ready_generic,
        get_generic,
        ring,
        cells,
        rising
    )
}

/// `mode_arm!` over `generic_op`, with the `State::Window` push and the
/// readiness check of `window_value` in front: the extracted `ta.mode` arm
/// (both source types).
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn mode_arm_generic<F: FloatOps>(
    ring: &mut Ring,
    cells: &mut [F],
    scratch: &mut [F],
    x: F,
) -> F {
    ring.push_generic(cells, x);
    let ring = *ring;
    let cells: &[F] = cells;
    if !ring.window_ready_generic(cells) {
        return F::nan();
    }
    mode_arm!(
        generic_op,
        sorted_window_generic,
        at_generic,
        ring,
        cells,
        scratch
    )
}

/// `percentile_nearest_rank_arm!` over `generic_op`, with the
/// `State::Window` push and the readiness check of `window_value` in front:
/// the extracted `ta.percentile_nearest_rank` arm (`p` the percentage).
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn percentile_nearest_rank_arm_generic<F: FloatOps>(
    ring: &mut Ring,
    cells: &mut [F],
    scratch: &mut [F],
    x: F,
    p: F,
) -> F {
    ring.push_generic(cells, x);
    let ring = *ring;
    let cells: &[F] = cells;
    if !ring.window_ready_generic(cells) {
        return F::nan();
    }
    percentile_nearest_rank_arm!(
        generic_op,
        sorted_window_generic,
        at_generic,
        ring,
        cells,
        scratch,
        p
    )
}

/// The scalar `ta.highest`/`ta.lowest` fold of `window_value` over an
/// operation dictionary: na until the window is full or when the newest
/// cell is na; otherwise the ordered (oldest to newest) extremum of the
/// present cells, earlier na cells skipped. `have` says whether `best`
/// holds a cell yet. A zero extremum is returned as `+0.0` whatever the
/// signs of the window's zeros: `f64::max`/`f64::min` leave the sign of a
/// zero result to the target, and window extrema never return `-0.0`.
/// `$get`/`$last` are the ring reads the dictionary pairs with.
macro_rules! extremum_fold_fn {
    (
        $op:ident, $f:ty, $get:ident, $last:ident,
        $(#[$m:meta])* $vis:vis fn $name:ident [$($g:tt)*]
    ) => {
        $(#[$m])*
        $vis fn $name<$($g)*>(ring: Ring, cells: &[$f], highest: bool) -> $f {
            if !ring.full() || $op!(is_nan ring.$last(cells)) {
                return $op!(nan);
            }
            let mut have = false;
            let mut best = $op!(nan);
            let mut i = 0u32;
            while i < ring.cap() {
                let value = ring.$get(cells, i);
                if !$op!(is_nan value) {
                    best = if !have {
                        value
                    } else if highest {
                        $op!(max_raw best, value)
                    } else {
                        $op!(min_raw best, value)
                    };
                    have = true;
                }
                i += 1;
            }
            if have {
                // Window extrema never return `-0.0`: a zero extremum is
                // `+0.0` on every target.
                $op!(norm $op!(canon_zero best))
            } else {
                $op!(nan)
            }
        }
    };
}

/// What the `State::WindowExtremum` arm returns for a zero extremum:
/// `+0.0`. Window extrema never return `-0.0`, so the cache's answer needs
/// no ordered fold behind it. (Under `cfg(test)` an ordered fold can be
/// re-enabled for comparison.)
#[inline]
#[cfg_attr(not(test), allow(unused_variables))]
fn long_window_zero(ring: Ring, cells: &[f64], highest: bool) -> f64 {
    #[cfg(test)]
    {
        if crate::state::tie_policy::plain_max_min() {
            return extremum_fold(ring, cells, highest);
        }
    }
    0.0
}

extremum_fold_fn!(
    f64_op,
    f64,
    get,
    last,
    /// The scalar `ta.highest` (`highest`) / `ta.lowest` fold.
    #[inline]
    fn extremum_fold[]
);

#[cfg(any(test, feature = "aeneas"))]
extremum_fold_fn!(
    generic_op,
    F,
    get_generic,
    last_generic,
    /// `extremum_fold_fn!` over `generic_op`.
    pub(crate) fn extremum_fold_generic[F: FloatOps]
);

/// The `ta.highestbars`/`ta.lowestbars` case of `window_value` once the
/// window is ready (`count` cells) over an operation dictionary: the offset
/// (≤ 0) of the best bar, ties resolving to the most recent. `best_value`
/// is always the cell at `best`; cell 0 is the first incumbent, so the scan
/// starts at cell 1. `$get` is the ring read the dictionary pairs with.
macro_rules! extremum_bars_fn {
    ($op:ident, $f:ty, $get:ident, $(#[$m:meta])* $vis:vis fn $name:ident [$($g:tt)*]) => {
        $(#[$m])*
        $vis fn $name<$($g)*>(ring: Ring, cells: &[$f], count: u32, highest: bool) -> $f {
            let mut best = 0u32;
            let mut best_value = ring.$get(cells, 0);
            let mut i = 1u32;
            while i < count {
                let x = ring.$get(cells, i);
                let better = if highest {
                    $op!(ge x, best_value)
                } else {
                    $op!(le x, best_value)
                };
                if better {
                    best = i;
                    best_value = x;
                }
                i += 1;
            }
            $op!(neg $op!(from_u32 count - 1 - best))
        }
    };
}

extremum_bars_fn!(
    f64_op,
    f64,
    get,
    /// The `ta.highestbars` (`highest`) / `ta.lowestbars` offset.
    #[inline]
    fn extremum_bars[]
);

#[cfg(any(test, feature = "aeneas"))]
extremum_bars_fn!(
    generic_op,
    F,
    get_generic,
    /// `extremum_bars_fn!` over `generic_op`.
    pub(crate) fn extremum_bars_generic[F: FloatOps]
);

/// `extremum_fold_fn!` over `generic_op`, with the `State::Window` push in
/// front: the extracted short-window `ta.highest` (`highest`) / `ta.lowest`
/// arm.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn extremum_short_arm_generic<F: FloatOps>(
    ring: &mut Ring,
    cells: &mut [F],
    x: F,
    highest: bool,
) -> F {
    ring.push_generic(cells, x);
    extremum_fold_generic(*ring, cells, highest)
}

/// `extremum_bars_fn!` over `generic_op`, with the `State::Window` push and
/// the readiness check of `window_value` in front: the extracted
/// short-window `ta.highestbars` (`highest`) / `ta.lowestbars` arm.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn extremum_bars_short_arm_generic<F: FloatOps>(
    ring: &mut Ring,
    cells: &mut [F],
    x: F,
    highest: bool,
) -> F {
    ring.push_generic(cells, x);
    let ring = *ring;
    let cells: &[F] = cells;
    if !ring.window_ready_generic(cells) {
        return F::nan();
    }
    extremum_bars_generic(ring, cells, ring.cap(), highest)
}

/// `window_extremum_arm!` over `generic_op` with the zero-extremum fallback
/// in place (`extremum_fold_generic` over the window's ring, evaluated only
/// in that branch): the extracted long-window `ta.highest` / `ta.lowest` /
/// `ta.highestbars` / `ta.lowestbars` arm.
#[cfg(any(test, feature = "aeneas"))]
pub(crate) fn window_extremum_arm_generic<F: FloatOps>(
    window: &mut crate::arena::ExtremumWindow,
    cells: &mut [F],
    x: F,
    highest: bool,
    bars: bool,
) -> F {
    window_extremum_arm!(
        generic_op,
        window,
        cells,
        x,
        highest,
        bars,
        // A zero extremum is `+0.0` (window extrema never return `-0.0`),
        // so the cache's answer needs no ordered fold behind it.
        F::zero(),
        push_generic,
        best_generic,
        best_present_generic,
        offset_generic
    )
}

window_sum_fn!(
    f64_op,
    f64,
    get,
    /// The plain window sum, oldest to newest (`Iterator::sum`'s fold order).
    fn window_sum[]
);

#[cfg(any(test, feature = "aeneas"))]
window_sum_fn!(
    generic_op,
    F,
    get_generic,
    /// `window_sum_fn!` over `generic_op`.
    pub(crate) fn window_sum_generic[F: FloatOps]
);

sorted_window_fn!(
    f64,
    get,
    sort_total,
    /// Copy the (na-free, full) window into `scratch` and sort it ascending.
    /// Returns the number of cells written, which is 0 when the scratch region
    /// is too small — impossible for a planned layout, and na-safe if it ever is.
    fn sorted_window[]
);

#[cfg(any(test, feature = "aeneas"))]
sorted_window_fn!(
    F,
    get_generic,
    sort_total_generic,
    /// `sorted_window_fn!` over the generic ring read and heapsort.
    pub(crate) fn sorted_window_generic[F: FloatOps]
);

at_fn!(
    f64_op,
    f64,
    #[inline]
    fn at[]
);

#[cfg(any(test, feature = "aeneas"))]
at_fn!(
    generic_op,
    F,
    /// `at_fn!` over `generic_op`.
    pub(crate) fn at_generic[F: FloatOps]
);

/// The engine for one program run: one [`State`] per declaration over a
/// shared cell array.
///
/// Both regions are sized by [`crate::Layout::plan`] from the same
/// declarations, so construction cannot under-allocate; a short slice
/// degrades to na rather than panicking.
pub struct Engine<'a> {
    decls: &'a [Decl],
    states: &'a mut [State],
    cells: &'a mut [f64],
    scratch: &'a mut [f64],
}

impl<'a> Engine<'a> {
    /// Bind regions that already hold initialized state — the per-bar path,
    /// where re-initializing would erase the history the engine exists to
    /// carry.
    pub fn bind(
        decls: &'a [Decl],
        states: &'a mut [State],
        cells: &'a mut [f64],
        scratch: &'a mut [f64],
    ) -> Engine<'a> {
        Engine {
            decls,
            states,
            cells,
            scratch,
        }
    }

    /// Bind and initialize every site's state, assigning each its slice of
    /// `cells`. Also the reset path: calling it again on the same regions is
    /// exactly "a fresh run over the same program".
    pub fn init(
        decls: &'a [Decl],
        states: &'a mut [State],
        cells: &'a mut [f64],
        scratch: &'a mut [f64],
    ) -> Engine<'a> {
        let mut engine = Engine::bind(decls, states, cells, scratch);
        engine.reset();
        engine
    }

    /// Re-initialize every site (a fresh run over the same program).
    pub fn reset(&mut self) {
        // Each site's base is the saturating prefix sum of `cells_for`; the
        // reset itself lives in `reset_region` so it can be extracted.
        crate::state::reset_region(self.decls, self.states, self.cells);
        for state in self.states.iter_mut().take(self.decls.len()) {
            if let State::WindowPair { ring } = *state {
                *state = match ring.independent_windows() {
                    Some((weighted, volumes)) => State::VwmaIndependent { weighted, volumes },
                    None => State::Invalid,
                };
            }
        }
    }

    /// Advance site `index` with this bar's operands (in argument order;
    /// unused trailing operands are ignored) and return its value.
    ///
    /// `None` means the declaration is not advanceable — a slot-based
    /// function in the engine table, which the program validator rejects before
    /// a program can reach here.
    pub fn advance<L: Lanes>(
        &mut self,
        index: u32,
        args: [f64; 3],
        lanes: &L,
        bar: u32,
    ) -> Option<Out> {
        let Engine {
            decls,
            states,
            cells,
            scratch,
        } = self;
        let decl = decls.get(index as usize)?;
        let func = decl.func()?;
        let state = states.get_mut(index as usize)?;
        advance_state(func, decl, state, args, lanes, bar, cells, scratch)
    }

    /// The states, for the interpreter's own inspection in tests.
    pub fn states(&self) -> &[State] {
        self.states
    }
}

/// The advance rules proper, split out so [`Engine::advance`] can hand it
/// disjoint borrows of the state and the cell array.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn advance_state<L: Lanes>(
    func: TaFunc,
    decl: &Decl,
    state: &mut State,
    args: [f64; 3],
    lanes: &L,
    bar: u32,
    cells: &mut [f64],
    scratch: &mut [f64],
) -> Option<Out> {
    let arg = |i: usize| args.get(i).copied().unwrap_or(f64::NAN);
    // A bool operand arrives as 0.0/1.0/NaN; an na condition is false
    // (consistent with `if`).
    let cond = |i: usize| arg(i) == 1.0;
    let value = match state {
        State::Invalid => return None,

        State::TimeframeChange { prev_bucket } => {
            timeframe_change_step!(prev_bucket, decl.param(0), lanes.time_ms(bar), changed);
            f64_op!(bool_out changed)
        }

        State::Fixnan { last } => fixnan_arm!(f64_op, last, arg(0)),

        State::Window { ring } => {
            ring.push(cells, arg(0));
            window_value(func, decl, *ring, cells, scratch)
        }

        State::WindowExtremum { window } => window_extremum_arm!(
            f64_op,
            window,
            cells,
            arg(0),
            matches!(func, TaFunc::Highest | TaFunc::HighestBars),
            matches!(func, TaFunc::HighestBars | TaFunc::LowestBars),
            long_window_zero(window.ring, cells, func == TaFunc::Highest),
            push,
            best,
            best_present,
            offset
        ),

        State::WindowPair { ring } => vwma_arm!(
            f64_op,
            push,
            ready,
            get,
            ring,
            cells,
            arg(0),
            lanes.volume(bar)
        ),

        State::VwmaIndependent { weighted, volumes } => {
            vwma_independent(weighted, volumes, cells, arg(0), lanes.volume(bar))
        }

        State::Hma { src, inter } => hma_arm!(
            f64_op,
            push,
            window_ready,
            last_window_ready,
            weighted_mean_last,
            src,
            inter,
            cells,
            arg(0)
        ),

        State::Smooth { s } => {
            let length = length_of(decl);
            smooth_arm!(
                f64_op,
                s,
                step,
                func,
                length,
                arg(0),
                true_range(lanes, bar, true)
            )
        }

        State::Rsi { prev, up, down } => {
            let length = length_of(decl);
            rsi_arm!(f64_op, prev, up, down, step, arg(0), length)
        }

        State::Stateless => match func {
            // ta.wvad: (close - open) / (high - low) * volume, per bar.
            TaFunc::Wvad => wvad_arm!(
                f64_op,
                lanes.open(bar),
                lanes.high(bar),
                lanes.low(bar),
                lanes.close(bar),
                lanes.volume(bar)
            ),
            // ta.iii: (2*close - high - low) / ((high - low) * volume).
            TaFunc::Iii => iii_arm!(
                f64_op,
                lanes.high(bar),
                lanes.low(bar),
                lanes.close(bar),
                lanes.volume(bar)
            ),
            // ta.tr(handle_na): computed from market data, no state.
            _ => {
                let handle_na = decl.param(0).unwrap_or(0) != 0;
                tr_arm!(f64_op, true_range(lanes, bar, handle_na))
            }
        },

        State::Cum { total } => cum_arm!(f64_op, total, arg(0)),

        // ta.max / ta.min: all-time running extremum. An na input is ignored;
        // the result is na until the first non-na input has been seen.
        State::Extremum { value } => {
            extremum_arm!(f64_op, value, *value, arg(0), func == TaFunc::Max)
        }

        State::Cross {
            prev_a,
            prev_b,
            seen,
        } => cross_arm!(f64_op, func, prev_a, prev_b, seen, arg(0), arg(1)),

        State::BarsSince { since } => barssince_arm!(f64_op, since, cond(0)),

        State::ValueWhen { vals } => {
            valuewhen_arm!(f64_op, push, get, vals, cells, cond(0), arg(1))
        }

        State::Corr { ring } => {
            correlation_arm!(f64_op, push, ready, get, ring, cells, arg(0), arg(1))
        }

        State::Stoch { ring } => stoch_arm!(
            f64_op,
            push,
            ready,
            get,
            ring,
            cells,
            arg(0),
            arg(1),
            arg(2)
        ),

        State::Mfi { prev, ring } => mfi_arm!(
            f64_op,
            push,
            ready,
            get,
            prev,
            ring,
            cells,
            arg(0),
            lanes.volume(bar)
        ),

        State::Wpr { highs, lows } => wpr_arm!(
            f64_op,
            push,
            window_ready,
            get,
            highs,
            lows,
            cells,
            lanes.high(bar),
            lanes.low(bar),
            lanes.close(bar)
        ),

        State::Obv { total, prev_close } => obv_arm!(
            f64_op,
            total,
            prev_close,
            lanes.close(bar),
            lanes.volume(bar)
        ),

        State::AccDist { total } => accdist_arm!(
            f64_op,
            total,
            lanes.high(bar),
            lanes.low(bar),
            lanes.close(bar),
            lanes.volume(bar)
        ),

        State::VolumeIndex {
            value,
            prev_close,
            prev_volume,
        } => volume_index_arm!(
            f64_op,
            func,
            value,
            prev_close,
            prev_volume,
            lanes.close(bar),
            lanes.volume(bar)
        ),

        State::Pvt { total, prev_close } => pvt_arm!(
            f64_op,
            total,
            prev_close,
            lanes.close(bar),
            lanes.volume(bar)
        ),

        State::Wad { total, prev_close } => wad_arm!(
            f64_op,
            total,
            prev_close,
            lanes.high(bar),
            lanes.low(bar),
            lanes.close(bar)
        ),

        State::Vwap {
            pv,
            p2v,
            vol,
            active,
        } if func != TaFunc::VwapBands => vwap_arm!(
            f64_op,
            advance_vwap_segment,
            pv,
            p2v,
            vol,
            active,
            vwap_source(func, args, lanes, bar),
            lanes.volume(bar),
            matches!(func, TaFunc::VwapAnchored) && cond(1)
        ),

        State::Sar { s } => sar_arm!(
            f64_op,
            out_scalar_return,
            s,
            f64::from_bits(decl.param(0).unwrap_or(0) as u64),
            f64::from_bits(decl.param(1).unwrap_or(0) as u64),
            f64::from_bits(decl.param(2).unwrap_or(0) as u64),
            lanes.high(bar),
            lanes.low(bar),
            lanes.close(bar)
        ),

        State::Tsi {
            prev,
            pc_long,
            pc_short,
            abs_long,
            abs_short,
        } => tsi_arm!(
            f64_op,
            step,
            prev,
            pc_long,
            pc_short,
            abs_long,
            abs_short,
            arg(0),
            param_len(decl, 0),
            param_len(decl, 1)
        ),

        State::Keltner { basis, range_ema } if func == TaFunc::Kcw => kcw_ratio(
            basis,
            range_ema,
            length_of(decl),
            f64::from_bits(decl.param(1).unwrap_or(0) as u64),
            arg(0),
            bar_span(lanes, bar, decl.param(2).unwrap_or(1) != 0),
        ),

        State::Pivot { ring } => pivot_arm!(
            f64_op,
            push,
            window_ready,
            get,
            ring,
            cells,
            arg(0),
            decl.param(0).unwrap_or(1).clamp(0, 100_000) as u32,
            func == TaFunc::PivotHigh
        ),

        // ------------------------------------------------------------------
        // Tuple-returning functions.
        // ------------------------------------------------------------------
        State::Macd { fast, slow, signal } => macd_arm!(
            f64_op,
            out_return,
            step,
            ema_alpha,
            fast,
            slow,
            signal,
            param_len(decl, 0),
            param_len(decl, 1),
            param_len(decl, 2),
            arg(0)
        ),

        State::Bb { ring } => bb_arm!(
            f64_op,
            out_return,
            push,
            window_ready,
            get,
            window_sum,
            ring,
            cells,
            arg(0),
            length_of(decl),
            f64::from_bits(decl.param(1).unwrap_or(0) as u64)
        ),

        State::Keltner { basis, range_ema } => kc_arm!(
            f64_op,
            out_return,
            step_if,
            ema_alpha,
            basis,
            range_ema,
            length_of(decl),
            f64::from_bits(decl.param(1).unwrap_or(0) as u64),
            use_tr = decl.param(2).unwrap_or(1) != 0,
            bar_span(lanes, bar, use_tr),
            arg(0)
        ),

        State::Dmi {
            prev_high,
            prev_low,
            tr,
            plus_dm,
            minus_dm,
            adx,
        } => dmi_arm!(
            f64_op,
            out_value,
            out_some_return,
            step,
            prev_high,
            prev_low,
            tr,
            plus_dm,
            minus_dm,
            adx,
            param_len(decl, 0),
            param_len(decl, 1),
            lanes.high(bar),
            lanes.low(bar),
            true_range(lanes, bar, false)
        ),

        State::Supertrend {
            atr,
            bands_seeded,
            prev_upper,
            prev_lower,
            prev_supertrend,
            prev_direction,
        } => supertrend_arm!(
            f64_op,
            out_return,
            out_return,
            step,
            atr,
            bands_seeded,
            prev_upper,
            prev_lower,
            prev_supertrend,
            prev_direction,
            param_len(decl, 1),
            f64::from_bits(decl.param(0).unwrap_or(0) as u64),
            true_range(lanes, bar, true),
            lanes.high(bar),
            lanes.low(bar),
            lanes.close(bar),
            bar == 0,
            lanes.close(bar - 1)
        ),

        // The only remaining arm is `ta.vwap(source, anchor, stdev_mult)`.
        State::Vwap {
            pv,
            p2v,
            vol,
            active,
        } => vwap_bands_arm!(
            f64_op,
            out_return,
            advance_vwap_segment,
            pv,
            p2v,
            vol,
            active,
            vwap_source(func, args, lanes, bar),
            lanes.volume(bar),
            cond(1),
            arg(2)
        ),
    };
    Some(Out::scalar(value))
}

/// 2 / (length + 1) — the ema alpha shared by macd and kc.
fn ema_alpha(length: u32) -> f64 {
    2.0 / (f64::from(length) + 1.0)
}

/// Const param `i` as a clamped window length.
fn param_len(decl: &Decl, i: usize) -> u32 {
    decl.param(i).unwrap_or(1).clamp(1, 100_000) as u32
}

step_if_fn!(
    f64_op,
    f64,
    step,
    /// Advance a smoother only for a present input (an na input leaves the
    /// recursion untouched and yields na).
    fn step_if[](Smoother)
);

#[cfg(any(test, feature = "aeneas"))]
step_if_fn!(
    generic_op,
    F,
    step_generic,
    /// `step_if_fn!` over `generic_op`.
    pub(crate) fn step_if_generic[F: FloatOps](Smoother<F>)
);

bar_span_fn!(
    f64_op,
    f64,
    /// `ta.kc`/`ta.kcw`'s bar range: the true range (`handle_na` = false, so bar 0
    /// is na) or `high − low`, per `useTrueRange`.
    fn bar_span[L: Lanes](lanes: &L, bar: u32)
    { tr: true_range(lanes, bar, false), high: lanes.high(bar), low: lanes.low(bar) }
);

#[cfg(any(test, feature = "aeneas"))]
bar_span_fn!(
    generic_op,
    F,
    /// `bar_span_fn!` over `generic_op`: from the raw reads `high(bar)`,
    /// `low(bar)`, `close(bar - 1)` (ignored when `first`).
    pub(crate) fn bar_span_generic[F: FloatOps](h: F, l: F, first: bool, pc: F)
    { tr: true_range_generic(h, l, first, pc, false), high: h, low: l }
);

vwap_source_fn!(
    f64_op,
    f64,
    fn vwap_source[L: Lanes](func: TaFunc, args: [f64; 3], lanes: &L, bar: u32)
    {
        use_src: matches!(
            func,
            TaFunc::VwapSource | TaFunc::VwapAnchored | TaFunc::VwapBands
        ),
        src: args.first().copied().unwrap_or(f64::NAN),
        high: lanes.high(bar),
        low: lanes.low(bar),
        close: lanes.close(bar)
    }
);

#[cfg(any(test, feature = "aeneas"))]
vwap_source_fn!(
    generic_op,
    F,
    /// `vwap_source_fn!` over `generic_op`: from `use_src` (the tag takes a
    /// source argument), the source argument and the raw reads `high(bar)`,
    /// `low(bar)`, `close(bar)`.
    pub(crate) fn vwap_source_generic[F: FloatOps](use_src: bool, src: F, h: F, l: F, c: F)
    { use_src: use_src, src: src, high: h, low: l, close: c }
);

vwap_segment_fn!(
    f64_op,
    f64,
    /// Returns `(pushed value, segment mean)`; the mean is na when the segment
    /// has no volume yet.
    fn advance_vwap_segment[]
);

#[cfg(any(test, feature = "aeneas"))]
vwap_segment_fn!(
    generic_op,
    F,
    /// `vwap_segment_fn!` over `generic_op`.
    pub(crate) fn advance_vwap_segment_generic[F: FloatOps]
);

/// Compute a pure window function's value after its ring has been advanced.
#[allow(clippy::too_many_lines)]
fn window_value(func: TaFunc, decl: &Decl, ring: Ring, cells: &[f64], scratch: &mut [f64]) -> f64 {
    // mom/roc only need the window's endpoints; na cells in between do not
    // poison (the formula never reads them).
    if matches!(func, TaFunc::Mom | TaFunc::Roc) {
        mom_roc_arm!(f64_op, get, last, ring, cells, func == TaFunc::Mom);
    }

    // rising/falling are bool-valued: plain false — never na — during warm-up
    // or when an na value sits in the window.
    if matches!(func, TaFunc::Rising | TaFunc::Falling) {
        rising_arm!(
            f64_op,
            window_ready,
            get,
            ring,
            cells,
            func == TaFunc::Rising
        );
    }

    // Scalar extrema ignore earlier missing values once a full bar window
    // exists, while a missing current source still yields `na`.
    if matches!(func, TaFunc::Highest | TaFunc::Lowest) {
        return extremum_fold(ring, cells, func == TaFunc::Highest);
    }

    if !ring.window_ready(cells) {
        return f64::NAN;
    }
    let count = ring.cap();
    let n = f64::from(count);
    match func {
        TaFunc::MathSum => math_sum_arm!(f64_op, window_sum, ring, cells),
        TaFunc::Wma => wma_arm!(f64_op, weighted_mean_last, ring, cells, count),
        TaFunc::Swma => swma_arm!(f64_op, at, get, ring, cells, count),
        TaFunc::Alma => alma_arm!(
            f64_op,
            get,
            ring,
            cells,
            count,
            n,
            f64::from_bits(decl.param(1).unwrap_or(0) as u64),
            f64::from_bits(decl.param(2).unwrap_or(0) as u64),
            decl.param(3).unwrap_or(0) != 0
        ),
        TaFunc::Stdev | TaFunc::Variance | TaFunc::Dev => stdev_arm!(
            f64_op,
            get,
            window_sum,
            ring,
            cells,
            count,
            n,
            func == TaFunc::Dev,
            func == TaFunc::Variance,
            decl.param(1).unwrap_or(1) != 0
        ),
        TaFunc::HighestBars | TaFunc::LowestBars => {
            extremum_bars(ring, cells, count, func == TaFunc::HighestBars)
        }
        TaFunc::Linreg => linreg_arm!(
            f64_op,
            get,
            ring,
            cells,
            count,
            n,
            decl.param(1).unwrap_or(0)
        ),
        TaFunc::Cci => cci_arm!(f64_op, get, last, window_sum, ring, cells, count, n),
        TaFunc::Cmo => cmo_arm!(f64_op, get, ring, cells, count),
        TaFunc::Cog => cog_arm!(f64_op, get, window_sum, ring, cells, count),
        TaFunc::Median | TaFunc::MedianInt => {
            median_arm!(f64_op, sorted_window, at, ring, cells, scratch)
        }
        TaFunc::Mode | TaFunc::ModeInt => {
            mode_arm!(f64_op, sorted_window, at, ring, cells, scratch)
        }
        TaFunc::PercentileLinearInterpolation => percentile_linear_interpolation_arm!(
            f64_op,
            sorted_window,
            at,
            ring,
            cells,
            scratch,
            f64::from_bits(decl.param(1).unwrap_or(0) as u64)
        ),
        TaFunc::PercentileNearestRank => percentile_nearest_rank_arm!(
            f64_op,
            sorted_window,
            at,
            ring,
            cells,
            scratch,
            f64::from_bits(decl.param(1).unwrap_or(0) as u64)
        ),
        TaFunc::PercentRank => percent_rank_arm!(f64_op, get, ring, cells, count),
        TaFunc::Range | TaFunc::RangeInt => range_arm!(f64_op, get, ring, cells, count),
        TaFunc::Rci => rci_arm!(
            f64_op,
            sorted_window,
            tie_block,
            get,
            ring,
            cells,
            scratch,
            tie_at,
            count,
            n
        ),
        TaFunc::Bbw => bbw_arm!(
            f64_op,
            get,
            window_sum,
            ring,
            cells,
            count,
            n,
            f64::from_bits(decl.param(1).unwrap_or(0) as u64)
        ),
        _ => f64::NAN,
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::{
        accdist_arm_generic, alma_arm_generic, barssince_arm_generic, bb_arm_generic,
        bbw_arm_generic, cross_arm_generic, cum_arm_generic, dmi_arm_generic, extremum_arm_generic,
        fixnan_arm_generic, hma_arm_generic, kc_arm_generic, kcw_arm_generic, linreg_arm_generic,
        macd_arm_generic, median_arm_generic, mode_arm_generic, obv_arm_generic,
        percentile_nearest_rank_arm_generic, pivot_arm_generic, pvt_arm_generic, range_arm_generic,
        rci_arm_generic, rising_arm_generic, rsi_arm_generic, sar_arm_generic, smooth_arm_generic,
        stdev_family_arm_generic, stoch_arm_generic, supertrend_arm_generic, tr_arm_generic,
        true_range, true_range_generic, tsi_arm_generic, valuewhen_arm_generic, vwap_arm_generic,
        vwap_bands_arm_generic, vwma_arm_generic, wad_arm_generic, Lanes,
    };
    use crate::state::Smoother;
    use crate::{Decl, Engine, Layout, State, TaFunc};
    use std::{vec, vec::Vec};

    /// Bit equality, except that any two NaNs agree (Rust leaves the NaN
    /// payload of arithmetic unspecified; see `state::tests::same_f64`).
    fn same_f64(a: f64, b: f64) -> bool {
        a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
    }

    #[test]
    fn kcw_production_ratio_matches_expected_values() {
        for (src, span, mult, expected) in [
            (10.0, 4.0, 1.0, 0.8),
            (-10.0, 4.0, 1.0, -0.8),
            (10.0, 0.0, 1.0, 0.0),
            (10.0, 4.0, -1.0, -0.8),
            // A ratio can remain finite when multiplying it by 100 would
            // overflow. Scaling the already-normalized percent output fails.
            (1e-308, 1.0, 0.5, 1e308),
        ] {
            let mut basis = Smoother::default();
            let mut range = Smoother::default();
            let actual = super::kcw_ratio(&mut basis, &mut range, 1, mult, src, span);
            assert!(same_f64(actual, expected), "{src} {span} {mult}: {actual}");
        }
        let mut basis = Smoother::default();
        let mut range = Smoother::default();
        assert!(super::kcw_ratio(&mut basis, &mut range, 1, 1.0, 0.0, 4.0).is_nan());

        let mut basis = Smoother::default();
        let mut range = Smoother::default();
        assert_eq!(
            super::kcw_ratio(&mut basis, &mut range, 1, 1.0, 10.0, 4.0),
            0.8
        );
        assert!(super::kcw_ratio(&mut basis, &mut range, 1, 1.0, f64::NAN, 4.0).is_nan());
        assert_eq!(
            super::kcw_ratio(&mut basis, &mut range, 1, 1.0, 10.0, 4.0),
            0.8
        );

        let mut old_basis = Smoother::default();
        let mut old_range = Smoother::default();
        let historical = kcw_arm_generic(
            &mut old_basis,
            &mut old_range,
            1,
            1.0,
            false,
            10.0,
            12.0,
            8.0,
            true,
            f64::NAN,
        );
        assert_eq!(
            historical, 80.0,
            "kcw_arm still computes the percent-valued width"
        );
    }

    /// Expected value for `max`: `f64::max`, except that `+0.0` is returned
    /// against `-0.0` in either order. The once-per-bar functions whose zero
    /// results this decides (`ta.max`/`ta.min`, `ta.sar`, the true range)
    /// call this where the plain form calls `f64::max`; that is their only
    /// difference. The expected values for `ta.rsi`, `ta.cmo`, `ta.wad` and
    /// the `ta.vwap` bands still call `f64::max`/`f64::min`, as the shipped
    /// arms do. Written without the shipped `fmax_det`, so the test does not
    /// depend on it.
    fn ref_max(a: f64, b: f64) -> f64 {
        if a == 0.0 && b == 0.0 && a.is_sign_negative() != b.is_sign_negative() {
            0.0
        } else {
            a.max(b)
        }
    }

    /// The expected rule for window extrema: a zero of either sign is
    /// `+0.0`. The expected values for `ta.highest`/`ta.lowest`,
    /// `ta.stoch`, `ta.wpr` and `ta.range` fold with `f64::max`/`f64::min`
    /// and apply this to every folded extreme; that is their only
    /// difference.
    fn ref_canon(v: f64) -> f64 {
        if v == 0.0 {
            0.0
        } else {
            v
        }
    }

    /// Expected value for `min`: `f64::min`, except `-0.0` on the signed-zero
    /// tie (see [`ref_max`]).
    fn ref_min(a: f64, b: f64) -> f64 {
        if a == 0.0 && b == 0.0 && a.is_sign_negative() != b.is_sign_negative() {
            -0.0
        } else {
            a.min(b)
        }
    }

    /// Per-bar market data; out-of-range bars read as na.
    struct Bars {
        high: Vec<f64>,
        low: Vec<f64>,
        close: Vec<f64>,
        volume: Vec<f64>,
    }

    fn lane(v: &[f64], bar: u32) -> f64 {
        v.get(bar as usize).copied().unwrap_or(f64::NAN)
    }

    impl Lanes for Bars {
        fn open(&self, _bar: u32) -> f64 {
            f64::NAN
        }
        fn high(&self, bar: u32) -> f64 {
            lane(&self.high, bar)
        }
        fn low(&self, bar: u32) -> f64 {
            lane(&self.low, bar)
        }
        fn close(&self, bar: u32) -> f64 {
            lane(&self.close, bar)
        }
        fn volume(&self, bar: u32) -> f64 {
            lane(&self.volume, bar)
        }
        fn time_ms(&self, bar: u32) -> i64 {
            i64::from(bar)
        }
    }

    /// The scalar `true_range`.
    fn ref_true_range<L: Lanes>(lanes: &L, bar: u32, handle_na: bool) -> f64 {
        let high = ref_norm(lanes.high(bar));
        let low = ref_norm(lanes.low(bar));
        if high.is_nan() || low.is_nan() {
            return f64::NAN;
        }
        let hl = high - low;
        let prev_close = if bar == 0 {
            f64::NAN
        } else {
            ref_norm(lanes.close(bar - 1))
        };
        if !prev_close.is_nan() {
            ref_max(
                ref_max(hl, (high - prev_close).abs()),
                (low - prev_close).abs(),
            )
        } else if handle_na {
            hl
        } else {
            f64::NAN
        }
    }

    fn ref_norm(v: f64) -> f64 {
        if v.is_finite() {
            v
        } else {
            f64::NAN
        }
    }

    /// The scalar non-generic `Smoother`.
    #[derive(Clone, Copy, Default)]
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

    /// The scalar `State::Smooth` arm (after `length`).
    fn ref_smooth<L: Lanes>(
        func: TaFunc,
        s: &mut RefSmoother,
        length: u32,
        x0: f64,
        lanes: &L,
        bar: u32,
    ) -> f64 {
        let (x, alpha) = match func {
            TaFunc::Atr => (ref_true_range(lanes, bar, true), 1.0 / f64::from(length)),
            TaFunc::Rma => (x0, 1.0 / f64::from(length)),
            _ => (x0, 2.0 / (f64::from(length) + 1.0)),
        };
        if !x.is_nan() {
            ref_norm(s.step(x, length, alpha))
        } else {
            f64::NAN
        }
    }

    /// The scalar `State::Rsi` arm (after `length`).
    fn ref_rsi(
        prev: &mut f64,
        up: &mut RefSmoother,
        down: &mut RefSmoother,
        x: f64,
        length: u32,
    ) -> f64 {
        let alpha = 1.0 / f64::from(length);
        if !x.is_nan() {
            let p = core::mem::replace(prev, x);
            if !p.is_nan() {
                let change = x - p;
                let gain = up.step(change.max(0.0), length, alpha);
                let loss = down.step((-change).max(0.0), length, alpha);
                if !gain.is_nan() && !loss.is_nan() {
                    ref_norm(100.0 * gain / (gain + loss))
                } else {
                    f64::NAN
                }
            } else {
                f64::NAN
            }
        } else {
            f64::NAN
        }
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    const SPECIALS: [f64; 12] = [
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
        2.5,
    ];

    fn sample(rng: &mut Rng, mode: u32, i: usize) -> f64 {
        let raw = rng.next();
        match mode {
            0 => (raw % 2001) as f64 * 0.5 - 500.0,
            1 => SPECIALS[(raw % SPECIALS.len() as u64) as usize],
            2 => f64::from_bits(raw),
            // Flat stretches without holes (zero-variance windows).
            4 => (raw % 4) as f64,
            // Flat stretches (zero changes, `-0.0` losses) and na holes.
            _ if i % 5 == 0 => f64::NAN,
            _ => (raw % 4) as f64,
        }
    }

    fn bars(rng: &mut Rng, mode: u32, n: usize) -> Bars {
        let mut b = Bars {
            high: Vec::new(),
            low: Vec::new(),
            close: Vec::new(),
            volume: Vec::new(),
        };
        for i in 0..n {
            b.high.push(sample(rng, mode, i));
            b.low.push(sample(rng, mode, i + 1));
            b.close.push(sample(rng, mode, i + 2));
            b.volume.push(sample(rng, mode, i + 3));
        }
        b
    }

    /// Differential for the true range: the scalar
    /// function, the shipped `f64_op` expansion and the generic twin at `f64`
    /// (fed the raw reads) agree bit for bit (up to NaN payload).
    #[test]
    fn generic_true_range_matches_the_reference() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for mode in 0..4 {
            let lanes = bars(&mut rng, mode, 3000);
            for bar in 0..3001u32 {
                for handle_na in [false, true] {
                    let old = ref_true_range(&lanes, bar, handle_na);
                    let new = true_range(&lanes, bar, handle_na);
                    let pc = if bar == 0 {
                        f64::from_bits(rng.next())
                    } else {
                        lanes.close(bar - 1)
                    };
                    let twin = true_range_generic(
                        lanes.high(bar),
                        lanes.low(bar),
                        bar == 0,
                        pc,
                        handle_na,
                    );
                    assert!(same_f64(old, new), "mode {mode} bar {bar}");
                    assert!(same_f64(old, twin), "mode {mode} bar {bar}");
                }
            }
        }
    }

    /// Differential for the `State::Smooth` (`ta.ema`,
    /// `ta.rma`, `ta.atr`) and `State::Rsi` arms: the shipped engine
    /// (`Engine::advance`), the scalar arms, and the generic twins at
    /// `f64` produce the same outputs bar by bar, on every length class and
    /// on random, special, raw-bit and flat/na inputs.
    #[test]
    fn generic_smooth_and_rsi_arms_match_the_reference() {
        let funcs = [TaFunc::Ema, TaFunc::Rma, TaFunc::Atr, TaFunc::Rsi];
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for param in [i64::MIN, 0, 1, 2, 3, 14, 100, 100_000, i64::MAX] {
            let length = crate::state::length_of(&Decl::new(TaFunc::Ema, &[param]).unwrap());
            let decls: Vec<Decl> = funcs
                .iter()
                .map(|&f| Decl::new(f, &[param]).unwrap())
                .collect();
            for mode in 0..4 {
                let n = 600;
                let lanes = bars(&mut rng, mode, n);
                let layout = Layout::plan(&decls);
                let mut states = vec![State::Invalid; decls.len()];
                let mut cells = vec![f64::NAN; layout.cells as usize];
                let mut scratch = vec![f64::NAN; layout.scratch as usize];
                let mut engine = Engine::init(&decls, &mut states, &mut cells, &mut scratch);
                let mut ref_s = [RefSmoother::default(); 3];
                let mut twin_s = [Smoother::<f64>::default(); 3];
                let (mut ref_prev, mut ref_up, mut ref_down) =
                    (f64::NAN, RefSmoother::default(), RefSmoother::default());
                let (mut tw_prev, mut tw_up, mut tw_down) = (
                    f64::NAN,
                    Smoother::<f64>::default(),
                    Smoother::<f64>::default(),
                );
                for bar in 0..n as u32 {
                    let x = sample(&mut rng, mode, bar as usize);
                    let pc = if bar == 0 {
                        f64::NAN
                    } else {
                        lanes.close(bar - 1)
                    };
                    for (k, &func) in funcs.iter().enumerate() {
                        let shipped = engine
                            .advance(k as u32, [x, f64::NAN, f64::NAN], &lanes, bar)
                            .unwrap()
                            .value;
                        let (old, twin) = if func == TaFunc::Rsi {
                            (
                                ref_rsi(&mut ref_prev, &mut ref_up, &mut ref_down, x, length),
                                rsi_arm_generic(&mut tw_prev, &mut tw_up, &mut tw_down, x, length),
                            )
                        } else {
                            (
                                ref_smooth(func, &mut ref_s[k], length, x, &lanes, bar),
                                smooth_arm_generic(
                                    &mut twin_s[k],
                                    func,
                                    length,
                                    x,
                                    lanes.high(bar),
                                    lanes.low(bar),
                                    bar == 0,
                                    pc,
                                ),
                            )
                        };
                        assert!(same_f64(shipped, old), "{func:?} len {length} bar {bar}");
                        assert!(same_f64(twin, old), "{func:?} len {length} bar {bar}");
                    }
                }
            }
        }
    }
    /// Scalar arms over
    /// `RefSmoother` (`fixnan`, `tr`, `cum`, `max`/`min`, the cross family,
    /// `macd`, `kc`, `kcw`).
    fn ref_step_if(s: &mut RefSmoother, x: f64, length: u32, alpha: f64) -> f64 {
        if !x.is_nan() {
            s.step(x, length, alpha)
        } else {
            f64::NAN
        }
    }

    fn ref_bar_span<L: Lanes>(lanes: &L, bar: u32, use_true_range: bool) -> f64 {
        if use_true_range {
            ref_true_range(lanes, bar, false)
        } else {
            let h = ref_norm(lanes.high(bar));
            let l = ref_norm(lanes.low(bar));
            if !h.is_nan() && !l.is_nan() {
                h - l
            } else {
                f64::NAN
            }
        }
    }

    fn ref_ema_alpha(length: u32) -> f64 {
        2.0 / (f64::from(length) + 1.0)
    }

    /// Expected state of one site (the scalar field layout).
    #[derive(Clone, Copy, Default)]
    struct RefSite {
        v: [f64; 2],
        seen: bool,
        since: i64,
        s: [RefSmoother; 3],
    }

    /// Twin state of one site.
    #[derive(Clone, Copy, Default)]
    struct TwinSite {
        v: [f64; 2],
        seen: bool,
        since: i64,
        s: [Smoother<f64>; 3],
    }

    /// Expected production ratio computed from the smoother states.
    /// Do not scale its normalized percent output: that
    /// intermediate can overflow or underflow before conversion.
    fn ref_kcw_ratio_output(st: &RefSite, decl: &Decl, src: f64, span: f64) -> f64 {
        let basis = st.s[0];
        let range = st.s[1];
        if !src.is_nan() && !span.is_nan() && basis.seeded && range.seeded {
            let mult = f64::from_bits(decl.param(1).unwrap_or(0) as u64);
            ref_norm(2.0 * mult * range.value / basis.value)
        } else {
            f64::NAN
        }
    }

    #[allow(clippy::too_many_lines)]
    fn ref_arm<L: Lanes>(
        func: TaFunc,
        decl: &Decl,
        st: &mut RefSite,
        args: [f64; 3],
        lanes: &L,
        bar: u32,
    ) -> [f64; 3] {
        let arg = |i: usize| args[i];
        let nan3 = |v: f64| [v, f64::NAN, f64::NAN];
        match func {
            TaFunc::Fixnan | TaFunc::FixnanInt => {
                let last = &mut st.v[0];
                let x = arg(0);
                if !x.is_nan() {
                    *last = x;
                }
                nan3(ref_norm(*last))
            }
            TaFunc::Tr => {
                let handle_na = decl.param(0).unwrap_or(0) != 0;
                nan3(ref_norm(ref_true_range(lanes, bar, handle_na)))
            }
            TaFunc::Cum => {
                let total = &mut st.v[0];
                let x = arg(0);
                nan3(if !x.is_nan() {
                    *total += x;
                    ref_norm(*total)
                } else {
                    f64::NAN
                })
            }
            TaFunc::Max | TaFunc::Min => {
                let value = &mut st.v[0];
                let x = arg(0);
                if !x.is_nan() {
                    *value = if value.is_nan() {
                        x
                    } else if func == TaFunc::Max {
                        ref_max(*value, x)
                    } else {
                        ref_min(*value, x)
                    };
                }
                nan3(ref_norm(*value))
            }
            TaFunc::Crossover | TaFunc::Crossunder | TaFunc::Cross => {
                let (a, b) = (arg(0), arg(1));
                let [prev_a, prev_b] = st.v;
                let crossed = if st.seen
                    && !a.is_nan()
                    && !b.is_nan()
                    && !prev_a.is_nan()
                    && !prev_b.is_nan()
                {
                    let (pa, pb) = (prev_a, prev_b);
                    match func {
                        TaFunc::Crossover => a > b && pa <= pb,
                        TaFunc::Crossunder => a < b && pa >= pb,
                        _ => (a > b && pa <= pb) || (a < b && pa >= pb),
                    }
                } else {
                    false
                };
                st.v = [a, b];
                st.seen = true;
                nan3(if crossed { 1.0 } else { 0.0 })
            }
            TaFunc::Macd => {
                let fast_len = decl.param(0).unwrap_or(1).clamp(1, 100_000) as u32;
                let slow_len = decl.param(1).unwrap_or(1).clamp(1, 100_000) as u32;
                let sig_len = decl.param(2).unwrap_or(1).clamp(1, 100_000) as u32;
                let [fast, slow, signal] = &mut st.s;
                let x = arg(0);
                let macd = if !x.is_nan() {
                    let f = fast.step(x, fast_len, ref_ema_alpha(fast_len));
                    let s = slow.step(x, slow_len, ref_ema_alpha(slow_len));
                    if !f.is_nan() && !s.is_nan() {
                        f - s
                    } else {
                        f64::NAN
                    }
                } else {
                    f64::NAN
                };
                let sig = if !macd.is_nan() {
                    signal.step(macd, sig_len, ref_ema_alpha(sig_len))
                } else {
                    f64::NAN
                };
                let hist = if !macd.is_nan() && !sig.is_nan() {
                    macd - sig
                } else {
                    f64::NAN
                };
                [ref_norm(macd), ref_norm(sig), ref_norm(hist)]
            }
            TaFunc::Kc | TaFunc::Kcw => {
                let length = crate::state::length_of(decl);
                let mult = f64::from_bits(decl.param(1).unwrap_or(0) as u64);
                let use_tr = decl.param(2).unwrap_or(1) != 0;
                let alpha = 2.0 / (f64::from(length) + 1.0);
                let span = ref_bar_span(lanes, bar, use_tr);
                let [basis, range_ema, _] = &mut st.s;
                let middle = ref_step_if(basis, arg(0), length, alpha);
                let range = ref_step_if(range_ema, span, length, alpha);
                if func == TaFunc::Kcw {
                    return nan3(if !middle.is_nan() && !range.is_nan() {
                        ref_norm(2.0 * mult * range / middle * 100.0)
                    } else {
                        f64::NAN
                    });
                }
                let (upper, lower) = if !middle.is_nan() && !range.is_nan() {
                    (middle + range * mult, middle - range * mult)
                } else {
                    (f64::NAN, f64::NAN)
                };
                [ref_norm(middle), ref_norm(upper), ref_norm(lower)]
            }
            TaFunc::BarsSince => {
                let since = &mut st.since;
                if args[0] == 1.0 {
                    *since = 0;
                } else if *since != i64::MIN {
                    *since = since.saturating_add(1);
                }
                nan3(if *since == i64::MIN {
                    f64::NAN
                } else {
                    *since as f64
                })
            }
            TaFunc::Obv => {
                let close = ref_norm(lanes.close(bar));
                let volume = ref_norm(lanes.volume(bar));
                let [total, prev_close] = &mut st.v;
                let value = if !close.is_nan() && !prev_close.is_nan() && !volume.is_nan() {
                    let sign = if close > *prev_close {
                        1.0
                    } else if close < *prev_close {
                        -1.0
                    } else {
                        0.0
                    };
                    *total += sign * volume;
                    ref_norm(*total)
                } else {
                    f64::NAN
                };
                *prev_close = close;
                nan3(value)
            }
            TaFunc::AccDist => {
                let h = ref_norm(lanes.high(bar));
                let l = ref_norm(lanes.low(bar));
                let c = ref_norm(lanes.close(bar));
                let v = ref_norm(lanes.volume(bar));
                let total = &mut st.v[0];
                nan3(
                    if !h.is_nan() && !l.is_nan() && !c.is_nan() && !v.is_nan() {
                        let term = if h == l {
                            0.0
                        } else {
                            (2.0 * c - l - h) / (h - l) * v
                        };
                        *total += term;
                        ref_norm(*total)
                    } else {
                        f64::NAN
                    },
                )
            }
            TaFunc::Pvt => {
                let close = ref_norm(lanes.close(bar));
                let volume = ref_norm(lanes.volume(bar));
                let [total, prev_close] = &mut st.v;
                let value = if !close.is_nan() && !prev_close.is_nan() && !volume.is_nan() {
                    let term = (close - *prev_close) / *prev_close * volume;
                    if term.is_finite() {
                        *total += term;
                        ref_norm(*total)
                    } else {
                        f64::NAN
                    }
                } else {
                    f64::NAN
                };
                *prev_close = close;
                nan3(value)
            }
            TaFunc::Wad => {
                let high = ref_norm(lanes.high(bar));
                let low = ref_norm(lanes.low(bar));
                let close = ref_norm(lanes.close(bar));
                let [total, prev_close] = &mut st.v;
                let gain = if !close.is_nan() && !prev_close.is_nan() {
                    let p = *prev_close;
                    if close > p {
                        if !low.is_nan() {
                            close - low.min(p)
                        } else {
                            f64::NAN
                        }
                    } else if close < p {
                        if !high.is_nan() {
                            close - high.max(p)
                        } else {
                            f64::NAN
                        }
                    } else {
                        0.0
                    }
                } else {
                    0.0
                };
                let value = if !gain.is_nan() {
                    *total += gain;
                    ref_norm(*total)
                } else {
                    f64::NAN
                };
                *prev_close = close;
                nan3(value)
            }
            _ => unreachable!(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn twin_arm<L: Lanes>(
        func: TaFunc,
        decl: &Decl,
        st: &mut TwinSite,
        args: [f64; 3],
        lanes: &L,
        bar: u32,
        pc: f64,
    ) -> [f64; 3] {
        let (h, l, first) = (lanes.high(bar), lanes.low(bar), bar == 0);
        let nan3 = |v: f64| [v, f64::NAN, f64::NAN];
        match func {
            TaFunc::Fixnan | TaFunc::FixnanInt => nan3(fixnan_arm_generic(&mut st.v[0], args[0])),
            TaFunc::Tr => nan3(tr_arm_generic(
                h,
                l,
                first,
                pc,
                decl.param(0).unwrap_or(0) != 0,
            )),
            TaFunc::Cum => nan3(cum_arm_generic(&mut st.v[0], args[0])),
            TaFunc::Max | TaFunc::Min => nan3(extremum_arm_generic(
                &mut st.v[0],
                args[0],
                func == TaFunc::Max,
            )),
            TaFunc::Crossover | TaFunc::Crossunder | TaFunc::Cross => {
                let [mut pa, mut pb] = st.v;
                let r = cross_arm_generic(func, &mut pa, &mut pb, &mut st.seen, args[0], args[1]);
                st.v = [pa, pb];
                nan3(r)
            }
            TaFunc::Macd => {
                let len = |i: usize| decl.param(i).unwrap_or(1).clamp(1, 100_000) as u32;
                let [fast, slow, signal] = &mut st.s;
                let (a, b, c) =
                    macd_arm_generic(fast, slow, signal, len(0), len(1), len(2), args[0]);
                [a, b, c]
            }
            TaFunc::Kc | TaFunc::Kcw => {
                let length = crate::state::length_of(decl);
                let mult = f64::from_bits(decl.param(1).unwrap_or(0) as u64);
                let use_tr = decl.param(2).unwrap_or(1) != 0;
                let [basis, range_ema, _] = &mut st.s;
                if func == TaFunc::Kcw {
                    return nan3(kcw_arm_generic(
                        basis, range_ema, length, mult, use_tr, args[0], h, l, first, pc,
                    ));
                }
                let (a, b, c) = kc_arm_generic(
                    basis, range_ema, length, mult, use_tr, args[0], h, l, first, pc,
                );
                [a, b, c]
            }
            TaFunc::BarsSince => nan3(barssince_arm_generic(&mut st.since, args[0] == 1.0)),
            TaFunc::Obv => {
                let [mut total, mut pc] = st.v;
                let r = obv_arm_generic(&mut total, &mut pc, lanes.close(bar), lanes.volume(bar));
                st.v = [total, pc];
                nan3(r)
            }
            TaFunc::AccDist => nan3(accdist_arm_generic(
                &mut st.v[0],
                h,
                l,
                lanes.close(bar),
                lanes.volume(bar),
            )),
            TaFunc::Pvt => {
                let [mut total, mut pc] = st.v;
                let r = pvt_arm_generic(&mut total, &mut pc, lanes.close(bar), lanes.volume(bar));
                st.v = [total, pc];
                nan3(r)
            }
            TaFunc::Wad => {
                let [mut total, mut pc] = st.v;
                let r = wad_arm_generic(&mut total, &mut pc, h, l, lanes.close(bar));
                st.v = [total, pc];
                nan3(r)
            }
            _ => unreachable!(),
        }
    }

    /// Differential for the arms above: the shipped
    /// engine (`Engine::advance`), the scalar arms, and the generic
    /// twins at `f64` agree bar by bar on every output (up to NaN payload;
    /// `max`/`min` are exact, the signed-zero tie included). KCW's
    /// percent-scaled helper is checked against its own output; the shipped
    /// ratio is tested separately.
    #[test]
    fn generic_taalgos2_arms_match_the_reference() {
        let mults = [0.0f64, 1.5, 2.0, -3.25, f64::INFINITY, f64::NAN];
        let mut rng = Rng(0x0123_4567_89ab_cdef);
        for param in [i64::MIN, 0, 1, 2, 3, 14, 100, i64::MAX] {
            let mut decls = vec![
                Decl::new(TaFunc::Fixnan, &[]).unwrap(),
                Decl::new(TaFunc::FixnanInt, &[]).unwrap(),
                Decl::new(TaFunc::Tr, &[0]).unwrap(),
                Decl::new(TaFunc::Tr, &[1]).unwrap(),
                Decl::new(TaFunc::Cum, &[]).unwrap(),
                Decl::new(TaFunc::Max, &[]).unwrap(),
                Decl::new(TaFunc::Min, &[]).unwrap(),
                Decl::new(TaFunc::Crossover, &[]).unwrap(),
                Decl::new(TaFunc::Crossunder, &[]).unwrap(),
                Decl::new(TaFunc::Cross, &[]).unwrap(),
                Decl::new(TaFunc::Macd, &[param, param.saturating_add(3), 2]).unwrap(),
                Decl::new(TaFunc::Macd, &[1, param, param]).unwrap(),
                Decl::new(TaFunc::BarsSince, &[]).unwrap(),
                Decl::new(TaFunc::Obv, &[]).unwrap(),
                Decl::new(TaFunc::AccDist, &[]).unwrap(),
                Decl::new(TaFunc::Pvt, &[]).unwrap(),
                Decl::new(TaFunc::Wad, &[]).unwrap(),
            ];
            for (k, &m) in mults.iter().enumerate() {
                let use_tr = i64::from(k % 2 == 0);
                decls.push(Decl::new(TaFunc::Kc, &[param, m.to_bits() as i64, use_tr]).unwrap());
                decls.push(Decl::new(TaFunc::Kcw, &[param, m.to_bits() as i64, use_tr]).unwrap());
            }
            decls.push(Decl::new(TaFunc::Kc, &[param]).unwrap());
            decls.push(Decl::new(TaFunc::Kcw, &[param]).unwrap());
            for mode in 0..4 {
                let n = 400;
                let lanes = bars(&mut rng, mode, n);
                let layout = Layout::plan(&decls);
                let mut states = vec![State::Invalid; decls.len()];
                let mut cells = vec![f64::NAN; layout.cells as usize];
                let mut scratch = vec![f64::NAN; layout.scratch as usize];
                let mut engine = Engine::init(&decls, &mut states, &mut cells, &mut scratch);
                // Initial values as `state::scalar_state`: `ta.cum` and
                // `ta.accdist` start at `0.0`, the running totals of `ta.obv`,
                // `ta.pvt`, `ta.wad` at `0.0` with a na previous close, the
                // other scalars at na.
                let init = |d: &Decl| match d.func() {
                    Some(TaFunc::Cum | TaFunc::AccDist) => [0.0; 2],
                    Some(TaFunc::Obv | TaFunc::Pvt | TaFunc::Wad) => [0.0, f64::NAN],
                    _ => [f64::NAN; 2],
                };
                let mut refs: Vec<RefSite> = decls
                    .iter()
                    .map(|d| RefSite {
                        v: init(d),
                        since: i64::MIN,
                        ..RefSite::default()
                    })
                    .collect();
                let mut twins: Vec<TwinSite> = decls
                    .iter()
                    .map(|d| TwinSite {
                        v: init(d),
                        since: i64::MIN,
                        ..TwinSite::default()
                    })
                    .collect();
                for bar in 0..n as u32 {
                    let args = [
                        sample(&mut rng, mode, bar as usize),
                        sample(&mut rng, mode, bar as usize + 1),
                        f64::NAN,
                    ];
                    let pc = if bar == 0 {
                        f64::from_bits(rng.next())
                    } else {
                        lanes.close(bar - 1)
                    };
                    for (k, decl) in decls.iter().enumerate() {
                        let func = decl.func().unwrap();
                        let out = engine.advance(k as u32, args, &lanes, bar).unwrap();
                        let shipped = [out.value, out.extra[0], out.extra[1]];
                        let old = ref_arm(func, decl, &mut refs[k], args, &lanes, bar);
                        let twin = twin_arm(func, decl, &mut twins[k], args, &lanes, bar, pc);
                        let ratio = if func == TaFunc::Kcw {
                            let span = ref_bar_span(&lanes, bar, decl.param(2).unwrap_or(1) != 0);
                            ref_kcw_ratio_output(&refs[k], decl, args[0], span)
                        } else {
                            old[0]
                        };
                        for j in 0..3 {
                            let zero_tie = matches!(func, TaFunc::Max | TaFunc::Min)
                                && old[j] == 0.0
                                && twin[j] == 0.0;
                            assert!(
                                same_f64(shipped[j], if j == 0 { ratio } else { old[j] }),
                                "{func:?} {param} mode {mode} bar {bar} out {j}: shipped={} ratio={} old={} args={args:?}", shipped[j], ratio, old[j]
                            );
                            assert!(
                                same_f64(twin[j], old[j]) || zero_tie,
                                "{func:?} {param} mode {mode} bar {bar} out {j}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// Scalar test window with the interpreter's `VecDeque<Option<f64>>`
    /// semantics: the last `cap` pushes, oldest first; ready when full and
    /// na-free. It is independent of `Ring`. Method names and argument shapes
    /// match `Ring`'s so the scalar arm bodies below stay (the
    /// `cells` argument is an unused empty slice).
    #[derive(Clone, Default)]
    struct RefWindow {
        cap: u32,
        buf: Vec<f64>,
    }

    impl RefWindow {
        fn new(cap: u32) -> RefWindow {
            RefWindow {
                cap: cap.max(1),
                buf: Vec::new(),
            }
        }
        fn push(&mut self, _cells: &mut [f64], x: f64) {
            if self.buf.len() == self.cap as usize {
                self.buf.remove(0);
            }
            self.buf.push(x);
        }
        fn cap(&self) -> u32 {
            self.cap
        }
        fn window_ready(&self, _cells: &[f64]) -> bool {
            self.buf.len() == self.cap as usize && self.buf.iter().all(|v| !v.is_nan())
        }
        fn get(&self, _cells: &[f64], i: u32) -> f64 {
            self.buf.get(i as usize).copied().unwrap_or(f64::NAN)
        }
        fn full(&self) -> bool {
            self.buf.len() == self.cap as usize
        }
        fn len(&self) -> u32 {
            self.buf.len() as u32
        }
        /// Scalar model of `Ring::last` (`self.len` read
        /// as `self.len()`).
        fn last(&self, cells: &[f64]) -> f64 {
            if self.len() == 0 {
                f64::NAN
            } else {
                self.get(cells, self.len() - 1)
            }
        }
    }

    /// Scalar model of `window_sum`.
    fn ref_window_sum(ring: &RefWindow, cells: &[f64]) -> f64 {
        let mut sum = 0.0;
        let mut i = 0u32;
        while i < ring.cap() {
            sum += ring.get(cells, i);
            i += 1;
        }
        sum
    }

    /// Scalar model of `window_value` for `ta.stdev`/`ta.variance`/`ta.dev`/
    /// `ta.bbw` after the push: the readiness prologue and the two match
    /// cases (`norm` → `ref_norm`, `window_sum` → `ref_window_sum`).
    fn ref_window_stats(func: TaFunc, decl: &Decl, ring: &RefWindow, cells: &[f64]) -> f64 {
        if !ring.window_ready(cells) {
            return f64::NAN;
        }
        let count = ring.cap();
        let n = f64::from(count);
        match func {
            TaFunc::Stdev | TaFunc::Variance | TaFunc::Dev => {
                let mean = ref_window_sum(ring, cells) / n;
                if func == TaFunc::Dev {
                    let mut abs_dev = 0.0;
                    let mut i = 0u32;
                    while i < count {
                        abs_dev += (ring.get(cells, i) - mean).abs();
                        i += 1;
                    }
                    return ref_norm(abs_dev / n);
                }
                let biased = decl.param(1).unwrap_or(1) != 0;
                let mut ssd = 0.0;
                let mut i = 0u32;
                while i < count {
                    let d = ring.get(cells, i) - mean;
                    ssd += d * d;
                    i += 1;
                }
                let variance = ssd / if biased { n } else { n - 1.0 };
                if func == TaFunc::Variance {
                    ref_norm(variance)
                } else {
                    ref_norm(libm::sqrt(variance))
                }
            }
            TaFunc::Bbw => {
                let mult = f64::from_bits(decl.param(1).unwrap_or(0) as u64);
                let basis = ref_window_sum(ring, cells) / n;
                let mut ssd = 0.0;
                let mut i = 0u32;
                while i < count {
                    let d = ring.get(cells, i) - basis;
                    ssd += d * d;
                    i += 1;
                }
                let stdev = libm::sqrt(ssd / n);
                ref_norm(2.0 * mult * stdev / basis * 100.0)
            }
            _ => unreachable!(),
        }
    }

    /// Scalar model of `State::Bb` arm (`Out` → `[value, extra..]`).
    fn ref_bb(decl: &Decl, ring: &mut RefWindow, cells: &mut [f64], x: f64) -> [f64; 3] {
        ring.push(cells, x);
        let length = crate::state::length_of(decl);
        let mult = f64::from_bits(decl.param(1).unwrap_or(0) as u64);
        if !ring.window_ready(cells) {
            return [f64::NAN, f64::NAN, f64::NAN];
        }
        let n = f64::from(length);
        let mean = ref_window_sum(ring, cells) / n;
        let mut ssd = 0.0;
        let mut i = 0u32;
        while i < ring.cap() {
            let d = ring.get(cells, i) - mean;
            ssd += d * d;
            i += 1;
        }
        let dev = mult * libm::sqrt(ssd / n);
        [ref_norm(mean), ref_norm(mean + dev), ref_norm(mean - dev)]
    }

    /// Scalar model of `vwap_source`.
    fn ref_vwap_source<L: Lanes>(func: TaFunc, args: [f64; 3], lanes: &L, bar: u32) -> f64 {
        if matches!(
            func,
            TaFunc::VwapSource | TaFunc::VwapAnchored | TaFunc::VwapBands
        ) {
            args.first().copied().unwrap_or(f64::NAN)
        } else {
            let h = ref_norm(lanes.high(bar));
            let l = ref_norm(lanes.low(bar));
            let c = ref_norm(lanes.close(bar));
            if !h.is_nan() && !l.is_nan() && !c.is_nan() {
                (h + l + c) / 3.0
            } else {
                f64::NAN
            }
        }
    }

    /// Scalar model of `advance_vwap_segment`.
    fn ref_vwap_segment(
        pv: &mut f64,
        p2v: &mut f64,
        vol: &mut f64,
        active: &mut bool,
        source: f64,
        volume: f64,
        anchor: bool,
    ) -> (f64, f64) {
        if anchor {
            *pv = 0.0;
            *p2v = 0.0;
            *vol = 0.0;
            *active = true;
        }
        if !*active {
            return (f64::NAN, f64::NAN);
        }
        if !source.is_nan() && !volume.is_nan() {
            *pv += source * volume;
            *p2v += source * source * volume;
            *vol += volume;
            if *vol > 0.0 {
                let mean = *pv / *vol;
                (ref_norm(mean), mean)
            } else {
                (f64::NAN, f64::NAN)
            }
        } else {
            (f64::NAN, f64::NAN)
        }
    }

    /// Expected / twin vwap state (`pv`, `p2v`, `vol`, `active`).
    #[derive(Clone, Copy)]
    struct VwapSite {
        pv: f64,
        p2v: f64,
        vol: f64,
        active: bool,
    }

    /// Scalar model of `State::Vwap` arms (both guards).
    fn ref_vwap<L: Lanes>(
        func: TaFunc,
        st: &mut VwapSite,
        args: [f64; 3],
        lanes: &L,
        bar: u32,
    ) -> [f64; 3] {
        let arg = |i: usize| args[i];
        let cond = |i: usize| arg(i) == 1.0;
        let VwapSite {
            pv,
            p2v,
            vol,
            active,
        } = st;
        if func != TaFunc::VwapBands {
            let source = ref_vwap_source(func, args, lanes, bar);
            let anchor = matches!(func, TaFunc::VwapAnchored) && cond(1);
            let v = ref_vwap_segment(
                pv,
                p2v,
                vol,
                active,
                source,
                ref_norm(lanes.volume(bar)),
                anchor,
            )
            .0;
            return [v, f64::NAN, f64::NAN];
        }
        let source = ref_vwap_source(func, args, lanes, bar);
        let anchor = cond(1);
        let (vwap, mean) = ref_vwap_segment(
            pv,
            p2v,
            vol,
            active,
            source,
            ref_norm(lanes.volume(bar)),
            anchor,
        );
        let mult = arg(2);
        if !mean.is_nan() && !mult.is_nan() {
            let variance = (*p2v / *vol - mean * mean).max(0.0);
            let dev = libm::sqrt(variance) * mult;
            return [vwap, ref_norm(mean + dev), ref_norm(mean - dev)];
        }
        [vwap, f64::NAN, f64::NAN]
    }

    /// One site's expected and twin state for the differential below.
    struct Algos3Site {
        reference: RefWindow,
        ref_vwap: VwapSite,
        ring: crate::arena::Ring,
        cells: Vec<f64>,
        twin_vwap: VwapSite,
    }

    /// Differential for one group of arms (`ta.stdev`,
    /// `ta.variance`, `ta.dev`, `ta.bbw`, `ta.bb` and the four `ta.vwap`
    /// forms): the shipped engine (`Engine::advance`), the scalar arms
    /// and the generic twins at `f64` agree bar by bar on every output (up to
    /// NaN payload), through window warm-up, na holes, ±∞, raw bit patterns,
    /// flat windows, every anchor/multiplier class and clamped lengths.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn generic_taalgos3_arms_match_the_reference() {
        let mults = [0.0f64, -0.0, 1.5, 2.0, -3.25, f64::INFINITY, f64::NAN];
        let mut rng = Rng(0x5851_f42d_4c95_7f2d);
        for param in [i64::MIN, 0, 1, 2, 3, 5, 14, 100, i64::MAX] {
            let mut decls = vec![
                Decl::new(TaFunc::Stdev, &[param]).unwrap(),
                Decl::new(TaFunc::Stdev, &[param, 0]).unwrap(),
                Decl::new(TaFunc::Stdev, &[param, 1]).unwrap(),
                Decl::new(TaFunc::Stdev, &[param, 7]).unwrap(),
                Decl::new(TaFunc::Variance, &[param]).unwrap(),
                Decl::new(TaFunc::Variance, &[param, 0]).unwrap(),
                Decl::new(TaFunc::Variance, &[param, 1]).unwrap(),
                Decl::new(TaFunc::Dev, &[param]).unwrap(),
                Decl::new(TaFunc::Bb, &[param]).unwrap(),
                Decl::new(TaFunc::Bbw, &[param]).unwrap(),
                Decl::new(TaFunc::Vwap, &[]).unwrap(),
                Decl::new(TaFunc::VwapSource, &[]).unwrap(),
                Decl::new(TaFunc::VwapAnchored, &[]).unwrap(),
                Decl::new(TaFunc::VwapBands, &[]).unwrap(),
            ];
            for &m in &mults {
                decls.push(Decl::new(TaFunc::Bb, &[param, m.to_bits() as i64]).unwrap());
                decls.push(Decl::new(TaFunc::Bbw, &[param, m.to_bits() as i64]).unwrap());
            }
            // Present (non-na) outputs per site and output, for the
            // non-vacuity check below.
            let mut present = vec![[0usize; 3]; decls.len()];
            for mode in 0..5 {
                let n = 300;
                let lanes = bars(&mut rng, mode, n);
                let layout = Layout::plan(&decls);
                let mut states = vec![State::Invalid; decls.len()];
                let mut cells = vec![f64::NAN; layout.cells as usize];
                let mut scratch = vec![f64::NAN; layout.scratch as usize];
                let mut engine = Engine::init(&decls, &mut states, &mut cells, &mut scratch);
                // Initial vwap state as `state::scalar_state`: zero sums,
                // active from bar 0 for the unanchored forms.
                let vwap0 = |d: &Decl| VwapSite {
                    pv: 0.0,
                    p2v: 0.0,
                    vol: 0.0,
                    active: matches!(d.func(), Some(TaFunc::Vwap | TaFunc::VwapSource)),
                };
                let mut sites: Vec<Algos3Site> = decls
                    .iter()
                    .map(|d| {
                        let cap = crate::state::length_of(d);
                        Algos3Site {
                            reference: RefWindow::new(cap),
                            ref_vwap: vwap0(d),
                            ring: crate::arena::Ring::new(0, cap),
                            cells: vec![f64::NAN; cap as usize],
                            twin_vwap: vwap0(d),
                        }
                    })
                    .collect();
                for bar in 0..n as u32 {
                    // arg 1 is the anchor condition (0/1/na, sometimes an
                    // arbitrary value), arg 2 the bands multiplier.
                    let anchor = match rng.next() % 8 {
                        0 => 1.0,
                        1 => f64::NAN,
                        2 => sample(&mut rng, mode, bar as usize),
                        _ => 0.0,
                    };
                    let args = [
                        sample(&mut rng, mode, bar as usize),
                        anchor,
                        if rng.next() % 4 == 0 {
                            mults[(rng.next() % mults.len() as u64) as usize]
                        } else {
                            sample(&mut rng, mode, bar as usize + 2)
                        },
                    ];
                    for (k, decl) in decls.iter().enumerate() {
                        let func = decl.func().unwrap();
                        let out = engine.advance(k as u32, args, &lanes, bar).unwrap();
                        let shipped = [out.value, out.extra[0], out.extra[1]];
                        let site = &mut sites[k];
                        let mult = f64::from_bits(decl.param(1).unwrap_or(0) as u64);
                        let biased = decl.param(1).unwrap_or(1) != 0;
                        let (old, twin) = match func {
                            TaFunc::Stdev | TaFunc::Variance | TaFunc::Dev | TaFunc::Bbw => {
                                site.reference.push(&mut [], args[0]);
                                let old = ref_window_stats(func, decl, &site.reference, &[]);
                                let twin = if func == TaFunc::Bbw {
                                    bbw_arm_generic(&mut site.ring, &mut site.cells, args[0], mult)
                                } else {
                                    stdev_family_arm_generic(
                                        &mut site.ring,
                                        &mut site.cells,
                                        args[0],
                                        func == TaFunc::Dev,
                                        func == TaFunc::Variance,
                                        biased,
                                    )
                                };
                                ([old, f64::NAN, f64::NAN], [twin, f64::NAN, f64::NAN])
                            }
                            TaFunc::Bb => {
                                let old = ref_bb(decl, &mut site.reference, &mut [], args[0]);
                                let (a, b, c) = bb_arm_generic(
                                    &mut site.ring,
                                    &mut site.cells,
                                    args[0],
                                    crate::state::length_of(decl),
                                    mult,
                                );
                                (old, [a, b, c])
                            }
                            _ => {
                                let old = ref_vwap(func, &mut site.ref_vwap, args, &lanes, bar);
                                let t = &mut site.twin_vwap;
                                let twin = if func == TaFunc::VwapBands {
                                    let (a, b, c) = vwap_bands_arm_generic(
                                        &mut t.pv,
                                        &mut t.p2v,
                                        &mut t.vol,
                                        &mut t.active,
                                        args[0],
                                        lanes.volume(bar),
                                        args[1] == 1.0,
                                        args[2],
                                    );
                                    [a, b, c]
                                } else {
                                    let v = vwap_arm_generic(
                                        &mut t.pv,
                                        &mut t.p2v,
                                        &mut t.vol,
                                        &mut t.active,
                                        func != TaFunc::Vwap,
                                        args[0],
                                        lanes.high(bar),
                                        lanes.low(bar),
                                        lanes.close(bar),
                                        lanes.volume(bar),
                                        func == TaFunc::VwapAnchored && args[1] == 1.0,
                                    );
                                    [v, f64::NAN, f64::NAN]
                                };
                                (old, twin)
                            }
                        };
                        for j in 0..3 {
                            present[k][j] += usize::from(!shipped[j].is_nan());
                            assert!(
                                same_f64(shipped[j], old[j]),
                                "shipped {func:?} {param} mode {mode} bar {bar} out {j}: \
                                 {} vs {}",
                                shipped[j],
                                old[j]
                            );
                            assert!(
                                same_f64(twin[j], old[j]),
                                "twin {func:?} {param} mode {mode} bar {bar} out {j}: \
                                 {} vs {}",
                                twin[j],
                                old[j]
                            );
                        }
                    }
                }
            }
            // Every site produces present values once its window can fill
            // (lengths up to 100 within 300 bars): the comparison above is
            // not vacuous. A non-finite multiplier makes `ta.bbw` and the
            // `ta.bb` bands na throughout.
            if param != i64::MAX {
                for (k, decl) in decls.iter().enumerate() {
                    let func = decl.func().unwrap();
                    let finite_mult = f64::from_bits(decl.param(1).unwrap_or(0) as u64).is_finite();
                    // An unbiased variance of one sample is 0/0 = na.
                    let one_unbiased = matches!(func, TaFunc::Stdev | TaFunc::Variance)
                        && decl.param(1) == Some(0)
                        && crate::state::length_of(decl) == 1;
                    let need = match func {
                        _ if one_unbiased => [false; 3],
                        TaFunc::Bbw => [finite_mult, false, false],
                        TaFunc::Bb => [true, finite_mult, finite_mult],
                        TaFunc::VwapBands => [true, true, true],
                        _ => [true, false, false],
                    };
                    for (j, &needed) in need.iter().enumerate() {
                        assert!(
                            !needed || present[k][j] > 0,
                            "{func:?} {param} out {j}: no present output"
                        );
                    }
                }
            }
        }
    }

    /// Scalar model of `arena::sort_total`.
    fn ref_sort_total(slice: &mut [f64]) {
        let n = slice.len();
        if n < 2 {
            return;
        }
        let mut start = n / 2;
        while start > 0 {
            start -= 1;
            ref_sift_down(slice, start, n);
        }
        let mut end = n;
        while end > 1 {
            end -= 1;
            slice.swap(0, end);
            ref_sift_down(slice, 0, end);
        }
    }

    /// Scalar model of `arena::sift_down`.
    fn ref_sift_down(slice: &mut [f64], mut root: usize, end: usize) {
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
            if left.total_cmp(&best).is_gt() {
                swap = child;
                best = left;
            }
            if let Some(&right) = slice.get(child + 1) {
                if child + 1 < end && right.total_cmp(&best).is_gt() {
                    swap = child + 1;
                }
            }
            if swap == root {
                return;
            }
            slice.swap(root, swap);
            root = swap;
        }
    }

    /// Scalar model of `sorted_window` (over `RefWindow`).
    fn ref_sorted_window(ring: &RefWindow, cells: &[f64], scratch: &mut [f64]) -> usize {
        let n = ring.cap() as usize;
        if scratch.len() < n {
            return 0;
        }
        let mut i = 0u32;
        while (i as usize) < n {
            if let Some(slot) = scratch.get_mut(i as usize) {
                *slot = ring.get(cells, i);
            }
            i += 1;
        }
        if let Some(head) = scratch.get_mut(..n) {
            ref_sort_total(head);
        }
        n
    }

    /// Scalar model of `at`.
    fn ref_at(slice: &[f64], i: usize) -> f64 {
        match slice.get(i) {
            Some(&v) => v,
            None => f64::NAN,
        }
    }

    /// Scalar model of `State::ValueWhen` arm (`arg`/`cond` as in
    /// `advance_state`).
    fn ref_valuewhen(vals: &mut RefWindow, cells: &mut [f64], args: [f64; 3]) -> f64 {
        let arg = |i: usize| args.get(i).copied().unwrap_or(f64::NAN);
        let cond = |i: usize| arg(i) == 1.0;
        if cond(0) {
            vals.push(cells, arg(1));
        }
        if vals.full() {
            ref_norm(vals.get(cells, 0))
        } else {
            f64::NAN
        }
    }

    /// Scalar model of `State::Pivot` arm.
    fn ref_pivot(
        func: TaFunc,
        decl: &Decl,
        ring: &mut RefWindow,
        cells: &mut [f64],
        args: [f64; 3],
    ) -> f64 {
        let arg = |i: usize| args.get(i).copied().unwrap_or(f64::NAN);
        ring.push(cells, arg(0));
        let left = decl.param(0).unwrap_or(1).clamp(0, 100_000) as u32;
        if ring.window_ready(cells) {
            let center = ring.get(cells, left);
            let mut is_pivot = true;
            let mut i = 0u32;
            while i < ring.cap() {
                if i != left {
                    let x = ring.get(cells, i);
                    let ok = if func == TaFunc::PivotHigh {
                        center > x
                    } else {
                        center < x
                    };
                    if !ok {
                        is_pivot = false;
                        break;
                    }
                }
                i += 1;
            }
            if is_pivot {
                ref_norm(center)
            } else {
                f64::NAN
            }
        } else {
            f64::NAN
        }
    }

    /// Scalar model of `window_value` for `ta.alma`/`ta.linreg`/`ta.median`
    /// after the push: the readiness prologue and the three match cases
    /// (`norm` → `ref_norm`, `sorted_window` → `ref_sorted_window`,
    /// `at` → `ref_at`).
    fn ref_window_algos4(
        func: TaFunc,
        decl: &Decl,
        ring: &RefWindow,
        cells: &[f64],
        scratch: &mut [f64],
    ) -> f64 {
        if !ring.window_ready(cells) {
            return f64::NAN;
        }
        let count = ring.cap();
        let n = f64::from(count);
        match func {
            TaFunc::Alma => {
                let offset = f64::from_bits(decl.param(1).unwrap_or(0) as u64);
                let sigma = f64::from_bits(decl.param(2).unwrap_or(0) as u64);
                let floor = decl.param(3).unwrap_or(0) != 0;
                let m = if floor {
                    libm::floor(offset * (n - 1.0))
                } else {
                    offset * (n - 1.0)
                };
                let s = n / sigma;
                let mut weighted = 0.0;
                let mut norm_sum = 0.0;
                let mut i = 0u32;
                while i < count {
                    let d = f64::from(i) - m;
                    let w = pinewasm_num::raw::exp(-(d * d) / (2.0 * s * s));
                    weighted += w * ring.get(cells, i);
                    norm_sum += w;
                    i += 1;
                }
                ref_norm(weighted / norm_sum)
            }
            TaFunc::Linreg => {
                let offset = decl.param(1).unwrap_or(0) as f64;
                let sum_x = n * (n - 1.0) / 2.0;
                let sum_xx = (n - 1.0) * n * (2.0 * n - 1.0) / 6.0;
                let mut sum_y = 0.0;
                let mut sum_xy = 0.0;
                let mut i = 0u32;
                while i < count {
                    let y = ring.get(cells, i);
                    sum_y += y;
                    sum_xy += f64::from(i) * y;
                    i += 1;
                }
                let slope = (n * sum_xy - sum_x * sum_y) / (n * sum_xx - sum_x * sum_x);
                let intercept = (sum_y - slope * sum_x) / n;
                ref_norm(intercept + slope * (n - 1.0 - offset))
            }
            TaFunc::Median | TaFunc::MedianInt => {
                let len = ref_sorted_window(ring, cells, scratch);
                if len == 0 {
                    return f64::NAN;
                }
                let mid = len / 2;
                if len % 2 == 1 {
                    ref_norm(ref_at(scratch, mid))
                } else {
                    ref_norm(f64::midpoint(
                        ref_at(scratch, mid - 1),
                        ref_at(scratch, mid),
                    ))
                }
            }
            _ => unreachable!(),
        }
    }

    /// One site's expected and twin state for the differential below.
    struct Algos4Site {
        reference: RefWindow,
        ref_scratch: Vec<f64>,
        ring: crate::arena::Ring,
        cells: Vec<f64>,
        scratch: Vec<f64>,
    }

    /// Differential for one group of arms (`ta.valuewhen`
    /// in all four source types, `ta.pivothigh`, `ta.pivotlow`, `ta.median` in
    /// both source types, `ta.linreg`, `ta.alma`): the shipped engine
    /// (`Engine::advance`), the scalar arms and the generic twins at
    /// `f64` agree bar by bar (up to NaN payload), through warm-up, na
    /// holes, ±∞, raw bit patterns, flat windows, clamped lengths, every
    /// occurrence index, pivot left/right shape, even and odd median lengths,
    /// linreg offsets and alma offset/sigma/floor classes.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn generic_taalgos4_arms_match_the_reference() {
        let alma_offsets = [0.85f64, 0.0, 1.0, -0.5, 1.5, f64::INFINITY, f64::NAN];
        let alma_sigmas = [6.0f64, 0.5, 1e-3, 0.0, -2.0, f64::INFINITY, f64::NAN];
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        let ps = |d: &Decl| [d.param(0), d.param(1), d.param(2), d.param(3)];
        for param in [i64::MIN, -1, 0, 1, 2, 3, 4, 5, 14, 100, i64::MAX] {
            let mut decls = vec![
                Decl::new(TaFunc::ValueWhen, &[param]).unwrap(),
                Decl::new(TaFunc::ValueWhenInt, &[param]).unwrap(),
                Decl::new(TaFunc::ValueWhenBool, &[param]).unwrap(),
                Decl::new(TaFunc::ValueWhenColor, &[param]).unwrap(),
                Decl::new(TaFunc::Median, &[param]).unwrap(),
                Decl::new(TaFunc::MedianInt, &[param]).unwrap(),
                Decl::new(TaFunc::Linreg, &[param]).unwrap(),
                Decl::new(TaFunc::Alma, &[param]).unwrap(),
            ];
            // Pivots: leftbars = param, rightbars over a spread (and the
            // mirrored shape), plus the defaults.
            for right in [0i64, 1, 3, -4] {
                for func in [TaFunc::PivotHigh, TaFunc::PivotLow] {
                    decls.push(Decl::new(func, &[param.clamp(-8, 8), right]).unwrap());
                    decls.push(Decl::new(func, &[right, param.clamp(-8, 8)]).unwrap());
                }
            }
            decls.push(Decl::new(TaFunc::PivotHigh, &[]).unwrap());
            decls.push(Decl::new(TaFunc::PivotLow, &[]).unwrap());
            for offset in [0i64, 1, 3, -2, 100_000, i64::MIN, i64::MAX] {
                decls.push(Decl::new(TaFunc::Linreg, &[param, offset]).unwrap());
            }
            for (k, &offset) in alma_offsets.iter().enumerate() {
                for (j, &sigma) in alma_sigmas.iter().enumerate() {
                    let o = offset.to_bits() as i64;
                    let s = sigma.to_bits() as i64;
                    decls.push(match (k + j) % 3 {
                        0 => Decl::new(TaFunc::Alma, &[param, o, s]).unwrap(),
                        1 => Decl::new(TaFunc::Alma, &[param, o, s, 0]).unwrap(),
                        _ => Decl::new(TaFunc::Alma, &[param, o, s, 1]).unwrap(),
                    });
                }
            }
            // Present (non-na) outputs per site, for the non-vacuity check.
            let mut present = vec![0usize; decls.len()];
            for mode in 0..5 {
                let n = 300;
                let lanes = bars(&mut rng, mode, n);
                let layout = Layout::plan(&decls);
                let mut states = vec![State::Invalid; decls.len()];
                let mut cells = vec![f64::NAN; layout.cells as usize];
                let mut scratch = vec![f64::NAN; layout.scratch as usize];
                let mut engine = Engine::init(&decls, &mut states, &mut cells, &mut scratch);
                let mut sites: Vec<Algos4Site> = decls
                    .iter()
                    .map(|d| {
                        let cap = crate::state::cells_for(d);
                        Algos4Site {
                            reference: RefWindow::new(cap),
                            ref_scratch: vec![f64::NAN; cap as usize],
                            ring: crate::arena::Ring::new(0, cap),
                            cells: vec![f64::NAN; cap as usize],
                            scratch: vec![f64::NAN; cap as usize],
                        }
                    })
                    .collect();
                for bar in 0..n as u32 {
                    let x = sample(&mut rng, mode, bar as usize);
                    // The valuewhen condition: true, false, na, or an
                    // arbitrary value (true only when exactly 1.0).
                    let cond = match rng.next() % 4 {
                        0 | 1 => 1.0,
                        2 => [0.0, f64::NAN][(rng.next() % 2) as usize],
                        _ => sample(&mut rng, mode, bar as usize + 1),
                    };
                    let vw_args = [cond, sample(&mut rng, mode, bar as usize + 2), f64::NAN];
                    let win_args = [x, f64::NAN, f64::NAN];
                    for (k, decl) in decls.iter().enumerate() {
                        let func = decl.func().unwrap();
                        let site = &mut sites[k];
                        let (args, old, twin) = match func {
                            TaFunc::ValueWhen
                            | TaFunc::ValueWhenInt
                            | TaFunc::ValueWhenBool
                            | TaFunc::ValueWhenColor => {
                                let old = ref_valuewhen(&mut site.reference, &mut [], vw_args);
                                let twin = valuewhen_arm_generic(
                                    &mut site.ring,
                                    &mut site.cells,
                                    vw_args[0] == 1.0,
                                    vw_args[1],
                                );
                                (vw_args, old, twin)
                            }
                            TaFunc::PivotHigh | TaFunc::PivotLow => {
                                let old =
                                    ref_pivot(func, decl, &mut site.reference, &mut [], win_args);
                                let twin = pivot_arm_generic(
                                    &mut site.ring,
                                    &mut site.cells,
                                    x,
                                    decl.param(0).unwrap_or(1).clamp(0, 100_000) as u32,
                                    func == TaFunc::PivotHigh,
                                );
                                (win_args, old, twin)
                            }
                            _ => {
                                site.reference.push(&mut [], x);
                                let old = ref_window_algos4(
                                    func,
                                    decl,
                                    &site.reference,
                                    &[],
                                    &mut site.ref_scratch,
                                );
                                let twin = match func {
                                    TaFunc::Median | TaFunc::MedianInt => median_arm_generic(
                                        &mut site.ring,
                                        &mut site.cells,
                                        &mut site.scratch,
                                        x,
                                    ),
                                    TaFunc::Linreg => linreg_arm_generic(
                                        &mut site.ring,
                                        &mut site.cells,
                                        x,
                                        decl.param(1).unwrap_or(0),
                                    ),
                                    _ => alma_arm_generic(
                                        &mut site.ring,
                                        &mut site.cells,
                                        x,
                                        f64::from_bits(decl.param(1).unwrap_or(0) as u64),
                                        f64::from_bits(decl.param(2).unwrap_or(0) as u64),
                                        decl.param(3).unwrap_or(0) != 0,
                                    ),
                                };
                                (win_args, old, twin)
                            }
                        };
                        let out = engine.advance(k as u32, args, &lanes, bar).unwrap();
                        let shipped = out.value;
                        present[k] += usize::from(!shipped.is_nan());
                        assert!(
                            same_f64(shipped, old),
                            "shipped {func:?} {:?} mode {mode} bar {bar}: {shipped} vs {old}",
                            ps(decl)
                        );
                        assert!(
                            same_f64(twin, old),
                            "twin {func:?} {:?} mode {mode} bar {bar}: {twin} vs {old}",
                            ps(decl)
                        );
                        assert!(out.extra.iter().all(|e| e.is_nan()));
                    }
                }
            }
            // Non-vacuity: every site whose window can fill within 300 bars
            // produces present values, except the documented na classes
            // (linreg of length 1 is 0/0; a non-finite alma offset or sigma).
            let length = crate::state::length_of(&decls[4]);
            if length <= 100 {
                for (k, decl) in decls.iter().enumerate() {
                    let func = decl.func().unwrap();
                    let need = match func {
                        TaFunc::ValueWhen
                        | TaFunc::ValueWhenInt
                        | TaFunc::ValueWhenBool
                        | TaFunc::ValueWhenColor => param <= 5,
                        TaFunc::PivotHigh | TaFunc::PivotLow => crate::state::cells_for(decl) <= 9,
                        TaFunc::Linreg => {
                            length > 1 && decl.param(1).is_none_or(|o| o.unsigned_abs() <= 100_000)
                        }
                        TaFunc::Alma => {
                            f64::from_bits(decl.param(1).unwrap_or(0) as u64).is_finite()
                                && f64::from_bits(decl.param(2).unwrap_or(0) as u64).is_finite()
                        }
                        _ => true,
                    };
                    assert!(
                        !need || present[k] > 0,
                        "{func:?} {:?}: no present output",
                        ps(decl)
                    );
                }
            }
        }
    }

    /// The scalar models for one group of arms (`ta.stoch`,
    /// `ta.sar`, `ta.tsi`, `ta.supertrend`, `ta.range`, `ta.rci`)
    /// with `norm`/`finite`/`present`/`param_len` restated
    /// below; rings are the independent `RefWindow`/`RefPairWindow`.
    mod algos5_ref {
        extern crate std;

        use super::super::Out;
        use super::{ref_sorted_window, ref_true_range, Lanes, RefSmoother, RefWindow};
        use crate::{Decl, TaFunc};
        use std::vec::Vec;

        /// Scalar model of `engine::norm`.
        #[inline]
        fn norm(v: f64) -> f64 {
            if v.is_finite() {
                v
            } else {
                f64::NAN
            }
        }

        /// Scalar model of `engine::finite`.
        #[inline]
        fn finite(v: f64) -> f64 {
            norm(v)
        }

        /// Scalar model of `engine::present`.
        #[inline]
        fn present(v: f64) -> bool {
            !v.is_nan()
        }

        /// Scalar model of `engine::param_len`.
        pub(super) fn param_len(decl: &Decl, i: usize) -> u32 {
            decl.param(i).unwrap_or(1).clamp(1, 100_000) as u32
        }

        /// Scalar test pair window: the interpreter's
        /// `VecDeque<Option<(f64, f64)>>` semantics (a pair with an na side is
        /// absent; ready when `cap` pairs are held and none is absent),
        /// independent of `PairRing`, with `PairRing`'s method shapes.
        #[derive(Clone, Default)]
        pub(super) struct RefPairWindow {
            cap: u32,
            buf: Vec<(f64, f64)>,
        }

        impl RefPairWindow {
            pub(super) fn new(cap: u32) -> RefPairWindow {
                RefPairWindow {
                    cap: cap.max(1),
                    buf: Vec::new(),
                }
            }
            pub(super) fn push(&mut self, _cells: &mut [f64], a: f64, b: f64) {
                if self.buf.len() == self.cap as usize {
                    self.buf.remove(0);
                }
                if a.is_nan() || b.is_nan() {
                    self.buf.push((f64::NAN, f64::NAN));
                } else {
                    self.buf.push((a, b));
                }
            }
            pub(super) fn cap(&self) -> u32 {
                self.cap
            }
            pub(super) fn ready(&self, _cells: &[f64]) -> bool {
                self.buf.len() == self.cap as usize && self.buf.iter().all(|p| !p.0.is_nan())
            }
            pub(super) fn get(&self, _cells: &[f64], i: u32) -> (f64, f64) {
                self.buf
                    .get(i as usize)
                    .copied()
                    .unwrap_or((f64::NAN, f64::NAN))
            }
        }

        /// The scalar non-generic `state::Sar`.
        #[derive(Debug, Clone, Copy, Default, PartialEq)]
        pub(super) struct RefSar {
            bars: u64,
            result: f64,
            max_min: f64,
            accel: f64,
            is_below: bool,
            prev1_high: f64,
            prev1_low: f64,
            prev2_high: f64,
            prev2_low: f64,
            prev_close: f64,
        }

        /// The `State::Supertrend` fields (initial values as `init_state`).
        #[derive(Clone, Copy)]
        pub(super) struct RefSupertrend {
            pub(super) atr: RefSmoother,
            pub(super) bands_seeded: bool,
            pub(super) prev_upper: f64,
            pub(super) prev_lower: f64,
            pub(super) prev_supertrend: f64,
            pub(super) prev_direction: f64,
        }

        /// The `State::Tsi` fields (initial values as `init_state`).
        #[derive(Clone, Copy)]
        pub(super) struct RefTsi {
            pub(super) prev: f64,
            pub(super) pc_long: RefSmoother,
            pub(super) pc_short: RefSmoother,
            pub(super) abs_long: RefSmoother,
            pub(super) abs_short: RefSmoother,
        }

        /// Scalar model of `State::Stoch` arm (`arg` as in
        /// `advance_state`).
        pub(super) fn stoch(ring: &mut RefPairWindow, cells: &mut [f64], args: [f64; 3]) -> f64 {
            let arg = |i: usize| args.get(i).copied().unwrap_or(f64::NAN);
            let source = arg(0);
            ring.push(cells, arg(1), arg(2));
            if present(source) && ring.ready(cells) {
                let mut hh = f64::MIN;
                let mut ll = f64::MAX;
                let mut i = 0u32;
                while i < ring.cap() {
                    let (h, l) = ring.get(cells, i);
                    hh = hh.max(h);
                    ll = ll.min(l);
                    i += 1;
                }
                let (hh, ll) = (super::ref_canon(hh), super::ref_canon(ll));
                // A flat window (hh == ll) divides by zero → na.
                norm(100.0 * (source - ll) / (hh - ll))
            } else {
                f64::NAN
            }
        }

        /// Scalar model of `State::Sar` arm, with `advance_state`'s
        /// scalar tail.
        pub(super) fn sar<L: Lanes>(
            s: &mut RefSar,
            decl: &Decl,
            lanes: &L,
            bar: u32,
        ) -> Option<Out> {
            let value = {
                let start = f64::from_bits(decl.param(0).unwrap_or(0) as u64);
                let inc = f64::from_bits(decl.param(1).unwrap_or(0) as u64);
                let max = f64::from_bits(decl.param(2).unwrap_or(0) as u64);
                let high = finite(lanes.high(bar));
                let low = finite(lanes.low(bar));
                let close = finite(lanes.close(bar));
                if !present(high) || !present(low) || !present(close) {
                    // Data hole: na, recursion frozen.
                    return Some(Out::scalar(f64::NAN));
                }
                // Sar recurrence; `s.bars` counts executed advances (the
                // site's own bar_index).
                let value = if s.bars == 0 {
                    f64::NAN
                } else {
                    let mut is_first_trend_bar = false;
                    if s.bars == 1 {
                        if close > s.prev_close {
                            s.is_below = true;
                            s.max_min = high;
                            s.result = s.prev1_low;
                        } else {
                            s.is_below = false;
                            s.max_min = low;
                            s.result = s.prev1_high;
                        }
                        is_first_trend_bar = true;
                        s.accel = start;
                    }
                    s.result += s.accel * (s.max_min - s.result);
                    if s.is_below {
                        if s.result > low {
                            is_first_trend_bar = true;
                            s.is_below = false;
                            s.result = super::ref_max(high, s.max_min);
                            s.max_min = low;
                            s.accel = start;
                        }
                    } else if s.result < high {
                        is_first_trend_bar = true;
                        s.is_below = true;
                        s.result = super::ref_min(low, s.max_min);
                        s.max_min = high;
                        s.accel = start;
                    }
                    if !is_first_trend_bar {
                        if s.is_below {
                            if high > s.max_min {
                                s.max_min = high;
                                s.accel = super::ref_min(s.accel + inc, max);
                            }
                        } else if low < s.max_min {
                            s.max_min = low;
                            s.accel = super::ref_min(s.accel + inc, max);
                        }
                    }
                    if s.is_below {
                        s.result = super::ref_min(s.result, s.prev1_low);
                        if s.bars > 1 {
                            s.result = super::ref_min(s.result, s.prev2_low);
                        }
                    } else {
                        s.result = super::ref_max(s.result, s.prev1_high);
                        if s.bars > 1 {
                            s.result = super::ref_max(s.result, s.prev2_high);
                        }
                    }
                    norm(s.result)
                };
                s.prev2_high = s.prev1_high;
                s.prev2_low = s.prev1_low;
                s.prev1_high = high;
                s.prev1_low = low;
                s.prev_close = close;
                s.bars += 1;
                value
            };
            Some(Out::scalar(value))
        }

        /// Scalar model of `State::Tsi` arm.
        pub(super) fn tsi(st: &mut RefTsi, decl: &Decl, args: [f64; 3]) -> f64 {
            let arg = |i: usize| args.get(i).copied().unwrap_or(f64::NAN);
            let RefTsi {
                prev,
                pc_long,
                pc_short,
                abs_long,
                abs_short,
            } = st;
            let short = param_len(decl, 0);
            let long = param_len(decl, 1);
            let alpha_short = 2.0 / (f64::from(short) + 1.0);
            let alpha_long = 2.0 / (f64::from(long) + 1.0);
            let x = arg(0);
            if present(x) {
                let p = core::mem::replace(prev, x);
                if present(p) {
                    let mom = x - p;
                    let long_pc = pc_long.step(mom, long, alpha_long);
                    let smoothed_pc = if present(long_pc) {
                        pc_short.step(long_pc, short, alpha_short)
                    } else {
                        f64::NAN
                    };
                    let long_abs = abs_long.step(mom.abs(), long, alpha_long);
                    let smoothed_abs = if present(long_abs) {
                        abs_short.step(long_abs, short, alpha_short)
                    } else {
                        f64::NAN
                    };
                    if present(smoothed_pc) && present(smoothed_abs) {
                        // In [-1, 1]; an all-flat history (0/0) → na.
                        norm(smoothed_pc / smoothed_abs)
                    } else {
                        f64::NAN
                    }
                } else {
                    f64::NAN
                }
            } else {
                // na input: na output, state untouched.
                f64::NAN
            }
        }

        /// Scalar model of `State::Supertrend` arm (`true_range` →
        /// `ref_true_range`; the trailing `return` is the arm's).
        #[allow(clippy::needless_return)]
        pub(super) fn supertrend<L: Lanes>(
            st: &mut RefSupertrend,
            decl: &Decl,
            lanes: &L,
            bar: u32,
        ) -> Option<Out> {
            let RefSupertrend {
                atr,
                bands_seeded,
                prev_upper,
                prev_lower,
                prev_supertrend,
                prev_direction,
            } = st;
            let atr_period = param_len(decl, 1);
            let factor = f64::from_bits(decl.param(0).unwrap_or(0) as u64);
            // atr = ta.atr(atr_period): the rma of the true range with
            // the handle_na = true form. An na true range freezes the state.
            let tr_now = ref_true_range(lanes, bar, true);
            let atr_now = if present(tr_now) {
                atr.step(tr_now, atr_period, 1.0 / f64::from(atr_period))
            } else {
                f64::NAN
            };
            let high = finite(lanes.high(bar));
            let low = finite(lanes.low(bar));
            let close = finite(lanes.close(bar));
            if !(present(atr_now) && present(high) && present(low) && present(close)) {
                return Some(Out {
                    value: f64::NAN,
                    extra: [f64::NAN; 2],
                });
            }
            let src = f64::midpoint(high, low);
            let upper_basic = src + factor * atr_now;
            let lower_basic = src - factor * atr_now;
            // prevUpperBand/prevLowerBand default to the current basic on the
            // first valid bar.
            let (prev_up, prev_lo) = if *bands_seeded {
                (*prev_upper, *prev_lower)
            } else {
                (upper_basic, lower_basic)
            };
            let prev_close = if bar == 0 {
                f64::NAN
            } else {
                finite(lanes.close(bar - 1))
            };
            let upper_band =
                if present(prev_close) && !(upper_basic < prev_up || prev_close > prev_up) {
                    prev_up
                } else {
                    upper_basic
                };
            let lower_band =
                if present(prev_close) && !(lower_basic > prev_lo || prev_close < prev_lo) {
                    prev_lo
                } else {
                    lower_basic
                };
            // direction: +1 downtrend, −1 uptrend. On the first valid bar
            // start at +1 (down).
            let direction = if !*bands_seeded {
                1.0
            } else if *prev_supertrend == *prev_upper {
                if close > upper_band {
                    -1.0
                } else {
                    1.0
                }
            } else if close < lower_band {
                1.0
            } else {
                -1.0
            };
            let supertrend = if direction == -1.0 {
                lower_band
            } else {
                upper_band
            };
            *bands_seeded = true;
            *prev_upper = upper_band;
            *prev_lower = lower_band;
            *prev_supertrend = supertrend;
            *prev_direction = direction;
            return Some(Out {
                value: norm(supertrend),
                extra: [norm(direction), f64::NAN],
            });
        }

        /// Scalar model of `arena::tie_block`.
        fn tie_block(sorted: &[f64], x: f64) -> (usize, usize) {
            let (mut lo, mut hi) = (0usize, sorted.len());
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                match sorted.get(mid) {
                    Some(&v) if v.total_cmp(&x).is_lt() => lo = mid + 1,
                    _ => hi = mid,
                }
            }
            let first = lo;
            let mut count = 0usize;
            let mut i = first;
            while let Some(&v) = sorted.get(i) {
                if v != x {
                    break;
                }
                count += 1;
                i += 1;
            }
            (first, count)
        }

        /// Scalar model of `window_value` for `ta.range`/`ta.rci` after the
        /// push: the readiness prologue and the two match cases
        /// (`sorted_window` → `ref_sorted_window`).
        pub(super) fn window(
            func: TaFunc,
            ring: &RefWindow,
            cells: &[f64],
            scratch: &mut [f64],
        ) -> f64 {
            if !ring.window_ready(cells) {
                return f64::NAN;
            }
            let count = ring.cap();
            let n = f64::from(count);
            match func {
                TaFunc::Range | TaFunc::RangeInt => {
                    let mut max = f64::MIN;
                    let mut min = f64::MAX;
                    let mut i = 0u32;
                    while i < count {
                        let x = ring.get(cells, i);
                        max = max.max(x);
                        min = min.min(x);
                        i += 1;
                    }
                    norm(super::ref_canon(max) - super::ref_canon(min))
                }
                TaFunc::Rci => {
                    // Spearman's rank correlation between the source and the bar
                    // index, scaled to [-100, 100]. Price ties take average ranks, so
                    // the coefficient is Pearson on (position, rank).
                    let len = ref_sorted_window(ring, cells, scratch);
                    if len == 0 {
                        return f64::NAN;
                    }
                    let mean = (n - 1.0) / 2.0; // both rank sets average this + 1
                    let (mut cov, mut var_x, mut var_y) = (0.0, 0.0, 0.0);
                    let mut j = 0u32;
                    while j < count {
                        let x = ring.get(cells, j);
                        let (first, ties) = match scratch.get(..len) {
                            Some(sorted) => tie_block(sorted, x),
                            None => (0, 1),
                        };
                        // Ranks are 1-based; a tie block shares the average of the
                        // ranks it spans.
                        let rank = first as f64 + (ties.max(1) - 1) as f64 / 2.0 + 1.0;
                        let dx = f64::from(j) - mean;
                        let dy = rank - 1.0 - mean;
                        cov += dx * dy;
                        var_x += dx * dx;
                        var_y += dy * dy;
                        j += 1;
                    }
                    // A flat window has zero rank variance (0/0) → na.
                    norm(100.0 * cov / (libm::sqrt(var_x) * libm::sqrt(var_y)))
                }
                _ => unreachable!(),
            }
        }

        /// Direct check of the shared tie-block search: the generic twin and
        /// the shipped function against the scalar model.
        pub(super) fn tie_block_ref(sorted: &[f64], x: f64) -> (usize, usize) {
            tie_block(sorted, x)
        }
    }

    /// One site's expected and twin state for the differential below.
    struct Algos5Site {
        win: RefWindow,
        pair: algos5_ref::RefPairWindow,
        ref_scratch: Vec<f64>,
        ref_sar: algos5_ref::RefSar,
        ref_tsi: algos5_ref::RefTsi,
        ref_st: algos5_ref::RefSupertrend,
        ring: crate::arena::Ring,
        pring: crate::arena::PairRing,
        cells: Vec<f64>,
        scratch: Vec<f64>,
        sar: crate::state::Sar<f64>,
        tsi_prev: f64,
        tsi: [Smoother<f64>; 4],
        atr: Smoother<f64>,
        bands_seeded: bool,
        st_prev: [f64; 4],
    }

    /// Differential for one group of arms (`ta.stoch`,
    /// `ta.sar`, `ta.tsi`, `ta.supertrend`, `ta.range` in both source types,
    /// `ta.rci`): the shipped engine (`Engine::advance`), the scalar
    /// arms and the generic twins at `f64` agree bar by bar (up to NaN
    /// payload), through warm-up, na holes, ±∞, raw bit patterns, flat
    /// windows, clamped lengths, tsi short/long pairs, supertrend
    /// factor/period classes and sar start/inc/max classes. Every site whose
    /// output can be present produces present values.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn generic_taalgos5_arms_match_the_reference() {
        let bits = |x: f64| x.to_bits() as i64;
        let factors = [3.0f64, 0.0, -0.0, -2.5, 1e300, f64::INFINITY, f64::NAN];
        let sar_params: [[f64; 3]; 9] = [
            [0.02, 0.02, 0.2],
            [0.1, 0.05, 0.5],
            [0.0, 0.0, 0.0],
            [-0.02, 0.02, 0.2],
            [1.0, 1.0, 1.0],
            [2.0, 0.5, 10.0],
            [0.02, 0.02, 0.01],
            [f64::INFINITY, 0.02, 0.2],
            [0.02, f64::NAN, 0.2],
        ];
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let ps = |d: &Decl| [d.param(0), d.param(1), d.param(2)];
        for param in [i64::MIN, -1, 0, 1, 2, 3, 5, 14, 100, 100_001, i64::MAX] {
            let mut decls = vec![
                Decl::new(TaFunc::Stoch, &[param]).unwrap(),
                Decl::new(TaFunc::Range, &[param]).unwrap(),
                Decl::new(TaFunc::RangeInt, &[param]).unwrap(),
                Decl::new(TaFunc::Rci, &[param]).unwrap(),
                Decl::new(TaFunc::Tsi, &[param]).unwrap(),
                Decl::new(TaFunc::Supertrend, &[bits(3.0), param]).unwrap(),
            ];
            for other in [1i64, 2, 13, 25, -3] {
                decls.push(Decl::new(TaFunc::Tsi, &[param, other]).unwrap());
                decls.push(Decl::new(TaFunc::Tsi, &[other, param]).unwrap());
            }
            for &factor in &factors {
                decls.push(
                    Decl::new(TaFunc::Supertrend, &[bits(factor), param.clamp(-2, 10)]).unwrap(),
                );
            }
            for p in &sar_params {
                decls.push(Decl::new(TaFunc::Sar, &[bits(p[0]), bits(p[1]), bits(p[2])]).unwrap());
            }
            decls.push(Decl::new(TaFunc::Sar, &[]).unwrap());
            decls.push(Decl::new(TaFunc::Supertrend, &[]).unwrap());
            decls.push(Decl::new(TaFunc::Tsi, &[]).unwrap());
            let mut present = vec![0usize; decls.len()];
            for mode in 0..5 {
                let n = 300;
                let lanes = bars(&mut rng, mode, n);
                let layout = Layout::plan(&decls);
                let mut states = vec![State::Invalid; decls.len()];
                let mut cells = vec![f64::NAN; layout.cells as usize];
                let mut scratch = vec![f64::NAN; layout.scratch as usize];
                let mut engine = Engine::init(&decls, &mut states, &mut cells, &mut scratch);
                let mut sites: Vec<Algos5Site> = decls
                    .iter()
                    .map(|d| {
                        let cap = crate::state::length_of(d);
                        let cells = crate::state::cells_for(d).max(2 * cap);
                        Algos5Site {
                            win: RefWindow::new(cap),
                            pair: algos5_ref::RefPairWindow::new(cap),
                            ref_scratch: vec![f64::NAN; cap as usize],
                            ref_sar: algos5_ref::RefSar::default(),
                            ref_tsi: algos5_ref::RefTsi {
                                prev: f64::NAN,
                                pc_long: RefSmoother::default(),
                                pc_short: RefSmoother::default(),
                                abs_long: RefSmoother::default(),
                                abs_short: RefSmoother::default(),
                            },
                            ref_st: algos5_ref::RefSupertrend {
                                atr: RefSmoother::default(),
                                bands_seeded: false,
                                prev_upper: 0.0,
                                prev_lower: 0.0,
                                prev_supertrend: 0.0,
                                prev_direction: 0.0,
                            },
                            ring: crate::arena::Ring::new(0, cap),
                            pring: crate::arena::PairRing::new(0, cap),
                            cells: vec![f64::NAN; cells as usize],
                            scratch: vec![f64::NAN; cap as usize],
                            sar: crate::state::Sar::default(),
                            tsi_prev: f64::NAN,
                            tsi: [Smoother::default(); 4],
                            atr: Smoother::default(),
                            bands_seeded: false,
                            st_prev: [0.0; 4],
                        }
                    })
                    .collect();
                for bar in 0..n as u32 {
                    let args = [
                        sample(&mut rng, mode, bar as usize),
                        sample(&mut rng, mode, bar as usize + 1),
                        sample(&mut rng, mode, bar as usize + 2),
                    ];
                    let h = lanes.high(bar);
                    let l = lanes.low(bar);
                    let c = lanes.close(bar);
                    let first = bar == 0;
                    let pc = if first {
                        f64::NAN
                    } else {
                        lanes.close(bar - 1)
                    };
                    for (k, decl) in decls.iter().enumerate() {
                        let func = decl.func().unwrap();
                        let site = &mut sites[k];
                        let fbits = |i: usize| f64::from_bits(decl.param(i).unwrap_or(0) as u64);
                        let (old, twin) = match func {
                            TaFunc::Stoch => {
                                let old = algos5_ref::stoch(&mut site.pair, &mut [], args);
                                let twin = stoch_arm_generic(
                                    &mut site.pring,
                                    &mut site.cells,
                                    args[0],
                                    args[1],
                                    args[2],
                                );
                                ((old, f64::NAN), (twin, f64::NAN, f64::NAN))
                            }
                            TaFunc::Sar => {
                                let old =
                                    algos5_ref::sar(&mut site.ref_sar, decl, &lanes, bar).unwrap();
                                let twin = sar_arm_generic(
                                    &mut site.sar,
                                    fbits(0),
                                    fbits(1),
                                    fbits(2),
                                    h,
                                    l,
                                    c,
                                );
                                assert!(old.extra.iter().all(|e| e.is_nan()));
                                ((old.value, f64::NAN), (twin, f64::NAN, f64::NAN))
                            }
                            TaFunc::Tsi => {
                                let old = algos5_ref::tsi(&mut site.ref_tsi, decl, args);
                                let [a, b, cc, d] = &mut site.tsi;
                                let twin = tsi_arm_generic(
                                    &mut site.tsi_prev,
                                    a,
                                    b,
                                    cc,
                                    d,
                                    args[0],
                                    algos5_ref::param_len(decl, 0),
                                    algos5_ref::param_len(decl, 1),
                                );
                                ((old, f64::NAN), (twin, f64::NAN, f64::NAN))
                            }
                            TaFunc::Supertrend => {
                                let old =
                                    algos5_ref::supertrend(&mut site.ref_st, decl, &lanes, bar)
                                        .unwrap();
                                let [pu, pl, ps_, pd] = &mut site.st_prev;
                                let twin = supertrend_arm_generic(
                                    &mut site.atr,
                                    &mut site.bands_seeded,
                                    pu,
                                    pl,
                                    ps_,
                                    pd,
                                    algos5_ref::param_len(decl, 1),
                                    fbits(0),
                                    h,
                                    l,
                                    c,
                                    first,
                                    pc,
                                );
                                assert!(old.extra[1].is_nan());
                                ((old.value, old.extra[0]), twin)
                            }
                            _ => {
                                site.win.push(&mut [], args[0]);
                                let old =
                                    algos5_ref::window(func, &site.win, &[], &mut site.ref_scratch);
                                let twin = if func == TaFunc::Rci {
                                    rci_arm_generic(
                                        &mut site.ring,
                                        &mut site.cells,
                                        &mut site.scratch,
                                        args[0],
                                    )
                                } else {
                                    range_arm_generic(&mut site.ring, &mut site.cells, args[0])
                                };
                                ((old, f64::NAN), (twin, f64::NAN, f64::NAN))
                            }
                        };
                        let out = engine.advance(k as u32, args, &lanes, bar).unwrap();
                        present[k] += usize::from(!out.value.is_nan());
                        let ctx = || std::format!("{func:?} {:?} mode {mode} bar {bar}", ps(decl));
                        assert!(
                            same_f64(out.value, old.0),
                            "shipped {}: {} vs {}",
                            ctx(),
                            out.value,
                            old.0
                        );
                        assert!(same_f64(out.extra[0], old.1), "shipped extra {}", ctx());
                        assert!(out.extra[1].is_nan(), "shipped extra[1] {}", ctx());
                        assert!(
                            same_f64(twin.0, old.0),
                            "twin {}: {} vs {}",
                            ctx(),
                            twin.0,
                            old.0
                        );
                        assert!(same_f64(twin.1, old.1), "twin extra {}", ctx());
                        assert!(twin.2.is_nan(), "twin extra[1] {}", ctx());
                    }
                }
            }
            // Non-vacuity: every site whose output can be present within 300
            // bars produces present values, except the documented na classes
            // (rci of length 1 has zero rank variance; a non-finite supertrend
            // factor or sar parameter).
            for (k, decl) in decls.iter().enumerate() {
                let func = decl.func().unwrap();
                let len = |i: usize| algos5_ref::param_len(decl, i);
                let finite_params =
                    (0..3).all(|i| f64::from_bits(decl.param(i).unwrap_or(0) as u64).is_finite());
                let need = match func {
                    TaFunc::Stoch | TaFunc::Range | TaFunc::RangeInt => len(0) <= 100,
                    TaFunc::Rci => (2..=100).contains(&len(0)),
                    TaFunc::Tsi => len(0) + len(1) <= 200,
                    TaFunc::Supertrend => {
                        len(1) <= 100
                            && f64::from_bits(decl.param(0).unwrap_or(0) as u64).is_finite()
                    }
                    TaFunc::Sar => finite_params,
                    _ => true,
                };
                assert!(
                    !need || present[k] > 0,
                    "{func:?} {:?}: no present output",
                    ps(decl)
                );
            }
        }
    }

    /// The shared tie-block search (`arena::tie_block`, its generic twin)
    /// against the scalar function on sorted slices with tie
    /// runs, signed zeros, infinities and absent values.
    #[test]
    fn tie_block_matches_the_reference() {
        let mut rng = Rng(0x0123_4567_89ab_cdef);
        for len in 0..40usize {
            for mode in 0..4u32 {
                let mut v: Vec<f64> = (0..len)
                    .map(|i| match mode {
                        0 => (rng.next() % 4) as f64,
                        1 => SPECIALS[(rng.next() % SPECIALS.len() as u64) as usize],
                        2 => f64::from_bits(rng.next()),
                        _ => [0.0, -0.0, 1.0, f64::INFINITY][(i + rng.next() as usize) % 4],
                    })
                    .collect();
                v.sort_by(f64::total_cmp);
                let mut probes: Vec<f64> = v.clone();
                probes.extend_from_slice(&SPECIALS);
                for x in probes {
                    let want = algos5_ref::tie_block_ref(&v, x);
                    assert_eq!(crate::arena::tie_block(&v, x), want, "{v:?} {x}");
                    assert_eq!(crate::arena::tie_block_generic(&v, x), want, "{v:?} {x}");
                }
            }
        }
    }

    /// The scalar models for one group of arms (`ta.vwma`,
    /// `ta.hma`, `ta.dmi`, `ta.rising`/`ta.falling`, `ta.mode`,
    /// `ta.percentile_nearest_rank`)
    /// (`norm`/`finite`/`present`/`param_len`/`bool_out` restated;
    /// rings are the independent `RefWindow`/`RefPairWindow`, `Ring`'s
    /// `self.len` read as `ring.len()`).
    mod algos6_ref {
        use super::super::Out;
        use super::algos5_ref::RefPairWindow;
        use super::{ref_at, ref_sorted_window, ref_true_range, Lanes, RefSmoother, RefWindow};
        use crate::{Decl, TaFunc};

        /// Scalar model of `engine::norm`.
        #[inline]
        fn norm(v: f64) -> f64 {
            if v.is_finite() {
                v
            } else {
                f64::NAN
            }
        }

        /// Scalar model of `engine::finite`.
        #[inline]
        fn finite(v: f64) -> f64 {
            norm(v)
        }

        /// Scalar model of `engine::present`.
        #[inline]
        fn present(v: f64) -> bool {
            !v.is_nan()
        }

        /// Scalar model of `engine::bool_out`.
        #[inline]
        fn bool_out(b: bool) -> f64 {
            if b {
                1.0
            } else {
                0.0
            }
        }

        /// Scalar model of `engine::param_len`.
        pub(super) fn param_len(decl: &Decl, i: usize) -> u32 {
            decl.param(i).unwrap_or(1).clamp(1, 100_000) as u32
        }

        /// Scalar model of `weighted_mean_last` (over `RefWindow`).
        fn weighted_mean_last(ring: &RefWindow, cells: &[f64], k: u32) -> f64 {
            let n = f64::from(k);
            let mut sum = 0.0;
            let start = ring.len() - k;
            let mut i = 0u32;
            while i < k {
                sum += (f64::from(i) + 1.0) * ring.get(cells, start + i);
                i += 1;
            }
            sum / (n * (n + 1.0) / 2.0)
        }

        /// Scalar model of `Ring::last_window_ready` (`self` is the
        /// `RefWindow`).
        pub(super) fn last_window_ready(ring: &RefWindow, cells: &[f64], k: u32) -> bool {
            if ring.len() < k {
                return false;
            }
            let start = ring.len() - k;
            let mut i = start;
            while i < ring.len() {
                if ring.get(cells, i).is_nan() {
                    return false;
                }
                i += 1;
            }
            true
        }

        /// The `State::Dmi` fields (initial values as `init_state`).
        #[derive(Clone, Copy)]
        pub(super) struct RefDmi {
            pub(super) prev_high: f64,
            pub(super) prev_low: f64,
            pub(super) tr: RefSmoother,
            pub(super) plus_dm: RefSmoother,
            pub(super) minus_dm: RefSmoother,
            pub(super) adx: RefSmoother,
        }

        /// Scalar model of `State::WindowPair` arm (`arg` as in
        /// `advance_state`).
        pub(super) fn vwma<L: Lanes>(
            ring: &mut RefPairWindow,
            cells: &mut [f64],
            args: [f64; 3],
            lanes: &L,
            bar: u32,
        ) -> f64 {
            let arg = |i: usize| args.get(i).copied().unwrap_or(f64::NAN);
            ring.push(cells, arg(0), finite(lanes.volume(bar)));
            if ring.ready(cells) {
                let mut weighted = 0.0;
                let mut volumes = 0.0;
                let mut i = 0u32;
                while i < ring.cap() {
                    let (s, v) = ring.get(cells, i);
                    weighted += s * v;
                    volumes += v;
                    i += 1;
                }
                norm(weighted / volumes)
            } else {
                f64::NAN
            }
        }

        /// Scalar model of `State::Hma` arm (`*src` → `src`).
        pub(super) fn hma(
            src: &mut RefWindow,
            inter: &mut RefWindow,
            cells: &mut [f64],
            args: [f64; 3],
        ) -> f64 {
            let arg = |i: usize| args.get(i).copied().unwrap_or(f64::NAN);
            src.push(cells, arg(0));
            let half = (src.cap() / 2).max(1);
            let inter_value = if last_window_ready(src, cells, half) && src.window_ready(cells) {
                let v = 2.0 * weighted_mean_last(src, cells, half)
                    - weighted_mean_last(src, cells, src.cap());
                if v.is_finite() {
                    v
                } else {
                    f64::NAN
                }
            } else {
                f64::NAN
            };
            inter.push(cells, inter_value);
            if inter.window_ready(cells) {
                norm(weighted_mean_last(inter, cells, inter.cap()))
            } else {
                f64::NAN
            }
        }

        /// Scalar model of `State::Dmi` arm (`true_range` →
        /// `ref_true_range`).
        #[allow(clippy::needless_return)]
        pub(super) fn dmi<L: Lanes>(
            st: &mut RefDmi,
            decl: &Decl,
            lanes: &L,
            bar: u32,
        ) -> Option<Out> {
            let RefDmi {
                prev_high,
                prev_low,
                tr,
                plus_dm,
                minus_dm,
                adx,
            } = st;
            let di_len = param_len(decl, 0);
            let adx_len = param_len(decl, 1);
            let alpha = 1.0 / f64::from(di_len); // the rma smoothing.
            let high = finite(lanes.high(bar));
            let low = finite(lanes.low(bar));
            let mut out = Out {
                value: f64::NAN,
                extra: [f64::NAN; 2],
            };
            if present(high) && present(low) && present(*prev_high) && present(*prev_low) {
                let up_move = high - *prev_high;
                let down_move = *prev_low - low;
                let plus = if up_move > down_move && up_move > 0.0 {
                    up_move
                } else {
                    0.0
                };
                let minus = if down_move > up_move && down_move > 0.0 {
                    down_move
                } else {
                    0.0
                };
                let tr_v = ref_true_range(lanes, bar, false);
                if present(tr_v) {
                    let str_ = tr.step(tr_v, di_len, alpha);
                    let splus = plus_dm.step(plus, di_len, alpha);
                    let sminus = minus_dm.step(minus, di_len, alpha);
                    if present(str_) && present(splus) && present(sminus) && str_ > 0.0 {
                        let plus_di = 100.0 * splus / str_;
                        let minus_di = 100.0 * sminus / str_;
                        let di_sum = plus_di + minus_di;
                        // A zero DI sum makes DX na (leaves ADX untouched,
                        // like an na smoother input).
                        let adx_v = if di_sum > 0.0 {
                            let dx = 100.0 * (plus_di - minus_di).abs() / di_sum;
                            adx.step(dx, adx_len, alpha)
                        } else {
                            f64::NAN
                        };
                        out = Out {
                            value: norm(plus_di),
                            extra: [norm(minus_di), norm(adx_v)],
                        };
                    }
                }
            }
            *prev_high = high;
            *prev_low = low;
            return Some(out);
        }

        /// Scalar model of `window_value` for `ta.rising`/`ta.falling`,
        /// `ta.mode` and `ta.percentile_nearest_rank` after the push: the
        /// rising/falling branch, the readiness prologue and the match cases
        /// (`sorted_window` → `ref_sorted_window`, `at` → `ref_at`).
        pub(super) fn window(
            func: TaFunc,
            decl: &Decl,
            ring: &RefWindow,
            cells: &[f64],
            scratch: &mut [f64],
        ) -> f64 {
            if matches!(func, TaFunc::Rising | TaFunc::Falling) {
                if !ring.window_ready(cells) {
                    return 0.0;
                }
                let last = ring.cap() - 1;
                let current = ring.get(cells, last);
                let mut ok = true;
                let mut i = 0u32;
                while i < last {
                    let prev = ring.get(cells, i);
                    let good = if func == TaFunc::Rising {
                        current > prev
                    } else {
                        current < prev
                    };
                    if !good {
                        ok = false;
                        break;
                    }
                    i += 1;
                }
                return bool_out(ok);
            }
            if !ring.window_ready(cells) {
                return f64::NAN;
            }
            match func {
                TaFunc::Mode | TaFunc::ModeInt => {
                    // The most frequent value; ties resolve to the smallest.
                    let len = ref_sorted_window(ring, cells, scratch);
                    if len == 0 {
                        return f64::NAN;
                    }
                    let first = ref_at(scratch, 0);
                    let (mut best, mut best_len) = (first, 0usize);
                    let (mut run_value, mut run_len) = (first, 0usize);
                    let mut i = 0usize;
                    while i < len {
                        let x = ref_at(scratch, i);
                        if x == run_value {
                            run_len += 1;
                        } else {
                            run_value = x;
                            run_len = 1;
                        }
                        if run_len > best_len {
                            best = run_value;
                            best_len = run_len;
                        }
                        i += 1;
                    }
                    norm(best)
                }
                TaFunc::PercentileNearestRank => {
                    // Nearest rank: the smallest window value with at least p% of the
                    // data at or below it; percentage 0 selects the smallest value.
                    let p = f64::from_bits(decl.param(1).unwrap_or(0) as u64);
                    let len = ref_sorted_window(ring, cells, scratch);
                    if len == 0 {
                        return f64::NAN;
                    }
                    let rank = libm::ceil(p / 100.0 * len as f64) as usize;
                    norm(ref_at(scratch, rank.max(1) - 1))
                }
                _ => f64::NAN,
            }
        }
    }

    /// One site's expected and twin state for the differential below.
    struct Algos6Site {
        win: RefWindow,
        win2: RefWindow,
        pair: algos5_ref::RefPairWindow,
        ref_scratch: Vec<f64>,
        ref_dmi: algos6_ref::RefDmi,
        ring: crate::arena::Ring,
        ring2: crate::arena::Ring,
        pring: crate::arena::PairRing,
        cells: Vec<f64>,
        scratch: Vec<f64>,
        prev_hl: [f64; 2],
        dmi: [Smoother<f64>; 4],
    }

    /// A Vec-backed pair of SMAs, not a conversion of the normalized
    /// pair-window result.
    fn ref_vwma_independent(
        weighted: &mut RefWindow,
        volumes: &mut RefWindow,
        src: f64,
        volume: f64,
    ) -> f64 {
        let volume = ref_norm(volume);
        let product = ref_norm(src * volume);
        if !product.is_nan() {
            weighted.push(&mut [], product);
        }
        if !volume.is_nan() {
            volumes.push(&mut [], volume);
        }
        if weighted.window_ready(&[]) && volumes.window_ready(&[]) {
            let numerator = ref_norm(ref_window_sum(weighted, &[]) / f64::from(weighted.cap()));
            let denominator = ref_norm(ref_window_sum(volumes, &[]) / f64::from(volumes.cap()));
            ref_norm(numerator / denominator)
        } else {
            f64::NAN
        }
    }

    /// Differential for one group of arms (`ta.vwma`,
    /// `ta.hma`, `ta.dmi`, `ta.rising`/`ta.falling`, `ta.mode` in both
    /// source types, `ta.percentile_nearest_rank`): the shipped engine
    /// (`Engine::advance`), the scalar arms and the generic twins at
    /// `f64` agree bar by bar (up to NaN payload), through warm-up, na holes,
    /// ±∞, raw bit patterns, flat windows, clamped lengths, dmi di/adx
    /// length pairs and percentage classes. Every site whose output can be
    /// present produces present values (rising/falling: true values). VWMA is
    /// the ratio of two SMAs, one of price times volume and one of volume;
    /// the test checks the twin against that formula.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn generic_taalgos6_arms_match_the_reference() {
        let bits = |x: f64| x.to_bits() as i64;
        let percents = [
            0.0f64,
            -0.0,
            1.0,
            25.0,
            50.0,
            99.9,
            100.0,
            100.000_000_1,
            150.0,
            -10.0,
            5e-324,
            1e300,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
        ];
        let mut rng = Rng(0xd1b5_4a32_d192_ed03);
        let ps = |d: &Decl| [d.param(0), d.param(1), d.param(2)];
        for param in [i64::MIN, -1, 0, 1, 2, 3, 5, 14, 100, 100_001, i64::MAX] {
            let mut decls = vec![
                Decl::new(TaFunc::Vwma, &[param]).unwrap(),
                Decl::new(TaFunc::Hma, &[param]).unwrap(),
                Decl::new(TaFunc::Rising, &[param]).unwrap(),
                Decl::new(TaFunc::Falling, &[param]).unwrap(),
                Decl::new(TaFunc::Mode, &[param]).unwrap(),
                Decl::new(TaFunc::ModeInt, &[param]).unwrap(),
                Decl::new(TaFunc::PercentileNearestRank, &[param]).unwrap(),
                Decl::new(TaFunc::Dmi, &[param]).unwrap(),
            ];
            for &p in &percents {
                decls.push(Decl::new(TaFunc::PercentileNearestRank, &[param, bits(p)]).unwrap());
            }
            for other in [1i64, 2, 14, -3] {
                decls.push(Decl::new(TaFunc::Dmi, &[param, other]).unwrap());
                decls.push(Decl::new(TaFunc::Dmi, &[other, param]).unwrap());
            }
            decls.push(Decl::new(TaFunc::Dmi, &[]).unwrap());
            let mut present = vec![0usize; decls.len()];
            for mode in 0..5 {
                let n = 300;
                let lanes = bars(&mut rng, mode, n);
                let layout = Layout::plan(&decls);
                let mut states = vec![State::Invalid; decls.len()];
                let mut cells = vec![f64::NAN; layout.cells as usize];
                let mut scratch = vec![f64::NAN; layout.scratch as usize];
                let mut engine = Engine::init(&decls, &mut states, &mut cells, &mut scratch);
                let mut sites: Vec<Algos6Site> = decls
                    .iter()
                    .map(|d| {
                        let func = d.func().unwrap();
                        let length = crate::state::length_of(d);
                        // The shipped ring shapes (`state::site_shape`).
                        let (cap, cap2) = match func {
                            TaFunc::Vwma => (length, length),
                            TaFunc::Rising | TaFunc::Falling => (length.saturating_add(1), 1),
                            TaFunc::Hma => {
                                let len = length.max(2);
                                (len, crate::state::hma_outer(len))
                            }
                            _ => (length, 1),
                        };
                        let zero = Smoother::default();
                        Algos6Site {
                            win: RefWindow::new(cap),
                            win2: RefWindow::new(cap2),
                            pair: algos5_ref::RefPairWindow::new(cap),
                            ref_scratch: vec![f64::NAN; cap as usize],
                            ref_dmi: algos6_ref::RefDmi {
                                prev_high: f64::NAN,
                                prev_low: f64::NAN,
                                tr: RefSmoother::default(),
                                plus_dm: RefSmoother::default(),
                                minus_dm: RefSmoother::default(),
                                adx: RefSmoother::default(),
                            },
                            ring: crate::arena::Ring::new(0, cap),
                            ring2: crate::arena::Ring::new(cap, cap2),
                            pring: crate::arena::PairRing::new(0, cap),
                            cells: vec![f64::NAN; 2 * cap as usize + cap2 as usize],
                            scratch: vec![f64::NAN; cap as usize],
                            prev_hl: [f64::NAN; 2],
                            dmi: [zero; 4],
                        }
                    })
                    .collect();
                for bar in 0..n as u32 {
                    let args = [
                        sample(&mut rng, mode, bar as usize),
                        sample(&mut rng, mode, bar as usize + 1),
                        sample(&mut rng, mode, bar as usize + 2),
                    ];
                    let h = lanes.high(bar);
                    let l = lanes.low(bar);
                    let v = lanes.volume(bar);
                    let first = bar == 0;
                    let pc = if first {
                        f64::NAN
                    } else {
                        lanes.close(bar - 1)
                    };
                    for (k, decl) in decls.iter().enumerate() {
                        let func = decl.func().unwrap();
                        let site = &mut sites[k];
                        let fbits = |i: usize| f64::from_bits(decl.param(i).unwrap_or(0) as u64);
                        let mut current_policy = None;
                        let (old, twin) = match func {
                            TaFunc::Vwma => {
                                current_policy = Some(ref_vwma_independent(
                                    &mut site.win,
                                    &mut site.win2,
                                    args[0],
                                    v,
                                ));
                                let old =
                                    algos6_ref::vwma(&mut site.pair, &mut [], args, &lanes, bar);
                                let twin =
                                    vwma_arm_generic(&mut site.pring, &mut site.cells, args[0], v);
                                ([old, f64::NAN, f64::NAN], (twin, f64::NAN, f64::NAN))
                            }
                            TaFunc::Hma => {
                                let old =
                                    algos6_ref::hma(&mut site.win, &mut site.win2, &mut [], args);
                                let twin = hma_arm_generic(
                                    &mut site.ring,
                                    &mut site.ring2,
                                    &mut site.cells,
                                    args[0],
                                );
                                ([old, f64::NAN, f64::NAN], (twin, f64::NAN, f64::NAN))
                            }
                            TaFunc::Dmi => {
                                let old =
                                    algos6_ref::dmi(&mut site.ref_dmi, decl, &lanes, bar).unwrap();
                                let [ph, pl] = &mut site.prev_hl;
                                let [tr, plus, minus, adx] = &mut site.dmi;
                                let twin = dmi_arm_generic(
                                    ph,
                                    pl,
                                    tr,
                                    plus,
                                    minus,
                                    adx,
                                    algos6_ref::param_len(decl, 0),
                                    algos6_ref::param_len(decl, 1),
                                    h,
                                    l,
                                    first,
                                    pc,
                                );
                                ([old.value, old.extra[0], old.extra[1]], twin)
                            }
                            _ => {
                                site.win.push(&mut [], args[0]);
                                let old = algos6_ref::window(
                                    func,
                                    decl,
                                    &site.win,
                                    &[],
                                    &mut site.ref_scratch,
                                );
                                let twin = match func {
                                    TaFunc::Rising | TaFunc::Falling => rising_arm_generic(
                                        &mut site.ring,
                                        &mut site.cells,
                                        args[0],
                                        func == TaFunc::Rising,
                                    ),
                                    TaFunc::Mode | TaFunc::ModeInt => mode_arm_generic(
                                        &mut site.ring,
                                        &mut site.cells,
                                        &mut site.scratch,
                                        args[0],
                                    ),
                                    _ => percentile_nearest_rank_arm_generic(
                                        &mut site.ring,
                                        &mut site.cells,
                                        &mut site.scratch,
                                        args[0],
                                        fbits(1),
                                    ),
                                };
                                ([old, f64::NAN, f64::NAN], (twin, f64::NAN, f64::NAN))
                            }
                        };
                        let out = engine.advance(k as u32, args, &lanes, bar).unwrap();
                        present[k] += usize::from(match func {
                            TaFunc::Rising | TaFunc::Falling => out.value == 1.0,
                            _ => !out.value.is_nan(),
                        });
                        let ctx = || std::format!("{func:?} {:?} mode {mode} bar {bar}", ps(decl));
                        let shipped = [out.value, out.extra[0], out.extra[1]];
                        let twin = [twin.0, twin.1, twin.2];
                        for j in 0..3 {
                            let expected = if j == 0 {
                                current_policy.unwrap_or(old[j])
                            } else {
                                old[j]
                            };
                            assert!(
                                same_f64(shipped[j], expected),
                                "shipped[{j}] {}: {} vs {}",
                                ctx(),
                                shipped[j],
                                expected
                            );
                            assert!(
                                same_f64(twin[j], old[j]),
                                "twin[{j}] {}: {} vs {}",
                                ctx(),
                                twin[j],
                                old[j]
                            );
                        }
                    }
                }
            }
            // Non-vacuity: every site whose output can be present within 300
            // bars produces present values (rising/falling: a true value),
            // except the documented na classes (a percentage above 100 or
            // infinite ranks past the window).
            for (k, decl) in decls.iter().enumerate() {
                let func = decl.func().unwrap();
                let len = |i: usize| algos6_ref::param_len(decl, i);
                let p = f64::from_bits(decl.param(1).unwrap_or(0) as u64);
                let need = match func {
                    TaFunc::Vwma | TaFunc::Mode | TaFunc::ModeInt => len(0) <= 100,
                    TaFunc::Hma => len(0) <= 100,
                    TaFunc::Rising | TaFunc::Falling => len(0) <= 14,
                    TaFunc::PercentileNearestRank => len(0) <= 100 && (p.is_nan() || p <= 100.0),
                    TaFunc::Dmi => len(0) <= 100,
                    _ => true,
                };
                assert!(
                    !need || present[k] > 0,
                    "{func:?} {:?}: no present output",
                    ps(decl)
                );
            }
        }
    }

    /// `Ring::last_window_ready` (shipped) and its generic twin against the
    /// scalar method on rings with na cells at every
    /// position, every fill level and every `k` up to `cap + 1`. (Inside the
    /// hma arm the full-window `window_ready` check masks this function's
    /// newest-cell check, so it is tested on its own.)
    #[test]
    fn last_window_ready_matches_the_reference() {
        for cap in 1..8u32 {
            for pushes in 0..(2 * cap + 2) {
                for hole in 0..=pushes {
                    let mut ring = crate::arena::Ring::new(0, cap);
                    let mut cells = vec![0.0f64; cap as usize];
                    let mut win = RefWindow::new(cap);
                    for j in 0..pushes {
                        let x = if j == hole { f64::NAN } else { f64::from(j) };
                        ring.push(&mut cells, x);
                        win.push(&mut [], x);
                    }
                    for k in 0..=cap + 1 {
                        let want = algos6_ref::last_window_ready(&win, &[], k);
                        assert_eq!(
                            ring.last_window_ready(&cells, k),
                            want,
                            "{cap} {pushes} {hole} {k}"
                        );
                        assert_eq!(
                            ring.last_window_ready_generic(&cells, k),
                            want,
                            "{cap} {pushes} {hole} {k}"
                        );
                    }
                }
            }
        }
    }

    /// The scalar models for one group of arms (`ta.mom`,
    /// `ta.roc`, `ta.wma`, `ta.cci`, `ta.cmo`,
    /// `ta.percentile_linear_interpolation`, `ta.correlation`, `ta.wpr`),
    /// with `norm`/`finite`/`present`/
    /// `weighted_mean_last` restated; rings are the independent
    /// `RefWindow`/`RefPairWindow`, `window_sum` → `ref_window_sum`,
    /// `sorted_window` → `ref_sorted_window`, `at` → `ref_at`).
    mod algos7_ref {
        use super::algos5_ref::RefPairWindow;
        use super::{ref_at, ref_sorted_window, ref_window_sum, Lanes, RefWindow};
        use crate::{Decl, TaFunc};

        /// Scalar model of `engine::norm`.
        #[inline]
        fn norm(v: f64) -> f64 {
            if v.is_finite() {
                v
            } else {
                f64::NAN
            }
        }

        /// Scalar model of `engine::finite`.
        #[inline]
        fn finite(v: f64) -> f64 {
            norm(v)
        }

        /// Scalar model of `engine::present`.
        #[inline]
        fn present(v: f64) -> bool {
            !v.is_nan()
        }

        /// Scalar model of `weighted_mean_last` (over
        /// `RefWindow`).
        fn weighted_mean_last(ring: &RefWindow, cells: &[f64], k: u32) -> f64 {
            let n = f64::from(k);
            let mut sum = 0.0;
            let start = ring.len() - k;
            let mut i = 0u32;
            while i < k {
                sum += (f64::from(i) + 1.0) * ring.get(cells, start + i);
                i += 1;
            }
            sum / (n * (n + 1.0) / 2.0)
        }

        /// Scalar model of `window_value` for `ta.mom`/`ta.roc`, `ta.wma`,
        /// `ta.cci`, `ta.cmo` and `ta.percentile_linear_interpolation` after
        /// the push: the mom/roc branch, the readiness prologue and the match
        /// cases.
        pub(super) fn window(
            func: TaFunc,
            decl: &Decl,
            ring: &RefWindow,
            cells: &[f64],
            scratch: &mut [f64],
        ) -> f64 {
            // mom/roc only need the window's endpoints; na cells in between do not
            // poison (the formula never reads them).
            if matches!(func, TaFunc::Mom | TaFunc::Roc) {
                if !ring.full() {
                    return f64::NAN;
                }
                let oldest = ring.get(cells, 0);
                let newest = ring.last(cells);
                if !present(oldest) || !present(newest) {
                    return f64::NAN;
                }
                return if func == TaFunc::Mom {
                    norm(newest - oldest)
                } else {
                    norm(100.0 * (newest - oldest) / oldest)
                };
            }
            if !ring.window_ready(cells) {
                return f64::NAN;
            }
            let count = ring.cap();
            let n = f64::from(count);
            match func {
                TaFunc::Wma => norm(weighted_mean_last(ring, cells, count)),
                TaFunc::Cci => {
                    // (source - sma) / (0.015 * dev); a flat window divides by 0 → na.
                    let mean = ref_window_sum(ring, cells) / n;
                    let mut dev = 0.0;
                    let mut i = 0u32;
                    while i < count {
                        dev += (ring.get(cells, i) - mean).abs();
                        i += 1;
                    }
                    dev /= n;
                    norm((ring.last(cells) - mean) / (0.015 * dev))
                }
                TaFunc::Cmo => {
                    // 100 * (sum_up - sum_down) / (sum_up + sum_down) over the
                    // window's `length` bar-to-bar changes; all-flat (0/0) → na.
                    let (mut up, mut down) = (0.0, 0.0);
                    let mut i = 1u32;
                    while i < count {
                        let change = ring.get(cells, i) - ring.get(cells, i - 1);
                        up += change.max(0.0);
                        down += (-change).max(0.0);
                        i += 1;
                    }
                    norm(100.0 * (up - down) / (up + down))
                }
                TaFunc::PercentileLinearInterpolation => {
                    // Linear interpolation between the two nearest ranks over rank
                    // positions 0 ..= n-1 (the inclusive method).
                    let p = f64::from_bits(decl.param(1).unwrap_or(0) as u64);
                    let len = ref_sorted_window(ring, cells, scratch);
                    if len == 0 {
                        return f64::NAN;
                    }
                    let rank = p / 100.0 * (len - 1) as f64;
                    let lo = libm::floor(rank) as usize;
                    let hi = libm::ceil(rank) as usize;
                    let frac = rank - lo as f64;
                    norm(ref_at(scratch, lo) + frac * (ref_at(scratch, hi) - ref_at(scratch, lo)))
                }
                _ => f64::NAN,
            }
        }

        /// Scalar model of `State::Corr` arm (`arg` as in
        /// `advance_state`).
        pub(super) fn correlation(
            ring: &mut RefPairWindow,
            cells: &mut [f64],
            args: [f64; 3],
        ) -> f64 {
            let arg = |i: usize| args.get(i).copied().unwrap_or(f64::NAN);
            ring.push(cells, arg(0), arg(1));
            if ring.ready(cells) {
                // Pearson correlation, computed mean-first to limit float
                // cancellation. A zero variance on either side is 0/0 → na.
                let n = f64::from(ring.cap());
                let (mut mean_a, mut mean_b) = (0.0, 0.0);
                let mut i = 0u32;
                while i < ring.cap() {
                    let (a, b) = ring.get(cells, i);
                    mean_a += a;
                    mean_b += b;
                    i += 1;
                }
                mean_a /= n;
                mean_b /= n;
                let (mut cov, mut var_a, mut var_b) = (0.0, 0.0, 0.0);
                i = 0;
                while i < ring.cap() {
                    let (a, b) = ring.get(cells, i);
                    cov += (a - mean_a) * (b - mean_b);
                    var_a += (a - mean_a) * (a - mean_a);
                    var_b += (b - mean_b) * (b - mean_b);
                    i += 1;
                }
                norm(cov / (libm::sqrt(var_a) * libm::sqrt(var_b)))
            } else {
                f64::NAN
            }
        }

        /// Scalar model of `State::Wpr` arm.
        pub(super) fn wpr<L: Lanes>(
            highs: &mut RefWindow,
            lows: &mut RefWindow,
            cells: &mut [f64],
            lanes: &L,
            bar: u32,
        ) -> f64 {
            highs.push(cells, finite(lanes.high(bar)));
            lows.push(cells, finite(lanes.low(bar)));
            let close = finite(lanes.close(bar));
            if highs.window_ready(cells) && lows.window_ready(cells) && present(close) {
                let mut hh = f64::MIN;
                let mut ll = f64::MAX;
                let mut i = 0u32;
                while i < highs.cap() {
                    hh = hh.max(highs.get(cells, i));
                    i += 1;
                }
                i = 0;
                while i < lows.cap() {
                    ll = ll.min(lows.get(cells, i));
                    i += 1;
                }
                let (hh, ll) = (super::ref_canon(hh), super::ref_canon(ll));
                // 100 * (close - highest(high)) / (highest - lowest).
                norm(100.0 * (close - hh) / (hh - ll))
            } else {
                f64::NAN
            }
        }
    }

    /// One site's expected and twin state for the differential below.
    struct Algos7Site {
        win: RefWindow,
        win2: RefWindow,
        pair: algos5_ref::RefPairWindow,
        ref_scratch: Vec<f64>,
        ring: crate::arena::Ring,
        ring2: crate::arena::Ring,
        pring: crate::arena::PairRing,
        cells: Vec<f64>,
        scratch: Vec<f64>,
    }

    /// Differential for one group of arms (`ta.mom`,
    /// `ta.roc`, `ta.wma`, `ta.cci`, `ta.cmo`,
    /// `ta.percentile_linear_interpolation`, `ta.correlation`, `ta.wpr`):
    /// the shipped engine (`Engine::advance`), the scalar arms and the
    /// generic twins at `f64` agree bar by bar (up to NaN payload), through
    /// warm-up, na holes, ±∞, raw bit patterns, flat windows (zero
    /// deviation, variance, changes and range), clamped lengths and
    /// percentage classes (negative, fractional ranks, above 100, ±∞, NaN).
    /// Every site whose output can be present produces present values.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn generic_taalgos7_arms_match_the_reference() {
        use super::{
            cci_arm_generic, cmo_arm_generic, correlation_arm_generic, mom_roc_arm_generic,
            percentile_linear_interpolation_arm_generic, wma_arm_generic, wpr_arm_generic,
        };
        let bits = |x: f64| x.to_bits() as i64;
        let percents = [
            0.0f64,
            -0.0,
            1.0,
            25.0,
            33.3,
            50.0,
            99.9,
            100.0,
            100.000_000_1,
            150.0,
            -10.0,
            -250.0,
            5e-324,
            1e300,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
        ];
        let mut rng = Rng(0x6a09_e667_f3bc_c909);
        let ps = |d: &Decl| [d.param(0), d.param(1)];
        let window_funcs = [
            TaFunc::Mom,
            TaFunc::Roc,
            TaFunc::Wma,
            TaFunc::Cci,
            TaFunc::Cmo,
            TaFunc::PercentileLinearInterpolation,
        ];
        for param in [i64::MIN, -1, 0, 1, 2, 3, 5, 14, 100, 100_001, i64::MAX] {
            let mut decls = vec![
                Decl::new(TaFunc::Correlation, &[param]).unwrap(),
                Decl::new(TaFunc::Wpr, &[param]).unwrap(),
            ];
            for func in window_funcs {
                decls.push(Decl::new(func, &[param]).unwrap());
            }
            for &p in &percents {
                decls.push(
                    Decl::new(TaFunc::PercentileLinearInterpolation, &[param, bits(p)]).unwrap(),
                );
            }
            let mut present = vec![0usize; decls.len()];
            for mode in 0..5 {
                let n = 300;
                let lanes = bars(&mut rng, mode, n);
                let layout = Layout::plan(&decls);
                let mut states = vec![State::Invalid; decls.len()];
                let mut cells = vec![f64::NAN; layout.cells as usize];
                let mut scratch = vec![f64::NAN; layout.scratch as usize];
                let mut engine = Engine::init(&decls, &mut states, &mut cells, &mut scratch);
                let mut sites: Vec<Algos7Site> = decls
                    .iter()
                    .map(|d| {
                        let func = d.func().unwrap();
                        let length = crate::state::length_of(d);
                        // The shipped ring shapes (`state::site_shape`).
                        let cap = match func {
                            TaFunc::Mom | TaFunc::Roc | TaFunc::Cmo => length.saturating_add(1),
                            _ => length,
                        };
                        Algos7Site {
                            win: RefWindow::new(cap),
                            win2: RefWindow::new(cap),
                            pair: algos5_ref::RefPairWindow::new(cap),
                            ref_scratch: vec![f64::NAN; cap as usize],
                            ring: crate::arena::Ring::new(0, cap),
                            ring2: crate::arena::Ring::new(cap, cap),
                            pring: crate::arena::PairRing::new(0, cap),
                            cells: vec![f64::NAN; 2 * cap as usize],
                            scratch: vec![f64::NAN; cap as usize],
                        }
                    })
                    .collect();
                for bar in 0..n as u32 {
                    let args = [
                        sample(&mut rng, mode, bar as usize),
                        sample(&mut rng, mode, bar as usize + 1),
                        sample(&mut rng, mode, bar as usize + 2),
                    ];
                    for (k, decl) in decls.iter().enumerate() {
                        let func = decl.func().unwrap();
                        let site = &mut sites[k];
                        let (old, twin) = match func {
                            TaFunc::Correlation => (
                                algos7_ref::correlation(&mut site.pair, &mut [], args),
                                correlation_arm_generic(
                                    &mut site.pring,
                                    &mut site.cells,
                                    args[0],
                                    args[1],
                                ),
                            ),
                            TaFunc::Wpr => (
                                algos7_ref::wpr(
                                    &mut site.win,
                                    &mut site.win2,
                                    &mut [],
                                    &lanes,
                                    bar,
                                ),
                                wpr_arm_generic(
                                    &mut site.ring,
                                    &mut site.ring2,
                                    &mut site.cells,
                                    lanes.high(bar),
                                    lanes.low(bar),
                                    lanes.close(bar),
                                ),
                            ),
                            _ => {
                                site.win.push(&mut [], args[0]);
                                let old = algos7_ref::window(
                                    func,
                                    decl,
                                    &site.win,
                                    &[],
                                    &mut site.ref_scratch,
                                );
                                let (ring, cells, x) = (&mut site.ring, &mut site.cells, args[0]);
                                let twin = match func {
                                    TaFunc::Mom | TaFunc::Roc => {
                                        mom_roc_arm_generic(ring, cells, x, func == TaFunc::Mom)
                                    }
                                    TaFunc::Wma => wma_arm_generic(ring, cells, x),
                                    TaFunc::Cci => cci_arm_generic(ring, cells, x),
                                    TaFunc::Cmo => cmo_arm_generic(ring, cells, x),
                                    _ => percentile_linear_interpolation_arm_generic(
                                        ring,
                                        cells,
                                        &mut site.scratch,
                                        x,
                                        f64::from_bits(decl.param(1).unwrap_or(0) as u64),
                                    ),
                                };
                                (old, twin)
                            }
                        };
                        let out = engine.advance(k as u32, args, &lanes, bar).unwrap();
                        present[k] += usize::from(!out.value.is_nan());
                        let ctx = || std::format!("{func:?} {:?} mode {mode} bar {bar}", ps(decl));
                        assert!(
                            same_f64(out.value, old),
                            "shipped {}: {} vs {}",
                            ctx(),
                            out.value,
                            old
                        );
                        assert!(same_f64(twin, old), "twin {}: {} vs {}", ctx(), twin, old);
                        // Single-output arms: both extra outputs stay na.
                        assert!(out.extra[0].is_nan() && out.extra[1].is_nan(), "{}", ctx());
                    }
                }
            }
            // Non-vacuity: every site whose output can be present within 300
            // bars produces present values, except the documented na
            // classes: a one-bar cci/correlation window (zero deviation or
            // variance, 0/0), and a percentage whose rank leaves the window
            // or is not finite (above 100, ±∞, NaN) past a one-bar window.
            for (k, decl) in decls.iter().enumerate() {
                let func = decl.func().unwrap();
                let len = crate::state::length_of(decl);
                let p = f64::from_bits(decl.param(1).unwrap_or(0) as u64);
                let need = len <= 100
                    && match func {
                        TaFunc::Cci | TaFunc::Correlation => len >= 2,
                        TaFunc::PercentileLinearInterpolation => {
                            p.is_finite() && (p <= 100.0 || len == 1)
                        }
                        _ => true,
                    };
                assert!(
                    !need || present[k] > 0,
                    "{func:?} {:?}: no present output",
                    ps(decl)
                );
            }
        }
    }

    /// `Ring::last` (shipped) and its generic twin against the
    /// scalar method at every fill level, empty included, with na
    /// cells at every position. (Inside the mom/roc and cci arms the ring is
    /// always full, so the empty branch is tested on its own.)
    #[test]
    fn ring_last_matches_the_reference() {
        for cap in 1..8u32 {
            for pushes in 0..(2 * cap + 2) {
                for hole in 0..=pushes {
                    let mut ring = crate::arena::Ring::new(0, cap);
                    let mut cells = vec![0.0f64; cap as usize];
                    let mut win = RefWindow::new(cap);
                    for j in 0..pushes {
                        let x = if j == hole { f64::NAN } else { f64::from(j) };
                        ring.push(&mut cells, x);
                        win.push(&mut [], x);
                    }
                    let want = win.last(&[]);
                    let ctx = std::format!("{cap} {pushes} {hole}");
                    assert!(same_f64(ring.last(&cells), want), "{ctx}");
                    assert!(same_f64(ring.last_generic(&cells), want), "{ctx}");
                }
            }
        }
    }

    /// The scalar models for one group of arms
    /// (`ta.math_sum`, `ta.swma`, `ta.cog`, `ta.percentrank`, `ta.wvad`,
    /// `ta.iii`, `ta.mfi`, `ta.nvi`/`ta.pvi`),
    /// with `norm`/`finite`/`present` restated; rings are the independent
    /// `RefWindow`/`RefPairWindow`,
    /// `window_sum` → `ref_window_sum`, `at` → `ref_at`).
    mod algos8_ref {
        use super::algos5_ref::RefPairWindow;
        use super::{ref_at, ref_window_sum, Lanes, RefWindow};
        use crate::TaFunc;

        /// Scalar model of `engine::norm`.
        #[inline]
        fn norm(v: f64) -> f64 {
            if v.is_finite() {
                v
            } else {
                f64::NAN
            }
        }

        /// Scalar model of `engine::finite`.
        #[inline]
        fn finite(v: f64) -> f64 {
            norm(v)
        }

        /// Scalar model of `engine::present`.
        #[inline]
        fn present(v: f64) -> bool {
            !v.is_nan()
        }

        /// Scalar model of `window_value` for `ta.math_sum`, `ta.swma`, `ta.cog`
        /// and `ta.percentrank` after the push: the readiness prologue and the
        /// match cases.
        pub(super) fn window(func: TaFunc, ring: &RefWindow, cells: &[f64]) -> f64 {
            if !ring.window_ready(cells) {
                return f64::NAN;
            }
            let count = ring.cap();
            match func {
                TaFunc::MathSum => norm(ref_window_sum(ring, cells)),
                TaFunc::Swma => {
                    // x[3]*1/6 + x[2]*2/6 + x[1]*2/6 + x[0]*1/6 (oldest → newest).
                    let weights = [1.0, 2.0, 2.0, 1.0];
                    let mut sum = 0.0;
                    let mut i = 0u32;
                    while i < count && (i as usize) < weights.len() {
                        sum += ref_at(&weights, i as usize) * ring.get(cells, i);
                        i += 1;
                    }
                    norm(sum / 6.0)
                }
                TaFunc::Cog => {
                    // -Σ source[i]·(i+1) / Σ source[i], where i = 0 is the newest bar;
                    // a zero window sum divides by zero → na.
                    let sum = ref_window_sum(ring, cells);
                    let mut num = 0.0;
                    let mut j = 0u32;
                    while j < count {
                        // Window is oldest → newest; the newest cell gets weight 1.
                        num += ring.get(cells, j) * f64::from(count - j);
                        j += 1;
                    }
                    norm(-num / sum)
                }
                TaFunc::PercentRank => {
                    // Percent of the `length` previous values <= the current value
                    // (the window holds length + 1 cells).
                    let last = count - 1;
                    let current = ring.get(cells, last);
                    let mut hits = 0u32;
                    let mut i = 0u32;
                    while i < last {
                        if ring.get(cells, i) <= current {
                            hits += 1;
                        }
                        i += 1;
                    }
                    norm(100.0 * f64::from(hits) / f64::from(last))
                }
                _ => f64::NAN,
            }
        }

        /// Scalar model of `State::Stateless` cases for `ta.wvad`/`ta.iii`.
        pub(super) fn stateless<L: Lanes>(func: TaFunc, lanes: &L, bar: u32) -> f64 {
            match func {
                // ta.wvad: (close - open) / (high - low) * volume, per bar.
                TaFunc::Wvad => {
                    let o = finite(lanes.open(bar));
                    let h = finite(lanes.high(bar));
                    let l = finite(lanes.low(bar));
                    let c = finite(lanes.close(bar));
                    let v = finite(lanes.volume(bar));
                    if present(o) && present(h) && present(l) && present(c) && present(v) {
                        norm((c - o) / (h - l) * v)
                    } else {
                        f64::NAN
                    }
                }
                // ta.iii: (2*close - high - low) / ((high - low) * volume).
                TaFunc::Iii => {
                    let h = finite(lanes.high(bar));
                    let l = finite(lanes.low(bar));
                    let c = finite(lanes.close(bar));
                    let v = finite(lanes.volume(bar));
                    if present(h) && present(l) && present(c) && present(v) {
                        norm((2.0 * c - h - l) / ((h - l) * v))
                    } else {
                        f64::NAN
                    }
                }
                _ => f64::NAN,
            }
        }

        /// Scalar model of `State::Mfi` arm (`arg` as in
        /// `advance_state`).
        pub(super) fn mfi<L: Lanes>(
            prev: &mut f64,
            ring: &mut RefPairWindow,
            cells: &mut [f64],
            args: [f64; 3],
            lanes: &L,
            bar: u32,
        ) -> f64 {
            let arg = |i: usize| args.get(i).copied().unwrap_or(f64::NAN);
            let source = arg(0);
            let volume = finite(lanes.volume(bar));
            let (up, down) = if present(source) && present(volume) {
                // Money flows, split by the sign of the source change:
                // `volume * (change <= 0 ? 0 : src)` and
                // `volume * (change >= 0 ? 0 : src)`; an **na** change (first
                // bar, or after an na source) takes both else branches,
                // contributing `volume * src` to both flows.
                let change = if present(*prev) {
                    source - *prev
                } else {
                    f64::NAN
                };
                let up = if present(change) && change <= 0.0 {
                    0.0
                } else {
                    source
                };
                let down = if present(change) && change >= 0.0 {
                    0.0
                } else {
                    source
                };
                (volume * up, volume * down)
            } else {
                (f64::NAN, f64::NAN)
            };
            *prev = source;
            ring.push(cells, up, down);
            if ring.ready(cells) {
                let (mut upper, mut lower) = (0.0, 0.0);
                let mut i = 0u32;
                while i < ring.cap() {
                    let (u, d) = ring.get(cells, i);
                    upper += u;
                    lower += d;
                    i += 1;
                }
                // lower == 0 → upper/lower = inf → mfi = 100; 0/0 → na.
                norm(100.0 - (100.0 / (1.0 + upper / lower)))
            } else {
                f64::NAN
            }
        }

        /// Scalar model of `State::VolumeIndex` arm.
        pub(super) fn volume_index<L: Lanes>(
            func: TaFunc,
            value: &mut f64,
            prev_close: &mut f64,
            prev_volume: &mut f64,
            lanes: &L,
            bar: u32,
        ) -> f64 {
            let close = finite(lanes.close(bar));
            let volume = finite(lanes.volume(bar));
            // Starts at 1.0. The value is carried unchanged when either close
            // is zero or na; otherwise a volume decrease (nvi) or increase
            // (pvi) against the previous volume applies the close's rate of
            // change.
            if present(close) && present(*prev_close) && close != 0.0 && *prev_close != 0.0 {
                let prev_vol = if present(*prev_volume) {
                    *prev_volume
                } else {
                    0.0
                };
                let trigger = present(volume)
                    && match func {
                        TaFunc::Nvi => volume < prev_vol,
                        TaFunc::Pvi => volume > prev_vol,
                        _ => false,
                    };
                if trigger {
                    let next = *value + ((close - *prev_close) / *prev_close) * *value;
                    if next.is_finite() {
                        *value = next;
                    }
                }
            }
            *prev_close = close;
            *prev_volume = volume;
            norm(*value)
        }
    }

    /// `Bars` plus an `open` lane (the shared `Bars` reads open as na), for
    /// `ta.wvad`.
    struct OpenBars {
        bars: Bars,
        open: Vec<f64>,
    }

    impl Lanes for OpenBars {
        fn open(&self, bar: u32) -> f64 {
            lane(&self.open, bar)
        }
        fn high(&self, bar: u32) -> f64 {
            self.bars.high(bar)
        }
        fn low(&self, bar: u32) -> f64 {
            self.bars.low(bar)
        }
        fn close(&self, bar: u32) -> f64 {
            self.bars.close(bar)
        }
        fn volume(&self, bar: u32) -> f64 {
            self.bars.volume(bar)
        }
        fn time_ms(&self, bar: u32) -> i64 {
            self.bars.time_ms(bar)
        }
    }

    /// One site's expected and twin state for the differential below.
    struct Algos8Site {
        win: RefWindow,
        pair: algos5_ref::RefPairWindow,
        ring: crate::arena::Ring,
        pring: crate::arena::PairRing,
        cells: Vec<f64>,
        /// Expected (`.0`) and twin (`.1`) scalar state: mfi's previous
        /// source; nvi/pvi's value, previous close and previous volume.
        prev: (f64, f64),
        value: (f64, f64),
        prev_close: (f64, f64),
        prev_volume: (f64, f64),
    }

    /// Differential for one group of arms
    /// (`ta.math_sum`, `ta.swma`, `ta.cog`, `ta.percentrank`, `ta.wvad`,
    /// `ta.iii`, `ta.mfi`, `ta.nvi`, `ta.pvi`): the shipped engine
    /// (`Engine::advance`), the scalar arms and the generic twins at
    /// `f64` agree bar by bar (up to NaN payload), through warm-up, na
    /// holes, ±∞, raw bit patterns, flat windows (zero sums, ties, zero
    /// flows, unchanged closes and volumes) and clamped lengths. Every site
    /// whose output can be present produces present values, and nvi/pvi
    /// move off 1.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn generic_taalgos8_arms_match_the_reference() {
        use super::{
            cog_arm_generic, iii_arm_generic, math_sum_arm_generic, mfi_arm_generic,
            percent_rank_arm_generic, swma_arm_generic, volume_index_arm_generic, wvad_arm_generic,
        };
        let mut rng = Rng(0xbb67_ae85_84ca_a73b);
        let window_funcs = [
            TaFunc::MathSum,
            TaFunc::Swma,
            TaFunc::Cog,
            TaFunc::PercentRank,
        ];
        for param in [
            i64::MIN,
            -1,
            0,
            1,
            2,
            3,
            4,
            5,
            14,
            31,
            32,
            100,
            100_001,
            i64::MAX,
        ] {
            let mut decls = vec![
                Decl::new(TaFunc::Mfi, &[param]).unwrap(),
                Decl::new(TaFunc::Wvad, &[]).unwrap(),
                Decl::new(TaFunc::Iii, &[]).unwrap(),
                Decl::new(TaFunc::Nvi, &[]).unwrap(),
                Decl::new(TaFunc::Pvi, &[]).unwrap(),
            ];
            for func in window_funcs {
                decls.push(Decl::new(func, &[param]).unwrap());
            }
            let mut present = vec![0usize; decls.len()];
            let mut moved = vec![0usize; decls.len()];
            for mode in 0..5 {
                let n = 300;
                let lanes = OpenBars {
                    bars: bars(&mut rng, mode, n),
                    open: (0..n).map(|i| sample(&mut rng, mode, i + 4)).collect(),
                };
                let layout = Layout::plan(&decls);
                let mut states = vec![State::Invalid; decls.len()];
                let mut cells = vec![f64::NAN; layout.cells as usize];
                let mut scratch = vec![f64::NAN; layout.scratch as usize];
                let mut engine = Engine::init(&decls, &mut states, &mut cells, &mut scratch);
                let mut sites: Vec<Algos8Site> = decls
                    .iter()
                    .map(|d| {
                        let func = d.func().unwrap();
                        let length = crate::state::length_of(d);
                        // The shipped ring shapes (`state::site_shape`).
                        let cap = match func {
                            TaFunc::PercentRank => length.saturating_add(1),
                            TaFunc::Swma => 4,
                            _ => length,
                        };
                        Algos8Site {
                            win: RefWindow::new(cap),
                            pair: algos5_ref::RefPairWindow::new(cap),
                            ring: crate::arena::Ring::new(0, cap),
                            pring: crate::arena::PairRing::new(0, cap),
                            cells: vec![f64::NAN; 2 * cap as usize],
                            prev: (f64::NAN, f64::NAN),
                            value: (1.0, 1.0),
                            prev_close: (f64::NAN, f64::NAN),
                            prev_volume: (f64::NAN, f64::NAN),
                        }
                    })
                    .collect();
                for bar in 0..n as u32 {
                    let args = [
                        sample(&mut rng, mode, bar as usize),
                        sample(&mut rng, mode, bar as usize + 1),
                        sample(&mut rng, mode, bar as usize + 2),
                    ];
                    for (k, decl) in decls.iter().enumerate() {
                        let func = decl.func().unwrap();
                        let site = &mut sites[k];
                        let (old, twin) = match func {
                            TaFunc::Wvad => (
                                algos8_ref::stateless(func, &lanes, bar),
                                wvad_arm_generic(
                                    lanes.open(bar),
                                    lanes.high(bar),
                                    lanes.low(bar),
                                    lanes.close(bar),
                                    lanes.volume(bar),
                                ),
                            ),
                            TaFunc::Iii => (
                                algos8_ref::stateless(func, &lanes, bar),
                                iii_arm_generic(
                                    lanes.high(bar),
                                    lanes.low(bar),
                                    lanes.close(bar),
                                    lanes.volume(bar),
                                ),
                            ),
                            TaFunc::Mfi => (
                                algos8_ref::mfi(
                                    &mut site.prev.0,
                                    &mut site.pair,
                                    &mut [],
                                    args,
                                    &lanes,
                                    bar,
                                ),
                                mfi_arm_generic(
                                    &mut site.prev.1,
                                    &mut site.pring,
                                    &mut site.cells,
                                    args[0],
                                    lanes.volume(bar),
                                ),
                            ),
                            TaFunc::Nvi | TaFunc::Pvi => (
                                algos8_ref::volume_index(
                                    func,
                                    &mut site.value.0,
                                    &mut site.prev_close.0,
                                    &mut site.prev_volume.0,
                                    &lanes,
                                    bar,
                                ),
                                volume_index_arm_generic(
                                    func,
                                    &mut site.value.1,
                                    &mut site.prev_close.1,
                                    &mut site.prev_volume.1,
                                    lanes.close(bar),
                                    lanes.volume(bar),
                                ),
                            ),
                            _ => {
                                site.win.push(&mut [], args[0]);
                                let old = algos8_ref::window(func, &site.win, &[]);
                                let (ring, cells, x) = (&mut site.ring, &mut site.cells, args[0]);
                                let twin = match func {
                                    TaFunc::MathSum => math_sum_arm_generic(ring, cells, x),
                                    TaFunc::Swma => swma_arm_generic(ring, cells, x),
                                    TaFunc::Cog => cog_arm_generic(ring, cells, x),
                                    _ => percent_rank_arm_generic(ring, cells, x),
                                };
                                (old, twin)
                            }
                        };
                        let out = engine.advance(k as u32, args, &lanes, bar).unwrap();
                        present[k] += usize::from(!out.value.is_nan());
                        moved[k] += usize::from(out.value != 1.0);
                        let ctx = || std::format!("{func:?} {param} mode {mode} bar {bar}");
                        assert!(
                            same_f64(out.value, old),
                            "shipped {}: {} vs {}",
                            ctx(),
                            out.value,
                            old
                        );
                        assert!(same_f64(twin, old), "twin {}: {} vs {}", ctx(), twin, old);
                        // Single-output arms: both extra outputs stay na.
                        assert!(out.extra[0].is_nan() && out.extra[1].is_nan(), "{}", ctx());
                    }
                }
            }
            // Non-vacuity: every site whose output can be present within 300
            // bars produces present values, and nvi/pvi leave their start
            // value (the trigger and the rate-of-change step both run).
            for (k, decl) in decls.iter().enumerate() {
                let func = decl.func().unwrap();
                let len = crate::state::length_of(decl);
                let need = len <= 100
                    || matches!(
                        func,
                        TaFunc::Wvad | TaFunc::Iii | TaFunc::Nvi | TaFunc::Pvi | TaFunc::Swma
                    );
                assert!(
                    !need || present[k] > 0,
                    "{func:?} {param}: no present output"
                );
                if matches!(func, TaFunc::Nvi | TaFunc::Pvi) {
                    assert!(moved[k] > 0, "{func:?}: never moved off 1");
                }
            }
        }
    }

    /// The scalar `State::TimeframeChange` arm (`timeframe.change`):
    /// `bool_out` restated; the state place, decl, lanes and bar passed in.
    mod tfchange_ref {
        use super::Lanes;
        use crate::Decl;

        /// Scalar model of `engine::bool_out`.
        #[inline]
        fn bool_out(b: bool) -> f64 {
            if b {
                1.0
            } else {
                0.0
            }
        }

        /// Scalar model of `State::TimeframeChange` arm of `advance_state`.
        pub(super) fn timeframe_change<L: Lanes>(
            prev_bucket: &mut i64,
            decl: &Decl,
            lanes: &L,
            bar: u32,
        ) -> f64 {
            // The baked const param is the timeframe length in seconds (>= 1);
            // clamp defensively so the divisor is never zero.
            let tf_secs = decl.param(0).unwrap_or(1).max(1);
            let tf_ms = tf_secs.saturating_mul(1000);
            let bucket = lanes.time_ms(bar).div_euclid(tf_ms);
            let changed = *prev_bucket != bucket;
            *prev_bucket = bucket;
            bool_out(changed)
        }
    }

    /// A time lane only (prices read na), for `timeframe.change`.
    struct TimeBars {
        time: Vec<i64>,
    }

    impl Lanes for TimeBars {
        fn open(&self, _bar: u32) -> f64 {
            f64::NAN
        }
        fn high(&self, _bar: u32) -> f64 {
            f64::NAN
        }
        fn low(&self, _bar: u32) -> f64 {
            f64::NAN
        }
        fn close(&self, _bar: u32) -> f64 {
            f64::NAN
        }
        fn volume(&self, _bar: u32) -> f64 {
            f64::NAN
        }
        fn time_ms(&self, bar: u32) -> i64 {
            self.time.get(bar as usize).copied().unwrap_or(0)
        }
    }

    /// Integer-kernel differential for timeframe.change
    /// (`timeframe.change`): the shipped engine (`Engine::advance`), the
    /// scalar arm and the generic twin at `f64` agree bar by
    /// bar, bit for bit, with both extra outputs na. Timeframe params cover
    /// absent, `i64::MIN`, negative, 0 (all clamped to 1 s), ordinary
    /// lengths, the `saturating_mul` edge (`i64::MAX / 1000` and one more)
    /// and `i64::MAX`; times cover `i64::MIN`/`MAX`, negatives, 0, bucket
    /// edges, repeats, monotone runs and raw random bits. Non-vacuity: the
    /// first bar is a change, and every site sees both changed and unchanged
    /// bars.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn generic_tfchange_arm_matches_the_reference() {
        use super::timeframe_change_arm_generic;
        let mut rng = Rng(0x3c6e_f372_fe94_f82b);
        let sat = i64::MAX / 1000;
        let params: [Option<i64>; 13] = [
            None,
            Some(i64::MIN),
            Some(-86_400),
            Some(-1),
            Some(0),
            Some(1),
            Some(60),
            Some(3_600),
            Some(86_400),
            Some(sat),
            Some(sat + 1),
            Some(i64::MAX - 1),
            Some(i64::MAX),
        ];
        let decls: Vec<Decl> = params
            .iter()
            .map(|p| match p {
                Some(p) => Decl::new(TaFunc::TimeframeChange, &[*p]).unwrap(),
                None => Decl::new(TaFunc::TimeframeChange, &[]).unwrap(),
            })
            .collect();
        let edges = [
            i64::MIN,
            i64::MIN + 1,
            -1_000_000_000_000,
            -86_400_000,
            -60_001,
            -60_000,
            -59_999,
            -1_001,
            -1_000,
            -999,
            -1,
            0,
            1,
            999,
            1_000,
            1_001,
            59_999,
            60_000,
            86_399_999,
            86_400_000,
            1_700_000_000_000,
            sat * 1000 - 1,
            sat * 1000,
            i64::MAX - 1,
            i64::MAX,
        ];
        let mut changed = vec![0usize; decls.len()];
        let mut unchanged = vec![0usize; decls.len()];
        for mode in 0..4 {
            let n = 400usize;
            let time: Vec<i64> = match mode {
                // Edge values, each repeated so unchanged bars occur.
                0 => (0..n).map(|i| edges[(i / 2) % edges.len()]).collect(),
                // Raw random bits.
                1 => (0..n).map(|_| rng.next() as i64).collect(),
                // A monotone 1-minute-ish run through 0 with jitter and
                // repeats.
                2 => {
                    let mut t = -12_000_000i64;
                    (0..n)
                        .map(|_| {
                            t += (rng.next() % 61_000) as i64;
                            t
                        })
                        .collect()
                }
                // Random picks among the edges and small offsets of them.
                _ => (0..n)
                    .map(|_| {
                        let e = edges[(rng.next() % edges.len() as u64) as usize];
                        e.wrapping_add((rng.next() % 3) as i64 - 1)
                    })
                    .collect(),
            };
            let lanes = TimeBars { time };
            let layout = Layout::plan(&decls);
            let mut states = vec![State::Invalid; decls.len()];
            let mut cells = vec![f64::NAN; layout.cells as usize];
            let mut scratch = vec![f64::NAN; layout.scratch as usize];
            let mut engine = Engine::init(&decls, &mut states, &mut cells, &mut scratch);
            let mut prev_ref = vec![i64::MIN; decls.len()];
            let mut prev_twin = vec![i64::MIN; decls.len()];
            for bar in 0..n as u32 {
                for (k, decl) in decls.iter().enumerate() {
                    let old = tfchange_ref::timeframe_change(&mut prev_ref[k], decl, &lanes, bar);
                    let twin: f64 = timeframe_change_arm_generic(
                        &mut prev_twin[k],
                        decl.param(0),
                        lanes.time_ms(bar),
                    );
                    let out = engine
                        .advance(k as u32, [f64::NAN; 3], &lanes, bar)
                        .expect("timeframe.change site advances");
                    assert_eq!(
                        old.to_bits(),
                        out.value.to_bits(),
                        "engine mode {mode} bar {bar} param {:?}",
                        params[k]
                    );
                    assert_eq!(
                        old.to_bits(),
                        twin.to_bits(),
                        "twin mode {mode} bar {bar} param {:?}",
                        params[k]
                    );
                    assert_eq!(prev_ref[k], prev_twin[k]);
                    assert!(out.extra[0].is_nan() && out.extra[1].is_nan());
                    if bar == 0 {
                        assert_eq!(old, 1.0, "first bar is a change");
                    }
                    if old == 1.0 {
                        changed[k] += 1;
                    } else {
                        assert_eq!(old, 0.0);
                        unchanged[k] += 1;
                    }
                }
            }
        }
        for k in 0..decls.len() {
            assert!(
                changed[k] > 4 && unchanged[k] > 0,
                "param {:?}: changed {} unchanged {}",
                params[k],
                changed[k],
                unchanged[k]
            );
        }
    }

    /// The scalar short-window extremum code of `window_value`
    /// (`ta.highest`, `ta.lowest`, `ta.highestbars`, `ta.lowestbars`),
    /// with `norm` restated; the ring is the
    /// independent `RefWindow`.
    mod extremum_short_ref {
        use super::RefWindow;
        use crate::TaFunc;

        /// Scalar model of `engine::norm`.
        #[inline]
        fn norm(v: f64) -> f64 {
            if v.is_finite() {
                v
            } else {
                f64::NAN
            }
        }

        /// Scalar model of `window_value` for the four extremum functions after
        /// the push: the scalar fold, the readiness prologue and the
        /// `HighestBars | LowestBars` match case.
        pub(super) fn window_value(func: TaFunc, ring: &RefWindow, cells: &[f64]) -> f64 {
            // Scalar extrema ignore earlier missing values once a full bar window
            // exists, while a missing current source still yields `na`. This ordered
            // fold is also the signed-zero fallback for the monotonic cache above.
            if matches!(func, TaFunc::Highest | TaFunc::Lowest) {
                if !ring.full() || ring.last(cells).is_nan() {
                    return f64::NAN;
                }
                let highest = func == TaFunc::Highest;
                let mut best = None;
                let mut i = 0u32;
                while i < ring.cap() {
                    let value = ring.get(cells, i);
                    if !value.is_nan() {
                        best = Some(match best {
                            Some(previous) if highest => f64::max(previous, value),
                            Some(previous) => f64::min(previous, value),
                            None => value,
                        });
                    }
                    i += 1;
                }
                return best.map_or(f64::NAN, |b| norm(super::ref_canon(b)));
            }

            if !ring.window_ready(cells) {
                return f64::NAN;
            }
            let count = ring.cap();
            match func {
                TaFunc::HighestBars | TaFunc::LowestBars => {
                    // Offset (≤ 0) of the best bar; ties resolve to the most recent.
                    let mut best = 0u32;
                    let mut i = 0u32;
                    while i < count {
                        let x = ring.get(cells, i);
                        let incumbent = ring.get(cells, best);
                        let better = if func == TaFunc::HighestBars {
                            x >= incumbent
                        } else {
                            x <= incumbent
                        };
                        if better {
                            best = i;
                        }
                        i += 1;
                    }
                    -f64::from(count - 1 - best)
                }
                _ => f64::NAN,
            }
        }
    }

    const EXTREMUM_FUNCS: [TaFunc; 4] = [
        TaFunc::Highest,
        TaFunc::Lowest,
        TaFunc::HighestBars,
        TaFunc::LowestBars,
    ];

    /// What the extremum differentials saw (non-vacuity).
    #[derive(Default)]
    struct ExtremumStats {
        /// Present outputs per function.
        present: [usize; 4],
        /// na outputs.
        na: usize,
        /// `*bars` outputs whose best value occurs more than once.
        ties: usize,
        /// `*bars` outputs that are not the newest bar.
        older: usize,
        /// Scalar outputs equal to zero, by sign bit, short and long sites.
        pos_zero: [usize; 2],
        neg_zero: [usize; 2],
        /// Present scalar outputs over a window that holds an na cell.
        skipped_holes: usize,
        /// Scalar outputs that are na because the extremum is infinite.
        infinite: usize,
    }

    /// Feed `xs` to fresh sites of the four extremum functions at length
    /// `param` and check, bar by bar and bit for bit (na is the canonical
    /// `f64::NAN` everywhere): the scalar code on the test
    /// window, the shipped engine, the short-window twins, and the
    /// long-window twin all agree; and the `*bars` tie-break is exactly the
    /// most recent best bar.
    fn check_extremum_run(param: i64, xs: &[f64], stats: &mut ExtremumStats) {
        use super::{
            extremum_bars_short_arm_generic, extremum_short_arm_generic,
            window_extremum_arm_generic,
        };
        use crate::arena::{ExtremumWindow, Ring};
        let decls: Vec<Decl> = EXTREMUM_FUNCS
            .iter()
            .map(|f| Decl::new(*f, &[param]).unwrap())
            .collect();
        let cap = crate::state::length_of(&decls[0]);
        let long = usize::from(cap >= 32);
        let layout = Layout::plan(&decls);
        let mut states = vec![State::Invalid; decls.len()];
        let mut cells = vec![f64::NAN; layout.cells as usize];
        let mut scratch = vec![f64::NAN; layout.scratch as usize];
        let mut engine = Engine::init(&decls, &mut states, &mut cells, &mut scratch);
        let lanes = Bars {
            high: Vec::new(),
            low: Vec::new(),
            close: Vec::new(),
            volume: Vec::new(),
        };
        let mut win = RefWindow::new(cap);
        let mut short: Vec<(Ring, Vec<f64>)> = (0..4)
            .map(|_| (Ring::new(0, cap), vec![f64::NAN; cap as usize]))
            .collect();
        let mut queued: Vec<(ExtremumWindow, Vec<f64>)> = (0..4)
            .map(|_| (ExtremumWindow::new(0, cap), vec![f64::NAN; cap as usize]))
            .collect();
        for (bar, &x) in xs.iter().enumerate() {
            win.push(&mut [], x);
            for (k, &func) in EXTREMUM_FUNCS.iter().enumerate() {
                let highest = matches!(func, TaFunc::Highest | TaFunc::HighestBars);
                let bars = matches!(func, TaFunc::HighestBars | TaFunc::LowestBars);
                let old = extremum_short_ref::window_value(func, &win, &[]);
                let out = engine
                    .advance(k as u32, [x, f64::NAN, f64::NAN], &lanes, bar as u32)
                    .unwrap();
                let (ring, ring_cells) = &mut short[k];
                let twin = if bars {
                    extremum_bars_short_arm_generic(ring, ring_cells, x, highest)
                } else {
                    extremum_short_arm_generic(ring, ring_cells, x, highest)
                };
                let (window, window_cells) = &mut queued[k];
                let long_twin = window_extremum_arm_generic(window, window_cells, x, highest, bars);
                let ctx = || std::format!("{func:?} {param} bar {bar} of {xs:?}");
                assert_eq!(out.value.to_bits(), old.to_bits(), "shipped {}", ctx());
                assert_eq!(twin.to_bits(), old.to_bits(), "twin {}", ctx());
                assert_eq!(long_twin.to_bits(), old.to_bits(), "long twin {}", ctx());
                assert!(out.extra[0].is_nan() && out.extra[1].is_nan(), "{}", ctx());
                if old.is_nan() {
                    assert_eq!(old.to_bits(), f64::NAN.to_bits(), "{}", ctx());
                    stats.na += 1;
                    if !bars && win.full() && !x.is_nan() {
                        assert!(win.buf.iter().any(|v| v.is_infinite()), "{}", ctx());
                        stats.infinite += 1;
                    }
                    continue;
                }
                stats.present[k] += 1;
                if bars {
                    // An independent reading of the tie-break: the most
                    // recent cell that no other cell beats.
                    let last_best = (0..win.buf.len())
                        .rev()
                        .find(|&i| {
                            win.buf.iter().all(|&v| {
                                if highest {
                                    win.buf[i] >= v
                                } else {
                                    win.buf[i] <= v
                                }
                            })
                        })
                        .unwrap();
                    let want = -((win.buf.len() - 1 - last_best) as f64);
                    assert_eq!(old.to_bits(), want.to_bits(), "tie-break {}", ctx());
                    let copies = win.buf.iter().filter(|&&v| v == win.buf[last_best]).count();
                    stats.ties += usize::from(copies > 1);
                    stats.older += usize::from(last_best + 1 != win.buf.len());
                } else {
                    stats.skipped_holes += usize::from(win.buf.iter().any(|v| v.is_nan()));
                    if old == 0.0 {
                        if old.is_sign_negative() {
                            stats.neg_zero[long] += 1;
                        } else {
                            stats.pos_zero[long] += 1;
                        }
                    }
                }
            }
        }
    }

    /// Differential for ta.highest, ta.lowest, ta.highestbars and
    /// ta.lowestbars: the scalar `window_value` fold and bars case, the
    /// shipped engine
    /// (`Engine::advance`: `State::Window` below length 32,
    /// `State::WindowExtremum` from 32), the short-window twins and the
    /// long-window twin agree bar by bar, bit for bit. Lengths cover 1 to 40
    /// and the clamped and invalid ones; inputs cover random values,
    /// specials (NaN, ±∞, ±0, MAX/MIN, subnormal), raw bit patterns, flat
    /// stretches with and without na holes, signed zeros, two-value ties,
    /// infinities, monotone runs, and an na (or payload-NaN) hole swept
    /// through every window position.
    #[test]
    fn generic_extremum_short_matches_the_reference() {
        const ZEROS: [f64; 5] = [0.0, -0.0, 0.0, -0.0, f64::NAN];
        const INFS: [f64; 6] = [
            f64::INFINITY,
            f64::NEG_INFINITY,
            0.0,
            -0.0,
            1.0,
            f64::from_bits(0xfff8_0000_0000_1234),
        ];
        let mut rng = Rng(0x3c6e_f372_fe94_f82b);
        let mut total = ExtremumStats::default();
        let lengths = (1..=40).chain([i64::MIN, -1, 0, 100, 100_001, i64::MAX]);
        for param in lengths {
            let cap = param.clamp(1, 100_000) as usize;
            let mut stats = ExtremumStats::default();
            for mode in 0..10u32 {
                let n = 200;
                let xs: Vec<f64> = (0..n)
                    .map(|i| match mode {
                        0..=4 => sample(&mut rng, mode, i),
                        5 => ZEROS[(rng.next() % 5) as usize],
                        6 => (rng.next() % 2) as f64 + 1.0,
                        7 => INFS[(rng.next() % 6) as usize],
                        8 => i as f64,
                        _ => -(i as f64),
                    })
                    .collect();
                check_extremum_run(param, &xs, &mut stats);
            }
            // One hole swept through every window position: with a hole
            // every `cap + 1` bars, each full window holds at most one hole
            // and every position is hit.
            if cap <= 40 {
                for phase in 0..=cap {
                    for flavour in 0..3u64 {
                        let hole = if phase % 2 == 0 {
                            f64::NAN
                        } else {
                            f64::from_bits(0x7ff8_0000_0000_0abc)
                        };
                        let xs: Vec<f64> = (0..3 * cap + 3)
                            .map(|i| {
                                let raw = rng.next();
                                if i % (cap + 1) == phase {
                                    hole
                                } else if flavour == 0 {
                                    (raw % 3) as f64
                                } else if flavour == 1 {
                                    ZEROS[(raw % 4) as usize]
                                } else {
                                    (raw % 2001) as f64 * 0.5 - 500.0
                                }
                            })
                            .collect();
                        check_extremum_run(param, &xs, &mut stats);
                    }
                }
            }
            // Non-vacuity, per length: all four functions produce present
            // values, and na, whenever the window can fill.
            if cap <= 100 {
                assert!(
                    stats.present.iter().all(|&c| c > 0) && stats.na > 0,
                    "{param}: present {:?} na {}",
                    stats.present,
                    stats.na
                );
            }
            if (2..=40).contains(&cap) {
                assert!(
                    stats.ties > 0 && stats.older > 0 && stats.skipped_holes > 0,
                    "{param}: ties {} older {} holes {}",
                    stats.ties,
                    stats.older,
                    stats.skipped_holes
                );
                let long = usize::from(cap >= 32);
                assert!(
                    stats.pos_zero[long] > 0 && stats.neg_zero[long] == 0 && stats.infinite > 0,
                    "{param}: +0 {:?} -0 {:?} infinite {}",
                    stats.pos_zero,
                    stats.neg_zero,
                    stats.infinite
                );
            }
            for k in 0..4 {
                total.present[k] += stats.present[k];
            }
            for k in 0..2 {
                total.pos_zero[k] += stats.pos_zero[k];
                total.neg_zero[k] += stats.neg_zero[k];
            }
        }
        // Both sides of the 32 switch reach a zero extremum (the long side
        // through the arm's zero branch).
        assert!(total.present.iter().all(|&c| c > 10_000));
        assert!(total.pos_zero.iter().all(|&c| c > 0));
        // Window extrema never return `-0.0`.
        assert!(total.neg_zero.iter().all(|&c| c == 0));
    }

    /// Exhaustive small-case check: for every window length up to
    /// 4, every input sequence of `length + 2` bars over
    /// {na, -0.0, +0.0, 1.0, 2.0, payload NaN, +∞} (so every window, through
    /// warm-up and two slides), scalar vs shipped vs twins, bit for
    /// bit.
    #[test]
    fn extremum_short_exhaustive_small_windows_match_the_reference() {
        const ALPHABET: [f64; 7] = [
            f64::NAN,
            -0.0,
            0.0,
            1.0,
            2.0,
            f64::from_bits(0x7ff8_0000_0000_0abc),
            f64::INFINITY,
        ];
        let mut stats = ExtremumStats::default();
        let mut runs = 0usize;
        for length in 1..=4usize {
            let n = length + 2;
            let mut digits = vec![0usize; n];
            loop {
                let xs: Vec<f64> = digits.iter().map(|&d| ALPHABET[d]).collect();
                check_extremum_run(length as i64, &xs, &mut stats);
                runs += 1;
                let mut pos = 0;
                while pos < n && digits[pos] == ALPHABET.len() - 1 {
                    digits[pos] = 0;
                    pos += 1;
                }
                if pos == n {
                    break;
                }
                digits[pos] += 1;
            }
        }
        assert_eq!(
            runs,
            7usize.pow(3) + 7usize.pow(4) + 7usize.pow(5) + 7usize.pow(6)
        );
        assert!(
            stats.present.iter().all(|&c| c > 1000),
            "{:?}",
            stats.present
        );
        assert!(stats.ties > 1000 && stats.older > 1000 && stats.skipped_holes > 1000);
        assert!(stats.pos_zero[0] > 100 && stats.neg_zero[0] == 0 && stats.infinite > 1000);
    }
    /// Const params for `func` with the caller's window lengths (`first`
    /// for the first const param, `rest` for later lengths).
    fn zero_tie_params(func: TaFunc, first: i64, rest: i64) -> Vec<i64> {
        use crate::TaParamRule;
        func.engine_param_rules()
            .iter()
            .enumerate()
            .map(|(i, rule)| match rule {
                TaParamRule::Length { min } => (*min).max(if i == 0 { first } else { rest }),
                TaParamRule::IntRange { min, max } => (*min).max(1).min(*max),
                TaParamRule::FloatBits { positive } => {
                    (if *positive { 6.0f64 } else { 2.0f64 }).to_bits() as i64
                }
                TaParamRule::Percent => 85.0f64.to_bits() as i64,
                TaParamRule::Bool01 => i64::from(i % 2 == 0),
            })
            .collect()
    }

    /// How a zero-tie run treats `max`/`min`: the model of the target's
    /// `f64::max`/`f64::min`, and whether the run is the shipped engine, the
    /// variant that uses plain `f64::max`/`f64::min`, or the engine with IEEE
    /// `maximum`/`minimum` at every site (`ieee`).
    #[derive(Clone, Copy)]
    struct TieMode {
        platform: crate::state::tie_policy::Policy,
        legacy: bool,
        ieee: bool,
    }

    impl TieMode {
        fn shipped(platform: crate::state::tie_policy::Policy) -> TieMode {
            TieMode {
                platform,
                legacy: false,
                ieee: false,
            }
        }
    }

    /// One engine run of `decls` under `mode`. Per output: the three result
    /// elements, and how many mixed-zero `max`/`min` ties that site had met
    /// by then.
    fn zero_tie_run(
        mode: TieMode,
        decls: &[Decl],
        operands: &[[f64; 3]],
        lanes: &Bars,
    ) -> Vec<([f64; 3], u64)> {
        use crate::state::tie_policy;
        let layout = Layout::plan(decls);
        let mut states = vec![State::Invalid; decls.len()];
        let mut cells = vec![f64::NAN; layout.cells as usize];
        let mut scratch = vec![f64::NAN; layout.scratch as usize];
        let mut engine = Engine::init(decls, &mut states, &mut cells, &mut scratch);
        let mut ties = vec![0u64; decls.len()];
        let mut out = Vec::new();
        let platform_before = tie_policy::set_platform(mode.platform);
        let baseline_before = tie_policy::set_plain_max_min(mode.legacy);
        let ieee_before = tie_policy::set_ieee(mode.ieee);
        for (bar, row) in operands.iter().enumerate() {
            for (site, tie_count) in ties.iter_mut().enumerate() {
                let args = [row[site % 3], row[(site + 1) % 3], row[(site + 2) % 3]];
                let seen = tie_policy::mixed_ties();
                let o = engine
                    .advance(site as u32, args, lanes, bar as u32)
                    .unwrap();
                *tie_count += tie_policy::mixed_ties() - seen;
                out.push(([o.value, o.extra[0], o.extra[1]], *tie_count));
            }
        }
        tie_policy::set_platform(platform_before);
        tie_policy::set_plain_max_min(baseline_before);
        tie_policy::set_ieee(ieee_before);
        out
    }

    /// The functions whose `max`/`min` on data stay plain
    /// `f64::max`/`f64::min` (`max_raw`/`min_raw`) with no sign decision.
    const RAW_MAX_MIN_FUNCS: [&str; 3] = ["ta.cmo", "ta.rsi", "ta.wad"];

    /// The window-fold functions, whose zero extremes are canonicalized to
    /// `+0.0`: a zero result can change sign without any mixed-zero tie
    /// (a window whose zeros are all `-0.0`).
    const WINDOW_FOLD_FUNCS: [&str; 5] =
        ["ta.highest", "ta.lowest", "ta.range", "ta.stoch", "ta.wpr"];

    /// The offset functions share the windows but return indices: unchanged.
    const OFFSET_FUNCS: [&str; 2] = ["ta.highestbars", "ta.lowestbars"];

    const TIE_POLICIES: [crate::state::tie_policy::Policy; 4] = [
        crate::state::tie_policy::Policy::HostF64,
        crate::state::tie_policy::Policy::LeftWins,
        crate::state::tie_policy::Policy::RightWins,
        crate::state::tie_policy::Policy::Scrambled,
    ];

    /// What the zero-tie checks saw.
    #[derive(Default)]
    struct ZeroTieTally {
        /// Per tie-break: functions with a changed output.
        changed: [std::collections::BTreeSet<&'static str>; 4],
        changed_outputs: [usize; 4],
        /// Functions whose `max`/`min` met a mixed-zero tie at all.
        tied: std::collections::BTreeSet<&'static str>,
        exact: usize,
        after_tie: usize,
        parity: usize,
        window_folds: usize,
    }

    /// One feed through the three claims of
    /// `signed_zero_tie_fix_changes_only_the_sign_of_zero_results`.
    fn zero_tie_check(
        funcs: &[TaFunc],
        decls: &[Decl],
        operands: &[[f64; 3]],
        lanes: &Bars,
        label: &str,
        tally: &mut ZeroTieTally,
    ) {
        use crate::state::tie_policy::Policy;
        let new = zero_tie_run(TieMode::shipped(Policy::HostF64), decls, operands, lanes);
        for (i, (_, ties)) in new.iter().enumerate() {
            if *ties > 0 {
                tally.tied.insert(funcs[i % funcs.len()].name());
            }
        }
        let same = |what: &str, policy: Policy, other: &[([f64; 3], u64)]| {
            assert_eq!(new.len(), other.len());
            for (i, ((n_out, _), (o_out, _))) in new.iter().zip(other).enumerate() {
                for e in 0..3 {
                    assert!(
                        n_out[e].to_bits() == o_out[e].to_bits(),
                        "{what} {:?} {label} bar {} {policy:?}: host {n_out:?} other {o_out:?}",
                        funcs[i % funcs.len()],
                        i / funcs.len()
                    );
                }
            }
            3 * new.len()
        };
        for (k, policy) in TIE_POLICIES.into_iter().enumerate() {
            // 1. Parity: the shipped engine on another target, and the
            // engine with IEEE `maximum`/`minimum` at every site.
            if policy != Policy::HostF64 {
                let other = zero_tie_run(TieMode::shipped(policy), decls, operands, lanes);
                tally.parity += same("parity", policy, &other);
            }
            let ieee = TieMode {
                platform: policy,
                legacy: false,
                ieee: true,
            };
            tally.parity += same("ieee", policy, &zero_tie_run(ieee, decls, operands, lanes));
            // 2. Against the variant that uses plain f64::max/f64::min on that target.
            let legacy = TieMode {
                platform: policy,
                legacy: true,
                ieee: false,
            };
            let old = zero_tie_run(legacy, decls, operands, lanes);
            assert_eq!(new.len(), old.len());
            for (i, ((n_out, n_ties), (o_out, o_ties))) in new.iter().zip(&old).enumerate() {
                let func = funcs[i % funcs.len()];
                let ctx = || {
                    std::format!(
                        "{func:?} {label} bar {} {policy:?}: new {n_out:?} old {o_out:?}",
                        i / funcs.len()
                    )
                };
                for e in 0..3 {
                    let (a, b) = (n_out[e], o_out[e]);
                    let identical = a.to_bits() == b.to_bits();
                    if RAW_MAX_MIN_FUNCS.contains(&func.name())
                        || OFFSET_FUNCS.contains(&func.name())
                    {
                        // 3. Plain `f64::max` before and after; offsets
                        // are indices.
                        assert!(identical, "{}", ctx());
                    }
                    if matches!(func, TaFunc::Highest | TaFunc::Lowest) {
                        // Window extrema never return `-0.0`.
                        assert!(!(a == 0.0 && a.is_sign_negative()), "{}", ctx());
                    }
                    if WINDOW_FOLD_FUNCS.contains(&func.name()) {
                        // A zero extreme is `+0.0` now, tie or no tie.
                        assert!(identical || (a == 0.0 && b == 0.0), "{}", ctx());
                        tally.window_folds += 1;
                        if !identical {
                            tally.changed[k].insert(func.name());
                            tally.changed_outputs[k] += 1;
                        }
                    } else if *n_ties == 0 {
                        // No mixed-zero tie yet at this site: the plain form
                        // met none either, and every bit (the NaN pattern
                        // included) is unchanged.
                        assert_eq!(*o_ties, 0, "{}", ctx());
                        assert!(identical, "{}", ctx());
                        tally.exact += 1;
                    } else {
                        assert!(identical || (a == 0.0 && b == 0.0), "{}", ctx());
                        tally.after_tie += 1;
                        if !identical {
                            tally.changed[k].insert(func.name());
                            tally.changed_outputs[k] += 1;
                        }
                    }
                }
            }
        }
    }

    /// The signed-zero tie-break changes output signs, and this pins exactly
    /// what it does. For every engine function, window lengths on
    /// both sides of 32, and the input classes of the extremum differential
    /// (random, specials, raw bit patterns, flat with and without holes,
    /// signed zeros, two-value ties, infinities with a payload NaN, rising
    /// and falling runs) plus dense signed zeros, on the operands and on
    /// every market lane, and a `ta.vwap` feed whose variance underflows to
    /// `-0.0`:
    ///
    /// 1. **Parity.** The shipped engine's outputs do not depend on how the
    ///    target's `f64::max`/`f64::min` break the `+0.0`/`-0.0` tie: this
    ///    host's, left operand wins (`libm`'s `fmax`), right operand wins,
    ///    and a pseudo-random choice per call (a compiler may swap the
    ///    operands per call site) all give
    ///    the same bits on every output of every function. They are also
    ///    the bits of the engine with IEEE `maximum`/`minimum` at every
    ///    site, so the places that use plain `f64::max`/`f64::min` (the
    ///    first fold of the window functions, the folds whose zero sign is
    ///    left undecided, and `ta.rsi`, `ta.cmo`, `ta.wad`) lose nothing.
    /// 2. **Only zero signs change.** Against the variant that uses plain
    ///    `f64::max`/`f64::min` (the same engine with the deterministic helpers
    ///    turned back into plain `f64::max`/`f64::min`, under each of the four
    ///    tie-breaks): a site
    ///    that has not yet met a mixed-zero tie is bit-identical; after one,
    ///    the outputs still agree except that a zero result may have the
    ///    other sign. No other value, and no na, ever changes.
    /// 3. **Which functions.** The functions that keep plain `f64::max`
    ///    are unchanged outright, and the set of functions that ever meet a
    ///    tie is pinned.
    #[test]
    fn signed_zero_tie_fix_changes_only_the_sign_of_zero_results() {
        const ZEROS: [f64; 5] = [0.0, -0.0, 0.0, -0.0, f64::NAN];
        const DENSE: [f64; 6] = [0.0, -0.0, -0.0, 0.0, 1.0, -1.0];
        const INFS: [f64; 6] = [
            f64::INFINITY,
            f64::NEG_INFINITY,
            0.0,
            -0.0,
            1.0,
            f64::from_bits(0xfff8_0000_0000_1234),
        ];
        fn series(rng: &mut Rng, mode: u32, n: usize, salt: usize) -> Vec<f64> {
            (0..n)
                .map(|i| match mode {
                    0..=4 => sample(rng, mode, i + salt),
                    5 => ZEROS[(rng.next() % 5) as usize],
                    6 => (rng.next() % 2) as f64 + 1.0,
                    7 => INFS[(rng.next() % 6) as usize],
                    8 => i as f64,
                    9 => -(i as f64),
                    10 => DENSE[(rng.next() % 4) as usize],
                    _ => DENSE[(rng.next() % 6) as usize],
                })
                .collect()
        }
        let funcs: Vec<TaFunc> = TaFunc::ALL
            .iter()
            .copied()
            .filter(|f| f.is_engine())
            .collect();
        assert!(funcs.len() > 60);
        let mut rng = Rng(0x1f83_d9ab_fb41_bd6b);
        let mut tally = ZeroTieTally::default();
        for (first, rest) in [
            (1i64, 1i64),
            (2, 2),
            (3, 2),
            (5, 3),
            (9, 4),
            (31, 5),
            (32, 7),
            (40, 33),
        ] {
            let decls: Vec<Decl> = funcs
                .iter()
                .map(|f| Decl::new(*f, &zero_tie_params(*f, first, rest)).unwrap())
                .collect();
            for mode in 0..12u32 {
                let n = 160;
                let columns = [
                    series(&mut rng, mode, n, 0),
                    series(&mut rng, mode, n, 1),
                    series(&mut rng, mode, n, 2),
                ];
                let operands: Vec<[f64; 3]> = (0..n)
                    .map(|i| [columns[0][i], columns[1][i], columns[2][i]])
                    .collect();
                let lanes = Bars {
                    high: series(&mut rng, mode, n, 0),
                    low: series(&mut rng, mode, n, 1),
                    close: series(&mut rng, mode, n, 2),
                    volume: series(&mut rng, mode, n, 3),
                };
                let label = std::format!("({first}, {rest}) mode {mode}");
                zero_tie_check(&funcs, &decls, &operands, &lanes, &label, &mut tally);
            }
        }
        // The functions the general feeds drive into a mixed-zero tie.
        let names = |set: &std::collections::BTreeSet<&'static str>| {
            set.iter().copied().collect::<Vec<_>>()
        };
        assert!(!tally.tied.contains("ta.vwap"));
        // The `ta.vwap` bands meet the tie only when `p2v / vol` is `-0.0`:
        // a negative volume sum and a `source² · volume` that underflows.
        // (source, anchor, mult) per bar, with the volume lane beside it:
        // the sign of the zero variance reaches the bands.
        let vwap = [TaFunc::VwapBands];
        let vwap_decls =
            [Decl::new(TaFunc::VwapBands, &zero_tie_params(TaFunc::VwapBands, 1, 1)).unwrap()];
        let vwap_lanes = Bars {
            high: Vec::new(),
            low: Vec::new(),
            close: Vec::new(),
            volume: vec![1e300, -1.0, -1.0, 1e300, -1.0],
        };
        let vwap_operands = [
            [0.0, 1.0, 1.0],
            [1e-161, 0.0, 1.0],
            [-1e-161, 0.0, -1.0],
            [0.0, 1.0, 2.0],
            [1e-162, 0.0, 2.0],
        ];
        zero_tie_check(
            &vwap,
            &vwap_decls,
            &vwap_operands,
            &vwap_lanes,
            "vwap underflow",
            &mut tally,
        );
        assert!(tally.tied.contains("ta.vwap"));
        assert!(tally.changed[1..].iter().any(|set| set.contains("ta.vwap")));
        // Non-vacuity: plenty of each regime, and the modelled tie-breaks
        // do change outputs.
        assert!(
            tally.exact > 1_000_000 && tally.after_tie > 100_000 && tally.parity > 1_000_000,
            "{} {} {}",
            tally.exact,
            tally.after_tie,
            tally.parity
        );
        assert!(
            tally.changed_outputs[1..].iter().all(|&c| c > 1000),
            "{:?}",
            tally.changed_outputs
        );
        // Pinned: these are the only engine functions whose
        // `max`/`min` ever sees `+0.0` against `-0.0`. Every other function
        // is bit-identical always.
        assert_eq!(
            names(&tally.tied),
            [
                "ta.atr",
                "ta.cmo",
                "ta.dmi",
                "ta.highest",
                "ta.kc",
                "ta.kcw",
                "ta.lowest",
                "ta.max",
                "ta.min",
                "ta.range",
                "ta.rsi",
                "ta.sar",
                "ta.stoch",
                "ta.supertrend",
                "ta.tr",
                "ta.vwap",
                "ta.wad",
                "ta.wpr",
            ]
        );
        // Of those, the ones whose own result is a `max`/`min` of the data
        // show the change under every modelled tie-break.
        for set in &tally.changed[1..] {
            assert!(set
                .iter()
                .all(|f| tally.tied.contains(f) || WINDOW_FOLD_FUNCS.contains(f)));
            assert!(RAW_MAX_MIN_FUNCS.iter().all(|f| !set.contains(f)));
            for name in [
                "ta.highest",
                "ta.lowest",
                "ta.max",
                "ta.min",
                "ta.sar",
                "ta.stoch",
                "ta.wpr",
            ] {
                assert!(set.contains(name), "{name}: {set:?}");
            }
        }
        // On any host, a window whose zeros are all `-0.0` changes.
        assert!(tally.changed[0].contains("ta.lowest") && tally.window_folds > 1_000_000);
        std::eprintln!(
            "changed {:?} {:?} exact {} after {} parity {} window folds {}",
            tally.changed.each_ref().map(names),
            tally.changed_outputs,
            tally.exact,
            tally.after_tie,
            tally.parity,
            tally.window_folds
        );
    }

    /// `ta.rsi`, `ta.cmo` and `ta.wad` keep plain `f64::max`/`f64::min`.
    /// They meet the mixed-zero tie constantly (`max(-change, 0.0)` on every
    /// flat bar), so the claim that the tie never reaches an output is
    /// checked exhaustively on small cases rather than on feeds that happen
    /// to tie: every input sequence over signed zeros, subnormals of both
    /// signs, `±1e300`, `+∞` and na, for lengths 1 to 4 (`ta.rsi`,
    /// `ta.cmo`: `length + 2` bars) and for three bars of high/low/close
    /// (`ta.wad`). Under all four tie-break models the outputs are the same
    /// bits, and they are the bits of the engine with IEEE
    /// `maximum`/`minimum` at every site.
    #[test]
    fn raw_max_min_sites_are_exhaustively_independent_of_the_tie_break() {
        use crate::state::tie_policy::Policy;
        const ALPHABET: [f64; 8] = [
            0.0,
            -0.0,
            5e-324,
            -5e-324,
            1e300,
            -1e300,
            f64::INFINITY,
            f64::NAN,
        ];
        fn check(
            decls: &[Decl],
            operands: &[[f64; 3]],
            lanes: &Bars,
            ties: &mut [u64],
            outputs: &mut usize,
        ) {
            let host = zero_tie_run(TieMode::shipped(Policy::HostF64), decls, operands, lanes);
            let last = host.len() - decls.len();
            for (site, tie_count) in ties.iter_mut().enumerate() {
                *tie_count += host[last + site].1;
            }
            let modes = [
                TieMode::shipped(Policy::LeftWins),
                TieMode::shipped(Policy::RightWins),
                TieMode::shipped(Policy::Scrambled),
                TieMode {
                    platform: Policy::HostF64,
                    legacy: false,
                    ieee: true,
                },
            ];
            for mode in modes {
                let other = zero_tie_run(mode, decls, operands, lanes);
                for (i, ((h, _), (o, _))) in host.iter().zip(&other).enumerate() {
                    for e in 0..3 {
                        assert!(
                            h[e].to_bits() == o[e].to_bits(),
                            "site {} bar {} {:?} ieee {}: {h:?} vs {o:?}, operands {operands:?}, \
                             high {:?} low {:?} close {:?}",
                            i % decls.len(),
                            i / decls.len(),
                            mode.platform,
                            mode.ieee,
                            lanes.high,
                            lanes.low,
                            lanes.close
                        );
                        *outputs += 1;
                    }
                }
            }
        }
        let no_lanes = Bars {
            high: Vec::new(),
            low: Vec::new(),
            close: Vec::new(),
            volume: Vec::new(),
        };
        let mut ties = [0u64; 2];
        let mut outputs = 0usize;
        for length in 1..=4usize {
            let decls = [
                Decl::new(TaFunc::Rsi, &[length as i64]).unwrap(),
                Decl::new(TaFunc::Cmo, &[length as i64]).unwrap(),
            ];
            let n = length + 2;
            let mut digits = vec![0usize; n];
            loop {
                let operands: Vec<[f64; 3]> = digits.iter().map(|&d| [ALPHABET[d]; 3]).collect();
                check(&decls, &operands, &no_lanes, &mut ties, &mut outputs);
                let mut pos = 0;
                while pos < n && digits[pos] == ALPHABET.len() - 1 {
                    digits[pos] = 0;
                    pos += 1;
                }
                if pos == n {
                    break;
                }
                digits[pos] += 1;
            }
        }
        assert!(ties.iter().all(|&t| t > 10_000), "{ties:?}");
        // `ta.wad`: bar 0 only contributes its close; bar 1 is every
        // (high, low, close); bar 2 every (high, low, close) over the
        // finite half of the alphabet.
        let wad = [Decl::new(TaFunc::Wad, &[]).unwrap()];
        let mut wad_ties = [0u64; 1];
        let operands = [[f64::NAN; 3]; 3];
        for code in 0..8usize.pow(4) * 5usize.pow(3) {
            let pick = |shift: usize, base: usize| ALPHABET[(code / shift) % base];
            let lanes = Bars {
                high: vec![1.0, pick(8, 8), pick(4096, 5)],
                low: vec![-1.0, pick(64, 8), pick(4096 * 5, 5)],
                close: vec![pick(1, 8), pick(512, 8), pick(4096 * 25, 5)],
                volume: Vec::new(),
            };
            check(&wad, &operands, &lanes, &mut wad_ties, &mut outputs);
        }
        assert!(wad_ties[0] > 10_000, "{wad_ties:?}");
        assert!(outputs > 10_000_000, "{outputs}");
    }
}
