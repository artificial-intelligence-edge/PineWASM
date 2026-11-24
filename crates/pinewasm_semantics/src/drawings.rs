//! Drawings, tables, footprints, and user-defined objects: constructor and
//! getter/setter analysis for the drawing namespaces, object field
//! reads/writes, and `Type.new`/`copy` handling.

use crate::*;

impl<'r> Analyzer<'r> {
    /// Analyze an object field read: read object variable `head_id`, then follow the
    /// dot-separated `path` (e.g. `child.x`) as a chain of field accesses. A
    /// history offset on a field read is rejected (`E0209` — objects are
    /// reference values with no field-level history). An unknown field or a
    /// field access through a non-object mid-path value is `E0237`.
    pub(crate) fn analyze_field_read(
        &mut self,
        head_id: SymbolId,
        path: &str,
        offset: u32,
        span: Span,
    ) -> HirExpr {
        if offset > 0 {
            self.error(
                codes::E0209,
                OBJECT_FIELD_HISTORY_MESSAGE,
                span,
                OBJECT_FIELD_HISTORY_LABEL,
            );
            return self.error_expr(span);
        }
        let symbol = &self.symbols[head_id.0 as usize];
        let mut current = HirExpr {
            kind: HirExprKind::ReadVar {
                symbol: head_id,
                offset: 0,
            },
            ty: symbol.ty.clone(),
            qualifier: symbol.qualifier,
            span,
        };
        for segment in path.split('.') {
            current = self.read_object_field(current, segment, span);
            if current.ty.is_error() {
                return current;
            }
        }
        current
    }

    /// Analyze a post-call field read such as `array.get(points, 0).price`.
    /// A field read off a bare identifier is represented as a dotted name and
    /// handled by `analyze_field_read`; this path handles arbitrary object
    /// expressions and reuses the same object schema and diagnostics.
    pub(crate) fn analyze_field_access(
        &mut self,
        receiver: &ast::Expr,
        field: &ast::Ident,
        span: Span,
    ) -> HirExpr {
        let receiver = self.analyze_expr(receiver);
        self.read_object_field(receiver, &field.name, span)
    }

    fn read_object_field(&mut self, current: HirExpr, segment: &str, span: Span) -> HirExpr {
        if current.ty.is_error() {
            return current;
        }
        let Type::Object(type_name) = current.ty.clone() else {
            self.error(
                codes::E0237,
                format!(
                    "cannot access field `{segment}` on a {}",
                    current.ty.describe()
                ),
                span,
                "field access requires an object value",
            );
            return self.error_expr(span);
        };
        let info = self
            .types
            .get(&type_name)
            .expect("object type is always collected");
        let Some((idx, field)) = info.field(segment) else {
            self.error(
                codes::E0237,
                format!("type `{type_name}` has no field `{segment}`"),
                span,
                format!("`{type_name}` has no field named `{segment}`"),
            );
            return self.error_expr(span);
        };
        HirExpr {
            kind: HirExprKind::GetField {
                obj: Box::new(current),
                field_idx: idx as u32,
            },
            ty: field.ty.clone(),
            // A field can be reassigned on any bar, so a read is never simple.
            qualifier: Qualifier::Series,
            span,
        }
    }

    /// Analyze an object field write `obj.<path> := value`. Resolves the container
    /// object (all but the last path segment) as a field-read chain, resolves
    /// the final field, coerces `value` to the field type (int → float widening;
    /// `E0237` otherwise), and produces a [`HirStmt::SetField`]. An unknown
    /// field is `E0237`.
    pub(crate) fn analyze_field_write(
        &mut self,
        head_id: SymbolId,
        path: &str,
        value: &ast::Expr,
        span: Span,
    ) -> Option<HirStmt> {
        // Split the path into the container prefix and the final field name.
        let (prefix, last) = match path.rsplit_once('.') {
            Some((p, l)) => (Some(p), l),
            None => (None, path),
        };
        // The object whose field is written: either the head variable directly,
        // or a field-read chain into it. Reuse `analyze_field_read` for the
        // prefix so chained writes (`a.b.field := ...`) resolve.
        let container = match prefix {
            Some(prefix) => self.analyze_field_read(head_id, prefix, 0, span),
            None => {
                let symbol = &self.symbols[head_id.0 as usize];
                HirExpr {
                    kind: HirExprKind::ReadVar {
                        symbol: head_id,
                        offset: 0,
                    },
                    ty: symbol.ty.clone(),
                    qualifier: symbol.qualifier,
                    span,
                }
            }
        };
        if container.ty.is_error() {
            let _ = self.analyze_expr(value);
            return None;
        }
        let Type::Object(type_name) = container.ty.clone() else {
            self.error(
                codes::E0237,
                format!(
                    "cannot write field `{last}` on a {}",
                    container.ty.describe()
                ),
                span,
                "field access requires an object value",
            );
            let _ = self.analyze_expr(value);
            return None;
        };
        let info = self
            .types
            .get(&type_name)
            .expect("object type is always collected");
        let Some((idx, field)) = info.field(last) else {
            self.error(
                codes::E0237,
                format!("type `{type_name}` has no field `{last}`"),
                span,
                format!("`{type_name}` has no field named `{last}`"),
            );
            let _ = self.analyze_expr(value);
            return None;
        };
        let field_ty = field.ty.clone();
        let field_span = field.span;
        let value_hir = self.analyze_expr_expecting(value, Some(&field_ty));
        let value_hir = self.coerce_to_field(value_hir, &field_ty, value.span(), field_span)?;
        Some(HirStmt::SetField {
            obj: container,
            field_idx: idx as u32,
            value: value_hir,
            span,
        })
    }

    /// Seed the synthetic drawing-object types (`chart.point`, `line`, `label`,
    /// `box`) as UDTs so `.new` reuses the object constructor and `obj.field`
    /// reuses field access. Each becomes a `TypeInfo` (with the schema's fields
    /// and defaults) and an `object_types` entry; the assigned `type_id` matches
    /// its index in both. Called once at analyzer construction, before any user
    /// type declaration, so drawing `type_ids` are stable and low.
    pub(crate) fn seed_drawing_types(&mut self) {
        for &kind in DrawKind::all() {
            let name = kind.type_name().to_string();
            let mut fields = Vec::new();
            for f in kind.fields() {
                let field_ty = f.ty();
                let default = self.drawing_field_default(&f.default, &field_ty);
                fields.push(FieldInfo {
                    name: f.name.to_string(),
                    ty: field_ty,
                    default: Some(default),
                    span: Span::dummy(),
                });
            }
            let type_id = self.object_types.len() as u32;
            self.object_types.push(ObjectTypeDecl {
                name: name.clone(),
                field_count: fields.len() as u32,
                field_lanes: Vec::new(),
            });
            self.types.insert(
                name.clone(),
                TypeInfo {
                    type_id,
                    fields,
                    name_span: Span::dummy(),
                },
            );
        }
    }

    /// Register the order-flow `footprint` and `volume_row` object types (newer
    /// namespace). Mirrors [`Self::seed_drawing_types`]: each is
    /// a synthetic [`Type::Object`] with a fixed field layout (see
    /// [`pinewasm_hir::footprint`]). `request.footprint()` materializes a
    /// `footprint` from the optional host table; the `footprint.*`/`volume_row.*`
    /// accessors are plain field reads over these types.
    pub(crate) fn seed_footprint_types(&mut self) {
        use pinewasm_hir::footprint::{
            FpField, FOOTPRINT_FIELDS, FOOTPRINT_TYPE, VOLUME_ROW_FIELDS, VOLUME_ROW_TYPE,
        };
        for (name, spec) in [
            (FOOTPRINT_TYPE, FOOTPRINT_FIELDS),
            (VOLUME_ROW_TYPE, VOLUME_ROW_FIELDS),
        ] {
            let fields: Vec<FieldInfo> = spec
                .iter()
                .map(|f: &FpField| {
                    let ty = f.ty.to_type();
                    let default = HirExpr {
                        kind: HirExprKind::ConstNa,
                        ty: ty.clone(),
                        qualifier: Qualifier::Const,
                        span: Span::dummy(),
                    };
                    FieldInfo {
                        name: f.name.to_string(),
                        ty,
                        default: Some(default),
                        span: Span::dummy(),
                    }
                })
                .collect();
            let type_id = self.object_types.len() as u32;
            self.object_types.push(ObjectTypeDecl {
                name: name.to_string(),
                field_count: fields.len() as u32,
                field_lanes: Vec::new(),
            });
            self.types.insert(
                name.to_string(),
                TypeInfo {
                    type_id,
                    fields,
                    name_span: Span::dummy(),
                },
            );
        }
    }

    /// Materialize a drawing field's declared default as a const [`HirExpr`].
    pub(crate) fn drawing_field_default(&self, default: &DrawDefault, ty: &Type) -> HirExpr {
        let kind = match default {
            DrawDefault::Na => HirExprKind::ConstNa,
            DrawDefault::Bool(b) => HirExprKind::ConstBool(*b),
            DrawDefault::Int(i) => HirExprKind::ConstInt(*i),
            DrawDefault::Float(x) => HirExprKind::ConstFloat(*x),
            DrawDefault::Color(_) => {
                let (r, g, b) = default.color_channels().expect("color default");
                HirExprKind::ConstColor { r, g, b, t: 0.0 }
            }
            DrawDefault::Str(s) => HirExprKind::ConstString(
                pinewasm_hir::drawing_options::pine_drawing_string(s)
                    .unwrap_or(s)
                    .to_string(),
            ),
        };
        HirExpr {
            kind,
            ty: ty.clone(),
            qualifier: Qualifier::Const,
            span: Span::dummy(),
        }
    }

    /// Intercept and analyze a stateful drawing-namespace call:
    /// `line.new(...)` / setters / getters / `line.delete(id)` / `line.copy(id)`
    /// and the `chart.point.*` constructors. Returns `None` when `callee` is not
    /// a drawing member (so the normal dispatch continues). Drawing objects are
    /// heap reference objects reusing the UDT machinery; `line.new`/`copy` wrap
    /// the constructed handle in a [`HirExprKind::RegisterDrawing`] so the VM
    /// tracks it live (with oldest-first GC); `delete` lowers to
    /// [`HirExprKind::DeleteDrawing`]; setters/getters are field writes/reads.
    pub(crate) fn analyze_drawing_call(
        &mut self,
        callee: &ast::Ident,
        args: &[ast::CallArg],
        span: Span,
    ) -> Option<HirExpr> {
        // Identify `(kind, member)` from the dotted callee. `chart.point.*` has
        // two dots; the rest have one.
        let (kind, member) = self.split_drawing_callee(&callee.name)?;
        if pinewasm_hir::drawing::drawing_function_spec(&callee.name).is_none() {
            self.error(
                codes::E0238,
                format!("`{}` has no member `{member}`", kind.namespace()),
                callee.span,
                "unknown drawing-object member",
            );
            for arg in args {
                let _ = self.analyze_expr(&arg.value);
            }
            return Some(self.error_expr(span));
        }

        // `<ns>.new(...)` — construct the object, then register (unless a point).
        if member == "new" {
            if kind == DrawKind::Point {
                return Some(self.analyze_point_ctor(
                    "chart.point.new",
                    &["time", "index", "price"],
                    false,
                    args,
                    span,
                ));
            }
            // `line`, `label` and `box` also accept `chart.point` corners instead
            // of raw coordinates.
            if let Some(new_expr) = self.analyze_drawing_new_pointwise(kind, args, span) {
                return Some(self.wrap_drawing_new(kind, new_expr, span));
            }
            let new_expr = self.analyze_object_new(kind.type_name(), args, span);
            return Some(self.wrap_drawing_new(kind, new_expr, span));
        }
        // `chart.point.from_index(index, price)` / `.from_time(time, price)` /
        // `.now(price)` — convenience constructors that bind a subset of fields.
        if kind == DrawKind::Point {
            match member.as_str() {
                "from_index" => {
                    return Some(self.analyze_point_ctor(
                        "chart.point.from_index",
                        &["index", "price"],
                        false,
                        args,
                        span,
                    ));
                }
                "from_time" => {
                    return Some(self.analyze_point_ctor(
                        "chart.point.from_time",
                        &["time", "price"],
                        false,
                        args,
                        span,
                    ));
                }
                "now" => {
                    return Some(self.analyze_point_ctor(
                        "chart.point.now",
                        &["price"],
                        true,
                        args,
                        span,
                    ));
                }
                _ => {}
            }
        }
        // `<ns>.copy(id)` — shallow copy, re-registered as a new live object.
        if member == "copy" {
            let copy_expr = self.analyze_object_copy(kind.type_name(), args, span);
            return Some(self.wrap_drawing_new(kind, copy_expr, span));
        }
        // `<ns>.delete(id)` — remove from the live registry.
        if member == "delete" && kind.is_registered() {
            return Some(self.analyze_drawing_delete(kind, args, span));
        }
        // Compound / point-taking / interpolation members:
        // intercepted before the generic `set_<field>` path because they touch
        // multiple fields (or a `chart.point` argument) rather than a single
        // named field, and must evaluate the receiver exactly once.
        if let Some(result) = self.analyze_drawing_compound(kind, &member, args, span) {
            return Some(result);
        }
        // Table per-cell grid: `table.cell`, `table.cell_set_*`,
        // `table.clear`, `table.merge_cells`. Intercepted before the generic
        // `set_`/`get_` field paths (these address cells by (column,row), not by
        // an object field slot).
        if kind == DrawKind::Table {
            if let Some(result) = self.analyze_table_cell_call(&member, args, span) {
                return Some(result);
            }
        }
        // `<ns>.set_<field>(id, value)` — field write.
        if let Some(field) = member.strip_prefix("set_") {
            return Some(self.analyze_drawing_setter(kind, field, args, span));
        }
        // `<ns>.get_<field>(id)` — field read.
        if let Some(field) = member.strip_prefix("get_") {
            return Some(self.analyze_drawing_getter(kind, field, args, span));
        }
        // A dotted name in a drawing namespace that is not a known member.
        self.error(
            codes::E0238,
            format!("`{}` has no member `{member}`", kind.namespace()),
            callee.span,
            "unknown drawing-object member",
        );
        for arg in args {
            let _ = self.analyze_expr(&arg.value);
        }
        Some(self.error_expr(span))
    }

    /// Resolve a dotted callee to `(DrawKind, member)` if it names a drawing
    /// namespace member; otherwise `None`.
    pub(crate) fn split_drawing_callee(&self, name: &str) -> Option<(DrawKind, String)> {
        if let Some(rest) = name.strip_prefix("chart.point.") {
            if !rest.contains('.') {
                return Some((DrawKind::Point, rest.to_string()));
            }
        }
        let (ns, member) = name.split_once('.')?;
        if member.contains('.') {
            return None;
        }
        let kind = match ns {
            "line" => DrawKind::Line,
            "label" => DrawKind::Label,
            "box" => DrawKind::Box,
            "table" => DrawKind::Table,
            "polyline" => DrawKind::Polyline,
            "linefill" => DrawKind::Linefill,
            _ => return None,
        };
        Some((kind, member.to_string()))
    }

    /// Wrap a freshly constructed drawing object in a [`HirExprKind::RegisterDrawing`]
    /// (registered kinds only). A `chart.point` is returned unwrapped.
    pub(crate) fn wrap_drawing_new(
        &mut self,
        kind: DrawKind,
        new_expr: HirExpr,
        span: Span,
    ) -> HirExpr {
        if new_expr.ty.is_error() || !kind.is_registered() {
            return new_expr;
        }
        HirExpr {
            ty: new_expr.ty.clone(),
            // Drawing handles are assigned the series qualifier even when their
            // constructor arguments are constants.
            qualifier: Qualifier::Series,
            kind: HirExprKind::RegisterDrawing {
                kind,
                inner: Box::new(new_expr),
            },
            span,
        }
    }

    /// The `chart.point`-taking overload of `line.new` / `label.new` /
    /// `box.new`: `line.new(first_point, second_point, xloc, ...)`,
    /// `label.new(point, text, ...)`, `box.new(top_left, bottom_right, ...)`.
    /// Returns `None` when the call is not that overload, so the caller falls
    /// through to the coordinate form.
    ///
    /// Each point expression is captured once at the call site. The drawing's
    /// xloc selects the point's time or index, while price supplies y. Captured
    /// scalar fields are copied into the drawing; later point mutations do not
    /// move an existing object. The existing DTO carries xloc and supports both
    /// logical indices and epoch-millisecond anchors.
    pub(crate) fn analyze_drawing_new_pointwise(
        &mut self,
        kind: DrawKind,
        args: &[ast::CallArg],
        span: Span,
    ) -> Option<HirExpr> {
        // (parameter name, the two coordinate fields it fills) for the leading
        // point parameters, then the tail parameters shared with the
        // coordinate form.
        let (points, tail): (&[(&str, [&str; 2])], &[&str]) = match kind {
            DrawKind::Line => (
                &[
                    ("first_point", ["x1", "y1"]),
                    ("second_point", ["x2", "y2"]),
                ],
                &["xloc", "extend", "color", "style", "width", "force_overlay"],
            ),
            DrawKind::Label => (
                &[("point", ["x", "y"])],
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
                &[
                    ("top_left", ["left", "top"]),
                    ("bottom_right", ["right", "bottom"]),
                ],
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

        // Pick the overload syntactically: an argument named after a point
        // parameter, or a leading positional argument that is already known to
        // be a `chart.point`. Peeking rather than analyzing keeps this free of
        // duplicate diagnostics when the call is really the coordinate form.
        let named_point = args.iter().any(|a| {
            a.name
                .as_ref()
                .is_some_and(|n| points.iter().any(|(p, _)| *p == n.name))
        });
        let positional_point = args
            .first()
            .is_some_and(|a| a.name.is_none() && self.is_chart_point_expr(&a.value));
        if !named_point && !positional_point {
            return None;
        }

        let param_names: Vec<&str> = points
            .iter()
            .map(|(name, _)| *name)
            .chain(tail.iter().copied())
            .collect();
        let (bound, overflow) = self.bind_call_args(kind.namespace(), &param_names, args)?;
        if !overflow.is_empty() {
            self.error(
                codes::E0237,
                format!(
                    "too many arguments for `{}.new` (this overload takes {} argument(s))",
                    kind.namespace(),
                    param_names.len()
                ),
                span,
                "each argument sets one parameter",
            );
            for e in overflow {
                let _ = self.analyze_expr(e);
            }
            return Some(self.error_expr(span));
        }

        // Start from the coordinate form's declared defaults, then overwrite the
        // slots this call supplies.
        let schema = kind.fields();
        let mut values: Vec<HirExpr> = schema
            .iter()
            .map(|f| self.drawing_field_default(&f.default, &f.ty()))
            .collect();
        let mut qualifier = Qualifier::Simple;
        let mut ok = true;
        let mut bindings = Vec::new();
        let mut captured_points = Vec::new();

        for (i, (_, coords)) in points.iter().enumerate() {
            let Some(arg) = bound[i] else {
                self.error(
                    codes::E0237,
                    format!(
                        "`{}.new` is missing the `{}` argument",
                        kind.namespace(),
                        param_names[i]
                    ),
                    span,
                    "this overload takes chart.point corners",
                );
                ok = false;
                continue;
            };
            let point = self.analyze_expr(arg);
            let point_ty = Type::Object(DrawKind::Point.type_name().to_string());
            if point.ty.is_error() {
                ok = false;
                continue;
            }
            if point.ty != point_ty {
                self.error(
                    codes::E0237,
                    format!(
                        "`{}.new` argument `{}` must be a chart.point, found {}",
                        kind.namespace(),
                        param_names[i],
                        point.ty.describe()
                    ),
                    arg.span(),
                    "expected a chart.point",
                );
                ok = false;
                continue;
            }
            qualifier = qualifier.join(point.qualifier);
            let point = self.capture_drawing_operand(point, &mut bindings);
            captured_points.push((coords, point));
        }

        for (i, name) in param_names.iter().enumerate().skip(points.len()) {
            let Some(arg) = bound[i] else { continue };
            let slot = kind
                .field_index(name)
                .expect("tail parameters name declared fields");
            let field_ty = schema[slot as usize].ty();
            let text_size_union = (kind == DrawKind::Label && *name == "size")
                || (kind == DrawKind::Box && *name == "text_size");
            let hir = if text_size_union {
                self.analyze_expr(arg)
            } else {
                self.analyze_expr_expecting(arg, Some(&field_ty))
            };
            if *name == "force_overlay" {
                self.defer_temporal_fixedness_at(
                    &hir,
                    &format!("{}.new", kind.namespace()),
                    name,
                    arg.span(),
                );
            }
            if *name == "force_overlay" && hir.qualifier != Qualifier::Const {
                self.error(
                    codes::E0206,
                    format!(
                        "`{}.new` argument `force_overlay` must be const bool",
                        kind.namespace()
                    ),
                    arg.span(),
                    "force_overlay must be a const bool",
                );
                ok = false;
                continue;
            }
            match self.coerce_drawing_field(
                hir,
                &field_ty,
                arg.span(),
                Span::dummy(),
                text_size_union,
            ) {
                Some(value) => {
                    qualifier = qualifier.join(value.qualifier);
                    values[slot as usize] = self.capture_drawing_operand(value, &mut bindings);
                }
                None => ok = false,
            }
        }

        if !ok {
            return Some(self.error_expr(span));
        }
        let xloc =
            values[kind.field_index("xloc").expect("point drawing has xloc") as usize].clone();
        for (coords, point) in captured_points {
            values[kind.field_index(coords[0]).expect("x coordinate") as usize] =
                Self::drawing_point_x(&point, &xloc, span);
            values[kind.field_index(coords[1]).expect("y coordinate") as usize] =
                Self::drawing_point_field(&point, "price", Type::Float, span);
        }
        let type_id = self
            .types
            .get(kind.type_name())
            .expect("drawing types are seeded")
            .type_id;
        let result = HirExpr {
            kind: HirExprKind::NewObject {
                type_id,
                fields: values,
            },
            ty: Type::Object(kind.type_name().to_string()),
            qualifier,
            span,
        };
        Some(Self::drawing_eval_sequence(bindings, result))
    }

    /// Keep a drawing operand inside the expression rather than hoisting it
    /// before its statement. This preserves conditional and var evaluation.
    fn capture_drawing_operand(
        &mut self,
        value: HirExpr,
        bindings: &mut Vec<(SeriesSlot, HirExpr)>,
    ) -> HirExpr {
        let slot = self.allocate_synthetic_slot(value.ty.clone(), value.span);
        self.temporal_binding(SymbolId(slot.0), &value);
        let read = HirExpr {
            kind: HirExprKind::ReadVar {
                symbol: SymbolId(slot.0),
                offset: 0,
            },
            ty: value.ty.clone(),
            qualifier: value.qualifier,
            span: value.span,
        };
        bindings.push((slot, value));
        read
    }

    fn drawing_eval_sequence(bindings: Vec<(SeriesSlot, HirExpr)>, result: HirExpr) -> HirExpr {
        HirExpr {
            ty: result.ty.clone(),
            qualifier: result.qualifier,
            span: result.span,
            kind: HirExprKind::EvalSequence {
                bindings,
                result: Box::new(result),
            },
        }
    }

    fn drawing_point_field(point: &HirExpr, field: &str, ty: Type, span: Span) -> HirExpr {
        HirExpr {
            kind: HirExprKind::GetField {
                obj: Box::new(point.clone()),
                field_idx: DrawKind::Point
                    .field_index(field)
                    .expect("chart.point coordinate"),
            },
            ty,
            qualifier: point.qualifier,
            span,
        }
    }

    /// Alias support is restricted to this xloc field, which accepts PineWASM's
    /// short alias `bt` for the bar-time anchor alongside the canonical DTO string.
    fn drawing_point_x(point: &HirExpr, xloc: &HirExpr, span: Span) -> HirExpr {
        let eq = |value: &str| HirExpr {
            kind: HirExprKind::Binary {
                op: BinOp::Eq,
                lhs: Box::new(xloc.clone()),
                rhs: Box::new(HirExpr {
                    kind: HirExprKind::ConstString(value.into()),
                    ty: Type::String,
                    qualifier: Qualifier::Const,
                    span,
                }),
            },
            ty: Type::Bool,
            qualifier: xloc.qualifier,
            span,
        };
        let time_mode = HirExpr {
            kind: HirExprKind::Binary {
                op: BinOp::Or,
                lhs: Box::new(eq(pinewasm_hir::drawing_options::pine_drawing_string(
                    "xloc.bar_time",
                )
                .expect("known xloc alias"))),
                rhs: Box::new(eq("xloc.bar_time")),
            },
            ty: Type::Bool,
            qualifier: xloc.qualifier,
            span,
        };
        HirExpr {
            kind: HirExprKind::Ternary {
                cond: Box::new(time_mode),
                then_e: Box::new(Self::drawing_point_field(point, "time", Type::Int, span)),
                else_e: Box::new(Self::drawing_point_field(point, "index", Type::Int, span)),
            },
            ty: Type::Int,
            qualifier: point.qualifier.join(xloc.qualifier),
            span,
        }
    }

    /// Inspect declared types without analyzing (or duplicating diagnostics
    /// and effects) while selecting the coordinate or point overload.
    pub(crate) fn is_chart_point_expr(&self, expr: &ast::Expr) -> bool {
        matches!(self.drawing_operand_type(expr), Some(Type::Object(name)) if name == DrawKind::Point.type_name())
    }

    fn drawing_operand_type(&self, expr: &ast::Expr) -> Option<Type> {
        let field_type = |ty: Type, field: &str| {
            let Type::Object(name) = ty else { return None };
            self.types
                .get(&name)?
                .fields
                .iter()
                .find(|f| f.name == field)
                .map(|f| f.ty.clone())
        };
        let collection_value = |ty: Type| match ty {
            Type::Array(value) | Type::Matrix(value) | Type::Map(_, value) => Some(*value),
            _ => None,
        };
        match expr {
            ast::Expr::Ident(ident) => {
                let mut path = ident.name.split('.');
                let id = self.lookup_variable(path.next()?)?;
                let mut ty = self.symbols[id.0 as usize].ty.clone();
                for field in path {
                    ty = field_type(ty, field)?;
                }
                Some(ty)
            }
            ast::Expr::Group { inner, .. } => self.drawing_operand_type(inner),
            ast::Expr::FieldAccess {
                receiver, field, ..
            } => field_type(self.drawing_operand_type(receiver)?, &field.name),
            ast::Expr::Call { callee, args, .. } => {
                if callee.name.starts_with("chart.point.") {
                    return Some(Type::Object(DrawKind::Point.type_name().into()));
                }
                if matches!(
                    callee.name.as_str(),
                    "array.get"
                        | "array.pop"
                        | "array.shift"
                        | "array.first"
                        | "array.last"
                        | "map.get"
                        | "matrix.get"
                ) {
                    let receiver = args
                        .iter()
                        .find(|a| a.name.as_ref().is_some_and(|n| n.name == "id"))
                        .or_else(|| args.first())?;
                    return collection_value(self.drawing_operand_type(&receiver.value)?);
                }
                // A simple dotted receiver such as `points.pop()` is parsed
                // as a named Call, whereas expression receivers use MethodCall.
                // Inspect both forms without evaluating either receiver.
                if let Some((receiver, method)) = callee.name.rsplit_once('.') {
                    if matches!(method, "get" | "pop" | "shift" | "first" | "last") {
                        let receiver = ast::Expr::Ident(ast::Ident {
                            name: receiver.to_string(),
                            span: callee.span,
                        });
                        return collection_value(self.drawing_operand_type(&receiver)?);
                    }
                }
                // A local wrapper can return a point directly (for example
                // `pt(x,y) => chart.point.from_time(x,y)`). Select the point
                // overload before analyzing its scalar-coordinate alternative.
                if let Some(definitions) = self.current_functions().get(&callee.name) {
                    if !definitions.is_empty()
                        && definitions.iter().all(|definition| {
                            matches!(definition.body.last(), Some(ast::Stmt::Expr {
                            expr: ast::Expr::Call { callee, .. }, ..
                        }) if callee.name.starts_with("chart.point."))
                        })
                    {
                        return Some(Type::Object(DrawKind::Point.type_name().into()));
                    }
                }
                None
            }
            ast::Expr::MethodCall {
                receiver, method, ..
            } if matches!(
                method.name.as_str(),
                "get" | "pop" | "shift" | "first" | "last"
            ) =>
            {
                collection_value(self.drawing_operand_type(receiver)?)
            }
            ast::Expr::Ternary { then_e, else_e, .. } => {
                let a = self.drawing_operand_type(then_e)?;
                let b = self.drawing_operand_type(else_e)?;
                (a == b).then_some(a)
            }
            _ => None,
        }
    }

    /// A `chart.point` convenience constructor: bind the documented parameter
    /// names and apply its required/default rules, defaulting other point fields.
    pub(crate) fn analyze_point_ctor(
        &mut self,
        callee: &str,
        field_names: &[&str],
        all_optional: bool,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        let kind = DrawKind::Point;
        let Some((bound, overflow)) = self.bind_call_args(callee, field_names, args) else {
            return self.error_expr(span);
        };
        if !overflow.is_empty() {
            self.error(
                codes::E0238,
                format!("`{callee}` takes at most {} argument(s)", field_names.len()),
                span,
                "too many coordinates",
            );
            for expr in overflow {
                let _ = self.analyze_expr(expr);
            }
            return self.error_expr(span);
        }
        let mut named = Vec::with_capacity(field_names.len());
        for (index, (name, value)) in field_names.iter().zip(bound).enumerate() {
            let Some(value) = value else {
                if all_optional {
                    continue;
                }
                self.error(
                    codes::E0238,
                    format!("`{callee}` is missing required argument `{name}`"),
                    span,
                    "this coordinate is required",
                );
                continue;
            };
            let value_span = value.span();
            named.push(ast::CallArg {
                name: Some(ast::Ident {
                    name: (*name).to_string(),
                    span: value_span,
                }),
                value: value.clone(),
                span: args.get(index).map_or(value_span, |arg| arg.span),
            });
        }
        if callee == "chart.point.now" {
            if named.is_empty() {
                named.push(ast::CallArg {
                    name: Some(ast::Ident {
                        name: "price".into(),
                        span,
                    }),
                    value: ast::Expr::Ident(builtin_ident("close", span)),
                    span,
                });
            }
            for (field, builtin) in [("time", "time"), ("index", "bar_index")] {
                named.push(ast::CallArg {
                    name: Some(ast::Ident {
                        name: field.into(),
                        span,
                    }),
                    value: ast::Expr::Ident(builtin_ident(builtin, span)),
                    span,
                });
            }
        }
        self.analyze_object_new(kind.type_name(), &named, span)
    }

    /// Reorder a drawing member's arguments into declaration order when any
    /// of them is named. Names bind against the member's entry in
    /// [`pinewasm_hir::drawing::drawing_function_spec`], so the receiver is
    /// `table_id` for table members and `id` everywhere else. All-positional
    /// calls pass through untouched. Returns `None` after reporting an unknown,
    /// repeated, or skipped parameter.
    fn bind_drawing_args<'a>(
        &mut self,
        callee: &str,
        args: &'a [ast::CallArg],
        span: Span,
    ) -> Option<std::borrow::Cow<'a, [ast::CallArg]>> {
        if args.iter().all(|a| a.name.is_none()) {
            return Some(std::borrow::Cow::Borrowed(args));
        }
        let param_names = Self::drawing_param_names(callee);
        let bound = self.bind_positional(callee, &param_names, args, span)?;
        Some(std::borrow::Cow::Owned(
            bound
                .into_iter()
                .map(|value| ast::CallArg {
                    span: value.span(),
                    name: None,
                    value,
                })
                .collect(),
        ))
    }

    /// The parameter names of a single-signature drawing member.
    fn drawing_param_names(callee: &str) -> Vec<&'static str> {
        let spec = pinewasm_hir::drawing::drawing_function_spec(callee)
            .expect("caller checked membership");
        debug_assert_eq!(spec.overloads.len(), 1, "`{callee}` is overloaded");
        spec.overloads[0]
            .iter()
            .map(|param| param.name.as_str())
            .collect()
    }

    /// `<ns>.delete(id)` — one object-handle argument; produces a void
    /// [`HirExprKind::DeleteDrawing`].
    pub(crate) fn analyze_drawing_delete(
        &mut self,
        kind: DrawKind,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        if args.len() != 1 {
            self.error(
                codes::E0238,
                format!(
                    "`{}.delete` takes exactly one object argument",
                    kind.namespace()
                ),
                span,
                "pass the drawing object to delete",
            );
            for a in args {
                let _ = self.analyze_expr(&a.value);
            }
            return self.error_expr(span);
        }
        let callee = format!("{}.delete", kind.namespace());
        let Some(args) = self.bind_drawing_args(&callee, args, span) else {
            return self.error_expr(span);
        };
        let id = self.analyze_expr(&args[0].value);
        let expected = Type::Object(kind.type_name().to_string());
        if !id.ty.is_error() && id.ty != expected {
            self.error(
                codes::E0238,
                format!(
                    "`{}.delete` expects a {} object, found {}",
                    kind.namespace(),
                    kind.namespace(),
                    id.ty.describe()
                ),
                args[0].value.span(),
                "wrong receiver type",
            );
            return self.error_expr(span);
        }
        HirExpr {
            kind: HirExprKind::DeleteDrawing {
                kind,
                id: Box::new(id),
            },
            ty: Type::Void,
            qualifier: Qualifier::Simple,
            span,
        }
    }

    /// Analyze the compound `(x,y)`/corner setters, the
    /// `chart.point`-taking setters, and `line.get_price`. Returns `None` when
    /// `member` is not one of these forms (so the caller falls through to the
    /// generic single-field `set_`/`get_` handling). Every form evaluates its
    /// receiver exactly once (via [`HirExprKind::SetFields`] /
    /// [`HirExprKind::LineGetPrice`]), so a `<ns>.new(...)` receiver is never
    /// re-registered.
    pub(crate) fn analyze_drawing_compound(
        &mut self,
        kind: DrawKind,
        member: &str,
        args: &[ast::CallArg],
        span: Span,
    ) -> Option<HirExpr> {
        // Compound coordinate setters: each trailing arg writes one field of
        // the receiver. `(field_idx, field_ty)` pairs follow the `drawing.rs`
        // schemas. `xy`-family setters write two adjacent coordinate fields;
        // `box.set_xloc` writes the two x-bounds plus the xloc token.
        let int = Type::Int;
        let float = Type::Float;
        let string = Type::String;
        let compound: Option<Vec<(u32, Type)>> = match (kind, member) {
            (DrawKind::Line, "set_xy1") => Some(vec![(0, int), (1, float)]),
            (DrawKind::Line, "set_xy2") => Some(vec![(2, int), (3, float)]),
            (DrawKind::Label, "set_xy") => Some(vec![(0, int), (1, float)]),
            (DrawKind::Box, "set_lefttop") => Some(vec![(0, int), (1, float)]),
            (DrawKind::Box, "set_rightbottom") => Some(vec![(2, int), (3, float)]),
            // The `set_xloc` family re-anchors the object: it rewrites the
            // x-coordinate(s) *and* the xloc token in one call, so it is a
            // compound setter rather than a single-field write.
            (DrawKind::Box, "set_xloc") => {
                let xloc_idx = kind.field_index("xloc").expect("box has an xloc field");
                Some(vec![(0, int), (2, Type::Int), (xloc_idx, string)])
            }
            (DrawKind::Line, "set_xloc") => {
                let xloc_idx = kind.field_index("xloc").expect("line has an xloc field");
                Some(vec![(0, int), (2, Type::Int), (xloc_idx, string)])
            }
            (DrawKind::Label, "set_xloc") => {
                let xloc_idx = kind.field_index("xloc").expect("label has an xloc field");
                Some(vec![(0, int), (xloc_idx, string)])
            }
            _ => None,
        };
        if let Some(fields) = compound {
            return Some(self.analyze_compound_setter(kind, member, &fields, args, span));
        }

        // `chart.point`-taking setters: write two coordinate fields of the
        // receiver from its xloc-selected coordinate and the point's price.
        let point_setter: Option<(u32, u32)> = match (kind, member) {
            (DrawKind::Line, "set_first_point") => Some((0, 1)),
            (DrawKind::Line, "set_second_point") => Some((2, 3)),
            (DrawKind::Label, "set_point") => Some((0, 1)),
            (DrawKind::Box, "set_top_left_point") => Some((0, 1)),
            (DrawKind::Box, "set_bottom_right_point") => Some((2, 3)),
            _ => None,
        };
        if let Some((x_idx, y_idx)) = point_setter {
            return Some(self.analyze_point_setter(kind, member, x_idx, y_idx, args, span));
        }

        // `line.get_price(id, x)` — interpolated price at bar `x`.
        if member == "get_price" && kind == DrawKind::Line {
            return Some(self.analyze_line_get_price(args, span));
        }
        None
    }

    /// Analyze a compound coordinate setter `<ns>.<member>(id, v1, v2, ...)`:
    /// bind named arguments in signature order, evaluate the receiver once,
    /// coerce each trailing arg, and emit one [`HirExprKind::SetFields`].
    pub(crate) fn analyze_compound_setter(
        &mut self,
        kind: DrawKind,
        member: &str,
        fields: &[(u32, Type)],
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        let arity = 1 + fields.len();
        if args.len() != arity {
            self.error(
                codes::E0238,
                format!("`{}.{member}` takes {arity} argument(s)", kind.namespace()),
                span,
                "pass the object then the coordinate values",
            );
            for a in args {
                let _ = self.analyze_expr(&a.value);
            }
            return self.error_expr(span);
        }
        // Bind in signature order so mixed and reordered named forms evaluate
        // the same way as the positional call.
        let callee = format!("{}.{member}", kind.namespace());
        let Some(args) = self.bind_drawing_args(&callee, args, span) else {
            return self.error_expr(span);
        };
        let obj = self.analyze_drawing_receiver(kind, member, &args[0], span);
        let mut writes = Vec::with_capacity(fields.len());
        let mut any_error = obj.ty.is_error();
        for ((field_idx, field_ty), arg) in fields.iter().zip(&args[1..]) {
            let hir = self.analyze_expr_expecting(&arg.value, Some(field_ty));
            match self.coerce_to_field(hir, field_ty, arg.value.span(), Span::dummy()) {
                Some(v) => writes.push((*field_idx, v)),
                None => any_error = true,
            }
        }
        if any_error {
            return self.error_expr(span);
        }
        let recv_slot =
            self.allocate_synthetic_slot(Type::Object(kind.type_name().to_string()), span);
        HirExpr {
            kind: HirExprKind::SetFields {
                obj: Box::new(obj),
                recv_slot,
                writes,
            },
            ty: Type::Void,
            qualifier: Qualifier::Simple,
            span,
        }
    }

    /// Analyze a `chart.point`-taking setter `<ns>.<member>(id, point)`:
    /// capture the receiver and point once, select time/index from xloc, and
    /// write them into the receiver's `x_idx`/`y_idx` fields via a single
    /// [`HirExprKind::SetFields`].
    pub(crate) fn analyze_point_setter(
        &mut self,
        kind: DrawKind,
        member: &str,
        x_idx: u32,
        y_idx: u32,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        let callee = format!("{}.{member}", kind.namespace());
        let Some(bound) = self.bind_optional_args(&callee, &["id", "point"], 2, args, span) else {
            return self.error_expr(span);
        };
        let receiver_arg = ast::CallArg {
            name: None,
            value: bound[0].as_ref().expect("required id").clone(),
            span,
        };
        let obj = self.analyze_drawing_receiver(kind, member, &receiver_arg, span);
        let point_arg = bound[1].as_ref().expect("required point");
        let point = self.analyze_expr(point_arg);
        let point_ty = Type::Object("chart.point".to_string());
        if !point.ty.is_error() && point.ty != point_ty {
            self.error(
                codes::E0238,
                format!(
                    "`{}.{member}` expects a chart.point, found {}",
                    kind.namespace(),
                    point.ty.describe()
                ),
                point_arg.span(),
                "wrong argument type",
            );
            return self.error_expr(span);
        }
        if obj.ty.is_error() || point.ty.is_error() {
            return self.error_expr(span);
        }
        let mut bindings = Vec::new();
        let obj = self.capture_drawing_operand(obj, &mut bindings);
        let point = self.capture_drawing_operand(point, &mut bindings);
        let xloc = HirExpr {
            kind: HirExprKind::GetField {
                obj: Box::new(obj.clone()),
                field_idx: kind.field_index("xloc").expect("point drawing has xloc"),
            },
            ty: Type::String,
            qualifier: Qualifier::Series,
            span,
        };
        let writes = vec![
            (x_idx, Self::drawing_point_x(&point, &xloc, span)),
            (
                y_idx,
                Self::drawing_point_field(&point, "price", Type::Float, span),
            ),
        ];
        let recv_slot =
            self.allocate_synthetic_slot(Type::Object(kind.type_name().to_string()), span);
        let result = HirExpr {
            kind: HirExprKind::SetFields {
                obj: Box::new(obj),
                recv_slot,
                writes,
            },
            ty: Type::Void,
            qualifier: Qualifier::Simple,
            span,
        };
        Self::drawing_eval_sequence(bindings, result)
    }

    /// `line.get_price(id, x)` — the interpolated price at bar `x`.
    pub(crate) fn analyze_line_get_price(&mut self, args: &[ast::CallArg], span: Span) -> HirExpr {
        if args.len() != 2 {
            self.error(
                codes::E0238,
                "`line.get_price` takes (line, x)".to_string(),
                span,
                "pass the line then the bar index",
            );
            for a in args {
                let _ = self.analyze_expr(&a.value);
            }
            return self.error_expr(span);
        }
        let Some(args) = self.bind_drawing_args("line.get_price", args, span) else {
            return self.error_expr(span);
        };
        let obj = self.analyze_drawing_getter_receiver(DrawKind::Line, "get_price", &args[0]);
        // The `x` argument is an integer bar index. Widen it only after the
        // signature check because interpolation arithmetic uses float values.
        let x_hir = self.analyze_expr_expecting(&args[1].value, Some(&Type::Int));
        let Some(x_index) =
            self.coerce_to_field(x_hir, &Type::Int, args[1].value.span(), Span::dummy())
        else {
            return self.error_expr(span);
        };
        let Some(x) =
            self.coerce_to_field(x_index, &Type::Float, args[1].value.span(), Span::dummy())
        else {
            return self.error_expr(span);
        };
        if obj.ty.is_error() {
            return self.error_expr(span);
        }
        // Evaluate the receiver, then `x`, once each and in that order, so an
        // `na` line still evaluates `x` before the getter returns `na`.
        let mut bindings = Vec::new();
        let obj = self.capture_drawing_operand(obj, &mut bindings);
        let x = self.capture_drawing_operand(x, &mut bindings);
        let recv_slot = self
            .allocate_synthetic_slot(Type::Object(DrawKind::Line.type_name().to_string()), span);
        let read = HirExpr {
            kind: HirExprKind::LineGetPrice {
                obj: Box::new(obj.clone()),
                recv_slot,
                x: Box::new(x),
            },
            ty: Type::Float,
            qualifier: Qualifier::Series,
            span,
        };
        Self::drawing_eval_sequence(bindings, Self::na_drawing_reads_na(&obj, read))
    }

    /// [`Self::analyze_drawing_receiver`] for a getter, whose `id` parameter
    /// also types a bare `na` argument: a getter on an `na` id returns `na`
    /// ([`Self::na_drawing_reads_na`]).
    fn analyze_drawing_getter_receiver(
        &mut self,
        kind: DrawKind,
        member: &str,
        arg: &ast::CallArg,
    ) -> HirExpr {
        let expected = Type::Object(kind.type_name().to_string());
        let obj = self.analyze_expr_expecting(&arg.value, Some(&expected));
        if !obj.ty.is_error() && obj.ty != expected {
            self.error(
                codes::E0238,
                format!(
                    "`{}.{member}` expects a {} object, found {}",
                    kind.namespace(),
                    kind.namespace(),
                    obj.ty.describe()
                ),
                arg.value.span(),
                "wrong receiver type",
            );
            return self.error_expr(arg.value.span());
        }
        obj
    }

    /// Analyze and type-check a drawing setter's receiver argument (the object
    /// handle), emitting a wrong-receiver diagnostic on a type mismatch.
    pub(crate) fn analyze_drawing_receiver(
        &mut self,
        kind: DrawKind,
        member: &str,
        arg: &ast::CallArg,
        _span: Span,
    ) -> HirExpr {
        let obj = self.analyze_expr(&arg.value);
        let expected = Type::Object(kind.type_name().to_string());
        if !obj.ty.is_error() && obj.ty != expected {
            self.error(
                codes::E0238,
                format!(
                    "`{}.{member}` expects a {} object, found {}",
                    kind.namespace(),
                    kind.namespace(),
                    obj.ty.describe()
                ),
                arg.value.span(),
                "wrong receiver type",
            );
            return self.error_expr(arg.value.span());
        }
        obj
    }

    /// `<ns>.set_<field>(id, value)` — a field write producing void.
    pub(crate) fn analyze_drawing_setter(
        &mut self,
        kind: DrawKind,
        field: &str,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        let Some((idx, field_ty)) = self.drawing_field(kind, field) else {
            self.error(
                codes::E0238,
                format!("`{}` has no field `{field}` to set", kind.namespace()),
                span,
                "unknown drawing field",
            );
            for a in args {
                let _ = self.analyze_expr(&a.value);
            }
            return self.error_expr(span);
        };
        let table_value_optional = kind == DrawKind::Table
            && matches!(
                field,
                "bgcolor" | "frame_color" | "frame_width" | "border_color" | "border_width"
            );
        if table_value_optional {
            let callee = format!("{}.set_{field}", kind.namespace());
            let Some(bound) = self.bind_optional_args(&callee, &["table_id", field], 1, args, span)
            else {
                return self.error_expr(span);
            };
            let Some(receiver) = bound[0].as_ref() else {
                return self.error_expr(span);
            };
            let obj = self.analyze_expr(receiver);
            let expected = Type::Object(kind.type_name().to_string());
            if !obj.ty.is_error() && obj.ty != expected {
                self.error(
                    codes::E0238,
                    format!(
                        "`{}.set_{field}` expects a {} object, found {}",
                        kind.namespace(),
                        kind.namespace(),
                        obj.ty.describe()
                    ),
                    receiver.span(),
                    "wrong receiver type",
                );
            }
            let value = if let Some(value) = bound[1].as_ref() {
                let value_hir = self.analyze_expr_expecting(value, Some(&field_ty));
                let Some(value) = self.coerce_drawing_field(
                    value_hir,
                    &field_ty,
                    value.span(),
                    Span::dummy(),
                    false,
                ) else {
                    return self.error_expr(span);
                };
                value
            } else {
                HirExpr {
                    kind: HirExprKind::ConstNa,
                    ty: field_ty,
                    qualifier: Qualifier::Const,
                    span,
                }
            };
            return HirExpr {
                kind: HirExprKind::SetFieldExpr {
                    obj: Box::new(obj),
                    field_idx: idx,
                    value: Box::new(value),
                },
                ty: Type::Void,
                qualifier: Qualifier::Simple,
                span,
            };
        }
        if args.len() != 2 {
            self.error(
                codes::E0238,
                format!("`{}.set_{field}` takes (object, value)", kind.namespace()),
                span,
                "pass the object then the new value",
            );
            for a in args {
                let _ = self.analyze_expr(&a.value);
            }
            return self.error_expr(span);
        }
        // The public setter argument can differ from its storage field
        // (`box.set_bgcolor(id, color)`). Bind against the public signature,
        // then write the value into the canonical object slot.
        let callee = format!("{}.set_{field}", kind.namespace());
        let Some(args) = self.bind_drawing_args(&callee, args, span) else {
            return self.error_expr(span);
        };
        let obj = self.analyze_expr(&args[0].value);
        let expected = Type::Object(kind.type_name().to_string());
        if !obj.ty.is_error() && obj.ty != expected {
            self.error(
                codes::E0238,
                format!(
                    "`{}.set_{field}` expects a {} object, found {}",
                    kind.namespace(),
                    kind.namespace(),
                    obj.ty.describe()
                ),
                args[0].value.span(),
                "wrong receiver type",
            );
        }
        let text_size_union = (kind == DrawKind::Label && field == "size")
            || (kind == DrawKind::Box && field == "text_size");
        let value_hir = if text_size_union {
            self.analyze_expr(&args[1].value)
        } else {
            self.analyze_expr_expecting(&args[1].value, Some(&field_ty))
        };
        let Some(value) = self.coerce_drawing_field(
            value_hir,
            &field_ty,
            args[1].value.span(),
            Span::dummy(),
            text_size_union,
        ) else {
            return self.error_expr(span);
        };
        HirExpr {
            kind: HirExprKind::SetFieldExpr {
                obj: Box::new(obj),
                field_idx: idx,
                value: Box::new(value),
            },
            ty: Type::Void,
            qualifier: Qualifier::Simple,
            span,
        }
    }

    /// `<ns>.get_<field>(id)` — a field read producing the field's type.
    pub(crate) fn analyze_drawing_getter(
        &mut self,
        kind: DrawKind,
        field: &str,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        let Some((idx, field_ty)) = self.drawing_field(kind, field) else {
            self.error(
                codes::E0238,
                format!("`{}` has no field `{field}` to get", kind.namespace()),
                span,
                "unknown drawing field",
            );
            for a in args {
                let _ = self.analyze_expr(&a.value);
            }
            return self.error_expr(span);
        };
        if args.len() != 1 {
            self.error(
                codes::E0238,
                format!(
                    "`{}.get_{field}` takes exactly one object",
                    kind.namespace()
                ),
                span,
                "pass the drawing object",
            );
            for a in args {
                let _ = self.analyze_expr(&a.value);
            }
            return self.error_expr(span);
        }
        let callee = format!("{}.get_{field}", kind.namespace());
        let Some(args) = self.bind_drawing_args(&callee, args, span) else {
            return self.error_expr(span);
        };
        // The `id` parameter types a bare `na` argument, as for any other
        // function argument.
        let expected = Type::Object(kind.type_name().to_string());
        let obj = self.analyze_expr_expecting(&args[0].value, Some(&expected));
        if !obj.ty.is_error() && obj.ty != expected {
            self.error(
                codes::E0238,
                format!(
                    "`{}.get_{field}` expects a {} object, found {}",
                    kind.namespace(),
                    kind.namespace(),
                    obj.ty.describe()
                ),
                args[0].value.span(),
                "wrong receiver type",
            );
            return self.error_expr(span);
        }
        if obj.ty.is_error() {
            return self.error_expr(span);
        }
        let mut bindings = Vec::new();
        let obj = self.capture_drawing_operand(obj, &mut bindings);
        let read = HirExpr {
            kind: HirExprKind::GetField {
                obj: Box::new(obj.clone()),
                field_idx: idx,
            },
            ty: field_ty,
            qualifier: Qualifier::Series,
            span,
        };
        Self::drawing_eval_sequence(bindings, Self::na_drawing_reads_na(&obj, read))
    }

    /// `na(obj) ? na : read` for a drawing getter whose receiver `obj` is
    /// already captured. A getter on an `na` drawing id returns `na` instead
    /// of stopping the script, so a drawing variable that starts as `na` and is
    /// assigned on a later bar reads `na` on the earlier bars. The ternary's
    /// arms are not literals, so MIR lowers it to a branch and `read` runs only
    /// for a live id. Field reads of a user-defined `na` object keep their
    /// structured error.
    fn na_drawing_reads_na(obj: &HirExpr, read: HirExpr) -> HirExpr {
        let span = read.span;
        let is_na = HirExpr {
            kind: HirExprKind::CallPure {
                func: PureFunc::Na,
                args: vec![obj.clone()],
            },
            ty: Type::Bool,
            qualifier: Qualifier::Series,
            span,
        };
        let na = HirExpr {
            kind: HirExprKind::ConstNa,
            ty: read.ty.clone(),
            qualifier: Qualifier::Const,
            span,
        };
        HirExpr {
            ty: read.ty.clone(),
            qualifier: Qualifier::Series,
            span,
            kind: HirExprKind::Ternary {
                cond: Box::new(is_na),
                then_e: Box::new(na),
                else_e: Box::new(read),
            },
        }
    }

    /// Dispatch the order-flow footprint namespaces. Returns `None`
    /// when `callee` is not a footprint member (so normal dispatch continues):
    /// - `request.footprint(...)` builds a `footprint` object from the optional
    ///   host table (mirrors `analyze_request_call`: identifying args must be
    ///   constant strings, joined into the selector; trailing optionals are
    ///   analyzed and ignored);
    /// - `footprint.<accessor>(fp)` / `volume_row.<accessor>(vr)` are field reads
    ///   over the seeded object types (an `na` receiver is a structured runtime
    ///   error, reusing the UDT [`HirExprKind::GetField`] handling);
    /// - `footprint.get_row_by_price(fp, price)` scans the footprint's rows and
    ///   yields the matching `volume_row` (or `na`).
    pub(crate) fn analyze_footprint_call(
        &mut self,
        callee: &ast::Ident,
        args: &[ast::CallArg],
        span: Span,
    ) -> Option<HirExpr> {
        use pinewasm_hir::footprint::{
            footprint_field, volume_row_field, FpFieldTy, FOOTPRINT_TYPE, VOLUME_ROW_TYPE,
        };

        if callee.name == "request.footprint" {
            return Some(self.analyze_footprint_ctor(args, span));
        }
        let (ns, member) = callee.name.split_once('.')?;
        if member.contains('.') {
            return None;
        }
        // `footprint.get_row_by_price(fp, price)` — a scan, not a field read.
        if ns == FOOTPRINT_TYPE && member == "get_row_by_price" {
            return Some(self.analyze_footprint_row_by_price(args, span));
        }
        let (type_name, field) = match ns {
            FOOTPRINT_TYPE => (FOOTPRINT_TYPE, footprint_field(member)?),
            VOLUME_ROW_TYPE => (VOLUME_ROW_TYPE, volume_row_field(member)?),
            _ => return None,
        };
        // Every accessor is a unary field read `<ns>.<member>(id)`, with the
        // receiver passed positionally or by its name `id`.
        let Some(bound) = self.bind_positional(&callee.name, &["id"], args, span) else {
            for a in args {
                let _ = self.analyze_expr(&a.value);
            }
            return Some(self.error_expr(span));
        };
        if bound.len() != 1 {
            self.error(
                codes::E0245,
                format!("`{ns}.{member}` takes exactly one {ns} object"),
                span,
                format!("pass the {ns} object"),
            );
            for a in &bound {
                let _ = self.analyze_expr(a);
            }
            return Some(self.error_expr(span));
        }
        let receiver = &bound[0];
        let obj = self.analyze_expr(receiver);
        let expected = Type::Object(type_name.to_string());
        if !obj.ty.is_error() && obj.ty != expected {
            self.error(
                codes::E0245,
                format!(
                    "`{ns}.{member}` expects a {ns} object, found {}",
                    obj.ty.describe()
                ),
                receiver.span(),
                "wrong receiver type",
            );
            return Some(self.error_expr(span));
        }
        let field_ty = match field.ty {
            FpFieldTy::Float => Type::Float,
            FpFieldTy::Bool => Type::Bool,
            FpFieldTy::RowsArray => {
                Type::Array(Box::new(Type::Object(VOLUME_ROW_TYPE.to_string())))
            }
        };
        // Every numeric and bool `footprint.*` and `volume_row.*` accessor
        // result is `series`: the footprint changes on every bar, so a value
        // derived from it is per-bar, cannot fill a `simple` parameter, and
        // `[]` on the call reads its history.
        Some(HirExpr {
            kind: HirExprKind::GetField {
                obj: Box::new(obj),
                field_idx: field.slot,
            },
            ty: field_ty,
            qualifier: Qualifier::Series,
            span,
        })
    }

    /// Analyze `request.footprint(...)`. Identifying string arguments (all
    /// present arguments that are constant strings, in order) are joined into
    /// the selector; a non-constant-string identifying argument is `E0245`.
    /// Named arguments are rejected. Produces a `footprint` object expression.
    pub(crate) fn analyze_footprint_ctor(&mut self, args: &[ast::CallArg], span: Span) -> HirExpr {
        for arg in args {
            if arg.name.is_some() {
                self.error(
                    codes::E0245,
                    "`request.footprint` does not take named arguments in this interface",
                    arg.value.span(),
                    "pass the identifying arguments positionally",
                );
                for a in args {
                    let _ = self.analyze_expr(&a.value);
                }
                return self.error_expr(span);
            }
        }
        // Build the selector from the leading constant-string arguments; the
        // remaining arguments are analyzed for well-formedness and otherwise
        // ignored.
        let mut selector = String::from("footprint");
        let mut ok = true;
        let mut in_prefix = true;
        for arg in args {
            let hir = self.analyze_expr(&arg.value);
            if hir.ty.is_error() {
                ok = false;
                continue;
            }
            if in_prefix {
                match &hir.kind {
                    HirExprKind::ConstString(s) => {
                        selector.push('/');
                        selector.push_str(s);
                    }
                    _ => in_prefix = false,
                }
            }
        }
        if !ok {
            return self.error_expr(span);
        }
        HirExpr {
            kind: HirExprKind::BuildFootprint { selector },
            ty: Type::Object(pinewasm_hir::footprint::FOOTPRINT_TYPE.to_string()),
            qualifier: Qualifier::Series,
            span,
        }
    }

    /// Analyze `footprint.get_row_by_price(fp, price)` — the row whose price
    /// range contains `price`, or `na`. `fp` must be a `footprint`; `price` is
    /// coerced to float. Lowers to [`HirExprKind::FootprintRowByPrice`].
    pub(crate) fn analyze_footprint_row_by_price(
        &mut self,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        use pinewasm_hir::footprint::{FOOTPRINT_TYPE, VOLUME_ROW_TYPE};
        let Some(bound) =
            self.bind_positional("footprint.get_row_by_price", &["id", "price"], args, span)
        else {
            for a in args {
                let _ = self.analyze_expr(&a.value);
            }
            return self.error_expr(span);
        };
        if bound.len() != 2 {
            self.error(
                codes::E0245,
                "`footprint.get_row_by_price` takes a footprint object and a price",
                span,
                "pass the footprint object and a price level",
            );
            for a in &bound {
                let _ = self.analyze_expr(a);
            }
            return self.error_expr(span);
        }
        let obj = self.analyze_expr(&bound[0]);
        let expected = Type::Object(FOOTPRINT_TYPE.to_string());
        if !obj.ty.is_error() && obj.ty != expected {
            self.error(
                codes::E0245,
                format!(
                    "`footprint.get_row_by_price` expects a footprint object, found {}",
                    obj.ty.describe()
                ),
                bound[0].span(),
                "wrong receiver type",
            );
            return self.error_expr(span);
        }
        let price = self.analyze_expr_expecting(&bound[1], Some(&Type::Float));
        let price = match self.coerce_to_field(price, &Type::Float, bound[1].span(), span) {
            Some(p) => p,
            None => return self.error_expr(span),
        };
        HirExpr {
            kind: HirExprKind::FootprintRowByPrice {
                footprint: Box::new(obj),
                price: Box::new(price),
            },
            ty: Type::Object(VOLUME_ROW_TYPE.to_string()),
            // The row comes from the bar's footprint and a `series` price.
            qualifier: Qualifier::Series,
            span,
        }
    }

    /// Dispatch the table per-cell members: `table.cell`,
    /// `table.cell_set_<attr>`, `table.clear`, `table.merge_cells`. Returns
    /// `None` when `member` is not one of these (so the caller falls through to
    /// the generic field setter/getter and its unknown-member diagnostic).
    pub(crate) fn analyze_table_cell_call(
        &mut self,
        member: &str,
        args: &[ast::CallArg],
        span: Span,
    ) -> Option<HirExpr> {
        match member {
            "cell" => Some(self.analyze_table_cell(args, span)),
            "clear" => Some(self.analyze_table_clear(args, span)),
            "merge_cells" => Some(self.analyze_table_merge_cells(args, span)),
            _ => {
                let suffix = member.strip_prefix("cell_set_")?;
                let attr = CellAttr::from_tag(suffix)?;
                Some(self.analyze_table_cell_set(attr, args, span))
            }
        }
    }

    /// A table-cell coordinate argument: analyze it, coerce to `int`.
    pub(crate) fn analyze_cell_coord(&mut self, arg: &ast::CallArg) -> Option<HirExpr> {
        let hir = self.analyze_expr_expecting(&arg.value, Some(&Type::Int));
        self.coerce_to_field(hir, &Type::Int, arg.value.span(), Span::dummy())
    }

    /// Analyze the receiver of a table-cell call: it must be a `table` object.
    pub(crate) fn analyze_table_receiver(&mut self, member: &str, arg: &ast::CallArg) -> HirExpr {
        let obj = self.analyze_expr(&arg.value);
        let expected = Type::Object(DrawKind::Table.type_name().to_string());
        if !obj.ty.is_error() && obj.ty != expected {
            self.error(
                codes::E0238,
                format!(
                    "`table.{member}` expects a table object, found {}",
                    obj.ty.describe()
                ),
                arg.value.span(),
                "wrong receiver type",
            );
            return self.error_expr(arg.value.span());
        }
        obj
    }

    /// `table.cell(t, column, row, text?, width?, height?, text_color?,
    /// text_halign?, text_valign?, text_size?, bgcolor?, tooltip?,
    /// text_font_family?)` — replace one cell, including omitted defaults.
    /// All parameters may be given positionally or by name.
    pub(crate) fn analyze_table_cell(&mut self, args: &[ast::CallArg], span: Span) -> HirExpr {
        let params = CellAttr::cell_param_order();
        let mut bound: Vec<Option<&ast::CallArg>> = vec![None; 3 + params.len()];
        let mut positional = 0;
        let mut any_error = false;
        for arg in args {
            let slot = match &arg.name {
                Some(name) => match name.name.as_str() {
                    "table_id" => Some(0),
                    "column" => Some(1),
                    "row" => Some(2),
                    other => params
                        .iter()
                        .position(|attr| attr.name() == other)
                        .map(|i| i + 3),
                },
                None => {
                    let slot = positional;
                    positional += 1;
                    (slot < bound.len()).then_some(slot)
                }
            };
            let Some(slot) = slot else {
                self.error(
                    codes::E0238,
                    match &arg.name {
                        Some(name) => format!("`table.cell` has no parameter `{}`", name.name),
                        None => "too many positional arguments to `table.cell`".to_string(),
                    },
                    arg.value.span(),
                    "unknown or extra cell argument",
                );
                any_error = true;
                continue;
            };
            if bound[slot].is_some() {
                self.error(
                    codes::E0238,
                    "`table.cell` parameter given twice",
                    arg.value.span(),
                    "duplicate cell argument",
                );
                any_error = true;
                continue;
            }
            bound[slot] = Some(arg);
        }
        for (index, name) in ["table_id", "column", "row"].iter().enumerate() {
            if bound[index].is_none() {
                self.error(
                    codes::E0238,
                    format!("`table.cell` is missing required parameter `{name}`"),
                    span,
                    "required cell argument is missing",
                );
                any_error = true;
            }
        }
        if any_error {
            for arg in args {
                let _ = self.analyze_expr(&arg.value);
            }
            return self.error_expr(span);
        }

        // Analyze table.cell arguments in parameter order, even when the caller
        // names them in a different order. Inlined helpers emit effects during
        // analysis, so changing this order can change the effects seen at runtime.
        let obj = self.analyze_table_receiver("cell", bound[0].unwrap());
        let column = self.analyze_cell_coord(bound[1].unwrap());
        let row = self.analyze_cell_coord(bound[2].unwrap());
        let mut writes: Vec<(CellAttr, HirExpr)> = Vec::new();
        let mut any_error = obj.ty.is_error() || column.is_none() || row.is_none();
        for (index, attr) in params.iter().enumerate() {
            let ty = attr.ty();
            let Some(arg) = bound[index + 3] else {
                writes.push((*attr, self.drawing_field_default(&attr.default(), &ty)));
                continue;
            };
            let text_size_union = *attr == CellAttr::TextSize;
            let hir = if text_size_union {
                self.analyze_expr(&arg.value)
            } else {
                self.analyze_expr_expecting(&arg.value, Some(&ty))
            };
            match self.coerce_drawing_field(
                hir,
                &ty,
                arg.value.span(),
                Span::dummy(),
                text_size_union,
            ) {
                Some(value) => writes.push((*attr, value)),
                None => any_error = true,
            }
        }
        if any_error {
            return self.error_expr(span);
        }
        self.build_set_cell(obj, column.unwrap(), row.unwrap(), writes, span)
    }

    /// `table.cell_set_<attr>(t, column, row, value)` — mutate one attribute of
    /// an existing cell.
    pub(crate) fn analyze_table_cell_set(
        &mut self,
        attr: CellAttr,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        let callee = format!("table.cell_set_{}", attr.setter_suffix());
        let value_required = matches!(attr, CellAttr::TextFontFamily | CellAttr::TextFormatting);
        let value_parameter = attr.name();
        let Some(bound) = self.bind_optional_args(
            &callee,
            &["table_id", "column", "row", value_parameter],
            if value_required { 4 } else { 3 },
            args,
            span,
        ) else {
            return self.error_expr(span);
        };
        let bound_args: Vec<_> = bound
            .iter()
            .map(|value| {
                value.as_ref().map(|value| ast::CallArg {
                    name: None,
                    value: value.clone(),
                    span: value.span(),
                })
            })
            .collect();
        let obj = self.analyze_table_receiver(
            &format!("cell_set_{}", attr.setter_suffix()),
            bound_args[0].as_ref().expect("table_id required"),
        );
        let column = self.analyze_cell_coord(bound_args[1].as_ref().expect("column required"));
        let row = self.analyze_cell_coord(bound_args[2].as_ref().expect("row required"));
        let ty = attr.ty();
        let text_size_union = attr == CellAttr::TextSize;
        let value_hir = if let Some(value) = bound_args[3].as_ref() {
            if text_size_union {
                self.analyze_expr(&value.value)
            } else {
                self.analyze_expr_expecting(&value.value, Some(&ty))
            }
        } else {
            HirExpr {
                kind: HirExprKind::ConstNa,
                ty: ty.clone(),
                qualifier: Qualifier::Const,
                span,
            }
        };
        let value = if text_size_union {
            self.coerce_drawing_field(value_hir, &ty, span, Span::dummy(), true)
        } else {
            self.coerce_drawing_field(value_hir, &ty, span, Span::dummy(), false)
        };
        if obj.ty.is_error() || column.is_none() || row.is_none() || value.is_none() {
            return self.error_expr(span);
        }
        self.build_set_cell(
            obj,
            column.unwrap(),
            row.unwrap(),
            vec![(attr, value.unwrap())],
            span,
        )
    }

    /// Build a [`HirExprKind::SetCell`] with a freshly allocated receiver slot.
    pub(crate) fn build_set_cell(
        &mut self,
        obj: HirExpr,
        column: HirExpr,
        row: HirExpr,
        writes: Vec<(CellAttr, HirExpr)>,
        span: Span,
    ) -> HirExpr {
        let recv_slot = self
            .allocate_synthetic_slot(Type::Object(DrawKind::Table.type_name().to_string()), span);
        HirExpr {
            kind: HirExprKind::SetCell {
                obj: Box::new(obj),
                recv_slot,
                column: Box::new(column),
                row: Box::new(row),
                writes,
            },
            ty: Type::Void,
            qualifier: Qualifier::Simple,
            span,
        }
    }

    /// `table.clear(t, start_column, start_row, end_column?, end_row?)`.
    pub(crate) fn analyze_table_clear(&mut self, args: &[ast::CallArg], span: Span) -> HirExpr {
        if !(3..=5).contains(&args.len()) {
            self.error(
                codes::E0238,
                "`table.clear` takes (table, start_column, start_row, end_column?, end_row?)"
                    .to_string(),
                span,
                "pass the table then the cell range",
            );
            for a in args {
                let _ = self.analyze_expr(&a.value);
            }
            return self.error_expr(span);
        }
        // Either end coordinate may be named without the other, so keep the
        // omitted one as a gap instead of flattening to a positional list.
        let param_names = Self::drawing_param_names("table.clear");
        let Some(bound) = self.bind_optional_args("table.clear", &param_names, 3, args, span)
        else {
            return self.error_expr(span);
        };
        let args: Vec<_> = bound
            .into_iter()
            .map(|value| {
                value.map(|value| ast::CallArg {
                    span: value.span(),
                    name: None,
                    value,
                })
            })
            .collect();
        let obj =
            self.analyze_table_receiver("clear", args[0].as_ref().expect("table_id required"));
        let sc = self.analyze_cell_coord(args[1].as_ref().expect("start_column required"));
        let sr = self.analyze_cell_coord(args[2].as_ref().expect("start_row required"));
        // The end coordinates default to the start (a single-cell clear).
        let ec = match &args[3] {
            Some(a) => self.analyze_cell_coord(a),
            None => sc.clone(),
        };
        let er = match &args[4] {
            Some(a) => self.analyze_cell_coord(a),
            None => sr.clone(),
        };
        if obj.ty.is_error() || sc.is_none() || sr.is_none() || ec.is_none() || er.is_none() {
            return self.error_expr(span);
        }
        let recv_slot = self
            .allocate_synthetic_slot(Type::Object(DrawKind::Table.type_name().to_string()), span);
        HirExpr {
            kind: HirExprKind::ClearCells {
                obj: Box::new(obj),
                recv_slot,
                start_column: Box::new(sc.unwrap()),
                start_row: Box::new(sr.unwrap()),
                end_column: Box::new(ec.unwrap()),
                end_row: Box::new(er.unwrap()),
            },
            ty: Type::Void,
            qualifier: Qualifier::Simple,
            span,
        }
    }

    /// `table.merge_cells(t, start_column, start_row, end_column, end_row)`.
    pub(crate) fn analyze_table_merge_cells(
        &mut self,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        if args.len() != 5 {
            self.error(
                codes::E0238,
                "`table.merge_cells` takes (table, start_column, start_row, end_column, end_row)"
                    .to_string(),
                span,
                "pass the table then the merge range",
            );
            for a in args {
                let _ = self.analyze_expr(&a.value);
            }
            return self.error_expr(span);
        }
        let Some(args) = self.bind_drawing_args("table.merge_cells", args, span) else {
            return self.error_expr(span);
        };
        let obj = self.analyze_table_receiver("merge_cells", &args[0]);
        let sc = self.analyze_cell_coord(&args[1]);
        let sr = self.analyze_cell_coord(&args[2]);
        let ec = self.analyze_cell_coord(&args[3]);
        let er = self.analyze_cell_coord(&args[4]);
        if obj.ty.is_error() || sc.is_none() || sr.is_none() || ec.is_none() || er.is_none() {
            return self.error_expr(span);
        }
        let recv_slot = self
            .allocate_synthetic_slot(Type::Object(DrawKind::Table.type_name().to_string()), span);
        HirExpr {
            kind: HirExprKind::MergeCells {
                obj: Box::new(obj),
                recv_slot,
                start_column: Box::new(sc.unwrap()),
                start_row: Box::new(sr.unwrap()),
                end_column: Box::new(ec.unwrap()),
                end_row: Box::new(er.unwrap()),
            },
            ty: Type::Void,
            qualifier: Qualifier::Simple,
            span,
        }
    }

    /// The (slot index, type) of `field` on drawing `kind`, or `None`.
    ///
    /// Constructor-only fields are hidden from this lookup: `force_overlay` is
    /// a `.new()` argument with no `set_`/`get_` member, and deriving the
    /// accessors from the field table would generate accessors the drawing
    /// surface does not define.
    pub(crate) fn drawing_field(&self, kind: DrawKind, field: &str) -> Option<(u32, Type)> {
        kind.fields()
            .iter()
            .enumerate()
            .find(|(_, f)| f.name == field && f.has_accessors())
            .map(|(i, f)| (i as u32, f.ty()))
    }

    /// Analyze an object construction `Name.new(field = value, ...)` (positional or
    /// named args). Each field's initializer is coerced to the field's declared
    /// type (int → float widening allowed); an omitted field takes its declared
    /// default, or `na` of the field type when it has none. A field-type
    /// mismatch is `E0237`. Produces a [`HirExprKind::NewObject`] typed
    /// `Type::Object(name)`.
    pub(crate) fn analyze_object_new(
        &mut self,
        type_name: &str,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        // Snapshot the field list (name, type, whether a default exists) so the
        // analyzer can borrow `self` mutably while iterating.
        let info = self
            .types
            .get(type_name)
            .expect("caller checked membership");
        let type_id = info.type_id;
        let field_names: Vec<String> = info.fields.iter().map(|f| f.name.clone()).collect();
        let field_types: Vec<Type> = info.fields.iter().map(|f| f.ty.clone()).collect();
        let field_defaults: Vec<Option<HirExpr>> =
            info.fields.iter().map(|f| f.default.clone()).collect();
        let field_spans: Vec<Span> = info.fields.iter().map(|f| f.span).collect();

        // Drawing constructors have required coordinates/handles even though
        // their internal object fields carry `na` defaults for allocation and
        // mutation. Keep UDT `.new()` semantics optional, but enforce the
        // required parameters that `drawing_function_spec` declares for
        // drawing-object constructors.
        let drawing_required: Vec<bool> = DrawKind::all()
            .iter()
            .copied()
            .find(|kind| *kind != DrawKind::Point && kind.type_name() == type_name)
            .and_then(|kind| {
                let name = format!("{}.new", kind.namespace());
                pinewasm_hir::drawing::drawing_function_spec(&name)
                    .and_then(|spec| {
                        spec.overloads.iter().find(|overload| {
                            overload.len() == field_names.len()
                                && overload
                                    .iter()
                                    .zip(&field_names)
                                    .all(|(param, field)| param.name == *field)
                        })
                    })
                    .map(|overload| overload.iter().map(|param| param.required).collect())
            })
            .unwrap_or_else(|| vec![false; field_names.len()]);

        let name_refs: Vec<&str> = field_names.iter().map(String::as_str).collect();
        let Some((bound, overflow)) = self.bind_call_args(type_name, &name_refs, args) else {
            return self.error_expr(span);
        };
        if !overflow.is_empty() {
            self.error(
                codes::E0237,
                format!(
                    "too many arguments for `{type_name}.new` (type `{type_name}` has {} field(s))",
                    field_names.len()
                ),
                span,
                "each argument sets one declared field",
            );
            for e in overflow {
                let _ = self.analyze_expr(e);
            }
            return self.error_expr(span);
        }

        let mut field_values: Vec<HirExpr> = Vec::with_capacity(field_names.len());
        let mut any_error = false;
        let mut qualifier = Qualifier::Const;
        for (i, arg) in bound.iter().enumerate() {
            let field_ty = &field_types[i];
            if drawing_required.get(i).copied().unwrap_or(false) && arg.is_none() {
                self.error(
                    codes::E0204,
                    format!(
                        "`{type_name}.new` is missing required argument `{}`",
                        field_names[i]
                    ),
                    span,
                    "this coordinate or handle is required",
                );
                any_error = true;
                continue;
            }
            let value = match arg {
                Some(expr) => {
                    let text_size_union = (type_name == "label" && field_names[i] == "size")
                        || (type_name == "box" && field_names[i] == "text_size");
                    let hir = if text_size_union {
                        self.analyze_expr(expr)
                    } else {
                        self.analyze_expr_expecting(expr, Some(field_ty))
                    };
                    let is_drawing_object = DrawKind::all()
                        .iter()
                        .any(|kind| kind != &DrawKind::Point && kind.type_name() == type_name);
                    if is_drawing_object && field_names[i] == "force_overlay" {
                        self.defer_temporal_fixedness_at(
                            &hir,
                            &format!("{type_name}.new"),
                            "force_overlay",
                            expr.span(),
                        );
                    }
                    if is_drawing_object
                        && field_names[i] == "force_overlay"
                        && hir.qualifier != Qualifier::Const
                    {
                        self.error(
                            codes::E0206,
                            format!(
                                "`{type_name}.new` argument `force_overlay` must be const bool"
                            ),
                            expr.span(),
                            "force_overlay must be a const bool",
                        );
                        any_error = true;
                        continue;
                    }
                    match self.coerce_drawing_field(
                        hir,
                        field_ty,
                        expr.span(),
                        field_spans[i],
                        text_size_union,
                    ) {
                        Some(v) => v,
                        None => {
                            any_error = true;
                            continue;
                        }
                    }
                }
                None => match &field_defaults[i] {
                    Some(default) => default.clone(),
                    // A bool field is never `na`, so an unset bool field is false.
                    None if *field_ty == Type::Bool => Self::const_bool_expr(false, span),
                    None => HirExpr {
                        kind: HirExprKind::ConstNa,
                        ty: field_ty.clone(),
                        qualifier: Qualifier::Const,
                        span,
                    },
                },
            };
            qualifier = qualifier.join(value.qualifier);
            field_values.push(value);
        }
        if any_error {
            return self.error_expr(span);
        }
        HirExpr {
            kind: HirExprKind::NewObject {
                type_id,
                fields: field_values,
            },
            ty: Type::Object(type_name.to_string()),
            // A constructed object is a fresh heap value each bar it runs;
            // treat it as at least simple (it is not a compile-time constant).
            qualifier: qualifier.join(Qualifier::Simple),
            span,
        }
    }

    /// Analyze an object shallow copy `Name.copy(obj)`: a NEW handle whose fields
    /// are copied from `obj`. `obj` must be an object of the same type. Produces
    /// a [`HirExprKind::CopyObject`].
    pub(crate) fn analyze_object_copy(
        &mut self,
        type_name: &str,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        let type_id = self
            .types
            .get(type_name)
            .expect("caller checked membership")
            .type_id;
        let callee = format!("{type_name}.copy");
        let Some((bound, overflow)) = self.bind_call_args(&callee, &["id"], args) else {
            return self.error_expr(span);
        };
        if !overflow.is_empty() {
            self.error(
                codes::E0237,
                format!("`{type_name}.copy` takes exactly one argument"),
                span,
                "call it as `{type_name}.copy(id)`",
            );
            for expr in overflow {
                let _ = self.analyze_expr(expr);
            }
            return self.error_expr(span);
        }
        let Some(obj_expr) = bound[0] else {
            self.error(
                codes::E0237,
                format!("`{type_name}.copy` is missing required argument `id`"),
                span,
                "pass the object to copy",
            );
            return self.error_expr(span);
        };
        let expected = Type::Object(type_name.to_string());
        let obj = self.analyze_expr_expecting(obj_expr, Some(&expected));
        if obj.ty.is_error() {
            return self.error_expr(span);
        }
        if obj.ty != expected {
            self.error(
                codes::E0237,
                format!(
                    "`{type_name}.copy` expects a `{type_name}`, found {}",
                    obj.ty.describe()
                ),
                obj_expr.span(),
                format!("expected an object of type `{type_name}`"),
            );
            return self.error_expr(span);
        }
        let qualifier = obj.qualifier.join(Qualifier::Simple);
        HirExpr {
            kind: HirExprKind::CopyObject {
                type_id,
                obj: Box::new(obj),
            },
            ty: expected,
            qualifier,
            span,
        }
    }

    /// Coerce `value` to a UDT field's declared type: exact match passes, an
    /// `int` widens to a `float` field (`IntToFloat`); any other mismatch is
    /// `E0237`. Returns `None` after reporting a mismatch.
    pub(crate) fn coerce_to_field(
        &mut self,
        value: HirExpr,
        field_ty: &Type,
        value_span: Span,
        field_span: Span,
    ) -> Option<HirExpr> {
        if value.ty.is_error() {
            return None;
        }
        if value.ty == *field_ty {
            return Some(value);
        }
        if *field_ty == Type::Float && value.ty == Type::Int {
            return Some(Self::coerce_int_to_float(value));
        }
        self.diagnostics.push(
            Diagnostic::error(
                codes::E0237,
                format!(
                    "field expects {}, found {}",
                    field_ty.describe(),
                    value.ty.describe()
                ),
            )
            .with_label(
                field_span,
                format!("field declared as {}", field_ty.describe()),
            )
            .with_label(value_span, format!("value is {}", value.ty.describe())),
        );
        None
    }

    /// Drawing text sizes accept either a named `size.*` string or a
    /// numeric point size. Keep the runtime int/string value intact so hosts
    /// can render custom sizes instead of rounding them to a preset.
    fn coerce_drawing_field(
        &mut self,
        value: HirExpr,
        field_ty: &Type,
        value_span: Span,
        field_span: Span,
        text_size_union: bool,
    ) -> Option<HirExpr> {
        if !text_size_union {
            return self.coerce_to_field(value, field_ty, value_span, field_span);
        }
        if value.ty.is_error() {
            return None;
        }
        if matches!(value.ty, Type::Int | Type::String) {
            return Some(value);
        }
        self.diagnostics.push(
            Diagnostic::error(
                codes::E0237,
                format!(
                    "text size expects int or string, found {}",
                    value.ty.describe()
                ),
            )
            .with_label(
                field_span,
                "text size accepts numeric point sizes or size.* values",
            )
            .with_label(value_span, format!("value is {}", value.ty.describe())),
        );
        None
    }
}
