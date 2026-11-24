//! Higher-timeframe (HTF) resampling for `request.security`.
//!
//! PineWASM models `request.security` with the **honest same-feed**
//! simulation: rather than fetching a second data feed (which this
//! network-free VM cannot do), it AGGREGATES the chart's own [`MarketData`]
//! into higher-timeframe buckets along the shared time axis. A secondary
//! `CsilFunction` (see `docs/runtime.md`, "secondary contexts") is then run
//! against the resampled bars and its results are mapped back onto the chart
//! bars honoring the `gaps` and `lookahead` modes.
//!
//! This module owns the deterministic, dependency-free core of that model:
//!
//!  * [`resample`] — group chart bars into HTF buckets by the time axis and
//!    aggregate OHLCV (open = first, high = max, low = min, close = last,
//!    volume = sum);
//!  * [`Resampled`] — the resulting HTF series plus the chart-bar → bucket
//!    map used to index HTF results back per chart bar under
//!    [`Gaps`]/[`Lookahead`].
//!
//! Fixed-width timeframes follow the exchange clock: widths under a day walk
//! from local midnight every day, longer ones from the first bar's local
//! midnight (`fixed_bucket_id`). Week and month units are calendar-aligned using the chart bar's trading
//! date: the host-supplied 00:00 UTC identity read as-is, or the nominal close
//! instant read in the exchange timezone. This preserves the semantic
//! distinction between `1W`/`1M` and fixed-width `7D`/`30D`.

use crate::{
    session::{civil_from_epoch_ms, days_from_civil, timeframe_parts},
    timeframe_in_seconds, MarketData,
};

/// Why a requested `request.security` timeframe is invalid for resampling.
///
/// The honest same-feed model can only AGGREGATE the chart feed UP to a higher
/// timeframe; it cannot manufacture finer bars than the feed provides. A
/// request for a timeframe shorter than the chart's is therefore rejected
/// (surfaced as `E0242` once the call site is wired through the analyzer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecurityTimeframeError {
    /// The requested timeframe resolves to fewer seconds than the chart's.
    LowerThanChart {
        chart_seconds: i64,
        requested_seconds: i64,
    },
}

/// Validate a requested `request.security` timeframe against the chart feed's
/// own timeframe. Returns the requested length in seconds when it is >= the
/// chart timeframe (resampling is well-defined), else the reason it is
/// rejected. Equal timeframes are allowed (the identity resample).
pub fn validate_security_timeframe(
    data: &MarketData,
    requested_tf: &str,
) -> Result<i64, SecurityTimeframeError> {
    let chart_seconds = timeframe_in_seconds(&data.timeframe);
    let requested_seconds = timeframe_in_seconds(requested_tf);
    if security_timeframe_admitted(chart_seconds, requested_seconds) {
        Ok(requested_seconds)
    } else {
        Err(SecurityTimeframeError::LowerThanChart {
            chart_seconds,
            requested_seconds,
        })
    }
}

/// The lower-timeframe admission test of `request.security`
/// (Aeneas-extractable): a request is admitted exactly when its timeframe is
/// at least the chart's, both in seconds. Equal timeframes are admitted (the
/// identity resample).
pub(crate) fn security_timeframe_admitted(chart_seconds: i64, requested_seconds: i64) -> bool {
    requested_seconds >= chart_seconds
}

/// `gaps` argument of `request.security`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gaps {
    /// `barmerge.gaps_off` (default): fill chart bars where the selected HTF
    /// visibility mode has no value with its most recent visible value.
    Off,
    /// `barmerge.gaps_on`: emit only when a requested bucket first becomes
    /// visible under the selected lookahead mode; otherwise yield `na`.
    On,
}

/// `lookahead` argument of `request.security`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookahead {
    /// `barmerge.lookahead_off` (default): a chart bar sees only HTF buckets
    /// that have already CLOSED (their publication index is at or before
    /// this one). No future information leaks; no repaint.
    Off,
    /// `barmerge.lookahead_on`: a chart bar sees the value of the HTF bucket
    /// it currently belongs to, including on earlier chart bars. This exposes
    /// the bucket's current aggregate before its last supplied chart bar and
    /// REPAINTS (documented in `docs/runtime.md`).
    On,
}

/// One aggregated higher-timeframe bar.
#[derive(Debug, Clone, PartialEq)]
pub struct HtfBar<F = f64> {
    pub open: F,
    pub high: F,
    pub low: F,
    pub close: F,
    pub volume: F,
    /// Open time (epoch ms) of the bucket = the open time of its first chart
    /// bar.
    pub time: i64,
    /// Index of the last supplied chart bar aggregated into this bucket.
    pub last_chart_bar: usize,
    /// First chart bar on which the bucket is confirmed. A host clock
    /// can defer this until the next bucket's first available bar.
    /// `usize::MAX` means no supplied chart bar has reached the closing time.
    /// Without host clocks this equals `last_chart_bar`.
    pub publication_chart_bar: usize,
}

/// The result of resampling a chart feed to a higher timeframe.
#[derive(Debug, Clone, PartialEq)]
pub struct Resampled {
    /// The aggregated HTF bars, in time order.
    pub bars: Vec<HtfBar>,
    /// `bucket_of[i]` is the index into [`Resampled::bars`] of the HTF bucket
    /// that chart bar `i` belongs to. Length equals the chart bar count.
    pub bucket_of: Vec<usize>,
}

impl Resampled {
    /// The HTF bucket index a chart bar should READ under `lookahead`.
    ///
    /// * `Lookahead::On` — the chart bar's own (forming) bucket.
    /// * `Lookahead::Off` — the most recently CLOSED bucket at or before this
    ///   chart bar, i.e. the chart bar's own bucket only once it reaches that
    ///   bucket's publication index; on earlier bars of the bucket, the previous
    ///   (already-closed) bucket. Returns `None` before any bucket has closed.
    pub fn visible_bucket(&self, chart_bar: usize, lookahead: Lookahead) -> Option<usize> {
        visible_bucket_kernel(&self.bucket_of, &self.bars, chart_bar, lookahead)
    }

    /// Whether a chart bar confirms its own HTF bucket. A previous bucket's
    /// delayed confirmation is handled separately by `map_to_chart_bars`.
    pub fn is_bucket_close(&self, chart_bar: usize) -> bool {
        is_bucket_close_kernel(&self.bucket_of, &self.bars, chart_bar)
    }

    /// Map a per-HTF-bucket value back onto every chart bar under the given
    /// `gaps`/`lookahead` modes. `htf_value(bucket_index)` supplies the
    /// secondary context's result for HTF bar `bucket_index` (e.g. the value
    /// produced by running `functions[k]` over that resampled bar); `None`
    /// models an HTF-side `na`. The returned vector has one entry per chart
    /// bar: `Some(v)` where a value is visible, `None` for a gap / not-yet-
    /// available bucket.
    ///
    /// This is the deterministic core of `request.security`'s back-mapping:
    ///
    ///  * `lookahead` selects WHICH bucket each chart bar reads
    ///    ([`Resampled::visible_bucket`]);
    ///  * `Gaps::On` emits only when the visible bucket changes. This is its
    ///    first available chart bar with `lookahead_on`, or its confirmation
    ///    bar with `lookahead_off`. `Gaps::Off` repeats the visible value.
    pub fn map_to_chart_bars<F>(
        &self,
        gaps: Gaps,
        lookahead: Lookahead,
        mut htf_value: F,
    ) -> Vec<Option<f64>>
    where
        F: FnMut(usize) -> Option<f64>,
    {
        let n = self.bucket_of.len();
        let mut out = Vec::with_capacity(n);
        for chart_bar in 0..n {
            let value = chart_bar_bucket(&self.bucket_of, &self.bars, chart_bar, gaps, lookahead)
                .and_then(&mut htf_value);
            out.push(value);
        }
        out
    }
}

/// Integer kernel of [`Resampled::map_to_chart_bars`] for one chart bar
/// (Aeneas-extractable): the HTF bucket whose value `chart_bar` shows, or
/// `None` for a gap / not-yet-available bucket. `lookahead` selects the bucket
/// ([`visible_bucket_kernel`]); under `Gaps::On` a value appears when the
/// visible requested bucket changes. A host clock can delay confirmation
/// until a later chart bar.
pub(crate) fn chart_bar_bucket<B: BucketClose>(
    bucket_of: &[usize],
    bars: &[B],
    chart_bar: usize,
    gaps: Gaps,
    lookahead: Lookahead,
) -> Option<usize> {
    let visible = visible_bucket_kernel(bucket_of, bars, chart_bar, lookahead);
    if matches!(gaps, Gaps::On) {
        let previous = chart_bar
            .checked_sub(1)
            .and_then(|prior| visible_bucket_kernel(bucket_of, bars, prior, lookahead));
        if visible == previous {
            return None;
        }
    }
    visible
}

/// Read access to the one integer field of an HTF bucket that the index
/// kernels inspect. Keeping the kernels generic over this trait keeps the
/// float-carrying [`HtfBar`] out of the Aeneas-extracted code; the only impl is
/// a field read, and monomorphization makes it free.
pub(crate) trait BucketClose {
    /// First chart bar that confirms this bucket, or `usize::MAX`.
    fn publication_chart_bar(&self) -> usize;
}

impl BucketClose for HtfBar {
    fn publication_chart_bar(&self) -> usize {
        self.publication_chart_bar
    }
}

/// Integer kernel of [`Resampled::visible_bucket`] (Aeneas-extractable).
///
/// Panics exactly where the method always did: when `bucket_of[chart_bar]` is
/// not a valid index into `bars`.
pub(crate) fn visible_bucket_kernel<B: BucketClose>(
    bucket_of: &[usize],
    bars: &[B],
    chart_bar: usize,
    lookahead: Lookahead,
) -> Option<usize> {
    if chart_bar >= bucket_of.len() {
        return None;
    }
    let own = bucket_of[chart_bar];
    match lookahead {
        Lookahead::On => Some(own),
        Lookahead::Off => {
            if chart_bar >= bars[own].publication_chart_bar() {
                // This bar closes the bucket: it is now completed.
                Some(own)
            } else if own == 0 {
                // First bucket still forming, nothing closed yet.
                None
            } else {
                Some(own - 1)
            }
        }
    }
}

/// Integer kernel of [`Resampled::is_bucket_close`] (Aeneas-extractable).
pub(crate) fn is_bucket_close_kernel<B: BucketClose>(
    bucket_of: &[usize],
    bars: &[B],
    chart_bar: usize,
) -> bool {
    if chart_bar < bucket_of.len() {
        bars[bucket_of[chart_bar]].publication_chart_bar() == chart_bar
    } else {
        false
    }
}

/// The bucket structure of a chart feed, computed from per-bar bucket ids
/// alone (no prices).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BucketIndex {
    /// `bucket_of[i]` is the bucket of chart bar `i`; same length as the ids.
    pub(crate) bucket_of: Vec<usize>,
    /// `last_chart_bar[k]` is the index of the last chart bar in bucket `k`.
    pub(crate) last_chart_bar: Vec<usize>,
}

/// Integer kernel of the bucket grouping (Aeneas-extractable): a new bucket
/// starts at bar 0 and wherever the id differs from the previous bar's id.
pub(crate) fn bucket_index(ids: &[i64]) -> BucketIndex {
    let n = ids.len();
    let mut bucket_of: Vec<usize> = Vec::with_capacity(n);
    let mut last_chart_bar: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < n {
        let need_new = i == 0 || ids[i - 1] != ids[i];
        if need_new {
            last_chart_bar.push(i);
        } else {
            let k = last_chart_bar.len() - 1;
            last_chart_bar[k] = i;
        }
        bucket_of.push(last_chart_bar.len() - 1);
        i += 1;
    }
    BucketIndex {
        bucket_of,
        last_chart_bar,
    }
}

/// How the bucket fold builds one HTF bar: open a bucket at its first chart
/// bar, then extend it with each later chart bar of the same bucket. The
/// fold ([`fold_buckets`]) is generic over this so its integer skeleton (which
/// bar opens which bucket, and each bucket's `last_chart_bar`) can be
/// extracted without the `f64` OHLCV data; the only impl is [`OhlcvSource`].
pub(crate) trait BarSource<B> {
    /// The HTF bar for a bucket whose first chart bar is `i` and whose last
    /// chart bar is `last_chart_bar`.
    fn open_bucket(&self, i: usize, last_chart_bar: usize) -> B;
    /// Fold chart bar `i` into its (already open) bucket's bar.
    fn extend_bucket(&self, bar: &mut B, i: usize);
}

/// The OHLCV aggregation of [`resample_by_bucket`].
struct OhlcvSource<'a>(&'a MarketData);

/// Open one OHLCV bucket from the actual chart feed, preserving the source-read
/// order and the supplied final chart index. The free function is the concrete
/// extraction boundary; float aggregation remains in `extend_bucket`.
pub(crate) fn ohlcv_open_bucket(data: &MarketData, i: usize, last_chart_bar: usize) -> HtfBar {
    HtfBar {
        open: data.open[i],
        high: data.high[i],
        low: data.low[i],
        close: data.close[i],
        volume: data.volume[i],
        time: data.time_at(i),
        last_chart_bar,
        publication_chart_bar: last_chart_bar,
    }
}

/// The three ordered value operations of OHLCV extension. The native instance
/// deliberately retains raw `f64::max`/`min`, including their tie behavior.
pub(crate) trait OhlcvMergeOps<F> {
    fn max(&self, left: F, right: F) -> F;
    fn min(&self, left: F, right: F) -> F;
    fn add(&self, left: F, right: F) -> F;
}

struct F64OhlcvMergeOps;

impl OhlcvMergeOps<f64> for F64OhlcvMergeOps {
    fn max(&self, left: f64, right: f64) -> f64 {
        left.max(right)
    }

    fn min(&self, left: f64, right: f64) -> f64 {
        left.min(right)
    }

    fn add(&self, left: f64, right: f64) -> f64 {
        left + right
    }
}

/// Extend the actual bucket through borrowed price lanes. Each indexed read,
/// operation and assignment stays interleaved: a later failed read leaves the
/// same earlier updates in place. Open/time and both metadata fields are inert.
pub(crate) fn ohlcv_extend_lanes<F: Copy, O: OhlcvMergeOps<F>>(
    ops: &O,
    high: &[F],
    low: &[F],
    close: &[F],
    volume: &[F],
    cur: &mut HtfBar<F>,
    i: usize,
) {
    cur.high = ops.max(cur.high, high[i]);
    cur.low = ops.min(cur.low, low[i]);
    cur.close = close[i];
    cur.volume = ops.add(cur.volume, volume[i]);
}

impl BarSource<HtfBar> for OhlcvSource<'_> {
    fn open_bucket(&self, i: usize, last_chart_bar: usize) -> HtfBar {
        ohlcv_open_bucket(self.0, i, last_chart_bar)
    }

    fn extend_bucket(&self, cur: &mut HtfBar, i: usize) {
        let data = self.0;
        ohlcv_extend_lanes(
            &F64OhlcvMergeOps,
            &data.high,
            &data.low,
            &data.close,
            &data.volume,
            cur,
            i,
        );
    }
}

/// Integer skeleton of the OHLCV fold (Aeneas-extractable): walk the chart
/// bars in order, opening a bar when a bar starts a new bucket and extending
/// the bucket's bar otherwise.
pub(crate) fn fold_buckets<B, S: BarSource<B>>(
    src: &S,
    bucket_of: &[usize],
    last_chart_bar: &[usize],
) -> Vec<B> {
    let mut bars: Vec<B> = Vec::with_capacity(last_chart_bar.len());
    let mut i = 0;
    while i < bucket_of.len() {
        let bucket = bucket_of[i];
        if bucket == bars.len() {
            bars.push(src.open_bucket(i, last_chart_bar[bucket]));
        } else {
            src.extend_bucket(&mut bars[bucket], i);
        }
        i += 1;
    }
    bars
}

/// Bucket grouping plus the fold (Aeneas-extractable): the HTF bars and
/// `bucket_of` of [`resample_by_bucket`] for the per-bar bucket ids `ids`.
pub(crate) fn resample_kernel<B, S: BarSource<B>>(src: &S, ids: &[i64]) -> (Vec<B>, Vec<usize>) {
    let BucketIndex {
        bucket_of,
        last_chart_bar,
    } = bucket_index(ids);
    // OHLCV aggregation over each (contiguous) bucket, folded in bar order.
    let bars = fold_buckets(src, &bucket_of, &last_chart_bar);
    (bars, bucket_of)
}

fn resample_by_bucket<F>(data: &MarketData, bucket_id: F) -> Resampled
where
    F: FnMut(usize) -> i64,
{
    let n = data.bars();
    let ids: Vec<i64> = (0..n).map(bucket_id).collect();
    let (bars, bucket_of) = resample_kernel(&OhlcvSource(data), &ids);
    Resampled { bars, bucket_of }
}

/// Group the requested-close prefix consumed by the actual chart feed.
/// The chart length is `MarketData::bars()` (the close-price length), not the
/// length of either clock lane. Read all ids before grouping or reading OHLCV;
/// a short requested-close slice still panics at its first out-of-bounds read.
pub(crate) fn requested_close_buckets(data: &MarketData, closes: &[i64]) -> BucketIndex {
    let n = data.bars();
    let mut ids = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        ids.push(closes[i]);
        i += 1;
    }
    bucket_index(&ids)
}

/// The host-supplied closes-only branch after request-key selection.
/// Collect every requested id, finish all OHLCV folding, then publish every
/// resulting bar. Payload and nominal operations remain in their dictionaries;
/// the returned bucket map is the original grouping allocation.
pub(crate) fn requested_close_resample<P, N, F, S>(
    data: &MarketData,
    source: &S,
    lanes: &crate::vm_publication_axis::PublicationLanes<'_, P, N>,
    closes: &[i64],
) -> (Vec<HtfBar<F>>, Vec<usize>)
where
    N: crate::vm_publication_axis::NominalSeconds,
    S: BarSource<HtfBar<F>>,
{
    let BucketIndex {
        bucket_of,
        last_chart_bar,
    } = requested_close_buckets(data, closes);
    let mut bars = fold_buckets(source, &bucket_of, &last_chart_bar);
    crate::vm_publication_axis::publish_lanes(lanes, &mut bars, closes);
    (bars, bucket_of)
}

/// Monthly bucket id of a month index `year * 12 + (month - 1)`, in buckets
/// of `months` months (Aeneas-extractable; `months >= 1`).
pub(crate) fn month_bucket_id(year: i64, month: i64, months: i64) -> i64 {
    year.saturating_mul(12)
        .saturating_add(month - 1)
        .div_euclid(months)
}

/// Monthly bucket id of the calendar instant `calendar_time` read at
/// `offset` seconds (Aeneas-extractable).
pub(crate) fn month_bucket_of_instant(calendar_time: i64, offset: i64, months: i64) -> i64 {
    let (year, month, ..) = civil_from_epoch_ms(calendar_time, offset);
    month_bucket_id(year, month, months)
}

/// Bucket width in milliseconds of a fixed-width timeframe
/// (Aeneas-extractable): `htf_seconds` clamped below at one second, the
/// product saturating at `i64::MAX`. Always positive.
pub(crate) fn fixed_step_ms(htf_seconds: i64) -> i64 {
    htf_seconds.max(1).saturating_mul(1000)
}

/// One calendar day in milliseconds.
const DAY_MS: i64 = 86_400_000;

/// Fixed-width bucket id of a bar opening at `time` (Aeneas-extractable).
///
/// Buckets follow the exchange clock, not the feed: `offset_ms` (the
/// exchange timezone's offset east of UTC) moves `time` to local time, and
/// a width under one day walks from local midnight, restarting every day,
/// so a 60-minute bucket always opens on the hour and a 240-minute one at
/// 00:00, 04:00, ... whatever bar the feed starts on. A width of a day or
/// more walks from local midnight of `first`, the feed's first open time:
/// the day a multi-day bucket starts on still follows the feed, but never
/// the time of day the feed starts at.
pub(crate) fn fixed_bucket_id(time: i64, first: i64, step_ms: i64, offset_ms: i64) -> i64 {
    let local = time.saturating_add(offset_ms);
    if step_ms < DAY_MS {
        let day = local.div_euclid(DAY_MS);
        let per_day = DAY_MS.div_euclid(step_ms) + i64::from(DAY_MS.rem_euclid(step_ms) != 0);
        day.saturating_mul(per_day)
            .saturating_add(local.rem_euclid(DAY_MS).div_euclid(step_ms))
    } else {
        let anchor = first
            .saturating_add(offset_ms)
            .div_euclid(DAY_MS)
            .saturating_mul(DAY_MS);
        local.saturating_sub(anchor).div_euclid(step_ms)
    }
}

/// The bar-open time axis that fixed-width resampling reads. Keeping
/// [`fixed_bucket_ids`] generic over it keeps [`MarketData`] out of the
/// Aeneas-extracted code; the only impl reads [`MarketData::bars`] and
/// [`MarketData::time_at`].
pub(crate) trait TimeAxis {
    /// Number of chart bars.
    fn axis_len(&self) -> usize;
    /// Open time (epoch ms) of chart bar `i < axis_len()`.
    fn open_time(&self, i: usize) -> i64;
}

impl TimeAxis for MarketData {
    fn axis_len(&self) -> usize {
        self.bars()
    }

    fn open_time(&self, i: usize) -> i64 {
        self.time_at(i)
    }
}

/// The bar opens of a feed read on the exchange's wall clock: each open
/// plus the exchange timezone's offset at that open, so a daylight-saving
/// change moves the clock the buckets follow. [`fixed_bucket_ids`] reads it
/// with a zero offset.
struct LocalAxis<'a>(&'a MarketData);

impl TimeAxis for LocalAxis<'_> {
    fn axis_len(&self) -> usize {
        self.0.bars()
    }

    fn open_time(&self, i: usize) -> i64 {
        self.0.local_time_at(i)
    }
}

/// Chart-clock reads used to publish a group formed by equal requested
/// close instants. Separate from [`TimeAxis`] so fixed resampling
/// does not require a close-time callback.
pub(crate) trait PublicationAxis: TimeAxis {
    fn close_time(&self, i: usize) -> i64;
}

impl PublicationAxis for MarketData {
    fn close_time(&self, i: usize) -> i64 {
        self.time_close_at(i)
    }
}

/// Publication of one host-supplied closes-only group (Aeneas-extractable).
/// Reads retain the producer's short-circuit order, including invalid-input
/// panics: current close, requested close, length, next open, requested close.
/// A missing crossing returns the unpublished sentinel. This policy is distinct
/// from the host `request_bars` path, which publishes on the preceding chart bar.
pub(crate) fn clock_group_publication<T: PublicationAxis>(
    axis: &T,
    end: usize,
    requested_closes: &[i64],
) -> usize {
    if axis.close_time(end) >= requested_closes[end] {
        end
    } else if end + 1 < axis.axis_len() && axis.open_time(end + 1) >= requested_closes[end] {
        end + 1
    } else {
        usize::MAX
    }
}

/// Assign host-supplied closes-only publication clocks, in bar order.
/// Keeping payloads generic separates the integer assignment loop from
/// floating-point data; no payload operation or trait bound is required. A panic retains
/// earlier assignments and does not overwrite the failing or later bars.
pub(crate) fn publish_clock_groups<T: PublicationAxis, F>(
    axis: &T,
    bars: &mut [HtfBar<F>],
    requested_closes: &[i64],
) {
    let mut i = 0;
    while i < bars.len() {
        let bar = &mut bars[i];
        let end = bar.last_chart_bar;
        bar.publication_chart_bar = clock_group_publication(axis, end, requested_closes);
        i += 1;
    }
}

/// Per-bar fixed-width bucket ids (Aeneas-extractable), on the exchange
/// clock of `fixed_bucket_id`; empty for an empty feed.
pub(crate) fn fixed_bucket_ids<T: TimeAxis>(
    axis: &T,
    htf_seconds: i64,
    offset_ms: i64,
) -> Vec<i64> {
    let n = axis.axis_len();
    let mut ids: Vec<i64> = Vec::with_capacity(n);
    if n == 0 {
        return ids;
    }
    let step_ms = fixed_step_ms(htf_seconds);
    let first = axis.open_time(0);
    let mut i = 0;
    while i < n {
        ids.push(fixed_bucket_id(
            axis.open_time(i),
            first,
            step_ms,
            offset_ms,
        ));
        i += 1;
    }
    ids
}

/// Fixed-width resampling (Aeneas-extractable): the bucket ids, then the
/// bucket grouping and fold of [`resample_kernel`].
pub(crate) fn resample_fixed_kernel<T: TimeAxis, B, S: BarSource<B>>(
    axis: &T,
    src: &S,
    htf_seconds: i64,
    offset_ms: i64,
) -> (Vec<B>, Vec<usize>) {
    let ids = fixed_bucket_ids(axis, htf_seconds, offset_ms);
    resample_kernel(src, &ids)
}

/// Aggregate `data` into fixed-width higher-timeframe buckets of length
/// `htf_seconds`.
///
/// Buckets follow the clock in the exchange timezone (`fixed_bucket_id`),
/// not the feed's first bar. `htf_seconds` must be positive; a feed with zero
/// bars yields an empty result.
pub fn resample(data: &MarketData, htf_seconds: i64) -> Resampled {
    // A bucket no longer than the chart's period holds one chart bar. Read
    // on the first bar's offset, so a repeated wall-clock hour (clocks going
    // back) cannot merge two bars into one bucket.
    let (bars, bucket_of) = if htf_seconds <= timeframe_in_seconds(&data.timeframe) {
        let offset_ms = match data.bars() {
            0 => 0,
            _ => data
                .exchange_offset_at(data.time_at(0))
                .saturating_mul(1000),
        };
        resample_fixed_kernel(data, &OhlcvSource(data), htf_seconds, offset_ms)
    } else {
        resample_fixed_kernel(&LocalAxis(data), &OhlcvSource(data), htf_seconds, 0)
    };
    Resampled { bars, bucket_of }
}

/// Aggregate into calendar-aligned month buckets.
///
/// A host-provided `trading_day` is taken as given, and it is a calendar-date
/// IDENTITY already encoded at 00:00 UTC (see [`MarketData::trading_day`] and
/// `fallback_trading_day_for_time`, which consumes the exchange offset and
/// never converts back). It is therefore decomposed at a ZERO offset:
/// localizing it a second time would shift a negative-offset symbol's
/// first-of-month bar back a day and merge it into the previous bucket.
///
/// Without a lane the nominal chart-bar close is used. That value IS an
/// instant, so it is read in the exchange timezone; subtracting one
/// millisecond keeps a bar that closes exactly at midnight in the calendar day
/// it completed.
pub fn resample_calendar_months(data: &MarketData, months: u32) -> Resampled {
    let months = i64::from(months.max(1));
    resample_by_bucket(data, |i| calendar_month_bucket_id(data, i, months))
}

/// The bucket id [`resample_calendar_months`] gives chart bar `i`, for
/// `months >= 1`, on the exchange's clock at the bar's close. It reads only
/// bar `i`, so [`BucketOpens`] can group one appended bar at a time.
pub(crate) fn calendar_month_bucket_id(data: &MarketData, i: usize, months: i64) -> i64 {
    let (calendar_time, offset) = match data.trading_day.get(i).copied() {
        Some(trading_day) => (trading_day, 0),
        None => {
            let close = data.time_close_at(i).saturating_sub(1);
            (close, data.exchange_offset_at(close))
        }
    };
    month_bucket_of_instant(calendar_time, offset, months)
}

/// Calendar identity of a resolved request, distinct from nominal seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CalendarAlignment {
    Fixed,
    Months(u32),
    Weeks(u32),
}

/// Strict identity for a host clock. Unlike the permissive display helpers,
/// malformed or overflowing selectors must never silently become daily.
pub(crate) fn host_timeframe_key(tf: &str) -> Option<(i64, CalendarAlignment)> {
    if tf.len() > 32 {
        return None;
    }
    let tf = tf.trim();
    let unit = tf.chars().last().filter(char::is_ascii_alphabetic);
    let number = if unit.is_some() {
        &tf[..tf.len() - 1]
    } else {
        tf
    };
    if tf.is_empty() || (!number.is_empty() && !number.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let count: i64 = if number.is_empty() {
        1
    } else {
        number.parse().ok()?
    };
    if count <= 0 {
        return None;
    }
    let (factor, calendar) = match unit.map(|u| u.to_ascii_uppercase()) {
        None => (60, CalendarAlignment::Fixed),
        Some('S') => (1, CalendarAlignment::Fixed),
        Some('D') => (86400, CalendarAlignment::Fixed),
        Some('W') => (604800, CalendarAlignment::Weeks(u32::try_from(count).ok()?)),
        Some('M') => (
            2592000,
            CalendarAlignment::Months(u32::try_from(count).ok()?),
        ),
        Some(_) => return None,
    };
    let seconds = count.checked_mul(factor)?;
    seconds.checked_mul(1000)?;
    Some((seconds, calendar))
}

/// Monday-based calendar weeks, with multiweek blocks restarted at the first
/// Monday of the year containing that week's Monday. A week crossing New Year
/// is grouped by the year that contains its Monday. Host trading days are UTC date
/// identities; the fallback uses the nominal closing date in exchange time.
pub fn resample_calendar_weeks(data: &MarketData, weeks: u32) -> Resampled {
    let weeks = i64::from(weeks.max(1));
    resample_by_bucket(data, |i| calendar_week_bucket_id(data, i, weeks))
}

/// The bucket id [`resample_calendar_weeks`] gives chart bar `i`, for
/// `weeks >= 1`, on the exchange's clock at the bar's close. It reads only
/// bar `i`, so [`BucketOpens`] can group one appended bar at a time.
pub(crate) fn calendar_week_bucket_id(data: &MarketData, i: usize, weeks: i64) -> i64 {
    let day = match data.trading_day.get(i).copied() {
        Some(day) => day.div_euclid(DAY_MS),
        None => {
            let close = data.time_close_at(i).saturating_sub(1);
            close
                .saturating_add(data.exchange_offset_at(close).saturating_mul(1000))
                .div_euclid(DAY_MS)
        }
    };
    // Epoch day 4 is Monday 1970-01-05. Euclidean division also handles
    // pre-epoch bars and partially available opening weeks.
    let week = day.saturating_sub(4).div_euclid(7);
    if weeks == 1 {
        return week;
    }
    let monday = week.saturating_mul(7).saturating_add(4);
    let (year, ..) = civil_from_epoch_ms(monday.saturating_mul(DAY_MS), 0);
    let jan1 = days_from_civil(year, 1, 1).unwrap_or(monday);
    let first_monday =
        jan1.saturating_add((7 - jan1.saturating_add(3).rem_euclid(7)).rem_euclid(7));
    let first_week = first_monday.saturating_sub(4).div_euclid(7);
    first_week.saturating_add(
        week.saturating_sub(first_week)
            .div_euclid(weeks)
            .saturating_mul(weeks),
    )
}

impl MarketData {
    /// Why bars of this feed cannot be grouped into buckets of `seconds`
    /// (with calendar identity `calendar`), or `None` when they can.
    ///
    /// Where a day, and each bucket within it, starts depends on the
    /// session: under PineWASM's grouping model, intraday buckets start at the
    /// session's open, not at midnight, and a session that opens the evening
    /// before belongs to the next day. A grouping is computed only where the session leaves
    /// no doubt:
    ///
    /// - around the clock (`24x7`), or with no session declared on an
    ///   exchange that keeps UTC (a fixed zero offset), buckets walk from
    ///   local midnight;
    /// - a single day, a week or a month also groups a session that opens
    ///   and closes within one local day by that day's date;
    /// - a host-supplied `tradingDay` settles days, weeks and months for any
    ///   session;
    /// - a requested feed the host supplies (`requestBars`,
    ///   `requestTimeClose`) settles its own timeframe, when the caller
    ///   reads it (`host_feeds`);
    /// - a timeframe no longer than the chart's puts each bar in its own
    ///   bucket, wherever buckets start.
    ///
    /// Sub-day buckets are also refused when the exchange's clock changes
    /// during the bars: a repeated or skipped hour has no defined bucket
    /// rule yet. Anything else is refused rather than guessed.
    pub(crate) fn bucket_refusal(
        &self,
        seconds: i64,
        calendar: CalendarAlignment,
        host_feeds: bool,
    ) -> Option<String> {
        self.bucket_refusal_with(seconds, calendar, host_feeds, || self.clock_changes())
    }

    /// [`Self::bucket_refusal`], reading the clock-change check from
    /// `clock_changes` so an incremental caller can scan only new bars.
    fn bucket_refusal_with(
        &self,
        seconds: i64,
        calendar: CalendarAlignment,
        host_feeds: bool,
        clock_changes: impl FnOnce() -> bool,
    ) -> Option<String> {
        let host_feed = |tf: &String| host_timeframe_key(tf) == Some((seconds, calendar));
        if host_feeds
            && (self.request_bars.keys().any(host_feed)
                || self.request_time_close.keys().any(host_feed))
        {
            return None;
        }
        let (chart_multiplier, chart_unit) = timeframe_parts(&self.timeframe);
        let chart_count = u32::try_from(chart_multiplier).unwrap_or(u32::MAX).max(1);
        let chart_calendar = match chart_unit {
            Some('M') => CalendarAlignment::Months(chart_count),
            Some('W') => CalendarAlignment::Weeks(chart_count),
            _ => CalendarAlignment::Fixed,
        };
        let own_or_lower = match calendar {
            CalendarAlignment::Fixed => seconds <= timeframe_in_seconds(&self.timeframe),
            _ => calendar == chart_calendar,
        };
        if own_or_lower {
            return None;
        }
        if calendar == CalendarAlignment::Fixed && seconds < 86_400 && clock_changes() {
            return Some(format!(
                "PineWASM does not group these bars into {} buckets: the exchange's clock (`{}`) changes during them, and where a bucket starts across a repeated or skipped hour is not settled yet",
                crate::session::timeframe_from_seconds(seconds),
                self.syminfo.timezone
            ));
        }
        let shape = crate::session::session_shape(&self.syminfo.session_hours)?;
        let utc = matches!(self.exchange_zone(), pinewasm_tz::TimeZone::Fixed(0));
        let lane = !self.trading_day.is_empty();
        use pinewasm_tz::SessionShape as S;
        let continuous = matches!(shape, S::Continuous) || (shape == S::Absent && utc);
        let grouped = match calendar {
            CalendarAlignment::Months(_) | CalendarAlignment::Weeks(_) => {
                continuous || lane || shape == S::WithinDay
            }
            CalendarAlignment::Fixed if seconds == 86_400 => {
                continuous || lane || shape == S::WithinDay
            }
            CalendarAlignment::Fixed => continuous,
        };
        if grouped {
            return None;
        }
        let timeframe = match calendar {
            CalendarAlignment::Months(n) => format!("{n}M"),
            CalendarAlignment::Weeks(n) => format!("{n}W"),
            CalendarAlignment::Fixed => crate::session::timeframe_from_seconds(seconds),
        };
        let why = if self.syminfo.session_hours.trim().is_empty() {
            format!(
                "the feed declares no session (syminfo.session_hours) for exchange timezone `{}`",
                self.syminfo.timezone
            )
        } else {
            format!(
                "where session `{}` starts each {timeframe} bucket is not settled yet",
                self.syminfo.session_hours
            )
        };
        Some(format!(
            "PineWASM does not group these bars into {timeframe} buckets: {why}. A 24x7 session, a tradingDay lane (for days, weeks and months), or the requested timeframe's own bars lift this"
        ))
    }

    /// Whether the exchange timezone's offset differs between any two bars.
    fn clock_changes(&self) -> bool {
        let zone = self.exchange_zone();
        if matches!(zone, pinewasm_tz::TimeZone::Fixed(_)) || self.bars() == 0 {
            return false;
        }
        let first = crate::session::offset_at_ms(&zone, self.time_at(0));
        (1..self.bars()).any(|i| crate::session::offset_at_ms(&zone, self.time_at(i)) != first)
    }

    /// Whether the offset differs from bar 0's anywhere in the feed, scanning
    /// only the bars `scan` has not yet covered. Valid while bars are only
    /// appended.
    fn clock_changes_incremental(&self, scan: &mut ClockScan) -> bool {
        let zone = self.exchange_zone();
        if matches!(zone, pinewasm_tz::TimeZone::Fixed(_)) || self.bars() == 0 {
            return false;
        }
        if scan.changed {
            return true;
        }
        let first = *scan
            .first_offset
            .get_or_insert_with(|| crate::session::offset_at_ms(&zone, self.time_at(0)));
        let from = scan.checked.max(1);
        scan.changed = (from..self.bars())
            .any(|i| crate::session::offset_at_ms(&zone, self.time_at(i)) != first);
        scan.checked = self.bars();
        scan.changed
    }

    /// [`Self::timeframe_bucket_refusal`] that remembers its clock-change
    /// scan in `scan`, so each call inspects only the bars appended since.
    pub(crate) fn timeframe_bucket_refusal_incremental(
        &self,
        timeframe: &str,
        scan: &mut ClockScan,
    ) -> Option<String> {
        let (multiplier, unit) = timeframe_parts(timeframe);
        let count = u32::try_from(multiplier).unwrap_or(u32::MAX);
        let calendar = match unit {
            Some('M') => CalendarAlignment::Months(count),
            Some('W') => CalendarAlignment::Weeks(count),
            _ => CalendarAlignment::Fixed,
        };
        self.bucket_refusal_with(timeframe_in_seconds(timeframe), calendar, false, || {
            self.clock_changes_incremental(scan)
        })
    }

    /// [`Self::bucket_refusal`] for a timeframe string.
    pub(crate) fn timeframe_bucket_refusal(&self, timeframe: &str) -> Option<String> {
        let (multiplier, unit) = timeframe_parts(timeframe);
        let count = u32::try_from(multiplier).unwrap_or(u32::MAX);
        let calendar = match unit {
            Some('M') => CalendarAlignment::Months(count),
            Some('W') => CalendarAlignment::Weeks(count),
            _ => CalendarAlignment::Fixed,
        };
        // `time` groups the chart's own bars (`BucketOpens`); it never reads
        // a host-supplied requested feed.
        self.bucket_refusal(timeframe_in_seconds(timeframe), calendar, false)
    }
}

/// [`resample`], with one-day buckets taken from the host's trading-day lane
/// when it supplies one, as [`MarketData::bucket_refusal`] assumes.
pub(crate) fn resample_days_or_fixed(data: &MarketData, seconds: i64) -> Resampled {
    if seconds == 86_400 && !data.trading_day.is_empty() {
        return resample_by_bucket(data, |i| data.trading_day[i]);
    }
    resample(data, seconds)
}

/// Resample a timeframe while preserving calendar week/month identity.
pub fn resample_timeframe(data: &MarketData, timeframe: &str) -> Resampled {
    let (multiplier, unit) = timeframe_parts(timeframe);
    let count = u32::try_from(multiplier).unwrap_or(u32::MAX);
    match unit {
        Some('M') => resample_calendar_months(data, count),
        Some('W') => resample_calendar_weeks(data, count),
        _ => resample(data, timeframe_in_seconds(timeframe)),
    }
}

/// How [`resample_timeframe`] assigns one chart bar to a bucket, resolved
/// once per timeframe. A bar's bucket id reads only that bar (and, for a
/// fixed width of a day or more, the first bar's open), so appending bars never moves an
/// earlier bar to another bucket unless the append also brings the
/// trading-day lane that calendar buckets read ([`BucketOpens`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeframeBuckets {
    /// Fixed-width buckets on the exchange clock ([`resample`]).
    Fixed { step_ms: i64 },
    /// Calendar months ([`resample_calendar_months`]).
    Months { months: i64 },
    /// Calendar weeks ([`resample_calendar_weeks`]).
    Weeks { weeks: i64 },
}

impl TimeframeBuckets {
    /// The grouping [`resample_timeframe`] selects for `timeframe`.
    fn new(timeframe: &str) -> Self {
        let (multiplier, unit) = timeframe_parts(timeframe);
        let count = i64::from(u32::try_from(multiplier).unwrap_or(u32::MAX).max(1));
        match unit {
            Some('M') => Self::Months { months: count },
            Some('W') => Self::Weeks { weeks: count },
            _ => Self::Fixed {
                step_ms: fixed_step_ms(timeframe_in_seconds(timeframe)),
            },
        }
    }

    /// The bucket id of chart bar `i < data.bars()`. A host's trading-day
    /// lane settles which day a bar belongs to.
    fn bucket_id(self, data: &MarketData, i: usize) -> i64 {
        match self {
            Self::Fixed { step_ms } if step_ms == DAY_MS && !data.trading_day.is_empty() => {
                data.trading_day[i]
            }
            Self::Fixed { step_ms } => {
                fixed_bucket_id(data.local_time_at(i), data.local_time_at(0), step_ms, 0)
            }
            Self::Months { months } => calendar_month_bucket_id(data, i, months),
            Self::Weeks { weeks } => calendar_week_bucket_id(data, i, weeks),
        }
    }
}

/// The open time of each chart bar's [`resample_timeframe`] bucket: the open
/// of the first bar grouped into it. This is what `time(timeframe)` reads.
///
/// Bars are grouped one at a time, so a feed that grows by realtime appends
/// extends the opens instead of regrouping every bar: appending never moves
/// an earlier bar's bucket or that bucket's open. The one exception is the
/// trading-day lane. Calendar buckets read it in preference to a bar's close,
/// and a host update that brings a `tradingDay` to a feed without a lane
/// materializes one for every bar, so its appearance regroups calendar
/// buckets once. Updates without a `tradingDay` leave an absent lane absent, so
/// the appended bars keep the close-date grouping one run over the same bars
/// uses (`docs/runtime.md`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BucketOpens {
    buckets: TimeframeBuckets,
    /// How far the clock-change refusal has already scanned the feed.
    clock_scan: ClockScan,
    /// Whether `opens` was grouped while the feed carried a trading-day lane.
    trading_day_lane: bool,
    /// `opens[i]` is the bucket open of chart bar `i`.
    opens: Vec<i64>,
}

/// Progress of the exchange-clock-change scan over an append-only feed.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ClockScan {
    /// Bars already compared against bar 0's offset.
    pub(crate) checked: usize,
    first_offset: Option<i64>,
    changed: bool,
}

impl BucketOpens {
    /// The clock-change scan state, for [`MarketData::timeframe_bucket_refusal_incremental`].
    pub(crate) fn clock_scan_mut(&mut self) -> &mut ClockScan {
        &mut self.clock_scan
    }

    #[cfg(test)]
    pub(crate) fn clock_scan_ref(&self) -> &ClockScan {
        &self.clock_scan
    }

    /// Group every bar of `data` into `timeframe` buckets.
    pub(crate) fn new(data: &MarketData, timeframe: &str) -> Self {
        let mut opens = BucketOpens {
            buckets: TimeframeBuckets::new(timeframe),
            clock_scan: ClockScan::default(),
            trading_day_lane: !data.trading_day.is_empty(),
            opens: Vec::with_capacity(data.bars()),
        };
        opens.group_appended(data);
        opens
    }

    /// The bars [`Self::refresh`] groups to bring these opens up to date with
    /// `data`: the bars appended since they were last grouped, or every bar
    /// when calendar buckets must be regrouped over a new trading-day lane.
    pub(crate) fn stale_bars(&self, data: &MarketData) -> usize {
        if self.regroups(data) {
            data.bars()
        } else {
            data.bars().saturating_sub(self.opens.len())
        }
    }

    /// Bring the opens up to date with `data`, a later state of the feed they
    /// were grouped over in which bars were only appended: the result equals
    /// [`Self::new`] over `data`, at the cost of [`Self::stale_bars`].
    pub(crate) fn refresh(&mut self, data: &MarketData) {
        if self.regroups(data) {
            self.opens.clear();
        }
        self.trading_day_lane = !data.trading_day.is_empty();
        self.group_appended(data);
    }

    /// Whether calendar buckets were grouped without the trading-day lane
    /// `data` now carries (or with one it no longer carries). Fixed widths
    /// read only bar opens.
    fn regroups(&self, data: &MarketData) -> bool {
        // Calendar buckets, and days, read the lane.
        let calendar =
            !matches!(self.buckets, TimeframeBuckets::Fixed { step_ms } if step_ms != DAY_MS);
        let lane = !data.trading_day.is_empty();
        calendar && lane != self.trading_day_lane
    }

    /// The number of chart bars grouped so far.
    pub(crate) fn grouped_bars(&self) -> usize {
        self.opens.len()
    }

    /// The bucket open of `chart_bar`, or `None` past the grouped bars.
    pub(crate) fn get(&self, chart_bar: usize) -> Option<i64> {
        self.opens.get(chart_bar).copied()
    }

    /// Group the bars of `data` after the ones already grouped. Like
    /// [`bucket_index`], a bar opens a new bucket exactly when its id differs
    /// from the previous bar's. This repeats that rule one bar at a time
    /// instead of calling the extracted kernel, which groups a whole id
    /// slice, so `bucket_opens_tests` holds the two to the same result.
    fn group_appended(&mut self, data: &MarketData) {
        let first = self.opens.len();
        let mut previous = first
            .checked_sub(1)
            .map(|bar| self.buckets.bucket_id(data, bar));
        for bar in first..data.bars() {
            let id = self.buckets.bucket_id(data, bar);
            let open = if previous == Some(id) {
                self.opens[bar - 1]
            } else {
                data.time_at(bar)
            };
            self.opens.push(open);
            previous = Some(id);
        }
    }
}

pub(crate) fn resample_aligned_security(
    data: &MarketData,
    seconds: i64,
    calendar: CalendarAlignment,
) -> Resampled {
    if let Some((_, feed)) = data
        .request_bars
        .iter()
        .find(|(tf, _)| host_timeframe_key(tf) == Some((seconds, calendar)))
    {
        let n = data.bars();
        let mut bucket_of = Vec::with_capacity(n);
        for chart_bar in 0..n {
            let time = data.time_at(chart_bar);
            // MarketData validation guarantees that the feed brackets every
            // chart open and that feed bars are ordered and nonoverlapping.
            let bucket = feed.time.partition_point(|&open| open <= time) - 1;
            bucket_of.push(bucket);
        }
        let mut bars: Vec<HtfBar> = (0..feed.time.len())
            .map(|bucket| HtfBar {
                open: feed.open[bucket],
                high: feed.high[bucket],
                low: feed.low[bucket],
                close: feed.close[bucket],
                volume: feed.volume[bucket],
                time: feed.time[bucket],
                last_chart_bar: 0,
                publication_chart_bar: usize::MAX,
            })
            .collect();
        for (chart_bar, &bucket) in bucket_of.iter().enumerate() {
            bars[bucket].last_chart_bar = chart_bar;
            if bars[bucket].publication_chart_bar == usize::MAX
                && (data.time_close_at(chart_bar) >= feed.time_close[bucket]
                    || (chart_bar + 1 < n
                        && data.time_at(chart_bar + 1) >= feed.time_close[bucket]))
            {
                bars[bucket].publication_chart_bar = chart_bar;
            }
        }
        return Resampled { bars, bucket_of };
    }
    if let Some((_, closes)) = data
        .request_time_close
        .iter()
        .find(|(tf, _)| host_timeframe_key(tf) == Some((seconds, calendar)))
    {
        // Equal closing instants identify the same requested
        // bar. OHLCV still comes entirely from the supplied chart feed.
        let lanes = crate::vm_publication_axis::market_data_lanes(data);
        let (bars, bucket_of) = requested_close_resample(data, &OhlcvSource(data), &lanes, closes);
        return Resampled { bars, bucket_of };
    }
    match calendar {
        CalendarAlignment::Months(months) => resample_calendar_months(data, months),
        CalendarAlignment::Weeks(weeks) => resample_calendar_weeks(data, weeks),
        CalendarAlignment::Fixed if seconds == 86_400 && !data.trading_day.is_empty() => {
            resample_by_bucket(data, |i| data.trading_day[i])
        }
        CalendarAlignment::Fixed => resample(data, seconds),
    }
}

/// Resample a compiled `request.security` timeframe. `calendar_months`
/// distinguishes a calendar `M` timeframe from an equal nominal duration such
/// as `30D`.
pub fn resample_security(
    data: &MarketData,
    htf_seconds: i64,
    calendar_months: Option<u32>,
) -> Resampled {
    resample_aligned_security(
        data,
        htf_seconds,
        calendar_months.map_or(CalendarAlignment::Fixed, CalendarAlignment::Months),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Copy of the full production function, kept independent of
    // requested_close_buckets and its closes-only branch assembly.
    fn requested_close_frozen_full_reference(
        data: &MarketData,
        seconds: i64,
        calendar: CalendarAlignment,
    ) -> Resampled {
        if let Some((_, feed)) = data
            .request_bars
            .iter()
            .find(|(tf, _)| host_timeframe_key(tf) == Some((seconds, calendar)))
        {
            let n = data.bars();
            let mut bucket_of = Vec::with_capacity(n);
            for chart_bar in 0..n {
                let time = data.time_at(chart_bar);
                // MarketData validation guarantees that the feed brackets every
                // chart open and that feed bars are ordered and nonoverlapping.
                let bucket = feed.time.partition_point(|&open| open <= time) - 1;
                bucket_of.push(bucket);
            }
            let mut bars: Vec<HtfBar> = (0..feed.time.len())
                .map(|bucket| HtfBar {
                    open: feed.open[bucket],
                    high: feed.high[bucket],
                    low: feed.low[bucket],
                    close: feed.close[bucket],
                    volume: feed.volume[bucket],
                    time: feed.time[bucket],
                    last_chart_bar: 0,
                    publication_chart_bar: usize::MAX,
                })
                .collect();
            for (chart_bar, &bucket) in bucket_of.iter().enumerate() {
                bars[bucket].last_chart_bar = chart_bar;
                if bars[bucket].publication_chart_bar == usize::MAX
                    && (data.time_close_at(chart_bar) >= feed.time_close[bucket]
                        || (chart_bar + 1 < n
                            && data.time_at(chart_bar + 1) >= feed.time_close[bucket]))
                {
                    bars[bucket].publication_chart_bar = chart_bar;
                }
            }
            return Resampled { bars, bucket_of };
        }
        if let Some((_, closes)) = data
            .request_time_close
            .iter()
            .find(|(tf, _)| host_timeframe_key(tf) == Some((seconds, calendar)))
        {
            // Equal closing instants identify the same requested
            // bar. OHLCV still comes entirely from the supplied chart feed.
            let mut r = resample_by_bucket(data, |i| closes[i]);
            crate::vm_publication_axis::publish_market_data(data, &mut r.bars, closes);
            return r;
        }
        match calendar {
            CalendarAlignment::Months(months) => resample_calendar_months(data, months),
            CalendarAlignment::Weeks(weeks) => resample_calendar_weeks(data, weeks),
            CalendarAlignment::Fixed => resample(data, seconds),
        }
    }

    type RequestedCloseSnapshot = (Vec<([u64; 5], i64, usize, usize)>, Vec<usize>);

    fn requested_close_outcome(
        run: impl FnOnce() -> Resampled,
    ) -> Result<RequestedCloseSnapshot, String> {
        use std::panic::{catch_unwind, AssertUnwindSafe};
        catch_unwind(AssertUnwindSafe(run))
            .map(|r| {
                (
                    r.bars.iter().map(publication_bar_bits).collect(),
                    r.bucket_of,
                )
            })
            .map_err(|error| {
                if let Some(message) = error.downcast_ref::<String>() {
                    message.clone()
                } else if let Some(message) = error.downcast_ref::<&str>() {
                    (*message).to_owned()
                } else {
                    "non-string panic".to_owned()
                }
            })
    }

    fn requested_close_compare_full(data: &MarketData) {
        let actual = requested_close_outcome(|| {
            resample_aligned_security(data, 7 * 86_400, CalendarAlignment::Weeks(1))
        });
        let old = requested_close_outcome(|| {
            requested_close_frozen_full_reference(data, 7 * 86_400, CalendarAlignment::Weeks(1))
        });
        assert_eq!(actual, old);
    }

    #[test]
    fn requested_close_buckets_uses_exact_price_length_and_prefix() {
        let mut data = daily_feed(&[(1.0, 2.0, 0.0, 1.0, 3.0); 4]);
        data.time.clear();
        data.time_close = vec![7, 8];
        let expected = BucketIndex {
            bucket_of: vec![0, 0, 1, 1],
            last_chart_bar: vec![1, 3],
        };
        assert_eq!(
            requested_close_buckets(&data, &[10, 10, 20, 20, 30]),
            expected
        );
        assert_eq!(
            requested_close_buckets(&data, &[10, 10, 20, 20, i64::MIN]),
            expected
        );
        data.close = vec![f64::NAN, -0.0, f64::INFINITY, f64::NEG_INFINITY];
        assert_eq!(requested_close_buckets(&data, &[10, 10, 20, 20]), expected);
    }

    #[test]
    fn requested_close_buckets_contiguous_groups_and_signed_boundaries() {
        let data = daily_feed(&[(1.0, 2.0, 0.0, 1.0, 3.0); 6]);
        assert_eq!(
            requested_close_buckets(&data, &[i64::MAX, i64::MIN, i64::MAX, 0, 0, i64::MAX]),
            BucketIndex {
                bucket_of: vec![0, 1, 2, 3, 3, 4],
                last_chart_bar: vec![0, 1, 2, 4, 5],
            }
        );
        let one = daily_feed(&[(1.0, 2.0, 0.0, 1.0, 3.0)]);
        assert_eq!(
            requested_close_buckets(&one, &[i64::MIN]),
            BucketIndex {
                bucket_of: vec![0],
                last_chart_bar: vec![0]
            }
        );
    }

    #[test]
    fn requested_close_buckets_empty_feed_reads_no_requested_or_ohlcv_lane() {
        let mut data = MarketData {
            time: vec![i64::MAX; 3],
            time_close: vec![i64::MIN; 2],
            ..MarketData::default()
        };
        let empty = BucketIndex {
            bucket_of: vec![],
            last_chart_bar: vec![],
        };
        assert_eq!(requested_close_buckets(&data, &[]), empty);
        assert_eq!(requested_close_buckets(&data, &[i64::MIN, i64::MAX]), empty);
        data.request_time_close.insert("W".into(), vec![]);
        requested_close_compare_full(&data);
        assert!(
            resample_aligned_security(&data, 7 * 86_400, CalendarAlignment::Weeks(1))
                .bars
                .is_empty()
        );
    }

    #[test]
    fn requested_close_buckets_short_slice_fails_before_any_ohlcv_fold() {
        let mut data = daily_feed(&[(1.0, 2.0, 0.0, 1.0, 3.0); 3]);
        data.open.clear();
        data.high.clear();
        data.time.clear();
        data.time_close.clear();
        data.request_time_close.insert("W".into(), vec![10]);
        requested_close_compare_full(&data);
        let error = requested_close_outcome(|| {
            resample_aligned_security(&data, 7 * 86_400, CalendarAlignment::Weeks(1))
        })
        .unwrap_err();
        assert!(
            error.contains("len is 1") && error.contains("index is 1"),
            "{error}"
        );
        data.request_time_close.insert("W".into(), vec![10, 10, 10]);
        requested_close_compare_full(&data);
        let error = requested_close_outcome(|| {
            resample_aligned_security(&data, 7 * 86_400, CalendarAlignment::Weeks(1))
        })
        .unwrap_err();
        assert!(
            error.contains("len is 0") && error.contains("index is 0"),
            "{error}"
        );
    }

    #[test]
    fn requested_close_buckets_full_branch_matches_frozen_4096_cases() {
        let bits = [
            0,
            0x8000_0000_0000_0000,
            0x7ff8_1234_5678_9abc,
            0x7ff0_0000_0000_0001,
            0x7ff0_0000_0000_0000,
            0xfff0_0000_0000_0000,
            1,
            0x3ff0_0000_0000_0000,
        ];
        for case in 0..4096usize {
            let n = case % 8;
            let mut data = daily_feed(&vec![(1.0, 2.0, 0.0, 1.0, 3.0); n]);
            let before_len = data.close.len();
            for i in 0..n {
                data.open[i] = f64::from_bits(bits[(case + i) % bits.len()]);
                data.high[i] = f64::from_bits(bits[(case + i + 1) % bits.len()]);
                data.low[i] = f64::from_bits(bits[(case + i + 2) % bits.len()]);
                data.close[i] = f64::from_bits(bits[(case + i + 3) % bits.len()]);
                data.volume[i] = f64::from_bits(bits[(case + i + 4) % bits.len()]);
                data.time[i] = i as i64 * 10;
            }
            data.time_close = (0..n).map(|i| i as i64 * 10 + 5).collect();
            let close_len = (case / 8) % 10;
            let closes = (0..close_len)
                .map(|i| match (case / 80) % 4 {
                    0 => (i / 2) as i64 * 20 + 7,
                    1 => {
                        if i % 2 == 0 {
                            i64::MIN
                        } else {
                            i64::MAX
                        }
                    }
                    2 => 50,
                    _ => i as i64 * 10 + 7,
                })
                .collect();
            data.request_time_close.insert("W".into(), closes);
            match (case / 320) % 5 {
                0 => data.time_close.clear(),
                1 => data.time_close.truncate(n / 2),
                2 => data.time.truncate(n / 2),
                3 => data.open.truncate(n / 2),
                _ => {}
            }
            let prices = data.close.iter().map(|v| v.to_bits()).collect::<Vec<_>>();
            requested_close_compare_full(&data);
            assert_eq!(data.close.len(), before_len);
            assert_eq!(
                data.close.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                prices
            );
        }
    }

    #[test]
    fn requested_close_buckets_publication_delay_sentinel_and_payload_bits() {
        let mut data = daily_feed(&[(1.0, 2.0, 0.0, -0.0, 3.0); 5]);
        data.time = vec![0, 3, 7, 11, 14];
        data.time_close = vec![1, 4, 8, 12, 15];
        data.open[0] = f64::from_bits(0x7ff8_1234_5678_9abc);
        data.volume[4] = f64::from_bits(0x7ff0_0000_0000_0001);
        data.request_time_close
            .insert("W".into(), vec![5, 5, 12, 12, 19]);
        requested_close_compare_full(&data);
        let result = resample_aligned_security(&data, 7 * 86_400, CalendarAlignment::Weeks(1));
        assert_eq!(result.bucket_of, [0, 0, 1, 1, 2]);
        assert_eq!(
            result
                .bars
                .iter()
                .map(|b| b.last_chart_bar)
                .collect::<Vec<_>>(),
            [1, 3, 4]
        );
        assert_eq!(
            result
                .bars
                .iter()
                .map(|b| b.publication_chart_bar)
                .collect::<Vec<_>>(),
            [2, 3, usize::MAX]
        );
        assert_eq!(result.bars[0].open.to_bits(), data.open[0].to_bits());
        assert_eq!(result.bars[2].volume.to_bits(), data.volume[4].to_bits());
    }

    #[test]
    fn requested_close_buckets_request_bars_priority_stays_separate() {
        let mut data = daily_feed(&[(1.0, 2.0, 0.0, 1.0, 3.0); 2]);
        data.time = vec![0, 10];
        data.time_close = vec![5, 15];
        data.request_time_close.insert("W".into(), vec![]);
        data.request_bars.insert(
            "W".into(),
            crate::RequestTimeframeBars {
                time: vec![0, 10],
                time_close: vec![7, 17],
                open: vec![1.0; 2],
                high: vec![2.0; 2],
                low: vec![0.0; 2],
                close: vec![1.0; 2],
                volume: vec![3.0; 2],
            },
        );
        requested_close_compare_full(&data);
        let result = resample_aligned_security(&data, 7 * 86_400, CalendarAlignment::Weeks(1));
        assert_eq!(result.bars[0].publication_chart_bar, 0);
    }

    struct PublicationCheck {
        close: i64,
        next_open: i64,
        len: usize,
        panic_at: Option<&'static str>,
        reads: std::cell::RefCell<Vec<(&'static str, usize)>>,
    }

    impl PublicationCheck {
        fn new(close: i64, next_open: i64, len: usize) -> Self {
            Self {
                close,
                next_open,
                len,
                panic_at: None,
                reads: std::cell::RefCell::new(Vec::new()),
            }
        }

        fn read(&self, kind: &'static str, index: usize) {
            self.reads.borrow_mut().push((kind, index));
            assert_ne!(self.panic_at, Some(kind), "unexpected {kind} callback");
        }
    }

    impl TimeAxis for PublicationCheck {
        fn axis_len(&self) -> usize {
            self.read("len", 0);
            self.len
        }

        fn open_time(&self, i: usize) -> i64 {
            self.read("open", i);
            self.next_open
        }
    }

    impl PublicationAxis for PublicationCheck {
        fn close_time(&self, i: usize) -> i64 {
            self.read("close", i);
            self.close
        }
    }

    // Copy of the publication expression.
    // Kept independent of clock_group_publication to detect policy/order drift.
    fn publication_reference<T: PublicationAxis>(axis: &T, end: usize, closes: &[i64]) -> usize {
        if axis.close_time(end) >= closes[end] {
            end
        } else if end + 1 < axis.axis_len() && axis.open_time(end + 1) >= closes[end] {
            end + 1
        } else {
            usize::MAX
        }
    }

    #[test]
    fn clock_group_publication_matches_frozen_expression_and_read_order() {
        let times = [i64::MIN, -1, 0, 1, i64::MAX];
        for close in times {
            for next_open in times {
                for requested_close in times {
                    for end in 0..3 {
                        for len in 0..4 {
                            let actual_axis = PublicationCheck::new(close, next_open, len);
                            let reference_axis = PublicationCheck::new(close, next_open, len);
                            let closes = [requested_close; 3];
                            assert_eq!(
                                clock_group_publication(&actual_axis, end, &closes),
                                publication_reference(&reference_axis, end, &closes),
                                "close={close}, next={next_open}, requested={requested_close}, end={end}, len={len}"
                            );
                            assert_eq!(actual_axis.reads, reference_axis.reads);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn clock_group_publication_boundary_and_lazy_reads() {
        let mut axis = PublicationCheck::new(7, 10, 2);
        axis.panic_at = Some("len");
        assert_eq!(clock_group_publication(&axis, 0, &[7]), 0);
        assert_eq!(*axis.reads.borrow(), [("close", 0)]);

        let mut axis = PublicationCheck::new(6, 7, 1);
        axis.panic_at = Some("open");
        assert_eq!(clock_group_publication(&axis, 0, &[7]), usize::MAX);
        assert_eq!(*axis.reads.borrow(), [("close", 0), ("len", 0)]);

        let axis = PublicationCheck::new(6, 7, 2);
        assert_eq!(clock_group_publication(&axis, 0, &[7]), 1);
        assert_eq!(
            *axis.reads.borrow(),
            [("close", 0), ("len", 0), ("open", 1)]
        );
        let axis = PublicationCheck::new(6, 6, 2);
        assert_eq!(clock_group_publication(&axis, 0, &[7]), usize::MAX);
        // A current close wins even for an axis with no later chart bar.
        let axis = PublicationCheck::new(i64::MAX, i64::MIN, 0);
        assert_eq!(clock_group_publication(&axis, 0, &[i64::MAX]), 0);
    }

    #[test]
    fn clock_group_publication_preserves_malformed_input_panics() {
        use std::panic::{catch_unwind, AssertUnwindSafe};

        for panic_at in [None, Some("close"), Some("len"), Some("open")] {
            for closes in [vec![], vec![7]] {
                for len in [1, 2] {
                    let mut actual_axis = PublicationCheck::new(6, 7, len);
                    let mut reference_axis = PublicationCheck::new(6, 7, len);
                    actual_axis.panic_at = panic_at;
                    reference_axis.panic_at = panic_at;
                    let actual = catch_unwind(AssertUnwindSafe(|| {
                        clock_group_publication(&actual_axis, 0, &closes)
                    }));
                    let reference = catch_unwind(AssertUnwindSafe(|| {
                        publication_reference(&reference_axis, 0, &closes)
                    }));
                    assert_eq!(actual.as_ref().ok(), reference.as_ref().ok());
                    assert_eq!(actual_axis.reads, reference_axis.reads);
                    if closes.is_empty() {
                        // The requested slice fails only after reading current close,
                        // and before either length or next-open callback.
                        assert_eq!(*actual_axis.reads.borrow(), [("close", 0)]);
                    }
                }
            }
        }
        // Both expressions fail their slice index before attempting end + 1.
        let axis = PublicationCheck::new(6, 7, usize::MAX);
        assert!(catch_unwind(AssertUnwindSafe(|| {
            clock_group_publication(&axis, usize::MAX, &[])
        }))
        .is_err());
        assert_eq!(*axis.reads.borrow(), [("close", usize::MAX)]);
    }

    #[test]
    fn clock_group_publication_is_used_by_authoritative_clock_producer() {
        let mut md = daily_feed(&[(1.0, 1.0, 1.0, 1.0, 1.0); 5]);
        md.time = vec![0, 3, 7, 11, 14];
        md.time_close = vec![1, 4, 8, 12, 15];
        md.request_time_close
            .insert("W".into(), vec![5, 5, 12, 12, 19]);
        assert!(md.validate().is_ok());
        let mut reference = resample_by_bucket(&md, |i| md.request_time_close["W"][i]);
        for bar in &mut reference.bars {
            bar.publication_chart_bar =
                publication_reference(&md, bar.last_chart_bar, &md.request_time_close["W"]);
        }
        let actual = resample_aligned_security(&md, 7 * 86_400, CalendarAlignment::Weeks(1));
        assert_eq!(actual, reference);
        assert_eq!(
            actual
                .bars
                .iter()
                .map(|b| b.publication_chart_bar)
                .collect::<Vec<_>>(),
            [2, 3, usize::MAX]
        );
    }

    #[test]
    fn clock_group_publication_keeps_request_bars_policy_distinct() {
        let mut md = daily_feed(&[(1.0, 1.0, 1.0, 1.0, 1.0); 2]);
        md.time = vec![0, 10];
        md.time_close = vec![5, 15];
        md.request_time_close.insert("W".into(), vec![7, 17]);
        assert!(md.validate().is_ok());
        let clocks = resample_aligned_security(&md, 7 * 86_400, CalendarAlignment::Weeks(1));
        assert_eq!(clocks.bars[0].publication_chart_bar, 1);
        md.request_bars.insert(
            "W".into(),
            crate::RequestTimeframeBars {
                time: vec![0, 10],
                time_close: vec![7, 17],
                open: vec![1.0; 2],
                high: vec![1.0; 2],
                low: vec![1.0; 2],
                close: vec![1.0; 2],
                volume: vec![1.0; 2],
            },
        );
        assert!(md.validate().is_ok());
        let supplied = resample_aligned_security(&md, 7 * 86_400, CalendarAlignment::Weeks(1));
        assert_eq!(supplied.bars[0].publication_chart_bar, 0);
        assert_eq!(clocks.bars[1].publication_chart_bar, usize::MAX);
        assert_eq!(supplied.bars[1].publication_chart_bar, usize::MAX);
    }

    // Copy of the assignment loop.
    // Keep the per-group expression independent of the helper too.
    fn publish_reference<T: PublicationAxis, F>(axis: &T, bars: &mut [HtfBar<F>], closes: &[i64]) {
        for bar in bars {
            let end = bar.last_chart_bar;
            bar.publication_chart_bar = publication_reference(axis, end, closes);
        }
    }

    fn publication_test_bar(last: usize, publication: usize) -> HtfBar {
        HtfBar {
            open: f64::from_bits(0x8000_0000_0000_0000),
            high: f64::from_bits(0x7ff8_1234_5678_9abc),
            low: f64::from_bits(0xfff0_0000_0000_0000),
            close: f64::from_bits(0x7ff0_0000_0000_0001),
            volume: f64::from_bits(0x7fef_ffff_ffff_ffff),
            time: -37,
            last_chart_bar: last,
            publication_chart_bar: publication,
        }
    }

    fn publication_bar_bits(bar: &HtfBar) -> ([u64; 5], i64, usize, usize) {
        (
            [
                bar.open.to_bits(),
                bar.high.to_bits(),
                bar.low.to_bits(),
                bar.close.to_bits(),
                bar.volume.to_bits(),
            ],
            bar.time,
            bar.last_chart_bar,
            bar.publication_chart_bar,
        )
    }

    #[test]
    fn publish_clock_groups_matches_frozen_loop_and_read_order() {
        let times = [i64::MIN, -1, 0, 1, i64::MAX];
        let groups: [&[usize]; 4] = [&[], &[0, 1, 2], &[2, 0, 2], &[1]];
        let mut cases = 0;
        for close in times {
            for next_open in times {
                for target in times {
                    for len in 0..4 {
                        for group in groups {
                            let actual_axis = PublicationCheck::new(close, next_open, len);
                            let reference_axis = PublicationCheck::new(close, next_open, len);
                            let mut actual: Vec<_> =
                                group.iter().map(|&i| publication_test_bar(i, 91)).collect();
                            let mut reference = actual.clone();
                            let closes = [target; 3];
                            publish_clock_groups(&actual_axis, &mut actual, &closes);
                            publish_reference(&reference_axis, &mut reference, &closes);
                            assert_eq!(
                                actual.iter().map(publication_bar_bits).collect::<Vec<_>>(),
                                reference
                                    .iter()
                                    .map(publication_bar_bits)
                                    .collect::<Vec<_>>()
                            );
                            assert_eq!(actual_axis.reads, reference_axis.reads);
                            cases += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 2000);
    }

    #[test]
    fn publish_clock_groups_empty_and_exact_boundary_outputs() {
        let mut axis = PublicationCheck::new(6, 10, 3);
        axis.panic_at = Some("close");
        let mut empty: [HtfBar; 0] = [];
        publish_clock_groups(&axis, &mut empty, &[]);
        assert!(axis.reads.borrow().is_empty());
        let axis = PublicationCheck::new(6, 10, 3);
        let mut bars = [
            publication_test_bar(0, 91),
            publication_test_bar(1, 92),
            publication_test_bar(2, 93),
        ];
        publish_clock_groups(&axis, &mut bars, &[6, 10, 11]);
        assert_eq!(
            bars.map(|bar| bar.publication_chart_bar),
            [0, 2, usize::MAX]
        );
        assert_eq!(
            *axis.reads.borrow(),
            [
                ("close", 0),
                ("close", 1),
                ("len", 0),
                ("open", 2),
                ("close", 2),
                ("len", 0)
            ]
        );
    }

    #[test]
    fn publish_clock_groups_middle_failure_keeps_prefix_and_panics_in_order() {
        use std::panic::{catch_unwind, AssertUnwindSafe};
        for panic_at in [None, Some("len"), Some("open")] {
            let mut actual_axis = PublicationCheck::new(6, 10, 3);
            let mut reference_axis = PublicationCheck::new(6, 10, 3);
            actual_axis.panic_at = panic_at;
            reference_axis.panic_at = panic_at;
            let middle = if panic_at.is_none() { 3 } else { 1 };
            let mut actual = [
                publication_test_bar(0, 91),
                publication_test_bar(middle, 92),
                publication_test_bar(2, 93),
            ];
            let mut reference = actual.clone();
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                publish_clock_groups(&actual_axis, &mut actual, &[5, 7, 9]);
            }));
            let expected = catch_unwind(AssertUnwindSafe(|| {
                publish_reference(&reference_axis, &mut reference, &[5, 7, 9]);
            }));
            assert!(outcome.is_err() && expected.is_err());
            let panic_text = |p: Box<dyn std::any::Any + Send>| {
                p.downcast_ref::<String>()
                    .cloned()
                    .or_else(|| p.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            };
            assert_eq!(
                panic_text(outcome.unwrap_err()),
                panic_text(expected.unwrap_err())
            );
            assert_eq!(
                actual.iter().map(publication_bar_bits).collect::<Vec<_>>(),
                reference
                    .iter()
                    .map(publication_bar_bits)
                    .collect::<Vec<_>>()
            );
            assert_eq!(actual_axis.reads, reference_axis.reads);
            assert_eq!(actual[0].publication_chart_bar, 0);
            assert_eq!(actual[1].publication_chart_bar, 92);
            assert_eq!(actual[2].publication_chart_bar, 93);
            assert!(!actual_axis.reads.borrow().contains(&("close", 2)));
        }
    }

    #[test]
    fn publish_clock_groups_preserves_every_float_bit_and_other_field() {
        let bits = [
            0,
            0x8000_0000_0000_0000,
            1,
            0x7ff0_0000_0000_0000,
            0xfff0_0000_0000_0000,
            0x7ff8_1234_5678_9abc,
            0x7ff0_0000_0000_0001,
        ];
        let axis = PublicationCheck::new(i64::MAX, i64::MIN, 0);
        for (index, &word) in bits.iter().enumerate() {
            let mut bar = publication_test_bar(index, 91);
            bar.open = f64::from_bits(word);
            bar.high = f64::from_bits(bits[(index + 1) % bits.len()]);
            bar.low = f64::from_bits(bits[(index + 2) % bits.len()]);
            bar.close = f64::from_bits(bits[(index + 3) % bits.len()]);
            bar.volume = f64::from_bits(bits[(index + 4) % bits.len()]);
            bar.time = if index % 2 == 0 { i64::MIN } else { i64::MAX };
            let before = publication_bar_bits(&bar);
            publish_clock_groups(&axis, std::slice::from_mut(&mut bar), &[i64::MAX; 7]);
            let after = publication_bar_bits(&bar);
            assert_eq!((after.0, after.1, after.2), (before.0, before.1, before.2));
            assert_eq!(after.3, index);
        }
    }

    #[test]
    fn publish_clock_groups_accepts_noncopy_nonclone_payload_without_moving_it() {
        struct Payload(Vec<u8>);
        let payload = |i| Payload(vec![i; usize::from(i) + 1]);
        let mut bar = HtfBar {
            open: payload(1),
            high: payload(2),
            low: payload(3),
            close: payload(4),
            volume: payload(5),
            time: i64::MIN,
            last_chart_bar: 0,
            publication_chart_bar: 91,
        };
        let before = [&bar.open, &bar.high, &bar.low, &bar.close, &bar.volume]
            .map(|p| (p.0.as_ptr(), p.0.len(), p.0[0]));
        let axis = PublicationCheck::new(7, 0, 1);
        publish_clock_groups(&axis, std::slice::from_mut(&mut bar), &[7]);
        let after = [&bar.open, &bar.high, &bar.low, &bar.close, &bar.volume]
            .map(|p| (p.0.as_ptr(), p.0.len(), p.0[0]));
        assert_eq!(before, after);
        assert_eq!(bar.time, i64::MIN);
        assert_eq!(bar.last_chart_bar, 0);
        assert_eq!(bar.publication_chart_bar, 0);
    }

    /// Build a feed with an explicit daily time axis and given OHLCV columns.
    fn daily_feed(rows: &[(f64, f64, f64, f64, f64)]) -> MarketData {
        const DAY: i64 = 86_400_000;
        const T0: i64 = 18_262 * DAY; // 2020-01-01
                                      // Default timeframe is already "1D"; keep it explicit for the reader.
        let mut md = MarketData {
            timeframe: "1D".to_string(),
            ..MarketData::default()
        };
        for (i, &(o, h, l, c, v)) in rows.iter().enumerate() {
            md.open.push(o);
            md.high.push(h);
            md.low.push(l);
            md.close.push(c);
            md.volume.push(v);
            md.time.push(T0 + i as i64 * DAY);
        }
        md
    }

    #[test]
    fn five_daily_bars_aggregate_into_one_weekly_bucket() {
        // Five contiguous daily bars span < 7 days, so they form one weekly
        // bucket. Hand-computed aggregation:
        //   open  = first open           = 100
        //   high  = max of highs         = 118
        //   low   = min of lows          = 96
        //   close = last close           = 114
        //   volume= sum of volumes       = 10+20+30+40+50 = 150
        let md = daily_feed(&[
            (100.0, 105.0, 99.0, 103.0, 10.0),
            (103.0, 110.0, 101.0, 108.0, 20.0),
            (108.0, 112.0, 96.0, 100.0, 30.0),
            (100.0, 118.0, 98.0, 116.0, 40.0),
            (116.0, 117.0, 110.0, 114.0, 50.0),
        ]);
        let week = 7 * 86_400;
        let r = resample(&md, week);
        assert_eq!(r.bars.len(), 1, "all five bars in one weekly bucket");
        let b = &r.bars[0];
        assert_eq!(b.open, 100.0);
        assert_eq!(b.high, 118.0);
        assert_eq!(b.low, 96.0);
        assert_eq!(b.close, 114.0);
        assert_eq!(b.volume, 150.0);
        assert_eq!(b.last_chart_bar, 4);
        assert_eq!(r.bucket_of, vec![0, 0, 0, 0, 0]);
    }

    #[test]
    fn nine_daily_bars_split_into_two_weekly_buckets() {
        // Days 0..6 form bucket 0 (boundary at day 7); days 7..8 bucket 1.
        let rows: Vec<(f64, f64, f64, f64, f64)> = (0..9)
            .map(|i| {
                let b = 100.0 + i as f64;
                (b, b + 1.0, b - 1.0, b + 0.5, 1.0)
            })
            .collect();
        let md = daily_feed(&rows);
        let r = resample(&md, 7 * 86_400);
        assert_eq!(r.bars.len(), 2);
        assert_eq!(r.bucket_of, vec![0, 0, 0, 0, 0, 0, 0, 1, 1]);
        assert_eq!(r.bars[0].last_chart_bar, 6);
        assert_eq!(r.bars[1].last_chart_bar, 8);
        // bucket 0 volume = 7 bars * 1.0; bucket 1 = 2 bars.
        assert_eq!(r.bars[0].volume, 7.0);
        assert_eq!(r.bars[1].volume, 2.0);
        // bucket 1 open = day-7 open = 107.
        assert_eq!(r.bars[1].open, 107.0);
    }

    #[test]
    fn calendar_weeks_align_partial_weeks_without_using_the_first_bar() {
        let mut md = daily_feed(&[(10.0, 10.0, 10.0, 10.0, 1.0); 16]);
        md.close = (100..116).map(f64::from).collect();
        let week = resample_calendar_weeks(&md, 1);
        assert_eq!(
            week.bucket_of,
            [vec![0; 5], vec![1; 7], vec![2; 4]].concat()
        );
        assert_eq!(
            week.bars.iter().map(|b| b.close).collect::<Vec<_>>(),
            vec![104.0, 111.0, 115.0]
        );
        assert_eq!(
            resample(&md, 7 * 86400).bucket_of,
            [vec![0; 7], vec![1; 7], vec![2; 2]].concat()
        );
        md.time.drain(..2);
        md.open.drain(..2);
        md.high.drain(..2);
        md.low.drain(..2);
        md.close.drain(..2);
        md.volume.drain(..2);
        let sliced = resample_calendar_weeks(&md, 1);
        assert_eq!(sliced.bucket_of, week.bucket_of[2..]);
        assert_eq!(
            sliced.bars.iter().map(|b| b.close).collect::<Vec<_>>(),
            vec![104.0, 111.0, 115.0]
        );
    }

    #[test]
    fn multiweek_calendar_restarts_by_monday_year_and_uses_host_date_identity() {
        for timezone in ["UTC", "UTC-5", "UTC+05:30", "America/New_York"] {
            let mut md = daily_feed(&[(10.0, 10.0, 10.0, 10.0, 1.0); 5]);
            md.syminfo.timezone = timezone.into();
            md.trading_day = [
                (2025, 12, 28),
                (2025, 12, 29),
                (2026, 1, 2),
                (2026, 1, 5),
                (2026, 1, 12),
            ]
            .map(|(y, m, d)| days_from_civil(y, m, d).unwrap() * 86400000)
            .to_vec();
            assert_eq!(
                resample_calendar_weeks(&md, 1).bucket_of,
                vec![0, 1, 1, 2, 3],
                "{timezone}"
            );
            assert_eq!(
                resample_calendar_weeks(&md, 2).bucket_of,
                vec![0, 0, 0, 1, 1],
                "{timezone}"
            );
            assert_eq!(
                resample_calendar_weeks(&md, 3).bucket_of,
                vec![0, 1, 1, 2, 2],
                "{timezone}"
            );
            assert_eq!(
                resample_calendar_weeks(&md, 4).bucket_of,
                vec![0, 0, 0, 1, 1],
                "{timezone}"
            );
        }
    }

    #[test]
    fn calendar_months_use_trading_day_and_differ_from_thirty_days() {
        let mut md = daily_feed(&[
            (10.0, 11.0, 9.0, 10.5, 1.0),
            (10.5, 12.0, 10.0, 11.5, 1.0),
            (11.5, 13.0, 11.0, 12.5, 1.0),
        ]);
        md.trading_day = vec![
            1_585_526_400_000, // 2020-03-30
            1_585_612_800_000, // 2020-03-31
            1_585_699_200_000, // 2020-04-01
        ];

        let month = resample_calendar_months(&md, 1);
        assert_eq!(month.bucket_of, vec![0, 0, 1]);
        assert_eq!(month.bars.len(), 2);
        assert_eq!(month.bars[0].close, 11.5);
        assert_eq!(month.bars[1].close, 12.5);

        let thirty_days = resample(&md, 30 * 86_400);
        assert_eq!(thirty_days.bucket_of, vec![0, 0, 0]);
        assert_eq!(thirty_days.bars.len(), 1);
    }

    /// A host `trading_day` is a calendar-date identity already encoded at
    /// 00:00 UTC, so the exchange offset must not be applied to it a second
    /// time. Applying it would push every first-of-period bar of a
    /// negative-offset symbol back into the previous bucket, which would
    /// silently move that bucket's close, high, low, volume, and `last_chart_bar`.
    #[test]
    fn calendar_months_read_the_host_trading_day_without_a_timezone_shift() {
        for tz in ["UTC", "UTC-5", "UTC-11", "UTC+05:30", "America/New_York"] {
            let mut md = daily_feed(&[
                (10.0, 10.0, 10.0, 10.0, 1.0),
                (11.0, 11.0, 11.0, 11.0, 1.0),
                (12.0, 12.0, 12.0, 12.0, 1.0),
                (13.0, 13.0, 13.0, 13.0, 1.0),
            ]);
            md.syminfo.timezone = tz.to_string();
            md.trading_day = vec![
                1_604_016_000_000, // 2020-10-30
                1_604_102_400_000, // 2020-10-31
                1_604_188_800_000, // 2020-11-01
                1_604_275_200_000, // 2020-11-02
            ];

            let month = resample_calendar_months(&md, 1);
            assert_eq!(month.bucket_of, vec![0, 0, 1, 1], "tz {tz}");
            assert_eq!(month.bars.len(), 2, "tz {tz}");
            assert_eq!(month.bars[0].close, 11.0, "tz {tz}");
            assert_eq!(month.bars[1].close, 13.0, "tz {tz}");
            assert_eq!(month.bars[0].last_chart_bar, 1, "tz {tz}");
            assert_eq!(month.bars[1].last_chart_bar, 3, "tz {tz}");

            // The same shift corrupted every multi-month width, including the
            // year boundary that `12M` walks.
            let mut year =
                daily_feed(&[(10.0, 10.0, 10.0, 10.0, 1.0), (11.0, 11.0, 11.0, 11.0, 1.0)]);
            year.syminfo.timezone = tz.to_string();
            year.trading_day = vec![
                1_609_372_800_000, // 2020-12-31
                1_609_459_200_000, // 2021-01-01
            ];
            assert_eq!(
                resample_calendar_months(&year, 12).bucket_of,
                vec![0, 1],
                "tz {tz}"
            );
        }
    }

    /// The no-lane fallback reads the nominal chart-bar close, which IS a real
    /// instant, so it keeps the exchange offset. This pins the asymmetry: a
    /// repair that simply dropped `timezone_offset` from both branches would
    /// turn this green test red.
    #[test]
    fn calendar_months_still_localize_the_nominal_close_fallback() {
        // Four daily bars opening 02:00 UTC on 2020-10-30 through 11-02, so
        // each bar's last millisecond lands on the following calendar date in
        // UTC but on the same date five hours west of it.
        let opens: Vec<i64> = (0..4)
            .map(|i| 1_604_016_000_000 + 7_200_000 + i * 86_400_000)
            .collect();
        let rows = [
            (10.0, 10.0, 10.0, 10.0, 1.0),
            (11.0, 11.0, 11.0, 11.0, 1.0),
            (12.0, 12.0, 12.0, 12.0, 1.0),
            (13.0, 13.0, 13.0, 13.0, 1.0),
        ];

        let mut utc = daily_feed(&rows);
        utc.time = opens.clone();
        assert!(utc.trading_day.is_empty());
        let month = resample_calendar_months(&utc, 1);
        assert_eq!(month.bucket_of, vec![0, 1, 1, 1]);
        assert_eq!(month.bars[0].close, 10.0);
        assert_eq!(month.bars[1].close, 13.0);

        let mut west = daily_feed(&rows);
        west.time = opens;
        west.syminfo.timezone = "UTC-5".to_string();
        let month = resample_calendar_months(&west, 1);
        assert_eq!(month.bucket_of, vec![0, 0, 1, 1]);
        assert_eq!(month.bars[0].close, 11.0);
        assert_eq!(month.bars[1].close, 13.0);
    }

    #[test]
    fn lookahead_off_uses_last_supplied_bar_as_bucket_close() {
        let rows: Vec<(f64, f64, f64, f64, f64)> = (0..9)
            .map(|i| (0.0, 0.0, 0.0, 100.0 + i as f64, 1.0))
            .collect();
        let md = daily_feed(&rows);
        let r = resample(&md, 7 * 86_400);
        // Bars 0..5 precede the last supplied bar in bucket 0 → None.
        for bar in 0..6 {
            assert_eq!(r.visible_bucket(bar, Lookahead::Off), None, "bar {bar}");
        }
        // Bar 6 is the last currently supplied bar in bucket 0 → sees bucket 0.
        assert_eq!(r.visible_bucket(6, Lookahead::Off), Some(0));
        // Bar 7 sees the prior bucket. Bar 8 is the last currently supplied
        // bar in bucket 1, so it sees bucket 1 (whether or not that realtime
        // bucket is calendar-complete is not represented by this feed).
        assert_eq!(r.visible_bucket(7, Lookahead::Off), Some(0));
        assert_eq!(r.visible_bucket(8, Lookahead::Off), Some(1));
    }

    #[test]
    fn lookahead_off_revises_tail_value_when_feed_grows_within_bucket() {
        let rows: Vec<(f64, f64, f64, f64, f64)> = (0..9)
            .map(|i| (0.0, 0.0, 0.0, 100.0 + i as f64, 1.0))
            .collect();

        // With only bars 0..7 available, bar 7 is treated as the last bar of
        // weekly bucket 1 and reads its current close, 107.
        let initial = daily_feed(&rows[..8]);
        let initial_resampled = resample(&initial, 7 * 86_400);
        assert_eq!(initial_resampled.visible_bucket(7, Lookahead::Off), Some(1));
        assert_eq!(
            initial_resampled
                .visible_bucket(7, Lookahead::Off)
                .map(|bucket| initial_resampled.bars[bucket].close),
            Some(107.0)
        );

        // Appending another daily bar in the same weekly bucket moves its
        // last supplied index to 8. Re-reading bar 7 now selects bucket 0's
        // close, 106. This is a feed-state change, not a claim that the VM
        // re-executes a previously committed bar.
        let extended = daily_feed(&rows);
        let extended_resampled = resample(&extended, 7 * 86_400);
        assert_eq!(
            extended_resampled.visible_bucket(7, Lookahead::Off),
            Some(0)
        );
        assert_eq!(
            extended_resampled
                .visible_bucket(7, Lookahead::Off)
                .map(|bucket| extended_resampled.bars[bucket].close),
            Some(106.0)
        );
    }

    #[test]
    fn lookahead_on_sees_forming_bucket_and_repaints() {
        let rows: Vec<(f64, f64, f64, f64, f64)> = (0..9)
            .map(|i| (0.0, 0.0, 0.0, 100.0 + i as f64, 1.0))
            .collect();
        let md = daily_feed(&rows);
        let r = resample(&md, 7 * 86_400);
        // Every chart bar in bucket 0 sees bucket 0 immediately (repaint).
        for bar in 0..7 {
            assert_eq!(r.visible_bucket(bar, Lookahead::On), Some(0), "bar {bar}");
        }
        for bar in 7..9 {
            assert_eq!(r.visible_bucket(bar, Lookahead::On), Some(1), "bar {bar}");
        }
    }

    #[test]
    fn gaps_on_emits_only_on_bucket_close() {
        let rows: Vec<(f64, f64, f64, f64, f64)> =
            (0..9).map(|_| (0.0, 0.0, 0.0, 1.0, 1.0)).collect();
        let md = daily_feed(&rows);
        let r = resample(&md, 7 * 86_400);
        let closes: Vec<bool> = (0..9).map(|b| r.is_bucket_close(b)).collect();
        // Only bar 6 (closes bucket 0) and bar 8 (closes bucket 1) emit.
        assert_eq!(
            closes,
            vec![false, false, false, false, false, false, true, false, true]
        );
    }

    #[test]
    fn security_close_gaps_off_repeats_last_htf_close() {
        // A weekly close requested with gaps off and lookahead off: closes of
        // bucket 0 = 106 (day-6 close), bucket 1 = 108 (day-8 close).
        // Gaps off and lookahead off ⇒ intermediate bars show the previous
        // bucket, while the last
        // currently supplied bar of each bucket shows that bucket's close.
        // The feed does not distinguish a complete bucket from a realtime tail.
        let rows: Vec<(f64, f64, f64, f64, f64)> = (0..9)
            .map(|i| (0.0, 0.0, 0.0, 100.0 + i as f64, 1.0))
            .collect();
        let md = daily_feed(&rows);
        let r = resample(&md, 7 * 86_400);
        let series = r.map_to_chart_bars(Gaps::Off, Lookahead::Off, |b| Some(r.bars[b].close));
        // Bars 0..5: None. Bar 6 is the current tail of bucket 0 ⇒ 106.
        // Bar 7 uses bucket 0; bar 8 is the current tail of bucket 1 ⇒ 108.
        assert_eq!(
            series,
            vec![
                None,
                None,
                None,
                None,
                None,
                None,
                Some(106.0),
                Some(106.0),
                Some(108.0),
            ]
        );
    }

    #[test]
    fn security_close_gaps_on_yields_na_between_htf_closes() {
        // Same feed, gaps_on: a value appears only on the last currently
        // supplied chart bar of each bucket; every other bar is na (None).
        let rows: Vec<(f64, f64, f64, f64, f64)> = (0..9)
            .map(|i| (0.0, 0.0, 0.0, 100.0 + i as f64, 1.0))
            .collect();
        let md = daily_feed(&rows);
        let r = resample(&md, 7 * 86_400);
        let series = r.map_to_chart_bars(Gaps::On, Lookahead::Off, |b| Some(r.bars[b].close));
        assert_eq!(
            series,
            vec![
                None,
                None,
                None,
                None,
                None,
                None,
                Some(106.0),
                None,
                Some(108.0),
            ]
        );
    }

    #[test]
    fn security_close_lookahead_on_repaints_forming_bucket() {
        // lookahead_on leaks the forming bucket's close onto its earlier chart
        // bars (repaint): bars 0..6 all show bucket 0's close 106, bars 7,8
        // show bucket 1's close 108 — the future value on bar 7.
        let rows: Vec<(f64, f64, f64, f64, f64)> = (0..9)
            .map(|i| (0.0, 0.0, 0.0, 100.0 + i as f64, 1.0))
            .collect();
        let md = daily_feed(&rows);
        let r = resample(&md, 7 * 86_400);
        let series = r.map_to_chart_bars(Gaps::Off, Lookahead::On, |b| Some(r.bars[b].close));
        assert_eq!(
            series,
            vec![
                Some(106.0),
                Some(106.0),
                Some(106.0),
                Some(106.0),
                Some(106.0),
                Some(106.0),
                Some(106.0),
                Some(108.0),
                Some(108.0),
            ]
        );
    }

    #[test]
    fn ta_over_htf_bars_keeps_its_own_per_context_state() {
        // A length-2 SMA requested on the weekly timeframe runs over
        // the RESAMPLED (weekly) closes — its own per-context state — NOT the
        // chart-bar closes. With three weekly buckets whose closes are
        // 106, 113, 118, a length-2 SMA over the HTF series is:
        //   bucket 0: na (only one value)      -> the SMA over one value is that value
        //   bucket 1: (106+113)/2 = 109.5
        //   bucket 2: (113+118)/2 = 115.5
        // A chart-bar SMA(close,2) would instead track adjacent DAILY closes
        // (…, 116.5, 117.5), so the values differ — proving the state is the
        // HTF context's, not the chart's.
        let rows: Vec<(f64, f64, f64, f64, f64)> = (0..19)
            .map(|i| (0.0, 0.0, 0.0, 100.0 + i as f64, 1.0))
            .collect();
        let md = daily_feed(&rows);
        let r = resample(&md, 7 * 86_400);
        // Weekly buckets: days 0..6, 7..13, 14..18 -> closes 106, 113, 118.
        assert_eq!(r.bars.len(), 3);
        assert_eq!(
            r.bars.iter().map(|b| b.close).collect::<Vec<_>>(),
            vec![106.0, 113.0, 118.0]
        );
        // A length-2 SMA carried across HTF bars (per-HTF-context state).
        let sma2: Vec<f64> = r
            .bars
            .iter()
            .enumerate()
            .map(|(i, b)| {
                if i == 0 {
                    b.close
                } else {
                    f64::midpoint(r.bars[i - 1].close, b.close)
                }
            })
            .collect();
        assert_eq!(sma2, vec![106.0, 109.5, 115.5]);
        let series = r.map_to_chart_bars(Gaps::Off, Lookahead::On, |b| Some(sma2[b]));
        // The last chart bar (in bucket 2) shows the HTF SMA 115.5 — distinct
        // from any adjacent-daily-close average.
        assert_eq!(series[18], Some(115.5));
    }

    #[test]
    fn requesting_a_lower_timeframe_is_rejected() {
        // Chart timeframe is daily; a request for a 60-minute (lower) HTF must
        // be rejected — the feed cannot be refined below its own resolution.
        let md = daily_feed(&[(1.0, 1.0, 1.0, 1.0, 1.0)]);
        let err = validate_security_timeframe(&md, "60").unwrap_err();
        assert_eq!(
            err,
            SecurityTimeframeError::LowerThanChart {
                chart_seconds: 86_400,
                requested_seconds: 3_600,
            }
        );
        // Equal (daily) is allowed; higher (weekly) is allowed.
        assert_eq!(validate_security_timeframe(&md, "1D"), Ok(86_400));
        assert_eq!(validate_security_timeframe(&md, "1W"), Ok(7 * 86_400));
    }

    #[test]
    fn empty_feed_resamples_to_nothing() {
        let md = MarketData::default();
        let r = resample(&md, 7 * 86_400);
        assert!(r.bars.is_empty());
        assert!(r.bucket_of.is_empty());
    }

    #[test]
    fn absurd_htf_seconds_saturates_without_panic_or_hang() {
        // Regression: a crafted `htf_seconds` near i64::MAX
        // (reachable via a hand-authored ReadSecurity/ReadTimeTf/ReadPivotLevels
        // opcode field) must not overflow the `* 1000` step (panic in
        // overflow-checked debug/test builds) or wrap negative and spin the
        // boundary loop (hang in release). It saturates and collapses all bars
        // into one bucket — deterministic, terminating, no panic.
        let rows: Vec<(f64, f64, f64, f64, f64)> = (0..5)
            .map(|i| {
                let b = 10.0 + i as f64;
                (b, b + 1.0, b - 1.0, b, 1.0)
            })
            .collect();
        let md = daily_feed(&rows);
        let r = resample(&md, i64::MAX);
        assert_eq!(
            r.bars.len(),
            1,
            "all bars collapse into one saturated bucket"
        );
        assert_eq!(r.bucket_of, vec![0, 0, 0, 0, 0]);
    }

    #[test]
    fn equal_timeframe_is_identity() {
        // Resampling to the SAME (daily) timeframe leaves one bucket per bar.
        let rows: Vec<(f64, f64, f64, f64, f64)> = (0..4)
            .map(|i| {
                let b = 10.0 + i as f64;
                (b, b + 2.0, b - 2.0, b + 1.0, 5.0)
            })
            .collect();
        let md = daily_feed(&rows);
        let r = resample(&md, 86_400);
        assert_eq!(r.bars.len(), 4);
        assert_eq!(r.bucket_of, vec![0, 1, 2, 3]);
        for (i, b) in r.bars.iter().enumerate() {
            assert_eq!(b.open, 10.0 + i as f64);
            assert_eq!(b.close, 11.0 + i as f64);
            assert_eq!(b.volume, 5.0);
            assert_eq!(b.last_chart_bar, i);
        }
    }
}

/// Test contract for raw native OHLCV operations. Rust permits arithmetic
/// NaN sign/payload variation; copying a value does not grant that latitude.
/// See Rust 1.97.1 `primitive_docs.rs`, "NaN bit patterns" (lines 1244-1302).
#[cfg(test)]
mod ohlcv_test_contract {
    use super::HtfBar;

    type Observation = (Result<(), String>, ([u64; 5], i64, usize, usize));

    fn same_field(actual: u64, expected: u64, arithmetic_ran: bool) -> bool {
        actual == expected
            || (arithmetic_ran
                && f64::from_bits(actual).is_nan()
                && f64::from_bits(expected).is_nan())
    }

    pub(super) fn same_fields(actual: &[u64; 5], expected: &[u64; 5], ran: [bool; 3]) -> bool {
        actual[0] == expected[0]
            && same_field(actual[1], expected[1], ran[0])
            && same_field(actual[2], expected[2], ran[1])
            && actual[3] == expected[3]
            && same_field(actual[4], expected[4], ran[2])
    }

    pub(super) fn same_observation(a: &Observation, b: &Observation, ran: [bool; 3]) -> bool {
        a.0 == b.0
            && same_fields(&a.1 .0, &b.1 .0, ran)
            && (a.1 .1, a.1 .2, a.1 .3) == (b.1 .1, b.1 .2, b.1 .3)
    }

    pub(super) fn same_bar(a: &HtfBar, b: &HtfBar, extended: bool) -> bool {
        same_fields(
            &[
                a.open.to_bits(),
                a.high.to_bits(),
                a.low.to_bits(),
                a.close.to_bits(),
                a.volume.to_bits(),
            ],
            &[
                b.open.to_bits(),
                b.high.to_bits(),
                b.low.to_bits(),
                b.close.to_bits(),
                b.volume.to_bits(),
            ],
            [extended; 3],
        ) && (a.time, a.last_chart_bar, a.publication_chart_bar)
            == (b.time, b.last_chart_bar, b.publication_chart_bar)
    }

    /// Derive which buckets actually reached extension from the real index map.
    pub(super) fn extended_buckets(bucket_of: &[usize], count: usize) -> Option<Vec<bool>> {
        let mut seen = vec![false; count];
        let mut extended = vec![false; count];
        for &bucket in bucket_of {
            let prior = seen.get_mut(bucket)?;
            extended[bucket] |= *prior;
            *prior = true;
        }
        Some(extended)
    }

    #[test]
    fn oracle_preserves_every_non_nan_bit_and_copied_nan() {
        let nan = 0x7ff8_0000_0000_1234;
        let other_nan = 0xfff8_0000_0000_5678;
        let baseline = [nan, nan, nan, nan, nan];
        for field in 0..5 {
            let mut changed = baseline;
            changed[field] = other_nan;
            assert!(!same_fields(&changed, &baseline, [false; 3]));
            assert_eq!(
                same_fields(&changed, &baseline, [true; 3]),
                matches!(field, 1 | 2 | 4)
            );
        }
        for (left, right) in [
            (0.0_f64, -0.0_f64),
            (1.0, 2.0),
            (f64::INFINITY, f64::NEG_INFINITY),
            (f64::INFINITY, f64::MAX),
            (f64::from_bits(nan), 1.0),
            (1.0, f64::from_bits(nan)),
        ] {
            for field in 0..5 {
                let a = [left.to_bits(); 5];
                let mut b = a;
                b[field] = right.to_bits();
                assert!(!same_fields(&a, &b, [true; 3]));
            }
        }
    }

    #[test]
    fn oracle_keeps_outcome_metadata_and_reached_operation_distinctions() {
        let nan = 0x7ff8_0000_0000_1234;
        let other_nan = 0xfff8_0000_0000_5678;
        let base = (Ok(()), ([nan; 5], 1, 2, 3));
        for reached in 0..3 {
            let mut changed = base.clone();
            changed.1 .0[[1, 2, 4][reached]] = other_nan;
            assert!(!same_observation(&base, &changed, [false; 3]));
            let mut ran = [false; 3];
            ran[reached] = true;
            assert!(same_observation(&base, &changed, ran));
        }
        let mut changed = base.clone();
        changed.0 = Err("different result".into());
        assert!(!same_observation(&base, &changed, [true; 3]));
        let mut changed = base.clone();
        changed.1 .1 += 1;
        assert!(!same_observation(&base, &changed, [true; 3]));
        changed = base.clone();
        changed.1 .2 += 1;
        assert!(!same_observation(&base, &changed, [true; 3]));
        changed = base.clone();
        changed.1 .3 += 1;
        assert!(!same_observation(&base, &changed, [true; 3]));
        let left = (Err("first panic".into()), base.1);
        let right = (Err("later panic".into()), base.1);
        assert!(!same_observation(&left, &right, [true; 3]));
    }

    #[test]
    fn actual_plain_security_read_collapses_nonfinite_without_changing_zero_sign() {
        for bits in [
            0x7ff8_0000_0000_1234,
            0xfff8_0000_0000_5678,
            0x7ff0_0000_0000_4321,
            0xfff0_0000_0000_9876,
        ] {
            let value = f64::from_bits(bits);
            let bar = HtfBar {
                open: 1.0,
                high: 2.0,
                low: 0.0,
                close: 1.0,
                volume: value,
                time: 1,
                last_chart_bar: 2,
                publication_chart_bar: 3,
            };
            assert_eq!(
                crate::Vm::htf_source_value(&bar, crate::BuiltinSeries::Volume),
                None
            );
            assert_eq!(crate::Value::from_f64(value), crate::Value::Na);
        }
        for value in [0.0_f64, -0.0_f64] {
            let bar = HtfBar {
                open: 1.0,
                high: 2.0,
                low: 0.0,
                close: 1.0,
                volume: value,
                time: 1,
                last_chart_bar: 2,
                publication_chart_bar: 3,
            };
            assert_eq!(
                crate::Vm::htf_source_value(&bar, crate::BuiltinSeries::Volume)
                    .expect("finite value")
                    .to_bits(),
                value.to_bits()
            );
        }
    }

    #[test]
    fn oracle_keeps_singleton_payloads_and_all_metadata_exact() {
        let nan = f64::from_bits(0x7ff8_0000_0000_1234);
        let other_nan = f64::from_bits(0xfff8_0000_0000_5678);
        let base = HtfBar {
            open: nan,
            high: nan,
            low: nan,
            close: nan,
            volume: nan,
            time: 1,
            last_chart_bar: 2,
            publication_chart_bar: 3,
        };
        let mut changed = base.clone();
        changed.volume = other_nan;
        assert!(!same_bar(&base, &changed, false));
        assert!(same_bar(&base, &changed, true));
        for copied in 0..2 {
            changed = base.clone();
            if copied == 0 {
                changed.open = other_nan;
            } else {
                changed.close = other_nan;
            }
            assert!(!same_bar(&base, &changed, true));
        }
        changed = base.clone();
        changed.publication_chart_bar += 1;
        assert!(!same_bar(&base, &changed, true));
        assert_eq!(
            extended_buckets(&[0, 1, 1, 2], 3),
            Some(vec![false, true, false])
        );
        assert_eq!(extended_buckets(&[0, 3], 3), None);
    }
}

/// Differential tests of the Aeneas-extractable bucket kernels against
/// copies of the same functions.
#[cfg(test)]
mod kernel_reference_equivalence {
    use super::*;

    /// Copy of `resample_by_bucket`, kept verbatim for comparison.
    fn reference_resample_by_bucket<F>(data: &MarketData, mut bucket_id: F) -> Resampled
    where
        F: FnMut(usize) -> i64,
    {
        let n = data.bars();
        let mut bars: Vec<HtfBar> = Vec::new();
        let mut bucket_of: Vec<usize> = Vec::with_capacity(n);
        let mut active_bucket = None;

        for i in 0..n {
            let id = bucket_id(i);
            let need_new = active_bucket != Some(id);
            if need_new {
                bars.push(HtfBar {
                    open: data.open[i],
                    high: data.high[i],
                    low: data.low[i],
                    close: data.close[i],
                    volume: data.volume[i],
                    time: data.time_at(i),
                    last_chart_bar: i,
                    publication_chart_bar: i,
                });
                active_bucket = Some(id);
            } else {
                let cur = bars.last_mut().expect("active bucket has an HTF bar");
                cur.high = cur.high.max(data.high[i]);
                cur.low = cur.low.min(data.low[i]);
                cur.close = data.close[i];
                cur.volume += data.volume[i];
                cur.last_chart_bar = i;
                cur.publication_chart_bar = i;
            }
            bucket_of.push(bars.len() - 1);
        }

        Resampled { bars, bucket_of }
    }

    /// Copy of `Resampled::visible_bucket`.
    fn reference_visible_bucket(
        r: &Resampled,
        chart_bar: usize,
        lookahead: Lookahead,
    ) -> Option<usize> {
        let own = *r.bucket_of.get(chart_bar)?;
        match lookahead {
            Lookahead::On => Some(own),
            Lookahead::Off => {
                let bar = &r.bars[own];
                if chart_bar >= bar.last_chart_bar {
                    Some(own)
                } else if own == 0 {
                    None
                } else {
                    Some(own - 1)
                }
            }
        }
    }

    /// Copy of `Resampled::is_bucket_close`.
    fn reference_is_bucket_close(r: &Resampled, chart_bar: usize) -> bool {
        r.bucket_of
            .get(chart_bar)
            .is_some_and(|&b| r.bars[b].last_chart_bar == chart_bar)
    }

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 11
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// A feed whose prices include NaN, signed zeros and infinities so that
    /// the `f64::max`/`min`/`+=` fold order is exercised, not just the index.
    fn feed(rng: &mut Lcg, n: usize) -> MarketData {
        const SPECIAL: [f64; 7] = [
            f64::NAN,
            0.0,
            -0.0,
            f64::INFINITY,
            f64::NEG_INFINITY,
            1e308,
            -1e308,
        ];
        let value = |rng: &mut Lcg| {
            if rng.below(8) == 0 {
                SPECIAL[usize::try_from(rng.below(7)).expect("small")]
            } else {
                f64::from(u32::try_from(rng.below(1_000_000)).expect("small")) / 7.0 - 50_000.0
            }
        };
        let mut md = MarketData::default();
        for i in 0..n {
            md.open.push(value(rng));
            md.high.push(value(rng));
            md.low.push(value(rng));
            md.close.push(value(rng));
            md.volume.push(value(rng));
            md.time
                .push(1_600_000_000_000 + i64::try_from(i).expect("small") * 60_000);
        }
        md
    }

    /// Non-NaN bits and copied fields remain exact; only reached arithmetic
    /// NaNs have Rust's unspecified sign/payload reading.
    fn same(a: &Resampled, b: &Resampled) -> bool {
        if a.bucket_of != b.bucket_of || a.bars.len() != b.bars.len() {
            return false;
        }
        let Some(extended) = ohlcv_test_contract::extended_buckets(&a.bucket_of, a.bars.len())
        else {
            return false;
        };
        a.bars
            .iter()
            .zip(&b.bars)
            .zip(extended)
            .all(|((x, y), ran)| ohlcv_test_contract::same_bar(x, y, ran))
    }

    fn check(data: &MarketData, ids: &[i64]) {
        let new = resample_by_bucket(data, |i| ids[i]);
        let old = reference_resample_by_bucket(data, |i| ids[i]);
        assert!(
            same(&new, &old),
            "ids {ids:?}; actual {new:?}; copy {old:?}"
        );
        let idx = bucket_index(ids);
        assert_eq!(idx.bucket_of, new.bucket_of);
        let lasts: Vec<usize> = new.bars.iter().map(|b| b.last_chart_bar).collect();
        assert_eq!(idx.last_chart_bar, lasts);
        for chart_bar in 0..ids.len() + 3 {
            for lookahead in [Lookahead::On, Lookahead::Off] {
                assert_eq!(
                    new.visible_bucket(chart_bar, lookahead),
                    reference_visible_bucket(&new, chart_bar, lookahead),
                );
            }
            assert_eq!(
                new.is_bucket_close(chart_bar),
                reference_is_bucket_close(&new, chart_bar)
            );
        }
    }

    /// `month_bucket_id` reproduces the closure arithmetic verbatim,
    /// including the saturating edges.
    #[test]
    fn month_bucket_id_matches_closure_arithmetic() {
        let mut rng = Lcg(11);
        let edges = [
            i64::MIN + 1,
            -1,
            0,
            1,
            i64::MAX / 12,
            i64::MAX / 12 + 1,
            i64::MAX,
        ];
        for _ in 0..20_000 {
            let year = if rng.below(4) == 0 {
                edges[usize::try_from(rng.below(7)).expect("small")]
            } else {
                i64::try_from(rng.below(2_000_000_000)).expect("small") - 1_000_000_000
            };
            let month = i64::try_from(rng.below(12)).expect("small") + 1;
            let months = i64::from(u32::try_from(rng.below(40)).expect("small").max(1));
            let old = year
                .saturating_mul(12)
                .saturating_add(month - 1)
                .div_euclid(months);
            assert_eq!(month_bucket_id(year, month, months), old);
        }
    }

    #[test]
    fn kernels_match_reference_on_edges() {
        let mut rng = Lcg(7);
        let empty = MarketData::default();
        check(&empty, &[]);
        let data = feed(&mut rng, 8);
        for ids in [
            vec![0; 8],
            vec![0, 1, 2, 3, 4, 5, 6, 7],
            vec![i64::MIN, i64::MIN, i64::MAX, i64::MAX, 0, 0, -1, -1],
            // Non-monotone ids: a repeated id after a change opens a new bucket.
            vec![1, 1, 2, 1, 1, 2, 2, 1],
            vec![5, 4, 3, 2, 1, 0, -1, -2],
        ] {
            check(&data, &ids);
        }
    }

    #[test]
    fn kernels_match_reference_on_sweep() {
        let mut rng = Lcg(0x5EED_B0C4);
        for round in 0..2_000 {
            let n = usize::try_from(rng.below(64)).expect("small");
            let data = feed(&mut rng, n);
            // Mix sorted (realistic) and arbitrary id sequences.
            let mut ids = Vec::with_capacity(n);
            let mut cur = i64::try_from(rng.below(10)).expect("small") - 5;
            for _ in 0..n {
                if round % 3 == 0 {
                    ids.push(i64::try_from(rng.below(4)).expect("small"));
                } else {
                    if rng.below(3) == 0 {
                        cur += i64::try_from(rng.below(3)).expect("small") + 1;
                    }
                    ids.push(cur);
                }
            }
            check(&data, &ids);
        }
    }

    /// Copy of the publication-aware `map_to_chart_bars`.
    fn reference_map_to_chart_bars<F>(
        r: &Resampled,
        gaps: Gaps,
        lookahead: Lookahead,
        mut htf_value: F,
    ) -> Vec<Option<f64>>
    where
        F: FnMut(usize) -> Option<f64>,
    {
        let n = r.bucket_of.len();
        let mut out = Vec::with_capacity(n);
        for chart_bar in 0..n {
            let visible = reference_visible_bucket(r, chart_bar, lookahead);
            let previous = chart_bar
                .checked_sub(1)
                .and_then(|bar| reference_visible_bucket(r, bar, lookahead));
            if gaps == Gaps::On && visible == previous {
                out.push(None);
                continue;
            }
            let value = visible.and_then(&mut htf_value);
            out.push(value);
        }
        out
    }

    /// The open instant of the exchange-clock bucket holding `time`, stated
    /// independently of `fixed_bucket_id` and in `i128` so nothing
    /// saturates: a width under a day walks from the bar's own UTC midnight,
    /// a longer one from the first bar's.
    fn reference_bucket_open(time: i64, first: i64, step_ms: i64) -> i128 {
        const DAY: i128 = 86_400_000;
        let (time, step) = (i128::from(time), i128::from(step_ms));
        let day_of = |t: i128| t.div_euclid(DAY) * DAY;
        let origin = if step < DAY {
            day_of(time)
        } else {
            day_of(i128::from(first))
        };
        origin + (time - origin).div_euclid(step) * step
    }

    /// Fixed-width resampling over [`reference_bucket_open`], grouped by the
    /// copied fold. Bars group when their bucket opens are equal, so
    /// the opens are renumbered to `i64` ids in bar order.
    fn reference_clock_resample(data: &MarketData, step_ms: i64) -> Resampled {
        let first = if data.bars() == 0 { 0 } else { data.time_at(0) };
        let opens: Vec<i128> = (0..data.bars())
            .map(|i| reference_bucket_open(data.time_at(i), first, step_ms))
            .collect();
        let mut ids = Vec::with_capacity(opens.len());
        for (i, open) in opens.iter().enumerate() {
            let previous = ids.last().copied().unwrap_or(0);
            ids.push(if i > 0 && opens[i - 1] == *open {
                previous
            } else {
                previous + 1
            });
        }
        reference_resample_by_bucket(data, |i| ids[i])
    }

    /// The fixed-width `resample` contract: buckets on the UTC clock.
    fn reference_resample(data: &MarketData, htf_seconds: i64) -> Resampled {
        reference_clock_resample(data, htf_seconds.max(1).saturating_mul(1000))
    }

    /// `map_to_chart_bars` through `chart_bar_bucket` equals the copied
    /// body, including the order and number of `htf_value` calls.
    #[test]
    fn map_to_chart_bars_matches_reference() {
        let mut rng = Lcg(0xB4C4);
        for round in 0..2_000 {
            let n = usize::try_from(rng.below(48)).expect("small");
            let data = feed(&mut rng, n);
            let mut ids = Vec::with_capacity(n);
            for _ in 0..n {
                ids.push(
                    i64::try_from(rng.below(if round % 2 == 0 { 3 } else { 12 })).expect("small"),
                );
            }
            ids.sort_unstable();
            let r = resample_by_bucket(&data, |i| ids[i]);
            for gaps in [Gaps::Off, Gaps::On] {
                for lookahead in [Lookahead::Off, Lookahead::On] {
                    let mut calls_new = Vec::new();
                    let mut calls_old = Vec::new();
                    let new = r.map_to_chart_bars(gaps, lookahead, |b| {
                        calls_new.push(b);
                        (b % 3 != 0).then_some(r.bars[b].close)
                    });
                    let old = reference_map_to_chart_bars(&r, gaps, lookahead, |b| {
                        calls_old.push(b);
                        (b % 3 != 0).then_some(r.bars[b].close)
                    });
                    assert_eq!(calls_new, calls_old);
                    assert_eq!(new.len(), old.len());
                    for (x, y) in new.iter().zip(&old) {
                        assert_eq!(x.map(f64::to_bits), y.map(f64::to_bits));
                    }
                }
            }
        }
    }

    /// `resample` through `resample_fixed_kernel` equals the copied body
    /// on edge intervals (nonpositive, saturating), edge and synthesized time
    /// axes, and the empty feed.
    #[test]
    fn resample_matches_reference_on_edges() {
        let mut rng = Lcg(0x71AE);
        let intervals = [
            i64::MIN,
            -86_400,
            -1,
            0,
            1,
            60,
            86_400,
            7 * 86_400,
            i64::MAX / 1000,
            i64::MAX / 1000 + 1,
            i64::MAX,
        ];
        let empty = MarketData::default();
        for &htf in &intervals {
            assert!(same(
                &resample(&empty, htf),
                &reference_resample(&empty, htf)
            ));
        }
        for round in 0..400 {
            let n = usize::try_from(rng.below(40)).expect("small") + 1;
            let mut data = feed(&mut rng, n);
            match round % 4 {
                // Synthesized daily axis.
                0 => data.time.clear(),
                // Times at the i64 edges (saturating subtraction).
                1 => {
                    for (i, t) in data.time.iter_mut().enumerate() {
                        *t = if i % 2 == 0 { i64::MIN } else { i64::MAX };
                    }
                }
                // Arbitrary (unsorted) times.
                2 => {
                    for t in &mut data.time {
                        *t = i64::try_from(rng.below(1 << 40)).expect("small") - (1 << 39);
                    }
                }
                _ => {}
            }
            for &htf in &intervals {
                let actual = resample(&data, htf);
                let expected = reference_resample(&data, htf);
                assert!(
                    same(&actual, &expected),
                    "htf {htf}; actual {actual:?}; copy {expected:?}"
                );
            }
        }
    }

    /// The admission kernel computes the same comparison as a direct ordering check.
    #[test]
    fn security_timeframe_admitted_matches_comparison() {
        let edges = [i64::MIN, -1, 0, 1, 60, 86_400, i64::MAX];
        for &chart in &edges {
            for &requested in &edges {
                assert_eq!(
                    security_timeframe_admitted(chart, requested),
                    !matches!(requested.cmp(&chart), std::cmp::Ordering::Less)
                );
            }
        }
    }

    #[test]
    fn real_resamplers_match_reference() {
        let mut rng = Lcg(0xFEED);
        let data = feed(&mut rng, 5_000);
        for htf_seconds in [60, 300, 420, 3600, 14_400, 86_400, 7 * 86_400] {
            let old = reference_clock_resample(&data, i64::from(htf_seconds) * 1000);
            let actual = resample(&data, htf_seconds.into());
            assert!(
                same(&actual, &old),
                "htf {htf_seconds}; actual {actual:?}; copy {old:?}"
            );
        }
    }

    /// Hourly, four-hour and daily buckets of a 15-minute feed open on the
    /// clock whatever bar the feed starts on, and in the exchange timezone.
    #[test]
    fn fixed_buckets_follow_the_exchange_clock_not_the_first_bar() {
        const MIDNIGHT: i64 = 1_700_006_400_000; // 2023-11-15T00:00Z
        const BAR: i64 = 15 * 60_000;
        let feed = |start: i64, timezone: &str| {
            let mut data = MarketData::default();
            for i in 0..400 {
                data.time.push(start + i * BAR);
                for lane in [
                    &mut data.open,
                    &mut data.high,
                    &mut data.low,
                    &mut data.close,
                    &mut data.volume,
                ] {
                    lane.push(1.0);
                }
            }
            data.syminfo.timezone = timezone.into();
            data
        };
        for first_bar in 0..4 {
            let data = feed(MIDNIGHT + first_bar * BAR, "UTC");
            for (seconds, width) in [
                (3600, 3_600_000),
                (14_400, 14_400_000),
                (86_400, 86_400_000),
            ] {
                let r = resample(&data, seconds);
                for bar in &r.bars[1..] {
                    assert_eq!(
                        bar.time.rem_euclid(width),
                        0,
                        "{seconds}s from bar {first_bar}"
                    );
                }
            }
        }
        // UTC-5: a day opens at 05:00 UTC.
        let r = resample(&feed(MIDNIGHT, "UTC-5"), 86_400);
        assert_eq!(r.bars[1].time, MIDNIGHT + 5 * 3_600_000);
    }
}

#[cfg(test)]
mod ohlcv_open_kernel {
    use super::*;
    use std::{any::Any, panic::AssertUnwindSafe};

    type Bits = ([u64; 5], i64, usize, usize);
    const PAYLOADS: [u64; 8] = [
        0,
        1 << 63,
        0x7ff8_0000_0000_1234,
        0x7ff0_0000_0000_4321,
        0x7ff0_0000_0000_0000,
        0xfff0_0000_0000_0000,
        0x3ff0_0000_0000_0000,
        0xc01c_0000_0000_0000,
    ];

    // Copy of the OhlcvSource::open_bucket body, with only its receiver
    // projection moved to this function argument. It does not call the helper.
    fn frozen(data: &MarketData, i: usize, last_chart_bar: usize) -> HtfBar {
        HtfBar {
            open: data.open[i],
            high: data.high[i],
            low: data.low[i],
            close: data.close[i],
            volume: data.volume[i],
            time: data.time_at(i),
            last_chart_bar,
            publication_chart_bar: last_chart_bar,
        }
    }

    fn bits(bar: HtfBar) -> Bits {
        (
            [
                bar.open.to_bits(),
                bar.high.to_bits(),
                bar.low.to_bits(),
                bar.close.to_bits(),
                bar.volume.to_bits(),
            ],
            bar.time,
            bar.last_chart_bar,
            bar.publication_chart_bar,
        )
    }

    fn panic_text(panic: &(dyn Any + Send)) -> String {
        if let Some(text) = panic.downcast_ref::<String>() {
            text.clone()
        } else if let Some(text) = panic.downcast_ref::<&str>() {
            (*text).to_owned()
        } else {
            "non-string panic".to_owned()
        }
    }

    fn observe(body: impl FnOnce() -> HtfBar) -> Result<Bits, String> {
        std::panic::catch_unwind(AssertUnwindSafe(body))
            .map(bits)
            .map_err(|panic| panic_text(&*panic))
    }

    fn feed(lengths: [usize; 6]) -> MarketData {
        let values = |length, salt| {
            (0..length)
                .map(|i| f64::from_bits(PAYLOADS[(i + salt) % PAYLOADS.len()]))
                .collect()
        };
        MarketData {
            open: values(lengths[0], 0),
            high: values(lengths[1], 1),
            low: values(lengths[2], 2),
            close: values(lengths[3], 3),
            volume: values(lengths[4], 4),
            time: (0..lengths[5])
                .map(|i| 1_700_000_000_000 + i64::try_from(i).expect("small test index") * 60_000)
                .collect(),
            ..MarketData::default()
        }
    }

    #[test]
    fn frozen_reference_matches_2048_independent_lane_cases() {
        let mut state = 0x015e_eda1_u64;
        for case in 0..2048 {
            let mut lengths = [0; 6];
            for length in &mut lengths {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                *length = usize::try_from((state >> 32) % 10).expect("small length");
            }
            let data = feed(lengths);
            let index = case % 11;
            let last = if case % 5 == 0 { usize::MAX } else { case + 17 };
            let expected = observe(|| frozen(&data, index, last));
            assert_eq!(observe(|| ohlcv_open_bucket(&data, index, last)), expected);
            assert_eq!(
                observe(|| OhlcvSource(&data).open_bucket(index, last)),
                expected
            );
        }
    }

    #[test]
    fn payload_bits_and_supplied_metadata_are_exact() {
        let data = feed([8; 6]);
        for index in 0..8 {
            for last in [0, 1, index, index + 17, usize::MAX] {
                let actual = bits(ohlcv_open_bucket(&data, index, last));
                assert_eq!(actual, bits(frozen(&data, index, last)));
                assert_eq!(actual.0[0], data.open[index].to_bits());
                assert_eq!(actual.0[1], data.high[index].to_bits());
                assert_eq!(actual.0[2], data.low[index].to_bits());
                assert_eq!(actual.0[3], data.close[index].to_bits());
                assert_eq!(actual.0[4], data.volume[index].to_bits());
                assert_eq!(actual.1, data.time[index]);
                assert_eq!((actual.2, actual.3), (last, last));
            }
        }
    }

    #[test]
    fn earliest_payload_failure_precedes_later_lanes_and_time() {
        for first in 0..5 {
            let mut lengths = [10; 6];
            for (lane, length) in lengths.iter_mut().enumerate().take(5).skip(first) {
                *length = lane;
            }
            // An absent time lane would synthesize successfully. A nonempty
            // short lane must still lose to the earlier price read.
            lengths[5] = 1;
            let data = feed(lengths);
            let actual = observe(|| ohlcv_open_bucket(&data, 9, 27));
            assert_eq!(actual, observe(|| frozen(&data, 9, 27)));
            assert_eq!(
                actual,
                Err(format!(
                    "index out of bounds: the len is {first} but the index is 9"
                ))
            );
        }
    }

    #[test]
    fn time_read_follows_all_payloads_and_retains_synthesis() {
        let explicit_short = feed([3, 3, 3, 3, 3, 1]);
        assert_eq!(
            observe(|| ohlcv_open_bucket(&explicit_short, 2, usize::MAX)),
            Err("index out of bounds: the len is 1 but the index is 2".to_owned())
        );
        let synthetic = feed([8, 8, 8, 8, 8, 0]);
        for index in 0..8 {
            let actual = bits(ohlcv_open_bucket(&synthetic, index, 91));
            assert_eq!(actual, bits(frozen(&synthetic, index, 91)));
            assert_eq!(
                actual.1,
                crate::SYNTHETIC_EPOCH_MS
                    + i64::try_from(index).expect("small") * crate::MS_PER_DAY
            );
        }
    }

    #[test]
    fn empty_and_max_indices_fail_before_synthetic_time() {
        let empty = MarketData::default();
        for index in [0, 1, usize::MAX] {
            let actual = observe(|| ohlcv_open_bucket(&empty, index, usize::MAX));
            assert_eq!(actual, observe(|| frozen(&empty, index, usize::MAX)));
            assert_eq!(
                actual,
                Err(format!(
                    "index out of bounds: the len is 0 but the index is {index}"
                ))
            );
        }
    }

    #[test]
    fn actual_fold_uses_the_opening_helper_without_changing_group_metadata() {
        let data = feed([3; 6]);
        let source_bits = (
            data.open.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            data.close.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            data.time.clone(),
        );
        let bars = fold_buckets(&OhlcvSource(&data), &[0, 0, 1], &[1, 2]);
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].open.to_bits(), data.open[0].to_bits());
        assert_eq!(bars[0].time, data.time[0]);
        assert_eq!(
            (bars[0].last_chart_bar, bars[0].publication_chart_bar),
            (1, 1)
        );
        assert_eq!(bits(bars[1].clone()), bits(frozen(&data, 2, 2)));
        assert_eq!(
            source_bits.0,
            data.open.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
        );
        assert_eq!(
            source_bits.1,
            data.close.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
        );
        assert_eq!(source_bits.2, data.time);
    }
}

#[cfg(test)]
mod ohlcv_extend_kernel {
    use super::*;
    use std::{any::Any, cell::RefCell, panic::AssertUnwindSafe};

    type Bits = ([u64; 5], i64, usize, usize);
    const PAYLOADS: [u64; 12] = [
        0,
        1 << 63,
        0x7ff8_0000_0000_1234,
        0xfff8_0000_0000_5678,
        0x7ff0_0000_0000_4321,
        0xfff0_0000_0000_9876,
        0x7ff0_0000_0000_0000,
        0xfff0_0000_0000_0000,
        0x7fef_ffff_ffff_ffff,
        0xffef_ffff_ffff_ffff,
        0x3ff0_0000_0000_0000,
        1,
    ];

    // Copy of the OhlcvSource::extend_bucket body, with only the receiver
    // projection moved to this argument. Independent of the dictionary helper.
    fn frozen(data: &MarketData, cur: &mut HtfBar, i: usize) {
        cur.high = cur.high.max(data.high[i]);
        cur.low = cur.low.min(data.low[i]);
        cur.close = data.close[i];
        cur.volume += data.volume[i];
    }

    fn bits(bar: &HtfBar) -> Bits {
        (
            [
                bar.open.to_bits(),
                bar.high.to_bits(),
                bar.low.to_bits(),
                bar.close.to_bits(),
                bar.volume.to_bits(),
            ],
            bar.time,
            bar.last_chart_bar,
            bar.publication_chart_bar,
        )
    }

    fn bar(salt: usize) -> HtfBar {
        HtfBar {
            open: f64::from_bits(PAYLOADS[salt % PAYLOADS.len()]),
            high: f64::from_bits(PAYLOADS[(salt + 1) % PAYLOADS.len()]),
            low: f64::from_bits(PAYLOADS[(salt + 2) % PAYLOADS.len()]),
            close: f64::from_bits(PAYLOADS[(salt + 3) % PAYLOADS.len()]),
            volume: f64::from_bits(PAYLOADS[(salt + 4) % PAYLOADS.len()]),
            time: i64::MIN,
            last_chart_bar: usize::MAX,
            publication_chart_bar: 17,
        }
    }

    fn feed(lengths: [usize; 4], salt: usize) -> MarketData {
        let lane = |length: usize, offset: usize| {
            (0..length)
                .map(|i| f64::from_bits(PAYLOADS[(salt + i + offset) % PAYLOADS.len()]))
                .collect()
        };
        MarketData {
            high: lane(lengths[0], 0),
            low: lane(lengths[1], 1),
            close: lane(lengths[2], 2),
            volume: lane(lengths[3], 3),
            // Opening/time lanes are deliberately absent: extension never reads them.
            ..MarketData::default()
        }
    }

    fn panic_text(panic: &(dyn Any + Send)) -> String {
        if let Some(text) = panic.downcast_ref::<String>() {
            text.clone()
        } else if let Some(text) = panic.downcast_ref::<&str>() {
            (*text).to_owned()
        } else {
            "non-string panic".to_owned()
        }
    }

    fn observe(mut cur: HtfBar, body: impl FnOnce(&mut HtfBar)) -> (Result<(), String>, Bits) {
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| body(&mut cur)))
            .map_err(|panic| panic_text(&*panic));
        (result, bits(&cur))
    }

    fn helper(data: &MarketData, cur: &mut HtfBar, i: usize) {
        ohlcv_extend_lanes(
            &F64OhlcvMergeOps,
            &data.high,
            &data.low,
            &data.close,
            &data.volume,
            cur,
            i,
        );
    }

    fn assert_same_observation(
        actual: &(Result<(), String>, Bits),
        expected: &(Result<(), String>, Bits),
        data: &MarketData,
        index: usize,
    ) {
        // These native operations cannot panic; only the reached indexed reads
        // decide whether an arithmetic assignment has run.
        let high_ran = index < data.high.len();
        let low_ran = high_ran && index < data.low.len();
        let volume_ran = low_ran && index < data.close.len() && index < data.volume.len();
        assert!(
            ohlcv_test_contract::same_observation(
                actual,
                expected,
                [high_ran, low_ran, volume_ran]
            ),
            "index {index}; actual {actual:?}; copy {expected:?}"
        );
    }

    #[test]
    fn frozen_reference_matches_4096_shapes_and_partial_states() {
        let mut state = 0x017e_eda1_u64;
        for case in 0..4096 {
            let mut lengths = [0; 4];
            for length in &mut lengths {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                *length = usize::try_from((state >> 32) % 10).expect("small length");
            }
            let data = feed(lengths, case);
            let index = case % 11;
            let expected = observe(bar(case + 3), |cur| frozen(&data, cur, index));
            assert_same_observation(
                &observe(bar(case + 3), |cur| helper(&data, cur, index)),
                &expected,
                &data,
                index,
            );
            assert_same_observation(
                &observe(bar(case + 3), |cur| {
                    OhlcvSource(&data).extend_bucket(cur, index);
                }),
                &expected,
                &data,
                index,
            );
        }
    }

    #[test]
    fn all_special_pairs_match_native_contract_and_preserve_frame() {
        for left in 0..PAYLOADS.len() {
            for right in 0..PAYLOADS.len() {
                let data = feed([1; 4], right);
                let before = bar(left);
                let actual = observe(before.clone(), |cur| helper(&data, cur, 0));
                assert_same_observation(
                    &actual,
                    &observe(before.clone(), |cur| frozen(&data, cur, 0)),
                    &data,
                    0,
                );
                assert_eq!(actual.0, Ok(()));
                assert_eq!(actual.1 .0[0], before.open.to_bits());
                assert_eq!(
                    (actual.1 .1, actual.1 .2, actual.1 .3),
                    (i64::MIN, usize::MAX, 17)
                );
                assert_eq!(actual.1 .0[3], data.close[0].to_bits());
            }
        }
    }

    #[test]
    fn later_failed_read_keeps_exact_earlier_assignments() {
        let initial = HtfBar {
            open: -0.0,
            high: 2.0,
            low: 6.0,
            close: 0.0,
            volume: 7.0,
            time: i64::MAX,
            last_chart_bar: 81,
            publication_chart_bar: usize::MAX,
        };
        for first in 0..4 {
            let mut data = MarketData {
                high: vec![10.0; 10],
                low: vec![-3.0; 10],
                close: vec![42.0; 10],
                volume: vec![5.0; 10],
                ..MarketData::default()
            };
            match first {
                0 => data.high.clear(),
                1 => data.low.truncate(1),
                2 => data.close.truncate(2),
                _ => data.volume.truncate(3),
            }
            let actual = observe(initial.clone(), |cur| helper(&data, cur, 9));
            assert_eq!(
                actual,
                observe(initial.clone(), |cur| frozen(&data, cur, 9))
            );
            assert_eq!(
                actual,
                observe(initial.clone(), |cur| OhlcvSource(&data)
                    .extend_bucket(cur, 9))
            );
            assert_eq!(
                actual.0,
                Err(format!(
                    "index out of bounds: the len is {first} but the index is 9"
                ))
            );
            let mut expected = initial.clone();
            if first > 0 {
                expected.high = 10.0;
            }
            if first > 1 {
                expected.low = -3.0;
            }
            if first > 2 {
                expected.close = 42.0;
            }
            assert_eq!(actual.1, bits(&expected));
        }
    }

    #[test]
    fn empty_and_maximum_indices_fail_before_any_assignment() {
        for data in [MarketData::default(), feed([2; 4], 0)] {
            for index in [usize::MAX, usize::MAX - 1] {
                let initial = bar(5);
                let actual = observe(initial.clone(), |cur| helper(&data, cur, index));
                assert_eq!(
                    actual,
                    observe(initial.clone(), |cur| frozen(&data, cur, index))
                );
                assert!(actual.0.is_err());
                assert_eq!(actual.1, bits(&initial));
            }
        }
    }

    struct TraceOps {
        calls: RefCell<Vec<(&'static str, u64, u64)>>,
        panic_at: Option<&'static str>,
    }

    impl TraceOps {
        fn call(&self, name: &'static str, left: u64, right: u64, scale: u64) -> u64 {
            self.calls.borrow_mut().push((name, left, right));
            assert_ne!(self.panic_at, Some(name), "operation {name}");
            left * scale + right
        }
    }

    impl OhlcvMergeOps<u64> for TraceOps {
        fn max(&self, left: u64, right: u64) -> u64 {
            self.call("max", left, right, 10)
        }
        fn min(&self, left: u64, right: u64) -> u64 {
            self.call("min", left, right, 20)
        }
        fn add(&self, left: u64, right: u64) -> u64 {
            self.call("add", left, right, 30)
        }
    }

    fn generic_bar() -> HtfBar<u64> {
        HtfBar {
            open: 91,
            high: 2,
            low: 3,
            close: 4,
            volume: 5,
            time: -17,
            last_chart_bar: usize::MAX,
            publication_chart_bar: 23,
        }
    }

    #[test]
    fn generic_dictionary_order_operands_and_interleaved_reads() {
        let ops = TraceOps {
            calls: RefCell::new(vec![]),
            panic_at: None,
        };
        let mut cur = generic_bar();
        ohlcv_extend_lanes(&ops, &[7], &[11], &[13], &[17], &mut cur, 0);
        assert_eq!(
            *ops.calls.borrow(),
            [("max", 2, 7), ("min", 3, 11), ("add", 5, 17)]
        );
        assert_eq!(
            (cur.high, cur.low, cur.close, cur.volume),
            (27, 71, 13, 167)
        );
        assert_eq!(
            (
                cur.open,
                cur.time,
                cur.last_chart_bar,
                cur.publication_chart_bar
            ),
            (91, -17, usize::MAX, 23)
        );
        for first in 0..4 {
            ops.calls.borrow_mut().clear();
            let mut cur = generic_bar();
            let mut lanes = [vec![7], vec![11], vec![13], vec![17]];
            lanes[first].clear();
            let failed = std::panic::catch_unwind(AssertUnwindSafe(|| {
                ohlcv_extend_lanes(
                    &ops, &lanes[0], &lanes[1], &lanes[2], &lanes[3], &mut cur, 0,
                );
            }));
            assert!(failed.is_err());
            let expected_calls = match first {
                0 => vec![],
                1 => vec![("max", 2, 7)],
                _ => vec![("max", 2, 7), ("min", 3, 11)],
            };
            assert_eq!(*ops.calls.borrow(), expected_calls);
            assert_eq!(cur.high, if first > 0 { 27 } else { 2 });
            assert_eq!(cur.low, if first > 1 { 71 } else { 3 });
            assert_eq!(cur.close, if first > 2 { 13 } else { 4 });
            assert_eq!(cur.volume, 5);
        }
    }

    #[test]
    fn generic_operation_panics_keep_only_prior_updates() {
        for (name, expected) in [
            ("max", (2, 3, 4, 5)),
            ("min", (27, 3, 4, 5)),
            ("add", (27, 71, 13, 5)),
        ] {
            let ops = TraceOps {
                calls: RefCell::new(vec![]),
                panic_at: Some(name),
            };
            let mut cur = generic_bar();
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                ohlcv_extend_lanes(&ops, &[7], &[11], &[13], &[17], &mut cur, 0);
            }));
            assert!(result.is_err());
            assert_eq!((cur.high, cur.low, cur.close, cur.volume), expected);
            assert_eq!(
                (
                    cur.open,
                    cur.time,
                    cur.last_chart_bar,
                    cur.publication_chart_bar
                ),
                (91, -17, usize::MAX, 23)
            );
            assert_eq!(ops.calls.borrow().last().expect("called").0, name);
        }
    }

    #[test]
    fn volume_retains_overflow_nan_and_left_fold_association() {
        let mut data = feed([3; 4], 0);
        data.high.fill(2.0);
        data.low.fill(-2.0);
        data.close.fill(-0.0);
        data.volume = vec![-1e16, 1.0, f64::MAX];
        let mut actual = bar(0);
        actual.volume = 1e16;
        let mut expected = actual.clone();
        for index in 0..2 {
            helper(&data, &mut actual, index);
            frozen(&data, &mut expected, index);
            assert_eq!(bits(&actual), bits(&expected));
        }
        assert_eq!(actual.volume, 1.0);
        assert_eq!(1e16 + (-1e16 + 1.0), 0.0);
        actual.volume = f64::MAX;
        helper(&data, &mut actual, 2);
        assert_eq!(actual.volume, f64::INFINITY);
        data.volume[2] = f64::NEG_INFINITY;
        let result = observe(actual.clone(), |cur| helper(&data, cur, 2));
        assert_same_observation(
            &result,
            &observe(actual, |cur| frozen(&data, cur, 2)),
            &data,
            2,
        );
        assert!(f64::from_bits(result.1 .0[4]).is_nan());
    }

    #[test]
    fn actual_fold_matches_frozen_source_and_preserves_final_metadata() {
        struct FrozenSource<'a>(&'a MarketData);
        impl BarSource<HtfBar> for FrozenSource<'_> {
            fn open_bucket(&self, index: usize, last: usize) -> HtfBar {
                ohlcv_open_bucket(self.0, index, last)
            }
            fn extend_bucket(&self, cur: &mut HtfBar, index: usize) {
                frozen(self.0, cur, index);
            }
        }
        let mut data = feed([6; 4], 0);
        data.open = vec![-0.0; 6];
        data.time = vec![1, 2, 3, 4, 5, 6];
        for ids in [[1, 1, 2, 2, 1, 1], [7, 7, 7, 7, 7, 7], [0, 1, 2, 3, 4, 5]] {
            let bi = bucket_index(&ids);
            let actual = fold_buckets(&OhlcvSource(&data), &bi.bucket_of, &bi.last_chart_bar);
            let expected = fold_buckets(&FrozenSource(&data), &bi.bucket_of, &bi.last_chart_bar);
            assert_eq!(actual.len(), expected.len());
            let extended = ohlcv_test_contract::extended_buckets(&bi.bucket_of, actual.len())
                .expect("real grouping produces in-range buckets");
            assert!(
                actual
                    .iter()
                    .zip(&expected)
                    .zip(extended)
                    .all(|((a, b), ran)| ohlcv_test_contract::same_bar(a, b, ran)),
                "ids {ids:?}; actual {actual:?}; copy {expected:?}"
            );
            for (group, last) in actual.iter().zip(&bi.last_chart_bar) {
                assert_eq!(
                    (group.last_chart_bar, group.publication_chart_bar),
                    (*last, *last)
                );
            }
        }
    }
}

#[cfg(test)]
mod requested_spine_tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    // Copy of the full production function; its branch body is kept unchanged.
    fn reference_aligned_security(
        data: &MarketData,
        seconds: i64,
        calendar: CalendarAlignment,
    ) -> Resampled {
        if let Some((_, feed)) = data
            .request_bars
            .iter()
            .find(|(tf, _)| host_timeframe_key(tf) == Some((seconds, calendar)))
        {
            let n = data.bars();
            let mut bucket_of = Vec::with_capacity(n);
            for chart_bar in 0..n {
                let time = data.time_at(chart_bar);
                // MarketData validation guarantees that the feed brackets every
                // chart open and that feed bars are ordered and nonoverlapping.
                let bucket = feed.time.partition_point(|&open| open <= time) - 1;
                bucket_of.push(bucket);
            }
            let mut bars: Vec<HtfBar> = (0..feed.time.len())
                .map(|bucket| HtfBar {
                    open: feed.open[bucket],
                    high: feed.high[bucket],
                    low: feed.low[bucket],
                    close: feed.close[bucket],
                    volume: feed.volume[bucket],
                    time: feed.time[bucket],
                    last_chart_bar: 0,
                    publication_chart_bar: usize::MAX,
                })
                .collect();
            for (chart_bar, &bucket) in bucket_of.iter().enumerate() {
                bars[bucket].last_chart_bar = chart_bar;
                if bars[bucket].publication_chart_bar == usize::MAX
                    && (data.time_close_at(chart_bar) >= feed.time_close[bucket]
                        || (chart_bar + 1 < n
                            && data.time_at(chart_bar + 1) >= feed.time_close[bucket]))
                {
                    bars[bucket].publication_chart_bar = chart_bar;
                }
            }
            return Resampled { bars, bucket_of };
        }
        if let Some((_, closes)) = data
            .request_time_close
            .iter()
            .find(|(tf, _)| host_timeframe_key(tf) == Some((seconds, calendar)))
        {
            // Equal closing instants identify the same requested
            // bar. OHLCV still comes entirely from the supplied chart feed.
            let BucketIndex {
                bucket_of,
                last_chart_bar,
            } = requested_close_buckets(data, closes);
            let bars = fold_buckets(&OhlcvSource(data), &bucket_of, &last_chart_bar);
            let mut r = Resampled { bars, bucket_of };
            crate::vm_publication_axis::publish_market_data(data, &mut r.bars, closes);
            return r;
        }
        match calendar {
            CalendarAlignment::Months(months) => resample_calendar_months(data, months),
            CalendarAlignment::Weeks(weeks) => resample_calendar_weeks(data, weeks),
            CalendarAlignment::Fixed => resample(data, seconds),
        }
    }

    fn outcome(run: impl FnOnce() -> Resampled) -> Result<Resampled, String> {
        catch_unwind(AssertUnwindSafe(run)).map_err(|error| {
            error
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| error.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_else(|| "non-string panic".to_owned())
        })
    }

    fn compare(data: &MarketData, seconds: i64, calendar: CalendarAlignment) {
        let actual = outcome(|| resample_aligned_security(data, seconds, calendar));
        let frozen = outcome(|| reference_aligned_security(data, seconds, calendar));
        match (actual, frozen) {
            (Ok(a), Ok(b)) => {
                assert_eq!(a.bucket_of, b.bucket_of, "copy bucket map");
                assert_eq!(a.bars.len(), b.bars.len(), "bar count");
                let extended = ohlcv_test_contract::extended_buckets(&a.bucket_of, a.bars.len())
                    .expect("real bucket indices");
                for (i, (a, b)) in a.bars.iter().zip(&b.bars).enumerate() {
                    assert!(
                        ohlcv_test_contract::same_bar(a, b, extended[i]),
                        "bucket {i}: actual {a:?}, copy {b:?}"
                    );
                }
            }
            (Err(a), Err(b)) => assert_eq!(a, b, "panic kind and first failed read"),
            (a, b) => panic!("outcome changed: actual {a:?}, copy {b:?}"),
        }
    }

    fn feed(n: usize) -> MarketData {
        MarketData {
            open: vec![1.0; n],
            high: vec![3.0; n],
            low: vec![-1.0; n],
            close: vec![2.0; n],
            volume: vec![4.0; n],
            time: (0..n).map(|i| i64::try_from(i).unwrap() * 10).collect(),
            time_close: (0..n).map(|i| i64::try_from(i).unwrap() * 10 + 5).collect(),
            ..MarketData::default()
        }
    }

    #[test]
    fn requested_spine_matches_current_entire_branch_4096_shapes() {
        let bits = [
            0,
            0x8000_0000_0000_0000,
            1,
            0x7ff8_1234_5678_9abc,
            0x7ff0_0000_0000_0001,
            0x7ff0_0000_0000_0000,
            0xfff0_0000_0000_0000,
            0x3ff0_0000_0000_0000,
        ];
        for case in 0usize..4096 {
            let n = case % 8;
            let mut data = feed(n);
            for i in 0..n {
                data.open[i] = f64::from_bits(bits[(case + i) % 8]);
                data.high[i] = f64::from_bits(bits[(case + i + 1) % 8]);
                data.low[i] = f64::from_bits(bits[(case + i + 2) % 8]);
                data.close[i] = f64::from_bits(bits[(case + i + 3) % 8]);
                data.volume[i] = f64::from_bits(bits[(case + i + 4) % 8]);
            }
            data.timeframe = ["1D", "?", "-3S", "\u{2003}8S\u{3000}"][case % 4].into();
            let count = (case / 8) % 10;
            let requested: Vec<i64> = (0..count)
                .map(|i| match (case / 80) % 4 {
                    0 => i64::try_from(i / 2).unwrap() * 20 + 7,
                    1 => {
                        if i % 2 == 0 {
                            i64::MIN
                        } else {
                            i64::MAX
                        }
                    }
                    2 => 17,
                    _ => i64::try_from(i).unwrap() * 10 + 7,
                })
                .collect();
            data.request_time_close.insert("W".into(), requested);
            match (case / 320) % 5 {
                0 => data.time_close.clear(),
                1 => data.time_close.truncate(n / 2),
                2 => data.time.truncate(n / 2),
                3 => data.open.truncate(n / 2),
                _ => {}
            }
            let before = data.close.iter().map(|v| v.to_bits()).collect::<Vec<_>>();
            compare(&data, 604_800, CalendarAlignment::Weeks(1));
            assert_eq!(
                before,
                data.close.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn requested_spine_exact_payload_map_and_delayed_publication() {
        let mut data = feed(5);
        data.open = vec![10.0, 11.0, 12.0, 13.0, 14.0];
        data.close = vec![20.0, 21.0, 22.0, 23.0, 24.0];
        data.volume = vec![1.0, 2.0, 4.0, 8.0, 16.0];
        data.request_time_close
            .insert("W".into(), vec![20, 20, 40, 40, 60, 999]);
        compare(&data, 604_800, CalendarAlignment::Weeks(1));
        let result = resample_aligned_security(&data, 604_800, CalendarAlignment::Weeks(1));
        assert_eq!(result.bucket_of, [0, 0, 1, 1, 2]);
        assert_eq!(
            result
                .bars
                .iter()
                .map(|b| (
                    b.open,
                    b.high,
                    b.low,
                    b.close,
                    b.volume,
                    b.time,
                    b.last_chart_bar,
                    b.publication_chart_bar
                ))
                .collect::<Vec<_>>(),
            [
                (10.0, 3.0, -1.0, 21.0, 3.0, 0, 1, 2),
                (12.0, 3.0, -1.0, 23.0, 12.0, 20, 3, 4),
                (14.0, 3.0, -1.0, 24.0, 16.0, 40, 4, usize::MAX)
            ]
        );
        data.request_time_close.get_mut("W").unwrap()[5] = i64::MIN;
        compare(&data, 604_800, CalendarAlignment::Weeks(1));
        assert_eq!(
            result.bucket_of,
            resample_aligned_security(&data, 604_800, CalendarAlignment::Weeks(1)).bucket_of
        );
    }

    #[test]
    fn requested_spine_empty_and_request_bars_priority_and_fallback() {
        let mut empty = feed(0);
        empty.request_time_close.insert("W".into(), vec![]);
        compare(&empty, 604_800, CalendarAlignment::Weeks(1));
        assert!(
            resample_aligned_security(&empty, 604_800, CalendarAlignment::Weeks(1))
                .bars
                .is_empty()
        );
        let mut data = feed(2);
        data.request_time_close.insert("W".into(), vec![]);
        data.request_bars.insert(
            "W".into(),
            crate::RequestTimeframeBars {
                time: vec![0, 10],
                time_close: vec![7, 17],
                open: vec![9.0; 2],
                high: vec![11.0; 2],
                low: vec![8.0; 2],
                close: vec![10.0; 2],
                volume: vec![2.0; 2],
            },
        );
        compare(&data, 604_800, CalendarAlignment::Weeks(1));
        assert_eq!(
            resample_aligned_security(&data, 604_800, CalendarAlignment::Weeks(1)).bars[0].open,
            9.0
        );
        data.request_time_close.clear();
        data.request_bars.clear();
        for calendar in [
            CalendarAlignment::Fixed,
            CalendarAlignment::Weeks(1),
            CalendarAlignment::Months(1),
        ] {
            compare(&data, 604_800, calendar);
        }
    }

    #[test]
    fn requested_spine_malformed_prefix_then_ohlcv_then_clock() {
        let mut data = feed(3);
        data.open.clear();
        data.time.clear();
        data.time_close.clear();
        data.request_time_close.insert("W".into(), vec![10]);
        compare(&data, 604_800, CalendarAlignment::Weeks(1));
        let first =
            outcome(|| resample_aligned_security(&data, 604_800, CalendarAlignment::Weeks(1)))
                .unwrap_err();
        assert!(
            first.contains("len is 1") && first.contains("index is 1"),
            "{first}"
        );
        data.request_time_close.insert("W".into(), vec![10, 10, 10]);
        compare(&data, 604_800, CalendarAlignment::Weeks(1));
        let second =
            outcome(|| resample_aligned_security(&data, 604_800, CalendarAlignment::Weeks(1)))
                .unwrap_err();
        assert!(
            second.contains("len is 0") && second.contains("index is 0"),
            "{second}"
        );
        data = feed(3);
        data.time_close = vec![1];
        data.request_time_close.insert("W".into(), vec![20, 20, 20]);
        compare(&data, 604_800, CalendarAlignment::Weeks(1));
        let third =
            outcome(|| resample_aligned_security(&data, 604_800, CalendarAlignment::Weeks(1)))
                .unwrap_err();
        assert!(
            third.contains("len is 1") && third.contains("index is 2"),
            "{third}"
        );
    }
}

/// [`BucketOpens`] against the full [`resample_timeframe`] regroup
/// for `time(timeframe)`: grouped at once, grown one appended bar at a time as
/// realtime ticks grow the feed, and regrouped when an append materializes the
/// trading-day lane.
#[cfg(test)]
mod bucket_opens_tests {
    use super::*;

    const HOUR: i64 = 3_600_000;
    const DAY: i64 = 24 * HOUR;
    /// Sunday 2020-12-20 00:00 UTC, so the feeds cross week, month and year
    /// boundaries.
    const T0: i64 = 18_616 * DAY;
    const BARS: usize = 160;

    /// Every grouping `resample_timeframe` selects: fixed widths (with the
    /// empty daily default), calendar weeks and months, a lowercase unit, and
    /// multipliers that clamp to one or saturate.
    const TIMEFRAMES: [&str; 15] = [
        "",
        "1S",
        "60",
        "240",
        "D",
        "3D",
        "1W",
        "2W",
        "3w",
        "1M",
        "2M",
        "3M",
        "12M",
        "0W",
        "99999999999M",
    ];

    #[derive(Clone, Copy)]
    enum Lane {
        /// No trading-day lane: calendar buckets read each bar's close.
        Absent,
        /// The lane a host update that brings a `tradingDay` materializes,
        /// from each bar's open.
        Fallback,
        /// A host lane naming the exchange date the bar closes on.
        ClosingDate,
    }

    struct Feed {
        timeframe: &'static str,
        timezone: &'static str,
        /// Bar open times; empty for the synthesized daily axis.
        times: Vec<i64>,
        lane: Lane,
    }

    struct Lcg(u64);

    impl Lcg {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 11) % n
        }
    }

    /// `BARS` open times from `T0 + offset`, each `step` apart plus an
    /// occasional gap of up to three days.
    fn times(rng: &mut Lcg, step: i64, offset: i64) -> Vec<i64> {
        let mut time = T0 + offset;
        (0..BARS)
            .map(|_| {
                let open = time;
                time += step;
                if rng.below(4) == 0 {
                    time += i64::try_from(rng.below(3 * 24)).expect("small") * HOUR;
                }
                open
            })
            .collect()
    }

    fn feeds() -> Vec<Feed> {
        let mut rng = Lcg(0x1362);
        vec![
            Feed {
                timeframe: "60",
                timezone: "UTC",
                times: times(&mut rng, HOUR, 0),
                lane: Lane::Absent,
            },
            // A half-hour offset: hourly bars straddle exchange midnight.
            Feed {
                timeframe: "60",
                timezone: "UTC+05:30",
                times: times(&mut rng, HOUR, 0),
                lane: Lane::Absent,
            },
            // Daily bars opening at 00:00 UTC open on the previous exchange
            // date but close on their own, so the lane regroups them.
            Feed {
                timeframe: "1D",
                timezone: "UTC-5",
                times: times(&mut rng, DAY, 0),
                lane: Lane::Absent,
            },
            Feed {
                timeframe: "1D",
                timezone: "UTC-5",
                times: times(&mut rng, DAY, 0),
                lane: Lane::Fallback,
            },
            Feed {
                timeframe: "240",
                timezone: "UTC-11",
                times: times(&mut rng, 4 * HOUR, 3 * HOUR),
                lane: Lane::Absent,
            },
            Feed {
                timeframe: "1D",
                timezone: "UTC+14",
                times: times(&mut rng, DAY, 0),
                lane: Lane::ClosingDate,
            },
            Feed {
                timeframe: "1D",
                timezone: "UTC",
                times: Vec::new(),
                lane: Lane::Absent,
            },
        ]
    }

    impl Feed {
        fn empty(&self) -> MarketData {
            let mut data = MarketData {
                timeframe: self.timeframe.to_string(),
                ..MarketData::default()
            };
            data.syminfo.timezone = self.timezone.to_string();
            data
        }

        /// Append bar `bar` of this feed to `data`, as a realtime tick does,
        /// extending the trading-day lane when `data` carries one.
        fn append(&self, data: &mut MarketData, bar: usize) {
            for lane in [
                &mut data.open,
                &mut data.high,
                &mut data.low,
                &mut data.close,
                &mut data.volume,
            ] {
                lane.push(1.0);
            }
            if !self.times.is_empty() {
                data.time.push(self.times[bar]);
            }
            let day = match self.lane {
                Lane::Absent if data.trading_day.is_empty() => return,
                Lane::Absent | Lane::Fallback => data.trading_day_at(bar),
                Lane::ClosingDate => {
                    data.fallback_trading_day_for_time(data.time_close_at(bar).saturating_sub(1))
                }
            };
            data.trading_day.push(day);
        }

        /// The first `bars` bars of this feed.
        fn prefix(&self, bars: usize) -> MarketData {
            let mut data = self.empty();
            for bar in 0..bars {
                self.append(&mut data, bar);
            }
            data
        }
    }

    /// Each chart bar's bucket open, as a full regroup reports it.
    fn regrouped(data: &MarketData, timeframe: &str) -> Vec<i64> {
        let r = resample_timeframe(data, timeframe);
        r.bucket_of
            .iter()
            .map(|&bucket| r.bars[bucket].time)
            .collect()
    }

    fn opens(memo: &BucketOpens) -> Vec<i64> {
        (0..memo.grouped_bars())
            .map(|bar| memo.get(bar).expect("grouped bar"))
            .collect()
    }

    /// Whether `timeframe`'s buckets read the trading-day lane: weeks,
    /// months and single days.
    fn is_calendar(timeframe: &str) -> bool {
        matches!(timeframe_parts(timeframe).1, Some('W' | 'M'))
            || timeframe_in_seconds(timeframe) == 86_400
    }

    #[test]
    fn one_day_resamples_follow_a_trading_day_lane() {
        // Evening-session bars: two trading days, each starting at 22:00 UTC
        // the evening before, which midnight would split.
        let mut data = MarketData {
            open: vec![1.0; 6],
            high: vec![1.0; 6],
            low: vec![1.0; 6],
            close: vec![1.0; 6],
            volume: vec![1.0; 6],
            ..MarketData::default()
        };
        let evening = 1_710_108_000_000; // 2024-03-10 22:00 UTC
        data.time = (0..6).map(|i| evening + i * 3 * 3_600_000).collect();
        let monday = 1_710_115_200_000; // 2024-03-11 00:00 UTC
        data.trading_day = (0..6)
            .map(|i| if i < 3 { monday } else { monday + 86_400_000 })
            .collect();
        let days = resample_days_or_fixed(&data, 86_400);
        assert_eq!(days.bucket_of, [0, 0, 0, 1, 1, 1]);
        // Without the lane, midnight splits them.
        data.trading_day.clear();
        let midnight = resample_days_or_fixed(&data, 86_400);
        assert_ne!(midnight.bucket_of, [0, 0, 0, 1, 1, 1]);
    }

    #[test]
    fn grouping_a_whole_feed_matches_a_full_regroup() {
        for (index, feed) in feeds().iter().enumerate() {
            let data = feed.prefix(BARS);
            for timeframe in TIMEFRAMES {
                let memo = BucketOpens::new(&data, timeframe);
                assert_eq!(memo.grouped_bars(), BARS);
                assert_eq!(memo.get(BARS), None);
                assert_eq!(memo.stale_bars(&data), 0);
                assert_eq!(
                    opens(&memo),
                    regrouped(&data, timeframe),
                    "feed {index}, timeframe {timeframe:?}"
                );
            }
        }
    }

    #[test]
    fn appending_one_bar_at_a_time_matches_a_full_regroup_of_each_feed_state() {
        let mut rng = Lcg(0xA1E);
        for (index, feed) in feeds().iter().enumerate() {
            for timeframe in TIMEFRAMES {
                let history = usize::try_from(rng.below(BARS as u64 / 2)).expect("small");
                let mut data = feed.prefix(history);
                let mut memo = BucketOpens::new(&data, timeframe);
                for bar in history..BARS {
                    feed.append(&mut data, bar);
                    assert_eq!(memo.stale_bars(&data), 1, "feed {index}, bar {bar}");
                    memo.refresh(&data);
                    assert_eq!(memo.stale_bars(&data), 0);
                    assert_eq!(
                        opens(&memo),
                        regrouped(&data, timeframe),
                        "feed {index}, timeframe {timeframe:?}, bar {bar}"
                    );
                    assert_eq!(memo, BucketOpens::new(&data, timeframe));
                }
            }
        }
    }

    #[test]
    fn a_materialized_trading_day_lane_regroups_calendar_buckets_once() {
        let mut regrouped_history = 0;
        for (index, feed) in feeds().iter().enumerate() {
            if !matches!(feed.lane, Lane::Absent) {
                continue;
            }
            for timeframe in TIMEFRAMES {
                let history = BARS / 2;
                let mut data = feed.prefix(history);
                let mut memo = BucketOpens::new(&data, timeframe);
                let before = opens(&memo);
                // A host update that brings a `tradingDay` to a feed without
                // a lane materializes one from every bar's open, then appends
                // the new bar's day.
                data.trading_day = (0..history).map(|bar| data.trading_day_at(bar)).collect();
                feed.append(&mut data, history);
                let expected_stale = if is_calendar(timeframe) {
                    history + 1
                } else {
                    1
                };
                assert_eq!(
                    memo.stale_bars(&data),
                    expected_stale,
                    "feed {index}, timeframe {timeframe:?}"
                );
                memo.refresh(&data);
                let after = opens(&memo);
                assert_eq!(
                    after,
                    regrouped(&data, timeframe),
                    "feed {index}, {timeframe:?}"
                );
                if before[..] != after[..history] {
                    assert!(is_calendar(timeframe), "fixed widths read only opens");
                    regrouped_history += 1;
                }
                for bar in history + 1..BARS {
                    feed.append(&mut data, bar);
                    assert_eq!(memo.stale_bars(&data), 1);
                    memo.refresh(&data);
                    assert_eq!(
                        opens(&memo),
                        regrouped(&data, timeframe),
                        "feed {index}, timeframe {timeframe:?}, bar {bar}"
                    );
                }
            }
        }
        // The regroup is not vacuous: the 00:00 UTC daily feed five hours
        // west of UTC moves first-of-period bars into the previous bucket.
        assert!(regrouped_history > 0);
    }
}
