//! `input()`/`input.*` call analysis: input declarations, `defval`/options/
//! constraint validation, and input-kind inference.

use crate::*;

/// The number a folded int or float holds.
fn numeric(value: &InputValue) -> Option<f64> {
    match value {
        InputValue::Int(value) => Some(*value as f64),
        InputValue::Float(value) => Some(*value),
        _ => None,
    }
}

/// A folded float, or `None` when `value` is not finite.
fn finite_float(value: f64) -> Option<InputValue> {
    value.is_finite().then_some(InputValue::Float(value))
}

#[derive(Debug, Clone, PartialEq)]
enum ActiveScalar {
    Na,
    Int(i64),
    Float(f64),
    Bool(bool),
    String(String),
    Color(u8, u8, u8, f64),
    Enum(String, u32),
}

/// Semantic type of a runtime input read. This stays in the analyzer rather
/// than on the host-facing [`InputKind`] DTO so facade users do not inherit an
/// implementation-only HIR type through an otherwise public inherent method.
pub(crate) fn input_kind_type(kind: InputKind) -> Type {
    match kind {
        InputKind::Int => Type::Int,
        InputKind::Float => Type::Float,
        InputKind::Bool => Type::Bool,
        InputKind::String => Type::String,
        InputKind::Color => Type::Color,
        InputKind::Source => Type::Float,
        InputKind::Price => Type::Float,
        InputKind::Time => Type::Int,
        // The declaring enum name lives in `InputValue::Enum`; call sites
        // replace this placeholder with `Type::Enum(name)`.
        InputKind::Enum => Type::Int,
    }
}

/// Qualifier of a runtime input read. Source selections remain series values;
/// every scalar input is input-qualified.
fn input_kind_qualifier(kind: InputKind) -> Qualifier {
    match kind {
        InputKind::Source => Qualifier::Series,
        _ => Qualifier::Input,
    }
}

impl<'r> Analyzer<'r> {
    /// Analyze the generic `input(defval, ...)` overload. Dispatches on the
    /// `defval` literal's type to the matching namespaced input kind, then
    /// reuses the shared `input.*` machinery (a semantics-level desugar; the
    /// host-override resolution is unchanged). The mapping is:
    /// int→`input.int`, float→`input.float`, bool→`input.bool`,
    /// string→`input.string`, color→`input.color`, a builtin source series
    /// ident→`input.source`. An enum member is **not** dispatched here (the
    /// explicit `input.enum` is used for that); an unclassifiable `defval` is
    /// an `E0230` type error.
    pub(crate) fn analyze_input_generic_call(
        &mut self,
        func: &BuiltinFunction,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        // Locate the `defval`: the first `defval = ...` named argument, else the
        // first positional argument.
        let defval_arg = args
            .iter()
            .find(|a| a.name.as_ref().is_some_and(|n| n.name == "defval"))
            .or_else(|| args.iter().find(|a| a.name.is_none()))
            .map(|a| &a.value);
        let Some(defval_arg) = defval_arg else {
            self.error(
                codes::E0230,
                "`input` requires a `defval` argument",
                span,
                "add the default value, e.g. input(14)",
            );
            return self.error_expr(span);
        };
        // `input.source` is the one generic overload whose default is
        // deliberately series-qualified. Every scalar overload accepts any
        // expression whose *result* is const, including const aliases and
        // const-returning pure calls; classify that evaluated value rather
        // than matching a handful of literal AST shapes.
        let source = match Self::ungroup_expr(defval_arg) {
            ast::Expr::Ident(ident) => self
                .resolve_input_source_ident(ident)
                .map(InputValue::Source),
            _ => None,
        };
        let evaluated = source.or_else(|| self.eval_const_input_expr(defval_arg));
        let target: Option<&'static str> = match &evaluated {
            Some(InputValue::Int(_)) => Some("input.int"),
            Some(InputValue::Float(_)) => Some("input.float"),
            Some(InputValue::Bool(_)) => Some("input.bool"),
            Some(InputValue::String(_)) => Some("input.string"),
            Some(InputValue::Color { .. }) => Some("input.color"),
            Some(InputValue::Source(_)) => Some("input.source"),
            // The generic overload does not infer enum inputs.
            Some(InputValue::Enum { .. } | InputValue::ExternalSource(_)) | None => None,
        };
        let Some(target) = target else {
            self.error(
                codes::E0230,
                "`input` could not infer the input type from its `defval`",
                defval_arg.span(),
                "use a const int/float/bool/string/color expression, one of the eight price source series, or a namespaced form like input.enum(...)",
            );
            return self.error_expr(span);
        };
        let kind = match target {
            "input.int" => InputKind::Int,
            "input.float" => InputKind::Float,
            "input.bool" => InputKind::Bool,
            "input.string" => InputKind::String,
            "input.color" => InputKind::Color,
            "input.source" => InputKind::Source,
            _ => {
                let _ = func;
                return self.error_expr(span);
            }
        };
        // Generic `input()` has its own parameter lists: it does not
        // accept `confirm`, and its source overload places `tooltip` after
        // `inline` and `group`. Do not bind it through the namespaced target.
        self.analyze_input_call_impl("input", kind, args, span, true, evaluated)
    }

    /// Analyze an `input.*(...)` declaration. Binds arguments (positional +
    /// named), enforces that every argument is a compile-time constant of the
    /// right type (`E0230` on a violation), records an [`InputDecl`] in the
    /// program metadata, and returns a [`HirExprKind::ReadInput`] whose type
    /// and qualifier come from the [`InputKind`] (`input`-qualified for scalar
    /// inputs, `series float` for `input.source`).
    pub(crate) fn analyze_input_call(
        &mut self,
        func: &BuiltinFunction,
        kind: InputKind,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        self.analyze_input_call_impl(func.name, kind, args, span, false, None)
    }

    fn analyze_input_call_impl(
        &mut self,
        callee: &'static str,
        kind: InputKind,
        args: &[ast::CallArg],
        span: Span,
        generic: bool,
        precomputed_defval: Option<InputValue>,
    ) -> HirExpr {
        // Several input widgets return the same `input string` runtime type.
        // Keep that source-level widget identity independent of `InputKind`
        // so that no VM or qualifier behavior changes.
        let widget = match callee {
            "input.string" => InputWidget::String,
            "input.text_area" => InputWidget::TextArea,
            "input.timeframe" => InputWidget::Timeframe,
            "input.session" => InputWidget::Session,
            "input.symbol" => InputWidget::Symbol,
            "input" if kind == InputKind::String => InputWidget::String,
            _ => InputWidget::Standard,
        };
        if matches!(kind, InputKind::Int | InputKind::Float) && !generic {
            let has_options = args
                .iter()
                .any(|arg| arg.name.as_ref().is_some_and(|name| name.name == "options"));
            let has_range = args.iter().any(|arg| {
                arg.name
                    .as_ref()
                    .is_some_and(|name| matches!(name.name.as_str(), "minval" | "maxval" | "step"))
            });
            let third_positional = args.iter().filter(|arg| arg.name.is_none()).nth(2);
            let positional_options =
                third_positional.is_some_and(|arg| Self::is_tuple_literal(&arg.value));
            let positional_range = third_positional.is_some() && !positional_options;
            if (has_options && (has_range || positional_range)) || (has_range && positional_options)
            {
                self.error(
                    codes::E0230,
                    format!(
                        "`{callee}` accepts either `minval`/`maxval`/`step` or `options`, not both"
                    ),
                    span,
                    "these are separate overloads",
                );
                return self.error_expr(span);
            }
        }
        let param_names = Self::input_signature(callee, kind, args, generic);
        // Bind positional + named args into per-parameter slots.
        let Some((bound, overflow)) = self.bind_call_args(callee, param_names, args) else {
            return self.error_expr(span);
        };
        if !overflow.is_empty() {
            self.error(
                codes::E0230,
                format!("`{callee}` was given too many arguments"),
                span,
                format!("expected at most {} argument(s)", param_names.len()),
            );
            return self.error_expr(span);
        }
        // `defval` (parameter 0) is required.
        if bound.first().copied().flatten().is_none() {
            self.error(
                codes::E0230,
                format!("`{callee}` requires a `defval` argument"),
                span,
                "add the default value, e.g. input.int(14)",
            );
            return self.error_expr(span);
        }

        // Evaluate the default and the title (shared by every kind).
        let defval_arg = bound[0].expect("defval presence checked above");
        let Some(defval) = precomputed_defval
            .or_else(|| self.eval_input_value(callee, "defval", kind, defval_arg))
        else {
            return self.error_expr(span);
        };
        let title = match bound.get(1).copied().flatten() {
            None => None,
            Some(arg) => match self.eval_const_string(callee, "title", arg) {
                Some(s) => Some(s),
                None => return self.error_expr(span),
            },
        };

        // Preserve host-facing layout metadata in the manifest. Input
        // signatures do not put these fields at the same numeric offsets, so
        // resolve them by their canonical parameter names rather than baking
        // in per-overload indices.
        let bound_named = |name: &str| {
            param_names
                .iter()
                .position(|candidate| *candidate == name)
                .and_then(|index| bound.get(index).copied().flatten())
        };
        let tooltip = match bound_named("tooltip") {
            None => None,
            Some(arg) => match self.eval_const_string(callee, "tooltip", arg) {
                Some(value) => Some(value),
                None => return self.error_expr(span),
            },
        };
        let inline = match bound_named("inline") {
            None => None,
            Some(arg) => match self.eval_const_string(callee, "inline", arg) {
                Some(value) => Some(value),
                None => return self.error_expr(span),
            },
        };
        let group = match bound_named("group") {
            None => None,
            Some(arg) => match self.eval_const_string(callee, "group", arg) {
                Some(value) => Some(value),
                None => return self.error_expr(span),
            },
        };
        let confirm = match bound_named("confirm") {
            None => false,
            Some(arg) => match self.eval_const_bool(callee, "confirm", arg) {
                Some(value) => value,
                None => return self.error_expr(span),
            },
        };
        let display = match bound_named("display") {
            None => Self::default_input_display(callee, kind),
            Some(arg) => match self.eval_input_display(callee, arg) {
                Some(value) => value,
                None => return self.error_expr(span),
            },
        };
        let active = match bound_named("active") {
            None => InputActive::default(),
            Some(arg) => match self.eval_input_active(callee, arg) {
                Some(value) => value,
                None => return self.error_expr(span),
            },
        };

        // Numeric inputs (int/float) may carry minval/maxval/step/options.
        let mut constraints = InputConstraints::default();
        if matches!(kind, InputKind::Int | InputKind::Float) {
            // options is mutually exclusive with min/max/step. We accept
            // whichever is present; if both, E0230.
            let has_range = ["minval", "maxval", "step"]
                .iter()
                .any(|name| bound_named(name).is_some());
            let options_arg = bound_named("options");
            if has_range && options_arg.is_some() {
                self.error(
                    codes::E0230,
                    format!(
                        "`{callee}` accepts either `minval`/`maxval`/`step` or `options`, not both"
                    ),
                    span,
                    "these two forms are mutually exclusive",
                );
                return self.error_expr(span);
            }
            let mut ok = true;
            for field in ["minval", "maxval", "step"] {
                if let Some(arg) = bound_named(field) {
                    match self.eval_input_value(callee, field, kind, arg) {
                        Some(v) => match field {
                            "minval" => constraints.min = Some(v),
                            "maxval" => constraints.max = Some(v),
                            _ => constraints.step = Some(v),
                        },
                        None => ok = false,
                    }
                }
            }
            if let Some(arg) = options_arg {
                match self.eval_input_options(callee, kind, arg) {
                    Some(opts) => constraints.options = opts,
                    None => ok = false,
                }
            }
            if !ok {
                return self.error_expr(span);
            }
            // A default must lie within a declared min/max and, if options are
            // given, be one of them.
            if !self.check_default_in_constraints(callee, &defval, &constraints, defval_arg) {
                return self.error_expr(span);
            }
        } else if matches!(kind, InputKind::String | InputKind::Enum) {
            if let Some(arg) = bound_named("options") {
                constraints.options = match self.eval_input_options(callee, kind, arg) {
                    Some(options) => options,
                    None => return self.error_expr(span),
                };
                if !self.check_default_in_constraints(callee, &defval, &constraints, defval_arg) {
                    return self.error_expr(span);
                }
            } else if let InputValue::Enum { name, .. } = &defval {
                // With no explicit `options`, the input lists every member of
                // the declaring enum. Materialize that complete list in the
                // manifest so a host does not need access to source ASTs.
                constraints.options = self
                    .enums
                    .get(name)
                    .map(|info| {
                        info.variants
                            .iter()
                            .enumerate()
                            .map(|(index, variant)| InputValue::Enum {
                                name: name.clone(),
                                index: index as u32,
                                member: variant.name.clone(),
                                title: variant.title.clone(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
            }
        }

        // An `input.enum` read types as the declaring `Type::Enum(name)` (the
        // name is not carried on the `Copy` kind — it comes from the resolved
        // `defval`). Every other kind's read type is fixed by the kind.
        let read_ty = match &defval {
            InputValue::Enum { name, .. } => Type::Enum(name.clone()),
            _ => input_kind_type(kind),
        };
        let id = self.inputs.len() as u32;
        // This is presentation metadata only: keep the exact raw default and
        // executable input read unchanged. Reused ids cannot inherit a rejected
        // nested declaration's provenance.
        self.input_color_property_transparencies.remove(&id);
        if kind == InputKind::Color {
            if let Some(t) = self
                .const_input_property_transparencies
                .get(&defval_arg.span())
            {
                self.input_color_property_transparencies.insert(id, *t);
            }
        }
        self.inputs.push(InputDecl {
            id,
            kind,
            widget,
            title,
            tooltip,
            inline,
            group,
            confirm,
            display,
            active,
            defval,
            constraints,
            span,
        });
        HirExpr {
            kind: HirExprKind::ReadInput { input: id },
            ty: read_ty,
            qualifier: input_kind_qualifier(kind),
            span,
        }
    }

    /// Select one input signature per overload. Numeric options and range
    /// forms are distinct overloads, not one flattened parameter list. The
    /// generic source overload also has a distinct positional ordering.
    fn input_signature(
        callee: &str,
        kind: InputKind,
        args: &[ast::CallArg],
        generic: bool,
    ) -> &'static [&'static str] {
        const GENERIC: &[&str] = &[
            "defval", "title", "tooltip", "inline", "group", "display", "active",
        ];
        const GENERIC_SOURCE: &[&str] = &[
            "defval", "title", "inline", "group", "tooltip", "display", "active",
        ];
        const SIMPLE: &[&str] = &[
            "defval", "title", "tooltip", "inline", "group", "confirm", "display", "active",
        ];
        const OPTIONS: &[&str] = &[
            "defval", "title", "options", "tooltip", "inline", "group", "confirm", "display",
            "active",
        ];
        const NUMERIC_RANGE: &[&str] = &[
            "defval", "title", "minval", "maxval", "step", "tooltip", "inline", "group", "confirm",
            "display", "active",
        ];
        const TEXT_AREA: &[&str] = &[
            "defval", "title", "tooltip", "group", "confirm", "display", "active",
        ];
        const SOURCE: &[&str] = &[
            "defval", "title", "tooltip", "inline", "group", "display", "active", "confirm",
        ];

        if generic {
            return if kind == InputKind::Source {
                GENERIC_SOURCE
            } else {
                GENERIC
            };
        }
        match callee {
            "input.int" | "input.float" => {
                let named_options = args
                    .iter()
                    .any(|arg| arg.name.as_ref().is_some_and(|name| name.name == "options"));
                let named_range = args.iter().any(|arg| {
                    arg.name.as_ref().is_some_and(|name| {
                        matches!(name.name.as_str(), "minval" | "maxval" | "step")
                    })
                });
                let third_positional_is_options = args
                    .iter()
                    .filter(|arg| arg.name.is_none())
                    .nth(2)
                    .is_some_and(|arg| Self::is_tuple_literal(&arg.value));
                if named_options || (!named_range && third_positional_is_options) {
                    OPTIONS
                } else {
                    NUMERIC_RANGE
                }
            }
            "input.string" | "input.timeframe" | "input.session" | "input.enum" => OPTIONS,
            "input.text_area" => TEXT_AREA,
            "input.source" => SOURCE,
            _ => SIMPLE,
        }
    }

    fn is_tuple_literal(expr: &ast::Expr) -> bool {
        match expr {
            ast::Expr::Tuple { .. } => true,
            ast::Expr::Group { inner, .. } => Self::is_tuple_literal(inner),
            _ => false,
        }
    }

    fn default_input_display(_callee: &str, kind: InputKind) -> InputDisplay {
        // Default: only bool and color inputs are hidden from both
        // external locations; every other input type uses display.all.
        if matches!(kind, InputKind::Bool | InputKind::Color) {
            InputDisplay::None
        } else {
            InputDisplay::All
        }
    }

    /// Evaluate the input display constants and their `+` / `-`
    /// combinations. Pine's input `display` parameter requires a const
    /// `plot_display`; validate that qualifier before folding cached aliases.
    fn eval_input_display(&mut self, callee: &str, arg: &ast::Expr) -> Option<InputDisplay> {
        // The display argument becomes manifest metadata rather than executable
        // HIR. Analyze it first so a mutable alias cannot pass using only its
        // initializer value, then discard any pending expression statements.
        let saved_pending = std::mem::take(&mut self.pending_stmts);
        let input_count = self.inputs.len();
        let hir = self.analyze_expr(arg);
        let _metadata_only = std::mem::replace(&mut self.pending_stmts, saved_pending);
        // A pending persistent write can make an initializer-shaped display
        // alias appear const at this analysis point. Save the dependency before
        // the AST token folder reads its cached value.
        if hir.qualifier == Qualifier::Const {
            self.defer_temporal_fixedness_at(&hir, callee, "display", arg.span());
        }
        if hir.ty.is_error() {
            self.inputs.truncate(input_count);
            return None;
        }
        if !hir.ty.is_plot_display() || hir.qualifier != Qualifier::Const {
            self.inputs.truncate(input_count);
            self.error(
                codes::E0230,
                format!("`{callee}` argument `display` must be a const plot display"),
                arg.span(),
                format!("found {} {}", hir.qualifier.describe(), hir.ty.describe()),
            );
            return None;
        }

        let value = match self.input_display_bits(arg) {
            Some(0) => InputDisplay::None,
            Some(1) => InputDisplay::DataWindow,
            Some(2) => InputDisplay::StatusLine,
            Some(3) => InputDisplay::All,
            _ => {
                self.inputs.truncate(input_count);
                self.error(
                    codes::E0230,
                    format!("`{callee}` argument `display` must be an input display constant"),
                    arg.span(),
                    "use display.none, display.data_window, display.status_line, display.all, or a +/- combination",
                );
                return None;
            }
        };
        Some(value)
    }

    pub(crate) fn input_display_bits(&self, expr: &ast::Expr) -> Option<u8> {
        let token_bits = |token: &str| match token {
            "display.none" => Some(0),
            "display.data_window" => Some(1),
            "display.status_line" => Some(2),
            "display.all" => Some(3),
            _ => None,
        };
        match expr {
            ast::Expr::Group { inner, .. } => self.input_display_bits(inner),
            ast::Expr::Ident(ident) => token_bits(&ident.name).or_else(|| {
                let symbol = self.lookup_variable(&ident.name)?;
                // A string variable is not a display value, whatever it holds.
                if !self.symbols[symbol.0 as usize].ty.is_plot_display() {
                    return None;
                }
                let InputValue::String(token) = self.const_input_values.get(&symbol)? else {
                    return None;
                };
                // A const alias keeps the inline verdict of its initializer,
                // so it is accepted exactly when the inline form would be.
                if let Some(bits) = self.held_input_display_bits.get(&symbol) {
                    return Some(*bits);
                }
                // Otherwise only the folded canonical token is known. PineWASM
                // accepts the inline, pane and price-scale bits only through
                // `display.all`. A set that holds them short of `display.all`,
                // such as `display.pane + display.price_scale`, is ambiguous,
                // so it is rejected; sets without them, and `display.all`, are
                // not.
                match pinewasm_hir::display_set::mask(token)? {
                    pinewasm_hir::display_set::ALL_MASK => Some(3),
                    mask if mask & 0b1_0011 != 0 => None,
                    mask => Some(u8::from(mask & 8 != 0) | (u8::from(mask & 4 != 0) << 1)),
                }
            }),
            ast::Expr::Binary { op, lhs, rhs, .. } => {
                let lhs = self.input_display_bits(lhs)?;
                let rhs = self.input_display_bits(rhs)?;
                match op {
                    ast::BinaryOp::Add => Some(lhs | rhs),
                    ast::BinaryOp::Sub => Some(lhs & !rhs & 3),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn eval_input_active(&mut self, callee: &str, arg: &ast::Expr) -> Option<InputActive> {
        // `active` is metadata, not a runtime expression. Preserve any pending
        // statements from the surrounding source and discard statements a
        // complex active expression may have desugared while type-checking.
        let saved_pending = std::mem::take(&mut self.pending_stmts);
        let hir = self.analyze_expr(arg);
        self.defer_temporal_fixedness_at(&hir, callee, "active", arg.span());
        let _metadata_only = std::mem::replace(&mut self.pending_stmts, saved_pending);
        if hir.ty.is_error() {
            return None;
        }
        if hir.ty != Type::Bool {
            self.error(
                codes::E0230,
                format!("`{callee}` argument `active` must be bool"),
                arg.span(),
                format!("found {}", hir.ty.describe()),
            );
            return None;
        }
        if hir.qualifier > Qualifier::Input {
            self.error(
                codes::E0230,
                format!("`{callee}` argument `active` must be const or input bool"),
                arg.span(),
                format!("found {} bool", hir.qualifier.describe()),
            );
            return None;
        }

        let expression = self.input_active_expr_from_hir(&hir);
        let mut dependencies = Vec::new();
        if let Some(expression) = &expression {
            Self::collect_active_expr_dependencies(expression, &mut dependencies);
        } else {
            self.collect_active_dependencies(&hir, &mut dependencies);
        }
        dependencies.sort_unstable();
        dependencies.dedup();
        let direct = self.input_origin_for_expr(&hir);
        if let Some(direct) = direct {
            if !dependencies.contains(&direct.input) {
                dependencies.push(direct.input);
                dependencies.sort_unstable();
            }
        }
        let default = match expression
            .as_ref()
            .and_then(|expression| self.eval_input_active_expr(expression))
            .or_else(|| self.eval_active_scalar(&hir))
        {
            Some(ActiveScalar::Bool(value)) => value,
            // A computed `na` bool is false when used as a condition.
            Some(ActiveScalar::Na) => false,
            // A valid but host-opaque input bool expression remains enabled
            // until a richer expression evaluator is available.
            _ => true,
        };
        Some(InputActive {
            default,
            dependencies,
            direct,
            expression,
        })
    }

    pub(crate) fn input_active_expr_from_hir(&self, expr: &HirExpr) -> Option<InputActiveExpr> {
        let value = |value| Some(InputActiveExpr::Value { value });
        match &expr.kind {
            HirExprKind::ConstInt(number) => match &expr.ty {
                Type::Enum(name) => value(InputValue::Enum {
                    name: name.clone(),
                    index: u32::try_from(*number).ok()?,
                    member: String::new(),
                    title: None,
                }),
                _ => value(InputValue::Int(*number)),
            },
            HirExprKind::ConstFloat(number) => value(InputValue::Float(*number)),
            HirExprKind::ConstBool(boolean) => value(InputValue::Bool(*boolean)),
            // A display set binds as the union of its member names, the form
            // the host binding reads, never as our canonical wire token.
            HirExprKind::ConstString(string) if expr.ty.is_plot_display() => {
                pinewasm_hir::display_set::binding_expr(pinewasm_hir::display_set::mask(string)?)
            }
            HirExprKind::ConstString(string) if string.len() <= MAX_INPUT_ACTIVE_STRING_BYTES => {
                value(InputValue::String(string.clone()))
            }
            HirExprKind::ConstColor { r, g, b, t } => value(InputValue::Color {
                r: *r,
                g: *g,
                b: *b,
                t: *t,
            }),
            HirExprKind::ConstNa => Some(InputActiveExpr::Na),
            HirExprKind::ReadInput { input } => Some(InputActiveExpr::Input { input: *input }),
            HirExprKind::ReadVar { symbol, offset: 0 } => {
                self.input_active_origins.get(symbol).cloned()
            }
            HirExprKind::Unary { op, operand } => Some(InputActiveExpr::Unary {
                op: match op {
                    UnOp::Not => InputActiveUnaryOp::Not,
                    UnOp::Neg => InputActiveUnaryOp::Neg,
                },
                operand: Box::new(self.input_active_expr_from_hir(operand)?),
            }),
            HirExprKind::IntToFloat(operand) => Some(InputActiveExpr::Unary {
                op: InputActiveUnaryOp::IntToFloat,
                operand: Box::new(self.input_active_expr_from_hir(operand)?),
            }),
            HirExprKind::Binary { op, lhs, rhs } => Some(InputActiveExpr::Binary {
                op: match op {
                    BinOp::Add => InputActiveBinaryOp::Add,
                    BinOp::Sub => InputActiveBinaryOp::Sub,
                    BinOp::Mul => InputActiveBinaryOp::Mul,
                    BinOp::Div => InputActiveBinaryOp::Div,
                    BinOp::Rem => InputActiveBinaryOp::Rem,
                    BinOp::Lt => InputActiveBinaryOp::Lt,
                    BinOp::Le => InputActiveBinaryOp::Le,
                    BinOp::Gt => InputActiveBinaryOp::Gt,
                    BinOp::Ge => InputActiveBinaryOp::Ge,
                    BinOp::Eq => InputActiveBinaryOp::Eq,
                    BinOp::Ne => InputActiveBinaryOp::Ne,
                    BinOp::And => InputActiveBinaryOp::And,
                    BinOp::Or => InputActiveBinaryOp::Or,
                },
                lhs: Box::new(self.input_active_expr_from_hir(lhs)?),
                rhs: Box::new(self.input_active_expr_from_hir(rhs)?),
            }),
            HirExprKind::Ternary {
                cond,
                then_e,
                else_e,
            } => match self.source_constant_condition(cond) {
                Some(true) => self.input_active_expr_from_hir(then_e),
                Some(false) => self.input_active_expr_from_hir(else_e),
                _ => Some(InputActiveExpr::Ternary {
                    condition: Box::new(self.input_active_expr_from_hir(cond)?),
                    when_true: Box::new(self.input_active_expr_from_hir(then_e)?),
                    when_false: Box::new(self.input_active_expr_from_hir(else_e)?),
                }),
            },
            // A run-time display set operation: apply it to the operands'
            // bindings under each input choice, so the host reads ternaries
            // over member unions, never a joined token or a difference.
            HirExprKind::EvalSequence { bindings, .. } if expr.ty.is_plot_display() => {
                let [(left, lhs), (_, rhs)] = bindings.as_slice() else {
                    return None;
                };
                let union = *self.display_set_ops.get(&SymbolId(left.0))?;
                pinewasm_hir::display_set::combine_bindings(
                    union,
                    &self.input_active_expr_from_hir(lhs)?,
                    &self.input_active_expr_from_hir(rhs)?,
                )
            }
            HirExprKind::CallPure { func, args }
                if !func.is_strategy() && input_active_call_is_supported(func.name()) =>
            {
                Some(InputActiveExpr::Call {
                    function: func.name().to_string(),
                    args: args
                        .iter()
                        .map(|arg| self.input_active_expr_from_hir(arg))
                        .collect::<Option<Vec<_>>>()?,
                })
            }
            _ => None,
        }
    }

    /// Preserve the input-free host expression used for Simple switch capture
    /// bindings. Host chart values can serialize as Value too, so an empty
    /// input-dependency list is deliberately not a source-closure proof.
    pub(crate) fn input_free_active_expr_from_hir(
        &self,
        expr: &HirExpr,
    ) -> Option<InputActiveExpr> {
        let expression = self.input_active_expr_from_hir(expr)?;
        let mut dependencies = Vec::new();
        Self::collect_active_expr_dependencies(&expression, &mut dependencies);
        dependencies.is_empty().then_some(expression)
    }

    /// Prove source closure independently of the host binding representation.
    /// The existing serializer still bounds the supported expression language;
    /// its output and dependency/default semantics are unchanged.
    pub(crate) fn source_closed_active_expr_from_hir(
        &self,
        expr: &HirExpr,
    ) -> Option<InputActiveExpr> {
        let expression = self.input_free_active_expr_from_hir(expr)?;
        self.source_closed_active_leaves_are_proven(expr)
            .then_some(expression)
    }

    pub(crate) fn source_closed_active_expr_is_proven(&self, expr: &HirExpr) -> bool {
        self.source_closed_active_expr_from_hir(expr).is_some()
    }

    fn source_closed_active_leaves_are_proven(&self, expr: &HirExpr) -> bool {
        if expr.ty.is_error() || expr.ty == Type::Void {
            return false;
        }
        match &expr.kind {
            // A host context value can lower to the same literal node, but
            // retains Input/Simple qualification. Obtain a real source proof
            // before an explicit declaration/parameter strengthens a literal.
            HirExprKind::ConstInt(_)
            | HirExprKind::ConstFloat(_)
            | HirExprKind::ConstBool(_)
            | HirExprKind::ConstString(_)
            | HirExprKind::ConstColor { .. }
            | HirExprKind::ConstNa => expr.qualifier == Qualifier::Const,
            HirExprKind::ReadVar { symbol, offset: 0 } => {
                self.source_closed_active_origins.contains(symbol)
            }
            HirExprKind::Unary { operand, .. } | HirExprKind::IntToFloat(operand) => {
                self.source_closed_active_leaves_are_proven(operand)
            }
            HirExprKind::Binary { lhs, rhs, .. } => {
                self.source_closed_active_leaves_are_proven(lhs)
                    && self.source_closed_active_leaves_are_proven(rhs)
            }
            HirExprKind::Ternary {
                cond,
                then_e,
                else_e,
            } => {
                self.source_closed_active_leaves_are_proven(cond)
                    && self.source_closed_active_leaves_are_proven(then_e)
                    && self.source_closed_active_leaves_are_proven(else_e)
            }
            HirExprKind::CallPure { args, .. } => args
                .iter()
                .all(|arg| self.source_closed_active_leaves_are_proven(arg)),
            HirExprKind::EvalSequence { bindings, .. } if expr.ty.is_plot_display() => bindings
                .iter()
                .all(|(_, value)| self.source_closed_active_leaves_are_proven(value)),
            _ => false,
        }
    }

    fn collect_active_expr_dependencies(expression: &InputActiveExpr, out: &mut Vec<u32>) {
        match expression {
            InputActiveExpr::Value { .. } | InputActiveExpr::Na => {}
            InputActiveExpr::Input { input } => out.push(*input),
            InputActiveExpr::Unary { operand, .. } => {
                Self::collect_active_expr_dependencies(operand, out);
            }
            InputActiveExpr::Binary { lhs, rhs, .. } => {
                Self::collect_active_expr_dependencies(lhs, out);
                Self::collect_active_expr_dependencies(rhs, out);
            }
            InputActiveExpr::Ternary {
                condition,
                when_true,
                when_false,
            } => {
                Self::collect_active_expr_dependencies(condition, out);
                Self::collect_active_expr_dependencies(when_true, out);
                Self::collect_active_expr_dependencies(when_false, out);
            }
            InputActiveExpr::Call { args, .. } => {
                for arg in args {
                    Self::collect_active_expr_dependencies(arg, out);
                }
            }
        }
    }

    fn eval_input_active_expr(&self, expression: &InputActiveExpr) -> Option<ActiveScalar> {
        match expression {
            InputActiveExpr::Value { value } => Self::scalar_from_input_value(value),
            InputActiveExpr::Na => Some(ActiveScalar::Na),
            InputActiveExpr::Input { input } => self
                .inputs
                .get(*input as usize)
                .and_then(|decl| Self::scalar_from_input_value(&decl.defval)),
            InputActiveExpr::Unary { op, operand } => {
                let operand = self.eval_input_active_expr(operand)?;
                match (op, operand) {
                    (_, ActiveScalar::Na) => Some(ActiveScalar::Na),
                    (InputActiveUnaryOp::Not, ActiveScalar::Bool(value)) => {
                        Some(ActiveScalar::Bool(!value))
                    }
                    (InputActiveUnaryOp::Neg, ActiveScalar::Int(value)) => Some(
                        value
                            .checked_neg()
                            .map_or(ActiveScalar::Na, ActiveScalar::Int),
                    ),
                    (InputActiveUnaryOp::Neg, ActiveScalar::Float(value)) => {
                        Some(ActiveScalar::Float(-value))
                    }
                    (InputActiveUnaryOp::IntToFloat, ActiveScalar::Int(value)) => {
                        Some(ActiveScalar::Float(value as f64))
                    }
                    (InputActiveUnaryOp::IntToFloat, value) => Some(value),
                    _ => None,
                }
            }
            InputActiveExpr::Binary { op, lhs, rhs } => {
                let lhs = self.eval_input_active_expr(lhs)?;
                let rhs = self.eval_input_active_expr(rhs)?;
                Self::eval_active_binary(
                    match op {
                        InputActiveBinaryOp::Add => BinOp::Add,
                        InputActiveBinaryOp::Sub => BinOp::Sub,
                        InputActiveBinaryOp::Mul => BinOp::Mul,
                        InputActiveBinaryOp::Div => BinOp::Div,
                        InputActiveBinaryOp::Rem => BinOp::Rem,
                        InputActiveBinaryOp::Lt => BinOp::Lt,
                        InputActiveBinaryOp::Le => BinOp::Le,
                        InputActiveBinaryOp::Gt => BinOp::Gt,
                        InputActiveBinaryOp::Ge => BinOp::Ge,
                        InputActiveBinaryOp::Eq => BinOp::Eq,
                        InputActiveBinaryOp::Ne => BinOp::Ne,
                        InputActiveBinaryOp::And => BinOp::And,
                        InputActiveBinaryOp::Or => BinOp::Or,
                    },
                    lhs,
                    rhs,
                )
            }
            InputActiveExpr::Ternary {
                condition,
                when_true,
                when_false,
            } => match self.eval_input_active_expr(condition)? {
                ActiveScalar::Bool(true) => self.eval_input_active_expr(when_true),
                ActiveScalar::Bool(false) | ActiveScalar::Na => {
                    self.eval_input_active_expr(when_false)
                }
                _ => None,
            },
            InputActiveExpr::Call { function, args } => {
                let args = args
                    .iter()
                    .map(|arg| self.eval_input_active_expr(arg))
                    .collect::<Option<Vec<_>>>()?;
                Self::eval_active_pure(function, &args)
            }
        }
    }

    fn eval_active_pure(function: &str, args: &[ActiveScalar]) -> Option<ActiveScalar> {
        if function == "na" {
            return Some(ActiveScalar::Bool(matches!(
                args.first(),
                Some(ActiveScalar::Na)
            )));
        }
        if function == "nz" {
            return match args.first()? {
                ActiveScalar::Na => Some(args.get(1).cloned().unwrap_or(ActiveScalar::Int(0))),
                value => Some(value.clone()),
            };
        }
        if args.iter().any(|value| matches!(value, ActiveScalar::Na)) {
            return Some(ActiveScalar::Na);
        }
        let number = |value: &ActiveScalar| match value {
            ActiveScalar::Int(value) => Some(*value as f64),
            ActiveScalar::Float(value) => Some(*value),
            _ => None,
        };
        let integer = |value: &ActiveScalar| match value {
            ActiveScalar::Int(value) => Some(*value),
            _ => None,
        };
        fn string(value: &ActiveScalar) -> Option<&str> {
            match value {
                ActiveScalar::String(value) => Some(value.as_str()),
                _ => None,
            }
        }
        let bounded_string = |value: String| {
            (value.len() <= MAX_INPUT_ACTIVE_STRING_BYTES).then_some(ActiveScalar::String(value))
        };
        let unary_number = |operation: fn(f64) -> f64| {
            let value = operation(number(args.first()?)?);
            value.is_finite().then_some(ActiveScalar::Float(value))
        };
        match function {
            "bool" => match args.first()? {
                ActiveScalar::Bool(value) => Some(ActiveScalar::Bool(*value)),
                ActiveScalar::Int(value) => Some(ActiveScalar::Bool(*value != 0)),
                ActiveScalar::Float(value) => Some(ActiveScalar::Bool(*value != 0.0)),
                _ => None,
            },
            "int" => match args.first()? {
                ActiveScalar::Int(value) => Some(ActiveScalar::Int(*value)),
                ActiveScalar::Float(value) if value.is_finite() => {
                    Some(ActiveScalar::Int(value.trunc() as i64))
                }
                _ => None,
            },
            "float" => number(args.first()?).map(ActiveScalar::Float),
            "string" | "str.tostring" => match args.first()? {
                ActiveScalar::String(value) => bounded_string(value.clone()),
                ActiveScalar::Bool(value) => bounded_string(value.to_string()),
                ActiveScalar::Int(value) => bounded_string(value.to_string()),
                ActiveScalar::Float(value) => bounded_string(value.to_string()),
                _ => None,
            },
            "math.abs" => match args.first()? {
                ActiveScalar::Int(value) => value.checked_abs().map(ActiveScalar::Int),
                ActiveScalar::Float(value) => Some(ActiveScalar::Float(value.abs())),
                _ => None,
            },
            "math.sign" => match args.first()? {
                ActiveScalar::Int(value) => Some(ActiveScalar::Int(value.signum())),
                ActiveScalar::Float(value) => Some(ActiveScalar::Float(if *value > 0.0 {
                    1.0
                } else if *value < 0.0 {
                    -1.0
                } else {
                    // PineWASM returns 0.0 for either signed zero.
                    0.0
                })),
                _ => None,
            },
            "math.sqrt" => unary_number(f64::sqrt),
            "math.exp" => unary_number(f64::exp),
            "math.log" => unary_number(f64::ln),
            "math.log10" => unary_number(f64::log10),
            "math.sin" => unary_number(f64::sin),
            "math.cos" => unary_number(f64::cos),
            "math.tan" => unary_number(f64::tan),
            "math.asin" => unary_number(f64::asin),
            "math.acos" => unary_number(f64::acos),
            "math.atan" => unary_number(f64::atan),
            "math.todegrees" => unary_number(f64::to_degrees),
            "math.toradians" => unary_number(f64::to_radians),
            "math.pow" => {
                let value = number(args.first()?)?.powf(number(args.get(1)?)?);
                value.is_finite().then_some(ActiveScalar::Float(value))
            }
            "math.floor" => Some(ActiveScalar::Int(number(args.first()?)?.floor() as i64)),
            "math.ceil" => Some(ActiveScalar::Int(number(args.first()?)?.ceil() as i64)),
            "math.round" if args.len() == 1 => {
                let value = number(args.first()?)?;
                let rounded = if value.is_sign_negative() {
                    (value - 0.5).ceil()
                } else {
                    (value + 0.5).floor()
                };
                Some(ActiveScalar::Int(rounded as i64))
            }
            "math.min" | "math.max" | "math.avg" => {
                if function != "math.avg"
                    && args.iter().all(|arg| matches!(arg, ActiveScalar::Int(_)))
                {
                    let values = args.iter().filter_map(integer);
                    let value = if function == "math.min" {
                        values.min()?
                    } else {
                        values.max()?
                    };
                    return Some(ActiveScalar::Int(value));
                }
                let values = args.iter().map(number).collect::<Option<Vec<_>>>()?;
                let value = match function {
                    "math.min" => values.into_iter().reduce(f64::min)?,
                    "math.max" => values.into_iter().reduce(f64::max)?,
                    _ => values.iter().sum::<f64>() / values.len() as f64,
                };
                Some(ActiveScalar::Float(value))
            }
            "str.tonumber" => string(args.first()?)?
                .parse::<f64>()
                .ok()
                .filter(|value| value.is_finite())
                .map(ActiveScalar::Float),
            "str.length" => Some(ActiveScalar::Int(
                string(args.first()?)?.chars().count() as i64
            )),
            "str.contains" => Some(ActiveScalar::Bool(
                string(args.first()?)?.contains(string(args.get(1)?)?),
            )),
            "str.startswith" => Some(ActiveScalar::Bool(
                string(args.first()?)?.starts_with(string(args.get(1)?)?),
            )),
            "str.endswith" => Some(ActiveScalar::Bool(
                string(args.first()?)?.ends_with(string(args.get(1)?)?),
            )),
            "str.pos" => Some(ActiveScalar::Int(
                string(args.first()?)?
                    .find(string(args.get(1)?)?)
                    .map_or(-1, |index| index as i64),
            )),
            "str.upper" => bounded_string(string(args.first()?)?.to_ascii_uppercase()),
            "str.lower" => bounded_string(string(args.first()?)?.to_ascii_lowercase()),
            "str.trim" => bounded_string(string(args.first()?)?.trim().to_string()),
            "str.repeat" => {
                let source = string(args.first()?)?;
                let repeat = usize::try_from(integer(args.get(1)?)?).ok()?;
                if repeat > MAX_INPUT_ACTIVE_STRING_BYTES {
                    return None;
                }
                let separator = match args.get(2) {
                    Some(value) => string(value)?,
                    None => "",
                };
                let projected = source
                    .len()
                    .checked_mul(repeat)?
                    .checked_add(separator.len().checked_mul(repeat.saturating_sub(1))?)?;
                if projected > MAX_INPUT_ACTIVE_STRING_BYTES {
                    return None;
                }
                let mut result = String::with_capacity(projected);
                for index in 0..repeat {
                    if index > 0 {
                        result.push_str(separator);
                    }
                    result.push_str(source);
                }
                Some(ActiveScalar::String(result))
            }
            "color.r" | "color.g" | "color.b" | "color.t" => {
                let ActiveScalar::Color(r, g, b, t) = args.first()? else {
                    return None;
                };
                Some(ActiveScalar::Float(match function {
                    "color.r" => f64::from(*r),
                    "color.g" => f64::from(*g),
                    "color.b" => f64::from(*b),
                    _ => *t,
                }))
            }
            _ => None,
        }
    }

    pub(crate) fn input_origin_for_expr(&self, expr: &HirExpr) -> Option<InputActiveDirect> {
        match &expr.kind {
            HirExprKind::ReadInput { input } => {
                self.inputs.get(*input as usize).map(|_| InputActiveDirect {
                    input: *input,
                    inverted: false,
                })
            }
            HirExprKind::ReadVar { symbol, offset: 0 } => self.input_origins.get(symbol).copied(),
            HirExprKind::Unary {
                op: UnOp::Not,
                operand,
            } => {
                let mut direct = self.input_origin_for_expr(operand)?;
                if self.inputs.get(direct.input as usize)?.kind != InputKind::Bool {
                    return None;
                }
                direct.inverted = !direct.inverted;
                Some(direct)
            }
            _ => None,
        }
    }

    pub(crate) fn source_constant_condition(&self, expr: &HirExpr) -> Option<bool> {
        if self.temporal_dependencies(expr) {
            return None;
        }
        match self.const_input_value_from_hir(expr)? {
            InputValue::Bool(value) => Some(value),
            _ => None,
        }
    }

    /// A selector with a source-constant condition can preserve one direct
    /// input identity. Reject carried dependencies before using that identity
    /// to specialize execution outside the chart-bar program.
    pub(crate) fn selected_input_for_expr(&self, expr: &HirExpr) -> Option<u32> {
        if self.temporal_dependencies(expr) {
            return None;
        }
        match self.input_active_expr_from_hir(expr)? {
            InputActiveExpr::Input { input } => Some(input),
            _ => None,
        }
    }

    fn collect_active_dependencies(&self, expr: &HirExpr, out: &mut Vec<u32>) {
        match &expr.kind {
            HirExprKind::ReadInput { input } => out.push(*input),
            HirExprKind::ReadVar { symbol, offset: 0 } => {
                if let Some(origin) = self.input_origins.get(symbol) {
                    out.push(origin.input);
                }
            }
            HirExprKind::Unary { operand, .. } | HirExprKind::IntToFloat(operand) => {
                self.collect_active_dependencies(operand, out);
            }
            HirExprKind::Binary { lhs, rhs, .. } => {
                self.collect_active_dependencies(lhs, out);
                self.collect_active_dependencies(rhs, out);
            }
            HirExprKind::Ternary {
                cond,
                then_e,
                else_e,
            } => {
                self.collect_active_dependencies(cond, out);
                self.collect_active_dependencies(then_e, out);
                self.collect_active_dependencies(else_e, out);
            }
            HirExprKind::CallPure { args, .. } => {
                for arg in args {
                    self.collect_active_dependencies(arg, out);
                }
            }
            _ => {}
        }
    }

    fn scalar_from_input_value(value: &InputValue) -> Option<ActiveScalar> {
        match value {
            InputValue::Int(value) => Some(ActiveScalar::Int(*value)),
            InputValue::Float(value) => Some(ActiveScalar::Float(*value)),
            InputValue::Bool(value) => Some(ActiveScalar::Bool(*value)),
            InputValue::String(value) if value.len() <= MAX_INPUT_ACTIVE_STRING_BYTES => {
                Some(ActiveScalar::String(value.clone()))
            }
            InputValue::Color { r, g, b, t } => Some(ActiveScalar::Color(*r, *g, *b, *t)),
            InputValue::Enum { name, index, .. } => Some(ActiveScalar::Enum(name.clone(), *index)),
            InputValue::String(_) | InputValue::Source(_) | InputValue::ExternalSource(_) => None,
        }
    }

    /// A concrete default display destination, even when the portable
    /// representation exceeds the host binding budget.
    pub(crate) fn default_display_mask(&self, expr: &HirExpr) -> Option<u8> {
        use pinewasm_hir::display_set;
        fn evaluate_binding(analyzer: &Analyzer, expression: &InputActiveExpr) -> Option<u8> {
            match expression {
                InputActiveExpr::Ternary {
                    condition,
                    when_true,
                    when_false,
                } => match analyzer.eval_input_active_expr(condition)? {
                    ActiveScalar::Bool(true) => evaluate_binding(analyzer, when_true),
                    ActiveScalar::Bool(false) => evaluate_binding(analyzer, when_false),
                    _ => None,
                },
                _ => display_set::binding_mask(expression),
            }
        }
        if !expr.ty.is_plot_display() || self.temporal_dependencies(expr) {
            return None;
        }
        if let Some(InputValue::String(token)) = self.const_input_value_from_hir(expr) {
            return display_set::mask(&token);
        }
        if let HirExprKind::ReadVar { symbol, offset: 0 } = &expr.kind {
            if let Some(mask) = self.default_display_masks.get(symbol) {
                return Some(*mask);
            }
        }
        if let HirExprKind::EvalSequence { bindings, .. } = &expr.kind {
            let [(left, lhs), (_, rhs)] = bindings.as_slice() else {
                return None;
            };
            let union = *self.display_set_ops.get(&SymbolId(left.0))?;
            let lhs = self.default_display_mask(lhs)?;
            let rhs = self.default_display_mask(rhs)?;
            return Some(if union { lhs | rhs } else { lhs & !rhs });
        }
        evaluate_binding(self, &self.input_active_expr_from_hir(expr)?)
    }

    fn eval_active_scalar(&self, expr: &HirExpr) -> Option<ActiveScalar> {
        let input_value = |input: u32| {
            self.inputs
                .get(input as usize)
                .and_then(|decl| Self::scalar_from_input_value(&decl.defval))
        };
        match &expr.kind {
            HirExprKind::ConstInt(value) => match &expr.ty {
                Type::Enum(name) => u32::try_from(*value)
                    .ok()
                    .map(|index| ActiveScalar::Enum(name.clone(), index)),
                _ => Some(ActiveScalar::Int(*value)),
            },
            HirExprKind::ConstFloat(value) => Some(ActiveScalar::Float(*value)),
            HirExprKind::ConstBool(value) => Some(ActiveScalar::Bool(*value)),
            HirExprKind::ConstString(value) if value.len() <= MAX_INPUT_ACTIVE_STRING_BYTES => {
                Some(ActiveScalar::String(value.clone()))
            }
            HirExprKind::ConstColor { r, g, b, t } => Some(ActiveScalar::Color(*r, *g, *b, *t)),
            HirExprKind::ReadInput { input } => input_value(*input),
            HirExprKind::ReadVar { symbol, offset: 0 } => {
                let origin = self.input_origins.get(symbol)?;
                let value = input_value(origin.input)?;
                if origin.inverted {
                    match value {
                        ActiveScalar::Bool(value) => Some(ActiveScalar::Bool(!value)),
                        _ => None,
                    }
                } else {
                    Some(value)
                }
            }
            HirExprKind::Unary { op, operand } => {
                let operand = self.eval_active_scalar(operand)?;
                match (op, operand) {
                    (UnOp::Not, ActiveScalar::Bool(value)) => Some(ActiveScalar::Bool(!value)),
                    (UnOp::Neg, ActiveScalar::Int(value)) => Some(
                        value
                            .checked_neg()
                            .map_or(ActiveScalar::Na, ActiveScalar::Int),
                    ),
                    (UnOp::Neg, ActiveScalar::Float(value)) => Some(ActiveScalar::Float(-value)),
                    _ => None,
                }
            }
            HirExprKind::IntToFloat(inner) => match self.eval_active_scalar(inner)? {
                ActiveScalar::Int(value) => Some(ActiveScalar::Float(value as f64)),
                value => Some(value),
            },
            HirExprKind::Binary { op, lhs, rhs } => {
                let lhs = self.eval_active_scalar(lhs)?;
                let rhs = self.eval_active_scalar(rhs)?;
                Self::eval_active_binary(*op, lhs, rhs)
            }
            HirExprKind::Ternary {
                cond,
                then_e,
                else_e,
            } => match self.eval_active_scalar(cond)? {
                ActiveScalar::Bool(true) => self.eval_active_scalar(then_e),
                ActiveScalar::Bool(false) => self.eval_active_scalar(else_e),
                _ => None,
            },
            _ => None,
        }
    }

    fn eval_active_binary(op: BinOp, lhs: ActiveScalar, rhs: ActiveScalar) -> Option<ActiveScalar> {
        if matches!(lhs, ActiveScalar::Na) || matches!(rhs, ActiveScalar::Na) {
            return match op {
                BinOp::And => match (&lhs, &rhs) {
                    (ActiveScalar::Bool(false), _) | (_, ActiveScalar::Bool(false)) => {
                        Some(ActiveScalar::Bool(false))
                    }
                    (ActiveScalar::Bool(true), ActiveScalar::Bool(true)) => {
                        Some(ActiveScalar::Bool(true))
                    }
                    _ => Some(ActiveScalar::Na),
                },
                BinOp::Or => match (&lhs, &rhs) {
                    (ActiveScalar::Bool(true), _) | (_, ActiveScalar::Bool(true)) => {
                        Some(ActiveScalar::Bool(true))
                    }
                    (ActiveScalar::Bool(false), ActiveScalar::Bool(false)) => {
                        Some(ActiveScalar::Bool(false))
                    }
                    _ => Some(ActiveScalar::Na),
                },
                _ => Some(ActiveScalar::Na),
            };
        }
        let numeric = |value: &ActiveScalar| match value {
            ActiveScalar::Int(value) => Some(*value as f64),
            ActiveScalar::Float(value) => Some(*value),
            _ => None,
        };
        // Two ints compare exactly. An int and a float compare by value, as
        // in the VM: a statically-int expression can hold a float
        // (`7 / 2 * 2` is `7.0`).
        let equality = || match (&lhs, &rhs) {
            (ActiveScalar::Int(lhs), ActiveScalar::Float(rhs)) => {
                pinewasm_num::comparison::eq(*lhs as f64, *rhs)
            }
            (ActiveScalar::Float(lhs), ActiveScalar::Int(rhs)) => {
                pinewasm_num::comparison::eq(*lhs, *rhs as f64)
            }
            (ActiveScalar::Float(lhs), ActiveScalar::Float(rhs)) => {
                pinewasm_num::comparison::eq(*lhs, *rhs)
            }
            _ => lhs == rhs,
        };
        match op {
            BinOp::And | BinOp::Or => match (lhs, rhs) {
                (ActiveScalar::Bool(lhs), ActiveScalar::Bool(rhs)) => {
                    Some(ActiveScalar::Bool(if op == BinOp::And {
                        lhs && rhs
                    } else {
                        lhs || rhs
                    }))
                }
                _ => None,
            },
            BinOp::Eq => Some(ActiveScalar::Bool(equality())),
            BinOp::Ne => Some(ActiveScalar::Bool(!equality())),
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let result = match (&lhs, &rhs) {
                    (ActiveScalar::Int(lhs), ActiveScalar::Int(rhs)) => match op {
                        BinOp::Lt => lhs < rhs,
                        BinOp::Le => lhs <= rhs,
                        BinOp::Gt => lhs > rhs,
                        BinOp::Ge => lhs >= rhs,
                        _ => unreachable!(),
                    },
                    _ => {
                        let lhs = numeric(&lhs)?;
                        let rhs = numeric(&rhs)?;
                        match op {
                            BinOp::Lt => pinewasm_num::comparison::lt(lhs, rhs),
                            BinOp::Le => pinewasm_num::comparison::le(lhs, rhs),
                            BinOp::Gt => pinewasm_num::comparison::gt(lhs, rhs),
                            BinOp::Ge => pinewasm_num::comparison::ge(lhs, rhs),
                            _ => unreachable!(),
                        }
                    }
                };
                Some(ActiveScalar::Bool(result))
            }
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => match (lhs, rhs) {
                (ActiveScalar::Int(lhs), ActiveScalar::Int(rhs)) => {
                    let value = match op {
                        BinOp::Add => lhs.checked_add(rhs),
                        BinOp::Sub => lhs.checked_sub(rhs),
                        BinOp::Mul => lhs.checked_mul(rhs),
                        // int / int keeps its fraction (the analyzer
                        // types it float and promotes both sides, so this is
                        // only reachable for a hand-built expression).
                        BinOp::Div => {
                            let value = lhs as f64 / rhs as f64;
                            return Some(if value.is_finite() {
                                ActiveScalar::Float(value)
                            } else {
                                ActiveScalar::Na
                            });
                        }
                        BinOp::Rem => lhs.checked_rem(rhs),
                        _ => unreachable!(),
                    };
                    Some(value.map_or(ActiveScalar::Na, ActiveScalar::Int))
                }
                (ActiveScalar::String(lhs), ActiveScalar::String(rhs)) if op == BinOp::Add => {
                    let length = lhs.len().checked_add(rhs.len())?;
                    if length > MAX_INPUT_ACTIVE_STRING_BYTES {
                        return Some(ActiveScalar::Na);
                    }
                    Some(ActiveScalar::String(lhs + &rhs))
                }
                (lhs, rhs) => {
                    let lhs = numeric(&lhs)?;
                    let rhs = numeric(&rhs)?;
                    let value = match op {
                        BinOp::Add => lhs + rhs,
                        BinOp::Sub => lhs - rhs,
                        BinOp::Mul => lhs * rhs,
                        BinOp::Div => lhs / rhs,
                        BinOp::Rem => lhs % rhs,
                        _ => unreachable!(),
                    };
                    Some(if value.is_finite() {
                        ActiveScalar::Float(value)
                    } else {
                        ActiveScalar::Na
                    })
                }
            },
        }
    }

    fn ungroup_expr(mut expr: &ast::Expr) -> &ast::Expr {
        while let ast::Expr::Group { inner, .. } = expr {
            expr = inner;
        }
        expr
    }

    /// Analyze and fold a scalar const expression without leaking any
    /// metadata-only lowering statements into the executable program. This is
    /// the shared path for every const argument of `input.*`:
    /// literals are merely the simplest case; const aliases, arithmetic,
    /// string concatenation, `timestamp(...)`, and const color constructors
    /// are valid too. A value with `input`, `simple`, or `series` qualification
    /// deliberately does not fold.
    pub(crate) fn eval_const_input_expr(&mut self, expr: &ast::Expr) -> Option<InputValue> {
        self.eval_const_input_expr_typed(expr).0
    }

    /// [`Self::eval_const_input_expr`] for a setting that must be a whole
    /// number. A statically-int expression can fold to a fractional value
    /// (`7 / 2` is `3.5`); such a setting takes its whole part, as an
    /// int. A float-typed expression is returned unchanged for the caller to
    /// reject.
    pub(crate) fn eval_const_int_setting(&mut self, expr: &ast::Expr) -> Option<InputValue> {
        match self.eval_const_input_expr_typed(expr) {
            (Some(InputValue::Float(value)), true) => {
                let whole = value.trunc();
                (whole >= i64::MIN as f64 && whole < i64::MAX as f64)
                    .then_some(InputValue::Int(whole as i64))
            }
            (value, _) => value,
        }
    }

    /// The folded value, and whether the expression is statically int.
    fn eval_const_input_expr_typed(&mut self, expr: &ast::Expr) -> (Option<InputValue>, bool) {
        let (value, ty, _) = self.eval_const_input_expr_checked(expr);
        (value, ty == Type::Int)
    }

    /// [`Self::eval_const_input_expr`] that also returns the analyzed type and
    /// qualifier, so a caller can say *why* an argument did not fold (a wrong
    /// type is `E0205`, a too-strong qualifier `E0206`) without analyzing the
    /// expression a second time and duplicating its own diagnostics.
    pub(crate) fn eval_const_input_expr_checked(
        &mut self,
        expr: &ast::Expr,
    ) -> (Option<InputValue>, Type, Qualifier) {
        self.const_input_property_transparencies
            .remove(&expr.span());
        let saved_pending = std::mem::take(&mut self.pending_stmts);
        let input_count = self.inputs.len();
        let hir = self.analyze_expr(expr);
        // This expression is discarded after the fixed manifest value is folded.
        self.defer_temporal_fixedness_at(&hir, "constant metadata", "value", expr.span());
        let _metadata_only = std::mem::replace(&mut self.pending_stmts, saved_pending);
        // Internal text-format and display values use the string wire lane.
        // They must not be inferred as generic/string input defaults.
        if hir.ty.is_text_format() || hir.ty.is_plot_display() {
            return (None, hir.ty, hir.qualifier);
        }
        if hir.qualifier != Qualifier::Const {
            // A nested input declaration is not a const expression. The outer
            // declaration will fail, so do not leave an unreachable manifest
            // entry behind.
            self.inputs.truncate(input_count);
            return (None, hir.ty, hir.qualifier);
        }
        if hir.ty == Type::Color {
            if let Some(t) = self.const_color_transparency(&hir) {
                self.const_input_property_transparencies
                    .insert(expr.span(), t);
            }
        }
        (self.const_input_value_from_hir(&hir), hir.ty, hir.qualifier)
    }

    /// Fold a const-qualified HIR expression to the manifest value domain.
    /// This also powers propagation through ordinary `=` aliases. Keeping the
    /// fold on typed HIR means Pine's normal type/qualifier rules run first and
    /// prevents accidental widening from const to input/simple/series.
    pub(crate) fn const_input_value_from_hir(&self, expr: &HirExpr) -> Option<InputValue> {
        if expr.qualifier != Qualifier::Const {
            return None;
        }
        match &expr.kind {
            HirExprKind::ConstInt(value) => {
                if let Type::Enum(name) = &expr.ty {
                    let index = usize::try_from(*value).ok()?;
                    let variant = self.enums.get(name)?.variants.get(index)?;
                    Some(InputValue::Enum {
                        name: name.clone(),
                        index: index as u32,
                        member: variant.name.clone(),
                        title: variant.title.clone(),
                    })
                } else {
                    Some(InputValue::Int(*value))
                }
            }
            HirExprKind::ConstFloat(value) => finite_float(*value),
            HirExprKind::ConstBool(value) => Some(InputValue::Bool(*value)),
            HirExprKind::ConstString(value) => Some(InputValue::String(value.clone())),
            HirExprKind::ConstColor { r, g, b, t } => Some(InputValue::Color {
                r: *r,
                g: *g,
                b: *b,
                t: *t,
            }),
            HirExprKind::ReadVar { symbol, offset: 0 } => {
                self.const_input_values.get(symbol).cloned()
            }
            HirExprKind::IntToFloat(inner) => {
                let value = self.const_input_value_from_hir(inner)?;
                finite_float(numeric(&value)?)
            }
            HirExprKind::Unary { op, operand } => {
                let value = self.const_input_value_from_hir(operand)?;
                match (op, value) {
                    (UnOp::Neg, InputValue::Int(value)) => value.checked_neg().map(InputValue::Int),
                    (UnOp::Neg, InputValue::Float(value)) => finite_float(-value),
                    (UnOp::Not, InputValue::Bool(value)) => Some(InputValue::Bool(!value)),
                    _ => None,
                }
            }
            HirExprKind::Binary { op, lhs, rhs } => {
                let lhs = self.const_input_value_from_hir(lhs)?;
                let rhs = self.const_input_value_from_hir(rhs)?;
                Self::fold_const_binary(*op, lhs, rhs)
            }
            HirExprKind::Ternary {
                cond,
                then_e,
                else_e,
            } => match self.const_input_value_from_hir(cond)? {
                InputValue::Bool(true) => self.const_input_value_from_hir(then_e),
                InputValue::Bool(false) => self.const_input_value_from_hir(else_e),
                _ => None,
            },
            HirExprKind::CallPure { func, args } => {
                if *func == PureFunc::ColorTFloat {
                    return finite_float(self.const_color_transparency(args.first()?)?);
                }
                let values = args
                    .iter()
                    .map(|arg| self.const_input_value_from_hir(arg))
                    .collect::<Option<Vec<_>>>()?;
                Self::fold_const_pure_call(*func, &values)
            }
            _ => None,
        }
    }

    /// Fold a binary operation over two folded operands.
    ///
    /// Kept out of `const_input_value_from_hir`, which recurses once per
    /// level of the expression: only the operands are held across that
    /// recursion.
    #[inline(never)]
    fn fold_const_binary(op: BinOp, lhs: InputValue, rhs: InputValue) -> Option<InputValue> {
        match op {
            BinOp::Add => match (&lhs, &rhs) {
                (InputValue::Int(lhs), InputValue::Int(rhs)) => {
                    lhs.checked_add(*rhs).map(InputValue::Int)
                }
                (InputValue::String(lhs), InputValue::String(rhs)) => {
                    Some(InputValue::String(format!("{lhs}{rhs}")))
                }
                _ => finite_float(numeric(&lhs)? + numeric(&rhs)?),
            },
            BinOp::Sub => match (&lhs, &rhs) {
                (InputValue::Int(lhs), InputValue::Int(rhs)) => {
                    lhs.checked_sub(*rhs).map(InputValue::Int)
                }
                _ => finite_float(numeric(&lhs)? - numeric(&rhs)?),
            },
            BinOp::Mul => match (&lhs, &rhs) {
                (InputValue::Int(lhs), InputValue::Int(rhs)) => {
                    lhs.checked_mul(*rhs).map(InputValue::Int)
                }
                _ => finite_float(numeric(&lhs)? * numeric(&rhs)?),
            },
            // int / int keeps its fraction, like every other
            // division (the analyzer types it float).
            BinOp::Div => finite_float(numeric(&lhs)? / numeric(&rhs)?),
            BinOp::Rem => match (&lhs, &rhs) {
                (InputValue::Int(lhs), InputValue::Int(rhs)) => {
                    lhs.checked_rem(*rhs).map(InputValue::Int)
                }
                _ => finite_float(numeric(&lhs)? % numeric(&rhs)?),
            },
            // Two ints compare exactly. An int and a float compare
            // by value, as in the VM: a statically-int expression
            // can hold a float (`7 / 2 * 2` is `7.0`).
            BinOp::Eq | BinOp::Ne => {
                let equal = match (&lhs, &rhs) {
                    (InputValue::Int(_) | InputValue::Float(_), InputValue::Float(_))
                    | (InputValue::Float(_), InputValue::Int(_)) => {
                        pinewasm_num::comparison::eq(numeric(&lhs)?, numeric(&rhs)?)
                    }
                    _ => lhs == rhs,
                };
                Some(InputValue::Bool(equal == (op == BinOp::Eq)))
            }
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let result = match (&lhs, &rhs) {
                    (InputValue::Int(lhs), InputValue::Int(rhs)) => match op {
                        BinOp::Lt => lhs < rhs,
                        BinOp::Le => lhs <= rhs,
                        BinOp::Gt => lhs > rhs,
                        BinOp::Ge => lhs >= rhs,
                        _ => unreachable!(),
                    },
                    _ => {
                        let lhs = numeric(&lhs)?;
                        let rhs = numeric(&rhs)?;
                        match op {
                            BinOp::Lt => pinewasm_num::comparison::lt(lhs, rhs),
                            BinOp::Le => pinewasm_num::comparison::le(lhs, rhs),
                            BinOp::Gt => pinewasm_num::comparison::gt(lhs, rhs),
                            BinOp::Ge => pinewasm_num::comparison::ge(lhs, rhs),
                            _ => unreachable!(),
                        }
                    }
                };
                Some(InputValue::Bool(result))
            }
            BinOp::And | BinOp::Or => match (lhs, rhs) {
                (InputValue::Bool(lhs), InputValue::Bool(rhs)) => {
                    Some(InputValue::Bool(if op == BinOp::And {
                        lhs && rhs
                    } else {
                        lhs || rhs
                    }))
                }
                _ => None,
            },
        }
    }

    /// Fold a pure built-in call over its folded arguments, for
    /// `const_input_value_from_hir`; `None` for a function that is not folded.
    /// Kept out of that function for the same reason as `fold_const_binary`.
    #[inline(never)]
    fn fold_const_pure_call(func: PureFunc, values: &[InputValue]) -> Option<InputValue> {
        match func {
            PureFunc::MathAbs => match values.first()? {
                InputValue::Int(value) => value.checked_abs().map(InputValue::Int),
                value => finite_float(numeric(value)?.abs()),
            },
            PureFunc::MathMax
                if values
                    .iter()
                    .all(|value| matches!(value, InputValue::Int(_))) =>
            {
                values
                    .iter()
                    .filter_map(|value| match value {
                        InputValue::Int(value) => Some(*value),
                        _ => None,
                    })
                    .max()
                    .map(InputValue::Int)
            }
            PureFunc::MathMax => {
                let numbers = values.iter().map(numeric).collect::<Option<Vec<_>>>()?;
                finite_float(numbers.into_iter().reduce(f64::max)?)
            }
            PureFunc::MathSqrt => finite_float(numeric(values.first()?)?.sqrt()),
            PureFunc::MathSin => finite_float(pinewasm_num::raw::sin(numeric(values.first()?)?)),
            PureFunc::ColorNew => {
                let InputValue::Color { r, g, b, .. } = values.first()? else {
                    return None;
                };
                let transparency = numeric(values.get(1)?)?.clamp(0.0, 100.0);
                Some(InputValue::Color {
                    r: *r,
                    g: *g,
                    b: *b,
                    t: transparency,
                })
            }
            PureFunc::ColorRgb => {
                let channel = |index: usize, max: f64| {
                    Some(numeric(values.get(index)?)?.round().clamp(0.0, max) as u8)
                };
                Some(InputValue::Color {
                    r: channel(0, 255.0)?,
                    g: channel(1, 255.0)?,
                    b: channel(2, 255.0)?,
                    t: numeric(values.get(3)?)?.clamp(0.0, 100.0),
                })
            }
            PureFunc::ColorFromGradient => {
                let value = numeric(values.first()?)?;
                let bottom_value = numeric(values.get(1)?)?;
                let top_value = numeric(values.get(2)?)?;
                let rgba = |index: usize| match values.get(index)? {
                    InputValue::Color { r, g, b, t } => Some((*r, *g, *b, *t)),
                    _ => None,
                };
                let bottom = rgba(3)?;
                let top = rgba(4)?;
                let fraction = if top_value == bottom_value {
                    1.0
                } else {
                    ((value - bottom_value) / (top_value - bottom_value)).clamp(0.0, 1.0)
                };
                let interpolate = |from: u8, to: u8| {
                    (f64::from(from) + (f64::from(to) - f64::from(from)) * fraction).round() as u8
                };
                Some(InputValue::Color {
                    r: interpolate(bottom.0, top.0),
                    g: interpolate(bottom.1, top.1),
                    b: interpolate(bottom.2, top.2),
                    t: bottom.3 + (top.3 - bottom.3) * fraction,
                })
            }
            PureFunc::ColorRFloat
            | PureFunc::ColorGFloat
            | PureFunc::ColorBFloat
            | PureFunc::ColorTFloat => {
                let InputValue::Color { r, g, b, t } = values.first()? else {
                    return None;
                };
                finite_float(match func {
                    PureFunc::ColorRFloat => f64::from(*r),
                    PureFunc::ColorGFloat => f64::from(*g),
                    PureFunc::ColorBFloat => f64::from(*b),
                    _ => *t,
                })
            }
            PureFunc::Timestamp => Self::const_timestamp(values, 0),
            PureFunc::TimestampTz => {
                let InputValue::String(timezone) = values.first()? else {
                    return None;
                };
                // A timezone PineWASM does not know is not folded;
                // the interpreter stops on it.
                let InputValue::Int(local) = Self::const_timestamp(values.get(1..7)?, 0)? else {
                    return None;
                };
                const MARGIN: i64 = 2 * 86_400_000;
                if local <= i64::MIN + MARGIN || local >= i64::MAX - MARGIN {
                    return None;
                }
                let zone = pinewasm_tz::resolve(timezone)?;
                Some(InputValue::Int(Self::utc_from_local_ms(&zone, local)))
            }
            PureFunc::CastInt => match values.first()? {
                InputValue::Int(value) => Some(InputValue::Int(*value)),
                InputValue::Float(value) => Some(InputValue::Int(*value as i64)),
                _ => None,
            },
            PureFunc::CastFloat => finite_float(numeric(values.first()?)?),
            PureFunc::CastBool => match values.first()? {
                InputValue::Bool(value) => Some(InputValue::Bool(*value)),
                InputValue::Int(value) => Some(InputValue::Bool(*value != 0)),
                InputValue::Float(value) => Some(InputValue::Bool(*value != 0.0)),
                _ => None,
            },
            PureFunc::CastColor => match values.first()? {
                value @ InputValue::Color { .. } => Some(value.clone()),
                _ => None,
            },
            // Other pure functions remain ordinary runtime calls. They
            // are not part of any required input-argument surface.
            _ => None,
        }
    }

    fn const_timestamp(values: &[InputValue], timezone_offset_seconds: i64) -> Option<InputValue> {
        if values.len() != 6 {
            return None;
        }
        let mut fields = [0i64; 6];
        for (field, value) in fields.iter_mut().zip(values) {
            let InputValue::Int(value) = value else {
                return None;
            };
            *field = *value;
        }
        Self::epoch_ms_from_civil(fields, timezone_offset_seconds).map(InputValue::Int)
    }

    pub(crate) fn epoch_ms_from_civil(
        fields: [i64; 6],
        timezone_offset_seconds: i64,
    ) -> Option<i64> {
        let [year, month, day, hour, minute, second] = fields;
        let month_zero = month.checked_sub(1)?;
        let year = year.checked_add(month_zero.div_euclid(12))?;
        let month = month_zero.rem_euclid(12) + 1;
        let adjusted_year = if month <= 2 {
            year.checked_sub(1)?
        } else {
            year
        };
        let era = adjusted_year.div_euclid(400);
        let year_of_era = adjusted_year.rem_euclid(400);
        let shifted_month = if month > 2 { month - 3 } else { month + 9 };
        let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        let days = era
            .checked_mul(146_097)?
            .checked_add(day_of_era)?
            .checked_sub(719_468)?;
        let seconds = days
            .checked_mul(86_400)?
            .checked_add(hour.checked_mul(3_600)?)?
            .checked_add(minute.checked_mul(60)?)?
            .checked_add(second)?
            .checked_sub(timezone_offset_seconds)?;
        seconds.checked_mul(1_000)
    }

    /// The epoch-millisecond instant of the wall-clock reading `fields`
    /// (`year, month, day, hour, minute, second`) in `timezone`, as the
    /// interpreter's `timestamp(timezone, ...)` reads it, or `None` when the
    /// fields overflow or PineWASM does not know the timezone.
    pub(crate) fn epoch_ms_in_timezone(fields: [i64; 6], timezone: &str) -> Option<i64> {
        let local = Self::epoch_ms_from_civil(fields, 0)?;
        // As the interpreter: `na` (not folded) where converting could
        // leave i64.
        const MARGIN: i64 = 2 * 86_400_000;
        if local <= i64::MIN + MARGIN || local >= i64::MAX - MARGIN {
            return None;
        }
        let zone = pinewasm_tz::resolve(timezone)?;
        Some(Self::utc_from_local_ms(&zone, local))
    }

    /// `pinewasm_vm`'s `utc_from_local_ms`: the instant of an
    /// epoch-millisecond wall-clock reading of `zone`.
    pub(crate) fn utc_from_local_ms(zone: &pinewasm_tz::TimeZone<'_>, local: i64) -> i64 {
        zone.utc_from_local(local.div_euclid(1000))
            .saturating_mul(1000)
            .saturating_add(local.rem_euclid(1000))
    }

    /// The offset of a timezone already checked to be `±hh:mm` in range.
    pub(crate) fn fixed_timezone_offset_seconds(timezone: &str) -> i64 {
        let timezone = timezone.trim();
        let rest = timezone
            .strip_prefix("UTC")
            .or_else(|| timezone.strip_prefix("GMT"))
            .unwrap_or(timezone);
        if rest.is_empty() {
            return 0;
        }
        let (sign, digits) = match rest.as_bytes().first() {
            Some(b'+') => (1, &rest[1..]),
            Some(b'-') => (-1, &rest[1..]),
            _ => return 0,
        };
        let (hours, minutes) = match digits.split_once(':') {
            Some((hours, minutes)) => (hours, minutes),
            None if digits.len() <= 2 => (digits, "0"),
            None if digits.len() == 4 && digits.is_char_boundary(2) => (&digits[..2], &digits[2..]),
            None => return 0,
        };
        match (hours.parse::<i64>(), minutes.parse::<i64>()) {
            (Ok(hours), Ok(minutes))
                if (0..=14).contains(&hours) && (0..=59).contains(&minutes) =>
            {
                sign * (hours * 3_600 + minutes * 60)
            }
            _ => 0,
        }
    }

    /// Evaluate one input argument as a compile-time constant of the kind's
    /// value type. For [`InputKind::Source`] the argument must name a builtin
    /// series (`close`, `hl2`, ...). Reports `E0230` and returns `None` on any
    /// violation.
    pub(crate) fn eval_input_value(
        &mut self,
        callee: &str,
        field: &str,
        kind: InputKind,
        arg: &ast::Expr,
    ) -> Option<InputValue> {
        let expr = Self::ungroup_expr(arg);
        // Source and enum defaults carry identity that is not reducible to a
        // scalar manifest value, so retain their dedicated resolution paths.
        if kind == InputKind::Source {
            return match expr {
                ast::Expr::Ident(ident) => self
                    .resolve_input_source_ident(ident)
                    .map(InputValue::Source)
                    .or_else(|| {
                        self.input_type_error(
                            callee,
                            field,
                            "one of open/high/low/close/hl2/hlc3/ohlc4/hlcc4",
                            expr,
                        )
                    }),
                _ => self.input_type_error(
                    callee,
                    field,
                    "one of open/high/low/close/hl2/hlc3/ohlc4/hlcc4",
                    expr,
                ),
            };
        }

        let diagnostic_count = self.diagnostics.mark();
        let value = if matches!(kind, InputKind::Int | InputKind::Time) {
            self.eval_const_int_setting(arg)
        } else {
            self.eval_const_input_expr(arg)
        };
        if value.is_none() && self.diagnostics.errors_since(diagnostic_count) {
            return None;
        }
        match kind {
            InputKind::Int => match value {
                Some(value @ InputValue::Int(_)) => Some(value),
                _ => self.input_type_error(callee, field, "a const int expression", expr),
            },
            InputKind::Float | InputKind::Price => match value {
                Some(InputValue::Int(value)) => Some(InputValue::Float(value as f64)),
                Some(value @ InputValue::Float(_)) => Some(value),
                _ => self.input_type_error(callee, field, "a const numeric expression", expr),
            },
            InputKind::Bool => match value {
                Some(value @ InputValue::Bool(_)) => Some(value),
                _ => self.input_type_error(callee, field, "a const bool expression", expr),
            },
            InputKind::String => match value {
                Some(value @ InputValue::String(_)) => Some(value),
                _ => self.input_type_error(callee, field, "a const string expression", expr),
            },
            InputKind::Color => match value {
                Some(value @ InputValue::Color { .. }) => Some(value),
                _ => self.input_type_error(callee, field, "a const color expression", expr),
            },
            InputKind::Time => match value {
                Some(value @ InputValue::Int(_)) => Some(value),
                _ => self.input_type_error(
                    callee,
                    field,
                    "a const int expression (epoch milliseconds)",
                    expr,
                ),
            },
            InputKind::Enum => match value {
                Some(value @ InputValue::Enum { .. }) => Some(value),
                _ => self.input_type_error(
                    callee,
                    field,
                    "a const member of a declared enum, e.g. MyEnum.first",
                    expr,
                ),
            },
            InputKind::Source => unreachable!("source handled before const folding"),
        }
    }

    /// Resolve an `input.source` default/override name to a [`BuiltinSeries`].
    /// Exactly eight price series are eligible builtins. Compound sources are
    /// first-class [`BuiltinSeries`] variants; no source is approximated by a
    /// different lane, and `volume`/time/bool/context series are rejected.
    pub(crate) fn resolve_input_source(&self, name: &str) -> Option<BuiltinSeries> {
        self.registry
            .lookup_series(name)
            .filter(|series| series.is_input_source())
    }

    /// Resolve a source default written in the script. A name that a script
    /// variable shadows is not a built-in source. A built-in one is a use of
    /// the built-in, so a later declaration shadowing it is `E0211`.
    fn resolve_input_source_ident(&mut self, ident: &ast::Ident) -> Option<BuiltinSeries> {
        if self.lookup_variable(&ident.name).is_some() {
            return None;
        }
        let series = self.resolve_input_source(&ident.name)?;
        if self.lib_ctx.is_empty() {
            self.builtin_reads
                .entry(ident.name.clone())
                .or_insert(ident.span);
        }
        self.reject_built_in_hidden_by_a_later_library_global(ident);
        Some(series)
    }

    /// Evaluate an `options = [...]` array literal into a value list. Every
    /// element must be a const of the kind's value type.
    pub(crate) fn eval_input_options(
        &mut self,
        callee: &str,
        kind: InputKind,
        arg: &ast::Expr,
    ) -> Option<Vec<InputValue>> {
        let mut expr = arg;
        while let ast::Expr::Group { inner, .. } = expr {
            expr = inner;
        }
        let ast::Expr::Tuple { elements, .. } = expr else {
            self.error(
                codes::E0230,
                format!("`{callee}` argument `options` must be a list, e.g. options=[1, 2, 3]"),
                arg.span(),
                "expected a bracketed list of const values",
            );
            return None;
        };
        let mut values = Vec::with_capacity(elements.len());
        let mut ok = true;
        for element in elements {
            match self.eval_input_value(callee, "options", kind, element) {
                Some(v) => values.push(v),
                None => ok = false,
            }
        }
        ok.then_some(values)
    }

    /// A const string argument (`title`, tooltip/layout fields). `E0230` on a
    /// non-const or wrong-typed expression.
    pub(crate) fn eval_const_string(
        &mut self,
        callee: &str,
        field: &str,
        arg: &ast::Expr,
    ) -> Option<String> {
        let expr = Self::ungroup_expr(arg);
        match self.eval_const_input_expr(arg) {
            Some(InputValue::String(value)) => Some(value),
            _ => {
                self.input_type_error(callee, field, "a const string expression", expr);
                None
            }
        }
    }

    /// A const bool argument (`confirm`). `E0230` on a non-const or
    /// wrong-typed expression.
    pub(crate) fn eval_const_bool(
        &mut self,
        callee: &str,
        field: &str,
        arg: &ast::Expr,
    ) -> Option<bool> {
        let expr = Self::ungroup_expr(arg);
        match self.eval_const_input_expr(arg) {
            Some(InputValue::Bool(value)) => Some(value),
            _ => {
                self.input_type_error(callee, field, "a const bool expression", expr);
                None
            }
        }
    }

    /// Report an `E0230` input argument type error and return `None`.
    pub(crate) fn input_type_error(
        &mut self,
        callee: &str,
        field: &str,
        expected: &str,
        expr: &ast::Expr,
    ) -> Option<InputValue> {
        self.error(
            codes::E0230,
            format!("`{callee}` argument `{field}` must be {expected}"),
            expr.span(),
            format!("expected {expected}"),
        );
        None
    }

    /// Enforce that an input's default satisfies its declared constraints:
    /// numeric bounds and, for every options-capable kind, membership in the
    /// declared options list. `E0230` on a violation.
    pub(crate) fn check_default_in_constraints(
        &mut self,
        callee: &str,
        defval: &InputValue,
        constraints: &InputConstraints,
        arg: &ast::Expr,
    ) -> bool {
        let as_f64 = |v: &InputValue| match v {
            InputValue::Int(i) => Some(*i as f64),
            InputValue::Float(f) => Some(*f),
            _ => None,
        };
        if let Some(d) = as_f64(defval) {
            if let Some(min) = constraints.min.as_ref().and_then(as_f64) {
                if d < min {
                    self.error(
                        codes::E0230,
                        format!("`{callee}` default {d} is below `minval` {min}"),
                        arg.span(),
                        "the default must be within the declared range",
                    );
                    return false;
                }
            }
            if let Some(max) = constraints.max.as_ref().and_then(as_f64) {
                if d > max {
                    self.error(
                        codes::E0230,
                        format!("`{callee}` default {d} is above `maxval` {max}"),
                        arg.span(),
                        "the default must be within the declared range",
                    );
                    return false;
                }
            }
        }
        if !constraints.options.is_empty()
            && !constraints.options.iter().any(|option| option == defval)
        {
            self.error(
                codes::E0230,
                format!("`{callee}` default is not one of the declared `options`"),
                arg.span(),
                "the default must be one of the options",
            );
            return false;
        }
        true
    }
}
