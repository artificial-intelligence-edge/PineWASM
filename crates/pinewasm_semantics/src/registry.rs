//! Data-driven registry of built-in series, functions, and constants.
//!
//! The registry is the single source of truth the resolver consults; adding
//! a new builtin means adding a table entry, not touching the checker or the
//! VM core. Namespace coverage is tracked in `docs/pine-v6-coverage.md`.

use crate::inputs::input_kind_type;
use pinewasm_hir::{
    BuiltinSeries, CalendarField, CollectionFunc, InputKind, LogLevel, OrderKind, PureFunc,
    Qualifier, RequestFunc, TaFunc, TickerFunc, TradeAccessor, Type, ARRAY_FROM_MAX_ARGS,
    STRATEGY_DIRECTION_TYPE, STR_FORMAT_MAX_ARGS,
};
use std::collections::HashMap;
use std::sync::OnceLock;

/// Parameter kind of an **engine-backed** stateful builtin (`ta.ema`,
/// `fixnan`, `math.sum`, ...). Runtime operands (`Source`, `Condition`)
/// become `CALL_TA` stack operands; the const kinds are literals baked into
/// the call site's `TaStateDecl::params` (in signature order).
///
/// This is the *front-end* taxonomy: it drives argument checking and
/// diagnostics in the analyzer only. It deliberately does **not** share a
/// definition with `pinewasm_hir::TaParamRule`, the post-lowering rule set
/// the CSIL validator re-checks baked const params against — the bytecode
/// verifier is defense in depth and must not trust (or depend on) the
/// front end's richer, diagnostic-oriented view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TaParamKind {
    /// `series int/float` source, coerced to float.
    Source,
    /// An **optional** source that defaults to a builtin series when the
    /// argument is omitted (`ta.pivothigh`'s `high`, `ta.pivotlow`'s
    /// `low`). At the CSIL level the operand is always present: the checker
    /// materializes the default series read.
    SourceDefaulted(BuiltinSeries),
    /// `series bool` condition (`ta.barssince`, `ta.valuewhen`).
    Condition,
    /// A const int literal within `min..=max`, baked as-is; optional when
    /// `default` is set.
    ConstInt {
        min: i64,
        max: i64,
        default: Option<i64>,
    },
    /// A const numeric literal (int- or float-form), baked as `f64::to_bits`;
    /// must be finite, and strictly positive when `positive`. Optional (trailing)
    /// when `optional` — an omitted argument bakes no param (`ta.vwap`'s
    /// `stdev_mult`, whose presence selects the banded tuple overload).
    ConstFloat { positive: bool, optional: bool },
    /// A const numeric literal within `0 ..= 100` inclusive, baked as
    /// `f64::to_bits` (the `ta.percentile_*` percentage).
    ConstPercent,
    /// A const `true`/`false` literal, baked as 1/0; optional when `default`
    /// is set.
    ConstBool { default: Option<bool> },
    /// A const `timeframe` string literal (`timeframe.change`). The compiler
    /// bakes the timeframe's length in seconds (a positive `i64`) as the const
    /// param. Optional when `default` is set.
    ConstTimeframe { default: Option<i64> },
}

/// One parameter of an engine-backed stateful builtin.
#[derive(Debug, Clone)]
pub struct TaParam {
    pub name: &'static str,
    pub kind: TaParamKind,
}

impl TaParam {
    pub fn required(&self) -> bool {
        match self.kind {
            TaParamKind::ConstInt { default, .. } => default.is_none(),
            TaParamKind::ConstBool { default } => default.is_none(),
            TaParamKind::ConstTimeframe { default } => default.is_none(),
            TaParamKind::ConstFloat { optional, .. } => !optional,
            TaParamKind::SourceDefaulted(_) => false,
            _ => true,
        }
    }
}

/// The display-plot family: the functions that annotate the chart
/// with shapes, characters, arrows, custom OHLC bars/candles, a horizontal
/// level, or a background/bar color. Each is fully type-checked so scripts
/// using them compile and run. Where a value is data-representable, its
/// per-bar series is surfaced to the run output like `plot()`; the purely
/// visual glyph/shading is not rendered by the headless VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayKind {
    /// `plotshape(series bool|number, ...)` — booleans surface as `1.0`/`na`;
    /// numeric series keep their original CSV-visible values.
    PlotShape,
    /// `plotchar(series bool|number, ...)` — same value behavior as plotshape.
    PlotChar,
    /// `plotarrow(series int/float, ...)` — one data plot carrying the value.
    PlotArrow,
    /// `plotcandle(open, high, low, close, ...)` — four data plots (OHLC).
    PlotCandle,
    /// `plotbar(open, high, low, close, ...)` — four data plots (OHLC).
    PlotBar,
    /// `hline(price, ...)` — one data plot: the constant price on every bar.
    Hline,
    /// `bgcolor(color, ...)` — color-only; type-checked and evaluated, no data
    /// plot (the headless VM records the call but renders no shading).
    BgColor,
    /// `barcolor(color, ...)` — color-only; as `bgcolor`.
    BarColor,
    /// `fill(id1, id2, color, ...)` — references two plot/hline outputs the
    /// headless VM does not model; type-checked and evaluated, no data plot.
    Fill,
}

impl DisplayKind {
    /// The function name this display kind is invoked as.
    pub fn name(self) -> &'static str {
        match self {
            DisplayKind::PlotShape => "plotshape",
            DisplayKind::PlotChar => "plotchar",
            DisplayKind::PlotArrow => "plotarrow",
            DisplayKind::PlotCandle => "plotcandle",
            DisplayKind::PlotBar => "plotbar",
            DisplayKind::Hline => "hline",
            DisplayKind::BgColor => "bgcolor",
            DisplayKind::BarColor => "barcolor",
            DisplayKind::Fill => "fill",
        }
    }

    /// Canonical public parameter names in positional order.
    ///
    /// The display analyzer and generated editor metadata share this table so
    /// optional tail additions cannot silently drift between the compiler and
    /// editor and language-server signature help.
    pub fn param_names(self) -> &'static [&'static str] {
        match self {
            DisplayKind::PlotShape => &[
                "series",
                "title",
                "style",
                "location",
                "color",
                "offset",
                "text",
                "textcolor",
                "editable",
                "size",
                "show_last",
                "display",
                "format",
                "precision",
                "force_overlay",
            ],
            DisplayKind::PlotChar => &[
                "series",
                "title",
                "char",
                "location",
                "color",
                "offset",
                "text",
                "textcolor",
                "editable",
                "size",
                "show_last",
                "display",
                "format",
                "precision",
                "force_overlay",
            ],
            DisplayKind::PlotArrow => &[
                "series",
                "title",
                "colorup",
                "colordown",
                "offset",
                "minheight",
                "maxheight",
                "editable",
                "show_last",
                "display",
                "format",
                "precision",
                "force_overlay",
            ],
            DisplayKind::PlotCandle => &[
                "open",
                "high",
                "low",
                "close",
                "title",
                "color",
                "wickcolor",
                "editable",
                "show_last",
                "bordercolor",
                "display",
                "format",
                "precision",
                "force_overlay",
            ],
            DisplayKind::PlotBar => &[
                "open",
                "high",
                "low",
                "close",
                "title",
                "color",
                "editable",
                "show_last",
                "display",
                "format",
                "precision",
                "force_overlay",
            ],
            DisplayKind::Hline => &[
                "price",
                "title",
                "color",
                "linestyle",
                "linewidth",
                "editable",
                "display",
            ],
            DisplayKind::BgColor => &[
                "color",
                "offset",
                "editable",
                "show_last",
                "title",
                "display",
                "force_overlay",
            ],
            DisplayKind::BarColor => &[
                "color",
                "offset",
                "editable",
                "show_last",
                "title",
                "display",
            ],
            DisplayKind::Fill => &[
                "hline1", "hline2", "color", "title", "editable", "fillgaps", "display",
            ],
        }
    }

    /// Maximum qualifier of each published display argument. This table also
    /// covers the gradient fill overload's parameters, which are bound by its
    /// dedicated analyzer. Keep the checker and language-tooling metadata on
    /// the same contract; series-only metadata used to silently admit invalid
    /// per-bar widths, offsets, text and pane-routing options.
    pub fn param_qualifier(self, name: &str) -> Qualifier {
        if let Some(qualifier) = pinewasm_hir::display_style_binding_qualifier(name) {
            return qualifier;
        }
        match name {
            "title" => Qualifier::Const,
            "price" => Qualifier::Input,
            "color" if self == Self::Hline => Qualifier::Input,
            "series" | "open" | "high" | "low" | "close" | "color" | "colorup" | "colordown"
            | "textcolor" | "wickcolor" | "bordercolor" | "hline1" | "hline2" | "plot1"
            | "plot2" | "top_value" | "bottom_value" | "top_color" | "bottom_color" => {
                Qualifier::Series
            }
            _ => panic!("display parameter `{name}` has no qualifier contract"),
        }
    }

    /// Number of leading required parameters in [`Self::param_names`].
    pub fn required_count(self) -> usize {
        match self {
            DisplayKind::PlotCandle | DisplayKind::PlotBar => 4,
            DisplayKind::Fill => 2,
            _ => 1,
        }
    }

    /// Number of leading numeric/bool data arguments surfaced as plots.
    pub(crate) fn data_count(self) -> usize {
        match self {
            DisplayKind::PlotCandle | DisplayKind::PlotBar => 4,
            DisplayKind::PlotShape
            | DisplayKind::PlotChar
            | DisplayKind::PlotArrow
            | DisplayKind::Hline => 1,
            DisplayKind::BgColor | DisplayKind::BarColor | DisplayKind::Fill => 0,
        }
    }
}

/// What kind of builtin a function is. Effect functions (`plot`,
/// `indicator`) desugar to statements/metadata; `Ta` functions are
/// value-producing and stateful (one hidden series slot per call site);
/// `Pure` functions are value-producing and stateless (one `CallPure`
/// instruction, no hidden slots).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinFunctionKind {
    /// `plot(value, title?)` — records one output series sample per bar.
    Plot,
    /// `indicator(title)` — script metadata declaration.
    Indicator,
    /// `max_bars_back(series, length)` — requests a history ceiling of
    /// `length` bars: the depth a history offset computed at run time may
    /// reach. The call itself has no runtime effect and lowers to nothing; it
    /// only raises the ceiling carried in the program's metadata. Callable at
    /// any statement position.
    MaxBarsBack,
    /// A stateful `ta.*` function (`ta.change`, `ta.sma`, ...).
    Ta(TaFunc),
    /// A stateless builtin (`math.*`, `na()`, `nz()`).
    Pure(PureFunc),
    /// An `input.*` declaration (`input.int`, `input.source`, ...). Value-
    /// producing (an `input`-qualified value, or `series float` for
    /// `input.source`) but validated with its own dedicated rules (const
    /// arguments, kind-specific constraints); it registers an input in the
    /// program metadata rather than lowering to a call.
    Input(InputKind),
    /// The generic `input(defval, ...)` overload. It has no fixed
    /// [`InputKind`]: the analyzer dispatches on the `defval`'s literal type to
    /// the matching namespaced kind (int→`input.int`, float→`input.float`,
    /// bool→`input.bool`, string→`input.string`, color→`input.color`, a builtin
    /// source series→`input.source`) before falling into the shared
    /// `input.*` machinery. A semantics-level desugar; the host-override
    /// resolution is unchanged.
    InputGeneric,
    /// A reference-typed collection builtin (`array.*`). Value-producing with
    /// receiver-dependent result types (`array.get` returns the receiver's
    /// element type), so it is checked by a dedicated analyzer rather than the
    /// generic parameter framework. `params` is unused for these functions.
    Collection(CollectionFunc),
    /// A typed `map.new_<key>_<val>()` constructor. The parser rewrites the
    /// scalar `map.new<K, V>` source form to these names, so each valid
    /// key/value pairing is a distinct registry name whose kind carries the
    /// static key and value types (both scalar value types). Checked by the
    /// dedicated collection analyzer.
    MapConstructor { key: ScalarTy, val: ScalarTy },
    /// A typed `matrix.new_<T>(rows, columns, initial_value)` constructor. The
    /// parser rewrites the scalar `matrix.new<T>` source form to these names,
    /// so each element type is a distinct registry name whose kind carries the
    /// static element type. Unlike `map.new_*`, it takes runtime
    /// `(rows, columns, initial_value)` arguments, bound by position or by
    /// name; the analyzer materializes the element-type tag.
    MatrixConstructor { elem: ScalarTy },
    /// A `log.info/warning/error(formatString, arg0, ...)` call. Variadic
    /// like `str.format`: a required format string plus scalar substitution
    /// args. Records a `(level, bar, message)` event; produces void.
    Log(LogLevel),
    /// An `alert(message, freq?)` call. Records a `(bar, message, freq)`
    /// event honoring the `alert.freq_*` firing rules; produces void.
    Alert,
    /// An `alertcondition(condition, title?, message?)` call. Fires only
    /// on bars where `condition` is true; produces void.
    AlertCondition,
    /// A display-plot-family call: `plotshape`/`plotchar`/`plotarrow`/
    /// `plotcandle`/`plotbar`/`hline`/`bgcolor`/`barcolor`/`fill`. Fully
    /// type-checked; surfaces data-representable series to the run output and
    /// produces void. Checked by a dedicated analyzer.
    Display(DisplayKind),
    /// `runtime.error(message)`: aborts the current run with `message`
    /// as a structured user runtime error on the bar/branch where it executes.
    /// Produces void.
    RuntimeErrorFn,
    /// A `strategy.*` order primitive: `strategy.entry/order/exit/close/
    /// close_all/cancel/cancel_all`. Places an order the broker emulator fills;
    /// produces void. Only valid in a `strategy()` script (`E0241`). Checked by
    /// a dedicated analyzer because the argument surface is kind-specific.
    Order(OrderKind),
    /// `request.security(symbol, timeframe, expression, gaps?, lookahead?)`.
    /// Produces a `series float` by resampling the chart feed to a
    /// higher timeframe and evaluating `expression` over it. Checked by a
    /// dedicated analyzer (symbol/timeframe validation, `barmerge.*` mode
    /// mapping, plain-series `expression` restriction; see docs/runtime.md).
    Security,
    /// A scalar `request.*` external-data interface:
    /// `request.dividends/splits/earnings/financial/economic/currency_rate`.
    /// Produces a `series float` read from an optional per-run host table
    /// (default `na`). Checked by the dedicated `analyze_request_call` analyzer
    /// because each has a kind-specific argument surface; the constant string
    /// arguments are folded into a selector key. See docs/runtime.md.
    Request(RequestFunc),
    /// `request.security_lower_tf(symbol, timeframe, expression,
    /// ignore_invalid_symbol?, currency?, ignore_invalid_timeframe?,
    /// calc_bars_count?)`.
    /// Produces an `array<float>` of the intrabar values within each chart bar,
    /// read from an optional per-run host table (default empty). Checked by the
    /// dedicated `analyze_security_lower_tf_call` analyzer; this VM only
    /// aggregates UP, so intrabar decomposition is a host concern.
    SecurityLowerTf,
    /// `ta.pivot_point_levels(type, anchor)`. Produces an
    /// `array<float>` of pivot levels computed from the PRIOR completed anchor
    /// period's HLC. Checked by the dedicated `analyze_pivot_point_levels_call`
    /// analyzer (const `type` string → pivot system; const `anchor` timeframe
    /// string → seconds), which lowers to the `ReadPivotLevels` op. Modeled via
    /// the same resample machinery as `request.security` — not the ta engine —
    /// so it sidesteps the array-returning-stateful engine plumbing.
    PivotPointLevels,
    /// The FUNCTION forms `time(timeframe, session, timezone, bars_back,
    /// timeframe_bars_back)` / `time_close(...)` (`close` selects
    /// `time_close`). Produces a `series int` (epoch ms). Checked by the
    /// dedicated analyzer, which materializes the optional arguments and
    /// lowers to `PureFunc::TimeOpen` / `PureFunc::TimeClose`. The bare
    /// `time`/`time_close` *variables* remain separate `BuiltinSeries`.
    TimeFunction { close: bool },
}

/// A scalar value type usable as a `map` key or value. A small `Copy` mirror
/// of the scalar subset of [`Type`] so [`BuiltinFunctionKind`] stays `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarTy {
    Float,
    Int,
    Bool,
    String,
    Color,
}

impl ScalarTy {
    /// The corresponding HIR [`Type`].
    pub fn to_type(self) -> Type {
        match self {
            ScalarTy::Float => Type::Float,
            ScalarTy::Int => Type::Int,
            ScalarTy::Bool => Type::Bool,
            ScalarTy::String => Type::String,
            ScalarTy::Color => Type::Color,
        }
    }
}

/// The stable integer tag for a scalar element [`Type`], matching
/// `pinewasm_mir::heap::ElemType::to_tag`. Used to materialize a `map`'s
/// key/value types as `ConstInt` arguments the VM decodes. An enum value is
/// its variant index at runtime, so an enum element or key takes the `int`
/// tag; the nominal enum type stays on the HIR. Returns `None` for any other
/// non-scalar type.
pub fn scalar_type_tag(ty: &Type) -> Option<i64> {
    match ty {
        Type::Float => Some(0),
        Type::Int | Type::Enum(_) => Some(1),
        Type::Bool => Some(2),
        Type::String => Some(3),
        Type::Color => Some(4),
        _ => None,
    }
}

/// What argument types a parameter accepts. `Declared` is the classic
/// exact-type rule; the other rules exist for the pure-builtin framework,
/// whose numeric functions accept int **or** float and decide the result
/// type per function (see [`PureResult`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamTypeRule {
    /// Exactly the declared `ty` (an int argument is coerced when the
    /// declared type is float).
    Declared,
    /// `int` or `float`, preserved as given; the function's result rule
    /// decides whether ints are kept or coerced.
    Numeric,
    /// `int`, `float`, or `bool`.
    NumericOrBool,
    /// Nullable scalar overloads (`nz`).
    NumericOrColor,
    /// Any value-producing type.
    AnyValue,
    /// Value-producing types other than bool; booleans cannot be na.
    NonBooleanValue,
    /// Exactly `string` (the `str.*` string parameters).
    StringOnly,
    /// Any scalar the `str.*` stringifiers accept: int, float, bool, or
    /// string (`str.tostring`'s value, `str.format`'s substitution args).
    Stringable,
}

impl ParamTypeRule {
    pub fn describe(self) -> &'static str {
        match self {
            ParamTypeRule::Declared => "the declared type",
            ParamTypeRule::Numeric => "int or float",
            ParamTypeRule::NumericOrBool => "int, float, or bool",
            ParamTypeRule::NumericOrColor => "int, float, or color",
            ParamTypeRule::AnyValue => "any value",
            ParamTypeRule::NonBooleanValue => "a non-boolean value",
            ParamTypeRule::StringOnly => "string",
            ParamTypeRule::Stringable => "int, float, bool, or string",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParamSpec {
    pub name: &'static str,
    /// The declared type for [`ParamTypeRule::Declared`] parameters; for
    /// the other rules it is the type the `na` literal adopts in this
    /// position (float for numeric-ish parameters).
    pub ty: Type,
    pub type_rule: ParamTypeRule,
    /// The loosest qualifier this parameter accepts. `Series` accepts
    /// anything; `Const` accepts only compile-time constants.
    pub max_qualifier: Qualifier,
    pub required: bool,
    /// When set, the argument must be an **int literal** within this
    /// inclusive range (like history offsets). Used by `ta.*` length
    /// parameters; violations are `E0219`.
    pub int_literal_range: Option<(i64, i64)>,
    /// Default value used when an optional int-literal argument is omitted
    /// (e.g. `ta.change` length defaults to 1).
    pub default_int: Option<i64>,
    /// Default value the compiler materializes as an ordinary float
    /// argument when this optional parameter is omitted (`math.random`'s
    /// 0.0/1.0 bounds). `nz`'s typed default is derived from the source
    /// argument instead (see [`PureResult::Nz`]).
    pub default_float: Option<f64>,
    /// Default string the compiler materializes when a *named* argument binds
    /// a later parameter and leaves this optional one as an interior gap
    /// (`ticker.modify(id, backadjustment = ...)` skipping `session` and
    /// `adjustment`). An empty string is the canonical "unspecified" token.
    ///
    /// Unlike `default_int`/`default_float`, this fills interior gaps only: a
    /// *trailing* omission stays omitted, so the emitted `argc` (and with it
    /// the canonical ticker id) is unchanged.
    pub default_string: Option<&'static str>,
    /// Default bool with the same interior-gap-only rule as `default_string`
    /// (`ticker.renko`'s `request_wicks`, which a caller may skip to reach
    /// `source`).
    pub default_bool: Option<bool>,
    /// Builtin series the compiler materializes for this optional parameter
    /// when it is omitted, including a *trailing* omission: `ticker.standard()`
    /// defaults `symbol` to the chart's own symbol, which only
    /// an explicit `syminfo.tickerid` read can express in the canonical id.
    pub default_series: Option<BuiltinSeries>,
}

impl ParamSpec {
    /// The type an `na` literal adopts when written in this argument
    /// position.
    pub fn expected_type(&self) -> Type {
        match self.type_rule {
            ParamTypeRule::Declared => self.ty.clone(),
            ParamTypeRule::StringOnly => Type::String,
            _ => Type::Float,
        }
    }
}

/// How a pure function's result type derives from its arguments. Data for
/// the checker; the CSIL validator and the VM implement the same rules on
/// abstract/runtime types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PureResult {
    /// Always float; int arguments of `Numeric` parameters are coerced.
    Float,
    /// Always bool (`na(x)`).
    Bool,
    /// Always int, numeric argument kept as given (`math.floor`,
    /// `math.ceil` — identity on int input).
    Int,
    /// Same numeric type as the argument (`math.abs`, `math.sign`).
    Preserve,
    /// Int when every argument is int, else float with ints coerced
    /// (`math.min`, `math.max`).
    IntIfAllInt,
    /// `nz`: the source's type; the replacement must match it (after the
    /// int → float widening), and the omitted replacement defaults to the
    /// typed zero (`0`, `0.0`, `false`).
    Nz,
    /// `math.round`: int without the precision argument, float with it.
    Round,
    /// Always `string` (the `str.*` string-producing functions).
    String,
    /// Always `color` (the `color.*` color-producing constructors:
    /// `color.new`, `color.rgb`, `color.from_gradient`).
    Color,
}

/// The inclusive bounds every `ta.*` length argument must satisfy.
pub const TA_LENGTH_RANGE: (i64, i64) = (1, 100_000);

/// Variadic bounds for `math.min` / `math.max` / `math.avg`.
pub const PURE_VARIADIC_MAX: usize = 10;

/// Canonical parameter names for compiler-special call analyzers whose rich
/// validation cannot use the generic [`ParamSpec`] binder.
pub const SECURITY_PARAM_NAMES: [&str; 8] = [
    "symbol",
    "timeframe",
    "expression",
    "gaps",
    "lookahead",
    "ignore_invalid_symbol",
    "currency",
    "calc_bars_count",
];
pub const SECURITY_LOWER_TF_PARAM_NAMES: [&str; 7] = [
    "symbol",
    "timeframe",
    "expression",
    "ignore_invalid_symbol",
    "currency",
    "ignore_invalid_timeframe",
    "calc_bars_count",
];
pub const PIVOT_POINT_LEVELS_PARAM_NAMES: [&str; 3] = ["type", "anchor", "developing"];
pub const TIME_FUNCTION_PARAM_NAMES: [&str; 5] = [
    "timeframe",
    "session",
    "timezone",
    "bars_back",
    "timeframe_bars_back",
];
pub const VWAP_PARAM_NAMES: [&str; 3] = ["source", "anchor", "stdev_mult"];
pub const TIMESTAMP_UTC_PARAM_NAMES: [&str; 6] =
    ["year", "month", "day", "hour", "minute", "second"];
pub const TIMESTAMP_TZ_PARAM_NAMES: [&str; 7] = [
    "timezone", "year", "month", "day", "hour", "minute", "second",
];
pub const TIMESTAMP_DATE_STRING_PARAM_NAMES: [&str; 1] = ["dateString"];
pub const GENERIC_INPUT_SOURCE_PARAM_NAMES: [&str; 7] = [
    "defval", "title", "inline", "group", "tooltip", "display", "active",
];

/// Parameter taxonomy shared by the host-backed `request.*` analyzer and the
/// public compiler metadata generator. Selector strings are compile-time host
/// lookup keys; the other parameters are ordinary series-capable values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestParamKind {
    Selector,
    String,
    Bool,
    Int,
    Numeric,
}

pub(crate) fn request_param_signature(func: RequestFunc) -> &'static [RequestParamKind] {
    use RequestParamKind::{Bool, Int, Numeric, Selector, String};

    const DIVIDENDS: &[RequestParamKind] = &[Selector, Selector, String, String, Bool, String];
    const SPLITS: &[RequestParamKind] = &[Selector, Selector, String, String, Bool];
    const FINANCIAL: &[RequestParamKind] = &[Selector, Selector, Selector, String, Bool, String];
    const ECONOMIC: &[RequestParamKind] = &[Selector, Selector, String, Bool];
    const CURRENCY_RATE: &[RequestParamKind] = &[Selector, Selector, Bool];
    const SEED: &[RequestParamKind] = &[Selector, Selector, Numeric, Bool, Int];
    const QUANDL: &[RequestParamKind] = &[String, String, Int, Bool];

    match func {
        RequestFunc::SecurityExpression => {
            unreachable!("security_expression is emitted internally by request.security")
        }
        RequestFunc::Dividends | RequestFunc::Earnings => DIVIDENDS,
        RequestFunc::Splits => SPLITS,
        RequestFunc::Financial => FINANCIAL,
        RequestFunc::Economic => ECONOMIC,
        RequestFunc::CurrencyRate => CURRENCY_RATE,
        RequestFunc::Seed => SEED,
        RequestFunc::Quandl => QUANDL,
    }
}

#[derive(Debug, Clone)]
pub struct BuiltinFunction {
    pub name: &'static str,
    pub params: Vec<ParamSpec>,
    pub ret: Type,
    pub kind: BuiltinFunctionKind,
    /// Result-typing rule; present exactly for `Pure` functions.
    pub pure_result: Option<PureResult>,
    /// When set, the last parameter may repeat: the call may pass from the
    /// required count up to this many arguments in total (`math.min`,
    /// `math.max`, `math.avg` accept 2..=10).
    pub variadic_max: Option<usize>,
    /// The signature of an **engine-backed** stateful builtin; present
    /// exactly when `kind` is `Ta(f)` with `f.is_engine()`. `params` is
    /// unused for these functions — the engine signature drives checking.
    pub ta_engine: Option<Vec<TaParam>>,
}

// Chart-type context variables are stored as literals by the headless host,
// but this catalog classifies them as Simple and permits their direct history. Keep
// registration, qualification and history capability on this exact catalog;
// neither the `chart.` prefix nor a non-Const qualifier grants that capability.
const CHART_TYPE_PREDICATES: [(&str, bool); 7] = [
    ("chart.is_standard", true),
    ("chart.is_heikinashi", false),
    ("chart.is_kagi", false),
    ("chart.is_linebreak", false),
    ("chart.is_pnf", false),
    ("chart.is_range", false),
    ("chart.is_renko", false),
];

/// A namespaced built-in constant (`math.pi`, `format.percent`, ...). Most
/// constants are floats (`value`); a `string_value` marks a string constant
/// (the `format.*` display tokens), whose `ty` is [`Type::String`] and whose
/// `value` field is unused.
#[derive(Debug, Clone)]
pub struct BuiltinConstant {
    pub name: &'static str,
    pub value: f64,
    pub ty: Type,
    /// Present for string-valued constants (`format.*`): the checker emits a
    /// `ConstString` instead of a `ConstFloat`.
    pub string_value: Option<&'static str>,
    /// Present for color-valued constants (`color.red`, ...): the checker
    /// emits a `ConstColor` instead of a `ConstFloat`. `(r, g, b, t)`.
    pub color_value: Option<(u8, u8, u8, u8)>,
    /// Present for int-valued constants (`dayofweek.sunday`, ...): the checker
    /// emits a `ConstInt` instead of a `ConstFloat`. `ty` is [`Type::Int`].
    pub int_value: Option<i64>,
    /// Present for bool-valued constants (`chart.is_standard`, ...): the checker
    /// emits a `ConstBool` instead of a `ConstFloat`. `ty` is [`Type::Bool`].
    pub bool_value: Option<bool>,
}

impl BuiltinConstant {
    /// Direct history belongs to the chart-type context variables, despite
    /// their literal host representation. In particular, the Input-qualified
    /// chart colors are direct built-in constants with no such history.
    pub(crate) fn permits_direct_history(&self) -> bool {
        CHART_TYPE_PREDICATES
            .iter()
            .any(|(name, _)| *name == self.name)
    }

    /// Host-provided chart context may be represented by a literal without
    /// granting it the compile-time const qualifier.
    pub fn qualifier(&self) -> Qualifier {
        if self.permits_direct_history() {
            return Qualifier::Simple;
        }
        match self.name {
            "chart.bg_color" | "chart.fg_color" => Qualifier::Input,
            _ => Qualifier::Const,
        }
    }
}

#[derive(Clone)]
pub struct BuiltinRegistry {
    series: HashMap<&'static str, BuiltinSeries>,
    functions: HashMap<&'static str, BuiltinFunction>,
    constants: HashMap<&'static str, BuiltinConstant>,
}

impl BuiltinRegistry {
    /// Override host chart colors without changing the shared headless defaults.
    pub fn with_chart_colors(
        &self,
        background: Option<[u8; 3]>,
        foreground: Option<[u8; 3]>,
    ) -> Self {
        let mut registry = self.clone();
        for (name, color) in [
            ("chart.bg_color", background),
            ("chart.fg_color", foreground),
        ] {
            if let Some([r, g, b]) = color {
                if let Some(constant) = registry.constants.get_mut(name) {
                    constant.color_value = Some((r, g, b, 0));
                }
            }
        }
        registry
    }

    /// Process-wide immutable standard registry.
    ///
    /// Constructing the implemented builtin surface
    /// allocates hundreds of function signatures and lookup entries.
    /// Compilation only ever reads the
    /// registry, so rebuilding it for every script adds pure allocator work to
    /// the semantics stage—especially noticeable when a chart loads several
    /// indicators together. Each native process or wasm worker initializes
    /// this once on first use and reuses it for subsequent compiles.
    pub fn standard_ref() -> &'static Self {
        static STANDARD: OnceLock<BuiltinRegistry> = OnceLock::new();
        STANDARD.get_or_init(Self::standard)
    }

    /// The standard registry: OHLCV series, `bar_index`, `plot`,
    /// `indicator`, the stateful `ta.*` functions, the pure builtins
    /// (`math.*`, `na`, `nz`), and the `math.*` constants.
    pub fn standard() -> Self {
        let mut series = HashMap::new();
        for s in BuiltinSeries::ALL {
            series.insert(s.name(), s);
        }

        let mut functions = HashMap::new();
        functions.insert(
            "plot",
            BuiltinFunction {
                name: "plot",
                params: vec![
                    // Float parameters also accept int arguments: the
                    // checker inserts an explicit IntToFloat coercion.
                    declared_param("series", Type::Float, Qualifier::Series, true),
                    declared_param("title", Type::String, Qualifier::Const, false),
                    declared_param("color", Type::Color, Qualifier::Series, false),
                    declared_param("linewidth", Type::Int, Qualifier::Input, false),
                    declared_param("style", Type::String, Qualifier::Input, false),
                    declared_param("trackprice", Type::Bool, Qualifier::Input, false),
                    declared_param("histbase", Type::Float, Qualifier::Input, false),
                    declared_param("offset", Type::Int, Qualifier::Simple, false),
                    declared_param("join", Type::Bool, Qualifier::Input, false),
                    declared_param("editable", Type::Bool, Qualifier::Input, false),
                    declared_param("show_last", Type::Int, Qualifier::Input, false),
                    declared_param("display", Type::PlotDisplay, Qualifier::Input, false),
                    declared_param("format", Type::String, Qualifier::Input, false),
                    declared_param("precision", Type::Int, Qualifier::Input, false),
                    declared_param("force_overlay", Type::Bool, Qualifier::Const, false),
                    declared_param("linestyle", Type::String, Qualifier::Input, false),
                ],
                ret: Type::Plot,
                kind: BuiltinFunctionKind::Plot,
                pure_result: None,
                variadic_max: None,
                ta_engine: None,
            },
        );
        functions.insert(
            "indicator",
            BuiltinFunction {
                name: "indicator",
                params: vec![
                    declared_param("title", Type::String, Qualifier::Const, true),
                    // The full display/metadata surface. Order matches
                    // the positional signature so `indicator("T", "sh", true)`
                    // binds shorttitle/overlay. All are const; the analyzer
                    // const-folds them. `max_*_count` are honored as drawing GC
                    // caps; the rest are accepted metadata.
                    declared_param("shorttitle", Type::String, Qualifier::Const, false),
                    declared_param("overlay", Type::Bool, Qualifier::Const, false),
                    declared_param("format", Type::String, Qualifier::Const, false),
                    declared_param("precision", Type::Int, Qualifier::Const, false),
                    declared_param("scale", Type::String, Qualifier::Const, false),
                    declared_param("max_bars_back", Type::Int, Qualifier::Const, false),
                    declared_param("timeframe", Type::String, Qualifier::Const, false),
                    declared_param("timeframe_gaps", Type::Bool, Qualifier::Const, false),
                    declared_param("explicit_plot_zorder", Type::Bool, Qualifier::Const, false),
                    declared_param("max_lines_count", Type::Int, Qualifier::Const, false),
                    declared_param("max_labels_count", Type::Int, Qualifier::Const, false),
                    declared_param("max_boxes_count", Type::Int, Qualifier::Const, false),
                    declared_param("calc_bars_count", Type::Int, Qualifier::Const, false),
                    declared_param("max_polylines_count", Type::Int, Qualifier::Const, false),
                    declared_param("dynamic_requests", Type::Bool, Qualifier::Const, false),
                    // `behind_chart` — draw the indicator behind the chart's
                    // candles (display-only; accepted metadata).
                    declared_param("behind_chart", Type::Bool, Qualifier::Const, false),
                ],
                ret: Type::Void,
                kind: BuiltinFunctionKind::Indicator,
                pure_result: None,
                variadic_max: None,
                ta_engine: None,
            },
        );

        functions.insert(
            "max_bars_back",
            BuiltinFunction {
                name: "max_bars_back",
                params: vec![
                    declared_param("var", Type::Float, Qualifier::Series, true),
                    declared_param("num", Type::Int, Qualifier::Const, true),
                ],
                ret: Type::Void,
                kind: BuiltinFunctionKind::MaxBarsBack,
                pure_result: None,
                variadic_max: None,
                ta_engine: None,
            },
        );

        // log.* event stream. Variadic (format string + scalar args) like
        // `str.format`; each records a `(level, bar, message)` event.
        for level in [LogLevel::Info, LogLevel::Warning, LogLevel::Error] {
            let name: &'static str = match level {
                LogLevel::Info => "log.info",
                LogLevel::Warning => "log.warning",
                LogLevel::Error => "log.error",
            };
            functions.insert(
                name,
                BuiltinFunction {
                    name,
                    params: vec![declared_param(
                        "formatString",
                        Type::String,
                        Qualifier::Series,
                        true,
                    )],
                    ret: Type::Void,
                    kind: BuiltinFunctionKind::Log(level),
                    pure_result: None,
                    // Format string + up to STR_FORMAT_MAX_ARGS-1 substitutions,
                    // matching `str.format`'s placeholder budget.
                    variadic_max: Some(STR_FORMAT_MAX_ARGS as usize),
                    ta_engine: None,
                },
            );
        }

        // alert(). One message plus an optional `alert.freq_*` token.
        functions.insert(
            "alert",
            BuiltinFunction {
                name: "alert",
                params: vec![
                    declared_param("message", Type::String, Qualifier::Series, true),
                    declared_param("freq", Type::String, Qualifier::Const, false),
                ],
                ret: Type::Void,
                kind: BuiltinFunctionKind::Alert,
                pure_result: None,
                variadic_max: None,
                ta_engine: None,
            },
        );

        // alertcondition(): fires only on bars where `condition` is true.
        functions.insert(
            "alertcondition",
            BuiltinFunction {
                name: "alertcondition",
                params: vec![
                    declared_param("condition", Type::Bool, Qualifier::Series, true),
                    declared_param("title", Type::String, Qualifier::Const, false),
                    declared_param("message", Type::String, Qualifier::Const, false),
                ],
                ret: Type::Void,
                kind: BuiltinFunctionKind::AlertCondition,
                pure_result: None,
                variadic_max: None,
                ta_engine: None,
            },
        );

        // Display-plot family. Each is fully type-checked by a dedicated
        // analyzer; the registry still publishes its exact typed parameter
        // contract so signature consumers cannot drift from accepted calls.
        for (name, kind) in [
            ("plotshape", DisplayKind::PlotShape),
            ("plotchar", DisplayKind::PlotChar),
            ("plotarrow", DisplayKind::PlotArrow),
            ("plotcandle", DisplayKind::PlotCandle),
            ("plotbar", DisplayKind::PlotBar),
            ("hline", DisplayKind::Hline),
            ("bgcolor", DisplayKind::BgColor),
            ("barcolor", DisplayKind::BarColor),
            ("fill", DisplayKind::Fill),
        ] {
            functions.insert(
                name,
                BuiltinFunction {
                    name,
                    params: display_params(kind),
                    ret: if matches!(kind, DisplayKind::Hline) {
                        Type::Hline
                    } else {
                        Type::Void
                    },
                    kind: BuiltinFunctionKind::Display(kind),
                    pure_result: None,
                    variadic_max: None,
                    ta_engine: None,
                },
            );
        }

        // runtime.error(message): abort the current run with `message`.
        functions.insert(
            "runtime.error",
            BuiltinFunction {
                name: "runtime.error",
                params: vec![declared_param(
                    "message",
                    Type::String,
                    Qualifier::Series,
                    true,
                )],
                ret: Type::Void,
                kind: BuiltinFunctionKind::RuntimeErrorFn,
                pure_result: None,
                variadic_max: None,
                ta_engine: None,
            },
        );

        // Strategy order primitives. Each places an order the broker
        // emulator fills; all produce void and are only valid inside a
        // `strategy()` script (checked in `E0241`). The argument surface is
        // kind-specific, so a dedicated analyzer validates them. The same
        // typed parameter metadata drives public signature help.
        for (name, kind) in [
            ("strategy.entry", OrderKind::Entry),
            ("strategy.order", OrderKind::Order),
            ("strategy.exit", OrderKind::Exit),
            ("strategy.close", OrderKind::Close),
            ("strategy.close_all", OrderKind::CloseAll),
            ("strategy.cancel", OrderKind::Cancel),
            ("strategy.cancel_all", OrderKind::CancelAll),
            (
                "strategy.risk.max_position_size",
                OrderKind::RiskMaxPositionSize,
            ),
            ("strategy.risk.allow_entry_in", OrderKind::RiskAllowEntryIn),
            ("strategy.risk.max_drawdown", OrderKind::RiskMaxDrawdown),
            (
                "strategy.risk.max_intraday_loss",
                OrderKind::RiskMaxIntradayLoss,
            ),
            (
                "strategy.risk.max_intraday_filled_orders",
                OrderKind::RiskMaxIntradayFilledOrders,
            ),
            (
                "strategy.risk.max_cons_loss_days",
                OrderKind::RiskMaxConsLossDays,
            ),
        ] {
            functions.insert(
                name,
                BuiltinFunction {
                    name,
                    params: order_params(kind),
                    ret: Type::Void,
                    kind: BuiltinFunctionKind::Order(kind),
                    pure_result: None,
                    variadic_max: None,
                    ta_engine: None,
                },
            );
        }

        // `request.security`. Produces a `series float`; the argument
        // surface (symbol, timeframe, expression, optional gaps/lookahead) is
        // validated by the dedicated `analyze_security_call` analyzer. The
        // registry metadata mirrors that analyzer's public contract.
        functions.insert(
            "request.security",
            BuiltinFunction {
                name: "request.security",
                params: security_params(),
                ret: Type::Float,
                kind: BuiltinFunctionKind::Security,
                pure_result: None,
                variadic_max: None,
                ta_engine: None,
            },
        );

        // `request.security_lower_tf`. Produces an `array<float>` of the
        // intrabar values within each chart bar; the argument surface
        // (symbol, timeframe, expression) is validated by the dedicated
        // `analyze_security_lower_tf_call` analyzer. Host-or-default (empty
        // array); typed metadata remains available to editor consumers.
        functions.insert(
            "request.security_lower_tf",
            BuiltinFunction {
                name: "request.security_lower_tf",
                params: security_lower_tf_params(),
                ret: Type::Array(Box::new(Type::Float)),
                kind: BuiltinFunctionKind::SecurityLowerTf,
                pure_result: None,
                variadic_max: None,
                ta_engine: None,
            },
        );

        // `ta.pivot_point_levels(type, anchor)`. Produces an
        // `array<float>` of pivot levels for the given system, computed from the
        // prior completed anchor period's HLC. The argument surface (const
        // `type` string, const `anchor` timeframe string) is validated by the
        // dedicated `analyze_pivot_point_levels_call` analyzer; its exact
        // const-string contract is kept in the registry metadata.
        functions.insert(
            "ta.pivot_point_levels",
            BuiltinFunction {
                name: "ta.pivot_point_levels",
                params: pivot_point_levels_params(),
                ret: Type::Array(Box::new(Type::Float)),
                kind: BuiltinFunctionKind::PivotPointLevels,
                pure_result: None,
                variadic_max: None,
                ta_engine: None,
            },
        );

        // The FUNCTION forms `time(timeframe, session, timezone,
        // bars_back, timeframe_bars_back)` and `time_close(...)`. Each produces
        // a `series int` (epoch ms) through the dedicated analyzer. The bare
        // `time`/`time_close` variables are separate `BuiltinSeries`; the
        // function form only wins for a `name(...)` call.
        for (name, close) in [("time", false), ("time_close", true)] {
            functions.insert(
                name,
                BuiltinFunction {
                    name,
                    params: time_function_params(),
                    ret: Type::Int,
                    kind: BuiltinFunctionKind::TimeFunction { close },
                    pure_result: None,
                    variadic_max: None,
                    ta_engine: None,
                },
            );
        }

        // `request.*` external-data interfaces. Each is a `series float`
        // whose argument surface is validated by `analyze_request_call`; the
        // registry exposes the same typed signature to public tooling.
        for (name, func) in [
            ("request.dividends", RequestFunc::Dividends),
            ("request.splits", RequestFunc::Splits),
            ("request.earnings", RequestFunc::Earnings),
            ("request.financial", RequestFunc::Financial),
            ("request.economic", RequestFunc::Economic),
            ("request.currency_rate", RequestFunc::CurrencyRate),
            ("request.seed", RequestFunc::Seed),
            ("request.quandl", RequestFunc::Quandl),
        ] {
            functions.insert(
                name,
                BuiltinFunction {
                    name,
                    params: request_params(func),
                    ret: Type::Float,
                    kind: BuiltinFunctionKind::Request(func),
                    pure_result: None,
                    variadic_max: None,
                    ta_engine: None,
                },
            );
        }

        // Stateful ta.* functions. Adding one is a table entry: name, the
        // TaFunc tag, and whether the length is optional.
        functions.insert(
            "ta.change",
            ta_function("ta.change", TaFunc::Change, Some(1)),
        );
        functions.insert("ta.sma", ta_function("ta.sma", TaFunc::Sma, None));

        // Engine-backed stateful builtins (`CALL_TA`). Adding one is a
        // table entry here plus a `TaFunc` data-table row and a VM state
        // arm — the checker core is driven entirely by this data.
        for func in ta_engine_functions() {
            functions.insert(func.name, func);
        }

        // Pure builtins. Adding one is a table entry here plus a dispatch
        // arm in the VM (and a validator typing row) — the checker core is
        // driven entirely by this data.
        for func in pure_functions() {
            functions.insert(func.name, func);
        }

        // `input.*` declarations. Each entry names the surface function and
        // its InputKind; kind-specific argument binding, constraint checking,
        // and metadata recording live in the analyzer's `analyze_input_call`.
        for func in input_functions() {
            functions.insert(func.name, func);
        }

        // `array.*` collection functions. Each entry names the surface
        // function and its CollectionFunc tag; argument binding and the
        // receiver-dependent result typing live in `analyze_collection_call`.
        for func in collection_functions() {
            functions.insert(func.name, func);
        }

        // `map.*` collection functions: receiver operations plus the
        // typed `map.new_<key>_<val>` constructors.
        for func in map_functions() {
            functions.insert(func.name, func);
        }

        // `matrix.*` collection functions: receiver operations plus the
        // typed `matrix.new_<T>` constructors.
        for func in matrix_functions() {
            functions.insert(func.name, func);
        }

        let mut constants = HashMap::new();
        let float_const = |name: &'static str, value: f64| BuiltinConstant {
            name,
            value,
            ty: Type::Float,
            string_value: None,
            color_value: None,
            int_value: None,
            bool_value: None,
        };
        // Integer-valued constants (`dayofweek.*`).
        let int_const = |name: &'static str, value: i64| BuiltinConstant {
            name,
            value: 0.0,
            ty: Type::Int,
            string_value: None,
            color_value: None,
            int_value: Some(value),
            bool_value: None,
        };
        // Bool-valued constants (`chart.is_*`). The chart type is a host
        // property, not a script value; each `chart.is_*` is a
        // `const bool`. This headless VM has one feed and no chart-type host
        // input, so we model the default: a standard chart
        // (`chart.is_standard == true`, every other predicate `false`).
        let bool_const = |name: &'static str, value: bool| BuiltinConstant {
            name,
            value: 0.0,
            ty: Type::Bool,
            string_value: None,
            color_value: None,
            int_value: None,
            bool_value: Some(value),
        };
        // String-valued display-format constants (`str.tostring`'s format
        // argument). Their value is the token the VM's numeric formatter
        // matches on. `format.inherit` is accepted but formats as the default.
        let format_const = |name: &'static str| BuiltinConstant {
            name,
            value: 0.0,
            ty: Type::String,
            string_value: Some(name),
            color_value: None,
            int_value: None,
            bool_value: None,
        };
        // The 17 named `color.*` constants (all opaque, `t = 0`).
        for (name, r, g, b) in pinewasm_hir::NAMED_COLORS {
            constants.insert(
                name,
                BuiltinConstant {
                    name,
                    value: 0.0,
                    ty: Type::Color,
                    string_value: None,
                    color_value: Some((r, g, b, 0)),
                    int_value: None,
                    bool_value: None,
                },
            );
        }
        // `chart.bg_color` / `chart.fg_color` — the chart's background /
        // foreground theme colors. These are host/UI theme properties with no
        // headless equivalent; this VM models the light-theme
        // default (white background, black foreground) as opaque literal-valued
        // Input colors. Their qualifier does not grant direct history. A
        // themed host overrides them through `with_chart_colors`.
        for (name, r, g, b) in [
            ("chart.bg_color", 255u8, 255u8, 255u8),
            ("chart.fg_color", 0u8, 0u8, 0u8),
        ] {
            constants.insert(
                name,
                BuiltinConstant {
                    name,
                    value: 0.0,
                    ty: Type::Color,
                    string_value: None,
                    color_value: Some((r, g, b, 0)),
                    int_value: None,
                    bool_value: None,
                },
            );
        }
        // `dayofweek.*` constants, numbered Sunday = 1 … Saturday = 7.
        // `order.*` sort-direction constants. ascending = 0 is the
        // default; the VM's sort arm matches on this value.
        for c in [
            int_const("order.ascending", 0),
            int_const("order.descending", 1),
        ] {
            constants.insert(c.name, c);
        }
        for c in [
            int_const("dayofweek.sunday", 1),
            int_const("dayofweek.monday", 2),
            int_const("dayofweek.tuesday", 3),
            int_const("dayofweek.wednesday", 4),
            int_const("dayofweek.thursday", 5),
            int_const("dayofweek.friday", 6),
            int_const("dayofweek.saturday", 7),
        ] {
            constants.insert(c.name, c);
        }
        // `chart.is_*` — Simple chart-type context predicates with history.
        // The chart type is a host property; this headless VM models only the
        // default of a standard chart, so `chart.is_standard` folds
        // to `true` and every other predicate to `false`. A script referencing
        // any of them compiles and evaluates the constant bool; branches that
        // depend on a non-standard chart type are statically dead here.
        for (name, value) in CHART_TYPE_PREDICATES {
            let c = bool_const(name, value);
            constants.insert(c.name, c);
        }
        for c in [
            // Exact f64 values (correctly rounded IEEE-754 doubles).
            float_const("math.pi", std::f64::consts::PI),
            float_const("math.e", std::f64::consts::E),
            // The golden ratio (1 + √5)/2 and its reciprocal (√5 − 1)/2,
            // computed rather than written as decimal literals so the doubles are exact.
            float_const("math.phi", f64::midpoint(1.0, 5.0_f64.sqrt())),
            float_const("math.rphi", (5.0_f64.sqrt() - 1.0) / 2.0),
            format_const("format.mintick"),
            format_const("format.percent"),
            format_const("format.volume"),
            format_const("format.inherit"),
            // `format.price` — the price-scale display format for
            // `str.tostring`/`indicator(format=)`. A distinct const token; the
            // VM's numeric formatter falls back to the default (like
            // `format.inherit`) for it, a precision approximation.
            format_const("format.price"),
            // `session.regular` / `session.extended` — the session-type
            // constants. Each is a const string whose value is a session
            // token ("regular" / "extended"), modeled like `format.*`: an
            // opaque string constant that round-trips through the pipeline.
            format_const("session.regular"),
            format_const("session.extended"),
        ] {
            constants.insert(c.name, c);
        }

        // Preserve the string values through comparisons, inputs,
        // heap objects and getters. The drawing DTO boundary maps these to
        // canonical schema-v1 names in explicit per-field domains. Other namespaces
        // retain their existing representation.
        let style_const = |name: &'static str| BuiltinConstant {
            name,
            value: 0.0,
            ty: if matches!(name, "strategy.long" | "strategy.short") {
                Type::Enum(STRATEGY_DIRECTION_TYPE.to_string())
            } else if name.starts_with("text.format_") {
                Type::TextFormat
            } else if matches!(name, "display.all" | "display.none") {
                Type::PlotSimpleDisplay
            } else if name.starts_with("display.") {
                Type::PlotDisplay
            } else {
                Type::String
            },
            string_value: Some(
                pinewasm_hir::drawing_options::pine_drawing_string(name).unwrap_or(name),
            ),
            color_value: None,
            int_value: None,
            bool_value: None,
        };
        for name in [
            // `xloc.*` — line/label/box x-coordinate reference.
            "xloc.bar_index",
            "xloc.bar_time",
            // `yloc.*` — label/box y-coordinate reference.
            "yloc.price",
            "yloc.abovebar",
            "yloc.belowbar",
            // `extend.*` — line/box extension direction.
            "extend.none",
            "extend.left",
            "extend.right",
            "extend.both",
            // `line.style_*` — line/box border style.
            "line.style_solid",
            "line.style_dotted",
            "line.style_dashed",
            "line.style_arrow_left",
            "line.style_arrow_right",
            "line.style_arrow_both",
            // `hline.style_*` — horizontal-line style.
            "hline.style_solid",
            "hline.style_dotted",
            "hline.style_dashed",
            // `label.style_*` — label marker style (the directional/shape markers
            // plus the text-only and none styles).
            "label.style_none",
            "label.style_xcross",
            "label.style_cross",
            "label.style_triangleup",
            "label.style_triangledown",
            "label.style_flag",
            "label.style_circle",
            "label.style_arrowup",
            "label.style_arrowdown",
            "label.style_label_up",
            "label.style_label_down",
            "label.style_label_left",
            "label.style_label_right",
            "label.style_label_lower_left",
            "label.style_label_lower_right",
            "label.style_label_upper_left",
            "label.style_label_upper_right",
            "label.style_label_center",
            "label.style_square",
            "label.style_diamond",
            "label.style_text_outline",
            // `size.*` — label/table text size.
            "size.auto",
            "size.tiny",
            "size.small",
            "size.normal",
            "size.large",
            "size.huge",
            // `text.align_*` — label/box/table text alignment.
            "text.align_left",
            "text.align_center",
            "text.align_right",
            "text.align_top",
            "text.align_bottom",
            // `text.wrap_*` — box/table text wrapping.
            "text.wrap_none",
            "text.wrap_auto",
            // `text.format_*` — label/box/table `text_formatting` style. They have
            // nominal text_format type, and `+` unions
            // the formatting flags rather than concatenating these strings.
            "text.format_none",
            "text.format_bold",
            "text.format_italic",
            // Enum-like token namespaces consumed by table positioning,
            // drawing objects, and alert frequency handling. They use the
            // same opaque-string-token model as the block above.
            //
            // `position.*` — table anchor position on the chart pane (the 3×3
            // anchor grid).
            "position.top_left",
            "position.top_center",
            "position.top_right",
            "position.middle_left",
            "position.middle_center",
            "position.middle_right",
            "position.bottom_left",
            "position.bottom_center",
            "position.bottom_right",
            // `alert.freq_*` — `alert()` triggering frequency.
            "alert.freq_all",
            "alert.freq_once_per_bar",
            "alert.freq_once_per_bar_close",
            // `strategy.long` / `strategy.short` — order direction tokens
            // consumed by the `strategy.*` order primitives. Their nominal
            // `strategy_direction` type is distinct from arbitrary strings,
            // while their string-backed runtime values remain stable through
            // HIR, MIR, CSIL and hosted-Wasm interpretation.
            "strategy.long",
            "strategy.short",
            // `strategy.direction.*` — `strategy.risk.allow_entry_in` value
            // tokens (restrict which direction entries may open). Opaque string
            // tokens; the order analyzer/broker match on the token value.
            "strategy.direction.long",
            "strategy.direction.short",
            "strategy.direction.all",
            // `strategy.*` risk-directive unit tokens: `value` type for
            // `max_drawdown`/`max_intraday_loss` (cash vs. percent-of-equity).
            "strategy.cash",
            "strategy.percent_of_equity",
            // `strategy.*` qty / commission / oca enum tokens.
            // Same opaque-string-token model as the blocks above; each is a
            // `const string` token accepted by the compiler (`strategy.fixed`,
            // `strategy.commission.*`, `strategy.oca.*`). The `strategy()`
            // `default_qty_type`/`commission_type` params and the order
            // primitives' `oca_type` argument accept these named constants
            // (they resolve to the same bare tokens the raw strings already
            // matched — see `strategy_const_enum`). `strategy.cash`/
            // `strategy.percent_of_equity` (also qty types) are above.
            "strategy.fixed",
            "strategy.commission.percent",
            "strategy.commission.cash_per_contract",
            "strategy.commission.cash_per_order",
            "strategy.oca.cancel",
            "strategy.oca.none",
            "strategy.oca.reduce",
            // `barmerge.*` — `request.security` merge-behavior tokens.
            // Opaque string tokens like the blocks above; the security
            // analyzer maps `gaps_on`/`gaps_off` to a gap mode and
            // `lookahead_on`/`lookahead_off` to a lookahead mode (see
            // `pinewasm_vm::resample`).
            "barmerge.gaps_on",
            "barmerge.gaps_off",
            "barmerge.lookahead_on",
            "barmerge.lookahead_off",
            // Display/style/selector token namespaces. Same
            // opaque-string-token model as the blocks above: each member is a
            // distinct compile-time string constant a script can reference and
            // round-trip. The on-chart *rendering* effect these select (plot
            // shapes, display locations, price scales, marker shapes, fonts) is
            // NOT modeled by the headless VM — only the token value is real.
            //
            // `plot.style_*` — plot rendering style.
            "plot.style_line",
            "plot.style_linebr",
            "plot.style_stepline",
            "plot.style_steplinebr",
            "plot.style_stepline_diamond",
            "plot.style_histogram",
            "plot.style_cross",
            "plot.style_area",
            "plot.style_areabr",
            "plot.style_columns",
            "plot.style_circles",
            // `plot.linestyle_*` — `plot()` `linestyle` line style (type
            // `const plot_line_style`; `solid` is the default).
            "plot.linestyle_solid",
            "plot.linestyle_dotted",
            "plot.linestyle_dashed",
            // `display.*` — where a plot/output is shown.
            "display.none",
            "display.pane",
            "display.data_window",
            "display.price_scale",
            "display.status_line",
            "display.pine_screener",
            "display.all",
            // `scale.*` — which price scale a series binds to.
            "scale.left",
            "scale.right",
            "scale.none",
            // `location.*` — `plotshape`/`plotchar` marker placement.
            "location.abovebar",
            "location.belowbar",
            "location.top",
            "location.bottom",
            "location.absolute",
            // `shape.*` — `plotshape`/`plotchar` marker shape.
            "shape.xcross",
            "shape.cross",
            "shape.circle",
            "shape.triangleup",
            "shape.triangledown",
            "shape.flag",
            "shape.arrowup",
            "shape.arrowdown",
            "shape.square",
            "shape.diamond",
            "shape.labelup",
            "shape.labeldown",
            // `font.family_*` — table/label font family.
            "font.family_default",
            "font.family_monospace",
            // `adjustment.*` — `request.security` price-adjustment mode.
            "adjustment.dividends",
            "adjustment.none",
            "adjustment.splits",
            // `backadjustment.*` — `ticker.new()`/`ticker.modify()` continuous-
            // futures back-adjustment mode.
            "backadjustment.inherit",
            "backadjustment.on",
            "backadjustment.off",
            // `settlement_as_close.*` — `ticker.new()`/`ticker.modify()`
            // settlement-price-as-close mode.
            "settlement_as_close.inherit",
            "settlement_as_close.on",
            "settlement_as_close.off",
            // `dividends.*` — `request.dividends` field selector.
            // NOTE: the `dividends.future_*` names are *built-in series*
            // (upcoming-dividend values), not field selectors, so they are
            // intentionally not registered here — modeling them needs a host
            // data surface.
            "dividends.gross",
            "dividends.net",
            // `earnings.*` — `request.earnings` field selector.
            // The `earnings.future_*` names are built-in series (see the
            // `dividends.*` note above), not selectors, so they are omitted.
            "earnings.actual",
            "earnings.estimate",
            "earnings.standardized",
            // `splits.*` — `request.splits` field selector.
            "splits.denominator",
            "splits.numerator",
        ] {
            constants.insert(name, style_const(name));
        }

        // `currency.*` — ISO 4217 currency codes (plus crypto and
        // `currency.NONE`). Each is a `const string`; the
        // runtime value is the bare code (`currency.USD == "USD"`), which the
        // `strategy(currency = …)` param and `syminfo.currency`/
        // `syminfo.basecurrency`/`strategy.account_currency` compare against.
        // So — unlike the display-token block above, which round-trips the full
        // dotted name — a currency constant folds to its trailing code.
        let currency_const = |name: &'static str, code: &'static str| BuiltinConstant {
            name,
            value: 0.0,
            ty: Type::String,
            string_value: Some(code),
            color_value: None,
            int_value: None,
            bool_value: None,
        };
        for (name, code) in [
            ("currency.AED", "AED"),
            ("currency.ARS", "ARS"),
            ("currency.AUD", "AUD"),
            ("currency.BDT", "BDT"),
            ("currency.BHD", "BHD"),
            ("currency.BRL", "BRL"),
            ("currency.BTC", "BTC"),
            ("currency.CAD", "CAD"),
            ("currency.CHF", "CHF"),
            ("currency.CLP", "CLP"),
            ("currency.CNY", "CNY"),
            ("currency.COP", "COP"),
            ("currency.CZK", "CZK"),
            ("currency.DKK", "DKK"),
            ("currency.EGP", "EGP"),
            ("currency.ETH", "ETH"),
            ("currency.EUR", "EUR"),
            ("currency.GBP", "GBP"),
            ("currency.HKD", "HKD"),
            ("currency.HUF", "HUF"),
            ("currency.IDR", "IDR"),
            ("currency.ILS", "ILS"),
            ("currency.INR", "INR"),
            ("currency.ISK", "ISK"),
            ("currency.JPY", "JPY"),
            ("currency.KES", "KES"),
            ("currency.KRW", "KRW"),
            ("currency.KWD", "KWD"),
            ("currency.LKR", "LKR"),
            ("currency.MAD", "MAD"),
            ("currency.MXN", "MXN"),
            ("currency.MYR", "MYR"),
            ("currency.NGN", "NGN"),
            ("currency.NOK", "NOK"),
            ("currency.NONE", "NONE"),
            ("currency.NZD", "NZD"),
            ("currency.PEN", "PEN"),
            ("currency.PHP", "PHP"),
            ("currency.PKR", "PKR"),
            ("currency.PLN", "PLN"),
            ("currency.QAR", "QAR"),
            ("currency.RON", "RON"),
            ("currency.RSD", "RSD"),
            ("currency.RUB", "RUB"),
            ("currency.SAR", "SAR"),
            ("currency.SEK", "SEK"),
            ("currency.SGD", "SGD"),
            ("currency.THB", "THB"),
            ("currency.TND", "TND"),
            ("currency.TRY", "TRY"),
            ("currency.TWD", "TWD"),
            ("currency.USD", "USD"),
            ("currency.USDT", "USDT"),
            ("currency.VES", "VES"),
            ("currency.VND", "VND"),
            ("currency.ZAR", "ZAR"),
        ] {
            constants.insert(name, currency_const(name, code));
        }

        BuiltinRegistry {
            series,
            functions,
            constants,
        }
    }

    pub fn lookup_series(&self, name: &str) -> Option<BuiltinSeries> {
        self.series.get(name).copied()
    }

    pub fn lookup_function(&self, name: &str) -> Option<&BuiltinFunction> {
        self.functions.get(name)
    }

    pub fn lookup_constant(&self, name: &str) -> Option<&BuiltinConstant> {
        self.constants.get(name)
    }

    /// Iterate over every registered built-in series name and its HIR tag.
    ///
    /// Language-surface tests use this read-only view to compare
    /// complete namespaces, so an unexpected alias cannot hide behind
    /// successful point lookups.
    pub fn series(&self) -> impl Iterator<Item = (&'static str, BuiltinSeries)> + '_ {
        self.series.iter().map(|(name, series)| (*name, *series))
    }

    /// Iterate over every registered function.
    ///
    /// Language-tooling hosts use this read-only view for completion, hover,
    /// and signature help. Keeping the registry as the source of truth avoids
    /// a second hand-maintained language surface outside the compiler.
    pub fn functions(&self) -> impl Iterator<Item = &BuiltinFunction> {
        self.functions.values()
    }

    /// Iterate over every registered namespaced constant.
    pub fn constants(&self) -> impl Iterator<Item = &BuiltinConstant> {
        self.constants.values()
    }

    /// Whether `name` is a built-in type or the namespace of a built-in
    /// function, variable or constant (`ta`, `color`, `syminfo`, ...). The
    /// An enum may not take such a name. The drawing,
    /// collection and order-flow types are listed explicitly because some of
    /// their calls are analyzed outside the registry.
    pub fn is_builtin_type_or_namespace(&self, name: &str) -> bool {
        const TYPES_AND_NAMESPACES: &[&str] = &[
            "int",
            "float",
            "bool",
            "string",
            "color",
            "array",
            "matrix",
            "map",
            "line",
            "label",
            "box",
            "table",
            "linefill",
            "polyline",
            "chart",
            "footprint",
            "volume_row",
        ];
        let in_namespace = |full: &str| full.split_once('.').is_some_and(|(ns, _)| ns == name);
        TYPES_AND_NAMESPACES.contains(&name)
            || self.series.keys().any(|full| in_namespace(full))
            || self.functions.keys().any(|full| in_namespace(full))
            || self.constants.keys().any(|full| in_namespace(full))
    }

    /// Every built-in function, as its bare method name when it is a
    /// `Collection`-kind builtin of the `namespace` (`array`/`map`/`matrix`;
    /// `push` for `array.push`) and as `None` when it is not. This is what a
    /// did-you-mean search for an unknown method-call (`E0235`) looks at. The
    /// typed constructors (`map.new_*`/`matrix.new_*`) are distinct registry
    /// kinds and are intentionally excluded — they are never method-callable
    /// on a value.
    pub(crate) fn collection_methods<'s>(
        &'s self,
        namespace: &'s str,
    ) -> impl Iterator<Item = Option<&'static str>> + 's {
        self.functions.values().map(move |function| {
            if !matches!(function.kind, BuiltinFunctionKind::Collection(_)) {
                return None;
            }
            function.name.strip_prefix(namespace)?.strip_prefix('.')
        })
    }

    /// How many names [`Self::suggest`] compares an unknown name with.
    pub fn suggestion_candidates(&self) -> usize {
        self.series.len() + self.functions.len() + self.constants.len()
    }

    /// Best-effort "did you mean" suggestion for unknown identifiers.
    pub fn suggest(&self, name: &str) -> Option<&'static str> {
        closest(
            name,
            self.series
                .keys()
                .copied()
                .chain(self.functions.keys().copied())
                .chain(self.constants.keys().copied()),
        )
    }
}

fn declared_param(
    name: &'static str,
    ty: Type,
    max_qualifier: Qualifier,
    required: bool,
) -> ParamSpec {
    ParamSpec {
        name,
        ty,
        type_rule: ParamTypeRule::Declared,
        max_qualifier,
        required,
        int_literal_range: None,
        default_int: None,
        default_float: None,
        default_string: None,
        default_bool: None,
        default_series: None,
    }
}

fn ruled_param(
    name: &'static str,
    ty: Type,
    type_rule: ParamTypeRule,
    max_qualifier: Qualifier,
    required: bool,
) -> ParamSpec {
    ParamSpec {
        name,
        ty,
        type_rule,
        max_qualifier,
        required,
        int_literal_range: None,
        default_int: None,
        default_float: None,
        default_string: None,
        default_bool: None,
        default_series: None,
    }
}

fn numeric_qualified_param(
    name: &'static str,
    max_qualifier: Qualifier,
    required: bool,
) -> ParamSpec {
    ruled_param(
        name,
        Type::Float,
        ParamTypeRule::Numeric,
        max_qualifier,
        required,
    )
}

fn numeric_or_bool_qualified_param(
    name: &'static str,
    max_qualifier: Qualifier,
    required: bool,
) -> ParamSpec {
    ruled_param(
        name,
        Type::Float,
        ParamTypeRule::NumericOrBool,
        max_qualifier,
        required,
    )
}

pub(crate) fn display_destination_type(kind: DisplayKind) -> Type {
    match kind {
        DisplayKind::Hline | DisplayKind::BgColor | DisplayKind::BarColor | DisplayKind::Fill => {
            Type::PlotSimpleDisplay
        }
        _ => Type::PlotDisplay,
    }
}

fn display_params(kind: DisplayKind) -> Vec<ParamSpec> {
    kind.param_names()
        .iter()
        .enumerate()
        .map(|(index, &name)| {
            let required = index < kind.required_count();
            let qualifier = kind.param_qualifier(name);
            if index < kind.data_count() {
                return if matches!(kind, DisplayKind::PlotShape | DisplayKind::PlotChar) {
                    numeric_or_bool_qualified_param(name, qualifier, required)
                } else {
                    numeric_qualified_param(name, qualifier, required)
                };
            }
            match name {
                "color" | "colorup" | "colordown" | "textcolor" | "wickcolor" | "bordercolor" => {
                    declared_param(name, Type::Color, qualifier, required)
                }
                "title" | "text" | "style" | "char" | "location" | "size" | "format"
                | "linestyle" => declared_param(name, Type::String, qualifier, required),
                "display" => {
                    declared_param(name, display_destination_type(kind), qualifier, required)
                }
                "editable" | "force_overlay" | "fillgaps" => {
                    declared_param(name, Type::Bool, qualifier, required)
                }
                "offset" | "precision" | "linewidth" | "minheight" | "maxheight" | "show_last" => {
                    declared_param(name, Type::Int, qualifier, required)
                }
                "hline1" | "hline2" => declared_param(name, Type::Hline, qualifier, required),
                _ => panic!("display parameter `{name}` has no public type metadata"),
            }
        })
        .collect()
}

fn order_params(kind: OrderKind) -> Vec<ParamSpec> {
    let default_max = if kind.is_risk() {
        Qualifier::Simple
    } else {
        Qualifier::Series
    };
    kind.param_names()
        .iter()
        .enumerate()
        .map(|(index, &name)| {
            let required = index < kind.required_count();
            match name {
                "direction" => declared_param(
                    name,
                    Type::Enum(STRATEGY_DIRECTION_TYPE.to_string()),
                    default_max,
                    required,
                ),
                "value" if kind == OrderKind::RiskAllowEntryIn => {
                    declared_param(name, Type::String, default_max, required)
                }
                "id" | "from_entry" | "oca_name" | "comment" | "comment_profit"
                | "comment_loss" | "comment_trailing" | "alert_message" | "alert_profit"
                | "alert_loss" | "alert_trailing" | "type" => {
                    declared_param(name, Type::String, default_max, required)
                }
                "oca_type" => declared_param(name, Type::String, Qualifier::Input, required),
                "count" => declared_param(name, Type::Int, default_max, required),
                "immediately" | "disable_alert" => {
                    declared_param(name, Type::Bool, default_max, required)
                }
                "value" => numeric_qualified_param(name, default_max, required),
                _ => numeric_qualified_param(name, default_max, required),
            }
        })
        .collect()
}

fn security_params() -> Vec<ParamSpec> {
    vec![
        declared_param("symbol", Type::String, Qualifier::Simple, true),
        declared_param("timeframe", Type::String, Qualifier::Const, true),
        numeric_or_bool_qualified_param("expression", Qualifier::Series, true),
        declared_param("gaps", Type::String, Qualifier::Const, false),
        declared_param("lookahead", Type::String, Qualifier::Const, false),
        declared_param(
            "ignore_invalid_symbol",
            Type::Bool,
            Qualifier::Series,
            false,
        ),
        declared_param("currency", Type::String, Qualifier::Const, false),
        declared_param("calc_bars_count", Type::Int, Qualifier::Const, false),
    ]
}

fn security_lower_tf_params() -> Vec<ParamSpec> {
    vec![
        declared_param("symbol", Type::String, Qualifier::Simple, true),
        declared_param("timeframe", Type::String, Qualifier::Const, true),
        numeric_qualified_param("expression", Qualifier::Series, true),
        declared_param(
            "ignore_invalid_symbol",
            Type::Bool,
            Qualifier::Series,
            false,
        ),
        declared_param("currency", Type::String, Qualifier::Series, false),
        declared_param(
            "ignore_invalid_timeframe",
            Type::Bool,
            Qualifier::Series,
            false,
        ),
        declared_param("calc_bars_count", Type::Int, Qualifier::Series, false),
    ]
}

fn pivot_point_levels_params() -> Vec<ParamSpec> {
    vec![
        declared_param("type", Type::String, Qualifier::Series, true),
        declared_param("anchor", Type::Bool, Qualifier::Series, true),
        declared_param("developing", Type::Bool, Qualifier::Series, false),
    ]
}

fn time_function_params() -> Vec<ParamSpec> {
    vec![
        declared_param("timeframe", Type::String, Qualifier::Series, true),
        declared_param("session", Type::String, Qualifier::Series, false),
        declared_param("timezone", Type::String, Qualifier::Series, false),
        declared_param("bars_back", Type::Int, Qualifier::Series, false),
        declared_param("timeframe_bars_back", Type::Int, Qualifier::Series, false),
    ]
}

fn request_params(func: RequestFunc) -> Vec<ParamSpec> {
    let kinds = request_param_signature(func);
    let names = func.param_names();
    assert_eq!(names.len(), kinds.len());
    names
        .iter()
        .zip(kinds)
        .enumerate()
        .map(|(index, (&name, kind))| {
            let required = index < func.required_count();
            match kind {
                RequestParamKind::Selector => {
                    declared_param(name, Type::String, Qualifier::Const, required)
                }
                RequestParamKind::String => {
                    declared_param(name, Type::String, Qualifier::Series, required)
                }
                RequestParamKind::Bool => {
                    declared_param(name, Type::Bool, Qualifier::Series, required)
                }
                RequestParamKind::Int => {
                    declared_param(name, Type::Int, Qualifier::Series, required)
                }
                RequestParamKind::Numeric => {
                    numeric_qualified_param(name, Qualifier::Series, required)
                }
            }
        })
        .collect()
}

/// A `series`-accepting numeric (int-or-float) parameter of a pure
/// function.
fn numeric_param(name: &'static str, required: bool) -> ParamSpec {
    ParamSpec {
        name,
        ty: Type::Float,
        type_rule: ParamTypeRule::Numeric,
        max_qualifier: Qualifier::Series,
        required,
        int_literal_range: None,
        default_int: None,
        default_float: None,
        default_string: None,
        default_bool: None,
        default_series: None,
    }
}

/// A `series`-accepting `string` parameter of a `str.*` function.
fn string_param(name: &'static str, required: bool) -> ParamSpec {
    ParamSpec {
        name,
        ty: Type::String,
        type_rule: ParamTypeRule::StringOnly,
        max_qualifier: Qualifier::Series,
        required,
        int_literal_range: None,
        default_int: None,
        default_float: None,
        default_string: None,
        default_bool: None,
        default_series: None,
    }
}

/// A `series`-accepting stringable parameter (int/float/bool/string) —
/// `str.tostring`'s value, `str.format`'s substitution args.
fn stringable_param(name: &'static str, required: bool) -> ParamSpec {
    ParamSpec {
        name,
        ty: Type::String,
        type_rule: ParamTypeRule::Stringable,
        max_qualifier: Qualifier::Series,
        required,
        int_literal_range: None,
        default_int: None,
        default_float: None,
        default_string: None,
        default_bool: None,
        default_series: None,
    }
}

/// A `series int` parameter (`str.substring` positions, `str.repeat` count,
/// `str.replace` occurrence). Unlike `ta.*` lengths, these accept any series
/// int expression, not just literals.
fn int_series_param(name: &'static str, required: bool) -> ParamSpec {
    ParamSpec {
        name,
        ty: Type::Int,
        type_rule: ParamTypeRule::Declared,
        max_qualifier: Qualifier::Series,
        required,
        int_literal_range: None,
        default_int: None,
        default_float: None,
        default_string: None,
        default_bool: None,
        default_series: None,
    }
}

/// A `series`-accepting `color` parameter of a `color.*` function
/// (`color.new`'s color, `color.from_gradient`'s endpoint colors, the
/// `color.r/g/b/t` accessor argument).
fn color_param(name: &'static str, required: bool) -> ParamSpec {
    declared_param(name, Type::Color, Qualifier::Series, required)
}

/// Build a fixed-signature `color.*` pure function.
fn color_function(
    name: &'static str,
    func: PureFunc,
    params: Vec<ParamSpec>,
    result: PureResult,
) -> BuiltinFunction {
    let ret = match result {
        PureResult::Color => Type::Color,
        PureResult::Int => Type::Int,
        _ => Type::Unknown,
    };
    BuiltinFunction {
        name,
        params,
        ret,
        kind: BuiltinFunctionKind::Pure(func),
        pure_result: Some(result),
        variadic_max: None,
        ta_engine: None,
    }
}

/// Build a fixed-signature `str.*` pure function.
fn str_function(
    name: &'static str,
    func: PureFunc,
    params: Vec<ParamSpec>,
    result: PureResult,
) -> BuiltinFunction {
    let ret = match result {
        PureResult::String => Type::String,
        PureResult::Bool => Type::Bool,
        PureResult::Int => Type::Int,
        PureResult::Float => Type::Float,
        _ => Type::Unknown,
    };
    BuiltinFunction {
        name,
        params,
        ret,
        kind: BuiltinFunctionKind::Pure(func),
        pure_result: Some(result),
        variadic_max: None,
        ta_engine: None,
    }
}

/// Build a calendar-component function form (`year(time[, timezone])`, ...).
/// The signature is a required `series int` UNIX-ms
/// `time` plus an optional `series string` timezone (default `syminfo.timezone`,
/// applied by the VM). Returns `series int`.
fn calendar_function(name: &'static str, field: CalendarField) -> BuiltinFunction {
    str_function(
        name,
        PureFunc::Calendar(field),
        vec![
            int_series_param("time", true),
            string_param("timezone", false),
        ],
        PureResult::Int,
    )
}

/// Build the signature of a stateful `ta.*` function taking
/// `(source: series numeric, length)` and returning `series float`. When
/// `default_length` is `Some(n)`, the length argument is optional and
/// defaults to `n`; otherwise it is required. Either way it must be an int
/// literal within [`TA_LENGTH_RANGE`] (`E0219`).
fn ta_function(name: &'static str, func: TaFunc, default_length: Option<i64>) -> BuiltinFunction {
    BuiltinFunction {
        name,
        params: vec![
            // Numeric sources are accepted: int arguments are coerced
            // to float through an explicit IntToFloat node.
            declared_param("source", Type::Float, Qualifier::Series, true),
            ParamSpec {
                name: "length",
                ty: Type::Int,
                type_rule: ParamTypeRule::Declared,
                max_qualifier: Qualifier::Const,
                required: default_length.is_none(),
                int_literal_range: Some(TA_LENGTH_RANGE),
                default_int: default_length,
                default_float: None,
                default_string: None,
                default_bool: None,
                default_series: None,
            },
        ],
        ret: Type::Float,
        kind: BuiltinFunctionKind::Ta(func),
        pure_result: None,
        variadic_max: None,
        ta_engine: None,
    }
}

/// Build the signature of an **engine-backed** stateful builtin: the
/// engine parameter list drives checking, `ret` comes from the function's
/// result rule, and the classic `params` list stays empty.
fn ta_engine_function(name: &'static str, func: TaFunc, params: Vec<TaParam>) -> BuiltinFunction {
    let ret = match func.engine_result() {
        pinewasm_hir::TaResult::Float => Type::Float,
        pinewasm_hir::TaResult::Int => Type::Int,
        pinewasm_hir::TaResult::Bool => Type::Bool,
        pinewasm_hir::TaResult::Color => Type::Color,
    };
    BuiltinFunction {
        name,
        params: Vec::new(),
        ret,
        kind: BuiltinFunctionKind::Ta(func),
        pure_result: None,
        variadic_max: None,
        ta_engine: Some(params),
    }
}

/// The engine-backed stateful builtin table (`fixnan`, `math.sum`, and the
/// `ta.*` batch). One entry per function. Open assumptions are listed in
/// `docs/language-support.md`.
fn ta_engine_functions() -> Vec<BuiltinFunction> {
    let src = |name: &'static str| TaParam {
        name,
        kind: TaParamKind::Source,
    };
    let cond = |name: &'static str| TaParam {
        name,
        kind: TaParamKind::Condition,
    };
    let length = || TaParam {
        name: "length",
        kind: TaParamKind::ConstInt {
            min: TA_LENGTH_RANGE.0,
            max: TA_LENGTH_RANGE.1,
            default: None,
        },
    };
    let biased = TaParam {
        name: "biased",
        kind: TaParamKind::ConstBool {
            default: Some(true),
        },
    };
    let percentage = TaParam {
        name: "percentage",
        kind: TaParamKind::ConstPercent,
    };
    let pos_float = |name: &'static str| TaParam {
        name,
        kind: TaParamKind::ConstFloat {
            positive: true,
            optional: false,
        },
    };
    let any_float = |name: &'static str| TaParam {
        name,
        kind: TaParamKind::ConstFloat {
            positive: false,
            optional: false,
        },
    };
    let bars = |name: &'static str| TaParam {
        name,
        kind: TaParamKind::ConstInt {
            min: 1,
            max: TA_LENGTH_RANGE.1,
            default: None,
        },
    };
    let pivot_bars = |name: &'static str| TaParam {
        name,
        kind: TaParamKind::ConstInt {
            min: 0,
            max: TA_LENGTH_RANGE.1,
            default: None,
        },
    };
    let src_len = |name: &'static str, func: TaFunc| {
        ta_engine_function(name, func, vec![src("source"), length()])
    };
    let src_len_defaulted = |name: &'static str, func: TaFunc, series: BuiltinSeries| {
        ta_engine_function(
            name,
            func,
            vec![
                TaParam {
                    name: "source",
                    kind: TaParamKind::SourceDefaulted(series),
                },
                length(),
            ],
        )
    };
    vec![
        ta_engine_function("fixnan", TaFunc::Fixnan, vec![src("source")]),
        src_len("math.sum", TaFunc::MathSum),
        src_len("ta.ema", TaFunc::Ema),
        src_len("ta.rma", TaFunc::Rma),
        src_len("ta.wma", TaFunc::Wma),
        src_len("ta.vwma", TaFunc::Vwma),
        // hma's inner wma over length/2 needs length >= 2.
        ta_engine_function(
            "ta.hma",
            TaFunc::Hma,
            vec![
                src("source"),
                TaParam {
                    name: "length",
                    kind: TaParamKind::ConstInt {
                        min: 2,
                        max: TA_LENGTH_RANGE.1,
                        default: None,
                    },
                },
            ],
        ),
        ta_engine_function("ta.swma", TaFunc::Swma, vec![src("source")]),
        ta_engine_function(
            "ta.alma",
            TaFunc::Alma,
            vec![
                src("series"),
                length(),
                TaParam {
                    name: "offset",
                    kind: TaParamKind::ConstFloat {
                        positive: false,
                        optional: false,
                    },
                },
                TaParam {
                    name: "sigma",
                    kind: TaParamKind::ConstFloat {
                        positive: true,
                        optional: false,
                    },
                },
                // `floor = true` truncates the offset calculation to a whole
                // bar before weighting.
                TaParam {
                    name: "floor",
                    kind: TaParamKind::ConstBool {
                        default: Some(false),
                    },
                },
            ],
        ),
        src_len("ta.rsi", TaFunc::Rsi),
        ta_engine_function("ta.atr", TaFunc::Atr, vec![length()]),
        ta_engine_function(
            "ta.tr",
            TaFunc::Tr,
            vec![TaParam {
                name: "handle_na",
                kind: TaParamKind::ConstBool { default: None },
            }],
        ),
        ta_engine_function(
            "ta.stdev",
            TaFunc::Stdev,
            vec![src("source"), length(), biased.clone()],
        ),
        src_len("ta.dev", TaFunc::Dev),
        ta_engine_function(
            "ta.variance",
            TaFunc::Variance,
            vec![src("source"), length(), biased],
        ),
        src_len_defaulted("ta.highest", TaFunc::Highest, BuiltinSeries::High),
        src_len_defaulted("ta.lowest", TaFunc::Lowest, BuiltinSeries::Low),
        src_len_defaulted("ta.highestbars", TaFunc::HighestBars, BuiltinSeries::High),
        src_len_defaulted("ta.lowestbars", TaFunc::LowestBars, BuiltinSeries::Low),
        src_len("ta.mom", TaFunc::Mom),
        src_len("ta.roc", TaFunc::Roc),
        ta_engine_function("ta.cum", TaFunc::Cum, vec![src("source")]),
        ta_engine_function("ta.max", TaFunc::Max, vec![src("source")]),
        ta_engine_function("ta.min", TaFunc::Min, vec![src("source")]),
        ta_engine_function(
            "ta.crossover",
            TaFunc::Crossover,
            vec![src("source1"), src("source2")],
        ),
        ta_engine_function(
            "ta.crossunder",
            TaFunc::Crossunder,
            vec![src("source1"), src("source2")],
        ),
        ta_engine_function(
            "ta.cross",
            TaFunc::Cross,
            vec![src("source1"), src("source2")],
        ),
        ta_engine_function("ta.barssince", TaFunc::BarsSince, vec![cond("condition")]),
        ta_engine_function(
            "ta.valuewhen",
            TaFunc::ValueWhen,
            vec![
                cond("condition"),
                src("source"),
                TaParam {
                    name: "occurrence",
                    kind: TaParamKind::ConstInt {
                        min: 0,
                        max: 100_000,
                        default: None,
                    },
                },
            ],
        ),
        ta_engine_function(
            "ta.linreg",
            TaFunc::Linreg,
            vec![
                src("source"),
                length(),
                TaParam {
                    name: "offset",
                    kind: TaParamKind::ConstInt {
                        min: 0,
                        max: 100_000,
                        default: None,
                    },
                },
            ],
        ),
        src_len("ta.rising", TaFunc::Rising),
        src_len("ta.falling", TaFunc::Falling),
        src_len("ta.cci", TaFunc::Cci),
        // This family's source parameter is named `series`.
        ta_engine_function("ta.cmo", TaFunc::Cmo, vec![src("series"), length()]),
        src_len("ta.cog", TaFunc::Cog),
        ta_engine_function(
            "ta.correlation",
            TaFunc::Correlation,
            vec![src("source1"), src("source2"), length()],
        ),
        src_len("ta.median", TaFunc::Median),
        src_len("ta.mode", TaFunc::Mode),
        ta_engine_function(
            "ta.percentile_linear_interpolation",
            TaFunc::PercentileLinearInterpolation,
            vec![src("source"), length(), percentage.clone()],
        ),
        ta_engine_function(
            "ta.percentile_nearest_rank",
            TaFunc::PercentileNearestRank,
            vec![src("source"), length(), percentage],
        ),
        src_len("ta.percentrank", TaFunc::PercentRank),
        src_len("ta.range", TaFunc::Range),
        ta_engine_function("ta.mfi", TaFunc::Mfi, vec![src("series"), length()]),
        ta_engine_function("ta.wpr", TaFunc::Wpr, vec![length()]),
        // Variable-style builtins: empty signatures. They read market data
        // directly, are referenced as bare names (`ta.obv`), and keep
        // per-site state like every other engine function.
        ta_engine_function("ta.obv", TaFunc::Obv, vec![]),
        ta_engine_function("ta.accdist", TaFunc::AccDist, vec![]),
        ta_engine_function("ta.nvi", TaFunc::Nvi, vec![]),
        ta_engine_function("ta.pvi", TaFunc::Pvi, vec![]),
        ta_engine_function("ta.pvt", TaFunc::Pvt, vec![]),
        ta_engine_function("ta.wad", TaFunc::Wad, vec![]),
        ta_engine_function("ta.wvad", TaFunc::Wvad, vec![]),
        ta_engine_function("ta.iii", TaFunc::Iii, vec![]),
        // Bare variable `ta.vwap`; function overloads are intercepted by the
        // analyzer because `ta.vwap(source)`, `ta.vwap(source, anchor)`, and
        // tuple `ta.vwap(source, anchor, stdev_mult)` map to distinct engine
        // tags with different runtime operands.
        ta_engine_function("ta.vwap", TaFunc::Vwap, vec![]),
        ta_engine_function(
            "ta.stoch",
            TaFunc::Stoch,
            vec![src("source"), src("high"), src("low"), length()],
        ),
        ta_engine_function(
            "ta.sar",
            TaFunc::Sar,
            vec![pos_float("start"), pos_float("inc"), pos_float("max")],
        ),
        ta_engine_function(
            "ta.tsi",
            TaFunc::Tsi,
            vec![
                src("source"),
                TaParam {
                    name: "short_length",
                    kind: length().kind,
                },
                TaParam {
                    name: "long_length",
                    kind: length().kind,
                },
            ],
        ),
        src_len("ta.rci", TaFunc::Rci),
        ta_engine_function(
            "ta.bbw",
            TaFunc::Bbw,
            vec![src("series"), length(), any_float("mult")],
        ),
        ta_engine_function(
            "ta.kcw",
            TaFunc::Kcw,
            vec![
                src("series"),
                length(),
                any_float("mult"),
                TaParam {
                    name: "useTrueRange",
                    kind: TaParamKind::ConstBool {
                        default: Some(true),
                    },
                },
            ],
        ),
        ta_engine_function(
            "ta.pivothigh",
            TaFunc::PivotHigh,
            vec![
                TaParam {
                    name: "source",
                    kind: TaParamKind::SourceDefaulted(BuiltinSeries::High),
                },
                pivot_bars("leftbars"),
                pivot_bars("rightbars"),
            ],
        ),
        ta_engine_function(
            "ta.pivotlow",
            TaFunc::PivotLow,
            vec![
                TaParam {
                    name: "source",
                    kind: TaParamKind::SourceDefaulted(BuiltinSeries::Low),
                },
                pivot_bars("leftbars"),
                pivot_bars("rightbars"),
            ],
        ),
        // `timeframe.change(timeframe)` — a stateful bool. Registered as an
        // engine function for its signature and argument binding only: the
        // analyzer lowers every call onto `time(timeframe)`
        // (`ta.rs`, `analyze_timeframe_change`), so `TaFunc::TimeframeChange`
        // is never emitted.
        ta_engine_function(
            "timeframe.change",
            TaFunc::TimeframeChange,
            vec![TaParam {
                name: "timeframe",
                kind: TaParamKind::ConstTimeframe { default: None },
            }],
        ),
        // Tuple-returning engine functions (`[a, b, c] = f(...)`). Each pushes
        // three values; only a destructuring declaration may consume them.
        ta_engine_function(
            "ta.macd",
            TaFunc::Macd,
            vec![
                src("source"),
                TaParam {
                    name: "fastlen",
                    kind: length().kind,
                },
                TaParam {
                    name: "slowlen",
                    kind: length().kind,
                },
                TaParam {
                    name: "siglen",
                    kind: length().kind,
                },
            ],
        ),
        ta_engine_function(
            "ta.bb",
            TaFunc::Bb,
            vec![src("series"), length(), any_float("mult")],
        ),
        ta_engine_function(
            "ta.kc",
            TaFunc::Kc,
            vec![
                src("series"),
                length(),
                any_float("mult"),
                TaParam {
                    name: "useTrueRange",
                    kind: TaParamKind::ConstBool {
                        default: Some(true),
                    },
                },
            ],
        ),
        ta_engine_function(
            "ta.dmi",
            TaFunc::Dmi,
            vec![bars("diLength"), bars("adxSmoothing")],
        ),
        // ta.supertrend(factor, atrPeriod) → [supertrend, direction]. `factor`
        // is any finite float multiplier; `atrPeriod` is a length.
        ta_engine_function(
            "ta.supertrend",
            TaFunc::Supertrend,
            vec![
                any_float("factor"),
                TaParam {
                    name: "atrPeriod",
                    kind: length().kind,
                },
            ],
        ),
    ]
}

/// One-numeric-argument, float-result pure function (`math.sqrt`, the
/// trig family, ...). `param` is the parameter name, which differs across
/// the family (`number` for the algebraic functions, `angle` for the trig
/// ones), and is load-bearing: scripts may pass it as a named argument.
fn float_unary(name: &'static str, param: &'static str, func: PureFunc) -> BuiltinFunction {
    BuiltinFunction {
        name,
        params: vec![numeric_param(param, true)],
        ret: Type::Float,
        kind: BuiltinFunctionKind::Pure(func),
        pure_result: Some(PureResult::Float),
        variadic_max: None,
        ta_engine: None,
    }
}

/// One-numeric-argument pure function with a non-`Float` result rule
/// (`math.abs`/`math.sign` preserve, `math.floor`/`math.ceil` return int).
fn numeric_unary(
    name: &'static str,
    param: &'static str,
    func: PureFunc,
    result: PureResult,
) -> BuiltinFunction {
    BuiltinFunction {
        name,
        params: vec![numeric_param(param, true)],
        ret: Type::Unknown,
        kind: BuiltinFunctionKind::Pure(func),
        pure_result: Some(result),
        variadic_max: None,
        ta_engine: None,
    }
}

/// Variadic numeric pure function (`math.min`/`math.max`/`math.avg`):
/// 2..=[`PURE_VARIADIC_MAX`] arguments.
fn variadic_numeric(name: &'static str, func: PureFunc, result: PureResult) -> BuiltinFunction {
    BuiltinFunction {
        name,
        params: vec![
            numeric_param("number0", true),
            numeric_param("number1", true),
        ],
        ret: Type::Unknown,
        kind: BuiltinFunctionKind::Pure(func),
        pure_result: Some(result),
        variadic_max: Some(PURE_VARIADIC_MAX),
        ta_engine: None,
    }
}

/// The pure-builtin table. One entry per function; the checker, validator,
/// and VM all key off the [`PureFunc`] tag.
fn pure_functions() -> Vec<BuiltinFunction> {
    let nullable_param = |name: &'static str, required: bool| ParamSpec {
        name,
        ty: Type::Float,
        type_rule: ParamTypeRule::NumericOrColor,
        max_qualifier: Qualifier::Series,
        required,
        int_literal_range: None,
        default_int: None,
        default_float: None,
        default_string: None,
        default_bool: None,
        default_series: None,
    };
    vec![
        BuiltinFunction {
            name: "na",
            params: vec![ParamSpec {
                name: "x",
                ty: Type::Float,
                type_rule: ParamTypeRule::NonBooleanValue,
                max_qualifier: Qualifier::Series,
                required: true,
                int_literal_range: None,
                default_int: None,
                default_float: None,
                default_string: None,
                default_bool: None,
                default_series: None,
            }],
            ret: Type::Bool,
            kind: BuiltinFunctionKind::Pure(PureFunc::Na),
            pure_result: Some(PureResult::Bool),
            variadic_max: None,
            ta_engine: None,
        },
        BuiltinFunction {
            name: "nz",
            params: vec![
                nullable_param("source", true),
                nullable_param("replacement", false),
            ],
            ret: Type::Unknown,
            kind: BuiltinFunctionKind::Pure(PureFunc::Nz),
            pure_result: Some(PureResult::Nz),
            variadic_max: None,
            ta_engine: None,
        },
        numeric_unary(
            "math.abs",
            "number",
            PureFunc::MathAbs,
            PureResult::Preserve,
        ),
        numeric_unary(
            "math.sign",
            "number",
            PureFunc::MathSign,
            PureResult::Preserve,
        ),
        numeric_unary("math.floor", "number", PureFunc::MathFloor, PureResult::Int),
        numeric_unary("math.ceil", "number", PureFunc::MathCeil, PureResult::Int),
        BuiltinFunction {
            name: "math.round",
            params: vec![
                numeric_param("number", true),
                declared_param("precision", Type::Int, Qualifier::Series, false),
            ],
            ret: Type::Unknown,
            kind: BuiltinFunctionKind::Pure(PureFunc::MathRound),
            pure_result: Some(PureResult::Round),
            variadic_max: None,
            ta_engine: None,
        },
        float_unary("math.sqrt", "number", PureFunc::MathSqrt),
        float_unary("math.exp", "number", PureFunc::MathExp),
        float_unary("math.log", "number", PureFunc::MathLog),
        float_unary("math.log10", "number", PureFunc::MathLog10),
        float_unary("math.sin", "angle", PureFunc::MathSin),
        float_unary("math.cos", "angle", PureFunc::MathCos),
        float_unary("math.tan", "angle", PureFunc::MathTan),
        float_unary("math.asin", "angle", PureFunc::MathAsin),
        float_unary("math.acos", "angle", PureFunc::MathAcos),
        float_unary("math.atan", "angle", PureFunc::MathAtan),
        float_unary("math.todegrees", "radians", PureFunc::MathToDegrees),
        float_unary("math.toradians", "degrees", PureFunc::MathToRadians),
        float_unary(
            "math.round_to_mintick",
            "number",
            PureFunc::MathRoundToMintick,
        ),
        BuiltinFunction {
            name: "math.pow",
            params: vec![numeric_param("base", true), numeric_param("exponent", true)],
            ret: Type::Float,
            kind: BuiltinFunctionKind::Pure(PureFunc::MathPow),
            pure_result: Some(PureResult::Float),
            variadic_max: None,
            ta_engine: None,
        },
        variadic_numeric("math.min", PureFunc::MathMin, PureResult::IntIfAllInt),
        variadic_numeric("math.max", PureFunc::MathMax, PureResult::IntIfAllInt),
        variadic_numeric("math.avg", PureFunc::MathAvg, PureResult::Float),
        // ---- str.* namespace -----------------------------------------
        str_function(
            "str.tostring",
            PureFunc::StrTostring,
            vec![
                stringable_param("value", true),
                string_param("format", false),
            ],
            PureResult::String,
        ),
        str_function(
            "str.tonumber",
            PureFunc::StrTonumber,
            vec![string_param("string", true)],
            // Parse result is float-or-na.
            PureResult::Float,
        ),
        BuiltinFunction {
            // format(formatString, arg0, ...): series format + up to N stringable
            // substitution args (variadic). The last declared parameter repeats.
            name: "str.format",
            params: vec![
                string_param("formatString", true),
                stringable_param("arg", true),
            ],
            ret: Type::String,
            kind: BuiltinFunctionKind::Pure(PureFunc::StrFormat),
            pure_result: Some(PureResult::String),
            variadic_max: Some(STR_FORMAT_MAX_ARGS as usize),
            ta_engine: None,
        },
        str_function(
            "str.length",
            PureFunc::StrLength,
            vec![string_param("string", true)],
            PureResult::Int,
        ),
        str_function(
            "str.substring",
            PureFunc::StrSubstring,
            vec![
                string_param("source", true),
                int_series_param("begin_pos", true),
                // Optional end position; no default (its absence selects the
                // 2-argument VM form).
                int_series_param("end_pos", false),
            ],
            PureResult::String,
        ),
        str_function(
            "str.replace",
            PureFunc::StrReplace,
            vec![
                string_param("source", true),
                string_param("target", true),
                string_param("replacement", true),
                // occurrence defaults to 0 (materialized by the checker).
                ParamSpec {
                    default_int: Some(0),
                    ..int_series_param("occurrence", false)
                },
            ],
            PureResult::String,
        ),
        str_function(
            "str.replace_all",
            PureFunc::StrReplaceAll,
            vec![
                string_param("source", true),
                string_param("target", true),
                string_param("replacement", true),
            ],
            PureResult::String,
        ),
        str_function(
            "str.contains",
            PureFunc::StrContains,
            vec![string_param("source", true), string_param("str", true)],
            PureResult::Bool,
        ),
        str_function(
            "str.startswith",
            PureFunc::StrStartswith,
            vec![string_param("source", true), string_param("str", true)],
            PureResult::Bool,
        ),
        str_function(
            "str.endswith",
            PureFunc::StrEndswith,
            vec![string_param("source", true), string_param("str", true)],
            PureResult::Bool,
        ),
        str_function(
            "str.pos",
            PureFunc::StrPos,
            vec![string_param("source", true), string_param("str", true)],
            PureResult::Int,
        ),
        str_function(
            "str.upper",
            PureFunc::StrUpper,
            vec![string_param("source", true)],
            PureResult::String,
        ),
        str_function(
            "str.lower",
            PureFunc::StrLower,
            vec![string_param("source", true)],
            PureResult::String,
        ),
        str_function(
            "str.trim",
            PureFunc::StrTrim,
            vec![string_param("source", true)],
            PureResult::String,
        ),
        str_function(
            "str.repeat",
            PureFunc::StrRepeat,
            vec![
                string_param("source", true),
                int_series_param("repeat", true),
                // Optional separator; no default (absence selects the
                // 2-argument VM form).
                string_param("separator", false),
            ],
            PureResult::String,
        ),
        str_function(
            "str.match",
            PureFunc::StrMatch,
            vec![string_param("source", true), string_param("regex", true)],
            PureResult::String,
        ),
        str_function(
            "str.format_time",
            PureFunc::StrFormatTime,
            vec![
                // epoch-ms UNIX time (int series).
                int_series_param("time", true),
                // Both options accept per-bar strings. Trailing omissions are
                // defaulted by the VM; a named `timezone` that skips `format`
                // receives the default here.
                ParamSpec {
                    default_string: Some(pinewasm_hir::STR_FORMAT_TIME_DEFAULT_FORMAT),
                    ..string_param("format", false)
                },
                string_param("timezone", false),
            ],
            PureResult::String,
        ),
        // `weekofyear(time[, timezone])` — the ISO 8601 week-of-year
        // for a UNIX-ms time. Mirrors `str.format_time`'s (time, timezone)
        // surface; the bare `weekofyear` *variable* is a separate BuiltinSeries.
        str_function(
            "weekofyear",
            PureFunc::WeekOfYear,
            vec![
                int_series_param("time", true),
                string_param("timezone", false),
            ],
            PureResult::Int,
        ),
        // The FUNCTION forms of the calendar builtins —
        // `year(time[, timezone])`, `month(...)`, ... — mirroring `weekofyear`.
        // Each decomposes an arbitrary UNIX-ms `time` into the named civil
        // component in the given fixed-offset timezone (default
        // `syminfo.timezone`, applied by the VM). The bare `year`/`month`/...
        // *variables* are separate BuiltinSeries decomposing the bar's open
        // time. Signature: `(time, timezone) → series int`.
        calendar_function("year", CalendarField::Year),
        calendar_function("month", CalendarField::Month),
        calendar_function("dayofmonth", CalendarField::DayOfMonth),
        calendar_function("dayofweek", CalendarField::DayOfWeek),
        calendar_function("hour", CalendarField::Hour),
        calendar_function("minute", CalendarField::Minute),
        calendar_function("second", CalendarField::Second),
        BuiltinFunction {
            name: "math.random",
            params: vec![
                ParamSpec {
                    default_float: Some(0.0),
                    ..numeric_param("min", false)
                },
                ParamSpec {
                    default_float: Some(1.0),
                    ..numeric_param("max", false)
                },
                declared_param("seed", Type::Int, Qualifier::Series, false),
            ],
            ret: Type::Float,
            kind: BuiltinFunctionKind::Pure(PureFunc::MathRandom),
            pure_result: Some(PureResult::Float),
            variadic_max: None,
            ta_engine: None,
        },
        // ---- color.* namespace ---------------------------------------
        // new(color, transp): transparency 0..=100 (clamped at runtime). Transparency
        // accepts a float (256 underlying alpha values), so the
        // param is numeric (int-or-float); the runtime rounds+clamps.
        color_function(
            "color.new",
            PureFunc::ColorNew,
            vec![color_param("color", true), numeric_param("transp", true)],
            PureResult::Color,
        ),
        // rgb(red, green, blue, transp = 0): channels 0..=255, transp
        // 0..=100 (all rounded+clamped at runtime). Float channels and
        // transparency are accepted (e.g. `color.rgb(math.random(0,255), …)`), so the
        // params are numeric; the transp default is materialized by the
        // checker as a float argument.
        color_function(
            "color.rgb",
            PureFunc::ColorRgb,
            vec![
                numeric_param("red", true),
                numeric_param("green", true),
                numeric_param("blue", true),
                ParamSpec {
                    default_float: Some(0.0),
                    ..numeric_param("transp", false)
                },
            ],
            PureResult::Color,
        ),
        // from_gradient(value, bottom_value, top_value, bottom_color,
        // top_color): numeric position + two endpoint colors.
        color_function(
            "color.from_gradient",
            PureFunc::ColorFromGradient,
            vec![
                numeric_param("value", true),
                numeric_param("bottom_value", true),
                numeric_param("top_value", true),
                color_param("bottom_color", true),
                color_param("top_color", true),
            ],
            PureResult::Color,
        ),
        // The color.r, color.g, color.b and color.t accessors return floats.
        // Earlier serialized integer opcodes are still handled by the VM but are never
        // emitted for source.
        color_function(
            "color.r",
            PureFunc::ColorRFloat,
            vec![color_param("color", true)],
            PureResult::Float,
        ),
        color_function(
            "color.g",
            PureFunc::ColorGFloat,
            vec![color_param("color", true)],
            PureResult::Float,
        ),
        color_function(
            "color.b",
            PureFunc::ColorBFloat,
            vec![color_param("color", true)],
            PureResult::Float,
        ),
        color_function(
            "color.t",
            PureFunc::ColorTFloat,
            vec![color_param("color", true)],
            PureResult::Float,
        ),
        // ---- timeframe.* functions -----------------------------------
        // The omitted timeframe is the current chart context, not a fixed
        // duration: materialize the same read as explicit `timeframe.period`.
        str_function(
            "timeframe.in_seconds",
            PureFunc::TimeframeInSeconds,
            vec![ParamSpec {
                default_series: Some(BuiltinSeries::TimeframePeriod),
                ..string_param("timeframe", false)
            }],
            PureResult::Int,
        ),
        str_function(
            "timeframe.from_seconds",
            PureFunc::TimeframeFromSeconds,
            vec![int_series_param("seconds", true)],
            PureResult::String,
        ),
        // ---- timestamp() ---------------------------------------------
        // Registered with the UTC overload's signature (six numeric
        // date/time fields; the last three optional). The dedicated analyzer
        // additionally dispatches the leading-`timezone` numeric form and
        // folds the one-string calendar-date form to a const epoch value.
        BuiltinFunction {
            name: "timestamp",
            params: vec![
                int_series_param("year", true),
                int_series_param("month", true),
                int_series_param("day", true),
                ParamSpec {
                    default_int: Some(0),
                    ..int_series_param("hour", false)
                },
                ParamSpec {
                    default_int: Some(0),
                    ..int_series_param("minute", false)
                },
                ParamSpec {
                    default_int: Some(0),
                    ..int_series_param("second", false)
                },
            ],
            ret: Type::Int,
            kind: BuiltinFunctionKind::Pure(PureFunc::Timestamp),
            pure_result: Some(PureResult::Int),
            variadic_max: None,
            ta_engine: None,
        },
    ]
    .into_iter()
    .chain(strategy_trade_functions())
    .chain(ticker_functions())
    .collect()
}

/// The nine `ticker.*` interface functions. Each folds its arguments
/// into a deterministic canonical ticker-id **string** (real symbology
/// resolution is a host concern). Modeled as pure string
/// functions, na-propagating.
///
/// Each ticker function accepts `simple` or `series` arguments: the result
/// qualifier is the strongest argument qualifier, so `series` arguments give a per-bar
/// `series string`. Only the `backadjustment` and `settlement_as_close`
/// selector tokens are `simple` in both overloads.
fn ticker_functions() -> Vec<BuiltinFunction> {
    // A string parameter (a symbol, ticker id, session or style) that takes
    // `series` values. Optional ones carry an empty-string gap default so a
    // named argument may skip them (`ticker.new(p, t, backadjustment = ...)`).
    let sym = |name: &'static str, required: bool| ParamSpec {
        default_string: (!required).then_some(""),
        ..string_param(name, required)
    };
    // A `simple` selector-token parameter (`backadjustment.*`,
    // `settlement_as_close.*`), `simple` in both overload families.
    let token = |name: &'static str| ParamSpec {
        max_qualifier: Qualifier::Simple,
        ..sym(name, false)
    };
    // A required count (`ticker.linebreak`'s `number_of_lines`,
    // `ticker.pointfigure`'s `reversal`): `int` in both overload families,
    // so a float is refused rather than
    // read as int-or-float like a box size or price amount.
    let count = |name: &'static str| declared_param(name, Type::Int, Qualifier::Series, true);
    // A bool parameter (`ticker.renko`'s `request_wicks`). An optional one
    // carries a `false` gap default so a named argument may skip it to reach
    // `source`; a trailing omission still emits no argument.
    let flag = |name: &'static str, required: bool| ParamSpec {
        default_bool: (!required).then_some(false),
        ..declared_param(name, Type::Bool, Qualifier::Series, required)
    };
    vec![
        str_function(
            "ticker.new",
            PureFunc::Ticker(TickerFunc::New),
            vec![
                sym("prefix", true),
                sym("ticker", true),
                sym("session", false),
                sym("adjustment", false),
                // `backadjustment.*` / `settlement_as_close.*` const tokens.
                // Each is a separate parameter, not a value passed through the
                // `adjustment` slot.
                token("backadjustment"),
                token("settlement_as_close"),
            ],
            PureResult::String,
        ),
        str_function(
            "ticker.modify",
            PureFunc::Ticker(TickerFunc::Modify),
            vec![
                sym("tickerid", true),
                sym("session", false),
                sym("adjustment", false),
                token("backadjustment"),
                token("settlement_as_close"),
            ],
            PureResult::String,
        ),
        str_function(
            "ticker.heikinashi",
            PureFunc::Ticker(TickerFunc::Heikinashi),
            vec![sym("symbol", true)],
            PureResult::String,
        ),
        // The style-bearing overload is selected before argument binding in
        // analyze_pure_call. Both families also accept series arguments.
        str_function(
            "ticker.kagi",
            PureFunc::Ticker(TickerFunc::Kagi),
            vec![
                string_param("symbol", true),
                numeric_param("reversal", true),
            ],
            PureResult::String,
        ),
        str_function(
            "ticker.linebreak",
            PureFunc::Ticker(TickerFunc::Linebreak),
            vec![sym("symbol", true), count("number_of_lines")],
            PureResult::String,
        ),
        str_function(
            "ticker.pointfigure",
            PureFunc::Ticker(TickerFunc::Pointfigure),
            vec![
                sym("symbol", true),
                sym("source", true),
                sym("style", true),
                numeric_param("param", true),
                count("reversal"),
            ],
            PureResult::String,
        ),
        str_function(
            "ticker.renko",
            PureFunc::Ticker(TickerFunc::Renko),
            vec![
                sym("symbol", true),
                sym("style", true),
                numeric_param("param", true),
                flag("request_wicks", false),
                sym("source", false),
            ],
            PureResult::String,
        ),
        str_function(
            "ticker.standard",
            PureFunc::Ticker(TickerFunc::Standard),
            // `symbol` is optional, defaulting to the chart's own
            // symbol. The canonical id has no way to say "current", so an
            // omitted argument materializes an explicit `syminfo.tickerid`
            // read rather than folding to a bare `standard` prefix.
            vec![ParamSpec {
                default_series: Some(BuiltinSeries::SyminfoTickerid),
                ..sym("symbol", false)
            }],
            PureResult::String,
        ),
        str_function(
            "ticker.inherit",
            PureFunc::Ticker(TickerFunc::Inherit),
            vec![sym("from_tickerid", true), sym("symbol", true)],
            PureResult::String,
        ),
        // `syminfo.prefix` / `syminfo.ticker` also have a *function* form
        // alongside the same-named variables: they split a ticker id on its
        // first `:`. The names coexist because a call site resolves against the
        // function table and a bare read against the constant table.
        str_function(
            "syminfo.prefix",
            PureFunc::SyminfoPrefix,
            vec![sym("symbol", true)],
            PureResult::String,
        ),
        str_function(
            "syminfo.ticker",
            PureFunc::SyminfoTicker,
            vec![sym("symbol", true)],
            PureResult::String,
        ),
    ]
}

/// Build one `strategy.*trades.<field>(trade_num)` accessor. `trade_num` is a
/// `series int` index into the broker's stored trade list; the result type
/// follows the field (string ids/comments, int bar-indices/times, else float).
fn trade_accessor(name: &'static str, func: PureFunc, field: TradeAccessor) -> BuiltinFunction {
    let (ret, result) = match field.result_type() {
        Type::String => (Type::String, PureResult::String),
        Type::Int => (Type::Int, PureResult::Int),
        _ => (Type::Float, PureResult::Float),
    };
    BuiltinFunction {
        name,
        params: vec![int_series_param("trade_num", true)],
        ret,
        kind: BuiltinFunctionKind::Pure(func),
        pure_result: Some(result),
        variadic_max: None,
        ta_engine: None,
    }
}

/// The `strategy.closedtrades.*` / `strategy.opentrades.*` trade accessors and
/// the `strategy.convert_to_account/symbol` conversions. Each reads
/// broker state and requires a `strategy()` declaration (E0241, enforced in the
/// analyzer via `PureFunc::is_strategy`). An out-of-range `trade_num` reads
/// `na` at runtime (never a panic).
fn strategy_trade_functions() -> Vec<BuiltinFunction> {
    use TradeAccessor::{
        Commission, EntryBarIndex, EntryComment, EntryId, EntryPrice, EntryTime, ExitBarIndex,
        ExitComment, ExitId, ExitPrice, ExitTime, MaxDrawdown, MaxDrawdownPercent, MaxRunup,
        MaxRunupPercent, Profit, ProfitPercent, Size,
    };
    // Closed-trade fields expose the full set; open-trade fields omit exit_*.
    let closed = [
        EntryPrice,
        ExitPrice,
        Size,
        Profit,
        ProfitPercent,
        Commission,
        EntryBarIndex,
        ExitBarIndex,
        EntryTime,
        ExitTime,
        MaxRunup,
        MaxRunupPercent,
        MaxDrawdown,
        MaxDrawdownPercent,
        EntryId,
        ExitId,
        EntryComment,
        ExitComment,
    ];
    let open = [
        EntryPrice,
        Size,
        Profit,
        ProfitPercent,
        Commission,
        EntryBarIndex,
        EntryTime,
        MaxRunup,
        MaxRunupPercent,
        MaxDrawdown,
        MaxDrawdownPercent,
        EntryId,
        EntryComment,
    ];
    let mut out = Vec::new();
    for field in closed {
        out.push(trade_accessor(
            field.closed_name(),
            PureFunc::StrategyClosedTrade(field),
            field,
        ));
    }
    for field in open {
        out.push(trade_accessor(
            field.open_name(),
            PureFunc::StrategyOpenTrade(field),
            field,
        ));
    }
    // Currency conversions: numeric in, float out.
    out.push(BuiltinFunction {
        name: "strategy.convert_to_account",
        params: vec![numeric_param("value", true)],
        ret: Type::Float,
        kind: BuiltinFunctionKind::Pure(PureFunc::StrategyConvertToAccount),
        pure_result: Some(PureResult::Float),
        variadic_max: None,
        ta_engine: None,
    });
    out.push(BuiltinFunction {
        name: "strategy.convert_to_symbol",
        params: vec![numeric_param("value", true)],
        ret: Type::Float,
        kind: BuiltinFunctionKind::Pure(PureFunc::StrategyConvertToSymbol),
        pure_result: Some(PureResult::Float),
        variadic_max: None,
        ta_engine: None,
    });
    out.push(BuiltinFunction {
        name: "strategy.default_entry_qty",
        params: vec![numeric_param("fill_price", true)],
        ret: Type::Float,
        kind: BuiltinFunctionKind::Pure(PureFunc::StrategyDefaultEntryQty),
        pure_result: Some(PureResult::Float),
        variadic_max: None,
        ta_engine: None,
    });
    out
}

/// Build one `input.*` registry entry. The `params` list is the ordered
/// parameter-name set for named-argument binding and did-you-mean
/// suggestions; kind-specific type/constraint checking is done by the
/// analyzer. `ret` is the surface return type (the input read's scalar type).
fn input_function(name: &'static str, kind: InputKind, params: Vec<ParamSpec>) -> BuiltinFunction {
    BuiltinFunction {
        name,
        params,
        ret: input_kind_type(kind),
        kind: BuiltinFunctionKind::Input(kind),
        pure_result: None,
        variadic_max: None,
        ta_engine: None,
    }
}

/// A named parameter slot used only for `input.*` argument binding (types are
/// checked by the analyzer's kind-specific rules, not by `check_call_args`).
fn input_param(name: &'static str, ty: Type, required: bool) -> ParamSpec {
    ParamSpec {
        name,
        ty,
        type_rule: ParamTypeRule::Declared,
        // Inputs require const arguments; the analyzer enforces this with a
        // dedicated E0230 diagnostic (a plain E0206 would be less specific).
        max_qualifier: Qualifier::Const,
        required,
        int_literal_range: None,
        default_int: None,
        default_float: None,
        default_string: None,
        default_bool: None,
        default_series: None,
    }
}

/// `active` is the sole common input parameter that accepts an input-qualified
/// value. The dedicated input analyzer performs the final bool/type check, but
/// retaining the Input qualifier here keeps the registry accurate for
/// signature consumers.
fn input_active_param() -> ParamSpec {
    ParamSpec {
        name: "active",
        ty: Type::Bool,
        type_rule: ParamTypeRule::Declared,
        max_qualifier: Qualifier::Input,
        required: false,
        int_literal_range: None,
        default_int: None,
        default_float: None,
        default_string: None,
        default_bool: None,
        default_series: None,
    }
}

/// Materialize the generic `input()` source overload from its registered
/// scalar catalog. The source form has a distinct positional tail order and a
/// series-qualified float `defval`; keeping that transformation in the Rust
/// registry layer prevents editor generators from re-declaring its types.
pub fn generic_input_source_params(function: &BuiltinFunction) -> Vec<ParamSpec> {
    assert!(matches!(function.kind, BuiltinFunctionKind::InputGeneric));
    GENERIC_INPUT_SOURCE_PARAM_NAMES
        .iter()
        .map(|&name| {
            let mut param = function
                .params
                .iter()
                .find(|param| param.name == name)
                .unwrap_or_else(|| panic!("generic input metadata is missing `{name}`"))
                .clone();
            if name == "defval" {
                param.ty = Type::Float;
                param.max_qualifier = Qualifier::Series;
            }
            param
        })
        .collect()
}

/// The `input.*` table. Numeric inputs expose bounds/options, string and enum
/// inputs preserve dropdown options, and every kind accepts the implemented
/// metadata tail (tooltip, inline, group, confirm, display, active).
/// `input.price`/`input.time` are host-override-or-default interfaces (chart
/// interaction is a host concern); generic `input()` dispatches to the matching namespaced kind.
fn input_functions() -> Vec<BuiltinFunction> {
    let standard_tail = |confirm: bool| {
        let mut params = vec![
            input_param("tooltip", Type::String, false),
            input_param("inline", Type::String, false),
            input_param("group", Type::String, false),
        ];
        if confirm {
            params.push(input_param("confirm", Type::Bool, false));
        }
        params.push(input_param("display", Type::PlotDisplay, false));
        params.push(input_active_param());
        params
    };
    // The registry stores a complete numeric parameter-name catalog. The
    // analyzer binds the two positional overloads independently:
    // `(defval,title,options,...)` and
    // `(defval,title,minval,maxval,step,...)`.
    let numeric_params = |ty: Type| {
        let mut params = vec![
            input_param("defval", ty.clone(), true),
            input_param("title", Type::String, false),
            input_param("options", ty.clone(), false),
            input_param("minval", ty.clone(), false),
            input_param("maxval", ty.clone(), false),
            input_param("step", ty.clone(), false),
        ];
        params.extend(standard_tail(true));
        params
    };
    let options_params = |ty: Type| {
        let mut params = vec![
            input_param("defval", ty.clone(), true),
            input_param("title", Type::String, false),
            input_param("options", ty, false),
        ];
        params.extend(standard_tail(true));
        params
    };
    let simple_params = |ty: Type| {
        let mut params = vec![
            input_param("defval", ty, true),
            input_param("title", Type::String, false),
        ];
        params.extend(standard_tail(true));
        params
    };
    // `input.text_area` uniquely omits `inline` and `options`.
    let text_area_params = || {
        let mut params = vec![
            input_param("defval", Type::String, true),
            input_param("title", Type::String, false),
            input_param("tooltip", Type::String, false),
            input_param("group", Type::String, false),
            input_param("confirm", Type::Bool, false),
            input_param("display", Type::PlotDisplay, false),
        ];
        params.push(input_active_param());
        params
    };
    // `input.source` places `confirm` after `active`, unlike every other
    // namespaced input function.
    let source_params = || {
        let mut params = vec![
            input_param("defval", Type::Float, true),
            input_param("title", Type::String, false),
            input_param("tooltip", Type::String, false),
            input_param("inline", Type::String, false),
            input_param("group", Type::String, false),
            input_param("display", Type::PlotDisplay, false),
        ];
        params.push(input_active_param());
        params.push(input_param("confirm", Type::Bool, false));
        params
    };
    vec![
        input_function("input.int", InputKind::Int, numeric_params(Type::Int)),
        input_function("input.float", InputKind::Float, numeric_params(Type::Float)),
        input_function("input.bool", InputKind::Bool, simple_params(Type::Bool)),
        input_function(
            "input.string",
            InputKind::String,
            options_params(Type::String),
        ),
        // text_area / timeframe / session / symbol are all `input string`
        // (format validation for the latter three is a known gap).
        input_function("input.text_area", InputKind::String, text_area_params()),
        input_function(
            "input.timeframe",
            InputKind::String,
            options_params(Type::String),
        ),
        input_function(
            "input.session",
            InputKind::String,
            options_params(Type::String),
        ),
        input_function(
            "input.symbol",
            InputKind::String,
            simple_params(Type::String),
        ),
        input_function("input.color", InputKind::Color, simple_params(Type::Color)),
        input_function("input.source", InputKind::Source, source_params()),
        // `input.price` — a float default or a host override
        // (chart interaction is a host concern).
        input_function("input.price", InputKind::Price, simple_params(Type::Float)),
        // `input.time` — an int (epoch ms) default or a host override.
        input_function("input.time", InputKind::Time, simple_params(Type::Int)),
        // `input.enum(defval, ...)` — an enum-typed input. The `defval` is an
        // enum member; the analyzer resolves it against the declared enums and
        // types the read as the declaring `Type::Enum`. The `defval` param type
        // is a placeholder (the analyzer does enum-member resolution).
        input_function("input.enum", InputKind::Enum, options_params(Type::Int)),
        // Generic `input(defval, ...)` — dispatches on the `defval` type to the
        // matching namespaced input. Same `defval, title` surface as the simple
        // kinds; numeric constraints belong to the namespaced forms.
        BuiltinFunction {
            name: "input",
            // Generic `input()` has no `confirm` parameter. The source
            // overload's positional tooltip/inline order is selected by the
            // dedicated analyzer after it infers the defval kind.
            params: {
                let mut params = vec![
                    input_param("defval", Type::Int, true),
                    input_param("title", Type::String, false),
                ];
                params.extend(standard_tail(false));
                params
            },
            ret: Type::Int,
            kind: BuiltinFunctionKind::InputGeneric,
            pure_result: None,
            variadic_max: None,
            ta_engine: None,
        },
    ]
}

/// One `array.*` collection registry entry. `params` is empty: collection
/// functions have receiver-dependent typing (`array.get` returns the
/// receiver's element type), so `analyze_collection_call` does the checking
/// directly rather than the generic parameter framework. `ret` is a
/// placeholder (the analyzer computes the real result type per call).
fn collection_function(func: CollectionFunc) -> BuiltinFunction {
    let (names, required) = func.surface_params();
    BuiltinFunction {
        name: func.name(),
        params: names
            .iter()
            .enumerate()
            .map(|(index, name)| ParamSpec {
                name,
                ty: Type::Unknown,
                type_rule: ParamTypeRule::AnyValue,
                max_qualifier: Qualifier::Series,
                required: index < required,
                int_literal_range: None,
                default_int: None,
                default_float: None,
                default_string: None,
                default_bool: None,
                default_series: None,
            })
            .collect(),
        ret: Type::Void,
        kind: BuiltinFunctionKind::Collection(func),
        pure_result: None,
        variadic_max: (func == CollectionFunc::ArrayFrom).then_some(ARRAY_FROM_MAX_ARGS as usize),
        ta_engine: None,
    }
}

/// The `array.*` collection table: the namespace call form with typed
/// constructors. Method syntax and scalar `<type>` generics resolve to these names.
fn collection_functions() -> Vec<BuiltinFunction> {
    use CollectionFunc::{
        ArrayAbs, ArrayAvg, ArrayBinarySearch, ArrayBinarySearchLeftmost,
        ArrayBinarySearchRightmost, ArrayClear, ArrayConcat, ArrayCopy, ArrayCovariance,
        ArrayEvery, ArrayFill, ArrayFirst, ArrayFrom, ArrayGet, ArrayIncludes, ArrayIndexOf,
        ArrayInsert, ArrayJoin, ArrayLast, ArrayLastIndexOf, ArrayMax, ArrayMedian, ArrayMin,
        ArrayMode, ArrayNewBool, ArrayNewColor, ArrayNewFloat, ArrayNewInt, ArrayNewString,
        ArrayPercentRank, ArrayPercentileLinearInterpolation, ArrayPercentileNearestRank, ArrayPop,
        ArrayPush, ArrayRange, ArrayRemove, ArrayReverse, ArraySet, ArrayShift, ArraySize,
        ArraySlice, ArraySome, ArraySort, ArraySortIndices, ArrayStandardize, ArrayStdev, ArraySum,
        ArrayUnshift, ArrayVariance, StrSplit,
    };
    [
        ArrayNewFloat,
        ArrayNewInt,
        ArrayNewBool,
        ArrayNewString,
        ArrayNewColor,
        ArrayFrom,
        // `str.split` returns array<string>; it rides the collection machinery.
        StrSplit,
        ArrayPush,
        ArrayPop,
        ArrayGet,
        ArraySet,
        ArraySize,
        ArrayClear,
        ArrayShift,
        ArrayUnshift,
        ArrayInsert,
        ArrayRemove,
        ArrayIncludes,
        ArrayIndexOf,
        ArrayLastIndexOf,
        ArrayReverse,
        ArrayFirst,
        ArrayLast,
        ArrayConcat,
        ArraySlice,
        ArrayCopy,
        ArrayFill,
        ArrayJoin,
        // Aggregates / transforms / ordering.
        ArraySum,
        ArrayAvg,
        ArrayMin,
        ArrayMax,
        ArrayMedian,
        ArrayRange,
        ArrayMode,
        ArrayVariance,
        ArrayStdev,
        ArrayAbs,
        ArrayStandardize,
        ArraySort,
        ArraySortIndices,
        // Boolean reductions: `array.every(id)`/`array.some(id)` take no
        // callback — they reduce the array's own elements to a bool.
        ArrayEvery,
        ArraySome,
        // Covariance, percentile / rank family, binary search.
        ArrayCovariance,
        ArrayPercentRank,
        ArrayPercentileNearestRank,
        ArrayPercentileLinearInterpolation,
        ArrayBinarySearch,
        ArrayBinarySearchLeftmost,
        ArrayBinarySearchRightmost,
    ]
    .into_iter()
    .map(collection_function)
    .collect()
}

/// The scalar value types a `map` key or value may take (keys must be a
/// value type). Used to enumerate the typed `map.new_*` constructor names. A
/// map of user-defined-type values has no registry name: `map.new<K, MyType>`
/// carries its type arguments on the call node.
const MAP_SCALAR_TYPES: [(&str, ScalarTy); 5] = [
    ("float", ScalarTy::Float),
    ("int", ScalarTy::Int),
    ("bool", ScalarTy::Bool),
    ("string", ScalarTy::String),
    ("color", ScalarTy::Color),
];

/// The `map.*` collection table. The receiver-taking operations share
/// the generic [`CollectionFunc`] machinery; the typed constructors are
/// distinct names (`map.new_<key>_<val>`), the targets of the parser's scalar
/// `map.new<K, V>` rewrite — each carries its static key/value types in its
/// kind.
fn map_functions() -> Vec<BuiltinFunction> {
    use CollectionFunc::{
        MapClear, MapContains, MapCopy, MapGet, MapKeys, MapPut, MapPutAll, MapRemove, MapSize,
        MapValues,
    };
    let mut out: Vec<BuiltinFunction> = [
        MapPut,
        MapGet,
        MapContains,
        MapRemove,
        MapSize,
        MapClear,
        MapKeys,
        MapValues,
        MapCopy,
        MapPutAll,
    ]
    .into_iter()
    .map(collection_function)
    .collect();
    // The typed constructors: one per (key, val) value-type pairing.
    for (kname, kty) in MAP_SCALAR_TYPES {
        for (vname, vty) in MAP_SCALAR_TYPES {
            let name: &'static str = Box::leak(format!("map.new_{kname}_{vname}").into_boxed_str());
            out.push(BuiltinFunction {
                name,
                params: Vec::new(),
                ret: Type::Void,
                kind: BuiltinFunctionKind::MapConstructor { key: kty, val: vty },
                pure_result: None,
                variadic_max: None,
                ta_engine: None,
            });
        }
    }
    out
}

/// The element value types a `matrix` may hold (the scalar forms of
/// `matrix.new<T>`: float/int/bool/string/color). Used to enumerate the typed `matrix.new_*`
/// constructor names.
const MATRIX_SCALAR_TYPES: [(&str, ScalarTy); 5] = [
    ("float", ScalarTy::Float),
    ("int", ScalarTy::Int),
    ("bool", ScalarTy::Bool),
    ("string", ScalarTy::String),
    ("color", ScalarTy::Color),
];

/// The `matrix.*` collection table. Receiver-taking operations share the
/// generic [`CollectionFunc`] machinery; the typed constructors are distinct
/// names (`matrix.new_<T>`), the targets of the parser's scalar
/// `matrix.new<T>` rewrite.
/// `matrix.eigenvalues` / `matrix.eigenvectors` are implemented for symmetric
/// matrices (cyclic Jacobi) and `matrix.pinv` for full-rank matrices
/// (normal-equations); non-symmetric eigen and rank-deficient pinv are
/// structured errors.
fn matrix_functions() -> Vec<BuiltinFunction> {
    use CollectionFunc::{
        MatrixAddCol, MatrixAddRow, MatrixAvg, MatrixCol, MatrixColumns, MatrixConcat, MatrixCopy,
        MatrixDet, MatrixDiff, MatrixEigenvalues, MatrixEigenvectors, MatrixElementsCount,
        MatrixFill, MatrixGet, MatrixInv, MatrixIsAntidiagonal, MatrixIsAntisymmetric,
        MatrixIsBinary, MatrixIsDiagonal, MatrixIsIdentity, MatrixIsSquare, MatrixIsStochastic,
        MatrixIsSymmetric, MatrixIsTriangular, MatrixIsZero, MatrixKron, MatrixMax, MatrixMedian,
        MatrixMin, MatrixMode, MatrixMult, MatrixPinv, MatrixPow, MatrixRank, MatrixRemoveCol,
        MatrixRemoveRow, MatrixReshape, MatrixReverse, MatrixRow, MatrixRows, MatrixSet,
        MatrixSort, MatrixSubmatrix, MatrixSum, MatrixSwapColumns, MatrixSwapRows, MatrixTrace,
        MatrixTranspose,
    };
    let mut out: Vec<BuiltinFunction> = [
        MatrixGet,
        MatrixSet,
        MatrixRows,
        MatrixColumns,
        MatrixElementsCount,
        MatrixFill,
        MatrixCopy,
        MatrixAddRow,
        MatrixAddCol,
        MatrixRemoveRow,
        MatrixRemoveCol,
        MatrixSwapRows,
        MatrixSwapColumns,
        MatrixReverse,
        MatrixTranspose,
        MatrixReshape,
        MatrixSubmatrix,
        MatrixConcat,
        MatrixSum,
        MatrixDiff,
        MatrixMult,
        MatrixDet,
        MatrixInv,
        MatrixTrace,
        MatrixPow,
        MatrixKron,
        MatrixAvg,
        MatrixMin,
        MatrixMax,
        MatrixMedian,
        MatrixMode,
        MatrixIsSquare,
        MatrixIsIdentity,
        MatrixIsDiagonal,
        MatrixIsSymmetric,
        MatrixIsAntisymmetric,
        MatrixIsTriangular,
        MatrixIsStochastic,
        MatrixIsBinary,
        MatrixIsZero,
        MatrixIsAntidiagonal,
        MatrixRank,
        MatrixRow,
        MatrixCol,
        MatrixEigenvalues,
        MatrixEigenvectors,
        MatrixPinv,
        MatrixSort,
    ]
    .into_iter()
    .map(collection_function)
    .collect();
    // The typed constructors: one per element value type. Their public
    // parameter names come from the collection surface table, the same list
    // `analyze_matrix_constructor` binds named arguments against.
    let (matrix_new_params, matrix_new_required) = CollectionFunc::MatrixNew.surface_params();
    for (tname, tty) in MATRIX_SCALAR_TYPES {
        let name: &'static str = Box::leak(format!("matrix.new_{tname}").into_boxed_str());
        out.push(BuiltinFunction {
            name,
            params: matrix_new_params
                .iter()
                .enumerate()
                .map(|(index, name)| ParamSpec {
                    name,
                    ty: Type::Unknown,
                    type_rule: ParamTypeRule::AnyValue,
                    max_qualifier: Qualifier::Series,
                    required: index < matrix_new_required,
                    int_literal_range: None,
                    default_int: None,
                    default_float: None,
                    default_string: None,
                    default_bool: None,
                    default_series: None,
                })
                .collect(),
            ret: Type::Void,
            kind: BuiltinFunctionKind::MatrixConstructor { elem: tty },
            pure_result: None,
            variadic_max: None,
            ta_engine: None,
        });
    }
    out
}

/// The closest candidate to `name` within edit distance 2, if any — used for
/// "did you mean" suggestions over an arbitrary set of names (e.g. a callee's
/// parameter names when binding named arguments).
///
/// Among candidates equally close the first in name order is chosen, so the
/// answer does not depend on the order the candidates arrive in. Several
/// candidate sets are the keys of a hash map, which have no order of their
/// own.
pub(crate) fn closest<'a>(
    name: &str,
    candidates: impl Iterator<Item = &'a str>,
) -> Option<&'a str> {
    let name = Leading::of(name);
    candidates
        .filter_map(|candidate| Some((suggestion_distance(&name, candidate)?, candidate)))
        .min()
        .map(|(_, candidate)| candidate)
}

/// How many leading characters of a name a suggestion compares.
const SUGGESTION_CHARS: usize = 32;

/// The farthest a suggestion may be from the name it is offered for.
const SUGGESTION_DISTANCE: usize = 2;

/// The leading characters of a name, held without allocating: a search
/// compares one name with every candidate there is.
struct Leading {
    chars: [char; SUGGESTION_CHARS],
    len: usize,
}

impl Leading {
    fn of(text: &str) -> Self {
        let mut chars = ['\0'; SUGGESTION_CHARS];
        let mut len = 0;
        for (slot, c) in chars.iter_mut().zip(text.chars()) {
            *slot = c;
            len += 1;
        }
        Leading { chars, len }
    }

    fn chars(&self) -> &[char] {
        &self.chars[..self.len]
    }
}

/// The edit distance from `name` to `candidate` when it is close enough to
/// suggest, over the leading characters of each.
fn suggestion_distance(name: &Leading, candidate: &str) -> Option<usize> {
    let candidate = Leading::of(candidate);
    // Two names are at least as far apart as their lengths differ, which
    // settles most candidates without comparing a character.
    if name.len.abs_diff(candidate.len) > SUGGESTION_DISTANCE {
        return None;
    }
    let distance = edit_distance(name.chars(), candidate.chars());
    (distance <= SUGGESTION_DISTANCE).then_some(distance)
}

/// Levenshtein distance, small-input only (identifier suggestions).
fn edit_distance(a: &[char], b: &[char]) -> usize {
    debug_assert!(b.len() <= SUGGESTION_CHARS);
    let mut prev = [0usize; SUGGESTION_CHARS + 1];
    let mut current = [0usize; SUGGESTION_CHARS + 1];
    for (j, cell) in prev.iter_mut().enumerate() {
        *cell = j;
    }
    for (i, ca) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitution = prev[j] + usize::from(ca != cb);
            current[j + 1] = substitution.min(prev[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut prev, &mut current);
    }
    prev[b.len()]
}

#[cfg(test)]
mod registry_cache_tests {
    use super::{
        generic_input_source_params, BuiltinFunctionKind, BuiltinRegistry, ParamTypeRule,
        GENERIC_INPUT_SOURCE_PARAM_NAMES, PIVOT_POINT_LEVELS_PARAM_NAMES,
        SECURITY_LOWER_TF_PARAM_NAMES, SECURITY_PARAM_NAMES, TIME_FUNCTION_PARAM_NAMES,
    };
    use pinewasm_hir::{Qualifier, Type};

    #[test]
    fn standard_ref_reuses_one_immutable_registry() {
        assert!(std::ptr::eq(
            BuiltinRegistry::standard_ref(),
            BuiltinRegistry::standard_ref(),
        ));
    }

    #[test]
    fn special_analyzers_publish_complete_typed_parameter_metadata() {
        let registry = BuiltinRegistry::standard();
        for function in registry.functions() {
            let expected = match function.kind {
                BuiltinFunctionKind::Display(kind) => Some(kind.param_names()),
                BuiltinFunctionKind::Order(kind) => Some(kind.param_names()),
                BuiltinFunctionKind::Security => Some(SECURITY_PARAM_NAMES.as_slice()),
                BuiltinFunctionKind::SecurityLowerTf => {
                    Some(SECURITY_LOWER_TF_PARAM_NAMES.as_slice())
                }
                BuiltinFunctionKind::PivotPointLevels => {
                    Some(PIVOT_POINT_LEVELS_PARAM_NAMES.as_slice())
                }
                BuiltinFunctionKind::TimeFunction { .. } => {
                    Some(TIME_FUNCTION_PARAM_NAMES.as_slice())
                }
                BuiltinFunctionKind::Request(func) => Some(func.param_names()),
                _ => None,
            };
            let Some(expected) = expected else { continue };
            assert_eq!(
                function
                    .params
                    .iter()
                    .map(|param| param.name)
                    .collect::<Vec<_>>(),
                expected,
                "{}",
                function.name
            );
        }

        let security = registry
            .lookup_function("request.security")
            .expect("request.security metadata");
        assert_eq!(security.params[0].ty, Type::String);
        assert_eq!(security.params[0].max_qualifier, Qualifier::Simple);
        assert_eq!(security.params[1].max_qualifier, Qualifier::Const);
        assert_eq!(security.params[2].type_rule, ParamTypeRule::NumericOrBool);

        let risk = registry
            .lookup_function("strategy.risk.max_drawdown")
            .expect("max_drawdown metadata");
        assert_eq!(risk.params[0].type_rule, ParamTypeRule::Numeric);
        assert_eq!(risk.params[0].max_qualifier, Qualifier::Simple);
        assert_eq!(risk.params[1].ty, Type::String);

        let display = registry
            .lookup_function("plotshape")
            .expect("plotshape metadata");
        assert_eq!(display.params[0].type_rule, ParamTypeRule::NumericOrBool);
        assert_eq!(
            display
                .params
                .iter()
                .find(|param| param.name == "offset")
                .expect("offset metadata")
                .type_rule,
            ParamTypeRule::Declared
        );

        let generic_input = registry
            .lookup_function("input")
            .expect("generic input metadata");
        let source = generic_input_source_params(generic_input);
        assert_eq!(
            source.iter().map(|param| param.name).collect::<Vec<_>>(),
            GENERIC_INPUT_SOURCE_PARAM_NAMES
        );
        assert_eq!(source[0].ty, Type::Float);
        assert_eq!(source[0].max_qualifier, Qualifier::Series);
    }
}

#[cfg(test)]
mod suggestion_tests {
    use super::{closest, edit_distance, BuiltinRegistry, Leading};

    /// The distance the suggestions are defined by: plain Levenshtein over
    /// the first 32 characters of each name, with the whole table kept.
    /// `rows[i][j]` is the distance between the first `i` characters of `a`
    /// and the first `j` of `b`.
    fn reference_distance(a: &str, b: &str) -> usize {
        let a: Vec<char> = a.chars().take(32).collect();
        let b: Vec<char> = b.chars().take(32).collect();
        let mut rows = vec![vec![0usize; b.len() + 1]; a.len() + 1];
        for (i, row) in rows.iter_mut().enumerate() {
            row[0] = i;
        }
        for (j, cell) in rows[0].iter_mut().enumerate() {
            *cell = j;
        }
        for (i, ca) in a.iter().enumerate() {
            for (j, cb) in b.iter().enumerate() {
                let substitution = rows[i][j] + usize::from(ca != cb);
                rows[i + 1][j + 1] = substitution.min(rows[i][j + 1] + 1).min(rows[i + 1][j] + 1);
            }
        }
        rows[a.len()][b.len()]
    }

    /// The closest candidate by definition: every candidate measured, the
    /// nearest within two edits kept, ties going to the first in name order.
    fn reference_closest<'a>(name: &str, candidates: &[&'a str]) -> Option<&'a str> {
        candidates
            .iter()
            .map(|candidate| (reference_distance(name, candidate), *candidate))
            .filter(|(distance, _)| *distance <= 2)
            .min()
            .map(|(_, candidate)| candidate)
    }

    fn distance(a: &str, b: &str) -> usize {
        edit_distance(Leading::of(a).chars(), Leading::of(b).chars())
    }

    #[test]
    fn edit_distance_is_levenshtein_over_the_leading_characters() {
        for (a, b, expected) in [
            ("", "", 0),
            ("", "abc", 3),
            ("abc", "", 3),
            ("close", "close", 0),
            ("clos", "close", 1),
            ("colse", "close", 2),
            ("kitten", "sitting", 3),
            ("ta.sma", "ta.ema", 1),
            ("é", "e", 1),
        ] {
            assert_eq!(distance(a, b), expected, "{a:?} to {b:?}");
            assert_eq!(distance(b, a), expected, "{b:?} to {a:?}");
            assert_eq!(reference_distance(a, b), expected, "{a:?} to {b:?}");
        }
        // Only the first 32 characters count: these differ at the 33rd.
        let long = "a".repeat(32);
        assert_eq!(distance(&format!("{long}x"), &format!("{long}y")), 0);
        assert_eq!(distance(&format!("{long}x"), &long), 0);
        assert_eq!(distance(&"b".repeat(40), &long), 32);
    }

    #[test]
    fn the_closest_name_does_not_depend_on_the_order_of_the_candidates() {
        // `ac` and `aa` are both one edit from `ab`.
        assert_eq!(closest("ab", ["ac", "aa"].into_iter()), Some("aa"));
        assert_eq!(closest("ab", ["aa", "ac"].into_iter()), Some("aa"));
        // A nearer name beats an earlier one.
        assert_eq!(
            closest("abc", ["aaa", "abd", "abc"].into_iter()),
            Some("abc")
        );
        assert_eq!(
            closest("lenght", ["offset", "length"].into_iter()),
            Some("length")
        );
        // Nothing within two edits is no suggestion.
        assert_eq!(closest("abc", ["xyz", "abcdef"].into_iter()), None);
        assert_eq!(closest("abc", std::iter::empty()), None);
    }

    /// How many candidates are exactly as near to `name` as the nearest one
    /// that could be suggested (zero when none can).
    fn nearest_count(name: &str, candidates: &[&str]) -> usize {
        let distances: Vec<usize> = candidates
            .iter()
            .map(|candidate| reference_distance(name, candidate))
            .filter(|distance| *distance <= 2)
            .collect();
        let nearest = distances.iter().min().copied();
        distances
            .iter()
            .filter(|distance| Some(**distance) == nearest)
            .count()
    }

    #[test]
    fn suggestions_for_built_in_names_match_the_definition() {
        let registry = BuiltinRegistry::standard_ref();
        let mut names: Vec<&'static str> = registry
            .series
            .keys()
            .chain(registry.functions.keys())
            .chain(registry.constants.keys())
            .copied()
            .collect();
        assert_eq!(names.len(), registry.suggestion_candidates());
        names.sort_unstable();
        // A spread of the built-in names, and the longest ones: those are
        // past the 32 characters a suggestion compares.
        let mut sample: Vec<&str> = names.iter().copied().step_by(37).collect();
        let mut by_length = names.clone();
        by_length.sort_by_key(|name| std::cmp::Reverse(name.chars().count()));
        sample.extend(&by_length[..8]);
        assert!(by_length[0].chars().count() > 32, "{}", by_length[0]);

        // Misspell each five ways and compare the answer with measuring
        // every candidate.
        let (mut compared, mut ties) = (0, 0);
        for name in sample {
            let chars: Vec<char> = name.chars().collect();
            let middle = chars.len() / 2;
            let mut dropped = chars.clone();
            dropped.remove(middle);
            let mut replaced = chars.clone();
            replaced[middle] = '_';
            let mut doubled = chars.clone();
            doubled.insert(middle, chars[middle]);
            let far: String = chars.iter().rev().collect::<String>() + "zzz";
            for misspelled in [
                dropped.iter().collect::<String>(),
                replaced.iter().collect::<String>(),
                doubled.iter().collect::<String>(),
                format!("{name}xy"),
                far,
            ] {
                assert_eq!(
                    registry.suggest(&misspelled),
                    reference_closest(&misspelled, &names),
                    "{misspelled:?} (from {name:?})"
                );
                compared += 1;
                if nearest_count(&misspelled, &names) > 1 {
                    ties += 1;
                }
            }
        }
        assert!(compared >= 100, "{compared}");
        // Some of those answers were a choice between equally near names.
        assert!(ties > 0);
    }
}
