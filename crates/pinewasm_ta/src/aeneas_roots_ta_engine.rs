//! Charon/Aeneas extraction roots for the `TaEngine` extraction (per-site state
//! footprints and `Engine::reset`'s base assignment). Compiled only with
//! `--features aeneas`; every item is a free function because `--start-from`
//! cannot name inherent methods in the pinned Charon version.
//!
//! The roots reach only the `f64`-free half of a reset: `Shape` and the
//! integer ring constructors. Turning a shape into a [`crate::State`] (the
//! float initial values) and the NaN fill of the cell array stay outside.

use crate::state::{init_shape, reset_sites, site_shape, Shape, ShapeSink};
use crate::Decl;

/// Root for `state::init_shape`: the rings one site's initial state takes.
pub fn ta_init_shape(decl: &Decl, base: u32) -> Shape {
    init_shape(decl, base)
}

/// Root for `state::site_shape`: one reset step (shape and next base).
pub fn ta_site_shape(decl: Option<&Decl>, base: u32) -> (Shape, u32) {
    site_shape(decl, base)
}

/// Root for `state::reset_sites`, the site loop `Engine::reset` runs (with
/// the sink `StateSlots`); generic over the sink.
pub fn ta_reset_sites<S: ShapeSink>(decls: &[Decl], sink: &mut S) {
    reset_sites(decls, sink);
}

/// Root for the generic `State::Smooth` arm `engine::smooth_arm_generic`
/// (`ta.ema`/`ta.rma`/`ta.atr`).
#[allow(clippy::too_many_arguments)]
pub fn ta_smooth_arm<F: crate::state::FloatOps>(
    s: &mut crate::state::Smoother<F>,
    ta_func: crate::TaFunc,
    length: u32,
    src: F,
    high: F,
    low: F,
    first: bool,
    prev_close: F,
) -> F {
    crate::engine::smooth_arm_generic(s, ta_func, length, src, high, low, first, prev_close)
}

/// Root for the generic true range `engine::true_range_generic`.
pub fn ta_true_range<F: crate::state::FloatOps>(
    high: F,
    low: F,
    first: bool,
    prev_close: F,
    handle_na: bool,
) -> F {
    crate::engine::true_range_generic(high, low, first, prev_close, handle_na)
}

/// Root for the generic `State::Rsi` arm `engine::rsi_arm_generic`.
pub fn ta_rsi_arm<F: crate::state::FloatOps>(
    prev: &mut F,
    up: &mut crate::state::Smoother<F>,
    down: &mut crate::state::Smoother<F>,
    x: F,
    length: u32,
) -> F {
    crate::engine::rsi_arm_generic(prev, up, down, x, length)
}

/// Root for the `f64` instance of the smoothing arm (`ta.atr` at `f64`).
#[allow(clippy::too_many_arguments)]
pub fn ta_smooth_arm_f64(
    s: &mut crate::state::Smoother<f64>,
    ta_func: crate::TaFunc,
    length: u32,
    src: f64,
    high: f64,
    low: f64,
    first: bool,
    prev_close: f64,
) -> f64 {
    crate::engine::smooth_arm_generic(s, ta_func, length, src, high, low, first, prev_close)
}

/// Root for the `f64` instance of the rsi arm.
pub fn ta_rsi_arm_f64(
    prev: &mut f64,
    up: &mut crate::state::Smoother<f64>,
    down: &mut crate::state::Smoother<f64>,
    x: f64,
    length: u32,
) -> f64 {
    crate::engine::rsi_arm_generic(prev, up, down, x, length)
}

/// Root for the generic `ta.tr` arm `engine::tr_arm_generic`.
pub fn ta_tr_arm<F: crate::state::FloatOps>(
    high: F,
    low: F,
    first: bool,
    prev_close: F,
    handle_na: bool,
) -> F {
    crate::engine::tr_arm_generic(high, low, first, prev_close, handle_na)
}

/// Root for the generic `State::Fixnan` arm.
pub fn ta_fixnan_arm<F: crate::state::FloatOps>(last: &mut F, x: F) -> F {
    crate::engine::fixnan_arm_generic(last, x)
}

/// Root for the generic `State::Cum` arm.
pub fn ta_cum_arm<F: crate::state::FloatOps>(total: &mut F, x: F) -> F {
    crate::engine::cum_arm_generic(total, x)
}

/// Root for the generic `State::Extremum` arm (`ta.max`/`ta.min`).
pub fn ta_extremum_arm<F: crate::state::FloatOps>(value: &mut F, x: F, is_max: bool) -> F {
    crate::engine::extremum_arm_generic(value, x, is_max)
}

/// Root for the generic `State::Cross` arm.
pub fn ta_cross_arm<F: crate::state::FloatOps>(
    ta_func: crate::TaFunc,
    prev_a: &mut F,
    prev_b: &mut F,
    seen: &mut bool,
    a: F,
    b: F,
) -> F {
    crate::engine::cross_arm_generic(ta_func, prev_a, prev_b, seen, a, b)
}

/// Root for the generic `State::Macd` arm.
#[allow(clippy::too_many_arguments)]
pub fn ta_macd_arm<F: crate::state::FloatOps>(
    fast: &mut crate::state::Smoother<F>,
    slow: &mut crate::state::Smoother<F>,
    signal: &mut crate::state::Smoother<F>,
    fast_len: u32,
    slow_len: u32,
    sig_len: u32,
    x: F,
) -> (F, F, F) {
    crate::engine::macd_arm_generic(fast, slow, signal, fast_len, slow_len, sig_len, x)
}

/// Root for the generic `State::Keltner` arm of `ta.kc`.
#[allow(clippy::too_many_arguments)]
pub fn ta_kc_arm<F: crate::state::FloatOps>(
    basis: &mut crate::state::Smoother<F>,
    range_ema: &mut crate::state::Smoother<F>,
    length: u32,
    mult: F,
    use_true_range: bool,
    src: F,
    high: F,
    low: F,
    first: bool,
    prev_close: F,
) -> (F, F, F) {
    crate::engine::kc_arm_generic(
        basis,
        range_ema,
        length,
        mult,
        use_true_range,
        src,
        high,
        low,
        first,
        prev_close,
    )
}

/// Root for the generic `State::Keltner` arm of `ta.kcw`.
#[allow(clippy::too_many_arguments)]
pub fn ta_kcw_arm<F: crate::state::FloatOps>(
    basis: &mut crate::state::Smoother<F>,
    range_ema: &mut crate::state::Smoother<F>,
    length: u32,
    mult: F,
    use_true_range: bool,
    src: F,
    high: F,
    low: F,
    first: bool,
    prev_close: F,
) -> F {
    crate::engine::kcw_arm_generic(
        basis,
        range_ema,
        length,
        mult,
        use_true_range,
        src,
        high,
        low,
        first,
        prev_close,
    )
}

/// Root for the generic `State::BarsSince` arm.
pub fn ta_barssince_arm<F: crate::state::FloatOps>(since: &mut i64, c: bool) -> F {
    crate::engine::barssince_arm_generic(since, c)
}

/// Root for the generic `State::TimeframeChange` arm (`timeframe.change`).
pub fn ta_timeframe_change_arm<F: crate::state::FloatOps>(
    prev_bucket: &mut i64,
    tf_param: Option<i64>,
    time_ms: i64,
) -> F {
    crate::engine::timeframe_change_arm_generic(prev_bucket, tf_param, time_ms)
}

/// Root for the generic `State::Obv` arm.
pub fn ta_obv_arm<F: crate::state::FloatOps>(
    total: &mut F,
    prev_close: &mut F,
    close: F,
    volume: F,
) -> F {
    crate::engine::obv_arm_generic(total, prev_close, close, volume)
}

/// Root for the generic `State::AccDist` arm.
pub fn ta_accdist_arm<F: crate::state::FloatOps>(
    total: &mut F,
    high: F,
    low: F,
    close: F,
    volume: F,
) -> F {
    crate::engine::accdist_arm_generic(total, high, low, close, volume)
}

/// Root for the generic `State::Pvt` arm.
pub fn ta_pvt_arm<F: crate::state::FloatOps>(
    total: &mut F,
    prev_close: &mut F,
    close: F,
    volume: F,
) -> F {
    crate::engine::pvt_arm_generic(total, prev_close, close, volume)
}

/// Root for the generic `State::Wad` arm.
pub fn ta_wad_arm<F: crate::state::FloatOps>(
    total: &mut F,
    prev_close: &mut F,
    high: F,
    low: F,
    close: F,
) -> F {
    crate::engine::wad_arm_generic(total, prev_close, high, low, close)
}

/// Root for the generic `ta.stdev` arm (`State::Window` push,
/// readiness, `stdev_arm!`).
pub fn ta_stdev_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::Ring,
    cells: &mut [F],
    x: F,
    biased: bool,
) -> F {
    crate::engine::stdev_family_arm_generic(ring, cells, x, false, false, biased)
}

/// Root for the generic `ta.variance` arm.
pub fn ta_variance_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::Ring,
    cells: &mut [F],
    x: F,
    biased: bool,
) -> F {
    crate::engine::stdev_family_arm_generic(ring, cells, x, false, true, biased)
}

/// Root for the generic `ta.dev` arm.
pub fn ta_dev_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::Ring,
    cells: &mut [F],
    x: F,
) -> F {
    crate::engine::stdev_family_arm_generic(ring, cells, x, true, false, true)
}

/// Root for the generic `ta.bbw` arm.
pub fn ta_bbw_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::Ring,
    cells: &mut [F],
    x: F,
    mult: F,
) -> F {
    crate::engine::bbw_arm_generic(ring, cells, x, mult)
}

/// Root for the generic `State::Bb` arm (`ta.bb`).
pub fn ta_bb_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::Ring,
    cells: &mut [F],
    x: F,
    length: u32,
    mult: F,
) -> (F, F, F) {
    crate::engine::bb_arm_generic(ring, cells, x, length, mult)
}

/// Root for the generic `State::Vwap` arm of `ta.vwap`, `ta.vwap(source)`
/// and `ta.vwap(source, anchor)`.
#[allow(clippy::too_many_arguments)]
pub fn ta_vwap_arm<F: crate::state::FloatOps>(
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
    crate::engine::vwap_arm_generic(
        pv, p2v, vol, active, use_src, src, high, low, close, volume, anchor,
    )
}

/// Root for the generic `State::Vwap` arm of `ta.vwap(source, anchor,
/// stdev_mult)`.
#[allow(clippy::too_many_arguments)]
pub fn ta_vwap_bands_arm<F: crate::state::FloatOps>(
    pv: &mut F,
    p2v: &mut F,
    vol: &mut F,
    active: &mut bool,
    src: F,
    volume: F,
    anchor: bool,
    mult: F,
) -> (F, F, F) {
    crate::engine::vwap_bands_arm_generic(pv, p2v, vol, active, src, volume, anchor, mult)
}

/// Root for the generic `State::ValueWhen` arm (`ta.valuewhen`,
/// every source type).
pub fn ta_valuewhen_arm<F: crate::state::FloatOps>(
    vals: &mut crate::arena::Ring,
    cells: &mut [F],
    cond: bool,
    x: F,
) -> F {
    crate::engine::valuewhen_arm_generic(vals, cells, cond, x)
}

/// Root for the generic `State::Pivot` arm (`ta.pivothigh` when `is_high`,
/// else `ta.pivotlow`).
pub fn ta_pivot_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::Ring,
    cells: &mut [F],
    x: F,
    left: u32,
    is_high: bool,
) -> F {
    crate::engine::pivot_arm_generic(ring, cells, x, left, is_high)
}

/// Root for the generic `ta.median` arm (`State::Window` push, readiness,
/// sorted scratch copy, `median_arm!`).
pub fn ta_median_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::Ring,
    cells: &mut [F],
    scratch: &mut [F],
    x: F,
) -> F {
    crate::engine::median_arm_generic(ring, cells, scratch, x)
}

/// Root for the generic `ta.linreg` arm.
pub fn ta_linreg_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::Ring,
    cells: &mut [F],
    x: F,
    offset: i64,
) -> F {
    crate::engine::linreg_arm_generic(ring, cells, x, offset)
}

/// Root for the generic `ta.alma` arm.
pub fn ta_alma_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::Ring,
    cells: &mut [F],
    x: F,
    offset: F,
    sigma: F,
    floor: bool,
) -> F {
    crate::engine::alma_arm_generic(ring, cells, x, offset, sigma, floor)
}

/// Root for the generic `State::Stoch` arm (`ta.stoch`).
pub fn ta_stoch_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::PairRing,
    cells: &mut [F],
    source: F,
    high: F,
    low: F,
) -> F {
    crate::engine::stoch_arm_generic(ring, cells, source, high, low)
}

/// Root for the generic `State::Sar` arm (`ta.sar`).
pub fn ta_sar_arm<F: crate::state::FloatOps>(
    s: &mut crate::state::Sar<F>,
    start: F,
    inc: F,
    max: F,
    high: F,
    low: F,
    close: F,
) -> F {
    crate::engine::sar_arm_generic(s, start, inc, max, high, low, close)
}

/// Root for the generic `State::Tsi` arm (`ta.tsi`).
#[allow(clippy::too_many_arguments)]
pub fn ta_tsi_arm<F: crate::state::FloatOps>(
    prev: &mut F,
    pc_long: &mut crate::state::Smoother<F>,
    pc_short: &mut crate::state::Smoother<F>,
    abs_long: &mut crate::state::Smoother<F>,
    abs_short: &mut crate::state::Smoother<F>,
    x: F,
    short: u32,
    long: u32,
) -> F {
    crate::engine::tsi_arm_generic(prev, pc_long, pc_short, abs_long, abs_short, x, short, long)
}

/// Root for the generic `State::Supertrend` arm (`ta.supertrend`):
/// `(supertrend, direction, na)`.
#[allow(clippy::too_many_arguments)]
pub fn ta_supertrend_arm<F: crate::state::FloatOps>(
    atr: &mut crate::state::Smoother<F>,
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
    crate::engine::supertrend_arm_generic(
        atr,
        bands_seeded,
        prev_upper,
        prev_lower,
        prev_supertrend,
        prev_direction,
        atr_period,
        factor,
        high,
        low,
        close,
        first,
        prev_close,
    )
}

/// Root for the generic `ta.range` arm (`State::Window` push, readiness,
/// `range_arm!`; both source types).
pub fn ta_range_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::Ring,
    cells: &mut [F],
    x: F,
) -> F {
    crate::engine::range_arm_generic(ring, cells, x)
}

/// Root for the generic `ta.rci` arm (`State::Window` push, readiness,
/// sorted scratch copy, `rci_arm!`).
pub fn ta_rci_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::Ring,
    cells: &mut [F],
    scratch: &mut [F],
    x: F,
) -> F {
    crate::engine::rci_arm_generic(ring, cells, scratch, x)
}

/// Root for the generic `State::WindowPair` arm
/// (`ta.vwma`).
pub fn ta_vwma_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::PairRing,
    cells: &mut [F],
    x: F,
    volume: F,
) -> F {
    crate::engine::vwma_arm_generic(ring, cells, x, volume)
}

/// Root for the generic `State::Hma` arm (`ta.hma`).
pub fn ta_hma_arm<F: crate::state::FloatOps>(
    src: &mut crate::arena::Ring,
    inter: &mut crate::arena::Ring,
    cells: &mut [F],
    x: F,
) -> F {
    crate::engine::hma_arm_generic(src, inter, cells, x)
}

/// Root for the generic `State::Dmi` arm (`ta.dmi`): `(+DI, −DI, ADX)`.
#[allow(clippy::too_many_arguments)]
pub fn ta_dmi_arm<F: crate::state::FloatOps>(
    prev_high: &mut F,
    prev_low: &mut F,
    tr: &mut crate::state::Smoother<F>,
    plus_dm: &mut crate::state::Smoother<F>,
    minus_dm: &mut crate::state::Smoother<F>,
    adx: &mut crate::state::Smoother<F>,
    di_len: u32,
    adx_len: u32,
    high: F,
    low: F,
    first: bool,
    prev_close: F,
) -> (F, F, F) {
    crate::engine::dmi_arm_generic(
        prev_high, prev_low, tr, plus_dm, minus_dm, adx, di_len, adx_len, high, low, first,
        prev_close,
    )
}

/// Root for the generic `ta.rising` / `ta.falling` arm (`State::Window`
/// push, `rising_arm!`).
pub fn ta_rising_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::Ring,
    cells: &mut [F],
    x: F,
    rising: bool,
) -> F {
    crate::engine::rising_arm_generic(ring, cells, x, rising)
}

/// Root for the generic `ta.mode` arm (`State::Window` push, readiness,
/// sorted scratch copy, `mode_arm!`; both source types).
pub fn ta_mode_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::Ring,
    cells: &mut [F],
    scratch: &mut [F],
    x: F,
) -> F {
    crate::engine::mode_arm_generic(ring, cells, scratch, x)
}

/// Root for the generic `ta.percentile_nearest_rank` arm (`State::Window`
/// push, readiness, sorted scratch copy, `percentile_nearest_rank_arm!`).
pub fn ta_percentile_nearest_rank_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::Ring,
    cells: &mut [F],
    scratch: &mut [F],
    x: F,
    p: F,
) -> F {
    crate::engine::percentile_nearest_rank_arm_generic(ring, cells, scratch, x, p)
}

/// Root for the generic `ta.mom` / `ta.roc` arm
/// (`State::Window` push, `mom_roc_arm!`).
pub fn ta_mom_roc_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::Ring,
    cells: &mut [F],
    x: F,
    mom: bool,
) -> F {
    crate::engine::mom_roc_arm_generic(ring, cells, x, mom)
}

/// Root for the generic `ta.wma` arm (`State::Window` push, readiness,
/// `wma_arm!`).
pub fn ta_wma_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::Ring,
    cells: &mut [F],
    x: F,
) -> F {
    crate::engine::wma_arm_generic(ring, cells, x)
}

/// Root for the generic `ta.cci` arm (`State::Window` push, readiness,
/// `cci_arm!`).
pub fn ta_cci_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::Ring,
    cells: &mut [F],
    x: F,
) -> F {
    crate::engine::cci_arm_generic(ring, cells, x)
}

/// Root for the generic `ta.cmo` arm (`State::Window` push, readiness,
/// `cmo_arm!`).
pub fn ta_cmo_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::Ring,
    cells: &mut [F],
    x: F,
) -> F {
    crate::engine::cmo_arm_generic(ring, cells, x)
}

/// Root for the generic `ta.percentile_linear_interpolation` arm
/// (`State::Window` push, readiness, sorted scratch copy,
/// `percentile_linear_interpolation_arm!`).
pub fn ta_percentile_linear_interpolation_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::Ring,
    cells: &mut [F],
    scratch: &mut [F],
    x: F,
    p: F,
) -> F {
    crate::engine::percentile_linear_interpolation_arm_generic(ring, cells, scratch, x, p)
}

/// Root for the generic `State::Corr` arm (`ta.correlation`).
pub fn ta_correlation_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::PairRing,
    cells: &mut [F],
    a: F,
    b: F,
) -> F {
    crate::engine::correlation_arm_generic(ring, cells, a, b)
}

/// Root for the generic `State::Wpr` arm (`ta.wpr`).
pub fn ta_wpr_arm<F: crate::state::FloatOps>(
    highs: &mut crate::arena::Ring,
    lows: &mut crate::arena::Ring,
    cells: &mut [F],
    high: F,
    low: F,
    close: F,
) -> F {
    crate::engine::wpr_arm_generic(highs, lows, cells, high, low, close)
}

/// Root for the generic `ta.math_sum` arm (`State::Window` push,
/// readiness, `math_sum_arm!`).
pub fn ta_math_sum_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::Ring,
    cells: &mut [F],
    x: F,
) -> F {
    crate::engine::math_sum_arm_generic(ring, cells, x)
}

/// Root for the generic `ta.swma` arm (`State::Window` push, readiness,
/// `swma_arm!`).
pub fn ta_swma_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::Ring,
    cells: &mut [F],
    x: F,
) -> F {
    crate::engine::swma_arm_generic(ring, cells, x)
}

/// Root for the generic `ta.cog` arm (`State::Window` push, readiness,
/// `cog_arm!`).
pub fn ta_cog_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::Ring,
    cells: &mut [F],
    x: F,
) -> F {
    crate::engine::cog_arm_generic(ring, cells, x)
}

/// Root for the generic `ta.percentrank` arm (`State::Window` push,
/// readiness, `percent_rank_arm!`).
pub fn ta_percent_rank_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::Ring,
    cells: &mut [F],
    x: F,
) -> F {
    crate::engine::percent_rank_arm_generic(ring, cells, x)
}

/// Root for the generic `ta.wvad` arm (`State::Stateless`).
pub fn ta_wvad_arm<F: crate::state::FloatOps>(o: F, h: F, l: F, c: F, v: F) -> F {
    crate::engine::wvad_arm_generic(o, h, l, c, v)
}

/// Root for the generic `ta.iii` arm (`State::Stateless`).
pub fn ta_iii_arm<F: crate::state::FloatOps>(h: F, l: F, c: F, v: F) -> F {
    crate::engine::iii_arm_generic(h, l, c, v)
}

/// Root for the generic `State::Mfi` arm (`ta.mfi`).
pub fn ta_mfi_arm<F: crate::state::FloatOps>(
    prev: &mut F,
    ring: &mut crate::arena::PairRing,
    cells: &mut [F],
    source: F,
    volume: F,
) -> F {
    crate::engine::mfi_arm_generic(prev, ring, cells, source, volume)
}

/// Root for the generic `State::VolumeIndex` arm (`ta.nvi` / `ta.pvi`).
pub fn ta_volume_index_arm<F: crate::state::FloatOps>(
    ta_func: crate::TaFunc,
    value: &mut F,
    prev_close: &mut F,
    prev_volume: &mut F,
    close: F,
    volume: F,
) -> F {
    crate::engine::volume_index_arm_generic(ta_func, value, prev_close, prev_volume, close, volume)
}

/// Root for the generic short-window `ta.highest` / `ta.lowest` arm
/// (`State::Window` push, `extremum_fold_fn!`).
pub fn ta_extremum_short_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::Ring,
    cells: &mut [F],
    x: F,
    highest: bool,
) -> F {
    crate::engine::extremum_short_arm_generic(ring, cells, x, highest)
}

/// Root for the generic short-window `ta.highestbars` / `ta.lowestbars` arm
/// (`State::Window` push, readiness, `extremum_bars_fn!`).
pub fn ta_extremum_bars_short_arm<F: crate::state::FloatOps>(
    ring: &mut crate::arena::Ring,
    cells: &mut [F],
    x: F,
    highest: bool,
) -> F {
    crate::engine::extremum_bars_short_arm_generic(ring, cells, x, highest)
}

/// Root for the generic `State::WindowExtremum` arm (long-window
/// `ta.highest` / `ta.lowest` / `ta.highestbars` / `ta.lowestbars`) with the
/// zero-extremum fold in place.
pub fn ta_window_extremum_arm<F: crate::state::FloatOps>(
    window: &mut crate::arena::ExtremumWindow,
    cells: &mut [F],
    x: F,
    highest: bool,
    bars: bool,
) -> F {
    crate::engine::window_extremum_arm_generic(window, cells, x, highest, bars)
}
