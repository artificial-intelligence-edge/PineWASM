//! Typed, resolved HIR for PineWASM.
//!
//! HIR is not a copy of the AST. By the time a program reaches HIR:
//! - every identifier is resolved to a builtin or a numbered [`SymbolId`];
//! - every expression carries a [`Type`] and [`Qualifier`];
//! - history references are desugared into offset reads
//!   ([`HirExprKind::ReadBuiltin`] / [`HirExprKind::ReadVar`]);
//! - `plot(...)` and `indicator(...)` calls are desugared into effect
//!   statements / program metadata.
//!
//! The resolver and type checker that build HIR live in
//! `pinewasm_semantics`.
#![forbid(unsafe_code)]

use pinewasm_span::Span;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

mod signed_decimal;

/// Parse optional ASCII `+` or `-` followed by one or more ASCII decimal
/// digits, returning `None` for malformed input or a value outside `i64`.
/// Whitespace and non-ASCII digits are not accepted. This shares the checked
/// parser used by durable-wire integer deserialization.
pub fn parse_i64_decimal_bytes(bytes: &[u8]) -> Option<i64> {
    signed_decimal::parse(bytes).ok()
}

/// Deserialize a durable-wire `i64` from either its native integer form or
/// the decimal string emitted by JSON-safe JavaScript hosts for values beyond
/// `Number.MAX_SAFE_INTEGER`. Serialization remains the normal `i64` form;
/// this is intentionally a read-side compatibility adapter.
pub fn deserialize_i64_number_or_string<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum WireI64 {
        Integer(i64),
        Decimal(String),
    }

    match WireI64::deserialize(deserializer)? {
        WireI64::Integer(value) => Ok(value),
        WireI64::Decimal(value) => {
            signed_decimal::parse(value.as_bytes()).map_err(serde::de::Error::custom)
        }
    }
}

pub mod builtins;
// Re-exported at the crate root so `pinewasm_hir::PureFunc` paths resolve.
pub use builtins::*;

pub mod drawing;
pub use drawing::{CellAttr, DrawDefault, DrawField, DrawKind};
pub mod display_set;
pub mod drawing_options;
pub mod text_format;

pub mod footprint;

// ---------------------------------------------------------------------------
// Types and qualifiers
// ---------------------------------------------------------------------------

/// The type model: scalars (`bool`/`int`/`float`/`string`/`color`), the
/// reference-typed collections (`array`/`matrix`/`map`), and the nominal
/// user-defined enum/object types. Series-ness is *not* a type constructor:
/// it is modeled by [`Qualifier::Series`] in the `const < input < simple <
/// series` lattice, carried alongside the `Type` wherever both matter.
/// Number literals without a decimal point are `Int`; with one they are
/// `Float`. Mixed int/float arithmetic promotes the int side through an
/// explicit [`HirExprKind::IntToFloat`] coercion.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Type {
    Bool,
    /// 64-bit signed integer with exact semantics (overflow yields `na` at
    /// runtime).
    Int,
    Float,
    String,
    /// Built-in formatting constants. Distinct from a user enum named
    /// `text_format`; values use our canonical string lane.
    TextFormat,
    /// Built-in `display.*` destination sets (Pine's `plot_display`). `+` and
    /// `-` are set union and difference, and a string is not a display
    /// value. Values use the canonical tokens of [`display_set`] on our string
    /// lane.
    PlotDisplay,
    PlotSimpleDisplay,
    /// An RGBA color (`#RRGGBB`/`#RRGGBBAA` literals, `color.*` constants and
    /// functions). Values are `Value::Color { r, g, b, t }`.
    Color,
    /// Result of effect calls such as `alert(...)`.
    Void,
    /// A plot identifier returned when `plot(...)` is captured.
    Plot,
    /// A horizontal-line identifier. Pine's fill overloads distinguish these
    /// from plots and reject mixed handle types.
    Hline,
    /// A reference-typed `array<T>` collection. `T` is one of the scalar
    /// element types (`Bool`/`Int`/`Float`/`String`/`Color`); nested
    /// collections are not representable (Pine forbids them except via UDTs).
    /// Equality is by element type: `array<int>` is not `array<float>` and
    /// there is no implicit element coercion (see `E0234`).
    Array(Box<Type>),
    /// A reference-typed `matrix<T>` collection.
    Matrix(Box<Type>),
    /// A reference-typed `map<K, V>` collection.
    Map(Box<Type>, Box<Type>),
    /// A user-defined enum type, identified by its declared name. Enum
    /// values are distinct from `int` even though they lower to an integer
    /// variant index at runtime: the checker only permits `==`/`!=` and switch
    /// between values of the *same* enum type. Two enums with the same members
    /// are still distinct types (nominal, not structural).
    Enum(String),
    /// A user-defined type, identified by its declared name. Instances
    /// are reference values on the VM heap (`HeapObj::Object`), constructed with
    /// `Name.new(...)` / `Name.copy(obj)`. Two types with the same fields are
    /// still distinct types (nominal). An unassigned object variable is `na`.
    Object(String),
    /// Not yet inferred.
    Unknown,
    /// Produced by checker failures; suppresses cascading diagnostics.
    Error,
}

impl Type {
    pub fn describe(&self) -> String {
        match self {
            Type::Bool => "bool".into(),
            Type::Int => "int".into(),
            Type::Float => "float".into(),
            Type::String => "string".into(),
            Type::TextFormat => text_format::TEXT_FORMAT_TYPE.into(),
            Type::PlotDisplay => display_set::PLOT_DISPLAY_TYPE.into(),
            Type::PlotSimpleDisplay => "plot_simple_display".into(),
            Type::Color => "color".into(),
            Type::Void => "void".into(),
            Type::Plot => "plot".into(),
            Type::Hline => "hline".into(),
            Type::Array(elem) => format!("array<{}>", elem.describe()),
            Type::Matrix(elem) => format!("matrix<{}>", elem.describe()),
            Type::Map(key, val) => format!("map<{}, {}>", key.describe(), val.describe()),
            Type::Enum(name) => name.clone(),
            Type::Object(name) => name.clone(),
            Type::Unknown => "unknown".into(),
            Type::Error => "<error>".into(),
        }
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Type::Error)
    }

    /// A nominal built-in constant type; it is not a string or a valid
    /// explicit Pine type keyword.
    pub fn is_text_format(&self) -> bool {
        matches!(self, Type::TextFormat)
    }

    /// The nominal built-in `plot_display` type of `display.*` values. It is
    /// not a string and not a valid explicit Pine type keyword.
    pub fn is_plot_display(&self) -> bool {
        matches!(self, Type::PlotDisplay | Type::PlotSimpleDisplay)
    }

    /// True for the reference-typed collection shapes (`array`/`matrix`/`map`).
    /// These hold a `HandleId` at runtime and follow reference semantics.
    pub fn is_collection(&self) -> bool {
        matches!(self, Type::Array(_) | Type::Matrix(_) | Type::Map(..))
    }

    /// The scalar element type of an `array<T>`, or `None` for non-arrays.
    pub fn array_elem(&self) -> Option<&Type> {
        match self {
            Type::Array(elem) => Some(elem),
            _ => None,
        }
    }

    /// The `(key, value)` element types of a `map<K, V>`, or `None` otherwise.
    pub fn map_types(&self) -> Option<(&Type, &Type)> {
        match self {
            Type::Map(k, v) => Some((k, v)),
            _ => None,
        }
    }

    /// The element type of a `matrix<T>`, or `None` for non-matrices.
    pub fn matrix_elem(&self) -> Option<&Type> {
        match self {
            Type::Matrix(elem) => Some(elem),
            _ => None,
        }
    }
}

/// The 17 named `color.*` constants and their RGB values, as PineWASM uses them.
/// All are fully opaque (`t = 0`). These are not the classic HTML basic colors
/// (e.g. `color.red` = `#F23645`, `color.blue` = `#2962FF`, `color.green` = `#4CAF50`,
/// not `#FF0000`/`#0000FF`/`#008000`).
pub const NAMED_COLORS: [(&str, u8, u8, u8); 17] = [
    ("color.aqua", 0x00, 0xBC, 0xD4),
    ("color.black", 0x36, 0x3A, 0x45),
    ("color.blue", 0x29, 0x62, 0xFF),
    ("color.fuchsia", 0xE0, 0x40, 0xFB),
    ("color.gray", 0x78, 0x7B, 0x86),
    ("color.green", 0x4C, 0xAF, 0x50),
    ("color.lime", 0x00, 0xE6, 0x76),
    ("color.maroon", 0x88, 0x0E, 0x4F),
    ("color.navy", 0x31, 0x1B, 0x92),
    ("color.olive", 0x80, 0x80, 0x00),
    ("color.orange", 0xFF, 0x98, 0x00),
    ("color.purple", 0x9C, 0x27, 0xB0),
    ("color.red", 0xF2, 0x36, 0x45),
    ("color.silver", 0xB2, 0xB5, 0xBE),
    ("color.teal", 0x08, 0x99, 0x81),
    ("color.white", 0xFF, 0xFF, 0xFF),
    ("color.yellow", 0xFD, 0xD8, 0x35),
];

/// Pine-style type qualifiers, ordered as a lattice:
/// `Const < Input < Simple < Series`. Combining values takes the join
/// (weakest guarantee wins).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Qualifier {
    Const,
    Input,
    Simple,
    Series,
}

/// Position of `q` in the lattice `Const < Input < Simple < Series`: the
/// declaration index the derived `Ord` compares.
pub const fn qualifier_rank(q: Qualifier) -> u8 {
    match q {
        Qualifier::Const => 0,
        Qualifier::Input => 1,
        Qualifier::Simple => 2,
        Qualifier::Series => 3,
    }
}

/// [`Qualifier::join`] as a free function (written in the Aeneas-extractable style):
/// the larger qualifier, `b` on a tie, exactly as `Ord::max(a, b)`.
pub const fn qualifier_join(a: Qualifier, b: Qualifier) -> Qualifier {
    if qualifier_rank(a) <= qualifier_rank(b) {
        b
    } else {
        a
    }
}

impl Qualifier {
    pub fn join(self, other: Qualifier) -> Qualifier {
        qualifier_join(self, other)
    }

    pub fn describe(self) -> &'static str {
        match self {
            Qualifier::Const => "const",
            Qualifier::Input => "input",
            Qualifier::Simple => "simple",
            Qualifier::Series => "series",
        }
    }
}

// ---------------------------------------------------------------------------
// Symbols, slots, builtins
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SymbolId(pub u32);

/// Storage slot for a user variable's series history. Every assigned
/// variable gets a slot so `x[1]` works on later bars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SeriesSlot(pub u32);

/// Output slot for one syntactic `plot(...)` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PlotSlot(pub u32);

/// Output slot for a color-only chart visual such as `bgcolor` or `barcolor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct VisualSlot(pub u32);

/// Output slot for a `fill(id1, id2, color)` band between two plots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FillSlot(pub u32);

/// The display primitive represented by one runtime output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayKind {
    #[default]
    Plot,
    PlotShape,
    PlotChar,
    PlotArrow,
    PlotCandle,
    PlotBar,
    Hline,
    Bgcolor,
    Barcolor,
}

/// `plot.style_*` selection retained for host renderers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlotStyleKind {
    #[default]
    Line,
    LineBreak,
    StepLine,
    StepLineBreak,
    StepLineDiamond,
    Histogram,
    Cross,
    Area,
    AreaBreak,
    Columns,
    Circles,
}

/// Line dash style shared by `plot(..., linestyle=)` and `hline`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlotLineStyle {
    #[default]
    Solid,
    Dotted,
    Dashed,
}

/// The named `display.*` destination requested by a display call.
/// The screener destination is the `display.pine_screener` token.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlotDisplay {
    #[default]
    All,
    None,
    Pane,
    DataWindow,
    PriceScale,
    StatusLine,
    /// The pane and the price scale.
    PanePriceScale,
    /// The pane and the status line.
    PaneStatusLine,
    /// The price scale and the status line.
    PriceScaleStatusLine,
    /// The pane, the price scale and the status line.
    PanePriceScaleStatusLine,
    /// The pane and the Data Window.
    PaneDataWindow,
    /// The price scale and the Data Window.
    PriceScaleDataWindow,
    /// The pane, the price scale and the Data Window.
    PanePriceScaleDataWindow,
    /// The status line and the Data Window.
    StatusLineDataWindow,
    /// The pane, the status line and the Data Window.
    PaneStatusLineDataWindow,
    /// The price scale, the status line and the Data Window.
    PriceScaleStatusLineDataWindow,
    /// The pane, the price scale, the status line and the Data Window.
    PanePriceScaleStatusLineDataWindow,
    /// screener.
    PineScreener,
    /// The pane and screener.
    PanePineScreener,
    /// The price scale and screener.
    PriceScalePineScreener,
    /// The pane, the price scale and screener.
    PanePriceScalePineScreener,
    /// The status line and screener.
    StatusLinePineScreener,
    /// The pane, the status line and screener.
    PaneStatusLinePineScreener,
    /// The price scale, the status line and screener.
    PriceScaleStatusLinePineScreener,
    /// The pane, the price scale, the status line and screener.
    PanePriceScaleStatusLinePineScreener,
    /// The Data Window and screener.
    DataWindowPineScreener,
    /// The pane, the Data Window and screener.
    PaneDataWindowPineScreener,
    /// The price scale, the Data Window and screener.
    PriceScaleDataWindowPineScreener,
    /// The pane, the price scale, the Data Window and screener.
    PanePriceScaleDataWindowPineScreener,
    /// The status line, the Data Window and screener.
    StatusLineDataWindowPineScreener,
    /// The pane, the status line, the Data Window and screener.
    PaneStatusLineDataWindowPineScreener,
    /// The price scale, the status line, the Data Window and screener.
    PriceScaleStatusLineDataWindowPineScreener,
}

/// One OHLC field in a grouped `plotcandle` or `plotbar` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OhlcField {
    Open,
    High,
    Low,
    Close,
}

/// `location.*` marker placement retained for `plotshape` and `plotchar`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkerLocation {
    AboveBar,
    BelowBar,
    /// `location.inbar`: the marker is drawn on the bar itself.
    InBar,
    Top,
    Bottom,
    Absolute,
}

/// `shape.*` marker selection retained for `plotshape`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkerShape {
    XCross,
    Cross,
    Circle,
    TriangleUp,
    TriangleDown,
    Flag,
    ArrowUp,
    ArrowDown,
    Square,
    Diamond,
    LabelUp,
    LabelDown,
}

/// `size.*` marker selection retained for display markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkerSize {
    Auto,
    Tiny,
    Small,
    Normal,
    Large,
    Huge,
}

/// The semantic role of one visual runtime channel. Gradient value roles carry numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlotColorRole {
    Color,
    Up,
    Down,
    Wick,
    Border,
    Text,
    /// Gradient upper color.
    TopColor,
    /// Gradient lower color.
    BottomColor,
    /// Gradient upper price, independent of the plot mask.
    TopValue,
    /// Gradient lower price, independent of the plot mask.
    BottomValue,
}

impl PlotColorRole {
    /// Wire value domain shared by execution and generated host admission.
    pub const fn value_domain(self) -> &'static str {
        match self {
            Self::TopValue | Self::BottomValue => "float-or-na",
            _ => "color-or-na",
        }
    }
}

impl DisplayKind {
    /// Ordered role domain for a numeric or color-only output. Ownership is
    /// carried by the enclosing output slot, rather than a title or palette ID.
    pub fn color_roles(self) -> &'static [PlotColorRole] {
        use PlotColorRole::{Border, Color, Down, Text, Up, Wick};
        match self {
            Self::PlotShape | Self::PlotChar => &[Color, Text],
            Self::PlotArrow => &[Up, Down],
            Self::PlotCandle => &[Color, Wick, Border],
            Self::Plot | Self::PlotBar | Self::Hline | Self::Bgcolor | Self::Barcolor => &[Color],
        }
    }
}

/// Solid and gradient fill lanes occupy a separate role domain. A single fill
/// uses either Color or all four gradient channels, in this stack order.
pub const FILL_COLOR_ROLES: &[PlotColorRole] = &[
    PlotColorRole::Color,
    PlotColorRole::TopValue,
    PlotColorRole::BottomValue,
    PlotColorRole::TopColor,
    PlotColorRole::BottomColor,
];

/// One visual option the portable display contract cannot encode exactly.
/// Keeping this on the wire prevents a missing value from being mistaken for a
/// supported hardcoded default; individual hosts remain free to support more.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnsupportedVisualOption {
    /// Pine call parameter whose value could not be represented portably.
    pub field: String,
    /// Human-readable explanation for host diagnostics and inspection tools.
    pub reason: String,
}

/// One style scalar the compiler could not fold to a literal, but which is
/// side-effect-free over `input.*` declarations and therefore fixed for the
/// whole run.
///
/// Pine's qualifier lattice is what makes this safe: an expression built only
/// from constants, input reads, and pure functions is at most `input`-qualified,
/// so it cannot change from bar to bar. The host resolves it against the run's
/// input overrides before creating an output. A genuine per-bar (`series`)
/// expression still lands in [`DisplayStyle::unsupported`] because this
/// contract carries style metadata once per output, not once per bar.
///
/// `field` is the Pine parameter name (`linewidth`, `offset`, ...), matching
/// [`UnsupportedVisualOption::field`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoundStyleScalar {
    /// Pine call parameter to which the resolved value applies.
    pub field: String,
    /// Side-effect-free expression evaluated from this run's input values.
    pub expression: InputActiveExpr,
}

/// A fixed presentation expression evaluated by the PineWASM VM. The
/// compiler materializes it in a hidden slot so functions, aliases and simple
/// chart-context values use the same semantics as script execution. A runtime
/// snapshot replaces this reference with a literal `BoundStyleScalar`; hosts
/// never read VM slots or independently execute these expressions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeStyleScalar {
    pub field: String,
    pub slot: SeriesSlot,
    pub qualifier: Qualifier,
}

/// Parameter names accepted by the fixed presentation binding
/// protocol. Export the same vocabulary to hosts and reject unknown targets
/// in serialized programs before execution.
pub const DISPLAY_STYLE_BINDING_FIELDS: &[&str] = &[
    "offset",
    "linewidth",
    "histbase",
    "show_last",
    "precision",
    "force_overlay",
    "trackprice",
    "join",
    "fillgaps",
    "editable",
    "minheight",
    "maxheight",
    "style",
    "linestyle",
    "display",
    "location",
    "size",
    "format",
    "char",
    "text",
    // Compiler-owned direct PlotBar input property, not a general expression target.
    "ohlc_body_color",
];

/// Fixed-style qualifier ceilings shared by the analyzer, bytecode guard
/// and host contract. `None` means this is not a style binding target.
pub fn display_style_binding_qualifier(field: &str) -> Option<Qualifier> {
    match field {
        "force_overlay" | "fillgaps" | "text" | "size" => Some(Qualifier::Const),
        "offset" => Some(Qualifier::Simple),
        field if DISPLAY_STYLE_BINDING_FIELDS.contains(&field) => Some(Qualifier::Input),
        _ => None,
    }
}

/// A color's compile-time identity, independent of its selected bars.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StyleColorSource {
    Constant {
        r: u8,
        g: u8,
        b: u8,
        #[serde(serialize_with = "serialize_transparency")]
        t: f64,
    },
    Input {
        input: u32,
    },
}

/// Color used when a `plot()` color argument is omitted, independent of named constants.
/// Channels are red, green, blue and transparency percentage respectively.
/// An explicit `na` color argument stays `na` and does not use this default.
pub const DEFAULT_PLOT_COLOR: [u8; 4] = [33, 150, 243, 0];

/// A statically known color choice, including a raw input.color source.
/// Selection between choices may still depend on a series condition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StyleColorPaletteEntry {
    pub role: PlotColorRole,
    pub source: StyleColorSource,
    /// Exact executed color, resolved by the engine using this run's inputs.
    /// Compiled declarations never carry a host-resolved snapshot.
    #[serde(
        default,
        rename = "resolvedColor",
        skip_serializing_if = "Option::is_none"
    )]
    pub resolved_color: Option<String>,
}

/// Whether a fill uses one run-wide paint or a per-bar paint channel.
/// This comes from expression qualifiers, not palette size or sampled values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FillColorMode {
    Fixed,
    Series,
}

/// Whether an ordinary plot uses one run-wide paint or a per-bar paint channel.
/// Retain the expression qualifier even when every sampled color is identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlotColorMode {
    Fixed,
    Series,
}

/// Why a plotarrow direction has its run-wide static style color.
/// Dynamic means fallback provenance, not the presence of a colorer lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArrowStaticColorOrigin {
    /// The argument was omitted, selecting the direction default.
    Omitted,
    /// The effective callsite expression folds to a constant color.
    Constant,
    /// The effective callsite expression is explicitly missing.
    Missing,
    /// Input, chart, history or persistent mutation requires static fallback.
    Dynamic,
}

/// A plotarrow direction's static color, separate from its runtime fill lane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArrowStaticRoleColor {
    /// Effective expression provenance, independent of palette cardinality.
    pub origin: ArrowStaticColorOrigin,
    /// Red channel in 0..=255.
    pub r: u8,
    /// Green channel in 0..=255.
    pub g: u8,
    /// Blue channel in 0..=255.
    pub b: u8,
    /// Exact transparency percentage; static alpha is `1 - t / 100`.
    #[serde(serialize_with = "serialize_transparency")]
    pub t: f64,
}

/// Static styles for both plotarrow directions; absent on other primitives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArrowStaticColors {
    /// Positive-value arrow style.
    pub up: ArrowStaticRoleColor,
    /// Negative-value arrow style.
    pub down: ArrowStaticRoleColor,
}

/// Public per-direction colorer binding, independent of static style origin.
/// A false role can still retain its VM lane as raw execution data; a host
/// selects the static direction style for paint. Old programs omit this record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArrowRuntimeColorers {
    /// Whether positive arrows use their runtime up-color lane for paint.
    pub up: bool,
    /// Whether negative arrows use their runtime down-color lane for paint.
    pub down: bool,
}

/// Public compiled OHLC body property color, separate from runtime paint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OhlcBodyColor {
    /// Red channel, encoded as an integer byte in 0..=255.
    pub r: u8,
    /// Green channel, encoded as an integer byte in 0..=255.
    pub g: u8,
    /// Blue channel, encoded as an integer byte in 0..=255.
    pub b: u8,
    /// Transparency percentage, finite in 0..=100; fractional values are valid.
    #[serde(
        serialize_with = "serialize_transparency",
        deserialize_with = "deserialize_ohlc_transparency"
    )]
    pub t: f64,
}

fn deserialize_ohlc_transparency<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<f64, D::Error> {
    let value = f64::deserialize(deserializer)?;
    if value.is_finite() && (0.0..=100.0).contains(&value) {
        Ok(value)
    } else {
        Err(serde::de::Error::custom(
            "OHLC transparency must be finite in 0..=100",
        ))
    }
}

fn deserialize_required_ohlc_color<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<OhlcBodyColor>, D::Error> {
    Option::<OhlcBodyColor>::deserialize(deserializer)
}

/// Atomic current public OHLC body property and colorer binding.
/// Absent property, present null and a concrete color are distinct states.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OhlcBodyStyle {
    /// Whether the compiled public body property exists, including present null.
    pub static_color_present: bool,
    /// Required nullable property value; absent properties must carry `None`.
    pub static_color: Option<OhlcBodyColor>,
    /// Whether body paint uses its separately declared runtime color lane.
    pub runtime_colorer: bool,
}

impl<'de> Deserialize<'de> for OhlcBodyStyle {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields, rename_all = "camelCase")]
        struct Wire {
            static_color_present: bool,
            #[serde(deserialize_with = "deserialize_required_ohlc_color")]
            static_color: Option<OhlcBodyColor>,
            runtime_colorer: bool,
        }
        let value = Wire::deserialize(deserializer)?;
        if !value.static_color_present && value.static_color.is_some() {
            return Err(serde::de::Error::custom(
                "absent OHLC body property cannot carry a color",
            ));
        }
        Ok(Self {
            static_color_present: value.static_color_present,
            static_color: value.static_color,
            runtime_colorer: value.runtime_colorer,
        })
    }
}

/// Structured presentation metadata shared by numeric plots and color-only
/// chart visuals. Runtime color expressions are carried separately as named
/// [`PlotColorRole`] channels so series colors may change from bar to bar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DisplayStyle {
    /// Declaration order shared by plots, horizontal levels, fills and visuals.
    /// Separate per-family slots cannot express `explicit_plot_zorder`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_order: Option<u32>,
    /// Display primitive that consumes this style record.
    pub kind: DisplayKind,
    /// Numeric-plot rendering mode, when [`DisplayStyle::kind`] is a plot.
    pub plot_style: PlotStyleKind,
    /// Stroke pattern for line-like outputs.
    pub line_style: PlotLineStyle,
    /// Stroke-width value retained for host rendering.
    pub line_width: u32,
    /// Requested chart destinations, such as the pane or price scale.
    pub display: PlotDisplay,
    /// Baseline used by histogram and column plots.
    pub histogram_base: f64,
    /// Horizontal bar displacement requested by the script.
    pub offset: i32,
    /// Maximum number of most-recent bars to display, or all bars when absent.
    pub show_last: Option<u32>,
    /// Whether a pane script requests this output on the main price chart.
    pub force_overlay: bool,
    /// Whether the output's latest value should track on the price scale.
    pub track_price: bool,
    /// Whether adjacent marker points should be joined.
    pub join: bool,
    /// Continue the last fill across missing plot values. Defaults to false.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub fill_gaps: bool,
    /// Fill paint registration mode. Absent on non-fill outputs and old programs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill_color_mode: Option<FillColorMode>,
    /// Ordinary-plot paint registration mode. Old programs leave this absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plot_color_mode: Option<PlotColorMode>,
    /// Per-direction static styles, independent of per-observation fill colors.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arrow_static_colors: Option<ArrowStaticColors>,
    /// Public runtime role bindings; absent on non-arrow and older outputs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arrow_runtime_colorers: Option<ArrowRuntimeColorers>,
    /// Compiled public OHLC body property and independent paint binding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ohlc_body_style: Option<OhlcBodyStyle>,
    /// Whether a host settings UI may edit this output's style.
    pub editable: bool,
    /// Automatic color selectors are separate from width/visibility controls.
    /// Runtime color calculations suppress selectors across the script.
    #[serde(skip_serializing_if = "color_selectors_enabled")]
    pub color_editable: bool,
    /// All statically discoverable color choices; never inferred from samples.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub color_palette: Vec<StyleColorPaletteEntry>,
    /// Optional script-requested numeric display format token.
    pub format: Option<String>,
    /// Optional number of decimal places used to display values.
    pub precision: Option<u8>,
    /// Shared identifier for the lanes of a grouped OHLC output.
    pub group: Option<u32>,
    /// OHLC lane represented by this output when it belongs to a group.
    pub ohlc_field: Option<OhlcField>,
    /// Shape token for marker outputs.
    pub marker_shape: Option<MarkerShape>,
    /// Placement token for marker outputs.
    pub marker_location: Option<MarkerLocation>,
    /// Size token for marker outputs.
    pub marker_size: Option<MarkerSize>,
    /// First literal glyph of a `plotchar` output, including an explicit presentation selector.
    pub character: Option<String>,
    /// Text displayed with a marker output.
    pub text: Option<String>,
    /// Lower bound for `plotarrow` marker height.
    pub min_height: Option<u32>,
    /// Upper bound for `plotarrow` marker height.
    pub max_height: Option<u32>,
    /// Requested options a portable host cannot reproduce exactly.
    pub unsupported: Vec<UnsupportedVisualOption>,
    /// Style scalars the host must resolve from the run's inputs before
    /// creating the series. Empty for the overwhelmingly common case of an
    /// all-literal style, and skipped on the wire so those programs serialize
    /// exactly as they did before this field existed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bound_scalars: Vec<BoundStyleScalar>,
    /// Compiler-owned references, absent from resolved runtime snapshots.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub runtime_scalars: Vec<RuntimeStyleScalar>,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // Serde skip predicates borrow their field.
fn color_selectors_enabled(enabled: &bool) -> bool {
    *enabled
}

impl Default for DisplayStyle {
    fn default() -> Self {
        DisplayStyle {
            source_order: None,
            kind: DisplayKind::Plot,
            plot_style: PlotStyleKind::Line,
            line_style: PlotLineStyle::Solid,
            line_width: 1,
            display: PlotDisplay::All,
            histogram_base: 0.0,
            offset: 0,
            show_last: None,
            force_overlay: false,
            track_price: false,
            join: false,
            fill_gaps: false,
            fill_color_mode: None,
            plot_color_mode: None,
            arrow_static_colors: None,
            arrow_runtime_colorers: None,
            ohlc_body_style: None,
            editable: true,
            color_editable: true,
            color_palette: Vec::new(),
            format: None,
            precision: None,
            group: None,
            ohlc_field: None,
            marker_shape: None,
            marker_location: None,
            marker_size: None,
            character: None,
            text: None,
            min_height: None,
            max_height: None,
            unsupported: Vec::new(),
            bound_scalars: Vec::new(),
            runtime_scalars: Vec::new(),
        }
    }
}

/// Severity level of a `log.*` call. Serialized (lowercased) into the run
/// outcome's `logs` stream and carried on the CSIL `Log` op as a stable tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Info,
    Warning,
    Error,
}

impl LogLevel {
    /// The `log.<member>` name this level is invoked as.
    pub fn member(self) -> &'static str {
        match self {
            LogLevel::Info => "info",
            LogLevel::Warning => "warning",
            LogLevel::Error => "error",
        }
    }

    /// A stable single-token tag for the textual CSIL form. Inverse of
    /// [`LogLevel::from_tag`].
    pub fn text_tag(self) -> &'static str {
        self.member()
    }

    /// Parse a [`LogLevel::text_tag`].
    pub fn from_tag(tag: &str) -> Option<LogLevel> {
        Some(match tag {
            "info" => LogLevel::Info,
            "warning" => LogLevel::Warning,
            "error" => LogLevel::Error,
            _ => return None,
        })
    }

    /// A stable integer tag for compact carriage on the CSIL op.
    pub fn to_u8(self) -> u8 {
        match self {
            LogLevel::Info => 0,
            LogLevel::Warning => 1,
            LogLevel::Error => 2,
        }
    }

    /// Decode [`LogLevel::to_u8`].
    pub fn from_u8(v: u8) -> Option<LogLevel> {
        Some(match v {
            0 => LogLevel::Info,
            1 => LogLevel::Warning,
            2 => LogLevel::Error,
            _ => return None,
        })
    }
}

/// A `request.*` external-data interface. Each variant is one of
/// the scalar `request.*` functions whose data source is external
/// (dividends, splits, earnings, fundamentals, macro series, fx rates). This
/// network-free VM cannot fetch that data, so these are modeled as INTERFACES:
/// the parameter names, arity, and scalar typing are modeled while the
/// VM reads the value from an OPTIONAL per-run host table keyed by the request's selector
/// (`MarketData::request_data`), defaulting to `na` when the host supplies
/// nothing. Carried as a stable tag on the CSIL `ReadRequest` op. Documented in
/// docs/ir.md and docs/runtime.md. The language surface is real; data
/// sourcing is a host concern. Parameter names and arity are regression-tested
/// by the semantic analyzer. Selectors are constant except for chart-symbol
/// references in corporate-action and financial requests, which the VM binds
/// to the run's market metadata. Other dynamic identifiers are rejected, and
/// trailing feed controls are not executed, so these interfaces are only
/// partially implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestFunc {
    /// A host-provided result for an external-symbol compound
    /// `request.security()` expression. The selector includes the symbol,
    /// timeframe, merge modes, and a canonical expression description.
    SecurityExpression,
    /// `request.dividends`.
    Dividends,
    /// `request.splits`.
    Splits,
    /// `request.earnings`.
    Earnings,
    /// `request.financial`.
    Financial,
    /// `request.economic`.
    Economic,
    /// `request.currency_rate` — an fx
    /// conversion rate.
    CurrencyRate,
    /// `request.seed` — a value published to a
    /// user seed data source. The `source`/`symbol` identifying strings form
    /// the selector; the trailing `expression` is accepted and analyzed but the
    /// value comes from the optional host table (host-or-default `na`). No real
    /// seed feed is involved — data sourcing is a host concern.
    Seed,
    /// `request.quandl` — **deprecated**. Registered as an accepted stub so
    /// scripts referencing it compile; it always evaluates to `na`.
    /// No host table is consulted.
    Quandl,
}

impl RequestFunc {
    /// The `request.<member>` name this interface is invoked as.
    pub fn member(self) -> &'static str {
        match self {
            RequestFunc::SecurityExpression => "security_expression",
            RequestFunc::Dividends => "dividends",
            RequestFunc::Splits => "splits",
            RequestFunc::Earnings => "earnings",
            RequestFunc::Financial => "financial",
            RequestFunc::Economic => "economic",
            RequestFunc::CurrencyRate => "currency_rate",
            RequestFunc::Seed => "seed",
            RequestFunc::Quandl => "quandl",
        }
    }

    /// Parameter names in call argument order.
    ///
    /// The request analyzer and generated editor metadata consume this same
    /// table. `SecurityExpression` is compiler-internal and has no public
    /// call surface.
    pub fn param_names(self) -> &'static [&'static str] {
        match self {
            RequestFunc::SecurityExpression => &[],
            RequestFunc::Dividends | RequestFunc::Earnings => &[
                "ticker",
                "field",
                "gaps",
                "lookahead",
                "ignore_invalid_symbol",
                "currency",
            ],
            RequestFunc::Splits => &[
                "ticker",
                "field",
                "gaps",
                "lookahead",
                "ignore_invalid_symbol",
            ],
            RequestFunc::Financial => &[
                "symbol",
                "financial_id",
                "period",
                "gaps",
                "ignore_invalid_symbol",
                "currency",
            ],
            RequestFunc::Economic => &["country_code", "field", "gaps", "ignore_invalid_symbol"],
            RequestFunc::CurrencyRate => &["from", "to", "ignore_invalid_currency"],
            RequestFunc::Seed => &[
                "source",
                "symbol",
                "expression",
                "ignore_invalid_symbol",
                "calc_bars_count",
            ],
            RequestFunc::Quandl => &["ticker", "gaps", "index", "ignore_invalid_symbol"],
        }
    }

    /// Number of leading required parameters in [`Self::param_names`].
    pub fn required_count(self) -> usize {
        match self {
            RequestFunc::SecurityExpression => 0,
            RequestFunc::Dividends | RequestFunc::Earnings => 1,
            RequestFunc::Splits | RequestFunc::Economic | RequestFunc::CurrencyRate => 2,
            RequestFunc::Financial | RequestFunc::Seed | RequestFunc::Quandl => 3,
        }
    }
}

/// A pivot-point calculation system for `ta.pivot_point_levels(type, anchor)`.
/// Each variant computes the 11-slot pivot array
/// `[P, R1, S1, R2, S2, R3, S3, R4, S4, R5, S5]` from the PRIOR completed
/// anchor period's High/Low/Close; a level a system does not define is `na`
/// (Fibonacci omits R4/S4/R5/S5). Carried as a stable tag on the CSIL
/// `ReadPivotLevels` op. The formulas below are PineWASM's definitions;
/// the 11-slot order is `[P, R1, S1, R2, S2, R3, S3, R4, S4, R5, S5]`.
/// All six built-in systems have formula support. The caller
/// supplies prior-period OHLC and the current-period open (Woodie needs both).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PivotSystem {
    /// `"Traditional"`: `P=(H+L+C)/3`, `R1=2P−L`, `S1=2P−H`, `R2=P+(H−L)`,
    /// `S2=P−(H−L)`, `R3=2P+(H−2L)`, `S3=2P−(2H−L)`, and the R4/S4/R5/S5
    /// extensions. All 11 slots are defined.
    Traditional,
    /// `"Fibonacci"`: `P=(H+L+C)/3`, `R1/S1=P±0.382(H−L)`,
    /// `R2/S2=P±0.618(H−L)`, `R3/S3=P±(H−L)`; R4/S4/R5/S5 are `na`.
    Fibonacci,
    /// `"Woodie"` uses the current anchor period open and prior high/low.
    Woodie,
    /// `"Classic"` uses the prior period HLC and omits fifth levels.
    Classic,
    /// `"DM"` uses prior period OHLC and defines only P/R1/S1.
    DeMark,
    /// `"Camarilla"` uses prior period HLC and defines through R5/S5.
    Camarilla,
}

impl PivotSystem {
    /// Number of `float` slots the pivot array always carries
    /// (`[P, R1, S1, R2, S2, R3, S3, R4, S4, R5, S5]`). Systems that omit a
    /// level fill it with `na`, so every system returns the same length.
    pub const LEVEL_COUNT: usize = 11;

    /// The `type` string literal this system is selected by (names are
    /// capitalized).
    pub fn type_name(self) -> &'static str {
        match self {
            PivotSystem::Traditional => "Traditional",
            PivotSystem::Fibonacci => "Fibonacci",
            PivotSystem::Woodie => "Woodie",
            PivotSystem::Classic => "Classic",
            PivotSystem::DeMark => "DM",
            PivotSystem::Camarilla => "Camarilla",
        }
    }

    /// Match a `type` string literal to a supported system, or `None` for an
    /// unrecognized / not-yet-implemented one (the analyzer rejects those).
    pub fn from_type_name(name: &str) -> Option<PivotSystem> {
        match name {
            "Traditional" => Some(PivotSystem::Traditional),
            "Fibonacci" => Some(PivotSystem::Fibonacci),
            "Woodie" => Some(PivotSystem::Woodie),
            "Classic" => Some(PivotSystem::Classic),
            "DM" => Some(PivotSystem::DeMark),
            "Camarilla" => Some(PivotSystem::Camarilla),
            _ => None,
        }
    }

    /// Compute the 11-slot pivot-level array from the prior completed anchor
    /// period's High/Low/Close. Returns finite values for the defined levels
    /// and `f64::NAN` for the levels a system does not define. Pure and
    /// deterministic; never panics.
    pub fn levels(
        self,
        open: f64,
        high: f64,
        low: f64,
        close: f64,
        current_open: f64,
    ) -> [f64; Self::LEVEL_COUNT] {
        let p = (high + low + close) / 3.0;
        let range = high - low;
        let nan = f64::NAN;
        match self {
            // Order: [P, R1, S1, R2, S2, R3, S3, R4, S4, R5, S5].
            PivotSystem::Traditional => [
                p,
                p * 2.0 - low,                // R1
                p * 2.0 - high,               // S1
                p + range,                    // R2
                p - range,                    // S2
                p * 2.0 + (high - 2.0 * low), // R3
                p * 2.0 - (2.0 * high - low), // S3
                p * 3.0 + (high - 3.0 * low), // R4
                p * 3.0 - (3.0 * high - low), // S4
                p * 4.0 + (high - 4.0 * low), // R5
                p * 4.0 - (4.0 * high - low), // S5
            ],
            PivotSystem::Fibonacci => [
                p,
                p + 0.382 * range, // R1
                p - 0.382 * range, // S1
                p + 0.618 * range, // R2
                p - 0.618 * range, // S2
                p + range,         // R3
                p - range,         // S3
                nan,               // R4 (undefined)
                nan,               // S4
                nan,               // R5
                nan,               // S5
            ],
            PivotSystem::Woodie => {
                let p = (high + low + 2.0 * current_open) / 4.0;
                let r1 = 2.0 * p - low;
                let s1 = 2.0 * p - high;
                let r2 = p + range;
                let s2 = p - range;
                let r3 = high + 2.0 * (p - low);
                let s3 = low - 2.0 * (high - p);
                [p, r1, s1, r2, s2, r3, s3, r3 + range, s3 - range, nan, nan]
            }
            PivotSystem::Classic => [
                p,
                p * 2.0 - low,
                p * 2.0 - high,
                p + range,
                p - range,
                p + 2.0 * range,
                p - 2.0 * range,
                p + 3.0 * range,
                p - 3.0 * range,
                nan,
                nan,
            ],
            PivotSystem::DeMark => {
                let x = if open == close {
                    high + low + 2.0 * close
                } else if close > open {
                    2.0 * high + low + close
                } else {
                    2.0 * low + high + close
                };
                [
                    x / 4.0,
                    x / 2.0 - low,
                    x / 2.0 - high,
                    nan,
                    nan,
                    nan,
                    nan,
                    nan,
                    nan,
                    nan,
                    nan,
                ]
            }
            PivotSystem::Camarilla => {
                let d = 1.1 * range;
                let r5 = if low == 0.0 { nan } else { high / low * close };
                [
                    p,
                    close + d / 12.0,
                    close - d / 12.0,
                    close + d / 6.0,
                    close - d / 6.0,
                    close + d / 4.0,
                    close - d / 4.0,
                    close + d / 2.0,
                    close - d / 2.0,
                    r5,
                    close - (r5 - close),
                ]
            }
        }
    }
}

/// A strategy order primitive. Each variant is one `strategy.*` order
/// call; the broker emulator turns the placed orders into fills. Carried as a
/// stable tag on the CSIL `Order` op. Documented in docs/ir.md and
/// docs/runtime.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderKind {
    /// `strategy.entry` — a pyramiding-aware entry; reversing closes the
    /// opposite position first.
    Entry,
    /// `strategy.order` — a basic order that ignores pyramiding.
    Order,
    /// `strategy.exit` — a take-profit / stop-loss / trailing exit bracket.
    Exit,
    /// `strategy.close` — a market order that closes the named entry.
    Close,
    /// `strategy.close_all` — market-closes the whole position.
    CloseAll,
    /// `strategy.cancel(id)` — cancel pending orders for `id`.
    Cancel,
    /// `strategy.cancel_all()` — cancel all pending orders.
    CancelAll,
    /// `strategy.risk.max_position_size(contracts)` — cap the position size the
    /// broker will open. Risk directives are hoisted into the
    /// unconditional MIR/CSIL prelude, so their source control-flow location
    /// cannot deactivate them. The excess of an oversized entry is dropped.
    /// Modeled in the bar-by-bar broker.
    RiskMaxPositionSize,
    /// `strategy.risk.allow_entry_in(value)` — restrict which direction new
    /// entries may open (`strategy.direction.long` / `.short` / `.all`).
    /// Registered unconditionally and **enforced** in the broker (a
    /// blocked-direction entry is dropped).
    RiskAllowEntryIn,
    /// `strategy.risk.max_drawdown(value, type, alert_message?)` — halt trading
    /// once the strategy's drawdown reaches `value`, expressed as either
    /// `strategy.percent_of_equity` or `strategy.cash`. Registered
    /// unconditionally and enforced by the broker.
    RiskMaxDrawdown,
    /// `strategy.risk.max_intraday_loss(value, type, alert_message?)` — cap the
    /// loss within one risk session. Registered unconditionally and enforced
    /// by the broker.
    RiskMaxIntradayLoss,
    /// `strategy.risk.max_intraday_filled_orders(count, alert_message?)` — cap
    /// filled orders per risk session. Registered unconditionally and enforced
    /// by the broker.
    RiskMaxIntradayFilledOrders,
    /// `strategy.risk.max_cons_loss_days(count, alert_message?)` — halt after
    /// `count` consecutive losing risk sessions. Registered unconditionally
    /// and enforced by the broker.
    RiskMaxConsLossDays,
}

impl OrderKind {
    /// Whether this command is one of Pine's globally-active
    /// `strategy.risk.*` directives.
    ///
    /// Risk directives apply regardless of their source location and cannot
    /// be disabled by runtime control flow. MIR
    /// lowering uses this classification to move them into the unconditional
    /// per-execution prelude while leaving ordinary order commands in place.
    pub fn is_risk(self) -> bool {
        matches!(
            self,
            OrderKind::RiskMaxPositionSize
                | OrderKind::RiskAllowEntryIn
                | OrderKind::RiskMaxDrawdown
                | OrderKind::RiskMaxIntradayLoss
                | OrderKind::RiskMaxIntradayFilledOrders
                | OrderKind::RiskMaxConsLossDays
        )
    }

    /// The `strategy.<member>` name this primitive is invoked as.
    pub fn member(self) -> &'static str {
        match self {
            OrderKind::Entry => "entry",
            OrderKind::Order => "order",
            OrderKind::Exit => "exit",
            OrderKind::Close => "close",
            OrderKind::CloseAll => "close_all",
            OrderKind::Cancel => "cancel",
            OrderKind::CancelAll => "cancel_all",
            OrderKind::RiskMaxPositionSize => "risk.max_position_size",
            OrderKind::RiskAllowEntryIn => "risk.allow_entry_in",
            OrderKind::RiskMaxDrawdown => "risk.max_drawdown",
            OrderKind::RiskMaxIntradayLoss => "risk.max_intraday_loss",
            OrderKind::RiskMaxIntradayFilledOrders => "risk.max_intraday_filled_orders",
            OrderKind::RiskMaxConsLossDays => "risk.max_cons_loss_days",
        }
    }

    /// Parameter names in call argument order.
    ///
    /// Semantic binding and every IR layer use this one table. Keeping the
    /// layout beside the stable [`OrderKind`] tag prevents a new optional
    /// parameter from silently shifting operands between the compiler and VM.
    pub fn param_names(self) -> &'static [&'static str] {
        match self {
            OrderKind::Entry | OrderKind::Order => &[
                "id",
                "direction",
                "qty",
                "limit",
                "stop",
                "oca_name",
                "oca_type",
                "comment",
                "alert_message",
                "disable_alert",
            ],
            OrderKind::Exit => &[
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
                "alert_message",
                "alert_profit",
                "alert_loss",
                "alert_trailing",
                "disable_alert",
            ],
            OrderKind::Close => &[
                "id",
                "comment",
                "qty",
                "qty_percent",
                "alert_message",
                "immediately",
                "disable_alert",
            ],
            OrderKind::CloseAll => &["comment", "alert_message", "immediately", "disable_alert"],
            OrderKind::Cancel => &["id"],
            OrderKind::CancelAll => &[],
            OrderKind::RiskMaxPositionSize => &["contracts"],
            OrderKind::RiskAllowEntryIn => &["value"],
            OrderKind::RiskMaxDrawdown | OrderKind::RiskMaxIntradayLoss => {
                &["value", "type", "alert_message"]
            }
            OrderKind::RiskMaxIntradayFilledOrders | OrderKind::RiskMaxConsLossDays => {
                &["count", "alert_message"]
            }
        }
    }

    /// Number of leading required parameters in [`OrderKind::param_names`].
    pub fn required_count(self) -> usize {
        match self {
            OrderKind::Entry | OrderKind::Order => 2,
            OrderKind::Exit
            | OrderKind::Close
            | OrderKind::Cancel
            | OrderKind::RiskMaxPositionSize
            | OrderKind::RiskAllowEntryIn
            | OrderKind::RiskMaxIntradayFilledOrders
            | OrderKind::RiskMaxConsLossDays => 1,
            OrderKind::RiskMaxDrawdown | OrderKind::RiskMaxIntradayLoss => 2,
            OrderKind::CloseAll | OrderKind::CancelAll => 0,
        }
    }

    /// The fixed number of operand slots this primitive carries through the
    /// pipeline (see the slot layouts in the semantics analyzer / `docs/ir.md`).
    /// The CSIL `Order` op pops exactly this many operands.
    pub fn operand_count(self) -> u8 {
        self.param_names().len() as u8
    }

    /// A stable integer tag for compact carriage on the CSIL op.
    pub fn to_u8(self) -> u8 {
        match self {
            OrderKind::Entry => 0,
            OrderKind::Order => 1,
            OrderKind::Exit => 2,
            OrderKind::Close => 3,
            OrderKind::CloseAll => 4,
            OrderKind::Cancel => 5,
            OrderKind::CancelAll => 6,
            OrderKind::RiskMaxPositionSize => 7,
            OrderKind::RiskAllowEntryIn => 8,
            OrderKind::RiskMaxDrawdown => 9,
            OrderKind::RiskMaxIntradayLoss => 10,
            OrderKind::RiskMaxIntradayFilledOrders => 11,
            OrderKind::RiskMaxConsLossDays => 12,
        }
    }

    /// Decode [`OrderKind::to_u8`].
    pub fn from_u8(v: u8) -> Option<OrderKind> {
        Some(match v {
            0 => OrderKind::Entry,
            1 => OrderKind::Order,
            2 => OrderKind::Exit,
            3 => OrderKind::Close,
            4 => OrderKind::CloseAll,
            5 => OrderKind::Cancel,
            6 => OrderKind::CancelAll,
            7 => OrderKind::RiskMaxPositionSize,
            8 => OrderKind::RiskAllowEntryIn,
            9 => OrderKind::RiskMaxDrawdown,
            10 => OrderKind::RiskMaxIntradayLoss,
            11 => OrderKind::RiskMaxIntradayFilledOrders,
            12 => OrderKind::RiskMaxConsLossDays,
            _ => return None,
        })
    }
}

/// A resolved user variable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Symbol {
    pub id: SymbolId,
    pub name: String,
    pub ty: Type,
    pub qualifier: Qualifier,
    pub slot: SeriesSlot,
    /// True for `var` and `varip` declarations: the slot carries the previous
    /// bar's final value forward at the start of each bar, and the initializer
    /// runs only the first time the declaration statement executes.
    pub persistent: bool,
    /// True only for `varip` declarations. A `varip` slot is `persistent` too
    /// (carry-forward + run-once init), but additionally it does NOT
    /// participate in realtime-tick rollback: its current cell survives the
    /// snapshot/restore around each tick (see docs/runtime.md). For
    /// historical execution `varip` and `var` behave identically.
    pub varip: bool,
    /// True for compiler-generated hidden symbols (names like `%h0`) that
    /// back [`HirExprKind::MaterializeHistory`] sites. Synthetic symbols
    /// own a series slot and appear in slot tables and dumps, but they
    /// never participate in scope lookup. History temporaries are not
    /// persistent; hidden `once` activation flags are persistent and roll back
    /// with ordinary `var` state.
    pub synthetic: bool,
    /// The compile-time integer value this symbol is known to carry: an
    /// inlined function parameter bound to an int-literal argument (directly,
    /// or forwarded through another literal-bound parameter). Positions that
    /// require an int literal (`ta.*` engine lengths) look through such
    /// symbols, so a library helper can forward its `simple int` length into
    /// an engine call. `None` for every other symbol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub const_int: Option<i64>,
    pub decl_span: Span,
}

/// The per-kind live-drawing GC caps declared by `indicator(max_*_count=)`.
/// Each `Some(n)` overrides the VM's default oldest-first eviction
/// limit for that drawing kind; `None` keeps the default. Carried through
/// MIR/CSIL metadata to the VM (which has no `IndicatorInfo`), so it is a small
/// standalone type rather than the whole `IndicatorInfo`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrawingCaps {
    #[serde(default)]
    pub max_lines_count: Option<u32>,
    #[serde(default)]
    pub max_labels_count: Option<u32>,
    #[serde(default)]
    pub max_boxes_count: Option<u32>,
    #[serde(default)]
    pub max_polylines_count: Option<u32>,
}

impl DrawingCaps {
    /// Whether any cap is set (so the metadata can be skipped when empty).
    pub fn is_empty(&self) -> bool {
        *self == DrawingCaps::default()
    }
}

/// Metadata from the `indicator(...)` declaration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndicatorInfo {
    pub title: String,
    /// Const declaration presentation options, preserved independently of broker settings.
    #[serde(default)]
    pub shorttitle: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub precision: Option<u8>,
    #[serde(default)]
    pub scale: Option<String>,
    #[serde(default)]
    pub explicit_plot_zorder: bool,
    #[serde(default = "input_active_default_true")]
    pub behind_chart: bool,
    /// The declared `max_bars_back=N` argument, when present: a request for a
    /// history ceiling of `N` bars. `None` means the script did not declare
    /// one. The effective ceiling, which also counts top-level
    /// `max_bars_back(...)` calls, literal history offsets and the compiler's
    /// default, is [`HirProgram::max_bars_back`]; that value bounds history
    /// offsets computed at run time and is part of the observable-depth
    /// calculation used to size the VM and AOT user-slot history rings (see
    /// `docs/runtime.md`).
    #[serde(default)]
    pub max_bars_back: Option<u32>,
    /// `max_lines_count` / `max_labels_count` / `max_boxes_count` /
    /// `max_polylines_count` — the per-kind live-drawing GC caps. When
    /// present, each **overrides** the VM's default oldest-first eviction limit
    /// for that drawing kind (see `DrawingRegistry`); `None` keeps the default.
    /// Const int in `1..=500` (`E0219` otherwise). `max_boxes_count` maps to the
    /// `box.*` registry cap.
    #[serde(default)]
    pub max_lines_count: Option<u32>,
    #[serde(default)]
    pub max_labels_count: Option<u32>,
    #[serde(default)]
    pub max_boxes_count: Option<u32>,
    #[serde(default)]
    pub max_polylines_count: Option<u32>,
    /// Wire/API compatibility field, emitted as false. The `indicator`
    /// declaration has no `calc_on_every_tick` parameter; indicators
    /// execute on realtime updates supplied by the host. Strategies expose
    /// their own setting in `StrategyInfo`.
    #[serde(default)]
    pub calc_on_every_tick: bool,
    /// `overlay` — whether the script draws on the price pane (`true`) or in a
    /// separate pane (`false`). Const-folded metadata carried
    /// through to `ProgramMetadata.overlay` for hosts; it has no effect on
    /// computed values, only on presentation (a host chart pane choice).
    #[serde(default)]
    pub overlay: bool,
    pub span: Span,
}

impl IndicatorInfo {
    /// The drawing GC caps this declaration sets, extracted for the
    /// runtime metadata.
    pub fn drawing_caps(&self) -> DrawingCaps {
        DrawingCaps {
            max_lines_count: self.max_lines_count,
            max_labels_count: self.max_labels_count,
            max_boxes_count: self.max_boxes_count,
            max_polylines_count: self.max_polylines_count,
        }
    }
}

/// The `strategy()` declaration parameters, in PineWASM binding order.
///
/// Keep this list in the order PineWASM binds `strategy()` arguments.
/// Semantic binding, regression tests, editor documentation, and the metadata
/// tables all key off this list so a newly added trailing parameter cannot
/// silently shift calls. The first 33 parameters accept
/// positional binding; the trailing history-tick parameter is named-only.
pub const STRATEGY_DECL_PARAMS: [&str; 34] = [
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
    "calc_on_every_history_tick",
];

fn default_strategy_format() -> String {
    "format.inherit".to_string()
}

fn default_strategy_pyramiding() -> u32 {
    1
}

fn default_strategy_initial_capital() -> f64 {
    1_000_000.0
}

fn default_strategy_qty_type() -> String {
    "fixed".to_string()
}

fn default_strategy_qty_value() -> f64 {
    1.0
}

fn default_strategy_commission_type() -> String {
    "percent".to_string()
}

fn default_strategy_currency() -> String {
    "NONE".to_string()
}

fn default_strategy_close_entries_rule() -> String {
    "FIFO".to_string()
}

fn default_strategy_margin() -> f64 {
    100.0
}

fn default_strategy_drawing_count() -> Option<u32> {
    Some(50)
}

fn default_strategy_risk_free_rate() -> f64 {
    2.0
}

fn default_true() -> bool {
    true
}

/// Metadata from the `strategy(...)` declaration. Accepted `strategy()`
/// parameters are const-folded here. Parameters
/// that require chart-host facilities (lower-timeframe bar magnifier data,
/// synthetic-chart standard OHLC, and realtime tick scheduling) remain
/// explicit metadata rather than being silently discarded; the runtime
/// capability report documents whether the supplied feed can realize them.
///
/// A script that declares `strategy(...)` is a *strategy* script and may not
/// also declare `indicator(...)` (`E0240`); the two are mutually exclusive
/// top-level declarations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrategyInfo {
    /// Required first argument: the strategy's display title.
    pub title: String,
    /// Compact display title. An empty value in an older serialized artifact
    /// means "use `title`"; newly compiled programs always store the resolved
    /// value.
    #[serde(default)]
    pub shorttitle: String,
    /// Whether visuals belong on the price pane rather than a separate pane.
    #[serde(default)]
    pub overlay: bool,
    /// Display number format token (`format.inherit`, `format.price`, ...).
    #[serde(default = "default_strategy_format")]
    pub format: String,
    /// Explicit display precision. `None` inherits the chart symbol precision.
    #[serde(default)]
    pub precision: Option<u32>,
    /// Price-scale token. `None` lets the chart host select the pane's scale.
    #[serde(default)]
    pub scale: Option<String>,
    /// Explicit history-buffer floor; `None`/zero requests auto detection.
    #[serde(default)]
    pub max_bars_back: Option<u32>,
    /// `initial_capital` — starting account equity. **Honored** by the
    /// emulator. Defaults to `1_000_000`.
    #[serde(default = "default_strategy_initial_capital")]
    pub initial_capital: f64,
    /// `default_qty_type` — how `default_qty_value` is interpreted:
    /// `"fixed"` (contracts), `"cash"` (currency), or `"percent_of_equity"`.
    /// **Honored** when an order omits an explicit quantity. Defaults to
    /// `"fixed"`.
    #[serde(default = "default_strategy_qty_type")]
    pub default_qty_type: String,
    /// `default_qty_value` — the default order size in the units named by
    /// `default_qty_type`. **Honored**. Defaults to `1.0`.
    #[serde(default = "default_strategy_qty_value")]
    pub default_qty_value: f64,
    /// `commission_type` — `"percent"`, `"cash_per_contract"`, or
    /// `"cash_per_order"`. **Honored** by the emulator. Defaults to
    /// `"percent"`.
    #[serde(default = "default_strategy_commission_type")]
    pub commission_type: String,
    /// `commission_value` — the commission amount in the units named by
    /// `commission_type`. **Honored**. Defaults to `0.0`.
    #[serde(default)]
    pub commission_value: f64,
    /// `slippage` — ticks of adverse slippage applied to market and stop fills.
    /// **Honored** by the emulator. Limit-only fills are not slipped. Defaults
    /// to `0`.
    #[serde(default)]
    pub slippage: u32,
    /// `pyramiding` — maximum number of concurrent same-direction entries.
    /// **Honored** (caps entries). Defaults to `1`; an explicitly supplied
    /// value of `0` also permits one entry, and N >= 2 permits N entries.
    #[serde(default = "default_strategy_pyramiding")]
    pub pyramiding: u32,
    /// Extra ticks by which price must exceed a historical limit before fill.
    #[serde(default)]
    pub backtest_fill_limits_assumption: u32,
    /// `process_orders_on_close` — when true, orders are eligible to fill on
    /// the same bar's close rather than waiting for the next bar's open.
    /// **Honored** by the emulator. Defaults to `false`.
    #[serde(default)]
    pub process_orders_on_close: bool,
    /// Exit allocation rule: `FIFO` (default) or `ANY`.
    #[serde(default = "default_strategy_close_entries_rule")]
    pub close_entries_rule: String,
    /// `calc_on_every_tick` — recalculate on each supplied realtime update.
    /// **Honored** by the emulator when realtime updates are present in the
    /// execution input. Defaults to `false`.
    #[serde(default)]
    pub calc_on_every_tick: bool,
    /// `calc_on_order_fills` — recalculate after broker fills in addition to
    /// the normal bar execution. **Honored** by the emulator. Defaults to
    /// `false`.
    #[serde(default)]
    pub calc_on_order_fills: bool,
    /// `currency` — account currency token (metadata only). Defaults to
    /// `"NONE"`.
    #[serde(default = "default_strategy_currency")]
    pub currency: String,
    /// `margin_long` — long margin percent. Defaults to `100.0`.
    #[serde(default = "default_strategy_margin")]
    pub margin_long: f64,
    /// `margin_short` — short margin percent. Defaults to `100.0`.
    #[serde(default = "default_strategy_margin")]
    pub margin_short: f64,
    /// Respect explicit plot call order when rendering visuals.
    #[serde(default)]
    pub explicit_plot_zorder: bool,
    /// Per-kind live drawing limits. Each defaults to approximately 50.
    #[serde(default = "default_strategy_drawing_count")]
    pub max_lines_count: Option<u32>,
    #[serde(default = "default_strategy_drawing_count")]
    pub max_labels_count: Option<u32>,
    #[serde(default = "default_strategy_drawing_count")]
    pub max_boxes_count: Option<u32>,
    #[serde(default = "default_strategy_drawing_count")]
    pub max_polylines_count: Option<u32>,
    /// Initial calculation window; zero means all available history.
    #[serde(default)]
    pub calc_bars_count: u32,
    /// Annual risk-free rate used by strategy performance ratios.
    #[serde(default = "default_strategy_risk_free_rate")]
    pub risk_free_rate: f64,
    /// Request lower-timeframe intrabars for historical fill simulation.
    #[serde(default)]
    pub use_bar_magnifier: bool,
    /// On synthetic charts, request host-supplied standard OHLC for fills.
    #[serde(default)]
    pub fill_orders_on_standard_ohlc: bool,
    /// Allow dynamic request.* arguments and local-scope request calls.
    #[serde(default = "default_true")]
    pub dynamic_requests: bool,
    /// Draw strategy visuals behind the chart's candles.
    #[serde(default = "default_true")]
    pub behind_chart: bool,
    /// Execute on every available historical tick. This is a host run setting,
    /// not a `strategy()` declaration parameter; PineWASM's host run override
    /// sets it.
    #[serde(default)]
    pub calc_on_every_history_tick: bool,
    /// The message from a `//@strategy_alert_message=...` compiler annotation,
    /// if present (metadata only; the default alert message for the strategy).
    /// Empty when the annotation is absent.
    pub alert_message: String,
    pub span: Span,
}

impl StrategyInfo {
    /// Resolve the display short title, including older serialized programs
    /// whose newly added `shorttitle` field deserializes as empty.
    pub fn resolved_shorttitle(&self) -> &str {
        if self.shorttitle.is_empty() {
            &self.title
        } else {
            &self.shorttitle
        }
    }

    /// The declaration's drawing GC caps, shared with indicator metadata.
    pub fn drawing_caps(&self) -> DrawingCaps {
        DrawingCaps {
            max_lines_count: self.max_lines_count,
            max_labels_count: self.max_labels_count,
            max_boxes_count: self.max_boxes_count,
            max_polylines_count: self.max_polylines_count,
        }
    }
}

/// The broker emulator's read surface over the declaration. Implemented here
/// (rather than the broker crate depending on the HIR) so `pinewasm_broker`
/// stays `no_std` and HIR-free for the AOT runtime blob, while the
/// interpreter and the emitter both configure brokers from this exact struct.
impl pinewasm_broker::StrategyProperties for StrategyInfo {
    fn initial_capital(&self) -> f64 {
        self.initial_capital
    }
    fn default_qty_type(&self) -> &str {
        &self.default_qty_type
    }
    fn default_qty_value(&self) -> f64 {
        self.default_qty_value
    }
    fn commission_type(&self) -> &str {
        &self.commission_type
    }
    fn commission_value(&self) -> f64 {
        self.commission_value
    }
    fn slippage(&self) -> u32 {
        self.slippage
    }
    fn pyramiding(&self) -> u32 {
        self.pyramiding
    }
    fn process_orders_on_close(&self) -> bool {
        self.process_orders_on_close
    }
    fn calc_on_every_tick(&self) -> bool {
        self.calc_on_every_tick
    }
    fn calc_on_order_fills(&self) -> bool {
        self.calc_on_order_fills
    }
    fn calc_on_every_history_tick(&self) -> bool {
        self.calc_on_every_history_tick
    }
    fn backtest_fill_limits_assumption(&self) -> u32 {
        self.backtest_fill_limits_assumption
    }
    fn close_entries_rule(&self) -> &str {
        &self.close_entries_rule
    }
    fn margin_long(&self) -> f64 {
        self.margin_long
    }
    fn margin_short(&self) -> f64 {
        self.margin_short
    }
    fn currency(&self) -> &str {
        &self.currency
    }
}

/// One data-bearing display call output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlotInfo {
    pub slot: PlotSlot,
    /// Optional display title (second argument of `plot`, when given).
    pub title: Option<String>,
    #[serde(default)]
    pub style: DisplayStyle,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub color_roles: Vec<PlotColorRole>,
    pub span: Span,
}

/// One color-only chart visual (`bgcolor` or `barcolor`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VisualInfo {
    pub slot: VisualSlot,
    pub title: Option<String>,
    #[serde(default)]
    pub style: DisplayStyle,
    pub color_roles: Vec<PlotColorRole>,
    pub span: Span,
}

/// One `fill(id1, id2, color, ...)` band between two plot outputs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FillInfo {
    pub slot: FillSlot,
    pub plot1: PlotSlot,
    pub plot2: PlotSlot,
    pub title: Option<String>,
    #[serde(default)]
    pub style: DisplayStyle,
    pub color_roles: Vec<PlotColorRole>,
    pub span: Span,
}

// ---------------------------------------------------------------------------
// Inputs (`input.*`)
// ---------------------------------------------------------------------------

/// The runtime kind of one `input.*` declaration. This durable classification
/// is carried from HIR through MIR, CSIL, and the VM: it fixes the value an
/// input read pushes and, for [`InputKind::Source`], the special "select a
/// builtin series" behavior. Host widget identity is deliberately separate in
/// [`InputWidget`], because several distinct Pine controls all return an
/// `input string`.
///
/// The `input.*` namespace is carried through a host-overridable manifest:
/// - `input.int` → [`InputKind::Int`], `input.float` → [`InputKind::Float`],
///   `input.bool` → [`InputKind::Bool`], `input.color` → [`InputKind::Color`];
/// - `input.string` / `input.text_area` / `input.timeframe` / `input.session`
///   / `input.symbol` all → [`InputKind::String`] (all are typed as
///   `input string`), while [`InputDecl::widget`] preserves their distinct
///   Settings controls;
/// - `input.source` → [`InputKind::Source`]: its compiler-authored default is
///   one of eight price [`BuiltinSeries`] values (normally `close`); a host can
///   select another builtin or attach another script's plot lane, and its
///   qualifier is `series`, not `input`.
///
/// - `input.time` and `input.price` preserve their integer/float defaults for
///   host controls (interactive chart placement is a host concern);
/// - `input.enum` preserves the declaring enum and allowed member indexes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InputKind {
    Int,
    Float,
    Bool,
    String,
    Color,
    /// `input.source(...)` — the runtime value is a numeric series sample at
    /// the current bar. The default names a [`BuiltinSeries`]; a
    /// host-only override can instead name an [`ExternalSourceRef`], whose
    /// samples are supplied separately to the VM for that run.
    Source,
    /// `input.price(defval, ...)` — a float input (`input float`). This
    /// headless VM has no chart interaction, so it is modeled like
    /// [`InputKind::Float`]: the `defval` (or a host override)
    /// with no min/max/step surface. Data sourcing (the click) is a host concern.
    Price,
    /// `input.time(defval, ...)` — a time value (epoch ms), `input int`.
    /// Modeled as an int input: the `defval` or a
    /// host override; chart interaction is a host concern.
    Time,
    /// `input.enum(defval, ...)` — an enum-typed input whose `defval` is an
    /// enum member. The runtime value is the member's variant index (an `int`,
    /// like any enum value); the read expression types as the declaring
    /// `Type::Enum(name)`. The name is not carried on the (`Copy`) kind — it
    /// lives in the [`InputValue::Enum`] `defval`, and the analyzer builds the
    /// read expression's type from there. A host override selects another
    /// member (by name + index).
    Enum,
}

/// The host-facing control requested by an `input.*` declaration.
///
/// This is orthogonal to [`InputKind`]: every string-family variant below has
/// the same Pine type, qualifier, runtime value, and VM override behavior. The
/// discriminator survives artifact serialization solely so Settings hosts do
/// not have to guess the intended widget from the scalar type.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputWidget {
    /// The ordinary control implied by a non-string [`InputKind`].
    #[default]
    Standard,
    /// `input.string()` and the generic `input()` string overload.
    String,
    /// `input.text_area()`.
    TextArea,
    /// `input.timeframe()`.
    Timeframe,
    /// `input.session()`.
    Session,
    /// `input.symbol()`.
    Symbol,
}

impl InputKind {
    /// Lowercase name used in diagnostics and host metadata.
    pub fn describe(self) -> &'static str {
        match self {
            InputKind::Int => "int",
            InputKind::Float => "float",
            InputKind::Bool => "bool",
            InputKind::String => "string",
            InputKind::Color => "color",
            InputKind::Source => "source",
            InputKind::Price => "price",
            InputKind::Time => "time",
            InputKind::Enum => "enum",
        }
    }
}

/// Stable host identity for a plot exposed by another script as an
/// `input.source` choice.
///
/// `script_id` and `plot_slot` are the durable identity. `plot_label` is the
/// human-readable label captured by the host for Settings; it is deliberately
/// not used to address the lane, so renaming a plot cannot silently retarget a
/// saved selection to a different slot.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalSourceRef {
    /// Stable host identity for the upstream script instance.
    pub script_id: String,
    /// Zero-based plot slot within the upstream script's result.
    pub plot_slot: u32,
    /// Presentation label captured for a settings interface.
    pub plot_label: String,
}

impl ExternalSourceRef {
    /// Host references require a non-empty stable script id. Empty labels are
    /// accepted because Pine permits an empty plot title; hosts can synthesize
    /// a `Plot N` display label without changing the underlying identity.
    pub fn is_valid(&self) -> bool {
        !self.script_id.trim().is_empty()
            && self.script_id.len() <= 256
            && self.plot_label.len() <= 1_024
    }

    /// Compare durable lane identity while treating the label as presentation
    /// metadata. This keeps persisted selections valid across plot renames.
    pub fn same_lane(&self, other: &Self) -> bool {
        self.script_id == other.script_id && self.plot_slot == other.plot_slot
    }
}

/// The compile-time default value of one input, and — for
/// [`InputKind::Source`] — the builtin series it selects. Serialized in the
/// program metadata so a host can render the declared defaults, and read by
/// the VM when no override is supplied.
///
/// [`InputValue::ExternalSource`] is host-only override metadata. The Pine
/// analyzer never emits it, and the CSIL validator rejects it in a declaration
/// default or constraint. Its values travel separately in a per-run external
/// source lane (see the VM's `RunOptions`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InputValue {
    Int(
        /// Exact signed integer used by an `input.int` or `input.time` value.
        #[serde(deserialize_with = "deserialize_i64_number_or_string")]
        i64,
    ),
    Float(
        /// Finite numeric value used by an `input.float` or `input.price` value.
        f64,
    ),
    Bool(
        /// Boolean value used by an `input.bool` value.
        bool,
    ),
    String(
        /// UTF-8 text used by an `input.string` value.
        String,
    ),
    /// `(r, g, b, t)` with `t` the transparency in `0..=100`.
    Color {
        /// Red channel in `0..=255`.
        r: u8,
        /// Green channel in `0..=255`.
        g: u8,
        /// Blue channel in `0..=255`.
        b: u8,
        /// Transparency percentage in `0..=100`.
        #[serde(serialize_with = "serialize_transparency")]
        t: f64,
    },
    /// The default/selected builtin series for an `input.source`.
    Source(
        /// Compiler-defined series selected by the input.
        BuiltinSeries,
    ),
    /// A host-selected numeric plot from another script. This may occur only
    /// in a per-run input override, never in compiler-authored metadata.
    ExternalSource(
        /// Host identity and plot slot of the selected external series.
        ExternalSourceRef,
    ),
    /// The default/selected enum member for an `input.enum`: the declaring
    /// enum's `name` and the member's `index` (its variant position, the value
    /// an enum member lowers to at runtime). `member` preserves the source
    /// identifier, while `title` preserves the optional declared display title
    /// (`None` means the host displays `member`). A host override supplies
    /// another member of the same enum.
    Enum {
        /// Declared enum type name.
        name: String,
        /// Zero-based position of the selected member in its enum declaration.
        index: u32,
        /// Source identifier of the selected member.
        #[serde(default)]
        member: String,
        /// Optional display title declared for the selected member.
        #[serde(default)]
        title: Option<String>,
    },
}

/// Compile-time-validated constraints on an input. Numeric inputs
/// (`input.int` / `input.float`) can use the optional bounds; `options`, when
/// non-empty, restricts numeric, string, or enum inputs to that set (and is
/// mutually exclusive with numeric bounds). Stored
/// in metadata so a host can render and enforce the same control surface. The
/// VM checks override types and intrinsic domains, but the embedding host is
/// responsible for enforcing min/max/step/options against an override.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct InputConstraints {
    /// `minval` (inclusive) for numeric inputs.
    pub min: Option<InputValue>,
    /// `maxval` (inclusive) for numeric inputs.
    pub max: Option<InputValue>,
    /// `step` for numeric inputs. Runtime override constraints are host-enforced
    /// as described on [`InputConstraints`].
    pub step: Option<InputValue>,
    /// The allowed value set (`options = [...]`); empty when unconstrained.
    /// Supported by numeric, string, and enum inputs.
    pub options: Vec<InputValue>,
}

impl InputConstraints {
    /// Whether the declaration has no numeric bounds, step, or allowed-value
    /// choices.
    pub fn is_empty(&self) -> bool {
        self.min.is_none() && self.max.is_none() && self.step.is_none() && self.options.is_empty()
    }
}

/// Where the input value is shown outside the Settings dialog: none, data
/// window, status line, or all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputDisplay {
    #[default]
    All,
    None,
    DataWindow,
    StatusLine,
}

/// A directly host-evaluable `active` dependency. Retained as a compact fast
/// path for the common `active = enabled` / `active = not enabled` forms; the
/// complete expression is also serialized in [`InputActive::expression`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InputActiveDirect {
    /// Index of the prior input declaration on which this input depends.
    pub input: u32,
    /// Whether the referenced input's boolean value is negated.
    #[serde(default)]
    pub inverted: bool,
}

/// Unary operations in a host-evaluable input `active` expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputActiveUnaryOp {
    Not,
    Neg,
    IntToFloat,
}

/// Binary operations in a host-evaluable input `active` expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputActiveBinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
}

/// Maximum number of nodes a host-reactive input expression may contain.
///
/// The manifest is also accepted from untrusted serialized artifacts, so this
/// is a protocol limit rather than merely a compiler implementation detail.
pub const MAX_INPUT_ACTIVE_EXPRESSION_NODES: usize = 256;

/// Maximum nesting depth of a host-reactive input expression.
pub const MAX_INPUT_ACTIVE_EXPRESSION_DEPTH: usize = 64;

/// Maximum arguments in one host-reactive pure call.
pub const MAX_INPUT_ACTIVE_CALL_ARGUMENTS: usize = 32;

/// Maximum UTF-8 size of a string materialized by a host-reactive expression.
pub const MAX_INPUT_ACTIVE_STRING_BYTES: usize = 64 * 1024;

/// Arity bounds for pure builtins with a bounded, cross-host-audited
/// [`InputActiveExpr::Call`] implementation.
///
/// This is intentionally much narrower than the VM's pure-call surface. Even
/// bounded implementations are excluded when Rust, JavaScript, and the VM can
/// disagree on parsing, Unicode, date, or floating-point edge cases. An
/// excluded call remains valid Pine; its host-reactive metadata simply falls
/// back to the compiler-provided default.
pub fn input_active_call_argc_bounds(function: &str) -> Option<(usize, usize)> {
    match function {
        "na" | "math.sign" => Some((1, 1)),
        "nz" => Some((1, 2)),
        "math.min" | "math.max" => Some((2, 10)),
        _ => None,
    }
}

/// Whether a pure builtin has a bounded, cross-host-audited implementation.
pub fn input_active_call_is_supported(function: &str) -> bool {
    input_active_call_argc_bounds(function).is_some()
}

/// Side-effect-free expression tree for the `active` input parameter.
///
/// It lets Settings hosts recompute enabled state from draft overrides
/// immediately, including arithmetic, comparisons, logical expressions,
/// aliases, and ternaries. A valid future expression shape that cannot be
/// represented remains accepted and falls back to [`InputActive::default`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputActiveExpr {
    /// Literal value embedded in the expression.
    Value {
        /// Literal in the input protocol's value domain.
        value: InputValue,
    },
    /// Pine's `na`. It has no [`InputValue`] because it is the absence of one:
    /// evaluators yield nothing for it, which leaves the consumer on its
    /// declared default. Carrying it explicitly is what lets a ternary like
    /// `useMirror ? width : na` be represented at all.
    Na,
    /// Read one prior input declaration by numeric id.
    Input {
        /// Index into the program's input declaration table.
        input: u32,
    },
    /// Apply one unary operation.
    Unary {
        /// Unary operation to evaluate.
        op: InputActiveUnaryOp,
        /// Expression supplying the operation's input.
        operand: Box<InputActiveExpr>,
    },
    /// Apply one binary operation.
    Binary {
        /// Binary operation to evaluate.
        op: InputActiveBinaryOp,
        /// Left-hand operand.
        lhs: Box<InputActiveExpr>,
        /// Right-hand operand.
        rhs: Box<InputActiveExpr>,
    },
    /// Select between two branches from a boolean condition.
    Ternary {
        /// Expression coerced to the branch condition.
        condition: Box<InputActiveExpr>,
        /// Value evaluated when the condition is true.
        when_true: Box<InputActiveExpr>,
        /// Value evaluated when the condition is false.
        when_false: Box<InputActiveExpr>,
    },
    /// A qualifier-preserving, side-effect-free builtin accepted by
    /// [`input_active_call_is_supported`]. The canonical Pine function name
    /// keeps the host protocol independent of Rust enum variant names.
    Call {
        /// Canonical Pine function name.
        function: String,
        /// Arguments in source order.
        args: Vec<InputActiveExpr>,
    },
}

/// Host-facing metadata for the `active` parameter shared by every input
/// function. `default` is the expression evaluated against declared input
/// defaults. `dependencies` contains every referenced prior input id.
/// `expression` preserves the complete supported scalar tree for reactive
/// Settings hosts; `direct` remains a backwards-compatible fast path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputActive {
    /// Enabled state produced from declaration defaults.
    #[serde(default = "input_active_default_true")]
    pub default: bool,
    /// Unique prior-input ids referenced anywhere in [`InputActive::expression`].
    #[serde(default)]
    pub dependencies: Vec<u32>,
    /// Compact representation for a direct boolean dependency, when available.
    #[serde(default)]
    pub direct: Option<InputActiveDirect>,
    /// Complete host-evaluable expression, or `None` for a constant default.
    #[serde(default)]
    pub expression: Option<InputActiveExpr>,
}

const fn input_active_default_true() -> bool {
    true
}

impl Default for InputActive {
    fn default() -> Self {
        Self {
            default: true,
            dependencies: Vec::new(),
            direct: None,
            expression: None,
        }
    }
}

/// One `input.*` declaration site. Allocated in syntactic order; the `id` is
/// its index into [`HirProgram::inputs`] and the operand of the input read.
///
/// The `title` doubles as the host-facing identifier when non-empty; the
/// override map is keyed by `id` (stable and unambiguous — titles need not be
/// unique). See docs/browser.md.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputDecl {
    /// Zero-based declaration id used by CSIL and host override maps.
    pub id: u32,
    /// Runtime value domain accepted by this input.
    pub kind: InputKind,
    /// Exact Settings control requested by the source declaration. This field
    /// defaults to `standard` when reading artifacts produced before widget
    /// identity was added.
    #[serde(default)]
    pub widget: InputWidget,
    /// The display title (the `title` argument), or `None` when omitted.
    pub title: Option<String>,
    /// Optional help text displayed with the input control.
    #[serde(default)]
    pub tooltip: Option<String>,
    /// Adjacent declarations with the same non-empty key share one row.
    #[serde(default)]
    pub inline: Option<String>,
    /// Optional settings-section heading.
    #[serde(default)]
    pub group: Option<String>,
    /// Whether the host should request interactive confirmation on first load.
    #[serde(default)]
    pub confirm: bool,
    /// Where the input value is displayed outside the Settings dialog.
    #[serde(default)]
    pub display: InputDisplay,
    /// Whether the Settings control is editable, including prior-input
    /// dependencies for host-side reactivity.
    #[serde(default)]
    pub active: InputActive,
    /// The declared default value.
    pub defval: InputValue,
    /// Compiler-validated bounds or allowed-value choices.
    pub constraints: InputConstraints,
    /// Source range of the input declaration call.
    pub span: Span,
}

// ---------------------------------------------------------------------------
// HIR nodes
// ---------------------------------------------------------------------------

/// Binary operators, re-declared here (rather than reusing the AST enum) so
/// HIR consumers do not depend on the surface syntax crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    /// `%` — remainder (modulo). Integer remainder follows the Rust rule
    /// (result takes the sign of the dividend).
    Rem,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::And => "and",
            BinOp::Or => "or",
        }
    }

    pub fn is_comparison(self) -> bool {
        matches!(
            self,
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::Eq | BinOp::Ne
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UnOp {
    Neg,
    Not,
}

impl UnOp {
    pub fn symbol(self) -> &'static str {
        match self {
            UnOp::Neg => "-",
            UnOp::Not => "not",
        }
    }
}

/// A run-constant numeric history index. Arithmetic retains fractional Pine
/// integer division; truncation happens only at an explicit cast or index use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HtfHistoryOffset {
    Int(i64),
    Float(f64),
    InputInt {
        input: u32,
        default: i64,
    },
    InputFloat {
        input: u32,
        default: f64,
    },
    Neg(Box<Self>),
    CastInt(Box<Self>),
    CastFloat(Box<Self>),
    Binary {
        op: BinOp,
        lhs: Box<Self>,
        rhs: Box<Self>,
    },
}

impl HtfHistoryOffset {
    /// Whether the index depends on a selected input rather than constants.
    pub fn uses_input(&self) -> bool {
        match self {
            Self::InputInt { .. } | Self::InputFloat { .. } => true,
            Self::Neg(x) | Self::CastInt(x) | Self::CastFloat(x) => x.uses_input(),
            Self::Binary { lhs, rhs, .. } => lhs.uses_input() || rhs.uses_input(),
            Self::Int(_) | Self::Float(_) => false,
        }
    }

    /// Numeric result, or None for na/overflow. Input type validation is also
    /// performed by the serialized-program validator and VM admission.
    pub fn evaluate(&self, selected: &impl Fn(u32) -> Option<InputValue>) -> Option<InputValue> {
        fn number(x: InputValue) -> Option<f64> {
            match x {
                InputValue::Int(x) => Some(x as f64),
                InputValue::Float(x) if x.is_finite() => Some(x),
                _ => None,
            }
        }
        fn finite(x: f64) -> Option<InputValue> {
            x.is_finite().then_some(InputValue::Float(x))
        }
        match self {
            Self::Int(x) => Some(InputValue::Int(*x)),
            Self::Float(x) => finite(*x),
            Self::InputInt { input, default } => match selected(*input) {
                Some(InputValue::Int(x)) => Some(InputValue::Int(x)),
                None => Some(InputValue::Int(*default)),
                _ => None,
            },
            Self::InputFloat { input, default } => match selected(*input) {
                Some(InputValue::Float(x)) => finite(x),
                None => finite(*default),
                _ => None,
            },
            Self::Neg(x) => match x.evaluate(selected)? {
                InputValue::Int(x) => x.checked_neg().map(InputValue::Int),
                x => finite(-number(x)?),
            },
            Self::CastFloat(x) => finite(number(x.evaluate(selected)?)?),
            Self::CastInt(x) => match x.evaluate(selected)? {
                InputValue::Int(x) => Some(InputValue::Int(x)),
                x => {
                    let x = number(x)?.trunc();
                    (x >= i64::MIN as f64 && x < i64::MAX as f64)
                        .then_some(InputValue::Int(x as i64))
                }
            },
            Self::Binary { op, lhs, rhs } => {
                let lhs = lhs.evaluate(selected)?;
                let rhs = rhs.evaluate(selected)?;
                if let (InputValue::Int(l), InputValue::Int(r)) = (&lhs, &rhs) {
                    return match op {
                        BinOp::Add => l.checked_add(*r).map(InputValue::Int),
                        BinOp::Sub => l.checked_sub(*r).map(InputValue::Int),
                        BinOp::Mul => l.checked_mul(*r).map(InputValue::Int),
                        BinOp::Rem => l.checked_rem(*r).map(InputValue::Int),
                        BinOp::Div => {
                            let q = l.checked_div(*r)?;
                            if l.checked_rem(*r)? == 0 {
                                Some(InputValue::Int(q))
                            } else {
                                finite(*l as f64 / *r as f64)
                            }
                        }
                        _ => None,
                    };
                }
                let (l, r) = (number(lhs)?, number(rhs)?);
                finite(match op {
                    BinOp::Add => l + r,
                    BinOp::Sub => l - r,
                    BinOp::Mul => l * r,
                    BinOp::Div => l / r,
                    BinOp::Rem => l % r,
                    _ => return None,
                })
            }
        }
    }

    /// Exact equality for translation validation, including signed zero.
    pub fn exact_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Int(a), Self::Int(b)) => a == b,
            (Self::Float(a), Self::Float(b)) => a.to_bits() == b.to_bits(),
            (
                Self::InputInt {
                    input: a,
                    default: ad,
                },
                Self::InputInt {
                    input: b,
                    default: bd,
                },
            ) => a == b && ad == bd,
            (
                Self::InputFloat {
                    input: a,
                    default: ad,
                },
                Self::InputFloat {
                    input: b,
                    default: bd,
                },
            ) => a == b && ad.to_bits() == bd.to_bits(),
            (Self::Neg(a), Self::Neg(b))
            | (Self::CastInt(a), Self::CastInt(b))
            | (Self::CastFloat(a), Self::CastFloat(b)) => a.exact_eq(b),
            (
                Self::Binary {
                    op: a,
                    lhs: al,
                    rhs: ar,
                },
                Self::Binary {
                    op: b,
                    lhs: bl,
                    rhs: br,
                },
            ) => a == b && al.exact_eq(bl) && ar.exact_eq(br),
            _ => false,
        }
    }

    /// Delimiter-safe selector fragment for the run-constant expression.
    pub fn selector_fragment(&self) -> String {
        match self {
            Self::Int(x) => format!("i{x}"),
            Self::Float(x) => format!("f{:016x}", x.to_bits()),
            Self::InputInt { input, default } => format!("ii{input}:{default}"),
            Self::InputFloat { input, default } => format!("if{input}:{:016x}", default.to_bits()),
            Self::Neg(x) => format!("n({})", x.selector_fragment()),
            Self::CastInt(x) => format!("ci({})", x.selector_fragment()),
            Self::CastFloat(x) => format!("cf({})", x.selector_fragment()),
            Self::Binary { op, lhs, rhs } => format!(
                "b{}({},{})",
                op.symbol(),
                lhs.selector_fragment(),
                rhs.selector_fragment()
            ),
        }
    }

    /// Heap storage owned by this index, excluding its inline root.
    pub fn allocated_bytes(&self) -> Option<usize> {
        let mut bytes = 0usize;
        let mut pending = vec![self];
        while let Some(node) = pending.pop() {
            match node {
                Self::Neg(x) | Self::CastInt(x) | Self::CastFloat(x) => {
                    bytes = bytes.checked_add(std::mem::size_of::<Self>())?;
                    pending.push(x);
                }
                Self::Binary { lhs, rhs, .. } => {
                    bytes = bytes.checked_add(2 * std::mem::size_of::<Self>())?;
                    pending.push(lhs);
                    pending.push(rhs);
                }
                _ => {}
            }
        }
        Some(bytes)
    }
}

/// A compound `request.security` sub-expression, lowered into a compact tree
/// that a **secondary context** evaluates once per RESAMPLED (higher-timeframe)
/// bar. This is the durable, serializable descriptor of the
/// sub-program; the VM walks it over the HTF series with its own per-context
/// `ta.*` state (distinct from `main`), then maps the per-bucket results back
/// per chart bar under `gaps`/`lookahead` (see `pinewasm_vm::resample`).
///
/// Only the tractable subset lowers here (analyzer builds it from the
/// already-checked HIR of the `expression` argument): OHLCV/derived price
/// reads, `barstate.islast`, numeric literals, unary neg, binary
/// arithmetic/comparison, and the slot/engine `ta.*` moving averages / change
/// over those. Boolean results are marked at the root while their compact
/// secondary values use exact `0.0`/`1.0` encodings. Any other construct keeps
/// the whole call a diagnostic (`E0244`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HtfExpr {
    /// Boolean result marker. The secondary evaluator carries booleans as
    /// exact `0.0`/`1.0` cells; this root marker preserves the Pine result type
    /// at the CSIL stack and VM boundary without changing arithmetic nodes.
    BoolResult(Box<HtfExpr>),
    /// A numeric constant (ints already widened to float — the secondary
    /// context is float-only).
    Const(f64),
    /// A plain OHLCV/derived price series read over the HTF bar (`close`,
    /// `hl2`, ...). Non-price builtins never reach here (rejected at analysis).
    Source(BuiltinSeries),
    /// An `input.source` selection, resolved against the run's builtin-source
    /// override before evaluating any requested bars.
    InputSource { input: u32 },
    /// `barstate.islast` in the requested context. It is true only on the
    /// final available requested bar in the dataset.
    BarstateIslast,
    /// `barstate.isrealtime` in the requested context. It is true only for the
    /// requested bucket containing the actively executing realtime chart bar.
    BarstateIsRealtime,
    /// A history read in the secondary context. `history_slot` identifies this
    /// syntactic history site, whose values are recorded once per resampled
    /// bar. This supports both direct source history (`close[1]`) and history
    /// of a stateful expression (`ta.sma(close, 10)[1]`).
    History {
        history_slot: u32,
        offset: u32,
        source: Box<HtfExpr>,
    },
    /// History whose offset is run-constant numeric input arithmetic.
    /// Binds and truncates the index before evaluating requested bars.
    HistoryInput {
        history_slot: u32,
        offset: HtfHistoryOffset,
        source: Box<HtfExpr>,
    },
    /// History selected by the requested context's realtime state. This
    /// represents the two-arm realtime-dependent history offset
    /// `x[barstate.isrealtime ? realtime_offset : history_offset]`.
    HistoryRealtime {
        history_slot: u32,
        history_offset: u32,
        realtime_offset: u32,
        source: Box<HtfExpr>,
    },
    /// Unary negation of a sub-expression.
    Neg(Box<HtfExpr>),
    /// A binary arithmetic/comparison over two sub-expressions. Comparisons
    /// yield `1.0`/`0.0` (the secondary context is float-typed); an `na`
    /// operand propagates to `na`.
    Binary {
        op: BinOp,
        /// True when the HIR node was integer-typed (`int OP int -> int`).
        /// The secondary context evaluates on `f64`, so a `Rem` marked here
        /// keeps integer semantics; `+`/`-`/`*` on integers are already exact
        /// on `f64`, so the flag is a no-op for them. A source `/` is never
        /// integer-typed, so a marked `Div` only comes from an older serialized
        /// program and keeps its truncating result.
        #[serde(default)]
        int_result: bool,
        lhs: Box<HtfExpr>,
        rhs: Box<HtfExpr>,
    },
    /// One arm of a run-constant enum selection. Bind only the chosen branch
    /// before resampling so unselected stateful sites never advance.
    EnumSelect {
        input: u32,
        index: u32,
        when_equal: Box<HtfExpr>,
        otherwise: Option<Box<HtfExpr>>,
    },
    /// A stateful `ta.*` over the HTF series. `ta_slot` is a per-site index into
    /// the secondary context's OWN state space (each site keeps an independent
    /// running history / accumulator over the HTF bars — never main's).
    Ta {
        func: TaFunc,
        ta_slot: u32,
        length: u32,
        source: Box<HtfExpr>,
    },
    /// A slot/engine TA call whose run-constant length comes from `input.int`.
    /// The VM resolves it before computing or costing the secondary context.
    TaInput {
        func: TaFunc,
        ta_slot: u32,
        input: u32,
        binding_target: TaInputTarget,
        default_length: u32,
        source: Box<HtfExpr>,
    },
}

impl HtfExpr {
    /// Whether the secondary expression returns a Pine bool rather than a
    /// numeric value. Only the root wrapper affects the result type.
    pub fn result_is_bool(&self) -> bool {
        matches!(self, HtfExpr::BoolResult(_))
    }

    /// The number of independent `ta.*` state slots the secondary context needs
    /// (the max `ta_slot + 1` over the tree; `0` when there is no `ta.*`).
    pub fn ta_slot_count(&self) -> u32 {
        match self {
            HtfExpr::BoolResult(inner) => inner.ta_slot_count(),
            HtfExpr::EnumSelect {
                when_equal,
                otherwise,
                ..
            } => when_equal
                .ta_slot_count()
                .max(otherwise.as_ref().map_or(0, |expr| expr.ta_slot_count())),
            HtfExpr::Const(_)
            | HtfExpr::Source(_)
            | HtfExpr::InputSource { .. }
            | HtfExpr::BarstateIslast
            | HtfExpr::BarstateIsRealtime => 0,
            HtfExpr::History { source, .. }
            | HtfExpr::HistoryInput { source, .. }
            | HtfExpr::HistoryRealtime { source, .. } => source.ta_slot_count(),
            HtfExpr::Neg(inner) => inner.ta_slot_count(),
            HtfExpr::Binary { lhs, rhs, .. } => lhs.ta_slot_count().max(rhs.ta_slot_count()),
            HtfExpr::Ta {
                ta_slot, source, ..
            }
            | HtfExpr::TaInput {
                ta_slot, source, ..
            } => ta_slot.saturating_add(1).max(source.ta_slot_count()),
        }
    }

    /// The number of independent history sites needed by the secondary
    /// context (the max `history_slot + 1` over the tree).
    pub fn history_slot_count(&self) -> u32 {
        match self {
            HtfExpr::BoolResult(inner) => inner.history_slot_count(),
            HtfExpr::EnumSelect {
                when_equal,
                otherwise,
                ..
            } => when_equal.history_slot_count().max(
                otherwise
                    .as_ref()
                    .map_or(0, |expr| expr.history_slot_count()),
            ),
            HtfExpr::Const(_)
            | HtfExpr::Source(_)
            | HtfExpr::InputSource { .. }
            | HtfExpr::BarstateIslast
            | HtfExpr::BarstateIsRealtime => 0,
            HtfExpr::History {
                history_slot,
                source,
                ..
            }
            | HtfExpr::HistoryInput {
                history_slot,
                source,
                ..
            }
            | HtfExpr::HistoryRealtime {
                history_slot,
                source,
                ..
            } => history_slot
                .saturating_add(1)
                .max(source.history_slot_count()),
            HtfExpr::Neg(inner) => inner.history_slot_count(),
            HtfExpr::Binary { lhs, rhs, .. } => {
                lhs.history_slot_count().max(rhs.history_slot_count())
            }
            HtfExpr::Ta { source, .. } | HtfExpr::TaInput { source, .. } => {
                source.history_slot_count()
            }
        }
    }

    /// Stable, delimiter-safe selector fragment for a host-backed external
    /// compound `request.security()` result.
    pub fn selector_fragment(&self) -> String {
        fn encode(expr: &HtfExpr, out: &mut String) {
            use std::fmt::Write;
            match expr {
                HtfExpr::BoolResult(inner) => {
                    out.push_str("br(");
                    encode(inner, out);
                    out.push(')');
                }
                HtfExpr::Const(value) => {
                    let _ = write!(out, "c{:016x}", value.to_bits());
                }
                HtfExpr::Source(series) => {
                    let name = series.name();
                    let _ = write!(out, "s{}:{name}", name.len());
                }
                HtfExpr::InputSource { input } => {
                    let _ = write!(out, "is{input}");
                }
                HtfExpr::BarstateIslast => out.push_str("bs:last"),
                HtfExpr::BarstateIsRealtime => out.push_str("bs:realtime"),
                HtfExpr::History { offset, source, .. } => {
                    let _ = write!(out, "h{offset}(");
                    encode(source, out);
                    out.push(')');
                }
                HtfExpr::HistoryInput { offset, source, .. } => {
                    let _ = write!(out, "hi{}(", offset.selector_fragment());
                    encode(source, out);
                    out.push(')');
                }
                HtfExpr::HistoryRealtime {
                    history_offset,
                    realtime_offset,
                    source,
                    ..
                } => {
                    let _ = write!(out, "hr{history_offset}:{realtime_offset}(");
                    encode(source, out);
                    out.push(')');
                }
                HtfExpr::Neg(inner) => {
                    out.push_str("n(");
                    encode(inner, out);
                    out.push(')');
                }
                HtfExpr::Binary {
                    op,
                    int_result,
                    lhs,
                    rhs,
                } => {
                    let _ = write!(out, "b{}{}(", op.symbol(), u8::from(*int_result));
                    encode(lhs, out);
                    out.push(',');
                    encode(rhs, out);
                    out.push(')');
                }
                HtfExpr::EnumSelect {
                    input,
                    index,
                    when_equal,
                    otherwise,
                } => {
                    let _ = write!(out, "es{input}:{index}(");
                    encode(when_equal, out);
                    out.push(',');
                    if let Some(otherwise) = otherwise {
                        encode(otherwise, out);
                    } else {
                        out.push_str("na");
                    }
                    out.push(')');
                }
                HtfExpr::Ta {
                    func,
                    length,
                    source,
                    ..
                } => {
                    let name = func.name();
                    let _ = write!(out, "t{}:{name}:{length}(", name.len());
                    encode(source, out);
                    out.push(')');
                }
                HtfExpr::TaInput {
                    func,
                    input,
                    source,
                    ..
                } => {
                    let name = func.name();
                    let _ = write!(out, "ti{}:{name}:{input}(", name.len());
                    encode(source, out);
                    out.push(')');
                }
            }
        }

        let mut out = String::new();
        encode(self, &mut out);
        out
    }

    /// Whether this tree needs run-selected input values before evaluation.
    pub fn uses_input(&self) -> bool {
        match self {
            HtfExpr::BoolResult(inner) => inner.uses_input(),
            HtfExpr::InputSource { .. } | HtfExpr::TaInput { .. } | HtfExpr::EnumSelect { .. } => {
                true
            }
            HtfExpr::HistoryInput { offset, source, .. } => {
                offset.uses_input() || source.uses_input()
            }
            HtfExpr::History { source, .. }
            | HtfExpr::HistoryRealtime { source, .. }
            | HtfExpr::Neg(source)
            | HtfExpr::Ta { source, .. } => source.uses_input(),
            HtfExpr::Binary { lhs, rhs, .. } => lhs.uses_input() || rhs.uses_input(),
            HtfExpr::Const(_)
            | HtfExpr::Source(_)
            | HtfExpr::BarstateIslast
            | HtfExpr::BarstateIsRealtime => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HirExpr {
    pub kind: HirExprKind,
    pub ty: Type,
    pub qualifier: Qualifier,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HirExprKind {
    ConstInt(i64),
    ConstFloat(f64),
    ConstBool(bool),
    ConstString(String),
    /// A compile-time color: an `#RRGGBB`/`#RRGGBBAA` literal or a `color.*`
    /// named constant. `t` is transparency in `0..=100`.
    ConstColor {
        r: u8,
        g: u8,
        b: u8,
        #[serde(serialize_with = "serialize_transparency")]
        t: f64,
    },
    /// The `na` literal. Its `ty` is adopted from the expression's context
    /// (function parameter, typed binary sibling, `:=` target); positions
    /// with no expected type are rejected at checking time (`E0220`), so a
    /// `ConstNa` node always carries a concrete type.
    ConstNa,
    /// `close`, `close[n]` — a builtin series read at a bar offset.
    ReadBuiltin {
        series: BuiltinSeries,
        offset: u32,
    },
    /// `x`, `x[n]` — a user series read at a bar offset.
    ReadVar {
        symbol: SymbolId,
        offset: u32,
    },
    /// `close[n]` where `n` is a **series/simple int expression** (not a
    /// literal). `offset` evaluates to an int at run time; the VM reads the
    /// builtin `series` at `current_bar - offset`. A negative offset, an
    /// offset beyond available history, or an `na` offset yields `na`.
    /// The literal-offset fast path stays [`HirExprKind::ReadBuiltin`].
    ReadBuiltinDynamic {
        series: BuiltinSeries,
        offset: Box<HirExpr>,
    },
    /// `x[n]` where `n` is a **series/simple int expression** (not a literal),
    /// on a user variable. `offset` evaluates to an int at run time; the VM
    /// reads the user slot at `current_bar - offset`. Out-of-range / `na`
    /// offset yields `na`. The literal-offset fast path stays
    /// [`HirExprKind::ReadVar`].
    ReadVarDynamic {
        symbol: SymbolId,
        offset: Box<HirExpr>,
    },
    /// A read of one `input.*` declaration ([`HirProgram::inputs`] entry
    /// `input`). At run time the VM resolves the value from the host override
    /// map (keyed by input id) or the declared default. The expression's
    /// `ty`/`qualifier` come from the declaration's [`InputKind`]
    /// (`input`-qualified for scalar inputs; `series float` for
    /// `input.source`, which reads a builtin series each bar).
    ReadInput {
        input: u32,
    },
    /// `request.security(sym, tf, expr)` restricted to a plain builtin series
    /// `expression`. `source` is the requested OHLCV/derived series,
    /// `htf_seconds` the requested higher-timeframe length (positive literal
    /// seconds, zero for chart timeframe, or `-(input_id + 1)` for a
    /// run-constant string input), and `gaps_on`/`lookahead_on` the `barmerge.*`
    /// modes. `selector=None` evaluates over the chart feed resampled to the
    /// higher timeframe and mapped back per chart bar (see
    /// `pinewasm_vm::resample`). `selector=Some(...)` is a host-provided
    /// external-symbol series read from `MarketData::request_data`. Always a
    /// `series float` (an HTF gap / not-yet-available bucket reads `na`).
    ReadSecurity {
        selector: Option<String>,
        source: BuiltinSeries,
        htf_seconds: i64,
        /// Hex-encoded input-stable string expression selected once per run.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeframe_expr: Option<String>,
        /// Calendar-month multiplier for `M` timeframes. `None` preserves the
        /// fixed-width semantics of an equal-duration timeframe such as
        /// `30D`, which must not be conflated with `1M`.
        #[serde(default)]
        calendar_months: Option<u32>,
        gaps_on: bool,
        lookahead_on: bool,
    },
    /// `request.security(sym, tf, expr)` for a COMPOUND `expression`,
    /// executed by a secondary context over the resampled HTF bars. `expr` is
    /// the compact sub-program tree; `htf_seconds`/`gaps_on`/`lookahead_on`
    /// carry the same meaning as [`HirExprKind::ReadSecurity`]. Always a
    /// `series float`.
    ReadSecurityExpr {
        expr: HtfExpr,
        htf_seconds: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeframe_expr: Option<String>,
        /// Calendar-month multiplier for `M` timeframes.
        #[serde(default)]
        calendar_months: Option<u32>,
        gaps_on: bool,
        lookahead_on: bool,
    },
    /// A `request.*` external-data interface read. `func` is the
    /// interface variant and `selector` is a stable, run-deterministic string
    /// key built from the (constant) request arguments (e.g.
    /// `"financial/AAPL/TOTAL_REVENUE/FQ"`). The VM looks the selector up in the
    /// optional host table `MarketData::request_data`; a hit yields that per-bar
    /// float series, a miss yields `na` every bar. Always a `series float`.
    ReadRequest {
        func: RequestFunc,
        selector: String,
    },
    /// `request.footprint(...)` — build the current bar's order-flow `footprint`
    /// object (newer namespace). `selector` is a stable,
    /// run-deterministic key built from the (constant) request arguments; the VM
    /// materializes the object's scalar order-flow fields and its trailing
    /// `array<volume_row>` rows from the optional host table
    /// `MarketData::footprint_data`, defaulting every field to `na`/empty. The
    /// expression's `ty` is the `footprint` [`Type::Object`]. Data sourcing is a
    /// host concern — no real order-flow feed is involved.
    BuildFootprint {
        selector: String,
    },
    /// `request.security_lower_tf(symbol, timeframe, expression, ...)` — build the
    /// current bar's `array<float>` of intrabar values. `selector`
    /// is a stable, run-deterministic key from the (constant) symbol/timeframe
    /// arguments; the VM materializes a fresh float array from the optional host
    /// table `MarketData::security_lower_tf_data`, defaulting to an EMPTY array.
    /// This VM only aggregates UP, so lower-timeframe intrabar decomposition is
    /// a host concern — no real intrabar feed is involved. The
    /// expression's `ty` is `array<float>`.
    ReadSecurityLowerTf {
        selector: String,
    },
    /// `ta.pivot_point_levels(type, anchor)`. Builds a fresh
    /// `array<float>` of pivot levels for the `system` from the PRIOR completed
    /// `anchor_seconds`-period High/Low/Close (via the same resample machinery
    /// as `request.security`, `lookahead_off` — never the forming bucket, no
    /// future leak). Before any anchor bucket has completed, the array is
    /// EMPTY. Always `array<float>`. Lowers 1:1 to
    /// `pinewasm_csil::CsilInstr::ReadPivotLevels`.
    ReadPivotLevels {
        system: PivotSystem,
        anchor_seconds: i64,
    },
    /// Runtime-stateful Pine `ta.pivot_point_levels(type, anchor, developing)`.
    /// The series operands are preserved in signature order for the VM; the
    /// returned value is a fresh `array<float>` on each execution.
    CallPivotPointLevels {
        args: Vec<HirExpr>,
    },
    /// Older baked-timeframe form for `time()` / `time_close()`. The current
    /// Pine-facing runtime-string/session overloads lower through
    /// [`PureFunc::TimeOpen`] / [`PureFunc::TimeClose`]; this variant remains so
    /// old serialized programs can still lower 1:1 to
    /// `pinewasm_csil::CsilInstr::ReadTimeTf`.
    ReadTimeTf {
        close: bool,
        tf_seconds: i64,
    },
    /// `footprint.get_row_by_price(fp, price)` — the `volume_row` of `footprint`
    /// whose `[down_price, up_price]` range contains `price`, or `na` when no row
    /// matches (or the receiver has no rows). `footprint` evaluates to the object
    /// handle (an `na` receiver is a structured runtime error); `price` is a
    /// float. The expression's `ty` is the `volume_row` [`Type::Object`].
    FootprintRowByPrice {
        footprint: Box<HirExpr>,
        price: Box<HirExpr>,
    },
    Unary {
        op: UnOp,
        operand: Box<HirExpr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<HirExpr>,
        rhs: Box<HirExpr>,
    },
    /// `cond ? then_e : else_e` — the ternary conditional. **Eager**: both
    /// branches evaluate every bar and the condition selects one value (a
    /// documented divergence from Pine's lazy evaluation; see
    /// `docs/language-support.md`). An `na` condition selects `else_e`
    /// (na-condition-is-false, consistent with `if`). The branch types were
    /// unified by the checker (int sides coerced when mixed with float), so
    /// both carry this expression's type.
    Ternary {
        cond: Box<HirExpr>,
        then_e: Box<HirExpr>,
        else_e: Box<HirExpr>,
    },
    /// Explicit int → float coercion, inserted by the type checker wherever
    /// a float is required but an int was given (mixed arithmetic and
    /// comparisons, `plot(...)` values, ...).
    IntToFloat(Box<HirExpr>),
    /// Evaluate operands once, in order, into hidden slots and then evaluate
    /// `result` using those captured values. Unlike statement-level hoisting,
    /// the bindings stay inside the call expression: skipped branches and a
    /// persistent initializer do not run them. Slots are refreshed on every
    /// actual execution, including repeated calls in a loop.
    EvalSequence {
        bindings: Vec<(SeriesSlot, HirExpr)>,
        result: Box<HirExpr>,
    },
    /// `(expr)[n]` on a compound series expression: evaluate `expr`, store
    /// it into the hidden slot for the current bar, then read the slot at
    /// offset `n`. The slot belongs to a synthetic [`Symbol`] (`%h0`, ...)
    /// allocated per syntactic site, so history composes: `close[1][2]`
    /// reads its hidden slot — which holds `close[1]` per bar — two bars
    /// back, giving `close[3]`. Identifier targets keep the direct
    /// [`HirExprKind::ReadBuiltin`]/[`HirExprKind::ReadVar`] offset path.
    MaterializeHistory {
        slot: SeriesSlot,
        expr: Box<HirExpr>,
        offset: u32,
    },
    /// `(expr)[n]` on a compound series expression where `n` is a
    /// **series/simple int expression** (not a literal). Like
    /// [`HirExprKind::MaterializeHistory`], `expr` is stored into the hidden
    /// `slot` for the current bar; the read then indexes the slot at the
    /// runtime-evaluated `offset` (out-of-range / `na` → `na`).
    MaterializeHistoryDynamic {
        slot: SeriesSlot,
        expr: Box<HirExpr>,
        offset: Box<HirExpr>,
    },
    /// A stateful `ta.*` call (`ta.change(...)`, `ta.sma(...)`). Each call
    /// site owns its own hidden slot (a synthetic `%hN` [`Symbol`], like
    /// [`HirExprKind::MaterializeHistory`]): when the site executes, the
    /// coerced-to-float `source` is stored into the slot's current-bar cell
    /// and the function's value is computed from the slot's history.
    /// `length` is a compile-time int literal (1..=100000, `E0219`
    /// otherwise).
    TaCall {
        func: TaFunc,
        slot: SeriesSlot,
        source: Box<HirExpr>,
        length: u32,
    },
    /// `ta.sma`, `ta.highest`, `ta.lowest`, or `ta.ema` with a runtime int
    /// length. The source and length are evaluated on every bar. Window
    /// functions retain source history; EMA uses the slot as a state identity.
    /// Fixed literals and direct inputs keep their existing specialized paths.
    TaCallDynamic {
        func: TaFunc,
        slot: SeriesSlot,
        source: Box<HirExpr>,
        length: Box<HirExpr>,
    },
    /// An **engine-backed** stateful call (`ta.ema`, `ta.rsi`, `fixnan`,
    /// `math.sum`, ...). The call site owns entry `state` of the program's
    /// [`HirProgram::ta_states`] table (its [`TaStateDecl`] carries the
    /// baked const parameters); `args` are the runtime source operands per
    /// [`TaFunc::engine_operands`] (numeric sources already coerced to
    /// float, conditions bool). The VM keeps the per-site state and
    /// advances it only when the site executes.
    CallTaState {
        func: TaFunc,
        state: u32,
        args: Vec<HirExpr>,
    },
    /// A pure builtin call (`math.sqrt(...)`, `na(...)`, `nz(...)`). No
    /// hidden state: the arguments are evaluated in order and the function
    /// maps them to one value. Optional-argument defaults (`nz`'s typed
    /// zero, `math.random`'s 0.0/1.0 bounds) are already materialized as
    /// ordinary argument expressions by the checker.
    CallPure {
        func: PureFunc,
        args: Vec<HirExpr>,
    },
    /// A reference-typed collection builtin call (`array.new_float(...)`,
    /// `array.push(...)`). The arguments (receiver handle first, per the
    /// namespace call form) are evaluated in order. Constructors materialize
    /// their `(size, initial)` defaults as ordinary argument expressions;
    /// mutators produce [`Type::Void`]. The expression's `ty` is the element
    /// type for accessors, `array<T>` for constructors, or `Void`/`Int`/`Bool`
    /// per the function.
    CallCollection {
        func: CollectionFunc,
        args: Vec<HirExpr>,
    },
    /// `Name.new(...)` — construct a user-defined-type instance. `type_id`
    /// indexes [`HirProgram::object_types`]; `fields` are the per-field
    /// initializer expressions in declaration order (already coerced to each
    /// field's type; an omitted field with a default carries that default, an
    /// omitted field with no default carries `ConstNa` of the field type). The
    /// MIR lowering evaluates them left to right and allocates a fresh heap
    /// object; the expression's `ty` is [`Type::Object`].
    NewObject {
        type_id: u32,
        fields: Vec<HirExpr>,
    },
    /// `Name.copy(obj)` — a shallow copy: a NEW handle whose field values are
    /// copied from `obj`. `type_id` indexes [`HirProgram::object_types`]; `obj`
    /// evaluates to the source object handle. The expression's `ty` is
    /// [`Type::Object`].
    CopyObject {
        type_id: u32,
        obj: Box<HirExpr>,
    },
    /// `obj.field` — read field `field_idx` of an object. `obj` evaluates to the
    /// object handle; the expression's `ty` is the field's declared type. An
    /// `na` object is a structured runtime error.
    GetField {
        obj: Box<HirExpr>,
        field_idx: u32,
    },
    /// A tuple expression `[e0, e1, ...]`. **Compile-time only**: there is no
    /// tuple runtime value. A `Tuple` node is legal only as the trailing
    /// value of a user-function body (whose caller destructures the result)
    /// or, transiently, as the RHS analyzed by a destructuring declaration.
    /// Its `ty` is [`Type::Void`] (a tuple has no scalar type); the element
    /// types are carried by the element expressions. The MIR lowering never
    /// sees a `Tuple` as a scalar operand — destructuring consumes it — so
    /// it has no register form.
    Tuple {
        elements: Vec<HirExpr>,
    },
    /// `<ns>.new(...)` for a registered drawing kind (`line`/`label`/`box`):
    /// `inner` constructs the underlying `HeapObj::Object` (a
    /// [`HirExprKind::NewObject`] over the kind's synthetic object type), and
    /// this node then **registers** the resulting handle in the kind's live
    /// registry, running deterministic oldest-first GC if the max count is
    /// exceeded. Evaluates to the same object handle (`ty` = the drawing's
    /// [`Type::Object`]). `chart.point.new` does NOT wrap in this node — a point
    /// is not registered.
    RegisterDrawing {
        kind: DrawKind,
        inner: Box<HirExpr>,
    },
    /// `<ns>.delete(id)` — remove a drawing object from its kind's live
    /// registry. A void expression (used as a bare statement); `id` evaluates
    /// to the drawing handle. An `na` receiver is a structured runtime error;
    /// deleting an id that is not (or no longer) live is a no-op.
    DeleteDrawing {
        kind: DrawKind,
        id: Box<HirExpr>,
    },
    /// `<ns>.set_<field>(obj, value)` — a drawing setter: write field
    /// `field_idx` of `obj` and produce **void** (used as a bare statement).
    /// The expression form of [`HirStmt::SetField`] so the setter can flow
    /// through the analyzer's expression-centric drawing dispatch. An `na`
    /// receiver is a structured runtime error.
    SetFieldExpr {
        obj: Box<HirExpr>,
        field_idx: u32,
        value: Box<HirExpr>,
    },
    /// A compound drawing setter that writes several fields of ONE object in a
    /// single call (`line.set_xy1`, `box.set_lefttop`, the `chart.point`-taking
    /// setters, ...). `obj` is evaluated **once**; each `(field_idx, value)`
    /// write targets that same handle. Produces **void**. Distinct from
    /// [`HirExprKind::SetFieldExpr`] precisely so the receiver (which may be a
    /// side-effecting `<ns>.new(...)`) is not re-evaluated per field. An `na`
    /// receiver is a structured runtime error (each underlying field write
    /// checks it). `recv_slot` is a hidden series slot that captures the
    /// evaluated receiver so the MIR lowering can re-read the handle for each
    /// field write without re-evaluating `obj`.
    SetFields {
        obj: Box<HirExpr>,
        recv_slot: SeriesSlot,
        writes: Vec<(u32, HirExpr)>,
    },
    /// `line.get_price(id, x)` — the interpolated y-price of `obj`'s line at bar
    /// `x`. `obj` is evaluated **once** into `recv_slot` (so a `<ns>.new(...)`
    /// receiver is not re-registered) and its four endpoint fields
    /// (`x1`,`y1`,`x2`,`y2`) are read from the captured handle; the result is
    /// `y1 + (x - x1) * (y2 - y1) / (x2 - x1)` (linear interpolation,
    /// extrapolating outside the segment). `ty` is `Type::Float`. An `na`
    /// receiver is a structured runtime error.
    LineGetPrice {
        obj: Box<HirExpr>,
        recv_slot: SeriesSlot,
        x: Box<HirExpr>,
    },
    /// `<ns>.all` for a registered drawing kind (`line`/`label`/`box`/`table`/
    /// `polyline`/`linefill`): a fresh `array<drawing>` of the live objects of
    /// that kind, in creation order. Reads the VM's live drawing registry and
    /// builds a new heap array each evaluation (`ty` = `Type::Array` of the
    /// kind's [`Type::Object`]); positive literal and dynamic history offsets
    /// capture the evaluated array in a hidden slot.
    ReadDrawingsAll {
        kind: DrawKind,
    },
    /// `table.cell(t, col, row, ...)` / `table.cell_set_<attr>(t, col, row, v)`
    /// — write one or more attributes of the cell at
    /// `(column, row)` of table `obj`. `obj` is evaluated **once** into
    /// `recv_slot`; `column`/`row` are `int` expressions; each write pairs a
    /// [`CellAttr`] with its value expression. Produces **void**. An `na`
    /// receiver or an out-of-range `(column, row)` (against the table's declared
    /// `columns`×`rows`) is a structured runtime error (never a panic).
    SetCell {
        obj: Box<HirExpr>,
        recv_slot: SeriesSlot,
        column: Box<HirExpr>,
        row: Box<HirExpr>,
        writes: Vec<(CellAttr, HirExpr)>,
    },
    /// `table.clear(t, start_column, start_row, end_column?, end_row?)`
    /// — clear every cell in the inclusive `(sc,sr)`..=`(ec,er)` range of
    /// table `obj` (defaulting the end coordinates to the start). Produces
    /// **void**. `na` receiver / out-of-range coordinates are structured errors.
    ClearCells {
        obj: Box<HirExpr>,
        recv_slot: SeriesSlot,
        start_column: Box<HirExpr>,
        start_row: Box<HirExpr>,
        end_column: Box<HirExpr>,
        end_row: Box<HirExpr>,
    },
    /// `table.merge_cells(t, start_column, start_row, end_column, end_row)`
    /// — mark the rectangular span a merged region. Produces
    /// **void**. `na` receiver / out-of-range coordinates are structured errors.
    MergeCells {
        obj: Box<HirExpr>,
        recv_slot: SeriesSlot,
        start_column: Box<HirExpr>,
        start_row: Box<HirExpr>,
        end_column: Box<HirExpr>,
        end_row: Box<HirExpr>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HirStmt {
    /// `x = value` / `var x = value` / `x := value` — store into the
    /// variable's series slot for this bar. Declarations and reassignments
    /// share this node: the resolver resolves `:=` to the existing symbol
    /// instead of declaring a new one.
    Assign {
        symbol: SymbolId,
        value: HirExpr,
        /// True when this statement is the variable's declaration site
        /// (`=` / `var ... =`); false for `:=` reassignment. A persistent
        /// symbol's declaration lowers to initialize-once control flow.
        init: bool,
        span: Span,
    },
    /// `[a, b, c] = tuple_expr` — a destructuring declaration. `targets` are
    /// the newly declared variables (one per tuple element, in order);
    /// `value` produces the tuple (a tuple-returning [`HirExprKind::CallTaState`]
    /// or a user-function result whose body ended in a
    /// [`HirExprKind::Tuple`]). The MIR lowering evaluates the tuple once —
    /// pushing N values left to right — and stores each into its target's
    /// slot (popping in reverse: the last element, on top of the stack, into
    /// the last target first). See `docs/ir.md` and `docs/language-support.md`.
    Destructure {
        targets: Vec<SymbolId>,
        value: HirExpr,
        span: Span,
    },
    /// `obj.field := value` — write field `field_idx` of an object. `obj`
    /// evaluates to the object handle; `value` is coerced to the field's type.
    /// An `na` object is a structured runtime error.
    SetField {
        obj: HirExpr,
        field_idx: u32,
        value: HirExpr,
        span: Span,
    },
    /// Desugared `plot(value)`. Also the lowering target of the data-bearing
    /// series of the display-plot family: `plotshape`/`plotchar`
    /// (booleans as `1.0`/`na`, numeric payloads verbatim), `plotarrow` (its numeric value),
    /// `plotcandle`/`plotbar` (each of the four OHLC series), and `hline` (the
    /// constant price). Each such series is allocated its own [`PlotSlot`] with
    /// a descriptive title, so the run surfaces the data even though the
    /// headless VM renders no glyph/shape/line.
    Plot {
        slot: PlotSlot,
        value: HirExpr,
        colors: Vec<(PlotColorRole, HirExpr)>,
        span: Span,
    },
    /// A color-only chart visual. Unlike a numeric plot it has no value
    /// channel, only named color channels evaluated once per bar.
    Visual {
        slot: VisualSlot,
        colors: Vec<(PlotColorRole, HirExpr)>,
        span: Span,
    },
    /// A color-only band whose geometry is supplied by two plot slots.
    Fill {
        slot: FillSlot,
        colors: Vec<(PlotColorRole, HirExpr)>,
        span: Span,
    },
    /// Desugared `runtime.error(message)`. Evaluates `message` (a
    /// `series string`; an `na`/omitted message yields an empty text) and
    /// aborts the current run with a structured user runtime error on the bar
    /// and branch where it executes. Bars completed before this one keep their
    /// recorded plots; `bars_executed` reflects the aborting bar. Allowed
    /// anywhere a statement is (including `if` branches), so a conditional
    /// `runtime.error` fires only when its branch runs.
    RuntimeError { message: HirExpr, span: Span },
    /// Desugared `log.info/warning/error(formatString, arg0, ...)`. The
    /// message is built with `str.format` placeholder rules from `format` and
    /// `args`; the VM records `(level, bar, message)` into the run outcome's
    /// `logs` stream. `format` is a `string`; each `arg` is a scalar value.
    Log {
        level: LogLevel,
        format: HirExpr,
        args: Vec<HirExpr>,
        span: Span,
    },
    /// Desugared `alert(message, freq?)`. `message` is a `string`; `freq`
    /// is one of the `alert.freq_*` const-string tokens (defaulting to
    /// `freq_once_per_bar`). The VM records `(bar, message, freq)` into the run
    /// outcome's `alerts` stream, honoring the per-bar firing rules.
    Alert {
        message: HirExpr,
        freq: HirExpr,
        span: Span,
    },
    /// Desugared `alertcondition(condition, title?, message?)`. Records an
    /// alert event only on bars where `condition` is true, once per bar close.
    /// `title`/`message` are strings.
    AlertCondition {
        condition: HirExpr,
        title: HirExpr,
        message: HirExpr,
        span: Span,
    },
    /// Desugared `strategy.*` order primitive. `kind` names the
    /// primitive; `operands` are the resolved argument expressions in a
    /// fixed, kind-specific slot layout (see `docs/ir.md` and the semantics
    /// analyzer). IDs, comments, alert messages, and OCA metadata are string
    /// operands; direction, prices, and quantities are numeric operands;
    /// immediate/alert switches are bool operands. Missing optional operands
    /// carry their defaults (`na`, `""`, `100`, or `false`). The
    /// MIR lowering evaluates every operand left to right and the VM broker
    /// loop consumes them.
    Order {
        kind: OrderKind,
        /// One expression per operand slot for `kind`, in slot order.
        operands: Vec<HirExpr>,
        span: Span,
    },
    /// A bare expression evaluated and discarded.
    Eval { expr: HirExpr, span: Span },
    If {
        cond: HirExpr,
        then_branch: Vec<HirStmt>,
        else_branch: Vec<HirStmt>,
        span: Span,
    },
    /// `for counter = start to end [by step]`. The `start` and `step`
    /// expressions are evaluated once at loop entry. `end` is evaluated
    /// once at entry to infer the direction of an implicit step, then again
    /// before each iteration to determine whether the loop continues. The
    /// initial `end` and signed `step` use hidden non-persistent slots
    /// (`end_slot`, `step_slot`); the current end is written into `end_slot`
    /// before each comparison. `counter` is a loop-local user variable stored
    /// into its own slot each iteration. `is_int` is true when start and step
    /// are int (the counter is int); a float end remains float for comparisons.
    /// `implicit_step` records that no `by` clause was written, so the step
    /// is `+1` when `start <= end` else `-1` (computed at entry from the
    /// captured bounds). The loop runs while the counter has not passed `end`
    /// inclusive, with direction taken from the step's sign.
    For {
        counter: SymbolId,
        start: HirExpr,
        end: HirExpr,
        step: HirExpr,
        end_slot: SeriesSlot,
        step_slot: SeriesSlot,
        is_int: bool,
        implicit_step: bool,
        body: Vec<HirStmt>,
        span: Span,
    },
    /// `while cond` with a bool condition, re-checked every iteration.
    While {
        cond: HirExpr,
        body: Vec<HirStmt>,
        span: Span,
    },
    /// `break` — exit the innermost enclosing loop.
    Break { span: Span },
    /// `continue` — jump to the next iteration of the innermost loop (the
    /// step for a `for`, the condition for a `while`).
    Continue { span: Span },
}

impl HirStmt {
    pub fn span(&self) -> Span {
        match self {
            HirStmt::Assign { span, .. }
            | HirStmt::Destructure { span, .. }
            | HirStmt::SetField { span, .. }
            | HirStmt::Plot { span, .. }
            | HirStmt::Visual { span, .. }
            | HirStmt::Fill { span, .. }
            | HirStmt::RuntimeError { span, .. }
            | HirStmt::Log { span, .. }
            | HirStmt::Alert { span, .. }
            | HirStmt::AlertCondition { span, .. }
            | HirStmt::Order { span, .. }
            | HirStmt::Eval { span, .. }
            | HirStmt::If { span, .. }
            | HirStmt::For { span, .. }
            | HirStmt::While { span, .. }
            | HirStmt::Break { span }
            | HirStmt::Continue { span } => *span,
        }
    }
}

/// Default historical-buffer ceiling used for Pine-style `max_bars_back`.
pub fn default_max_bars_back() -> u32 {
    500
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HirProgram {
    pub indicator: Option<IndicatorInfo>,
    /// The `strategy(...)` declaration, when the script is a strategy.
    /// Mutually exclusive with `indicator` (`E0240`).
    #[serde(default)]
    pub strategy: Option<StrategyInfo>,
    pub body: Vec<HirStmt>,
    /// Effective historical-buffer ceiling: the largest of
    /// `indicator(max_bars_back=...)`, top-level `max_bars_back(...)`, the
    /// deepest literal history offset, and the default. Semantics rejects a
    /// literal offset past the hard limit; dynamic offsets are checked by the
    /// VM against this carried value.
    #[serde(default = "default_max_bars_back")]
    pub max_bars_back: u32,
    /// All user variables, indexed by `SymbolId`.
    pub symbols: Vec<Symbol>,
    /// All plot call sites, indexed by `PlotSlot`.
    pub plots: Vec<PlotInfo>,
    /// Color-only chart visuals, indexed by `VisualSlot`.
    #[serde(default)]
    pub visuals: Vec<VisualInfo>,
    /// `fill(...)` bands between two plots, indexed by `FillSlot`.
    #[serde(default)]
    pub fills: Vec<FillInfo>,
    /// Engine-backed stateful call sites ([`HirExprKind::CallTaState`]),
    /// indexed by state id, allocated in syntactic order.
    pub ta_states: Vec<TaStateDecl>,
    /// Input-backed TA parameters resolved once before each run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ta_input_bindings: Vec<TaInputBinding>,
    /// `input.*` declaration sites ([`HirExprKind::ReadInput`]), indexed by
    /// input id, allocated in syntactic order.
    pub inputs: Vec<InputDecl>,
    /// User-defined-type layouts, indexed by `type_id`. Referenced by
    /// [`HirExprKind::NewObject`] / [`HirExprKind::CopyObject`]; the VM reads
    /// the field count to size a fresh heap object.
    pub object_types: Vec<ObjectTypeDecl>,
}

/// The runtime layout of one user-defined type: its name (for
/// diagnostics/dumps) and its field count in declaration order. Field values
/// are stored positionally on the heap; scalar type checking is done statically
/// by the checker, so the runtime only needs the arity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectTypeDecl {
    pub name: String,
    pub field_count: u32,
    /// Declared lane per field, indexed by `field_idx`. Empty means no claim
    /// for any field; otherwise there are exactly `field_count` entries, and
    /// a `None` entry means no claim for that field. Omitted from the
    /// serialized form when empty, so a program without declarations
    /// serializes exactly as it did before the field existed.
    ///
    /// A declaration is a claim, not a fact: nothing reads it until a checker
    /// has established it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub field_lanes: Vec<Option<Lane>>,
}

/// The runtime tag every non-na value stored in a slot or an object field
/// has. It describes the stored representation, not the Pine type: a place
/// that can hold the fractional result of an integer division has no lane,
/// although its Pine type is `int`.
///
/// Wire form (serde's externally tagged enum): the unit variants are the
/// strings `"F64"`, `"I64"`, `"Bool"`, `"Color"`, `"Str"` and `"Ref"`;
/// `Obj(n)` is the one-key object `{"Obj": n}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lane {
    /// A float cell.
    F64,
    /// An integer cell.
    I64,
    /// A boolean cell.
    Bool,
    /// A color cell.
    Color,
    /// A string cell.
    Str,
    /// A handle to an object of the user-defined type `type_id`
    /// (an index into `object_types`).
    Obj(u32),
    /// Any other heap handle (collection, drawing, footprint).
    Ref,
}

impl HirProgram {
    pub fn symbol(&self, id: SymbolId) -> &Symbol {
        &self.symbols[id.0 as usize]
    }

    pub fn series_slot_count(&self) -> u32 {
        self.symbols.len() as u32
    }

    pub fn plot_slot_count(&self) -> u32 {
        self.plots.len() as u32
    }
}

// ---------------------------------------------------------------------------
// Debug dump
// ---------------------------------------------------------------------------

/// Render a span for an IR dump comment: bare `start..end` for the main
/// buffer, `source:start..end` for an imported library (matching the CSIL and
/// MIR dumps, so hosts parse every stage's source map identically).
fn render_span(span: Span) -> String {
    if span.source.0 == 0 {
        format!("{}..{}", span.start, span.end)
    } else {
        format!("{}:{}..{}", span.source.0, span.start, span.end)
    }
}

/// Splice a `; span` comment in before the first newline at or after `from`.
/// A HIR node writes its own header line first, then recurses to write its
/// children; annotating up to the first newline tags only that own line, so
/// every node in the tree carries its own span.
fn splice_span(out: &mut String, from: usize, span: Span) {
    if let Some(rel) = out[from..].find('\n') {
        let at = from + rel;
        out.insert_str(at, &format!("  ; {}", render_span(span)));
    }
}

/// Human-readable HIR listing (span-free — the form snapshot tests pin).
pub fn dump_hir(program: &HirProgram) -> String {
    dump_hir_impl(program, false)
}

/// HIR listing with a trailing `; span` source map on every statement and
/// expression, for line-linked pipeline inspection.
pub fn dump_hir_annotated(program: &HirProgram) -> String {
    dump_hir_impl(program, true)
}

fn dump_hir_impl(program: &HirProgram, annotate: bool) -> String {
    let mut out = String::new();
    if let Some(info) = &program.strategy {
        let _ = writeln!(
            out,
            "HirProgram strategy={:?} initial_capital={}",
            info.title, info.initial_capital
        );
    }
    match &program.indicator {
        Some(info) => {
            let _ = writeln!(out, "HirProgram indicator={:?}", info.title);
        }
        None => {
            let _ = writeln!(out, "HirProgram indicator=<none>");
        }
    }
    for sym in &program.symbols {
        let suffix = match (sym.persistent, sym.synthetic) {
            (true, _) => ", var",
            (false, true) => ", synthetic",
            (false, false) => "",
        };
        let _ = writeln!(
            out,
            "  symbol {} `{}`: {} {} (slot {}{suffix})",
            sym.id.0,
            sym.name,
            sym.qualifier.describe(),
            sym.ty.describe(),
            sym.slot.0
        );
    }
    for plot in &program.plots {
        let _ = writeln!(
            out,
            "  plot slot {} title={:?}",
            plot.slot.0,
            plot.title.as_deref().unwrap_or("<none>")
        );
    }
    for (i, decl) in program.ta_states.iter().enumerate() {
        let _ = writeln!(
            out,
            "  ta state {i} {} params={:?}",
            decl.func.name(),
            decl.params
        );
    }
    for decl in &program.inputs {
        let _ = writeln!(
            out,
            "  input {} input.{} title={:?} defval={} {}",
            decl.id,
            decl.kind.describe(),
            decl.title.as_deref().unwrap_or("<none>"),
            render_input_value(&decl.defval),
            render_input_constraints(&decl.constraints),
        );
    }
    for stmt in &program.body {
        dump_stmt(&mut out, program, stmt, 1, annotate);
    }
    out
}

/// Render an [`InputValue`] for the HIR dump (kept stable across redeploys of
/// the snapshot suite).
fn render_input_value(value: &InputValue) -> String {
    match value {
        InputValue::Int(v) => v.to_string(),
        InputValue::Float(v) => format!("{v}"),
        InputValue::Bool(v) => v.to_string(),
        InputValue::String(v) => format!("{v:?}"),
        InputValue::Color { r, g, b, t } => {
            if *t == 0.0 {
                format!("#{r:02X}{g:02X}{b:02X}")
            } else {
                format!("#{r:02X}{g:02X}{b:02X} t={t}")
            }
        }
        InputValue::Source(series) => series.name().to_string(),
        InputValue::ExternalSource(source) => format!(
            "external({:?}, slot {}, {:?})",
            source.script_id, source.plot_slot, source.plot_label
        ),
        InputValue::Enum {
            name,
            index,
            member,
            title,
        } => {
            let identity = if member.is_empty() {
                format!("{name}#{index}")
            } else {
                format!("{name}.{member}#{index}")
            };
            match title {
                Some(title) => format!("{identity} title={title:?}"),
                None => identity,
            }
        }
    }
}

fn render_input_constraints(c: &InputConstraints) -> String {
    if c.is_empty() {
        return "constraints=none".to_string();
    }
    let mut parts = Vec::new();
    if let Some(v) = &c.min {
        parts.push(format!("min={}", render_input_value(v)));
    }
    if let Some(v) = &c.max {
        parts.push(format!("max={}", render_input_value(v)));
    }
    if let Some(v) = &c.step {
        parts.push(format!("step={}", render_input_value(v)));
    }
    if !c.options.is_empty() {
        let opts: Vec<String> = c.options.iter().map(render_input_value).collect();
        parts.push(format!("options=[{}]", opts.join(", ")));
    }
    format!("constraints={{{}}}", parts.join(", "))
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn dump_stmt(out: &mut String, program: &HirProgram, stmt: &HirStmt, depth: usize, annotate: bool) {
    // Statements write their own header line first, then recurse; capturing
    // the length here lets `splice_span` tag that header line (the first
    // newline) with the statement's span while children carry their own.
    let line_start = out.len();
    match stmt {
        HirStmt::Assign {
            symbol,
            value,
            init,
            ..
        } => {
            indent(out, depth);
            let sym = program.symbol(*symbol);
            let label = match (init, sym.persistent) {
                (false, _) => "Reassign",
                (true, true) => "VarAssign",
                (true, false) => "Assign",
            };
            let _ = writeln!(out, "{label} `{}` (sym {})", sym.name, symbol.0);
            splice_stmt(out, line_start, stmt, annotate);
            dump_expr(out, program, value, depth + 1, annotate);
        }
        HirStmt::Destructure { targets, value, .. } => {
            indent(out, depth);
            let names: Vec<String> = targets
                .iter()
                .map(|t| format!("`{}` (sym {})", program.symbol(*t).name, t.0))
                .collect();
            let _ = writeln!(out, "Destructure [{}]", names.join(", "));
            splice_stmt(out, line_start, stmt, annotate);
            dump_expr(out, program, value, depth + 1, annotate);
        }
        HirStmt::SetField {
            obj,
            field_idx,
            value,
            ..
        } => {
            indent(out, depth);
            let _ = writeln!(out, "SetField .{field_idx}");
            splice_stmt(out, line_start, stmt, annotate);
            dump_expr(out, program, obj, depth + 1, annotate);
            dump_expr(out, program, value, depth + 1, annotate);
        }
        HirStmt::Plot {
            slot,
            value,
            colors,
            ..
        } => {
            indent(out, depth);
            let _ = writeln!(out, "Plot slot {}", slot.0);
            splice_stmt(out, line_start, stmt, annotate);
            dump_expr(out, program, value, depth + 1, annotate);
            for (role, color) in colors {
                indent(out, depth + 1);
                let _ = writeln!(out, "Color {role:?}");
                dump_expr(out, program, color, depth + 2, annotate);
            }
        }
        HirStmt::Visual { slot, colors, .. } => {
            indent(out, depth);
            let _ = writeln!(out, "Visual slot {}", slot.0);
            splice_stmt(out, line_start, stmt, annotate);
            for (role, color) in colors {
                indent(out, depth + 1);
                let _ = writeln!(out, "Color {role:?}");
                dump_expr(out, program, color, depth + 2, annotate);
            }
        }
        HirStmt::Fill { slot, colors, .. } => {
            indent(out, depth);
            let _ = writeln!(out, "Fill slot {}", slot.0);
            splice_stmt(out, line_start, stmt, annotate);
            for (role, color) in colors {
                indent(out, depth + 1);
                let _ = writeln!(out, "Color {role:?}");
                dump_expr(out, program, color, depth + 2, annotate);
            }
        }
        HirStmt::RuntimeError { message, .. } => {
            indent(out, depth);
            out.push_str("RuntimeError\n");
            splice_stmt(out, line_start, stmt, annotate);
            dump_expr(out, program, message, depth + 1, annotate);
        }
        HirStmt::Log {
            level,
            format,
            args,
            ..
        } => {
            indent(out, depth);
            let _ = writeln!(out, "Log {}", level.member());
            splice_stmt(out, line_start, stmt, annotate);
            dump_expr(out, program, format, depth + 1, annotate);
            for a in args {
                dump_expr(out, program, a, depth + 1, annotate);
            }
        }
        HirStmt::Alert { message, freq, .. } => {
            indent(out, depth);
            out.push_str("Alert\n");
            splice_stmt(out, line_start, stmt, annotate);
            dump_expr(out, program, message, depth + 1, annotate);
            dump_expr(out, program, freq, depth + 1, annotate);
        }
        HirStmt::AlertCondition {
            condition,
            title,
            message,
            ..
        } => {
            indent(out, depth);
            out.push_str("AlertCondition\n");
            splice_stmt(out, line_start, stmt, annotate);
            dump_expr(out, program, condition, depth + 1, annotate);
            dump_expr(out, program, title, depth + 1, annotate);
            dump_expr(out, program, message, depth + 1, annotate);
        }
        HirStmt::Order { kind, operands, .. } => {
            indent(out, depth);
            let _ = writeln!(out, "Order strategy.{}", kind.member());
            splice_stmt(out, line_start, stmt, annotate);
            for op in operands {
                dump_expr(out, program, op, depth + 1, annotate);
            }
        }
        HirStmt::Eval { expr, .. } => {
            indent(out, depth);
            out.push_str("Eval\n");
            splice_stmt(out, line_start, stmt, annotate);
            dump_expr(out, program, expr, depth + 1, annotate);
        }
        HirStmt::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            indent(out, depth);
            out.push_str("If\n");
            splice_stmt(out, line_start, stmt, annotate);
            indent(out, depth + 1);
            out.push_str("Cond\n");
            dump_expr(out, program, cond, depth + 2, annotate);
            indent(out, depth + 1);
            out.push_str("Then\n");
            for s in then_branch {
                dump_stmt(out, program, s, depth + 2, annotate);
            }
            if !else_branch.is_empty() {
                indent(out, depth + 1);
                out.push_str("Else\n");
                for s in else_branch {
                    dump_stmt(out, program, s, depth + 2, annotate);
                }
            }
        }
        HirStmt::For {
            counter,
            start,
            end,
            step,
            end_slot,
            step_slot,
            is_int,
            implicit_step,
            body,
            ..
        } => {
            indent(out, depth);
            let sym = program.symbol(*counter);
            let flavor = if *is_int { "int" } else { "float" };
            let step_note = if *implicit_step { " implicit" } else { "" };
            let _ = writeln!(
                out,
                "For `{}` (sym {}) {flavor} end_slot{} step_slot{}{step_note}",
                sym.name, counter.0, end_slot.0, step_slot.0
            );
            splice_stmt(out, line_start, stmt, annotate);
            indent(out, depth + 1);
            out.push_str("Start\n");
            dump_expr(out, program, start, depth + 2, annotate);
            indent(out, depth + 1);
            out.push_str("End\n");
            dump_expr(out, program, end, depth + 2, annotate);
            indent(out, depth + 1);
            out.push_str("Step\n");
            dump_expr(out, program, step, depth + 2, annotate);
            indent(out, depth + 1);
            out.push_str("Body\n");
            for s in body {
                dump_stmt(out, program, s, depth + 2, annotate);
            }
        }
        HirStmt::While { cond, body, .. } => {
            indent(out, depth);
            out.push_str("While\n");
            splice_stmt(out, line_start, stmt, annotate);
            indent(out, depth + 1);
            out.push_str("Cond\n");
            dump_expr(out, program, cond, depth + 2, annotate);
            indent(out, depth + 1);
            out.push_str("Body\n");
            for s in body {
                dump_stmt(out, program, s, depth + 2, annotate);
            }
        }
        HirStmt::Break { .. } => {
            indent(out, depth);
            out.push_str("Break\n");
            splice_stmt(out, line_start, stmt, annotate);
        }
        HirStmt::Continue { .. } => {
            indent(out, depth);
            out.push_str("Continue\n");
            splice_stmt(out, line_start, stmt, annotate);
        }
    }
}

/// Append the statement's own span to the header line just written.
fn splice_stmt(out: &mut String, line_start: usize, stmt: &HirStmt, annotate: bool) {
    if annotate {
        splice_span(out, line_start, stmt.span());
    }
}

fn dump_expr(out: &mut String, program: &HirProgram, expr: &HirExpr, depth: usize, annotate: bool) {
    indent(out, depth);
    let line_start = out.len();
    let ty = format!("{} {}", expr.qualifier.describe(), expr.ty.describe());
    dump_expr_kind(out, program, expr, depth, &ty, annotate);
    if annotate {
        splice_span(out, line_start, expr.span);
    }
}

fn dump_expr_kind(
    out: &mut String,
    program: &HirProgram,
    expr: &HirExpr,
    depth: usize,
    ty: &str,
    annotate: bool,
) {
    match &expr.kind {
        HirExprKind::ConstInt(v) => {
            let _ = writeln!(out, "ConstInt {v} : {ty}");
        }
        HirExprKind::ConstFloat(v) => {
            let _ = writeln!(out, "ConstFloat {v} : {ty}");
        }
        HirExprKind::ConstBool(v) => {
            let _ = writeln!(out, "ConstBool {v} : {ty}");
        }
        HirExprKind::ConstColor { r, g, b, t } => {
            // Inline hex form (HIR must not depend on the MIR value crate).
            if *t == 0.0 {
                let _ = writeln!(out, "ConstColor #{r:02X}{g:02X}{b:02X} : {ty}");
            } else {
                let _ = writeln!(out, "ConstColor #{r:02X}{g:02X}{b:02X} t={t} : {ty}");
            }
        }
        HirExprKind::ConstString(v) => {
            let _ = writeln!(out, "ConstString {v:?} : {ty}");
        }
        HirExprKind::ConstNa => {
            let _ = writeln!(out, "ConstNa : {ty}");
        }
        HirExprKind::ReadBuiltin { series, offset } => {
            let _ = writeln!(out, "ReadBuiltin {}[{offset}] : {ty}", series.name());
        }
        HirExprKind::ReadVar { symbol, offset } => {
            let name = &program.symbol(*symbol).name;
            let _ = writeln!(out, "ReadVar `{name}`[{offset}] (sym {}) : {ty}", symbol.0);
        }
        HirExprKind::ReadBuiltinDynamic { series, offset } => {
            let _ = writeln!(out, "ReadBuiltinDynamic {}[expr] : {ty}", series.name());
            dump_expr(out, program, offset, depth + 1, annotate);
        }
        HirExprKind::ReadVarDynamic { symbol, offset } => {
            let name = &program.symbol(*symbol).name;
            let _ = writeln!(
                out,
                "ReadVarDynamic `{name}`[expr] (sym {}) : {ty}",
                symbol.0
            );
            dump_expr(out, program, offset, depth + 1, annotate);
        }
        HirExprKind::ReadInput { input } => {
            let _ = writeln!(out, "ReadInput input{input} : {ty}");
        }
        HirExprKind::ReadSecurity {
            selector,
            source,
            htf_seconds,
            calendar_months,
            gaps_on,
            lookahead_on,
            ..
        } => {
            let _ = writeln!(
                out,
                "ReadSecurity {}{}[{htf_seconds}s, months={calendar_months:?}] gaps={} lookahead={} : {ty}",
                selector
                    .as_ref()
                    .map(|s| format!("{s} "))
                    .unwrap_or_default(),
                source.name(),
                if *gaps_on { "on" } else { "off" },
                if *lookahead_on { "on" } else { "off" },
            );
        }
        HirExprKind::ReadSecurityExpr {
            htf_seconds,
            gaps_on,
            lookahead_on,
            ..
        } => {
            let _ = writeln!(
                out,
                "ReadSecurityExpr [{htf_seconds}s] gaps={} lookahead={} : {ty}",
                if *gaps_on { "on" } else { "off" },
                if *lookahead_on { "on" } else { "off" },
            );
        }
        HirExprKind::ReadRequest { func, selector } => {
            let _ = writeln!(out, "ReadRequest {}[{selector}] : {ty}", func.member());
        }
        HirExprKind::BuildFootprint { selector } => {
            let _ = writeln!(out, "BuildFootprint [{selector}] : {ty}");
        }
        HirExprKind::ReadSecurityLowerTf { selector } => {
            let _ = writeln!(out, "ReadSecurityLowerTf [{selector}] : {ty}");
        }
        HirExprKind::ReadPivotLevels {
            system,
            anchor_seconds,
        } => {
            let _ = writeln!(
                out,
                "ReadPivotLevels {} [{anchor_seconds}s] : {ty}",
                system.type_name(),
            );
        }
        HirExprKind::CallPivotPointLevels { args } => {
            let _ = writeln!(out, "CallPivotPointLevels argc={} : {ty}", args.len());
            for arg in args {
                dump_expr(out, program, arg, depth + 1, annotate);
            }
        }
        HirExprKind::ReadTimeTf { close, tf_seconds } => {
            let _ = writeln!(
                out,
                "ReadTimeTf {} [{tf_seconds}s] : {ty}",
                if *close { "close" } else { "open" },
            );
        }
        HirExprKind::FootprintRowByPrice { footprint, price } => {
            let _ = writeln!(out, "FootprintRowByPrice : {ty}");
            dump_expr(out, program, footprint, depth + 1, annotate);
            dump_expr(out, program, price, depth + 1, annotate);
        }
        HirExprKind::Unary { op, operand } => {
            let _ = writeln!(out, "Unary `{}` : {ty}", op.symbol());
            dump_expr(out, program, operand, depth + 1, annotate);
        }
        HirExprKind::Binary { op, lhs, rhs } => {
            let _ = writeln!(out, "Binary `{}` : {ty}", op.symbol());
            dump_expr(out, program, lhs, depth + 1, annotate);
            dump_expr(out, program, rhs, depth + 1, annotate);
        }
        HirExprKind::Ternary {
            cond,
            then_e,
            else_e,
        } => {
            let _ = writeln!(out, "Ternary : {ty}");
            dump_expr(out, program, cond, depth + 1, annotate);
            dump_expr(out, program, then_e, depth + 1, annotate);
            dump_expr(out, program, else_e, depth + 1, annotate);
        }
        HirExprKind::IntToFloat(operand) => {
            let _ = writeln!(out, "IntToFloat : {ty}");
            dump_expr(out, program, operand, depth + 1, annotate);
        }
        HirExprKind::EvalSequence { bindings, result } => {
            let _ = writeln!(out, "EvalSequence : {ty}");
            for (slot, value) in bindings {
                indent(out, depth + 1);
                let _ = writeln!(out, "Capture slot{}", slot.0);
                dump_expr(out, program, value, depth + 2, annotate);
            }
            dump_expr(out, program, result, depth + 1, annotate);
        }
        HirExprKind::MaterializeHistory { slot, expr, offset } => {
            let _ = writeln!(out, "MaterializeHistory slot{}[{offset}] : {ty}", slot.0);
            dump_expr(out, program, expr, depth + 1, annotate);
        }
        HirExprKind::MaterializeHistoryDynamic { slot, expr, offset } => {
            let _ = writeln!(out, "MaterializeHistoryDynamic slot{}[expr] : {ty}", slot.0);
            dump_expr(out, program, expr, depth + 1, annotate);
            dump_expr(out, program, offset, depth + 1, annotate);
        }
        HirExprKind::TaCall {
            func,
            slot,
            source,
            length,
        } => {
            let _ = writeln!(
                out,
                "TaCall {} slot{} len={length} : {ty}",
                func.name(),
                slot.0
            );
            dump_expr(out, program, source, depth + 1, annotate);
        }
        HirExprKind::TaCallDynamic {
            func,
            slot,
            source,
            length,
        } => {
            let _ = writeln!(out, "TaCallDynamic {} slot{} : {ty}", func.name(), slot.0);
            dump_expr(out, program, source, depth + 1, annotate);
            dump_expr(out, program, length, depth + 1, annotate);
        }
        HirExprKind::CallTaState { func, state, args } => {
            let _ = writeln!(
                out,
                "CallTaState {} state{state} params={:?} : {ty}",
                func.name(),
                program.ta_states[*state as usize].params
            );
            for arg in args {
                dump_expr(out, program, arg, depth + 1, annotate);
            }
        }
        HirExprKind::CallPure { func, args } => {
            let _ = writeln!(out, "CallPure {} argc={} : {ty}", func.name(), args.len());
            for arg in args {
                dump_expr(out, program, arg, depth + 1, annotate);
            }
        }
        HirExprKind::CallCollection { func, args } => {
            let _ = writeln!(
                out,
                "CallCollection {} argc={} : {ty}",
                func.name(),
                args.len()
            );
            for arg in args {
                dump_expr(out, program, arg, depth + 1, annotate);
            }
        }
        HirExprKind::NewObject { type_id, fields } => {
            let _ = writeln!(
                out,
                "NewObject {} argc={} : {ty}",
                program.object_types[*type_id as usize].name,
                fields.len()
            );
            for field in fields {
                dump_expr(out, program, field, depth + 1, annotate);
            }
        }
        HirExprKind::CopyObject { type_id, obj } => {
            let _ = writeln!(
                out,
                "CopyObject {} : {ty}",
                program.object_types[*type_id as usize].name
            );
            dump_expr(out, program, obj, depth + 1, annotate);
        }
        HirExprKind::GetField { obj, field_idx } => {
            let _ = writeln!(out, "GetField .{field_idx} : {ty}");
            dump_expr(out, program, obj, depth + 1, annotate);
        }
        HirExprKind::Tuple { elements } => {
            let _ = writeln!(out, "Tuple arity={} : {ty}", elements.len());
            for element in elements {
                dump_expr(out, program, element, depth + 1, annotate);
            }
        }
        HirExprKind::RegisterDrawing { kind, inner } => {
            let _ = writeln!(out, "RegisterDrawing {} : {ty}", kind.namespace());
            dump_expr(out, program, inner, depth + 1, annotate);
        }
        HirExprKind::DeleteDrawing { kind, id } => {
            let _ = writeln!(out, "DeleteDrawing {} : {ty}", kind.namespace());
            dump_expr(out, program, id, depth + 1, annotate);
        }
        HirExprKind::SetFieldExpr {
            obj,
            field_idx,
            value,
        } => {
            let _ = writeln!(out, "SetFieldExpr .{field_idx} : {ty}");
            dump_expr(out, program, obj, depth + 1, annotate);
            dump_expr(out, program, value, depth + 1, annotate);
        }
        HirExprKind::SetFields {
            obj,
            recv_slot,
            writes,
        } => {
            let idxs: Vec<String> = writes.iter().map(|(i, _)| format!(".{i}")).collect();
            let _ = writeln!(
                out,
                "SetFields %{} [{}] : {ty}",
                recv_slot.0,
                idxs.join(",")
            );
            dump_expr(out, program, obj, depth + 1, annotate);
            for (_, v) in writes {
                dump_expr(out, program, v, depth + 1, annotate);
            }
        }
        HirExprKind::LineGetPrice { obj, recv_slot, x } => {
            let _ = writeln!(out, "LineGetPrice %{} : {ty}", recv_slot.0);
            dump_expr(out, program, obj, depth + 1, annotate);
            dump_expr(out, program, x, depth + 1, annotate);
        }
        HirExprKind::ReadDrawingsAll { kind } => {
            let _ = writeln!(out, "ReadDrawingsAll {} : {ty}", kind.namespace());
        }
        HirExprKind::SetCell {
            obj,
            recv_slot,
            column,
            row,
            writes,
        } => {
            let attrs = writes
                .iter()
                .map(|(a, _)| a.name())
                .collect::<Vec<_>>()
                .join(",");
            let _ = writeln!(out, "SetCell %{} [{attrs}] : {ty}", recv_slot.0);
            dump_expr(out, program, obj, depth + 1, annotate);
            dump_expr(out, program, column, depth + 1, annotate);
            dump_expr(out, program, row, depth + 1, annotate);
            for (_, value) in writes {
                dump_expr(out, program, value, depth + 1, annotate);
            }
        }
        HirExprKind::ClearCells {
            obj,
            recv_slot,
            start_column,
            start_row,
            end_column,
            end_row,
        } => {
            let _ = writeln!(out, "ClearCells %{} : {ty}", recv_slot.0);
            dump_expr(out, program, obj, depth + 1, annotate);
            dump_expr(out, program, start_column, depth + 1, annotate);
            dump_expr(out, program, start_row, depth + 1, annotate);
            dump_expr(out, program, end_column, depth + 1, annotate);
            dump_expr(out, program, end_row, depth + 1, annotate);
        }
        HirExprKind::MergeCells {
            obj,
            recv_slot,
            start_column,
            start_row,
            end_column,
            end_row,
        } => {
            let _ = writeln!(out, "MergeCells %{} : {ty}", recv_slot.0);
            dump_expr(out, program, obj, depth + 1, annotate);
            dump_expr(out, program, start_column, depth + 1, annotate);
            dump_expr(out, program, start_row, depth + 1, annotate);
            dump_expr(out, program, end_column, depth + 1, annotate);
            dump_expr(out, program, end_row, depth + 1, annotate);
        }
    }
}

/// Serialization shared by color declarations and bytecode constants.
pub fn serialize_transparency<S: serde::Serializer>(
    value: &f64,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    if value.is_finite() && (0.0..=100.0).contains(value) && value.fract() == 0.0 {
        serializer.serialize_u8(*value as u8)
    } else {
        serializer.serialize_f64(*value)
    }
}

#[cfg(test)]
mod qualifier_join_tests {
    use super::{qualifier_join, qualifier_rank, Qualifier};

    const ALL: [Qualifier; 4] = [
        Qualifier::Const,
        Qualifier::Input,
        Qualifier::Simple,
        Qualifier::Series,
    ];

    /// The kernel is `Ord::max` under the derived order, on every pair.
    #[test]
    fn join_matches_derived_max() {
        for a in ALL {
            for b in ALL {
                assert_eq!(qualifier_join(a, b), a.max(b), "{a:?} {b:?}");
                assert_eq!(a.join(b), a.max(b), "{a:?} {b:?}");
            }
        }
    }

    /// `qualifier_rank` is the derived order.
    #[test]
    fn rank_matches_derived_order() {
        for a in ALL {
            for b in ALL {
                assert_eq!(
                    qualifier_rank(a).cmp(&qualifier_rank(b)),
                    a.cmp(&b),
                    "{a:?} {b:?}"
                );
            }
        }
    }
}
