//! The engine-backed `ta.*` function tags, their operand/result shapes,
//! and the const-parameter validation rules.
//!
//! These live here rather than in `pinewasm_hir` (which re-exports them unchanged)
//! so the engine in [`crate::engine`] and the front end can share one
//! definition: the same tags drive the checker, the CSIL validator, the
//! interpreter, and the copy of the engine compiled into every
//! emitted artifact. A tag that existed in two places would eventually mean
//! two different functions.

/// Stateful built-in functions (`ta.*`, `math.sum`, `fixnan`).
///
/// Two lowering strategies coexist (see `docs/ir.md`):
/// - [`TaFunc::Change`] and [`TaFunc::Sma`] are **slot-based**: each call
///   site owns a hidden series slot that materializes the source bar by
///   bar, and the value is computed from that history (`ta.change` is a
///   store + two reads + a subtraction; `ta.sma` is a store + the
///   `WINDOW_MEAN` opcode).
/// - Every other variant is **engine-based**: each call site owns a
///   `TaStateDecl` (a `TaStateId` into the program's `ta_states` table)
///   and lowers to the single `CALL_TA` opcode; the VM keeps the
///   per-call-site state (ring buffers / scalars) itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[repr(u16)]
pub enum TaFunc {
    /// `ta.change(source, length = 1)` — source now minus source `length`
    /// bars ago; `na` when either side is `na` (warm-up: the first `length`
    /// bars are `na`).
    Change,
    /// `ta.sma(source, length)` — arithmetic mean of the last `length`
    /// values of source; `na` while the window is incomplete or when any
    /// value in the window is `na`.
    Sma,
    /// `fixnan(source)` — `source` with `na` replaced by the last non-`na`
    /// value seen at this call site (`na` until one exists).
    Fixnan,
    /// `math.sum(source, length)` — sliding sum of the last `length` values.
    MathSum,
    /// `ta.ema(source, length)` — exponential MA, `alpha = 2 / (length + 1)`.
    Ema,
    /// `ta.rma(source, length)` — Wilder's smoothing, `alpha = 1 / length`.
    Rma,
    /// `ta.wma(source, length)` — linearly weighted MA.
    Wma,
    /// `ta.vwma(source, length)` — volume-weighted MA:
    /// `sma(source * volume, length) / sma(volume, length)`.
    Vwma,
    /// `ta.hma(source, length)` — Hull MA:
    /// `wma(2*wma(source, length/2) - wma(source, length), round(sqrt(length)))`.
    Hma,
    /// `ta.swma(source)` — symmetric 4-bar weighted MA (weights 1/6, 2/6, 2/6, 1/6).
    Swma,
    /// `ta.alma(series, length, offset, sigma)` — Arnaud Legoux MA.
    Alma,
    /// `ta.rsi(source, length)` — relative strength index over `ta.rma`.
    Rsi,
    /// `ta.atr(length)` — `rma` of true range (no source operand).
    Atr,
    /// `ta.tr(handle_na)` — true range (no source operand, no state).
    Tr,
    /// `ta.stdev(source, length, biased = true)`.
    Stdev,
    /// `ta.dev(source, length)` — mean absolute deviation from the mean.
    Dev,
    /// `ta.variance(source, length, biased = true)`.
    Variance,
    /// `ta.highest(source, length)`.
    Highest,
    /// `ta.lowest(source, length)`.
    Lowest,
    /// `ta.highestbars(source, length)` — offset (≤ 0) of the highest bar, `int`.
    HighestBars,
    /// `ta.lowestbars(source, length)` — offset (≤ 0) of the lowest bar, `int`.
    LowestBars,
    /// `ta.mom(source, length)` — `source - source[length]`.
    Mom,
    /// `ta.roc(source, length)` — `100 * (source - source[length]) / source[length]`.
    Roc,
    /// `ta.cum(source)` — cumulative (running) sum.
    Cum,
    /// `ta.max(source)` — the all-time (running) maximum of `source` over
    /// every bar so far; `na` inputs are ignored (the running max is
    /// untouched) and the result is `na` until the first non-`na` input.
    Max,
    /// `ta.min(source)` — the all-time (running) minimum of `source` over
    /// every bar so far; `na` inputs are ignored and the result is `na`
    /// until the first non-`na` input.
    Min,
    /// `ta.crossover(source1, source2)` — bool.
    Crossover,
    /// `ta.crossunder(source1, source2)` — bool.
    Crossunder,
    /// `ta.cross(source1, source2)` — bool.
    Cross,
    /// `ta.barssince(condition)` — bars since the condition was true, `int`.
    BarsSince,
    /// `ta.valuewhen(condition, source, occurrence)`.
    ValueWhen,
    /// `ta.linreg(source, length, offset)` — least-squares regression value.
    Linreg,
    /// `ta.rising(source, length)` — `bool`: source is greater than every
    /// one of the previous `length` values.
    Rising,
    /// `ta.falling(source, length)` — `bool`: source is less than every one
    /// of the previous `length` values.
    Falling,
    /// `ta.cci(source, length)` — `(source - sma) / (0.015 * dev)`.
    Cci,
    /// `ta.cmo(source, length)` — Chande momentum oscillator:
    /// `100 * (sum_up - sum_down) / (sum_up + sum_down)` over the last
    /// `length` bar-to-bar changes.
    Cmo,
    /// `ta.cog(source, length)` — center of gravity:
    /// `-Σ source[i]·(i+1) / Σ source[i]` (i = 0 the newest bar).
    Cog,
    /// `ta.correlation(source1, source2, length)` — Pearson correlation
    /// coefficient over the window (two runtime source operands).
    Correlation,
    /// `ta.median(source, length)` — median of the window.
    Median,
    /// `ta.mode(source, length)` — most frequent window value; the engine
    /// resolves ties to the smallest value.
    Mode,
    /// `ta.percentile_linear_interpolation(source, length, percentage)`.
    PercentileLinearInterpolation,
    /// `ta.percentile_nearest_rank(source, length, percentage)`.
    PercentileNearestRank,
    /// `ta.percentrank(source, length)` — percent of the previous `length`
    /// values that are `<=` the current value.
    PercentRank,
    /// `ta.range(source, length)` — window max minus window min.
    Range,
    /// `ta.mfi(source, length)` — money flow index (reads volume).
    Mfi,
    /// `ta.wpr(length)` — Williams %R (reads high/low/close, no source).
    Wpr,
    /// `ta.obv` — on-balance volume (variable-style, no operands).
    Obv,
    /// `ta.accdist` — accumulation/distribution index (variable-style).
    AccDist,
    /// `ta.nvi` — negative volume index (variable-style).
    Nvi,
    /// `ta.pvi` — positive volume index (variable-style).
    Pvi,
    /// `ta.pvt` — price-volume trend (variable-style).
    Pvt,
    /// `ta.wad` — Williams accumulation/distribution (variable-style).
    Wad,
    /// `ta.wvad` — Williams variable accumulation/distribution
    /// (variable-style, per-bar).
    Wvad,
    /// `ta.iii` — intraday intensity index (variable-style, per-bar).
    Iii,
    /// `ta.vwap` — the bare **variable** form: volume-weighted average of
    /// `hlc3`, cumulative from bar 0 (no operand, empty signature).
    Vwap,
    /// `ta.vwap(source)` — volume-weighted average price of the supplied
    /// source, cumulative from bar 0.
    VwapSource,
    /// `ta.vwap(source, anchor)` — reset cumulative VWAP when the anchor
    /// condition is true, then include the anchor bar in the new segment.
    VwapAnchored,
    /// `ta.vwap(source, anchor, stdev_mult)` →
    /// `[vwap, upper_band, lower_band]`, where the bands use the
    /// volume-weighted population standard deviation of the active segment.
    /// Tuple-returning (arity 3).
    VwapBands,
    /// `ta.stoch(source, high, low, length)` — stochastic %K.
    Stoch,
    /// `ta.sar(start, inc, max)` — parabolic SAR (reads high/low/close).
    Sar,
    /// `ta.tsi(source, short_length, long_length)` — true strength index in
    /// [-1, 1] (double ema smoothing of the bar-to-bar change).
    Tsi,
    /// `ta.rci(source, length)` — rank correlation index in [-100, 100]
    /// (Spearman's rank correlation against the bar index).
    Rci,
    /// `ta.bbw(source, length, mult)` — Bollinger Bands width.
    Bbw,
    /// `ta.kcw(source, length, mult, useTrueRange = true)` — Keltner
    /// Channels width as the ratio `(upper - lower) / basis`.
    Kcw,
    /// `ta.pivothigh(source = high, leftbars, rightbars)` — pivot-high
    /// price, reported on the confirmation bar.
    PivotHigh,
    /// `ta.pivotlow(source = low, leftbars, rightbars)` — pivot-low price,
    /// reported on the confirmation bar.
    PivotLow,
    /// `ta.macd(source, fast_length, slow_length, signal_length)` →
    /// `[macdLine, signalLine, histLine]`. `macdLine = ema(source, fast) −
    /// ema(source, slow)`, `signalLine = ema(macdLine, signal)`,
    /// `histLine = macdLine − signalLine`. Tuple-returning (arity 3).
    Macd,
    /// `ta.bb(source, length, mult)` → `[middle, upper, lower]`.
    /// `middle = sma(source, length)`, `dev = mult · stdev(source, length)`
    /// (population), `upper = middle + dev`, `lower = middle − dev`.
    /// Tuple-returning (arity 3).
    Bb,
    /// `ta.kc(source, length, mult, useTrueRange = true)` →
    /// `[middle, upper, lower]`. `middle = ema(source, length)`,
    /// `range = ema(useTrueRange ? tr : high − low, length)`,
    /// `upper = middle + range · mult`, `lower = middle − range · mult`.
    /// Tuple-returning (arity 3).
    Kc,
    /// `ta.dmi(di_length, adx_smoothing)` → `[+DI, −DI, ADX]`. Wilder's
    /// directional movement index (reads high/low/close, no source operand).
    /// Tuple-returning (arity 3).
    Dmi,
    /// `ta.supertrend(factor, atr_period)` → `[supertrend, direction]`. The
    /// ATR trailing stop over `atr = ta.atr(atr_period)` and the `hl2`
    /// source (reads high/low/close, no source operand). `direction` is `-1`
    /// in an uptrend and `+1` in a downtrend.
    /// Tuple-returning (arity 2).
    Supertrend,
    /// `timeframe.change(timeframe)` — `bool`: `true` on the first chart bar
    /// of each new higher-timeframe bucket. The `timeframe` argument is a
    /// const string; the compiler bakes its length in seconds as the single
    /// const param. Bucketing is epoch-anchored fixed-width (`floor(time_ms /
    /// (tf_seconds · 1000))`); calendar-aligned week and month starts are not
    /// implemented. Stateful per call site (remembers the previous bar's
    /// bucket id). No runtime operands.
    TimeframeChange,
    /// `ta.valuewhen` with an `int` source; shares the float engine state but
    /// preserves the source's result type at the bytecode boundary.
    ValueWhenInt,
    /// `fixnan` with an `int` source; shares the numeric engine state while
    /// retaining the source type at the bytecode boundary.
    FixnanInt,
    /// Integer-source overloads preserve their declared result type while
    /// sharing the same numeric window kernels as the float forms.
    MedianInt,
    ModeInt,
    RangeInt,
    /// `ta.valuewhen` with a `bool` source, encoded as numeric 0/1 internally.
    ValueWhenBool,
    /// `ta.valuewhen` with a `color` source, encoded as exact packed RGBT.
    ValueWhenColor,
}

/// Runtime operand kind of one popped `CALL_TA` operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum TaOperand {
    /// A numeric source (float on the stack; the compiler coerces ints).
    Num,
    /// A boolean condition (`ta.barssince`, `ta.valuewhen`).
    Cond,
    /// Boolean value source (`ta.valuewhen`), separate from a condition.
    Bool,
    /// Color value source (`ta.valuewhen`).
    Color,
}

#[derive(Clone, Copy)]
enum EngineOperandShape {
    None,
    Num,
    NumCond,
    NumCondNum,
    NumNum,
    NumNumNum,
    Cond,
    CondNum,
    CondBool,
    CondColor,
}

/// Result type a `CALL_TA` instruction pushes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum TaResult {
    Float,
    Int,
    Bool,
    Color,
}

/// Validation rule for one baked-in const parameter of a `TaStateDecl`.
///
/// Consumed solely by the CSIL validator (`pinewasm_csil::validate`), which
/// re-checks the *already-lowered* const params as defense in depth. It
/// intentionally overlaps with — but stays independent of — the semantics
/// registry's `TaParamKind`, the richer front-end taxonomy that drives
/// argument checking and diagnostics: the bytecode verifier must not trust
/// the front end, so the two rule sets are maintained separately by design.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaParamRule {
    /// A window length: `min ..= 100000` (`TA_LENGTH_RANGE`'s upper bound).
    Length { min: i64 },
    /// A plain const int in an inclusive range (`ta.valuewhen` occurrence,
    /// `ta.linreg` offset).
    IntRange { min: i64, max: i64 },
    /// An `f64` stored as its raw bit pattern (`f64::to_bits` as `i64`);
    /// must decode to a finite float, strictly positive when `positive`.
    FloatBits { positive: bool },
    /// A percentage stored as an `f64` bit pattern; must decode to a finite
    /// float within `0.0 ..= 100.0` (the `ta.percentile_*` percentage).
    Percent,
    /// A boolean stored as 0 or 1.
    Bool01,
}

/// Owned identities for the engine parameter-rule sequences.
/// This representation lets extraction tooling avoid a returned static borrow;
/// the public table API still returns static slices without allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaParamRuleSet {
    Empty,
    Length,
    Hma,
    Alma,
    Bool,
    LengthBool,
    Occurrence,
    LengthOccurrence,
    LengthPercent,
    Sar,
    TwoLengths,
    LengthFloat,
    LengthFloatBool,
    TwoOccurrences,
    ThreeLengths,
    FloatLength,
    Timeframe,
}

impl TaParamRuleSet {
    fn as_slice(self) -> &'static [TaParamRule] {
        const LEN: TaParamRule = TaParamRule::Length { min: 1 };
        match self {
            Self::Empty => &[],
            Self::Length => &[LEN],
            Self::Hma => &[TaParamRule::Length { min: 2 }],
            Self::Alma => &[
                LEN,
                TaParamRule::FloatBits { positive: false },
                TaParamRule::FloatBits { positive: true },
                TaParamRule::Bool01,
            ],
            Self::Bool => &[TaParamRule::Bool01],
            Self::LengthBool => &[LEN, TaParamRule::Bool01],
            Self::Occurrence => &[TaParamRule::IntRange {
                min: 0,
                max: 100_000,
            }],
            Self::LengthOccurrence => &[
                LEN,
                TaParamRule::IntRange {
                    min: 0,
                    max: 100_000,
                },
            ],
            Self::LengthPercent => &[LEN, TaParamRule::Percent],
            Self::Sar => &[
                TaParamRule::FloatBits { positive: true },
                TaParamRule::FloatBits { positive: true },
                TaParamRule::FloatBits { positive: true },
            ],
            Self::TwoLengths => &[LEN, LEN],
            Self::LengthFloat => &[LEN, TaParamRule::FloatBits { positive: false }],
            Self::LengthFloatBool => &[
                LEN,
                TaParamRule::FloatBits { positive: false },
                TaParamRule::Bool01,
            ],
            Self::TwoOccurrences => &[
                TaParamRule::IntRange {
                    min: 0,
                    max: 100_000,
                },
                TaParamRule::IntRange {
                    min: 0,
                    max: 100_000,
                },
            ],
            Self::ThreeLengths => &[LEN, LEN, LEN],
            Self::FloatLength => &[TaParamRule::FloatBits { positive: false }, LEN],
            Self::Timeframe => &[TaParamRule::IntRange {
                min: 1,
                max: i64::MAX,
            }],
        }
    }

    pub(crate) fn rule_at(self, index: u32) -> Option<TaParamRule> {
        const LEN: TaParamRule = TaParamRule::Length { min: 1 };
        match self {
            Self::Empty => None,
            Self::Length => match index {
                0 => Some(LEN),
                _ => None,
            },
            Self::Hma => match index {
                0 => Some(TaParamRule::Length { min: 2 }),
                _ => None,
            },
            Self::Alma => match index {
                0 => Some(LEN),
                1 => Some(TaParamRule::FloatBits { positive: false }),
                2 => Some(TaParamRule::FloatBits { positive: true }),
                3 => Some(TaParamRule::Bool01),
                _ => None,
            },
            Self::Bool => match index {
                0 => Some(TaParamRule::Bool01),
                _ => None,
            },
            Self::LengthBool => match index {
                0 => Some(LEN),
                1 => Some(TaParamRule::Bool01),
                _ => None,
            },
            Self::Occurrence => match index {
                0 => Some(TaParamRule::IntRange {
                    min: 0,
                    max: 100_000,
                }),
                _ => None,
            },
            Self::LengthOccurrence => match index {
                0 => Some(LEN),
                1 => Some(TaParamRule::IntRange {
                    min: 0,
                    max: 100_000,
                }),
                _ => None,
            },
            Self::LengthPercent => match index {
                0 => Some(LEN),
                1 => Some(TaParamRule::Percent),
                _ => None,
            },
            Self::Sar => match index {
                0 => Some(TaParamRule::FloatBits { positive: true }),
                1 => Some(TaParamRule::FloatBits { positive: true }),
                2 => Some(TaParamRule::FloatBits { positive: true }),
                _ => None,
            },
            Self::TwoLengths => match index {
                0 => Some(LEN),
                1 => Some(LEN),
                _ => None,
            },
            Self::LengthFloat => match index {
                0 => Some(LEN),
                1 => Some(TaParamRule::FloatBits { positive: false }),
                _ => None,
            },
            Self::LengthFloatBool => match index {
                0 => Some(LEN),
                1 => Some(TaParamRule::FloatBits { positive: false }),
                2 => Some(TaParamRule::Bool01),
                _ => None,
            },
            Self::TwoOccurrences => match index {
                0 => Some(TaParamRule::IntRange {
                    min: 0,
                    max: 100_000,
                }),
                1 => Some(TaParamRule::IntRange {
                    min: 0,
                    max: 100_000,
                }),
                _ => None,
            },
            Self::ThreeLengths => match index {
                0 => Some(LEN),
                1 => Some(LEN),
                2 => Some(LEN),
                _ => None,
            },
            Self::FloatLength => match index {
                0 => Some(TaParamRule::FloatBits { positive: false }),
                1 => Some(LEN),
                _ => None,
            },
            Self::Timeframe => match index {
                0 => Some(TaParamRule::IntRange {
                    min: 1,
                    max: i64::MAX,
                }),
                _ => None,
            },
        }
    }
}

impl TaFunc {
    pub fn name(self) -> &'static str {
        match self {
            TaFunc::Change => "ta.change",
            TaFunc::Sma => "ta.sma",
            TaFunc::Fixnan | TaFunc::FixnanInt => "fixnan",
            TaFunc::MathSum => "math.sum",
            TaFunc::Ema => "ta.ema",
            TaFunc::Rma => "ta.rma",
            TaFunc::Wma => "ta.wma",
            TaFunc::Vwma => "ta.vwma",
            TaFunc::Hma => "ta.hma",
            TaFunc::Swma => "ta.swma",
            TaFunc::Alma => "ta.alma",
            TaFunc::Rsi => "ta.rsi",
            TaFunc::Atr => "ta.atr",
            TaFunc::Tr => "ta.tr",
            TaFunc::Stdev => "ta.stdev",
            TaFunc::Dev => "ta.dev",
            TaFunc::Variance => "ta.variance",
            TaFunc::Highest => "ta.highest",
            TaFunc::Lowest => "ta.lowest",
            TaFunc::HighestBars => "ta.highestbars",
            TaFunc::LowestBars => "ta.lowestbars",
            TaFunc::Mom => "ta.mom",
            TaFunc::Roc => "ta.roc",
            TaFunc::Cum => "ta.cum",
            TaFunc::Max => "ta.max",
            TaFunc::Min => "ta.min",
            TaFunc::Crossover => "ta.crossover",
            TaFunc::Crossunder => "ta.crossunder",
            TaFunc::Cross => "ta.cross",
            TaFunc::BarsSince => "ta.barssince",
            TaFunc::ValueWhen
            | TaFunc::ValueWhenInt
            | TaFunc::ValueWhenBool
            | TaFunc::ValueWhenColor => "ta.valuewhen",
            TaFunc::Linreg => "ta.linreg",
            TaFunc::Rising => "ta.rising",
            TaFunc::Falling => "ta.falling",
            TaFunc::Cci => "ta.cci",
            TaFunc::Cmo => "ta.cmo",
            TaFunc::Cog => "ta.cog",
            TaFunc::Correlation => "ta.correlation",
            TaFunc::Median => "ta.median",
            TaFunc::MedianInt => "ta.median",
            TaFunc::Mode => "ta.mode",
            TaFunc::ModeInt => "ta.mode",
            TaFunc::PercentileLinearInterpolation => "ta.percentile_linear_interpolation",
            TaFunc::PercentileNearestRank => "ta.percentile_nearest_rank",
            TaFunc::PercentRank => "ta.percentrank",
            TaFunc::Range => "ta.range",
            TaFunc::RangeInt => "ta.range",
            TaFunc::Mfi => "ta.mfi",
            TaFunc::Wpr => "ta.wpr",
            TaFunc::Obv => "ta.obv",
            TaFunc::AccDist => "ta.accdist",
            TaFunc::Nvi => "ta.nvi",
            TaFunc::Pvi => "ta.pvi",
            TaFunc::Pvt => "ta.pvt",
            TaFunc::Wad => "ta.wad",
            TaFunc::Wvad => "ta.wvad",
            TaFunc::Iii => "ta.iii",
            TaFunc::Vwap => "ta.vwap",
            TaFunc::VwapSource => "ta.vwap",
            TaFunc::VwapAnchored => "ta.vwap",
            TaFunc::VwapBands => "ta.vwap",
            TaFunc::Stoch => "ta.stoch",
            TaFunc::Sar => "ta.sar",
            TaFunc::Tsi => "ta.tsi",
            TaFunc::Rci => "ta.rci",
            TaFunc::Bbw => "ta.bbw",
            TaFunc::Kcw => "ta.kcw",
            TaFunc::PivotHigh => "ta.pivothigh",
            TaFunc::PivotLow => "ta.pivotlow",
            TaFunc::Macd => "ta.macd",
            TaFunc::Bb => "ta.bb",
            TaFunc::Kc => "ta.kc",
            TaFunc::Dmi => "ta.dmi",
            TaFunc::Supertrend => "ta.supertrend",
            TaFunc::TimeframeChange => "timeframe.change",
        }
    }

    /// Number of values this function pushes onto the stack when its
    /// `CALL_TA` executes. Scalar functions push 1; the arity-3 tuple
    /// functions (`ta.macd`, `ta.bb`, `ta.kc`, `ta.dmi`) push 3 and the
    /// arity-2 `ta.supertrend` pushes 2, left to right (the first tuple
    /// element is pushed first, so it is deepest on the stack; the last
    /// element is on top). Destructuring pops them in reverse, storing each
    /// into its target slot (see `docs/ir.md`).
    pub fn engine_result_arity(self) -> u32 {
        match self {
            TaFunc::Macd | TaFunc::Bb | TaFunc::Kc | TaFunc::Dmi | TaFunc::VwapBands => 3,
            TaFunc::Supertrend => 2,
            _ => 1,
        }
    }

    /// True for the tuple-returning engine functions (result arity above 1).
    /// A tuple function may appear **only** as the RHS of a destructuring
    /// declaration (`[a, b, c] = ta.macd(...)`); using it as a scalar value is
    /// a checker error.
    pub fn is_tuple(self) -> bool {
        self.engine_result_arity() > 1
    }

    /// True for the VM-engine functions (backed by `CALL_TA` and a
    /// `TaStateDecl`); false for the two slot-based functions
    /// (`ta.change`, `ta.sma`), which keep their original lowering.
    pub fn is_engine(self) -> bool {
        !matches!(self, TaFunc::Change | TaFunc::Sma)
    }

    fn engine_operand_shape(self) -> EngineOperandShape {
        use EngineOperandShape::{
            Cond, CondBool, CondColor, CondNum, None, Num, NumCond, NumCondNum, NumNum, NumNumNum,
        };
        match self {
            // No runtime operands: computed from market data / const params.
            TaFunc::Atr
            | TaFunc::Tr
            | TaFunc::Wpr
            | TaFunc::Sar
            // ta.dmi/ta.supertrend read high/low/close directly (like
            // ta.atr): no operand.
            | TaFunc::Dmi
            | TaFunc::Supertrend
            | TaFunc::Obv
            | TaFunc::AccDist
            | TaFunc::Nvi
            | TaFunc::Pvi
            | TaFunc::Pvt
            | TaFunc::Wad
            | TaFunc::Wvad
            | TaFunc::Iii
            // timeframe.change reads the time axis + baked tf-seconds param.
            | TaFunc::TimeframeChange
            | TaFunc::Vwap => None,
            TaFunc::VwapSource => Num,
            TaFunc::VwapAnchored => NumCond,
            TaFunc::VwapBands => NumCondNum,
            TaFunc::Crossover | TaFunc::Crossunder | TaFunc::Cross | TaFunc::Correlation => {
                NumNum
            }
            TaFunc::Stoch => NumNumNum,
            TaFunc::BarsSince => Cond,
            TaFunc::ValueWhen | TaFunc::ValueWhenInt => CondNum,
            TaFunc::ValueWhenBool => CondBool,
            TaFunc::ValueWhenColor => CondColor,
            _ => Num,
        }
    }

    /// Runtime operands `CALL_TA` pops for this function, in **argument
    /// order** (the last one is on top of the stack). The checker, the CSIL
    /// validator, and the VM all key off this table.
    pub fn engine_operands(self) -> &'static [TaOperand] {
        use EngineOperandShape as Shape;
        use TaOperand::{Bool, Color, Cond, Num};
        match self.engine_operand_shape() {
            Shape::None => &[],
            Shape::Num => &[Num],
            Shape::NumCond => &[Num, Cond],
            Shape::NumCondNum => &[Num, Cond, Num],
            Shape::NumNum => &[Num, Num],
            Shape::NumNumNum => &[Num, Num, Num],
            Shape::Cond => &[Cond],
            Shape::CondNum => &[Cond, Num],
            Shape::CondBool => &[Cond, Bool],
            Shape::CondColor => &[Cond, Color],
        }
    }

    /// Number of runtime operands consumed by this TA function.
    #[doc(hidden)]
    pub fn engine_operand_count(self) -> usize {
        use EngineOperandShape as Shape;
        match self.engine_operand_shape() {
            Shape::None => 0,
            Shape::Num | Shape::Cond => 1,
            Shape::NumCond
            | Shape::NumNum
            | Shape::CondNum
            | Shape::CondBool
            | Shape::CondColor => 2,
            Shape::NumCondNum | Shape::NumNumNum => 3,
        }
    }

    /// Result type `CALL_TA` pushes for this function.
    pub fn engine_result(self) -> TaResult {
        match self {
            TaFunc::HighestBars
            | TaFunc::LowestBars
            | TaFunc::BarsSince
            | TaFunc::ValueWhenInt
            | TaFunc::FixnanInt
            | TaFunc::MedianInt
            | TaFunc::ModeInt
            | TaFunc::RangeInt => TaResult::Int,
            TaFunc::Crossover
            | TaFunc::Crossunder
            | TaFunc::Cross
            | TaFunc::Rising
            | TaFunc::Falling
            | TaFunc::TimeframeChange => TaResult::Bool,
            TaFunc::ValueWhenBool => TaResult::Bool,
            TaFunc::ValueWhenColor => TaResult::Color,
            _ => TaResult::Float,
        }
    }

    /// Validation rules for the const parameters baked into this function's
    /// `TaStateDecl`, in `params` order. Empty for `Change`/`Sma` (they
    /// are never engine states).
    pub fn engine_param_rules(self) -> &'static [TaParamRule] {
        self.engine_param_rule_set().as_slice()
    }

    /// The owned rule at one engine parameter index, or `None` when absent.
    /// This agrees with indexing [`Self::engine_param_rules`] without exposing
    /// a static borrow to callers or allocating a rule sequence.
    #[inline]
    pub fn engine_param_rule(self, index: u32) -> Option<TaParamRule> {
        self.engine_param_rule_set().rule_at(index)
    }

    /// Owned classification of the unchanged static rule table.
    pub(crate) fn engine_param_rule_set(self) -> TaParamRuleSet {
        match self {
            TaFunc::Change | TaFunc::Sma => TaParamRuleSet::Empty,
            TaFunc::Fixnan
            | TaFunc::FixnanInt
            | TaFunc::Swma
            | TaFunc::Cum
            | TaFunc::Max
            | TaFunc::Min
            | TaFunc::Crossover
            | TaFunc::Crossunder
            | TaFunc::Cross
            | TaFunc::BarsSince
            | TaFunc::Obv
            | TaFunc::AccDist
            | TaFunc::Nvi
            | TaFunc::Pvi
            | TaFunc::Pvt
            | TaFunc::Wad
            | TaFunc::Wvad
            | TaFunc::Iii
            | TaFunc::Vwap
            | TaFunc::VwapSource
            | TaFunc::VwapAnchored
            | TaFunc::VwapBands => TaParamRuleSet::Empty,
            TaFunc::MathSum
            | TaFunc::Ema
            | TaFunc::Rma
            | TaFunc::Wma
            | TaFunc::Vwma
            | TaFunc::Rsi
            | TaFunc::Atr
            | TaFunc::Dev
            | TaFunc::Highest
            | TaFunc::Lowest
            | TaFunc::HighestBars
            | TaFunc::LowestBars
            | TaFunc::Mom
            | TaFunc::Roc
            | TaFunc::Rising
            | TaFunc::Falling
            | TaFunc::Cci
            | TaFunc::Cmo
            | TaFunc::Cog
            | TaFunc::Correlation
            | TaFunc::Median
            | TaFunc::MedianInt
            | TaFunc::Mode
            | TaFunc::ModeInt
            | TaFunc::PercentRank
            | TaFunc::Range
            | TaFunc::RangeInt
            | TaFunc::Mfi
            | TaFunc::Wpr
            | TaFunc::Stoch
            | TaFunc::Rci => TaParamRuleSet::Length,

            TaFunc::Hma => TaParamRuleSet::Hma,
            TaFunc::Alma => TaParamRuleSet::Alma,
            TaFunc::Tr => TaParamRuleSet::Bool,
            TaFunc::Stdev | TaFunc::Variance => TaParamRuleSet::LengthBool,
            TaFunc::ValueWhen
            | TaFunc::ValueWhenInt
            | TaFunc::ValueWhenBool
            | TaFunc::ValueWhenColor => TaParamRuleSet::Occurrence,
            TaFunc::Linreg => TaParamRuleSet::LengthOccurrence,
            TaFunc::PercentileLinearInterpolation | TaFunc::PercentileNearestRank => {
                TaParamRuleSet::LengthPercent
            }

            TaFunc::Sar => TaParamRuleSet::Sar,

            TaFunc::Tsi => TaParamRuleSet::TwoLengths,
            TaFunc::Bbw => TaParamRuleSet::LengthFloat,
            TaFunc::Kcw => TaParamRuleSet::LengthFloatBool,

            TaFunc::PivotHigh | TaFunc::PivotLow => TaParamRuleSet::TwoOccurrences,

            TaFunc::Macd => TaParamRuleSet::ThreeLengths,

            TaFunc::Bb => TaParamRuleSet::LengthFloat,

            TaFunc::Kc => TaParamRuleSet::LengthFloatBool,

            TaFunc::Dmi => TaParamRuleSet::TwoLengths,

            TaFunc::Supertrend => TaParamRuleSet::FloatLength,

            TaFunc::TimeframeChange => TaParamRuleSet::Timeframe,
        }
    }

    /// Instruction-budget cost of executing one `CALL_TA` for this function
    /// (see docs/ir.md). Functions that rescan a window cost the window
    /// length; O(1)-state/step functions cost 1. `params` is this state's
    /// (validated) `TaStateDecl::params`; the values are clamped defensively.
    pub fn engine_cost_kind(self) -> TaCost {
        match self {
            // Full window rescans: cost = window length. The sort-based
            // functions (`ta.median`, `ta.mode`, the percentiles, `ta.rci`)
            // are also metered as their window length — the sort's log
            // factor is absorbed into the unit (documented in docs/ir.md).
            TaFunc::MathSum
            | TaFunc::Wma
            | TaFunc::Vwma
            | TaFunc::Alma
            | TaFunc::Stdev
            | TaFunc::Dev
            | TaFunc::Variance
            | TaFunc::Highest
            | TaFunc::Lowest
            | TaFunc::HighestBars
            | TaFunc::LowestBars
            | TaFunc::Linreg
            | TaFunc::Cci
            | TaFunc::Cog
            | TaFunc::Correlation
            | TaFunc::Median
            | TaFunc::MedianInt
            | TaFunc::Mode
            | TaFunc::ModeInt
            | TaFunc::PercentileLinearInterpolation
            | TaFunc::PercentileNearestRank
            | TaFunc::Range
            | TaFunc::RangeInt
            | TaFunc::Mfi
            | TaFunc::Wpr
            | TaFunc::Stoch
            | TaFunc::Rci
            // ta.bb rescans the window for its population stdev.
            | TaFunc::Bb
            | TaFunc::Bbw => TaCost::Length { extra: 0 },
            // Window of `length + 1` values (the current bar plus `length`
            // predecessors / changes).
            TaFunc::Cmo | TaFunc::PercentRank | TaFunc::Rising | TaFunc::Falling => TaCost::Length { extra: 1 },
            // Window of `leftbars + rightbars + 1` values.
            TaFunc::PivotHigh | TaFunc::PivotLow => TaCost::Pivots,
            // Fixed 4-bar window.
            TaFunc::Swma => TaCost::Fixed(4),
            // Three wma rescans: length + length/2 + round(sqrt(length)).
            TaFunc::Hma => TaCost::Hma,
            // O(1) state or O(1) work per executed instruction.
            _ => TaCost::Fixed(1),
        }
    }

    pub fn engine_cost(self, params: &[i64]) -> u64 {
        let length = |i: usize| params.get(i).copied().unwrap_or(1).clamp(1, 100_000) as u64;
        match self.engine_cost_kind() {
            TaCost::Fixed(cost) => cost,
            TaCost::Length { extra } => length(0) + extra,
            TaCost::Pivots => length(0) + length(1) + 1,
            TaCost::Hma => {
                let n = length(0);
                n + n / 2 + libm::round(libm::sqrt(n as f64)) as u64
            }
        }
    }
}

/// One instance of the tag table, for [`TaFunc::from_tag`] to index.
static TAGS: [TaFunc; TaFunc::COUNT as usize] = TaFunc::ALL;

/// Tag round-tripping, for the declaration table an artifact carries in a
/// data segment.
impl TaFunc {
    /// One past the highest discriminant. Derived from the last variant, so a
    /// variant appended after it makes [`TaFunc::from_tag`] reject its tag
    /// (an emit-time refusal) rather than silently mis-decoding — and the
    /// length of [`TaFunc::ALL`] is checked against it at compile time.
    pub const COUNT: u16 = TaFunc::ValueWhenColor as u16 + 1;

    /// Every variant, in discriminant order. `ALL[i] as u16 == i`, pinned by
    /// `tags_round_trip_in_declaration_order`.
    pub const ALL: [TaFunc; TaFunc::COUNT as usize] = [
        TaFunc::Change,
        TaFunc::Sma,
        TaFunc::Fixnan,
        TaFunc::MathSum,
        TaFunc::Ema,
        TaFunc::Rma,
        TaFunc::Wma,
        TaFunc::Vwma,
        TaFunc::Hma,
        TaFunc::Swma,
        TaFunc::Alma,
        TaFunc::Rsi,
        TaFunc::Atr,
        TaFunc::Tr,
        TaFunc::Stdev,
        TaFunc::Dev,
        TaFunc::Variance,
        TaFunc::Highest,
        TaFunc::Lowest,
        TaFunc::HighestBars,
        TaFunc::LowestBars,
        TaFunc::Mom,
        TaFunc::Roc,
        TaFunc::Cum,
        TaFunc::Max,
        TaFunc::Min,
        TaFunc::Crossover,
        TaFunc::Crossunder,
        TaFunc::Cross,
        TaFunc::BarsSince,
        TaFunc::ValueWhen,
        TaFunc::Linreg,
        TaFunc::Rising,
        TaFunc::Falling,
        TaFunc::Cci,
        TaFunc::Cmo,
        TaFunc::Cog,
        TaFunc::Correlation,
        TaFunc::Median,
        TaFunc::Mode,
        TaFunc::PercentileLinearInterpolation,
        TaFunc::PercentileNearestRank,
        TaFunc::PercentRank,
        TaFunc::Range,
        TaFunc::Mfi,
        TaFunc::Wpr,
        TaFunc::Obv,
        TaFunc::AccDist,
        TaFunc::Nvi,
        TaFunc::Pvi,
        TaFunc::Pvt,
        TaFunc::Wad,
        TaFunc::Wvad,
        TaFunc::Iii,
        TaFunc::Vwap,
        TaFunc::VwapSource,
        TaFunc::VwapAnchored,
        TaFunc::VwapBands,
        TaFunc::Stoch,
        TaFunc::Sar,
        TaFunc::Tsi,
        TaFunc::Rci,
        TaFunc::Bbw,
        TaFunc::Kcw,
        TaFunc::PivotHigh,
        TaFunc::PivotLow,
        TaFunc::Macd,
        TaFunc::Bb,
        TaFunc::Kc,
        TaFunc::Dmi,
        TaFunc::Supertrend,
        TaFunc::TimeframeChange,
        TaFunc::ValueWhenInt,
        TaFunc::FixnanInt,
        TaFunc::MedianInt,
        TaFunc::ModeInt,
        TaFunc::RangeInt,
        TaFunc::ValueWhenBool,
        TaFunc::ValueWhenColor,
    ];

    /// The wire tag: `#[repr(u16)]` makes this the discriminant itself.
    pub fn tag(self) -> u16 {
        self as u16
    }

    /// The variant a tag names, or `None` when it names none.
    ///
    /// Reads the `static`, not [`TaFunc::ALL`]: a `const` array is
    /// materialized fresh at every use site, and this runs once per ta call
    /// per bar.
    pub fn from_tag(tag: u16) -> Option<TaFunc> {
        TAGS.get(tag as usize).copied()
    }
}

/// Instruction cost formula shared with native emitters for runtime parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaCost {
    Fixed(u64),
    Length { extra: u64 },
    Pivots,
    Hma,
}
