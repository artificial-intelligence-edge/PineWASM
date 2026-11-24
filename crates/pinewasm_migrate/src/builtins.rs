//! What the migrator needs to know about Pine v5's built-ins.
//!
//! The compiler's own registry describes v6 and only the surface PineWASM
//! executes, so it does not model v5 built-ins: the result type of a v5
//! call, or the parameters a v5 call accepts. These tables are the v5
//! facts the rules depend on, kept small and grouped by namespace: value
//! types and qualifiers for type inference, which calls are stateful or
//! effectful (for hoisting), and the parameter layouts of the calls whose
//! arguments a rule rewrites.

use std::fmt;

/// A value type as the migrator models it. Only as precise as the rules
/// need: collections and objects carry just enough to answer element and
/// method questions.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Ty {
    Int,
    Float,
    Bool,
    Str,
    Color,
    /// The untyped `na` literal.
    Na,
    Void,
    Unknown,
    Array(Box<Ty>),
    Matrix(Box<Ty>),
    Map(Box<Ty>, Box<Ty>),
    /// A drawing or reference object (`line`, `label`, `box`, `table`,
    /// `linefill`, `polyline`, `chart.point`, `plot`, `hline`) or a
    /// user-defined type or enum, by name.
    Named(String),
    /// A value of one of Pine's special constant types.
    Unique(Unique),
    Tuple(Vec<Ty>),
}

impl Ty {
    pub(crate) fn is_numeric(&self) -> bool {
        matches!(self, Ty::Int | Ty::Float)
    }

    /// Element type of a collection receiver.
    pub(crate) fn element(&self) -> Ty {
        match self {
            Ty::Array(e) | Ty::Matrix(e) => (**e).clone(),
            Ty::Map(_, v) => (**v).clone(),
            _ => Ty::Unknown,
        }
    }

    /// The namespace whose functions act as this type's methods.
    pub(crate) fn method_namespace(&self) -> Option<&str> {
        Some(match self {
            Ty::Array(_) => "array",
            Ty::Matrix(_) => "matrix",
            Ty::Map(..) => "map",
            Ty::Str => "str",
            Ty::Color => "color",
            Ty::Named(name) => match name.as_str() {
                "line" | "label" | "box" | "table" | "linefill" | "polyline" | "chart.point" => {
                    name.as_str()
                }
                _ => return None,
            },
            _ => return None,
        })
    }

    /// Unify two branch types the way a conditional expression would.
    pub(crate) fn join(&self, other: &Ty) -> Ty {
        match (self, other) {
            (a, b) if a == b => a.clone(),
            (Ty::Na, t) | (t, Ty::Na) => t.clone(),
            (Ty::Unknown, t) | (t, Ty::Unknown) => t.clone(),
            (Ty::Int, Ty::Float) | (Ty::Float, Ty::Int) => Ty::Float,
            // A boolean branch joined with a numeric one yields `bool`,
            // casting the number.
            (Ty::Bool, Ty::Int | Ty::Float) | (Ty::Int | Ty::Float, Ty::Bool) => Ty::Bool,
            _ => Ty::Unknown,
        }
    }
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ty::Int => write!(f, "int"),
            Ty::Float => write!(f, "float"),
            Ty::Bool => write!(f, "bool"),
            Ty::Str => write!(f, "string"),
            Ty::Color => write!(f, "color"),
            Ty::Na => write!(f, "na"),
            Ty::Void => write!(f, "void"),
            Ty::Unknown => write!(f, "unknown"),
            Ty::Array(e) => write!(f, "array<{e}>"),
            Ty::Matrix(e) => write!(f, "matrix<{e}>"),
            Ty::Map(k, v) => write!(f, "map<{k}, {v}>"),
            Ty::Named(n) => write!(f, "{n}"),
            Ty::Unique(u) => write!(f, "{}", u.type_name()),
            Ty::Tuple(items) => {
                write!(f, "[")?;
                for (i, t) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{t}")?;
                }
                write!(f, "]")
            }
        }
    }
}

/// Pine's special constant types: each is its own type, so `na` is not
/// accepted wherever one is required.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Unique {
    PlotStyle,
    Display,
    Order,
    Gaps,
    Lookahead,
    BackAdjustment,
    SettlementAsClose,
    Scale,
    Direction,
    HlineStyle,
}

impl Unique {
    fn type_name(self) -> &'static str {
        match self {
            Unique::PlotStyle => "plot_style",
            Unique::Display => "plot_display",
            Unique::Order => "sort_order",
            Unique::Gaps => "barmerge_gaps",
            Unique::Lookahead => "barmerge_lookahead",
            Unique::BackAdjustment => "backadjustment",
            Unique::SettlementAsClose => "settlement",
            Unique::Scale => "scale_type",
            Unique::Direction => "strategy_direction",
            Unique::HlineStyle => "hline_style",
        }
    }

    /// The constant a parameter of this type falls back to when omitted, if
    /// the language documents one. `None` means no constant expresses the
    /// default (or none is documented), so an `na` cannot be replaced.
    pub(crate) fn default_constant(self) -> Option<&'static str> {
        Some(match self {
            Unique::PlotStyle => "plot.style_line",
            Unique::Display => "display.all",
            Unique::Order => "order.ascending",
            Unique::Gaps => "barmerge.gaps_off",
            Unique::Lookahead => "barmerge.lookahead_off",
            Unique::BackAdjustment => "backadjustment.inherit",
            Unique::SettlementAsClose => "settlement_as_close.inherit",
            Unique::Scale | Unique::Direction | Unique::HlineStyle => return None,
        })
    }

    /// For a type whose default is undocumented, the constant a fall-through
    /// branch most plausibly stood for. Using it is approximate: it only
    /// matters on bars where no branch matched.
    pub(crate) fn assumed_default(self) -> Option<&'static str> {
        Some(match self {
            Unique::HlineStyle => "hline.style_solid",
            _ => return None,
        })
    }
}

/// Type qualifiers, weakest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Qual {
    Const,
    Input,
    Simple,
    Series,
}

/// How evaluating a call interacts with bar-by-bar execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Effect {
    /// No history and no side effects: skipping it changes nothing.
    Pure,
    /// Keeps per-call-site history, so it must run on the same bars it ran
    /// on in v5.
    Stateful,
    /// Mutates state or produces output.
    Mutates,
}

/// The v5 type and qualifier of a built-in variable.
pub(crate) fn variable(name: &str) -> Option<(Ty, Qual)> {
    use Qual::{Input, Series, Simple};
    let series_float = Some((Ty::Float, Series));
    let series_int = Some((Ty::Int, Series));
    match name {
        "open" | "high" | "low" | "close" | "volume" | "hl2" | "hlc3" | "hlcc4" | "ohlc4" => {
            series_float
        }
        "bar_index" | "last_bar_index" | "last_bar_time" | "time" | "time_close"
        | "time_tradingday" | "timenow" | "year" | "month" | "weekofyear" | "dayofmonth"
        | "dayofweek" | "hour" | "minute" | "second" => series_int,
        "na" => Some((Ty::Na, Qual::Const)),
        "timeframe.period"
        | "syminfo.ticker"
        | "syminfo.tickerid"
        | "syminfo.root"
        | "syminfo.prefix"
        | "syminfo.session"
        | "syminfo.timezone"
        | "syminfo.description"
        | "syminfo.currency"
        | "syminfo.basecurrency"
        | "syminfo.type"
        | "syminfo.sector"
        | "syminfo.industry"
        | "syminfo.country"
        | "syminfo.volumetype"
        | "strategy.account_currency" => Some((Ty::Str, Simple)),
        "timeframe.multiplier"
        | "syminfo.minmove"
        | "syminfo.pricescale"
        | "syminfo.expiration_date"
        | "syminfo.employees"
        | "syminfo.shareholders"
        | "syminfo.shares_outstanding_total" => Some((Ty::Int, Simple)),
        "syminfo.mintick" | "syminfo.pointvalue" | "syminfo.shares_outstanding_float" => {
            Some((Ty::Float, Simple))
        }
        "timeframe.isdwm"
        | "timeframe.isintraday"
        | "timeframe.isdaily"
        | "timeframe.isweekly"
        | "timeframe.ismonthly"
        | "timeframe.isminutes"
        | "timeframe.isseconds"
        | "timeframe.isticks"
        | "chart.is_heikinashi"
        | "chart.is_renko"
        | "chart.is_linebreak"
        | "chart.is_kagi"
        | "chart.is_pnf"
        | "chart.is_range"
        | "chart.is_standard" => Some((Ty::Bool, Simple)),
        "chart.bg_color" | "chart.fg_color" => Some((Ty::Color, Input)),
        "chart.left_visible_bar_time" | "chart.right_visible_bar_time" => Some((Ty::Int, Input)),
        "strategy.position_entry_name" => Some((Ty::Str, Series)),
        "strategy.opentrades"
        | "strategy.closedtrades"
        | "strategy.wintrades"
        | "strategy.losstrades"
        | "strategy.eventrades"
        | "syminfo.target_price_date"
        | "syminfo.recommendations_buy"
        | "syminfo.recommendations_buy_strong"
        | "syminfo.recommendations_date"
        | "syminfo.recommendations_hold"
        | "syminfo.recommendations_total"
        | "syminfo.recommendations_sell"
        | "syminfo.recommendations_sell_strong"
        | "earnings.future_time"
        | "earnings.future_period_end_time"
        | "dividends.future_ex_date"
        | "dividends.future_pay_date" => series_int,
        "label.all" => Some((Ty::Array(Box::new(Ty::Named("label".into()))), Series)),
        "line.all" => Some((Ty::Array(Box::new(Ty::Named("line".into()))), Series)),
        "box.all" => Some((Ty::Array(Box::new(Ty::Named("box".into()))), Series)),
        "table.all" => Some((Ty::Array(Box::new(Ty::Named("table".into()))), Series)),
        "linefill.all" => Some((Ty::Array(Box::new(Ty::Named("linefill".into()))), Series)),
        "polyline.all" => Some((Ty::Array(Box::new(Ty::Named("polyline".into()))), Series)),
        _ if name.starts_with("barstate.") || name.starts_with("session.is") => {
            Some((Ty::Bool, Series))
        }
        _ if name.starts_with("ta.")
            || name.starts_with("strategy.")
            || name.starts_with("syminfo.target_price")
            || name.starts_with("earnings.future")
            || name.starts_with("dividends.future") =>
        {
            // The remaining `ta.*` variables (`ta.tr`, `ta.obv`, ...) and the
            // `strategy.*` performance variables are all series floats.
            if is_known_float_variable(name) {
                series_float
            } else {
                None
            }
        }
        _ => None,
    }
}

fn is_known_float_variable(name: &str) -> bool {
    matches!(
        name,
        "ta.vwap"
            | "ta.accdist"
            | "ta.tr"
            | "ta.iii"
            | "ta.wvad"
            | "ta.wad"
            | "ta.obv"
            | "ta.pvt"
            | "ta.nvi"
            | "ta.pvi"
            | "strategy.position_size"
            | "strategy.position_avg_price"
            | "strategy.openprofit"
            | "strategy.netprofit"
            | "strategy.equity"
            | "strategy.grossprofit"
            | "strategy.grossloss"
            | "strategy.max_drawdown"
            | "strategy.max_runup"
            | "strategy.grossloss_percent"
            | "strategy.max_runup_percent"
            | "strategy.netprofit_percent"
            | "strategy.openprofit_percent"
            | "strategy.grossprofit_percent"
            | "strategy.max_drawdown_percent"
            | "strategy.avg_trade"
            | "strategy.avg_trade_percent"
            | "strategy.avg_winning_trade"
            | "strategy.avg_winning_trade_percent"
            | "strategy.avg_losing_trade"
            | "strategy.avg_losing_trade_percent"
            | "strategy.max_contracts_held_all"
            | "strategy.max_contracts_held_long"
            | "strategy.max_contracts_held_short"
            | "strategy.opentrades.capital_held"
            | "strategy.initial_capital"
            | "strategy.margin_liquidation_price"
            | "syminfo.target_price_average"
            | "syminfo.target_price_estimates"
            | "syminfo.target_price_high"
            | "syminfo.target_price_low"
            | "syminfo.target_price_median"
            | "earnings.future_eps"
            | "earnings.future_revenue"
            | "dividends.future_amount"
    )
}

/// The type of a built-in constant (always `const`).
pub(crate) fn constant(name: &str) -> Option<Ty> {
    if matches!(name, "true" | "false") {
        return Some(Ty::Bool);
    }
    if let Some(color) = name.strip_prefix("color.") {
        return color_value(color).map(|_| Ty::Color);
    }
    let unique = if name.starts_with("plot.style_") {
        Some(Unique::PlotStyle)
    } else if name.starts_with("display.") {
        Some(Unique::Display)
    } else if name.starts_with("order.") {
        Some(Unique::Order)
    } else if name.starts_with("barmerge.gaps_") {
        Some(Unique::Gaps)
    } else if name.starts_with("barmerge.lookahead_") {
        Some(Unique::Lookahead)
    } else if name.starts_with("backadjustment.") {
        Some(Unique::BackAdjustment)
    } else if name.starts_with("settlement_as_close.") {
        Some(Unique::SettlementAsClose)
    } else if name.starts_with("scale.") {
        Some(Unique::Scale)
    } else if matches!(name, "strategy.long" | "strategy.short") {
        Some(Unique::Direction)
    } else if name.starts_with("hline.style_") {
        Some(Unique::HlineStyle)
    } else {
        None
    };
    if let Some(unique) = unique {
        return Some(Ty::Unique(unique));
    }
    if matches!(name, "math.pi" | "math.e" | "math.phi" | "math.rphi") {
        return Some(Ty::Float);
    }
    if name.starts_with("dayofweek.") {
        return Some(Ty::Int);
    }
    const STRING_NAMESPACES: &[&str] = &[
        "shape.",
        "location.",
        "size.",
        "label.style_",
        "line.style_",
        "extend.",
        "xloc.",
        "yloc.",
        "text.",
        "position.",
        "font.",
        "format.",
        "currency.",
        "session.regular",
        "session.extended",
        "adjustment.",
        "alert.freq_",
        "strategy.direction.",
        "strategy.commission.",
        "strategy.oca.",
        "earnings.actual",
        "earnings.estimate",
        "earnings.standardized",
        "dividends.gross",
        "dividends.net",
        "splits.denominator",
        "splits.numerator",
    ];
    if STRING_NAMESPACES.iter().any(|p| name.starts_with(p))
        || matches!(
            name,
            "strategy.percent_of_equity" | "strategy.cash" | "strategy.fixed"
        )
    {
        return Some(Ty::Str);
    }
    None
}

/// Hex value of each built-in color constant, as the migrator emits it.
pub(crate) fn color_value(name: &str) -> Option<&'static str> {
    Some(match name {
        "aqua" => "#00BCD4",
        "black" => "#363A45",
        "blue" => "#2962FF",
        "fuchsia" => "#E040FB",
        "gray" => "#787B86",
        "green" => "#4CAF50",
        "lime" => "#00E676",
        "maroon" => "#880E4F",
        "navy" => "#311B92",
        "olive" => "#808000",
        "orange" => "#FF9800",
        "purple" => "#9C27B0",
        "red" => "#FF5252",
        "silver" => "#B2B5BE",
        "teal" => "#00897B",
        "white" => "#FFFFFF",
        "yellow" => "#FFEB3B",
        _ => return None,
    })
}

/// Color constants whose value changed in v6.
pub(crate) fn color_changed_in_v6(name: &str) -> bool {
    matches!(name, "color.red" | "color.teal" | "color.yellow")
}

/// How evaluating a built-in function interacts with execution order.
pub(crate) fn effect(name: &str) -> Effect {
    if name.starts_with("ta.")
        || name.starts_with("request.")
        || matches!(name, "math.sum" | "fixnan" | "timeframe.change")
    {
        return Effect::Stateful;
    }
    if let Some((ns, func)) = name.rsplit_once('.') {
        let mutating = match ns {
            "array" => matches!(
                func,
                "push"
                    | "pop"
                    | "shift"
                    | "unshift"
                    | "insert"
                    | "remove"
                    | "set"
                    | "clear"
                    | "fill"
                    | "sort"
                    | "reverse"
                    | "concat"
            ),
            "map" => matches!(func, "put" | "remove" | "clear" | "put_all"),
            "matrix" => {
                func == "set"
                    || func.starts_with("add_")
                    || func.starts_with("remove_")
                    || func.starts_with("swap_")
                    || matches!(func, "fill" | "sort" | "reverse" | "reshape")
            }
            "label" | "line" | "box" | "table" | "polyline" | "linefill" => {
                !func.starts_with("get_") && func != "all"
            }
            "log" => true,
            "strategy" => matches!(
                func,
                "entry" | "order" | "exit" | "close" | "close_all" | "cancel" | "cancel_all"
            ),
            "strategy.risk" => true,
            "runtime" => func == "error",
            _ => false,
        };
        if mutating {
            return Effect::Mutates;
        }
    }
    if matches!(
        name,
        "alert"
            | "alertcondition"
            | "max_bars_back"
            | "plot"
            | "plotshape"
            | "plotchar"
            | "plotarrow"
            | "plotbar"
            | "plotcandle"
            | "hline"
            | "fill"
            | "bgcolor"
            | "barcolor"
    ) {
        return Effect::Mutates;
    }
    Effect::Pure
}

/// Whether `name` is a built-in function the migrator recognizes at all.
pub(crate) fn is_known_function(name: &str) -> bool {
    const NAMESPACES: &[&str] = &[
        "ta.",
        "math.",
        "str.",
        "color.",
        "array.",
        "matrix.",
        "map.",
        "input.",
        "request.",
        "strategy.",
        "label.",
        "line.",
        "box.",
        "table.",
        "linefill.",
        "polyline.",
        "chart.point.",
        "ticker.",
        "timeframe.",
        "log.",
        "runtime.",
        "syminfo.",
    ];
    NAMESPACES.iter().any(|ns| name.starts_with(ns))
        || matches!(
            name,
            "na" | "nz"
                | "fixnan"
                | "int"
                | "float"
                | "bool"
                | "string"
                | "color"
                | "input"
                | "plot"
                | "plotshape"
                | "plotchar"
                | "plotarrow"
                | "plotbar"
                | "plotcandle"
                | "hline"
                | "fill"
                | "bgcolor"
                | "barcolor"
                | "alert"
                | "alertcondition"
                | "indicator"
                | "strategy"
                | "library"
                | "time"
                | "time_close"
                | "timestamp"
                | "year"
                | "month"
                | "weekofyear"
                | "dayofmonth"
                | "dayofweek"
                | "hour"
                | "minute"
                | "second"
                | "max_bars_back"
                | "label"
                | "line"
                | "box"
                | "table"
                | "linefill"
        )
}

/// The v5 result type of a built-in call, given its argument types in
/// source order (named arguments included, in order). `Unknown` when the
/// call is not modeled.
#[allow(clippy::too_many_lines)]
pub(crate) fn call_type(name: &str, args: &[Ty]) -> Ty {
    let arg = |i: usize| args.get(i).cloned().unwrap_or(Ty::Unknown);
    let numeric_of = |tys: &[Ty]| {
        if !tys.is_empty() && tys.iter().all(|t| *t == Ty::Int) {
            Ty::Int
        } else {
            Ty::Float
        }
    };
    let arr = |t: Ty| Ty::Array(Box::new(t));
    match name {
        "na" | "ta.crossover" | "ta.crossunder" | "ta.cross" | "ta.rising" | "ta.falling"
        | "timeframe.change" | "str.contains" | "str.startswith" | "str.endswith"
        | "array.includes" | "array.every" | "array.some" | "map.contains" | "input.bool"
        | "bool" => Ty::Bool,
        "nz" | "fixnan" => match arg(0) {
            Ty::Na => arg(1),
            t => t,
        },
        "ta.valuewhen" => arg(1),
        "ta.change" => match arg(0) {
            Ty::Bool => Ty::Bool,
            Ty::Int => Ty::Int,
            _ => Ty::Float,
        },
        "int" | "input.int" | "input.time" | "math.ceil" | "math.floor" | "math.round"
            if name != "math.round" || args.len() < 2 =>
        {
            Ty::Int
        }
        "math.round" => Ty::Float,
        "float" | "input.float" | "input.source" | "input.price" | "str.tonumber" => Ty::Float,
        "string"
        | "str.tostring"
        | "str.format"
        | "str.format_time"
        | "str.trim"
        | "str.repeat"
        | "str.replace_all"
        | "str.replace"
        | "str.substring"
        | "str.lower"
        | "str.upper"
        | "str.match"
        | "array.join"
        | "timeframe.from_seconds"
        | "label.get_text" => Ty::Str,
        "color" | "color.new" | "color.rgb" | "color.from_gradient" | "input.color" => Ty::Color,
        "color.r" | "color.g" | "color.b" | "color.t" => Ty::Float,
        "str.pos"
        | "str.length"
        | "timeframe.in_seconds"
        | "time"
        | "time_close"
        | "timestamp"
        | "year"
        | "month"
        | "weekofyear"
        | "dayofmonth"
        | "dayofweek"
        | "hour"
        | "minute"
        | "second"
        | "ta.barssince"
        | "ta.highestbars"
        | "ta.lowestbars"
        | "array.size"
        | "array.indexof"
        | "array.lastindexof"
        | "array.binary_search"
        | "array.binary_search_leftmost"
        | "array.binary_search_rightmost"
        | "matrix.rows"
        | "matrix.columns"
        | "matrix.elements_count"
        | "matrix.rank"
        | "map.size"
        | "box.get_left"
        | "box.get_right"
        | "line.get_x1"
        | "line.get_x2"
        | "label.get_x" => Ty::Int,
        "math.abs" | "math.max" | "math.min" | "ta.max" | "ta.min" => numeric_of(args),
        "input" => match arg(0) {
            Ty::Unknown => Ty::Unknown,
            t => t,
        },
        "input.string" | "input.text_area" | "input.symbol" | "input.timeframe"
        | "input.session" | "ticker.new" | "ticker.standard" | "ticker.modify"
        | "ticker.inherit" | "ticker.heikinashi" | "ticker.renko" | "ticker.linebreak"
        | "ticker.kagi" | "ticker.pointfigure" => Ty::Str,
        "ta.macd" | "ta.bb" | "ta.kc" | "ta.dmi" => Ty::Tuple(vec![Ty::Float; 3]),
        "ta.supertrend" => Ty::Tuple(vec![Ty::Float; 2]),
        "request.security" => arg(2),
        "request.security_lower_tf" => match arg(2) {
            Ty::Tuple(items) => Ty::Tuple(items.into_iter().map(arr).collect()),
            t => arr(t),
        },
        "str.split" => arr(Ty::Str),
        "array.new_float" => arr(Ty::Float),
        "array.new_int" | "array.sort_indices" => arr(Ty::Int),
        "array.new_bool" => arr(Ty::Bool),
        "array.new_string" => arr(Ty::Str),
        "array.new_color" => arr(Ty::Color),
        "array.new_line" => arr(Ty::Named("line".into())),
        "array.new_label" => arr(Ty::Named("label".into())),
        "array.new_box" => arr(Ty::Named("box".into())),
        "array.new_table" => arr(Ty::Named("table".into())),
        "array.new_linefill" => arr(Ty::Named("linefill".into())),
        "array.from" => arr(args.iter().fold(Ty::Na, |acc, t| acc.join(t))),
        "array.copy" | "array.slice" | "array.concat" | "array.abs" | "array.standardize" => arg(0),
        "array.get" | "array.first" | "array.last" | "array.pop" | "array.shift"
        | "array.remove" | "array.min" | "array.max" | "array.median" | "array.mode"
        | "array.range" | "array.sum" | "matrix.get" | "map.get" | "map.put" | "map.remove"
        | "matrix.min" | "matrix.max" | "matrix.median" | "matrix.mode" => arg(0).element(),
        "array.avg"
        | "array.stdev"
        | "array.variance"
        | "array.covariance"
        | "array.percentrank"
        | "array.percentile_nearest_rank"
        | "array.percentile_linear_interpolation"
        | "matrix.avg"
        | "matrix.det"
        | "matrix.trace" => Ty::Float,
        "matrix.row" | "matrix.col" | "matrix.remove_row" | "matrix.remove_col" => {
            arr(arg(0).element())
        }
        "matrix.copy" | "matrix.submatrix" | "matrix.transpose" | "matrix.concat"
        | "matrix.sum" | "matrix.diff" | "matrix.pow" | "matrix.inv" | "matrix.pinv"
        | "matrix.kron" | "map.copy" => arg(0),
        "map.keys" => match arg(0) {
            Ty::Map(k, _) => arr(*k),
            _ => Ty::Unknown,
        },
        "map.values" => match arg(0) {
            Ty::Map(_, v) => arr(*v),
            _ => Ty::Unknown,
        },
        "label.new" | "label.copy" => Ty::Named("label".into()),
        "line.new" | "line.copy" | "linefill.get_line1" | "linefill.get_line2" => {
            Ty::Named("line".into())
        }
        "box.new" | "box.copy" => Ty::Named("box".into()),
        "table.new" => Ty::Named("table".into()),
        "linefill.new" => Ty::Named("linefill".into()),
        "polyline.new" => Ty::Named("polyline".into()),
        "chart.point.new"
        | "chart.point.from_index"
        | "chart.point.from_time"
        | "chart.point.now"
        | "chart.point.copy" => Ty::Named("chart.point".into()),
        "plot" => Ty::Named("plot".into()),
        "hline" => Ty::Named("hline".into()),
        "box.get_top"
        | "box.get_bottom"
        | "line.get_y1"
        | "line.get_y2"
        | "line.get_price"
        | "label.get_y"
        | "math.pow"
        | "math.log"
        | "math.log10"
        | "math.sqrt"
        | "math.sign"
        | "math.exp"
        | "math.sin"
        | "math.cos"
        | "math.tan"
        | "math.asin"
        | "math.acos"
        | "math.atan"
        | "math.round_to_mintick"
        | "math.todegrees"
        | "math.toradians"
        | "math.random"
        | "math.sum"
        | "math.avg"
        | "ta.median"
        | "ta.mode"
        | "ta.range" => Ty::Float,
        _ if name.starts_with("strategy.closedtrades.")
            || name.starts_with("strategy.opentrades.") =>
        {
            if name.ends_with("_id") || name.ends_with("_comment") {
                Ty::Str
            } else if name.ends_with("_bar_index") || name.ends_with("_time") {
                Ty::Int
            } else {
                Ty::Float
            }
        }
        _ if name.starts_with("ta.") || name.starts_with("request.") => Ty::Float,
        _ if name.starts_with("matrix.is_") => Ty::Bool,
        _ if name.starts_with("matrix.new_") => match &name["matrix.new_".len()..] {
            "float" => Ty::Matrix(Box::new(Ty::Float)),
            "int" => Ty::Matrix(Box::new(Ty::Int)),
            "bool" => Ty::Matrix(Box::new(Ty::Bool)),
            "string" => Ty::Matrix(Box::new(Ty::Str)),
            "color" => Ty::Matrix(Box::new(Ty::Color)),
            _ => Ty::Unknown,
        },
        _ if effect(name) == Effect::Mutates => Ty::Void,
        _ => Ty::Unknown,
    }
}

/// The v5 qualifier of a built-in call's result, given its arguments'
/// qualifiers.
pub(crate) fn call_qual(name: &str, args: &[Qual]) -> Qual {
    let widest = args.iter().copied().max().unwrap_or(Qual::Const);
    if name == "input.source" {
        return Qual::Series;
    }
    if name.starts_with("input") {
        return Qual::Input;
    }
    if name.starts_with("math.") && name != "math.random" && name != "math.sum"
        || name.starts_with("str.")
        || matches!(
            name,
            "int"
                | "float"
                | "bool"
                | "string"
                | "color"
                | "nz"
                | "na"
                | "color.new"
                | "color.rgb"
                | "color.r"
                | "color.g"
                | "color.b"
                | "color.t"
                | "timeframe.in_seconds"
                | "timeframe.from_seconds"
                | "timestamp"
                | "ticker.new"
                | "ticker.modify"
                | "ticker.standard"
                | "ticker.inherit"
        )
    {
        return widest;
    }
    Qual::Series
}

/// A parameter layout used when binding arguments to names. `params` lists
/// the names in parameter order, including v5-only names that v6 removed (such as `when`).
pub(crate) struct Signature {
    pub(crate) params: &'static [&'static str],
}

/// Positional parameter layouts (v5) for the calls whose arguments the
/// migration rewrites. Named-only v5 parameters (`transp`) are not listed:
/// they never bind by position.
pub(crate) fn v5_signature(name: &str, positional: &[Ty]) -> Option<Signature> {
    let params: &'static [&'static str] = match name {
        "strategy.entry" => &[
            "id",
            "direction",
            "qty",
            "limit",
            "stop",
            "oca_name",
            "oca_type",
            "comment",
            "when",
            "alert_message",
            "disable_alert",
        ],
        "strategy.order" => &[
            "id",
            "direction",
            "qty",
            "limit",
            "stop",
            "oca_name",
            "oca_type",
            "comment",
            "when",
            "alert_message",
            "disable_alert",
        ],
        "strategy.exit" => &[
            "id",
            "from_entry",
            "qty",
            "qty_percent",
            "profit",
            "limit",
            "loss",
            "stop",
            "trail_price",
            "trail_points",
            "trail_offset",
            "oca_name",
            "comment",
            "comment_profit",
            "comment_loss",
            "comment_trailing",
            "when",
            "alert_message",
            "alert_profit",
            "alert_loss",
            "alert_trailing",
            "disable_alert",
        ],
        "strategy.close" => {
            // This overload takes a bool second positional argument as its
            // `when` condition, so it is selected only by a bool there.
            if positional.get(1) == Some(&Ty::Bool) {
                &[
                    "id",
                    "when",
                    "comment",
                    "qty",
                    "qty_percent",
                    "alert_message",
                    "immediately",
                    "disable_alert",
                ]
            } else {
                &[
                    "id",
                    "comment",
                    "qty",
                    "qty_percent",
                    "alert_message",
                    "immediately",
                    "disable_alert",
                ]
            }
        }
        "strategy.close_all" => &[
            "when",
            "comment",
            "alert_message",
            "immediately",
            "disable_alert",
        ],
        "strategy.cancel" => &["id", "when"],
        "strategy.cancel_all" => &["when"],
        "strategy" => &[
            "title",
            "shorttitle",
            "overlay",
            "format",
            "precision",
            "scale",
            "pyramiding",
            "calc_on_order_fills",
            "calc_on_every_tick",
            "max_bars_back",
            "backtest_fill_limits_assumption",
            "default_qty_type",
            "default_qty_value",
            "initial_capital",
            "currency",
            "slippage",
            "commission_type",
            "commission_value",
            "process_orders_on_close",
            "close_entries_rule",
            "margin_long",
            "margin_short",
            "explicit_plot_zorder",
            "max_lines_count",
            "max_labels_count",
            "max_boxes_count",
            "calc_bars_count",
            "risk_free_rate",
            "use_bar_magnifier",
            "fill_orders_on_standard_ohlc",
            "max_polylines_count",
            "dynamic_requests",
            "behind_chart",
        ],
        "indicator" => &[
            "title",
            "shorttitle",
            "overlay",
            "format",
            "precision",
            "scale",
            "max_bars_back",
            "timeframe",
            "timeframe_gaps",
            "explicit_plot_zorder",
            "max_lines_count",
            "max_labels_count",
            "max_boxes_count",
            "calc_bars_count",
            "max_polylines_count",
            "dynamic_requests",
            "behind_chart",
        ],
        "library" => &["title", "overlay", "dynamic_requests"],
        "input.source" => &[
            "defval", "title", "tooltip", "inline", "group", "display", "confirm",
        ],
        "time" | "time_close" => {
            // Overloads: (timeframe, bars_back), (timeframe, session,
            // bars_back) and (timeframe, session, timezone, bars_back),
            // chosen by where the first `int` argument sits.
            if positional.get(1) == Some(&Ty::Int) {
                &["timeframe", "bars_back"]
            } else if positional.get(2) == Some(&Ty::Int) {
                &["timeframe", "session", "bars_back"]
            } else {
                &["timeframe", "session", "timezone", "bars_back"]
            }
        }
        "plot" => &[
            "series",
            "title",
            "color",
            "linewidth",
            "style",
            "trackprice",
            "histbase",
            "offset",
            "join",
            "editable",
            "show_last",
            "display",
            "format",
            "precision",
            "force_overlay",
        ],
        "plotshape" => &[
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
        "plotchar" => &[
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
        "plotarrow" => &[
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
        "plotbar" => &[
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
        "plotcandle" => &[
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
        "bgcolor" => &[
            "color",
            "offset",
            "editable",
            "show_last",
            "title",
            "display",
            "force_overlay",
        ],
        "barcolor" => &[
            "color",
            "offset",
            "editable",
            "show_last",
            "title",
            "display",
        ],
        "hline" => &[
            "price",
            "title",
            "color",
            "linestyle",
            "linewidth",
            "editable",
            "display",
        ],
        "fill" => {
            // `fill(plot1, plot2, color, ...)` and the gradient overload
            // `fill(plot1, plot2, top_value, bottom_value, top_color,
            // bottom_color, ...)` share the leading pair.
            if positional.get(2).is_some_and(Ty::is_numeric) {
                &[
                    "plot1",
                    "plot2",
                    "top_value",
                    "bottom_value",
                    "top_color",
                    "bottom_color",
                    "title",
                    "display",
                    "fillgaps",
                    "editable",
                ]
            } else if positional.first() == Some(&Ty::Named("hline".into())) {
                &[
                    "hline1", "hline2", "color", "title", "editable", "fillgaps", "display",
                ]
            } else {
                &[
                    "plot1",
                    "plot2",
                    "color",
                    "title",
                    "editable",
                    "show_last",
                    "fillgaps",
                    "display",
                ]
            }
        }
        "label.new" => {
            if positional.first() == Some(&Ty::Named("chart.point".into())) {
                &[
                    "point",
                    "text",
                    "xloc",
                    "yloc",
                    "color",
                    "style",
                    "textcolor",
                    "size",
                    "textalign",
                    "tooltip",
                    "text_font_family",
                    "force_overlay",
                ]
            } else {
                &[
                    "x",
                    "y",
                    "text",
                    "xloc",
                    "yloc",
                    "color",
                    "style",
                    "textcolor",
                    "size",
                    "textalign",
                    "tooltip",
                    "text_font_family",
                    "force_overlay",
                ]
            }
        }
        "request.security" => &[
            "symbol",
            "timeframe",
            "expression",
            "gaps",
            "lookahead",
            "ignore_invalid_symbol",
            "currency",
            "calc_bars_count",
        ],
        "array.sort" => &["id", "order"],
        "array.sort_indices" => &["id", "order"],
        "matrix.sort" => &["id", "column", "order"],
        "ticker.new" => &[
            "prefix",
            "ticker",
            "session",
            "adjustment",
            "backadjustment",
            "settlement_as_close",
        ],
        "ticker.modify" => &[
            "tickerid",
            "session",
            "adjustment",
            "backadjustment",
            "settlement_as_close",
        ],
        _ => return None,
    };
    Some(Signature { params })
}

/// Parameters whose type is a special constant type, so `na` is not accepted.
pub(crate) fn unique_param(func: &str, param: &str) -> Option<Unique> {
    Some(match (func, param) {
        ("plot", "style") => Unique::PlotStyle,
        (_, "display") if is_display_family(func) => Unique::Display,
        ("hline", "linestyle") => Unique::HlineStyle,
        ("array.sort" | "array.sort_indices" | "matrix.sort", "order") => Unique::Order,
        (f, "gaps") if f.starts_with("request.") => Unique::Gaps,
        (f, "lookahead") if f.starts_with("request.") => Unique::Lookahead,
        ("indicator" | "strategy", "scale") => Unique::Scale,
        ("ticker.new" | "ticker.modify", "backadjustment") => Unique::BackAdjustment,
        ("ticker.new" | "ticker.modify", "settlement_as_close") => Unique::SettlementAsClose,
        ("strategy.entry" | "strategy.order", "direction") => Unique::Direction,
        _ => return None,
    })
}

fn is_display_family(func: &str) -> bool {
    matches!(
        func,
        "plot"
            | "plotshape"
            | "plotchar"
            | "plotarrow"
            | "plotbar"
            | "plotcandle"
            | "bgcolor"
            | "barcolor"
            | "fill"
            | "hline"
    ) || func.starts_with("input")
}

/// Built-in parameters whose argument must be a `bool`.
pub(crate) fn bool_param(func: &str, param: &str) -> bool {
    matches!(
        (func, param),
        (
            "alertcondition" | "ta.barssince" | "ta.valuewhen",
            "condition"
        ) | ("strategy.close" | "strategy.close_all", "immediately")
    )
}

/// Positional parameter names for built-ins whose `bool` parameters are
/// checked.
pub(crate) fn bool_param_positions(func: &str) -> &'static [&'static str] {
    match func {
        "alertcondition" => &["condition", "title", "message"],
        "ta.barssince" => &["condition"],
        "ta.valuewhen" => &["condition", "source", "occurrence"],
        _ => &[],
    }
}

/// Plot-family calls whose `offset` parameter is `simple int`.
pub(crate) fn has_simple_offset(func: &str) -> bool {
    matches!(
        func,
        "plot" | "plotshape" | "plotchar" | "plotarrow" | "bgcolor" | "barcolor"
    )
}

/// Built-ins that take a named-only `transp` argument.
pub(crate) fn accepts_transp(func: &str) -> bool {
    matches!(
        func,
        "plot" | "plotshape" | "plotchar" | "plotarrow" | "bgcolor" | "fill"
    )
}

/// The v5 `strategy.*` calls that had a `when` parameter.
pub(crate) fn has_when(func: &str) -> bool {
    matches!(
        func,
        "strategy.entry"
            | "strategy.order"
            | "strategy.exit"
            | "strategy.close"
            | "strategy.close_all"
            | "strategy.cancel"
            | "strategy.cancel_all"
    )
}

/// Parameters that take a timeframe string. Passing `timeframe.period` to
/// one is unaffected by whether the daily timeframe is spelled `"D"` or
/// `"1D"`, because both spellings name the same timeframe.
pub(crate) fn is_timeframe_param(func: &str, param: &str) -> bool {
    matches!(
        (func, param),
        (_, "timeframe") if func.starts_with("request.") || matches!(func, "time" | "time_close" | "timeframe.in_seconds" | "timeframe.change" | "indicator" | "input.timeframe")
    ) || matches!(
        (func, param),
        ("ta.vwap" | "ta.pivot_point_levels", "anchor")
    )
}

/// Parameters restricted to non-`series` arguments.
pub(crate) fn requires_non_series(func: &str, param: &str) -> bool {
    if func.starts_with("input") || matches!(func, "indicator" | "strategy" | "library") {
        return true;
    }
    if func.starts_with("ta.") {
        return matches!(
            param,
            "length" | "len" | "leftbars" | "rightbars" | "mult_length"
        );
    }
    matches!(
        (func, param),
        (
            "plot"
                | "plotshape"
                | "plotchar"
                | "plotarrow"
                | "bgcolor"
                | "barcolor"
                | "hline"
                | "fill",
            "title"
        ) | (
            "plot" | "plotshape" | "plotchar" | "plotarrow" | "bgcolor" | "barcolor",
            "offset"
        ) | (
            "plotshape" | "plotchar",
            "style" | "location" | "size" | "char"
        ) | ("max_bars_back", "num")
    )
}

/// The positional parameter list of common calls, for binding arguments when
/// checking their qualifiers (a subset: `ta.*` length parameters).
pub(crate) fn ta_params(func: &str) -> &'static [&'static str] {
    match func {
        "ta.sma" | "ta.ema" | "ta.rma" | "ta.wma" | "ta.hma" | "ta.vwma" | "ta.swma" | "ta.rsi"
        | "ta.cci" | "ta.cmo" | "ta.mfi" | "ta.roc" | "ta.mom" | "ta.dev" | "ta.variance"
        | "ta.median" | "ta.mode" | "ta.range" | "ta.highest" | "ta.lowest" | "ta.highestbars"
        | "ta.lowestbars" | "ta.percentrank" | "ta.falling" | "ta.rising" | "ta.cog"
        | "ta.change" | "math.sum" => &["source", "length"],
        "ta.stdev" => &["source", "length", "biased"],
        "ta.atr" => &["length"],
        "ta.linreg" => &["source", "length", "offset"],
        "ta.stoch" => &["source", "high", "low", "length"],
        "ta.alma" => &["series", "length", "offset", "sigma", "floor"],
        _ => &[],
    }
}
