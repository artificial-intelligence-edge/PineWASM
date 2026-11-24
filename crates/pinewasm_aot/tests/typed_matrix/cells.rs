//! The typed matrix's generation tables and cell enumeration: a pure function
//! of the tables, shared by `tests/typed_matrix.rs` (which builds and runs
//! every cell) and `tests/emitter_byte_gate.rs` (which pins the bytes the
//! backend emits for every cell). See `tests/typed_matrix.rs` for what the
//! tables mean.

use std::fmt::Write as _;

/// The title of the plot designated as the module's signal in every cell.
pub const SIGNAL_TITLE: &str = "Backtest Signal";

// ---------------------------------------------------------------------------
// The tables.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ty {
    Int,
    Float,
    Bool,
}

/// One way a value reaches a consumer: the declarations it needs and the
/// expression that reads it.
pub struct Source {
    pub id: &'static str,
    pub ty: Ty,
    pub declarations: &'static str,
    pub value: &'static str,
}

pub const fn source(
    id: &'static str,
    ty: Ty,
    declarations: &'static str,
    value: &'static str,
) -> Source {
    Source {
        id,
        ty,
        declarations,
        value,
    }
}

pub const SOURCES: &[Source] = &[
    // ---- int ------------------------------------------------------------
    source("int.literal", Ty::Int, "", "8"),
    source("int.input", Ty::Int, "n = input.int(8, \"N\")", "n"),
    source(
        "int.input.bounded",
        Ty::Int,
        "n = input.int(8, \"N\", minval=1, maxval=100)",
        "n",
    ),
    source("int.series.bar_index", Ty::Int, "", "bar_index"),
    source("int.var.plain", Ty::Int, "int n = 8", "n"),
    source("int.var.persistent", Ty::Int, "var int n = 8", "n"),
    source(
        "int.var.later",
        Ty::Int,
        "var int n = na\nn := bar_index > 3 ? 8 : n",
        "n",
    ),
    source(
        "int.field",
        Ty::Int,
        "type S\n    float src\n    int n\nS st = S.new(close, input.int(8, \"N\"))",
        "st.n",
    ),
    source(
        "int.field.var",
        Ty::Int,
        "type S\n    float src\n    int n\nvar S st = S.new(0.0, 8)",
        "st.n",
    ),
    source(
        "int.field.bounded",
        Ty::Int,
        "type S\n    float src\n    int n\nS st = S.new(close, input.int(8, \"N\", minval=1, maxval=100))",
        "st.n",
    ),
    source("int.array", Ty::Int, "a = array.new_int(2, 8)", "array.get(a, 0)"),
    source("int.function", Ty::Int, "f(int a) => a + 1", "f(7)"),
    source(
        "int.tuple",
        Ty::Int,
        "g() => [bar_index, close]\n[t0, t1] = g()",
        "t0",
    ),
    source("int.history", Ty::Int, "", "bar_index[1]"),
    source(
        "int.ternary",
        Ty::Int,
        "type S\n    int n\nS st = S.new(input.int(8, \"N\"))",
        "(close > open ? st.n : 5)",
    ),
    source(
        "int.loop",
        Ty::Int,
        "int acc = 0\nfor i = 0 to 2\n    acc := acc + i",
        "acc",
    ),
    source(
        "int.switch",
        Ty::Int,
        "int sw = switch\n    close > open => 8\n    => 5",
        "sw",
    ),
    source("int.na", Ty::Int, "int n = na", "n"),
    // A variable that holds an integer quotient: statically an int, a float
    // at run time on the bars where the division is uneven.
    source("int.quotient", Ty::Int, "int q = bar_index / 2", "q"),
    source(
        "int.quotient.var",
        Ty::Int,
        "var int q = na\nq := bar_index / 3",
        "q",
    ),
    // ---- float ----------------------------------------------------------
    source("float.literal", Ty::Float, "", "8.5"),
    source("float.input", Ty::Float, "xf = input.float(8.5, \"X\")", "xf"),
    source(
        "float.input.bounded",
        Ty::Float,
        "xf = input.float(8.5, \"X\", minval=0.5, maxval=100.0)",
        "xf",
    ),
    source("float.series.close", Ty::Float, "", "close"),
    source("float.series.volume", Ty::Float, "", "volume"),
    source("float.var.plain", Ty::Float, "float xf = 8.5", "xf"),
    source("float.var.persistent", Ty::Float, "var float xf = 8.5", "xf"),
    source(
        "float.var.later",
        Ty::Float,
        "var float xf = na\nxf := bar_index > 3 ? 8.5 : xf",
        "xf",
    ),
    source(
        "float.field",
        Ty::Float,
        "type S\n    float src\n    int n\nS st = S.new(close, 8)",
        "st.src",
    ),
    source(
        "float.field.var",
        Ty::Float,
        "type S\n    float src\n    int n\nvar S st = S.new(8.5, 8)",
        "st.src",
    ),
    source(
        "float.field.bounded",
        Ty::Float,
        "type S\n    float src\n    int n\nS st = S.new(input.float(8.5, \"X\", minval=0.5, maxval=100.0), 8)",
        "st.src",
    ),
    source(
        "float.array",
        Ty::Float,
        "a = array.new_float(2, 8.5)",
        "array.get(a, 0)",
    ),
    source("float.function", Ty::Float, "f(float a) => a + 1.5", "f(7.0)"),
    source(
        "float.tuple",
        Ty::Float,
        "g() => [bar_index, close]\n[t0, t1] = g()",
        "t1",
    ),
    source("float.history", Ty::Float, "", "close[1]"),
    source("float.ternary", Ty::Float, "", "(close > open ? close : 8.5)"),
    source(
        "float.loop",
        Ty::Float,
        "float acc = 0.0\nfor i = 0 to 2\n    acc := acc + close",
        "acc",
    ),
    source(
        "float.switch",
        Ty::Float,
        "float sw = switch\n    close > open => 8.5\n    => 5.5",
        "sw",
    ),
    source("float.na", Ty::Float, "float xf = na", "xf"),
    // ---- bool -----------------------------------------------------------
    source("bool.literal", Ty::Bool, "", "true"),
    source("bool.input", Ty::Bool, "b = input.bool(true, \"B\")", "b"),
    source("bool.series.comparison", Ty::Bool, "", "(close > open)"),
    source("bool.var.plain", Ty::Bool, "bool b = close > open", "b"),
    source("bool.var.persistent", Ty::Bool, "var bool b = true", "b"),
    source(
        "bool.var.later",
        Ty::Bool,
        "var bool b = false\nb := bar_index > 3 ? true : b",
        "b",
    ),
    source(
        "bool.field",
        Ty::Bool,
        "type S\n    float src\n    bool b\nS st = S.new(close, close > open)",
        "st.b",
    ),
    source(
        "bool.field.var",
        Ty::Bool,
        "type S\n    float src\n    bool b\nvar S st = S.new(0.0, true)",
        "st.b",
    ),
    source(
        "bool.array",
        Ty::Bool,
        "a = array.new_bool(2, true)",
        "array.get(a, 0)",
    ),
    source("bool.function", Ty::Bool, "f(bool a) => not a", "f(false)"),
    source(
        "bool.tuple",
        Ty::Bool,
        "g() => [close > open, close]\n[t0, t1] = g()",
        "t0",
    ),
    source("bool.history", Ty::Bool, "", "(close > open)[1]"),
    source(
        "bool.ternary",
        Ty::Bool,
        "b = input.bool(false, \"B\")",
        "(close > open ? b : true)",
    ),
    source(
        "bool.loop",
        Ty::Bool,
        "bool acc = false\nfor i = 0 to 2\n    acc := acc or close > open",
        "acc",
    ),
    source(
        "bool.switch",
        Ty::Bool,
        "bool sw = switch\n    close > open => true\n    => false",
        "sw",
    ),
];

/// The argument types a consumer takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Domain {
    /// `int` or `float`.
    Numeric,
    /// `int` only (a length, a history offset).
    Int,
    Bool,
    Any,
}

impl Domain {
    pub fn admits(self, ty: Ty) -> bool {
        match self {
            Domain::Numeric => ty != Ty::Bool,
            Domain::Int => ty == Ty::Int,
            Domain::Bool => ty == Ty::Bool,
            Domain::Any => true,
        }
    }
}

/// The type a consumer returns in this front end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Returns {
    /// The argument's type.
    Same,
    Int,
    Float,
    Bool,
}

/// One typed reader of a value. `{}` in the template is the value.
pub struct Consumer {
    pub id: &'static str,
    pub template: &'static str,
    pub domain: Domain,
    pub returns: Returns,
}

impl Consumer {
    pub fn result(&self, argument: Ty) -> Option<Ty> {
        self.domain.admits(argument).then_some(match self.returns {
            Returns::Same => argument,
            Returns::Int => Ty::Int,
            Returns::Float => Ty::Float,
            Returns::Bool => Ty::Bool,
        })
    }

    pub fn apply(&self, value: &str) -> String {
        self.template.replace("{}", value)
    }
}

pub const fn consumer(
    id: &'static str,
    template: &'static str,
    domain: Domain,
    returns: Returns,
) -> Consumer {
    Consumer {
        id,
        template,
        domain,
        returns,
    }
}

pub const fn numeric(id: &'static str, template: &'static str, returns: Returns) -> Consumer {
    consumer(id, template, Domain::Numeric, returns)
}

/// PineWASM's own list of the built-in forms this front end is tested with,
/// each with the argument domain it admits and the type it returns here.
pub const CONSUMERS: &[Consumer] = &[
    // `plot` is every cell's last consumer; `identity` is the cell where it
    // is the only one.
    consumer("identity", "{}", Domain::Any, Returns::Same),
    // ---- numeric math.* -------------------------------------------------
    numeric("math.abs(v)", "math.abs({})", Returns::Same),
    numeric("math.sign(v)", "math.sign({})", Returns::Same),
    numeric("math.floor(v)", "math.floor({})", Returns::Int),
    numeric("math.ceil(v)", "math.ceil({})", Returns::Int),
    numeric("math.round(v)", "math.round({})", Returns::Int),
    numeric("math.round(v,2)", "math.round({}, 2)", Returns::Float),
    numeric(
        "math.round_to_mintick(v)",
        "math.round_to_mintick({})",
        Returns::Float,
    ),
    numeric("math.sqrt(v)", "math.sqrt({})", Returns::Float),
    numeric("math.exp(v)", "math.exp({})", Returns::Float),
    numeric("math.log(v)", "math.log({})", Returns::Float),
    numeric("math.log10(v)", "math.log10({})", Returns::Float),
    numeric("math.sin(v)", "math.sin({})", Returns::Float),
    numeric("math.cos(v)", "math.cos({})", Returns::Float),
    numeric("math.tan(v)", "math.tan({})", Returns::Float),
    numeric("math.asin(v)", "math.asin({})", Returns::Float),
    numeric("math.acos(v)", "math.acos({})", Returns::Float),
    numeric("math.atan(v)", "math.atan({})", Returns::Float),
    numeric("math.todegrees(v)", "math.todegrees({})", Returns::Float),
    numeric("math.toradians(v)", "math.toradians({})", Returns::Float),
    numeric("math.pow(v,2)", "math.pow({}, 2)", Returns::Float),
    numeric("math.pow(2,v)", "math.pow(2, {})", Returns::Float),
    numeric("math.min(v,3)", "math.min({}, 3)", Returns::Same),
    numeric("math.max(v,3)", "math.max({}, 3)", Returns::Same),
    numeric("math.min(v,3.5)", "math.min({}, 3.5)", Returns::Float),
    numeric("math.max(v,3.5)", "math.max({}, 3.5)", Returns::Float),
    numeric("math.min(v,3,5)", "math.min({}, 3, 5)", Returns::Same),
    numeric("math.max(v,3,5)", "math.max({}, 3, 5)", Returns::Same),
    numeric("math.avg(v,3)", "math.avg({}, 3)", Returns::Float),
    numeric("math.avg(v,3,5)", "math.avg({}, 3, 5)", Returns::Float),
    // ---- arithmetic -----------------------------------------------------
    numeric("v+1", "({} + 1)", Returns::Same),
    numeric("v-1", "({} - 1)", Returns::Same),
    numeric("v*2", "({} * 2)", Returns::Same),
    numeric("v/2", "({} / 2)", Returns::Same),
    numeric("v%3", "({} % 3)", Returns::Same),
    numeric("v+1.5", "({} + 1.5)", Returns::Float),
    numeric("v*2.5", "({} * 2.5)", Returns::Float),
    numeric("v/2.5", "({} / 2.5)", Returns::Float),
    numeric("-v", "(-{})", Returns::Same),
    // ---- comparisons ----------------------------------------------------
    numeric("v>3", "({} > 3)", Returns::Bool),
    numeric("v>=3", "({} >= 3)", Returns::Bool),
    numeric("v<3", "({} < 3)", Returns::Bool),
    numeric("v<=3", "({} <= 3)", Returns::Bool),
    numeric("v==3", "({} == 3)", Returns::Bool),
    numeric("v!=3", "({} != 3)", Returns::Bool),
    // ---- casts and missing values ---------------------------------------
    numeric("int(v)", "int({})", Returns::Int),
    numeric("float(v)", "float({})", Returns::Float),
    numeric("bool(v)", "bool({})", Returns::Bool),
    numeric("nz(v)", "nz({})", Returns::Same),
    numeric("nz(v,1)", "nz({}, 1)", Returns::Same),
    numeric("na(v)", "na({})", Returns::Bool),
    numeric("fixnan(v)", "fixnan({})", Returns::Same),
    // ---- ta.* arguments and the history index ---------------------------
    numeric("ta.sma(v,3)", "ta.sma({}, 3)", Returns::Float),
    numeric("ta.ema(v,3)", "ta.ema({}, 3)", Returns::Float),
    // This front end types the result of `ta.change` as `float`, so the
    // `widen` cells of an int source are pinned as front-end rejections
    // (`E0226`).
    numeric("ta.change(v)", "ta.change({})", Returns::Same),
    numeric("ta.highest(v,3)", "ta.highest({}, 3)", Returns::Float),
    consumer(
        "ta.sma(close,v)",
        "ta.sma(close, {})",
        Domain::Int,
        Returns::Float,
    ),
    // `ta.ema` takes a `simple int` length, so a series length is rejected by
    // the front end (`E0219`); those cells are pinned as such.
    consumer(
        "ta.ema(close,v)",
        "ta.ema(close, {})",
        Domain::Int,
        Returns::Float,
    ),
    consumer("close[v]", "close[{}]", Domain::Int, Returns::Float),
    // ---- readers of an integer quotient ---------------------------------
    // In PineWASM, `int / int` is an int-typed value whose uneven quotient
    // keeps its fraction, so what reads `v / 2` decides whether a module needs
    // a cell that holds either an int or a float. `v / 3` is uneven for more
    // sources than the `v/2` cell above.
    consumer("int(v/2)", "int({} / 2)", Domain::Int, Returns::Int),
    consumer(
        "math.floor(v/2)",
        "math.floor({} / 2)",
        Domain::Int,
        Returns::Int,
    ),
    consumer(
        "math.ceil(v/2)",
        "math.ceil({} / 2)",
        Domain::Int,
        Returns::Int,
    ),
    consumer(
        "math.round(v/2)",
        "math.round({} / 2)",
        Domain::Int,
        Returns::Int,
    ),
    consumer("v/2+1", "({} / 2 + 1)", Domain::Int, Returns::Int),
    consumer("v/2*2", "({} / 2 * 2)", Domain::Int, Returns::Int),
    consumer("-(v/2)", "(-({} / 2))", Domain::Int, Returns::Int),
    consumer("v/2>1", "({} / 2 > 1)", Domain::Int, Returns::Bool),
    // A computed quotient is a legal history target even when its operands
    // are const, input or simple; expression-site capture supplies its lane.
    consumer("(v/2)[1]", "({} / 2)[1]", Domain::Int, Returns::Int),
    consumer("v/3", "({} / 3)", Domain::Int, Returns::Int),
    // ---- an integer quotient as a `ta.*` source --------------------------
    // `ta.valuewhen` and `fixnan` over an int hold the captured value in the
    // engine's float cell, so they return an uneven quotient with its
    // fraction: a module must carry it or decline, never truncate it. The
    // other readers take the quotient as a float source. `v / 3` is uneven
    // for more sources than `v / 2`.
    numeric(
        "ta.valuewhen(c,v,0)",
        "ta.valuewhen(close > open, {}, 0)",
        Returns::Same,
    ),
    consumer(
        "ta.valuewhen(c,v/2,0)",
        "ta.valuewhen(close > open, {} / 2, 0)",
        Domain::Int,
        Returns::Int,
    ),
    consumer(
        "ta.valuewhen(c,v/3,1)",
        "ta.valuewhen(close > open, {} / 3, 1)",
        Domain::Int,
        Returns::Int,
    ),
    consumer("fixnan(v/2)", "fixnan({} / 2)", Domain::Int, Returns::Int),
    consumer("fixnan(v/3)", "fixnan({} / 3)", Domain::Int, Returns::Int),
    // History retains the quotient's fraction. The int-result fixnan engine
    // requires fractional carry or an explicit PineIntDiv capability refusal.
    consumer(
        "fixnan((v/2)[1])",
        "fixnan(({} / 2)[1])",
        Domain::Int,
        Returns::Int,
    ),
    consumer("nz(v/2)", "nz({} / 2)", Domain::Int, Returns::Int),
    consumer(
        "ta.change(v/2)",
        "ta.change({} / 2)",
        Domain::Int,
        Returns::Float,
    ),
    consumer(
        "ta.highest(v/2,3)",
        "ta.highest({} / 2, 3)",
        Domain::Int,
        Returns::Float,
    ),
    consumer(
        "ta.lowest(v/3,3)",
        "ta.lowest({} / 3, 3)",
        Domain::Int,
        Returns::Float,
    ),
    // ---- bool -----------------------------------------------------------
    consumer("not v", "(not {})", Domain::Bool, Returns::Bool),
    consumer(
        "v and c",
        "({} and close > open)",
        Domain::Bool,
        Returns::Bool,
    ),
    consumer(
        "v or c",
        "({} or close > open)",
        Domain::Bool,
        Returns::Bool,
    ),
    consumer("v==true", "({} == true)", Domain::Bool, Returns::Bool),
    consumer("v!=true", "({} != true)", Domain::Bool, Returns::Bool),
    consumer("v?1:0", "({} ? 1 : 0)", Domain::Bool, Returns::Int),
    consumer(
        "v?1.5:0.5",
        "({} ? 1.5 : 0.5)",
        Domain::Bool,
        Returns::Float,
    ),
];

/// Where a consumer's result goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    /// Bound to an `int`, then used in float arithmetic. The shape of the
    /// script that exercises the int-widening path.
    Widen,
    /// Bound to a `float` (an `int` result is widened at the binding).
    Float,
    /// Bound to a `bool`.
    Bool,
    /// Not bound: the expression is the plot argument.
    Direct,
}

impl Context {
    pub fn id(self) -> &'static str {
        match self {
            Context::Widen => "widen",
            Context::Float => "float",
            Context::Bool => "bool",
            Context::Direct => "direct",
        }
    }

    /// The contexts a result of type `ty` is placed in, the first being the
    /// one the depth-2 layer uses.
    pub fn for_result(ty: Ty) -> &'static [Context] {
        match ty {
            Ty::Int => &[Context::Widen, Context::Float, Context::Direct],
            Ty::Float => &[Context::Float, Context::Direct],
            Ty::Bool => &[Context::Bool, Context::Direct],
        }
    }
}

/// The value sources of the depth-2 layer.
pub const DEPTH2_SOURCES: &[&str] = &[
    "int.input",
    "int.input.bounded",
    "int.series.bar_index",
    "int.var.later",
    "int.field",
    "int.field.var",
    "int.array",
    "float.input",
    "float.series.close",
    "float.field",
];

/// The consumers of the depth-2 layer, used as both the inner and the outer
/// one wherever the inner result type is in the outer domain.
pub const DEPTH2_CONSUMERS: &[&str] = &[
    "math.abs(v)",
    "math.sign(v)",
    "math.floor(v)",
    "math.round(v)",
    "math.min(v,3)",
    "math.max(v,3)",
    "math.avg(v,3)",
    "v+1",
    "v*2",
    "-v",
    "v>3",
    "int(v)",
    "float(v)",
    "nz(v)",
    "v?1:0",
];

// ---------------------------------------------------------------------------
// Enumeration.

/// One generated program.
pub struct Cell {
    pub id: String,
    pub program: String,
}

pub fn program(declarations: &str, expression: &str, result: Ty, context: Context) -> String {
    let mut out = String::from("//@version=6\nindicator(\"typed matrix\")\n");
    if !declarations.is_empty() {
        out.push_str(declarations);
        out.push('\n');
    }
    match (context, result) {
        (Context::Widen, _) => {
            let _ = write!(
                out,
                "int k = {expression}\nx = close / k * 100\n\
                 plot(x > 50 ? 1 : 0, \"{SIGNAL_TITLE}\")\nplot(k, \"Value\")\nplot(x, \"Widened\")\n"
            );
        }
        (Context::Float, _) => {
            let _ = write!(
                out,
                "float k = {expression}\nplot(k > 5 ? 1 : 0, \"{SIGNAL_TITLE}\")\nplot(k, \"Value\")\n"
            );
        }
        (Context::Bool, _) => {
            let _ = write!(
                out,
                "bool k = {expression}\nplot(k ? 1 : 0, \"{SIGNAL_TITLE}\")\n"
            );
        }
        (Context::Direct, Ty::Bool) => {
            let _ = writeln!(out, "plot({expression} ? 1 : 0, \"{SIGNAL_TITLE}\")");
        }
        (Context::Direct, _) => {
            let _ = write!(
                out,
                "plot({expression} > 5 ? 1 : 0, \"{SIGNAL_TITLE}\")\nplot({expression}, \"Value\")\n"
            );
        }
    }
    out
}

pub fn consumer_by_id(id: &str) -> &'static Consumer {
    CONSUMERS
        .iter()
        .find(|consumer| consumer.id == id)
        .unwrap_or_else(|| panic!("no consumer {id}"))
}

pub fn source_by_id(id: &str) -> &'static Source {
    SOURCES
        .iter()
        .find(|source| source.id == id)
        .unwrap_or_else(|| panic!("no source {id}"))
}

/// Every cell, in table order: the full depth-1 cross product, then the
/// depth-2 layer.
pub fn cells() -> Vec<Cell> {
    let mut out = Vec::new();
    for source in SOURCES {
        for consumer in CONSUMERS {
            let Some(result) = consumer.result(source.ty) else {
                continue;
            };
            for &context in Context::for_result(result) {
                out.push(Cell {
                    id: format!("{}|{}|{}", source.id, consumer.id, context.id()),
                    program: program(
                        source.declarations,
                        &consumer.apply(source.value),
                        result,
                        context,
                    ),
                });
            }
        }
    }
    for source in DEPTH2_SOURCES.iter().map(|id| source_by_id(id)) {
        for inner in DEPTH2_CONSUMERS.iter().map(|id| consumer_by_id(id)) {
            let Some(between) = inner.result(source.ty) else {
                continue;
            };
            for outer in DEPTH2_CONSUMERS.iter().map(|id| consumer_by_id(id)) {
                let Some(result) = outer.result(between) else {
                    continue;
                };
                let context = Context::for_result(result)[0];
                out.push(Cell {
                    id: format!("{}|{}=>{}|{}", source.id, inner.id, outer.id, context.id()),
                    program: program(
                        source.declarations,
                        &outer.apply(&inner.apply(source.value)),
                        result,
                        context,
                    ),
                });
            }
        }
    }
    out
}
