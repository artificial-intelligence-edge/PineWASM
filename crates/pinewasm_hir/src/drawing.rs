//! Drawing-object schema: the single source of truth for the
//! stateful drawing namespaces (`chart.point`, `line`, `label`, `box`, and the
//! other kinds in [`DrawKind`]).
//!
//! A drawing object is a **heap reference object** — it reuses the UDT object
//! machinery (heap-program-agnostic `HeapObj::Object` values): each drawing
//! kind is a synthetic object type with a fixed field layout, so `.new`
//! constructs a `HeapObj::Object`, setters/getters are ordinary field
//! writes/reads, and `.copy` shallow-copies. What the plain UDT path does *not*
//! give us is (a) a per-kind **live registry** with a bounded max count and
//! deterministic oldest-first GC, and (b) the serialized `drawings` section of
//! the run outcome. Those two are the only genuinely new VM mechanisms; they
//! are driven by the [`DrawKind`] tag carried on the `RegisterDrawing` /
//! `DeleteDrawing` CSIL ops.
//!
//! This module is data only: it names each kind, its ordered fields (with the
//! scalar [`Type`] the checker enforces and the field's default), and the
//! member functions (`<ns>.new`, setters, getters, `.copy`, `.delete`). The
//! registry, checker, CSIL lowering, and VM all read this table so the schema
//! stays in one place.

use crate::{Qualifier, Type};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

/// The stateful drawing-object kinds. The discriminant is stable (serialized on
/// the CSIL ops and used to key the VM's per-kind live registry). `chart.point`
/// is a data-carrying reference object but is **not** registered/GC'd (it has
/// no on-chart lifetime of its own — it is a coordinate passed into line/label
/// constructors), so it has no `max`-count and never appears in the `drawings`
/// DTO.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DrawKind {
    /// `chart.point` — an (index, time, price) coordinate. Reference object,
    /// not GC-tracked.
    Point,
    /// `line.*` — a segment between two chart points.
    Line,
    /// `label.*` — a text marker at a chart point.
    Label,
    /// `box.*` — a rectangle between two corners.
    Box,
    /// `table.*` — a grid of cells anchored to a chart position. The cell grid
    /// itself is stored out-of-line in the VM (keyed by handle); the object's
    /// declared fields cover the table-level attributes (position, colors,
    /// frame/border widths, dimensions).
    Table,
    /// `polyline.*` — a multi-point path over an `array<chart.point>`.
    Polyline,
    /// `linefill.*` — a shaded fill between two existing `line` objects.
    Linefill,
}

impl DrawKind {
    /// The namespace prefix (`line`, `label`, `box`, `chart.point`).
    pub fn namespace(self) -> &'static str {
        match self {
            DrawKind::Point => "chart.point",
            DrawKind::Line => "line",
            DrawKind::Label => "label",
            DrawKind::Box => "box",
            DrawKind::Table => "table",
            DrawKind::Polyline => "polyline",
            DrawKind::Linefill => "linefill",
        }
    }

    /// The synthetic object-type name used when this kind is registered as a
    /// UDT (so `Type::Object(type_name)` describes a drawing value). Chosen to
    /// not collide with any user type name (dots are not legal in user type
    /// names).
    pub fn type_name(self) -> &'static str {
        match self {
            DrawKind::Point => "chart.point",
            DrawKind::Line => "line",
            DrawKind::Label => "label",
            DrawKind::Box => "box",
            DrawKind::Table => "table",
            DrawKind::Polyline => "polyline",
            DrawKind::Linefill => "linefill",
        }
    }

    /// A stable single-token tag for the textual CSIL form (no dots, so the CSIL
    /// text stays whitespace-tokenizable). The inverse is [`DrawKind::from_tag`].
    pub fn text_tag(self) -> &'static str {
        match self {
            DrawKind::Point => "point",
            DrawKind::Line => "line",
            DrawKind::Label => "label",
            DrawKind::Box => "box",
            DrawKind::Table => "table",
            DrawKind::Polyline => "polyline",
            DrawKind::Linefill => "linefill",
        }
    }

    /// Parse a [`DrawKind::text_tag`].
    pub fn from_tag(tag: &str) -> Option<DrawKind> {
        Some(match tag {
            "point" => DrawKind::Point,
            "line" => DrawKind::Line,
            "label" => DrawKind::Label,
            "box" => DrawKind::Box,
            "table" => DrawKind::Table,
            "polyline" => DrawKind::Polyline,
            "linefill" => DrawKind::Linefill,
            _ => return None,
        })
    }

    /// The `DrawKind` for a drawing-type cast builtin name (`line`, `label`,
    /// `box`, `table`, `linefill`), or `None`. `chart.point`/`polyline` have no
    /// cast function, so they are excluded.
    pub fn from_cast_name(name: &str) -> Option<DrawKind> {
        Some(match name {
            "line" => DrawKind::Line,
            "label" => DrawKind::Label,
            "box" => DrawKind::Box,
            "table" => DrawKind::Table,
            "linefill" => DrawKind::Linefill,
            _ => return None,
        })
    }

    /// The kind produced by a named drawing-array shorthand constructor.
    pub fn from_array_constructor_name(name: &str) -> Option<DrawKind> {
        Some(match name {
            "array.new_line" => DrawKind::Line,
            "array.new_label" => DrawKind::Label,
            "array.new_box" => DrawKind::Box,
            "array.new_table" => DrawKind::Table,
            "array.new_linefill" => DrawKind::Linefill,
            _ => return None,
        })
    }

    /// Whether this kind participates in the live registry + `drawings` DTO +
    /// oldest-first GC. `chart.point` does not (it is a passed-in coordinate).
    pub fn is_registered(self) -> bool {
        !matches!(self, DrawKind::Point)
    }

    /// The kind's default max live-object count, overridable by
    /// the matching `indicator(max_*_count=)` metadata.
    pub fn default_max(self) -> usize {
        match self {
            // Tables are anchored by position: registration replaces a table at
            // the same position, and there is no separate oldest-first
            // table-count cap.
            DrawKind::Table => usize::MAX,
            // Default live-object cap for these kinds is 50; indicator metadata
            // can override it.
            _ => 50,
        }
    }

    /// The ordered fields (declaration/slot order) of this kind's object.
    pub fn fields(self) -> &'static [DrawField] {
        match self {
            DrawKind::Point => POINT_FIELDS,
            DrawKind::Line => LINE_FIELDS,
            DrawKind::Label => LABEL_FIELDS,
            DrawKind::Box => BOX_FIELDS,
            DrawKind::Table => TABLE_FIELDS,
            DrawKind::Polyline => POLYLINE_FIELDS,
            DrawKind::Linefill => LINEFILL_FIELDS,
        }
    }

    /// Field slot index by name, or `None`.
    pub fn field_index(self, name: &str) -> Option<u32> {
        self.fields()
            .iter()
            .position(|f| f.name == name)
            .map(|i| i as u32)
    }

    /// Every drawing kind, in stable order.
    pub fn all() -> &'static [DrawKind] {
        &[
            DrawKind::Point,
            DrawKind::Line,
            DrawKind::Label,
            DrawKind::Box,
            DrawKind::Table,
            DrawKind::Polyline,
            DrawKind::Linefill,
        ]
    }
}

/// One field of a drawing object: its member name, type (the checker enforces
/// argument/return typing against it), and default value used when a
/// constructor omits the corresponding named argument.
#[derive(Debug, Clone)]
pub struct DrawField {
    pub name: &'static str,
    pub ty: DrawFieldTy,
    pub default: DrawDefault,
}

impl DrawField {
    /// Materialize the field's declared [`Type`]. Owned because reference-typed
    /// fields (`array<chart.point>`, `line` handles) need heap allocation and so
    /// cannot be `const`.
    pub fn ty(&self) -> Type {
        self.ty.to_type()
    }

    /// Whether this field gets generic `set_<field>` / `get_<field>` members.
    pub fn has_accessors(&self) -> bool {
        self.name != "force_overlay"
    }
}

/// One source-level parameter on a drawing special-analyzer call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawingParameterSpec {
    pub name: String,
    pub ty: Type,
    pub required: bool,
    /// A qualifier restriction that narrows the normal qualifier lattice for
    /// this parameter (for example, `force_overlay` accepts only `const`).
    pub required_qualifier: Option<Qualifier>,
}

/// One drawing callable and every overload accepted by the semantic analyzer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawingFunctionSpec {
    pub name: String,
    pub overloads: Vec<Vec<DrawingParameterSpec>>,
    pub ret: Type,
    pub return_qualifier: Option<DrawingReturnQualifier>,
}

/// How a drawing call's result qualifier is computed by the semantic
/// analyzer. Drawing identities and mutable-state getters are series-qualified;
/// `chart.point` values retain the argument-dependent qualifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawingReturnQualifier {
    Fixed(Qualifier),
    JoinArgumentsAtLeastSimple,
}

/// One drawing value special-cased before the builtin-series registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawingSeriesSpec {
    pub name: String,
    pub ty: Type,
    pub qualifier: Qualifier,
}

fn drawing_param(name: &str, ty: Type, required: bool) -> DrawingParameterSpec {
    DrawingParameterSpec {
        name: name.to_string(),
        ty,
        required,
        required_qualifier: None,
    }
}

fn drawing_const_param(name: &str, ty: Type, required: bool) -> DrawingParameterSpec {
    DrawingParameterSpec {
        required_qualifier: Some(Qualifier::Const),
        ..drawing_param(name, ty, required)
    }
}

fn drawing_object(kind: DrawKind) -> Type {
    Type::Object(kind.type_name().to_string())
}

fn drawing_spec(
    name: String,
    overloads: Vec<Vec<DrawingParameterSpec>>,
    ret: Type,
) -> DrawingFunctionSpec {
    let return_qualifier =
        (ret != Type::Void).then_some(DrawingReturnQualifier::Fixed(Qualifier::Series));
    DrawingFunctionSpec {
        name,
        overloads,
        ret,
        return_qualifier,
    }
}

fn drawing_constructor_spec(
    name: String,
    overloads: Vec<Vec<DrawingParameterSpec>>,
    ret: Type,
) -> DrawingFunctionSpec {
    let return_qualifier = Some(if name.starts_with("chart.point.") {
        DrawingReturnQualifier::JoinArgumentsAtLeastSimple
    } else {
        DrawingReturnQualifier::Fixed(Qualifier::Series)
    });
    DrawingFunctionSpec {
        name,
        overloads,
        ret,
        return_qualifier,
    }
}

fn pointwise_constructor(kind: DrawKind) -> Option<Vec<DrawingParameterSpec>> {
    let point = drawing_object(DrawKind::Point);
    let (leading, tail): (&[&str], &[&str]) = match kind {
        DrawKind::Line => (
            &["first_point", "second_point"],
            &["xloc", "extend", "color", "style", "width", "force_overlay"],
        ),
        DrawKind::Label => (
            &["point"],
            &[
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
                "text_formatting",
            ],
        ),
        DrawKind::Box => (
            &["top_left", "bottom_right"],
            &[
                "border_color",
                "border_width",
                "border_style",
                "extend",
                "xloc",
                "bgcolor",
                "text",
                "text_size",
                "text_color",
                "text_halign",
                "text_valign",
                "text_wrap",
                "text_font_family",
                "force_overlay",
                "text_formatting",
            ],
        ),
        _ => return None,
    };
    let mut params: Vec<_> = leading
        .iter()
        .map(|name| drawing_param(name, point.clone(), true))
        .collect();
    params.extend(tail.iter().map(|name| {
        let field = kind
            .fields()
            .iter()
            .find(|field| field.name == *name)
            .expect("pointwise constructor tail names a drawing field");
        if *name == "force_overlay" {
            drawing_const_param(name, field.ty(), false)
        } else {
            drawing_param(name, field.ty(), false)
        }
    }));
    Some(params)
}

/// Drawing function table: every drawing callable and its accepted overloads.
///
/// The semantic analyzer consumes this as its allowlist, while language-tooling
/// clients can consume the same names, overloads, parameter types, and result
/// types. Most members derive mechanically from [`DrawKind::fields`];
/// only compound coordinate and table-cell operations are enumerated here.
fn build_drawing_function_surface() -> Vec<DrawingFunctionSpec> {
    let mut specs = Vec::new();
    for &kind in DrawKind::all() {
        let object = drawing_object(kind);
        let object_id_parameter = if kind == DrawKind::Table {
            "table_id"
        } else {
            "id"
        };
        let coordinate_params = kind
            .fields()
            .iter()
            .map(|field| {
                let required = match kind {
                    DrawKind::Point => true,
                    DrawKind::Line => matches!(field.name, "x1" | "y1" | "x2" | "y2"),
                    DrawKind::Label => matches!(field.name, "x" | "y"),
                    DrawKind::Box => matches!(field.name, "left" | "top" | "right" | "bottom"),
                    DrawKind::Table => matches!(field.name, "position" | "columns" | "rows"),
                    DrawKind::Polyline => field.name == "points",
                    DrawKind::Linefill => matches!(field.name, "line1" | "line2" | "color"),
                };
                if field.name == "force_overlay" {
                    drawing_const_param(field.name, field.ty(), required)
                } else {
                    drawing_param(field.name, field.ty(), required)
                }
            })
            .collect();
        let mut constructor_overloads = vec![coordinate_params];
        if let Some(pointwise) = pointwise_constructor(kind) {
            constructor_overloads.push(pointwise);
        }
        specs.push(drawing_constructor_spec(
            format!("{}.new", kind.namespace()),
            constructor_overloads,
            object.clone(),
        ));
        specs.push(drawing_constructor_spec(
            format!("{}.copy", kind.namespace()),
            vec![vec![drawing_param(
                object_id_parameter,
                object.clone(),
                true,
            )]],
            object.clone(),
        ));
        if kind.is_registered() {
            specs.push(drawing_spec(
                format!("{}.delete", kind.namespace()),
                vec![vec![drawing_param(
                    object_id_parameter,
                    object.clone(),
                    true,
                )]],
                Type::Void,
            ));
        }

        for field in kind.fields().iter().filter(|field| field.has_accessors()) {
            let compound_setter = field.name == "xloc"
                && matches!(kind, DrawKind::Line | DrawKind::Label | DrawKind::Box);
            if !compound_setter {
                let parameter_name = match (kind, field.name) {
                    (DrawKind::Box, "border_color" | "bgcolor") => "color",
                    (DrawKind::Box, "border_width") => "width",
                    (DrawKind::Box, "border_style") => "style",
                    (DrawKind::Line, "x1" | "x2") => "x",
                    (DrawKind::Line, "y1" | "y2") => "y",
                    _ => field.name,
                };
                let value_required = !(kind == DrawKind::Table
                    && matches!(
                        field.name,
                        "bgcolor" | "frame_color" | "frame_width" | "border_color" | "border_width"
                    ));
                specs.push(drawing_spec(
                    format!("{}.set_{}", kind.namespace(), field.name),
                    vec![vec![
                        drawing_param(object_id_parameter, object.clone(), true),
                        drawing_param(parameter_name, field.ty(), value_required),
                    ]],
                    Type::Void,
                ));
            }
            specs.push(drawing_spec(
                format!("{}.get_{}", kind.namespace(), field.name),
                vec![vec![drawing_param(
                    object_id_parameter,
                    object.clone(),
                    true,
                )]],
                field.ty(),
            ));
        }
    }

    let point = drawing_object(DrawKind::Point);
    for (member, params) in [
        (
            "from_index",
            vec![
                drawing_param("index", Type::Int, true),
                drawing_param("price", Type::Float, true),
            ],
        ),
        (
            "from_time",
            vec![
                drawing_param("time", Type::Int, true),
                drawing_param("price", Type::Float, true),
            ],
        ),
        ("now", vec![drawing_param("price", Type::Float, false)]),
    ] {
        specs.push(drawing_constructor_spec(
            format!("chart.point.{member}"),
            vec![params],
            point.clone(),
        ));
    }

    let line = drawing_object(DrawKind::Line);
    let label = drawing_object(DrawKind::Label);
    let box_ty = drawing_object(DrawKind::Box);
    for (name, params) in [
        (
            "line.set_xy1",
            vec![
                drawing_param("id", line.clone(), true),
                drawing_param("x", Type::Int, true),
                drawing_param("y", Type::Float, true),
            ],
        ),
        (
            "line.set_xy2",
            vec![
                drawing_param("id", line.clone(), true),
                drawing_param("x", Type::Int, true),
                drawing_param("y", Type::Float, true),
            ],
        ),
        (
            "label.set_xy",
            vec![
                drawing_param("id", label.clone(), true),
                drawing_param("x", Type::Int, true),
                drawing_param("y", Type::Float, true),
            ],
        ),
        (
            "box.set_lefttop",
            vec![
                drawing_param("id", box_ty.clone(), true),
                drawing_param("left", Type::Int, true),
                drawing_param("top", Type::Float, true),
            ],
        ),
        (
            "box.set_rightbottom",
            vec![
                drawing_param("id", box_ty.clone(), true),
                drawing_param("right", Type::Int, true),
                drawing_param("bottom", Type::Float, true),
            ],
        ),
        (
            "line.set_xloc",
            vec![
                drawing_param("id", line.clone(), true),
                drawing_param("x1", Type::Int, true),
                drawing_param("x2", Type::Int, true),
                drawing_param("xloc", Type::String, true),
            ],
        ),
        (
            "label.set_xloc",
            vec![
                drawing_param("id", label.clone(), true),
                drawing_param("x", Type::Int, true),
                drawing_param("xloc", Type::String, true),
            ],
        ),
        (
            "box.set_xloc",
            vec![
                drawing_param("id", box_ty.clone(), true),
                drawing_param("left", Type::Int, true),
                drawing_param("right", Type::Int, true),
                drawing_param("xloc", Type::String, true),
            ],
        ),
    ] {
        specs.push(drawing_spec(name.to_string(), vec![params], Type::Void));
    }
    for (name, receiver) in [
        ("line.set_first_point", line.clone()),
        ("line.set_second_point", line.clone()),
        ("label.set_point", label),
        ("box.set_top_left_point", box_ty.clone()),
        ("box.set_bottom_right_point", box_ty),
    ] {
        specs.push(drawing_spec(
            name.to_string(),
            vec![vec![
                drawing_param("id", receiver, true),
                drawing_param("point", point.clone(), true),
            ]],
            Type::Void,
        ));
    }
    specs.push(drawing_spec(
        "line.get_price".to_string(),
        vec![vec![
            drawing_param("id", line, true),
            drawing_param("x", Type::Int, true),
        ]],
        Type::Float,
    ));

    let table = drawing_object(DrawKind::Table);
    let mut cell_params = vec![
        drawing_param("table_id", table.clone(), true),
        drawing_param("column", Type::Int, true),
        drawing_param("row", Type::Int, true),
    ];
    cell_params.extend(
        CellAttr::cell_param_order()
            .iter()
            .map(|attr| drawing_param(attr.name(), attr.ty(), false)),
    );
    specs.push(drawing_spec(
        "table.cell".to_string(),
        vec![cell_params],
        Type::Void,
    ));
    for &attr in CellAttr::all() {
        let value_required = matches!(attr, CellAttr::TextFontFamily | CellAttr::TextFormatting);
        specs.push(drawing_spec(
            format!("table.cell_set_{}", attr.setter_suffix()),
            vec![vec![
                drawing_param("table_id", table.clone(), true),
                drawing_param("column", Type::Int, true),
                drawing_param("row", Type::Int, true),
                drawing_param(attr.name(), attr.ty(), value_required),
            ]],
            Type::Void,
        ));
    }
    specs.push(drawing_spec(
        "table.clear".to_string(),
        vec![vec![
            drawing_param("table_id", table.clone(), true),
            drawing_param("start_column", Type::Int, true),
            drawing_param("start_row", Type::Int, true),
            drawing_param("end_column", Type::Int, false),
            drawing_param("end_row", Type::Int, false),
        ]],
        Type::Void,
    ));
    specs.push(drawing_spec(
        "table.merge_cells".to_string(),
        vec![vec![
            drawing_param("table_id", table, true),
            drawing_param("start_column", Type::Int, true),
            drawing_param("start_row", Type::Int, true),
            drawing_param("end_column", Type::Int, true),
            drawing_param("end_row", Type::Int, true),
        ]],
        Type::Void,
    ));

    for name in [
        "array.new_line",
        "array.new_label",
        "array.new_box",
        "array.new_table",
        "array.new_linefill",
    ] {
        let kind = DrawKind::from_array_constructor_name(name)
            .expect("drawing array constructor table is self-consistent");
        specs.push(drawing_spec(
            name.to_string(),
            vec![vec![
                drawing_param("size", Type::Int, false),
                drawing_param("initial_value", drawing_object(kind), false),
            ]],
            Type::Array(Box::new(drawing_object(kind))),
        ));
    }

    specs.sort_by(|left, right| left.name.cmp(&right.name));
    // Keep the macro in expression position so the syntax itself proves it
    // cannot expand to an item such as an inherent implementation.
    let () = debug_assert!(specs.windows(2).all(|pair| pair[0].name != pair[1].name));
    specs
}

/// Cached complete drawing-call surface. Building the table allocates owned
/// object/collection types, so semantic lookups share one immutable instance.
pub fn drawing_function_surface() -> &'static [DrawingFunctionSpec] {
    static SURFACE: OnceLock<Vec<DrawingFunctionSpec>> = OnceLock::new();
    SURFACE
        .get_or_init(build_drawing_function_surface)
        .as_slice()
}

/// Lookup one drawing call from [`drawing_function_surface`].
pub fn drawing_function_spec(name: &str) -> Option<&'static DrawingFunctionSpec> {
    drawing_function_surface()
        .iter()
        .find(|spec| spec.name == name)
}

/// Drawing `.all` series table.
pub fn drawing_series_surface() -> &'static [DrawingSeriesSpec] {
    static SURFACE: OnceLock<Vec<DrawingSeriesSpec>> = OnceLock::new();
    SURFACE
        .get_or_init(|| {
            DrawKind::all()
                .iter()
                .copied()
                .filter(|kind| kind.is_registered())
                .map(|kind| DrawingSeriesSpec {
                    name: format!("{}.all", kind.namespace()),
                    ty: Type::Array(Box::new(drawing_object(kind))),
                    qualifier: Qualifier::Series,
                })
                .collect()
        })
        .as_slice()
}

/// A const-friendly descriptor of a drawing field's declared type. Scalar
/// variants map 1:1 to [`Type`]; the two reference variants exist because
/// `Type::Array`/`Type::Object` carry heap-allocated payloads that cannot live
/// in a `const` field table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawFieldTy {
    Bool,
    Int,
    Float,
    String,
    TextFormat,
    Color,
    /// An `array<chart.point>` handle (`polyline.points`).
    ArrayOfPoint,
    /// A `line` object handle (`linefill.line1`/`line2`).
    LineObject,
}

impl DrawFieldTy {
    /// The owned [`Type`] this descriptor denotes.
    pub fn to_type(self) -> Type {
        match self {
            DrawFieldTy::Bool => Type::Bool,
            DrawFieldTy::Int => Type::Int,
            DrawFieldTy::Float => Type::Float,
            DrawFieldTy::String => Type::String,
            DrawFieldTy::TextFormat => Type::TextFormat,
            DrawFieldTy::Color => Type::Color,
            DrawFieldTy::ArrayOfPoint => {
                Type::Array(Box::new(Type::Object("chart.point".to_string())))
            }
            DrawFieldTy::LineObject => Type::Object("line".to_string()),
        }
    }
}

/// A field's default when unspecified at construction.
#[derive(Debug, Clone, Copy)]
pub enum DrawDefault {
    /// `na` (numeric coordinates, optional colors, reference-typed handles).
    Na,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// A named Pine color, resolved through the canonical palette.
    Color(&'static str),
    /// A style/enum const-string token (`xloc.bar_index`, `extend.none`, ...).
    Str(&'static str),
}

impl DrawDefault {
    pub fn color_channels(self) -> Option<(u8, u8, u8)> {
        let Self::Color(name) = self else {
            return None;
        };
        let (_, r, g, b) = crate::NAMED_COLORS
            .iter()
            .find(|color| color.0 == name)
            .expect("drawing default must name a registered Pine color");
        Some((*r, *g, *b))
    }
}

// ---- chart.point ----------------------------------------------------------
// Field order of chart.point.new: time, index, price. The order is the
// positional argument order of the constructor, so it is load-bearing, not
// cosmetic.
const POINT_FIELDS: &[DrawField] = &[
    DrawField {
        name: "time",
        ty: DrawFieldTy::Int,
        default: DrawDefault::Na,
    },
    DrawField {
        name: "index",
        ty: DrawFieldTy::Int,
        default: DrawDefault::Na,
    },
    DrawField {
        name: "price",
        ty: DrawFieldTy::Float,
        default: DrawDefault::Na,
    },
];

// ---- line -----------------------------------------------------------------
const LINE_FIELDS: &[DrawField] = &[
    DrawField {
        name: "x1",
        ty: DrawFieldTy::Int,
        default: DrawDefault::Na,
    },
    DrawField {
        name: "y1",
        ty: DrawFieldTy::Float,
        default: DrawDefault::Na,
    },
    DrawField {
        name: "x2",
        ty: DrawFieldTy::Int,
        default: DrawDefault::Na,
    },
    DrawField {
        name: "y2",
        ty: DrawFieldTy::Float,
        default: DrawDefault::Na,
    },
    DrawField {
        name: "xloc",
        ty: DrawFieldTy::String,
        default: DrawDefault::Str("xloc.bar_index"),
    },
    DrawField {
        name: "extend",
        ty: DrawFieldTy::String,
        default: DrawDefault::Str("extend.none"),
    },
    DrawField {
        name: "color",
        ty: DrawFieldTy::Color,
        default: DrawDefault::Color("color.blue"),
    },
    DrawField {
        name: "style",
        ty: DrawFieldTy::String,
        default: DrawDefault::Str("line.style_solid"),
    },
    DrawField {
        name: "width",
        ty: DrawFieldTy::Int,
        default: DrawDefault::Int(1),
    },
    DrawField {
        name: "force_overlay",
        ty: DrawFieldTy::Bool,
        default: DrawDefault::Bool(false),
    },
];

// ---- label ----------------------------------------------------------------
const LABEL_FIELDS: &[DrawField] = &[
    DrawField {
        name: "x",
        ty: DrawFieldTy::Int,
        default: DrawDefault::Na,
    },
    DrawField {
        name: "y",
        ty: DrawFieldTy::Float,
        default: DrawDefault::Na,
    },
    DrawField {
        name: "text",
        ty: DrawFieldTy::String,
        default: DrawDefault::Str(""),
    },
    DrawField {
        name: "xloc",
        ty: DrawFieldTy::String,
        default: DrawDefault::Str("xloc.bar_index"),
    },
    DrawField {
        name: "yloc",
        ty: DrawFieldTy::String,
        default: DrawDefault::Str("yloc.price"),
    },
    DrawField {
        name: "color",
        ty: DrawFieldTy::Color,
        default: DrawDefault::Color("color.blue"),
    },
    DrawField {
        name: "style",
        ty: DrawFieldTy::String,
        default: DrawDefault::Str("label.style_label_down"),
    },
    DrawField {
        name: "textcolor",
        ty: DrawFieldTy::Color,
        default: DrawDefault::Color("color.white"),
    },
    DrawField {
        name: "size",
        ty: DrawFieldTy::String,
        default: DrawDefault::Str("size.normal"),
    },
    DrawField {
        name: "textalign",
        ty: DrawFieldTy::String,
        default: DrawDefault::Str("text.align_center"),
    },
    // Tooltip / text metadata: written by `label.set_tooltip`,
    // `label.set_text_font_family`, and `label.set_text_formatting`. Surfaced
    // in the label DTO.
    DrawField {
        name: "tooltip",
        ty: DrawFieldTy::String,
        default: DrawDefault::Str(""),
    },
    DrawField {
        name: "text_font_family",
        ty: DrawFieldTy::String,
        default: DrawDefault::Str("font.family_default"),
    },
    DrawField {
        name: "force_overlay",
        ty: DrawFieldTy::Bool,
        default: DrawDefault::Bool(false),
    },
    DrawField {
        name: "text_formatting",
        ty: DrawFieldTy::TextFormat,
        default: DrawDefault::Str("text.format_none"),
    },
];

// ---- box ------------------------------------------------------------------
const BOX_FIELDS: &[DrawField] = &[
    DrawField {
        name: "left",
        ty: DrawFieldTy::Int,
        default: DrawDefault::Na,
    },
    DrawField {
        name: "top",
        ty: DrawFieldTy::Float,
        default: DrawDefault::Na,
    },
    DrawField {
        name: "right",
        ty: DrawFieldTy::Int,
        default: DrawDefault::Na,
    },
    DrawField {
        name: "bottom",
        ty: DrawFieldTy::Float,
        default: DrawDefault::Na,
    },
    DrawField {
        name: "border_color",
        ty: DrawFieldTy::Color,
        default: DrawDefault::Color("color.blue"),
    },
    DrawField {
        name: "border_width",
        ty: DrawFieldTy::Int,
        default: DrawDefault::Int(1),
    },
    DrawField {
        name: "border_style",
        ty: DrawFieldTy::String,
        default: DrawDefault::Str("line.style_solid"),
    },
    // Text/extent metadata: the box's optional caption and its
    // horizontal extension / x-anchoring. Added so the compound and text
    // setters (`box.set_text`, `box.set_extend`, `box.set_xloc`, and the
    // `box.set_text_*` family) have fields to write; all surface in the box DTO.
    DrawField {
        name: "extend",
        ty: DrawFieldTy::String,
        default: DrawDefault::Str("extend.none"),
    },
    DrawField {
        name: "xloc",
        ty: DrawFieldTy::String,
        default: DrawDefault::Str("xloc.bar_index"),
    },
    DrawField {
        name: "bgcolor",
        ty: DrawFieldTy::Color,
        default: DrawDefault::Color("color.blue"),
    },
    DrawField {
        name: "text",
        ty: DrawFieldTy::String,
        default: DrawDefault::Str(""),
    },
    DrawField {
        name: "text_size",
        ty: DrawFieldTy::String,
        default: DrawDefault::Str("size.auto"),
    },
    DrawField {
        name: "text_color",
        ty: DrawFieldTy::Color,
        default: DrawDefault::Color("color.black"),
    },
    DrawField {
        name: "text_halign",
        ty: DrawFieldTy::String,
        default: DrawDefault::Str("text.align_center"),
    },
    DrawField {
        name: "text_valign",
        ty: DrawFieldTy::String,
        default: DrawDefault::Str("text.align_center"),
    },
    DrawField {
        name: "text_wrap",
        ty: DrawFieldTy::String,
        default: DrawDefault::Str("text.wrap_none"),
    },
    DrawField {
        name: "text_font_family",
        ty: DrawFieldTy::String,
        default: DrawDefault::Str("font.family_default"),
    },
    DrawField {
        name: "force_overlay",
        ty: DrawFieldTy::Bool,
        default: DrawDefault::Bool(false),
    },
    DrawField {
        name: "text_formatting",
        ty: DrawFieldTy::TextFormat,
        default: DrawDefault::Str("text.format_none"),
    },
];

// ---- table ----------------------------------------------------------------
// table.new parameters in order: position, columns, rows, then optional
// bgcolor, frame_color, frame_width, border_color, border_width, force_overlay.
// The cell grid is stored out-of-line in the VM keyed by handle; these fields
// are the table-level attributes plus the declared dimensions.
const TABLE_FIELDS: &[DrawField] = &[
    DrawField {
        name: "position",
        ty: DrawFieldTy::String,
        default: DrawDefault::Str("position.top_right"),
    },
    DrawField {
        name: "columns",
        ty: DrawFieldTy::Int,
        default: DrawDefault::Int(0),
    },
    DrawField {
        name: "rows",
        ty: DrawFieldTy::Int,
        default: DrawDefault::Int(0),
    },
    DrawField {
        name: "bgcolor",
        ty: DrawFieldTy::Color,
        default: DrawDefault::Na,
    },
    DrawField {
        name: "frame_color",
        ty: DrawFieldTy::Color,
        default: DrawDefault::Na,
    },
    DrawField {
        name: "frame_width",
        ty: DrawFieldTy::Int,
        default: DrawDefault::Int(0),
    },
    DrawField {
        name: "border_color",
        ty: DrawFieldTy::Color,
        default: DrawDefault::Na,
    },
    DrawField {
        name: "border_width",
        ty: DrawFieldTy::Int,
        default: DrawDefault::Int(0),
    },
    DrawField {
        name: "force_overlay",
        ty: DrawFieldTy::Bool,
        default: DrawDefault::Bool(false),
    },
];

// ---- polyline --------------------------------------------------------------
// `polyline.new(points, curved?, closed?, xloc?, line_color?, fill_color?,
// line_style?, line_width?, force_overlay?)`. `points` is an
// `array<chart.point>` handle.
const POLYLINE_FIELDS: &[DrawField] = &[
    DrawField {
        name: "points",
        ty: DrawFieldTy::ArrayOfPoint,
        default: DrawDefault::Na,
    },
    DrawField {
        name: "curved",
        ty: DrawFieldTy::Bool,
        default: DrawDefault::Bool(false),
    },
    DrawField {
        name: "closed",
        ty: DrawFieldTy::Bool,
        default: DrawDefault::Bool(false),
    },
    DrawField {
        name: "xloc",
        ty: DrawFieldTy::String,
        default: DrawDefault::Str("xloc.bar_index"),
    },
    DrawField {
        name: "line_color",
        ty: DrawFieldTy::Color,
        default: DrawDefault::Color("color.blue"),
    },
    DrawField {
        name: "fill_color",
        ty: DrawFieldTy::Color,
        default: DrawDefault::Na,
    },
    DrawField {
        name: "line_style",
        ty: DrawFieldTy::String,
        default: DrawDefault::Str("line.style_solid"),
    },
    DrawField {
        name: "line_width",
        ty: DrawFieldTy::Int,
        default: DrawDefault::Int(1),
    },
    DrawField {
        name: "force_overlay",
        ty: DrawFieldTy::Bool,
        default: DrawDefault::Bool(false),
    },
];

// ---- linefill --------------------------------------------------------------
// `linefill.new(line1, line2, color)`. `line1`/`line2` are `line` object
// handles; `get_line1`/`get_line2`/`get_color` and `set_color` derive from
// these fields via the generic setter/getter machinery.
const LINEFILL_FIELDS: &[DrawField] = &[
    DrawField {
        name: "line1",
        ty: DrawFieldTy::LineObject,
        default: DrawDefault::Na,
    },
    DrawField {
        name: "line2",
        ty: DrawFieldTy::LineObject,
        default: DrawDefault::Na,
    },
    DrawField {
        name: "color",
        ty: DrawFieldTy::Color,
        default: DrawDefault::Na,
    },
];

// ---- table cells ----------------------------------------------------------
// The per-cell 2D grid is stored out-of-line (keyed by table handle) rather
// than in the table object's declared fields, because it is bounded by
// `columns * rows` (validated at `table.new`) and mutated by (column, row)
// coordinate rather than by field slot. Each cell carries the attributes
// below; a cell that has never been written is absent (serialized as no cell).
//
// table.cell parameters in order: table_id, column, row, then optional text,
// width, height, text_color, text_halign, text_valign, text_size, bgcolor,
// tooltip, text_font_family, text_formatting. The `cell_set_*` mutators each
// set one attribute of an existing cell.

/// One mutable attribute of a table cell. The discriminant is the
/// single source of truth shared by the checker (argument name + type), the CSIL
/// (a stable text/serialized tag), and the VM (which attribute slot to write).
/// Ordered to match the trailing optional parameters of `table.cell(...)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CellAttr {
    Text,
    Width,
    Height,
    TextColor,
    TextHalign,
    TextValign,
    TextSize,
    Bgcolor,
    Tooltip,
    TextFontFamily,
    TextFormatting,
}

impl CellAttr {
    /// Each `table.cell` call replaces all attributes; omitted values use these
    /// defaults. A `cell_set_*` call writes only its selected attribute.
    pub fn default(self) -> DrawDefault {
        match self {
            Self::Text | Self::Tooltip => DrawDefault::Str(""),
            Self::Width | Self::Height => DrawDefault::Float(0.0),
            Self::TextColor => DrawDefault::Color("color.black"),
            Self::TextHalign | Self::TextValign => DrawDefault::Str("text.align_center"),
            Self::TextSize => DrawDefault::Str("size.normal"),
            Self::Bgcolor => DrawDefault::Na,
            Self::TextFontFamily => DrawDefault::Str("font.family_default"),
            Self::TextFormatting => DrawDefault::Str("text.format_none"),
        }
    }
    /// Every cell attribute, in `table.cell(...)` trailing-parameter order.
    pub fn all() -> &'static [CellAttr] {
        &[
            CellAttr::Text,
            CellAttr::Width,
            CellAttr::Height,
            CellAttr::TextColor,
            CellAttr::TextHalign,
            CellAttr::TextValign,
            CellAttr::TextSize,
            CellAttr::Bgcolor,
            CellAttr::Tooltip,
            CellAttr::TextFontFamily,
            CellAttr::TextFormatting,
        ]
    }

    /// The parameter / DTO field name (`text`, `bgcolor`, `text_color`, ...).
    pub fn name(self) -> &'static str {
        match self {
            CellAttr::Text => "text",
            CellAttr::Width => "width",
            CellAttr::Height => "height",
            CellAttr::TextColor => "text_color",
            CellAttr::TextHalign => "text_halign",
            CellAttr::TextValign => "text_valign",
            CellAttr::TextSize => "text_size",
            CellAttr::Bgcolor => "bgcolor",
            CellAttr::Tooltip => "tooltip",
            CellAttr::TextFontFamily => "text_font_family",
            CellAttr::TextFormatting => "text_formatting",
        }
    }

    /// The `cell_set_<suffix>` mutator name suffix (identical to the attribute
    /// name here, but kept explicit so the two never silently drift).
    pub fn setter_suffix(self) -> &'static str {
        self.name()
    }

    /// The declared type the checker enforces for this attribute's value.
    pub fn ty(self) -> Type {
        match self {
            CellAttr::Text
            | CellAttr::TextHalign
            | CellAttr::TextValign
            | CellAttr::TextSize
            | CellAttr::Tooltip
            | CellAttr::TextFontFamily => Type::String,
            CellAttr::TextFormatting => Type::TextFormat,
            // Cell width and height are float values, not style tokens.
            CellAttr::Width | CellAttr::Height => Type::Float,
            CellAttr::TextColor | CellAttr::Bgcolor => Type::Color,
        }
    }

    /// A stable single-token tag for the textual/serialized CSIL form. The inverse
    /// is [`CellAttr::from_tag`].
    pub fn text_tag(self) -> &'static str {
        self.name()
    }

    /// Parse a [`CellAttr::text_tag`].
    pub fn from_tag(tag: &str) -> Option<CellAttr> {
        CellAttr::all().iter().copied().find(|a| a.name() == tag)
    }

    /// The `table.cell(...)` positional parameter names for the trailing
    /// optional attributes, in order (after `table_id, column, row`).
    pub fn cell_param_order() -> &'static [CellAttr] {
        CellAttr::all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn drawing_surface_is_unique_and_covers_special_analyzer_members() {
        let functions = drawing_function_surface();
        let names: HashSet<_> = functions.iter().map(|spec| spec.name.as_str()).collect();
        assert_eq!(names.len(), functions.len());
        for name in [
            "table.new",
            "polyline.new",
            "linefill.new",
            "chart.point.new",
            "line.set_xy1",
            "label.set_xy",
            "box.set_lefttop",
            "line.get_price",
            "table.cell",
            "table.clear",
            "table.merge_cells",
        ] {
            assert!(names.contains(name), "missing `{name}`");
        }
        assert!(!names.contains("line.set_force_overlay"));
        assert!(!names.contains("line.get_force_overlay"));
    }

    #[test]
    fn drawing_surface_preserves_overloads_and_all_series_types() {
        let line_new = drawing_function_spec("line.new").expect("line.new metadata");
        assert_eq!(line_new.overloads.len(), 2);
        assert_eq!(line_new.overloads[1][0].name, "first_point");
        assert_eq!(line_new.overloads[1][1].name, "second_point");
        assert_eq!(
            line_new.return_qualifier,
            Some(DrawingReturnQualifier::Fixed(Qualifier::Series))
        );
        assert_eq!(
            drawing_function_spec("line.copy")
                .expect("line.copy metadata")
                .return_qualifier,
            Some(DrawingReturnQualifier::Fixed(Qualifier::Series))
        );
        assert_eq!(
            drawing_function_spec("line.get_x1")
                .expect("line.get_x1 metadata")
                .return_qualifier,
            Some(DrawingReturnQualifier::Fixed(Qualifier::Series))
        );

        let series = drawing_series_surface();
        assert_eq!(series.len(), DrawKind::all().len() - 1);
        assert!(series.iter().any(|value| {
            value.name == "line.all"
                && value.ty == Type::Array(Box::new(Type::Object("line".to_string())))
                && value.qualifier == Qualifier::Series
        }));
    }
}
