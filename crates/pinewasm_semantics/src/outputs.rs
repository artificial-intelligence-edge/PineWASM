//! Output and side-effect calls: `plot*`/`hline`/`fill` and friends, `log.*`,
//! `alert`/`alertcondition`, display-argument checking, `runtime.error`, and
//! the `strategy.*` order/risk statement calls.

use crate::*;

/// A resolved `fill` endpoint slot and the variable it was read through, if any.
type FillEndpointHandle = (PlotSlot, Option<SymbolId>);

impl<'r> Analyzer<'r> {
    /// Reject known invalid precision before converting it into display metadata.
    /// Input-qualified values remain runtime settings, not compile-time defaults.
    fn check_plot_precision(&mut self, name: &str, expr: &HirExpr) {
        if expr.qualifier != Qualifier::Const || expr.ty != Type::Int {
            return;
        }
        if let Some(InputValue::Int(value)) = self.const_input_value_from_hir(expr) {
            if !(0..=16).contains(&value) {
                self.error(
                    codes::E0219,
                    format!("invalid `{name}` argument `precision`: must be between 0 and 16, found {value}"),
                    expr.span,
                    "use an integer from 0 through 16",
                );
            }
        }
    }

    /// A known linewidth below one is a source error. An absent
    /// width and an input-qualified width are left to the display contract.
    fn check_plot_linewidth(&mut self, name: &str, expr: &HirExpr) {
        if expr.qualifier != Qualifier::Const || expr.ty != Type::Int {
            return;
        }
        let Some(InputValue::Int(value)) = self.const_input_value_from_hir(expr) else {
            return;
        };
        if value < 1 {
            self.error(
                codes::E0219,
                format!("invalid `{name}` argument `linewidth`: must be at least 1, found {value}"),
                expr.span,
                "use a linewidth of 1 or greater",
            );
        }
    }

    /// Evaluate fixed styles in Pine when they lack a portable expression.
    /// Always evaluate input linewidth here so every host receives the same
    /// signed width, including an absent value or a nonpositive input.
    fn bind_runtime_style_fallbacks(
        &mut self,
        style: &mut DisplayStyle,
        names: &[&str],
        checked: &[Option<HirExpr>],
    ) {
        for (name, expr) in names.iter().zip(checked) {
            let Some(expr) = expr.as_ref().filter(|e| e.qualifier <= Qualifier::Simple) else {
                continue;
            };
            let input_linewidth = *name == "linewidth" && expr.qualifier == Qualifier::Input;
            if !input_linewidth && !style.unsupported.iter().any(|option| option.field == *name
                && option.reason == "the portable display contract carries this metadata once per output, not per bar") {
                continue;
            }
            let slot = self.allocate_synthetic_slot(expr.ty.clone(), expr.span);
            self.symbols[slot.0 as usize].qualifier = expr.qualifier;
            // A fixed expression is evaluated once. Persistence also makes
            // the slot an observed root in the CSIL optimization pipeline.
            self.symbols[slot.0 as usize].persistent = true;
            self.symbols[slot.0 as usize].name = format!("%render{}", slot.0);
            self.pending_stmts.push(HirStmt::Assign {
                symbol: SymbolId(slot.0),
                value: expr.clone(),
                init: true,
                span: expr.span,
            });
            style
                .runtime_scalars
                .push(pinewasm_hir::RuntimeStyleScalar {
                    field: (*name).to_string(),
                    slot,
                    qualifier: expr.qualifier,
                });
            style.unsupported.retain(|option| option.field != *name);
            if input_linewidth {
                style.bound_scalars.retain(|binding| binding.field != *name);
            }
        }
    }

    pub(crate) fn analyze_plot_call(
        &mut self,
        callee: &ast::Ident,
        args: &[ast::CallArg],
        span: Span,
    ) -> Option<HirStmt> {
        let func = self
            .registry
            .lookup_function(&callee.name)
            .expect("caller checked");
        let param_names: Vec<&'static str> = func.params.iter().map(|p| p.name).collect();
        let bound = self.bind_optional_args(func.name, &param_names, 1, args, callee.span)?;
        let mut checked = Vec::with_capacity(bound.len());
        let mut ok = true;
        for (arg, param) in bound.iter().zip(&func.params) {
            let Some(arg) = arg else {
                checked.push(None);
                continue;
            };
            let expected = param.expected_type();
            let mut hir = self.analyze_expr_expecting(arg, Some(&expected));
            if hir.ty.is_error() {
                ok = false;
            } else {
                if hir.ty != param.ty
                    && !(param.ty == Type::PlotDisplay && hir.ty == Type::PlotSimpleDisplay)
                {
                    if param.ty == Type::Float && hir.ty == Type::Int {
                        hir = Self::coerce_int_to_float(hir);
                    } else {
                        self.error(
                            codes::E0205,
                            format!(
                                "`plot` argument `{}` must be {}, found {}",
                                param.name,
                                param.ty.describe(),
                                hir.ty.describe()
                            ),
                            arg.span(),
                            format!("expected {}", param.ty.describe()),
                        );
                        ok = false;
                    }
                }
                if param.max_qualifier < Qualifier::Series {
                    self.defer_temporal_fixedness_at(&hir, func.name, param.name, arg.span());
                }
                if hir.qualifier > param.max_qualifier {
                    self.error(
                        codes::E0206,
                        format!(
                            "`plot` argument `{}` must be {} or weaker, found {}",
                            param.name,
                            param.max_qualifier.describe(),
                            hir.qualifier.describe()
                        ),
                        arg.span(),
                        format!("expected a {} value", param.max_qualifier.describe()),
                    );
                    ok = false;
                }
            }
            match param.name {
                "precision" => self.check_plot_precision("plot", &hir),
                "linewidth" => self.check_plot_linewidth("plot", &hir),
                _ => {}
            }
            checked.push(Some(hir));
        }
        if !ok {
            return None;
        }
        let value = checked[0].take().expect("plot has one required arg");

        // A title is a constant string expression, including variables,
        // concatenation and const-returning calls. Preserve the folded value
        // instead of requiring a source string literal.
        let title = match checked[1].as_ref() {
            None => None,
            Some(expr) => match self.const_input_value_from_hir(expr) {
                Some(InputValue::String(value)) => Some(value),
                _ => {
                    self.error(
                        codes::E0205,
                        "`plot` title must be a constant string expression",
                        expr.span,
                        "expected a string known at compile time",
                    );
                    return None;
                }
            },
        };

        let bind: &StyleBinder<'_> = &|expr| self.input_active_expr_from_hir(expr);
        let mut style = DisplayStyle::default();
        style.source_order =
            Some((self.plots.len() + self.visuals.len() + self.fills.len()) as u32);
        style.line_width = style_positive_u32(
            &mut style,
            "linewidth",
            bound[3].is_some(),
            checked[3].as_ref(),
            bind,
        );
        style.plot_style = checked[4]
            .as_ref()
            .and_then(const_string_hir)
            .and_then(plot_style_kind)
            .unwrap_or_default();
        bind_dynamic_style(
            &mut style,
            "style",
            bound[4].is_some(),
            checked[4].as_ref(),
            bind,
        );
        style.track_price = style_bool(
            &mut style,
            "trackprice",
            bound[5].is_some(),
            checked[5].as_ref(),
            bind,
            false,
        );
        style.histogram_base = style_float(
            &mut style,
            "histbase",
            bound[6].is_some(),
            checked[6].as_ref(),
            bind,
            0.0,
        );
        style.offset = style_i32(
            &mut style,
            "offset",
            bound[7].is_some(),
            checked[7].as_ref(),
            bind,
            0,
        );
        style.join = style_bool(
            &mut style,
            "join",
            bound[8].is_some(),
            checked[8].as_ref(),
            bind,
            false,
        );
        style.editable = style_bool(
            &mut style,
            "editable",
            bound[9].is_some(),
            checked[9].as_ref(),
            bind,
            true,
        );
        style.show_last = style_optional_u32(
            &mut style,
            "show_last",
            bound[10].is_some(),
            checked[10].as_ref(),
            bind,
        );
        style.display = checked[11]
            .as_ref()
            .and_then(plot_display)
            .unwrap_or_default();
        if checked[11].as_ref().and_then(plot_display).is_none() {
            bind_dynamic_style(
                &mut style,
                "display",
                bound[11].is_some(),
                checked[11].as_ref(),
                bind,
            );
        }
        style.format = checked[12].as_ref().and_then(const_string_hir);
        bind_dynamic_style(
            &mut style,
            "format",
            bound[12].is_some(),
            checked[12].as_ref(),
            bind,
        );
        style.precision = style_optional_u8(
            &mut style,
            "precision",
            bound[13].is_some(),
            checked[13].as_ref(),
            bind,
            0,
            16,
        );
        style.force_overlay = style_bool(
            &mut style,
            "force_overlay",
            bound[14].is_some(),
            checked[14].as_ref(),
            bind,
            false,
        );
        style.line_style = checked[15]
            .as_ref()
            .and_then(const_string_hir)
            .and_then(plot_line_style)
            .unwrap_or_default();
        bind_dynamic_style(
            &mut style,
            "linestyle",
            bound[15].is_some(),
            checked[15].as_ref(),
            bind,
        );
        self.bind_runtime_style_fallbacks(&mut style, &param_names, &checked);
        let color = checked[2].take().unwrap_or_else(|| {
            let [r, g, b, t] = pinewasm_hir::DEFAULT_PLOT_COLOR;
            HirExpr {
                kind: HirExprKind::ConstColor {
                    r,
                    g,
                    b,
                    t: f64::from(t),
                },
                ty: Type::Color,
                qualifier: Qualifier::Const,
                span,
            }
        });
        style.plot_color_mode = Some(if color.qualifier == Qualifier::Series {
            pinewasm_hir::PlotColorMode::Series
        } else {
            pinewasm_hir::PlotColorMode::Fixed
        });
        self.add_plot_counts(1 + u32::from(color.qualifier == Qualifier::Series), span);
        let colors = vec![(PlotColorRole::Color, color)];
        let color_roles = colors.iter().map(|(role, _)| *role).collect();
        let slot = PlotSlot(self.plots.len() as u32);
        self.plots.push(PlotInfo {
            slot,
            title,
            style,
            color_roles,
            span,
        });
        Some(HirStmt::Plot {
            slot,
            value,
            colors,
            span,
        })
    }

    /// Analyze `plot(...)`/`hline(...)` in value position. The display output
    /// is emitted normally while the expression becomes a compile-time plot ID
    /// usable by `fill(...)`.
    pub(crate) fn analyze_plot_handle_call(
        &mut self,
        kind: BuiltinFunctionKind,
        callee: &ast::Ident,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        if self.in_local_scope() || self.ternary_operand_depth > 0 {
            let place = if self.in_local_scope() {
                "local scope"
            } else {
                "a ternary operand"
            };
            self.error(
                codes::E0254,
                format!("Cannot use \"{}\" in {place}", callee.name),
                span,
                "plot-family calls are only allowed in global scope",
            );
            for arg in args {
                let _ = self.analyze_expr(&arg.value);
            }
            return self.error_expr(span);
        }
        let stmt = match kind {
            BuiltinFunctionKind::Plot => self.analyze_plot_call(callee, args, span),
            BuiltinFunctionKind::Display(DisplayKind::Hline) => {
                self.analyze_display_call(DisplayKind::Hline, args, span)
            }
            _ => unreachable!("only plot and hline produce plot handles"),
        };
        let Some(stmt) = stmt else {
            return self.error_expr(span);
        };
        let index = match &stmt {
            HirStmt::Plot { slot, .. } => slot.0,
            _ => unreachable!("plot and hline always emit a plot statement"),
        };
        self.pending_stmts.push(stmt);
        HirExpr {
            kind: HirExprKind::ConstInt(i64::from(index)),
            ty: if kind == BuiltinFunctionKind::Display(DisplayKind::Hline) {
                Type::Hline
            } else {
                Type::Plot
            },
            qualifier: Qualifier::Const,
            span,
        }
    }

    /// `log.info/warning/error(formatString, arg0, ...)`. The format
    /// string must be `string`; substitution arguments are `str.format`-style
    /// stringable scalars. Produces a [`HirStmt::Log`]; allowed anywhere a
    /// statement is (including function bodies and `if` branches).
    pub(crate) fn analyze_log_call(
        &mut self,
        level: LogLevel,
        args: &[ast::CallArg],
        span: Span,
    ) -> Option<HirStmt> {
        // The overloads are `log.*(message)` and `log.*(formatString, arg0, ...)`.
        // Substitution arguments are positional-only, so a named argument can
        // only be the sole one, under either overload's name.
        let named_message = args.len() == 1
            && args[0]
                .name
                .as_ref()
                .is_some_and(|name| matches!(name.name.as_str(), "message" | "formatString"));
        if args.is_empty() || (!named_message && args.iter().any(|a| a.name.is_some())) {
            self.error(
                codes::E0204,
                format!(
                    "`log.{}` expects (formatString, ...) positional arguments",
                    level.member()
                ),
                span,
                "pass a format string and optional substitution values",
            );
            for a in args {
                let _ = self.analyze_expr(&a.value);
            }
            return None;
        }
        if args.len() > STR_FORMAT_MAX_ARGS as usize {
            self.error(
                codes::E0204,
                format!(
                    "`log.{}` accepts at most {} argument(s), found {}",
                    level.member(),
                    STR_FORMAT_MAX_ARGS,
                    args.len()
                ),
                span,
                "too many substitution arguments",
            );
            for a in args {
                let _ = self.analyze_expr(&a.value);
            }
            return None;
        }
        // Format string: must be `string`.
        let format = self.analyze_expr_expecting(&args[0].value, Some(&Type::String));
        if !format.ty.is_error() && format.ty != Type::String {
            self.error(
                codes::E0205,
                format!(
                    "`log.{}` format must be a string, found {}",
                    level.member(),
                    format.ty.describe()
                ),
                args[0].value.span(),
                "expected a format string",
            );
        }
        // Scalar substitutions exclude colors. Array
        // substitutions need separate runtime formatting support.
        let mut sub_args = Vec::with_capacity(args.len().saturating_sub(1));
        for a in &args[1..] {
            let hir = self.analyze_expr(&a.value);
            let stringable = is_numeric(&hir.ty) || matches!(hir.ty, Type::Bool | Type::String);
            if !hir.ty.is_error() && !stringable {
                self.error(
                    codes::E0205,
                    format!(
                        "`log.{}` substitution argument must be a scalar value, found {}",
                        level.member(),
                        hir.ty.describe()
                    ),
                    a.value.span(),
                    "expected a number, bool, or string",
                );
            }
            sub_args.push(hir);
        }
        Some(HirStmt::Log {
            level,
            format,
            args: sub_args,
            span,
        })
    }

    /// `alert(message, freq?)`. `message` is a `string`; `freq` is one of
    /// the `alert.freq_*` const-string tokens (defaulting to
    /// `alert.freq_once_per_bar`). Produces a [`HirStmt::Alert`].
    pub(crate) fn analyze_alert_call(
        &mut self,
        args: &[ast::CallArg],
        span: Span,
    ) -> Option<HirStmt> {
        let bound = self.bind_positional("alert", &["message", "freq"], args, span)?;
        let args = bound.as_slice();
        if args.is_empty() || args.len() > 2 {
            self.error(
                codes::E0204,
                format!("`alert` expects 1 to 2 argument(s), found {}", args.len()),
                span,
                "expected (message, freq?)",
            );
            for a in args {
                let _ = self.analyze_expr(a);
            }
            return None;
        }
        let message = self.analyze_expr_expecting(&args[0], Some(&Type::String));
        if !message.ty.is_error() && message.ty != Type::String {
            self.error(
                codes::E0205,
                format!(
                    "`alert` message must be a string, found {}",
                    message.ty.describe()
                ),
                args[0].span(),
                "expected a string message",
            );
        }
        let freq = match args.get(1) {
            None => HirExpr {
                kind: HirExprKind::ConstString("alert.freq_once_per_bar".to_string()),
                ty: Type::String,
                qualifier: Qualifier::Const,
                span,
            },
            Some(arg) => {
                let f = self.analyze_expr_expecting(arg, Some(&Type::String));
                if !f.ty.is_error() && f.ty != Type::String {
                    self.error(
                        codes::E0205,
                        format!(
                            "`alert` freq must be an `alert.freq_*` token, found {}",
                            f.ty.describe()
                        ),
                        arg.span(),
                        "expected one of alert.freq_all/once_per_bar/once_per_bar_close",
                    );
                }
                f
            }
        };
        Some(HirStmt::Alert {
            message,
            freq,
            span,
        })
    }

    /// `alertcondition(condition, title?, message?)`. `condition` is a
    /// `bool` series; `title`/`message` are strings. Produces a
    /// [`HirStmt::AlertCondition`] that fires only on bars where the condition
    /// is true.
    pub(crate) fn analyze_alertcondition_call(
        &mut self,
        args: &[ast::CallArg],
        span: Span,
    ) -> Option<HirStmt> {
        // Sparse binding: `title` and `message` are independently optional, so
        // `alertcondition(cond, message = "...")` may skip `title`.
        let bound = self.bind_optional_args(
            "alertcondition",
            &["condition", "title", "message"],
            1,
            args,
            span,
        );
        let Some(bound) = bound else {
            for a in args {
                let _ = self.analyze_expr(&a.value);
            }
            return None;
        };
        let condition_arg = bound[0].as_ref()?;
        let condition = self.analyze_expr_expecting(condition_arg, Some(&Type::Bool));
        if !condition.ty.is_error() && condition.ty != Type::Bool {
            self.error(
                codes::E0205,
                format!(
                    "`alertcondition` condition must be a bool, found {}",
                    condition.ty.describe()
                ),
                condition_arg.span(),
                "expected a bool condition",
            );
        }
        let str_or_default = |this: &mut Self, arg: Option<&ast::Expr>, default: &str| -> HirExpr {
            match arg {
                None => HirExpr {
                    kind: HirExprKind::ConstString(default.to_string()),
                    ty: Type::String,
                    qualifier: Qualifier::Const,
                    span,
                },
                Some(a) => {
                    let h = this.analyze_expr_expecting(a, Some(&Type::String));
                    if !h.ty.is_error() && h.ty != Type::String {
                        this.error(
                            codes::E0205,
                            format!(
                                "`alertcondition` argument must be a string, found {}",
                                h.ty.describe()
                            ),
                            a.span(),
                            "expected a string",
                        );
                    }
                    h
                }
            }
        };
        let title = str_or_default(self, bound[1].as_ref(), "");
        let message = str_or_default(self, bound[2].as_ref(), "");
        self.add_plot_counts(1, span);
        Some(HirStmt::AlertCondition {
            condition,
            title,
            message,
            span,
        })
    }

    pub(crate) fn emit_data_plot(
        &mut self,
        title: String,
        value: HirExpr,
        style: DisplayStyle,
        colors: Vec<(PlotColorRole, HirExpr)>,
        rendering_colors: &HashMap<PlotColorRole, HirExpr>,
        span: Span,
    ) -> HirStmt {
        let slot = PlotSlot(self.plots.len() as u32);
        for (role, color) in &colors {
            self.rendering_inline.attach(
                rendering_inline_provenance::Anchor::Plot(slot, *role),
                color,
                rendering_colors.get(role).cloned(),
            );
        }
        self.plots.push(PlotInfo {
            slot,
            title: Some(title),
            style,
            color_roles: colors.iter().map(|(role, _)| *role).collect(),
            span,
        });
        HirStmt::Plot {
            slot,
            value,
            colors,
            span,
        }
    }

    /// Analyze a display-plot-family call and desugar it. Every argument
    /// is evaluated and type-checked (so scripts using the function compile and
    /// run and argument errors surface); data-representable series are surfaced
    /// to the run output as plots (with descriptive titles), while the purely
    /// visual glyph/shape/shading is not rendered by the headless VM. Never
    /// panics on `na`/out-of-range arguments.
    ///
    /// Data surfaced per kind:
    /// * `plotshape`/`plotchar`: one plot — boolean inputs become `1.0`/`0.0`;
    ///   numeric inputs retain their original values for lossless host-data
    ///   round trips.
    /// * `plotarrow`: one plot carrying the numeric value.
    /// * `plotcandle`/`plotbar`: four plots (open, high, low, close).
    /// * `hline`: one plot — the price on every bar.
    /// * `bgcolor`/`barcolor`/`fill`: no data plot (color/id only); the call is
    ///   type-checked and its args evaluated, then recorded but not rendered.
    pub(crate) fn analyze_display_call(
        &mut self,
        kind: DisplayKind,
        args: &[ast::CallArg],
        span: Span,
    ) -> Option<HirStmt> {
        // Resolve the fill handle family once. In particular, evaluating an
        // inline plot/hline twice would create duplicate outputs. The three
        // fill overload families have different names and optional tails.
        let first_fill_handle = if kind == DisplayKind::Fill {
            args.first()
                .filter(|arg| arg.name.is_none())
                .or_else(|| {
                    args.iter().find(|arg| {
                        arg.name
                            .as_ref()
                            .is_some_and(|name| matches!(name.name.as_str(), "plot1" | "hline1"))
                    })
                })
                .map(|arg| self.analyze_expr(&arg.value))
        } else {
            None
        };
        // Resolve the positional overload once, retaining its checked expression.
        let third =
            if kind == DisplayKind::Fill && args.get(2).is_some_and(|arg| arg.name.is_none()) {
                Some(self.analyze_expr_expecting(&args[2].value, Some(&Type::Color)))
            } else {
                None
            };
        let gradient = kind == DisplayKind::Fill
            && (third.as_ref().is_some_and(|expr| is_numeric(&expr.ty))
                || args.iter().any(|arg| {
                    arg.name.as_ref().is_some_and(|name| {
                        matches!(
                            name.name.as_str(),
                            "top_value" | "bottom_value" | "top_color" | "bottom_color"
                        )
                    })
                }));
        if !gradient {
            if let Some(expr) = &third {
                if !expr.ty.is_error() && expr.ty != Type::Color {
                    self.error(
                        codes::E0205,
                        "`fill` color must be a color",
                        args[2].value.span(),
                        "expected a color",
                    );
                }
            }
        }
        let hline_fill = first_fill_handle
            .as_ref()
            .is_some_and(|handle| handle.ty == Type::Hline);
        let param_names = if gradient {
            &[
                if hline_fill { "hline1" } else { "plot1" },
                if hline_fill { "hline2" } else { "plot2" },
                "top_value",
                "bottom_value",
                "top_color",
                "bottom_color",
                "title",
                "display",
                "fillgaps",
                "editable",
            ][..]
        } else if kind == DisplayKind::Fill && !hline_fill {
            &[
                "plot1",
                "plot2",
                "color",
                "title",
                "editable",
                "show_last",
                "fillgaps",
                "display",
            ][..]
        } else {
            kind.param_names()
        };
        let required = kind.required_count();
        let min_data = kind.data_count();
        let bound = self.bind_optional_args(kind.name(), param_names, required, args, span)?;

        // Type-check every argument against its parameter's expected type and
        // retain the checked HIR so color expressions can execute per bar.
        let mut checked: Vec<Option<HirExpr>> = Vec::with_capacity(bound.len());
        let mut rendering_arguments = HashMap::new();
        let mut title: Option<String> = None;
        for (i, arg) in bound.iter().enumerate() {
            let Some(arg) = arg else {
                checked.push(None);
                continue;
            };
            let pname = param_names[i];
            let checked_arg = if i == 0 && kind == DisplayKind::Fill {
                let handle = first_fill_handle.clone().expect("bound first fill handle");
                let expected = if pname == "hline1" {
                    Type::Hline
                } else {
                    Type::Plot
                };
                if !handle.ty.is_error() && handle.ty != expected {
                    self.error(
                        codes::E0205,
                        format!(
                            "`fill` argument `{pname}` must be {}, found {}",
                            expected.describe(),
                            handle.ty.describe()
                        ),
                        arg.span(),
                        "both endpoints must match the selected fill overload",
                    );
                }
                handle
            } else if i == 2 {
                third
                    .clone()
                    .unwrap_or_else(|| self.check_display_arg(kind, pname, i < min_data, arg))
            } else {
                self.check_display_arg(kind, pname, i < min_data, arg)
            };
            if let Some(shadow) = self.rendering_inline.last(&checked_arg) {
                rendering_arguments.insert(pname, shadow);
            }
            let max_qualifier = kind.param_qualifier(pname);
            if max_qualifier < Qualifier::Series {
                self.defer_temporal_fixedness_at(&checked_arg, kind.name(), pname, arg.span());
            }
            if !checked_arg.ty.is_error() && checked_arg.qualifier > max_qualifier {
                self.error(
                    codes::E0206,
                    format!(
                        "`{}` argument `{pname}` must be {} or weaker, found {}",
                        kind.name(),
                        max_qualifier.describe(),
                        checked_arg.qualifier.describe(),
                    ),
                    arg.span(),
                    format!("expected a {} value", max_qualifier.describe()),
                );
            }
            if pname == "precision" {
                self.check_plot_precision(kind.name(), &checked_arg);
            }
            if kind == DisplayKind::Hline && pname == "linewidth" {
                self.check_plot_linewidth("hline", &checked_arg);
            }
            if pname == "title" {
                title = match self.const_input_value_from_hir(&checked_arg) {
                    Some(InputValue::String(value)) => Some(value),
                    _ => None,
                };
            }
            checked.push(Some(checked_arg));
        }

        let mut style = display_style_for_call(kind, param_names, &bound, &checked, &|expr| {
            self.input_active_expr_from_hir(expr)
        });
        self.bind_runtime_style_fallbacks(&mut style, param_names, &checked);
        style.source_order =
            Some((self.plots.len() + self.visuals.len() + self.fills.len()) as u32);
        let data: Vec<HirExpr> = (0..min_data)
            .map(|index| checked[index].take().expect("required data argument"))
            .collect();
        // Each resolved endpoint carries the variable it was read through, if
        // any, so a later `:=` of a `var`/`varip` source can still report it.
        let fill_resolved: Option<[Option<FillEndpointHandle>; 2]> = (kind == DisplayKind::Fill)
            .then(|| {
                let endpoint = |analyzer: &Self, index: usize| -> Option<FillEndpointHandle> {
                    match checked[index].as_ref() {
                        Some(HirExpr {
                            kind: HirExprKind::ConstInt(slot),
                            ty: Type::Plot | Type::Hline,
                            ..
                        }) => u32::try_from(*slot).ok().map(|slot| (PlotSlot(slot), None)),
                        Some(HirExpr {
                            ty: Type::Plot | Type::Hline,
                            ..
                        }) => {
                            let mut probe: &ast::Expr = bound[index].as_ref()?;
                            while let ast::Expr::Group { inner, .. } = probe {
                                probe = inner;
                            }
                            let ast::Expr::Ident(ident) = probe else {
                                return None;
                            };
                            let id = analyzer.lookup_variable(&ident.name)?;
                            analyzer.plot_handles.get(&id).map(|slot| (*slot, Some(id)))
                        }
                        _ => None,
                    }
                };
                [endpoint(self, 0), endpoint(self, 1)]
            });
        let fill_handles: Option<[Option<PlotSlot>; 2]> =
            fill_resolved.map(|ends| ends.map(|end| end.map(|(slot, _)| slot)));
        let fill_endpoints = fill_handles.and_then(|[plot1, plot2]| plot1.zip(plot2));
        if fill_endpoints.is_some() {
            let call = if hline_fill { "hline" } else { "plot" };
            for (index, end) in fill_resolved.into_iter().flatten().enumerate() {
                let Some((_, Some(id))) = end else {
                    continue;
                };
                let arg_span = bound[index].as_ref().map_or(span, ast::Expr::span);
                for root in self.plot_handle_persistent_roots(id) {
                    self.persistent_fill_endpoints.push(PersistentFillEndpoint {
                        root,
                        span: arg_span,
                        param: param_names[index],
                        call,
                    });
                }
            }
        }
        if let Some(handles) = fill_handles.filter(|_| fill_endpoints.is_none()) {
            // A band needs both plot slots before the first bar. An endpoint
            // of the right type that names no single call (a conditional
            // value, a reassigned variable) would otherwise drop the band
            // without a diagnostic. Mistyped endpoints are reported above.
            let handle_types = (
                checked[0].as_ref().map(|expr| &expr.ty),
                checked[1].as_ref().map(|expr| &expr.ty),
            );
            if matches!(
                handle_types,
                (Some(Type::Plot), Some(Type::Plot)) | (Some(Type::Hline), Some(Type::Hline))
            ) {
                let call = if hline_fill { "hline" } else { "plot" };
                for index in 0..2 {
                    if handles[index].is_none() {
                        self.error(
                            codes::E0205,
                            format!(
                                "`fill` argument `{}` does not name one `{call}` call",
                                param_names[index]
                            ),
                            bound[index].as_ref().map_or(span, ast::Expr::span),
                            format!(
                                "pass a variable that holds one `{call}` result and is not reassigned"
                            ),
                        );
                    }
                }
            }
        }
        let mut take = |name: &str| {
            param_names
                .iter()
                .position(|candidate| *candidate == name)
                .and_then(|index| checked[index].take())
        };
        let color_names: &[(PlotColorRole, &str)] = match kind {
            DisplayKind::PlotShape | DisplayKind::PlotChar => &[
                (PlotColorRole::Color, "color"),
                (PlotColorRole::Text, "textcolor"),
            ],
            DisplayKind::PlotArrow => &[
                (PlotColorRole::Up, "colorup"),
                (PlotColorRole::Down, "colordown"),
            ],
            DisplayKind::PlotCandle => &[
                (PlotColorRole::Color, "color"),
                (PlotColorRole::Wick, "wickcolor"),
                (PlotColorRole::Border, "bordercolor"),
            ],
            DisplayKind::PlotBar => &[(PlotColorRole::Color, "color")],
            DisplayKind::Hline | DisplayKind::BgColor | DisplayKind::BarColor => {
                &[(PlotColorRole::Color, "color")]
            }
            DisplayKind::Fill if gradient => &[
                (PlotColorRole::TopValue, "top_value"),
                (PlotColorRole::BottomValue, "bottom_value"),
                (PlotColorRole::TopColor, "top_color"),
                (PlotColorRole::BottomColor, "bottom_color"),
            ],
            DisplayKind::Fill => &[(PlotColorRole::Color, "color")],
        };
        let rendering_colors: HashMap<PlotColorRole, HirExpr> = color_names
            .iter()
            .filter_map(|(role, name)| rendering_arguments.remove(name).map(|value| (*role, value)))
            .collect();
        let colors: Vec<(PlotColorRole, HirExpr)> = color_names
            .iter()
            .filter_map(|(role, name)| {
                take(name)
                    .or_else(|| {
                        (kind == DisplayKind::Fill && gradient).then_some(HirExpr {
                            kind: HirExprKind::ConstNa,
                            ty: if matches!(
                                role,
                                PlotColorRole::TopValue | PlotColorRole::BottomValue
                            ) {
                                Type::Float
                            } else {
                                Type::Color
                            },
                            qualifier: Qualifier::Const,
                            span,
                        })
                    })
                    .map(|color| {
                        (
                            *role,
                            if matches!(role, PlotColorRole::TopValue | PlotColorRole::BottomValue)
                            {
                                Self::coerce_int_to_float(color)
                            } else {
                                color
                            },
                        )
                    })
            })
            .collect();
        let series_colors = colors
            .iter()
            .filter(|(role, color)| {
                !matches!(role, PlotColorRole::TopValue | PlotColorRole::BottomValue)
                    && color.qualifier == Qualifier::Series
            })
            .count() as u32;
        let plot_counts = match kind {
            DisplayKind::PlotShape | DisplayKind::PlotChar | DisplayKind::PlotArrow => {
                1 + series_colors
            }
            DisplayKind::PlotCandle | DisplayKind::PlotBar => 4 + series_colors,
            DisplayKind::BgColor | DisplayKind::BarColor => 1,
            DisplayKind::Fill => u32::from(series_colors > 0),
            DisplayKind::Hline => 0,
        };
        self.add_plot_counts(plot_counts, span);

        // Build the surfaced data plots. Kinds with no data plot (color/id only)
        // fall through with an empty `data` and produce nothing.
        let mut plots: Vec<HirStmt> = Vec::with_capacity(min_data);
        match kind {
            DisplayKind::PlotShape | DisplayKind::PlotChar => {
                // Preserve the original numeric `series` payload even though
                // zero suppresses the on-chart marker. Boolean inputs are emitted
                // as 1.0/0.0 values; false must not become a missing value.
                let data_hir = data.into_iter().next().expect("one data arg");
                let value = if data_hir.ty == Type::Bool {
                    HirExpr {
                        kind: HirExprKind::Ternary {
                            cond: Box::new(data_hir),
                            then_e: Box::new(HirExpr {
                                kind: HirExprKind::ConstFloat(1.0),
                                ty: Type::Float,
                                qualifier: Qualifier::Const,
                                span,
                            }),
                            else_e: Box::new(HirExpr {
                                kind: HirExprKind::ConstFloat(0.0),
                                ty: Type::Float,
                                qualifier: Qualifier::Const,
                                span,
                            }),
                        },
                        ty: Type::Float,
                        qualifier: Qualifier::Series,
                        span,
                    }
                } else {
                    Self::coerce_int_to_float(data_hir)
                };
                let label = title.unwrap_or_else(|| kind.name().to_string());
                plots.push(self.emit_data_plot(
                    label,
                    value,
                    style,
                    colors,
                    &rendering_colors,
                    span,
                ));
            }
            DisplayKind::PlotArrow | DisplayKind::Hline => {
                let value =
                    Self::coerce_int_to_float(data.into_iter().next().expect("one data arg"));
                let label = title.unwrap_or_else(|| kind.name().to_string());
                plots.push(self.emit_data_plot(
                    label,
                    value,
                    style,
                    colors,
                    &rendering_colors,
                    span,
                ));
            }
            DisplayKind::PlotCandle | DisplayKind::PlotBar => {
                let base = title.unwrap_or_else(|| kind.name().to_string());
                style.group = Some(self.plots.len() as u32);
                let (data, colors) = self.capture_ohlc_arguments(data, colors);
                for (val, (field_name, field)) in data.into_iter().zip([
                    ("open", OhlcField::Open),
                    ("high", OhlcField::High),
                    ("low", OhlcField::Low),
                    ("close", OhlcField::Close),
                ]) {
                    let val = Self::coerce_int_to_float(val);
                    let mut field_style = style.clone();
                    field_style.ohlc_field = Some(field);
                    plots.push(self.emit_data_plot(
                        format!("{base}.{field_name}"),
                        val,
                        field_style,
                        colors.clone(),
                        &rendering_colors,
                        span,
                    ));
                }
            }
            DisplayKind::BgColor | DisplayKind::BarColor => {
                let slot = VisualSlot(self.visuals.len() as u32);
                let color_roles = colors.iter().map(|(role, _)| *role).collect();
                self.visuals.push(VisualInfo {
                    slot,
                    title,
                    style,
                    color_roles,
                    span,
                });
                return Some(HirStmt::Visual { slot, colors, span });
            }
            DisplayKind::Fill => {
                if let Some((plot1, plot2)) = fill_endpoints {
                    let slot = FillSlot(self.fills.len() as u32);
                    let color_roles = colors.iter().map(|(role, _)| *role).collect();
                    // Gradient bounds are paint channels too: fixed colors with
                    // series-qualified bounds still need per-bar registration.
                    style.fill_color_mode = Some(
                        if colors
                            .iter()
                            .any(|(_, expression)| expression.qualifier == Qualifier::Series)
                        {
                            pinewasm_hir::FillColorMode::Series
                        } else {
                            pinewasm_hir::FillColorMode::Fixed
                        },
                    );
                    self.fills.push(FillInfo {
                        slot,
                        plot1,
                        plot2,
                        title,
                        style,
                        color_roles,
                        span,
                    });
                    return Some(HirStmt::Fill { slot, colors, span });
                }
            }
        }

        // Emit all but the last plot into the pending buffer; return the last.
        // Kinds with no data plot return None (a no-op after type-checking).
        let last = plots.pop();
        self.pending_stmts.extend(plots);
        last
    }

    /// `plotbar`/`plotcandle` surface one call as four OHLC plots, and each
    /// plot statement carries every color argument. Each call argument is
    /// evaluated once per bar, so a color that changes state when it runs
    /// (`array.shift(colors)`, a `ta.*` call, `math.random`) must not run once
    /// per channel. Such a color is evaluated once into a hidden slot that all
    /// four channels read. When any color is captured, data arguments that are
    /// not safe to repeat are captured too, and every capture runs in
    /// parameter order (`open`, `high`, `low`, `close`, then the colors in
    /// signature order), never in the order the arguments are written. Named
    /// arguments are bound and evaluated in parameter order, so a fully
    /// reordered named call computes the same values as the positional call (see
    /// `table_cell_reordered_named_arguments_evaluate_in_parameter_order` in
    /// `pinewasm_vm`). `data` and `colors` arrive in that order from the
    /// binder, so the captures keep it. Colors that are safe to repeat
    /// (literals, reads, pure color arithmetic) stay inline, so their static
    /// rendering metadata is unchanged.
    ///
    /// An argument that failed to type-check has already reported its error,
    /// so nothing is captured (as `eval_switch_subject` does for an
    /// error-typed subject).
    fn capture_ohlc_arguments(
        &mut self,
        mut data: Vec<HirExpr>,
        mut colors: Vec<(PlotColorRole, HirExpr)>,
    ) -> (Vec<HirExpr>, Vec<(PlotColorRole, HirExpr)>) {
        let any_error = data
            .iter()
            .chain(colors.iter().map(|(_, color)| color))
            .any(|expr| expr.ty.is_error());
        if any_error || colors.iter().all(|(_, color)| repeatable(color)) {
            return (data, colors);
        }
        // Bound parameter order; never sort by span.
        let targets = data
            .iter_mut()
            .chain(colors.iter_mut().map(|(_, color)| color))
            .filter(|expr| !repeatable(expr));
        for target in targets {
            let span = target.span;
            let ty = target.ty.clone();
            let qualifier = target.qualifier;
            let slot = self.allocate_synthetic_slot(ty.clone(), span);
            self.symbols[slot.0 as usize].qualifier = qualifier;
            let read = HirExpr {
                kind: HirExprKind::ReadVar {
                    symbol: SymbolId(slot.0),
                    offset: 0,
                },
                ty,
                qualifier,
                span,
            };
            let value = std::mem::replace(target, read);
            self.pending_stmts.push(HirStmt::Assign {
                symbol: SymbolId(slot.0),
                value,
                init: false,
                span,
            });
        }
        (data, colors)
    }

    /// Type-check one display-plot-family argument against its parameter and
    /// return the checked HIR. Data args (`is_data`) require the parameter's
    /// data type (bool for shape/char `series`; numeric elsewhere); other args
    /// are checked against their conventional type and, on mismatch, produce a
    /// non-fatal `E0205` (the call still compiles and runs, and the display
    /// surface stays lenient as far as the headless model allows).
    pub(crate) fn check_display_arg(
        &mut self,
        kind: DisplayKind,
        pname: &str,
        is_data: bool,
        arg: &ast::Expr,
    ) -> HirExpr {
        if is_data {
            // Data-argument typing per kind.
            let is_bool_series = matches!(kind, DisplayKind::PlotShape | DisplayKind::PlotChar);
            if is_bool_series {
                // `plotshape`/`plotchar` accept a `series bool` or a `series
                // int/float`: a shape is plotted where the value is not `na`
                // and not `0`/`false` (so a conditional such as `cond ? price : na`
                // draws only where its condition holds). Analyze without a bool
                // expectation so a numeric data series types naturally; the
                // lowering derives presence.
                let hir = self.analyze_expr(arg);
                if !hir.ty.is_error() && hir.ty != Type::Bool && !is_numeric(&hir.ty) {
                    self.error(
                        codes::E0205,
                        format!(
                            "`{}` series must be a bool or number, found {}",
                            kind.name(),
                            hir.ty.describe()
                        ),
                        arg.span(),
                        "expected a series bool or series int/float",
                    );
                }
                return hir;
            }
            let hir = self.analyze_expr_expecting(arg, Some(&Type::Float));
            if !hir.ty.is_error() && !is_numeric(&hir.ty) {
                self.error(
                    codes::E0205,
                    format!(
                        "`{}` argument `{}` must be a number, found {}",
                        kind.name(),
                        pname,
                        hir.ty.describe()
                    ),
                    arg.span(),
                    "expected a series int/float",
                );
            }
            return hir;
        }
        // Non-data arguments follow the declared types; only int-to-float
        // widening is implicit.
        let expected: Option<Type> = match pname {
            "color" | "colorup" | "colordown" | "textcolor" | "wickcolor" | "bordercolor"
            | "top_color" | "bottom_color" => Some(Type::Color),
            "title" | "text" | "style" | "char" | "location" | "size" | "format" | "linestyle" => {
                Some(Type::String)
            }
            "display" => Some(crate::registry::display_destination_type(kind)),
            "editable" | "force_overlay" | "fillgaps" => Some(Type::Bool),
            // `show_last` is a *count* of trailing bars, not a flag.
            "offset" | "precision" | "linewidth" | "minheight" | "maxheight" | "show_last" => {
                Some(Type::Int)
            }
            "hline1" | "hline2" => Some(Type::Hline),
            "plot1" | "plot2" => Some(Type::Plot),
            "top_value" | "bottom_value" => Some(Type::Float),
            _ => None,
        };
        let hir = self.analyze_expr_expecting(arg, expected.as_ref());
        if pname == "display"
            && matches!(
                kind,
                DisplayKind::Hline
                    | DisplayKind::BgColor
                    | DisplayKind::BarColor
                    | DisplayKind::Fill
            )
            && (self.default_display_mask(&hir).is_some_and(|mask| {
                mask != pinewasm_hir::display_set::ALL_MASK
                    && mask & pinewasm_hir::display_set::PINE_SCREENER_MASK != 0
            }) || self
                .input_active_expr_from_hir(&hir)
                .as_ref()
                .is_some_and(has_explicit_pine_screener))
        {
            self.error(
                codes::E0205,
                format!(
                    "`{}` does not accept a screener display destination",
                    kind.name()
                ),
                arg.span(),
                "use display.all or display.none",
            );
        }
        if let Some(exp) = &expected {
            let ok = hir.ty.is_error()
                || hir.ty == *exp
                || (*exp == Type::PlotDisplay && hir.ty == Type::PlotSimpleDisplay)
                || (*exp == Type::Float && hir.ty == Type::Int);
            if !ok {
                self.error(
                    codes::E0205,
                    format!(
                        "`{}` argument `{}` must be {}, found {}",
                        kind.name(),
                        pname,
                        exp.describe(),
                        hir.ty.describe()
                    ),
                    arg.span(),
                    format!("expected {}", exp.describe()),
                );
            }
        }
        hir
    }

    /// Analyze `runtime.error(message)` and desugar it to a
    /// [`HirStmt::RuntimeError`]. `message` is a `series string`; an `na`
    /// message is accepted (renders as empty text). Allowed anywhere a
    /// statement is (including `if` branches), so a conditional call aborts
    /// only on the bar/branch where it runs.
    pub(crate) fn analyze_runtime_error_call(
        &mut self,
        args: &[ast::CallArg],
        span: Span,
    ) -> Option<HirStmt> {
        let bound = self.bind_positional("runtime.error", &["message"], args, span)?;
        let args = bound.as_slice();
        if args.len() != 1 {
            self.error(
                codes::E0204,
                format!("`runtime.error` expects 1 argument, found {}", args.len()),
                span,
                "expected (message)",
            );
            for a in args {
                let _ = self.analyze_expr(a);
            }
            return None;
        }
        let message = self.analyze_expr_expecting(&args[0], Some(&Type::String));
        if !message.ty.is_error() && message.ty != Type::String {
            self.error(
                codes::E0205,
                format!(
                    "`runtime.error` message must be a string, found {}",
                    message.ty.describe()
                ),
                args[0].span(),
                "expected a string message",
            );
        }
        Some(HirStmt::RuntimeError { message, span })
    }

    /// Analyze a `strategy.*` order primitive and desugar it to a
    /// [`HirStmt::Order`] with the kind's fixed operand slot layout. Only valid
    /// in a `strategy()` script (`E0241`). Unlike declaration and plot-family
    /// calls, order commands are valid in local scopes and user-defined
    /// functions; function expansion carries their effects to each written
    /// call site.
    ///
    /// Slot layouts (missing optionals become `na`):
    /// * Entry / Order: 10 slots, as listed in [`OrderKind::param_names`].
    /// * Exit: 21 slots (trailing, OCA, comments, and alerts included), as
    ///   listed in [`OrderKind::param_names`].
    /// * Close: 7 slots.
    /// * `CloseAll`: 4 slots.
    /// * Cancel: `[id]`
    /// * `CancelAll`: `[]`
    pub(crate) fn analyze_order_call(
        &mut self,
        kind: OrderKind,
        callee: &ast::Ident,
        args: &[ast::CallArg],
        span: Span,
    ) -> Option<HirStmt> {
        // Order primitives require a `strategy()` declaration. The
        // strategy/indicator declaration is analyzed before statement bodies,
        // so `self.strategy` is already known here.
        if self.strategy.is_none() {
            self.error(
                codes::E0241,
                format!(
                    "`strategy.{}(...)` requires a `strategy(...)` declaration",
                    kind.member()
                ),
                span,
                "add a `strategy(\"...\")` declaration, or use `indicator(...)` without order calls",
            );
            for arg in args {
                let _ = self.analyze_expr(&arg.value);
            }
            return None;
        }

        let member = kind.member();
        let param_names = kind.param_names();
        let required = kind.required_count();
        // Bind arguments to named slots (sparse: named args may skip optional
        // slots, which then stay `None` and become a typed `na` operand).
        let (bound, overflow) = self.bind_call_args(callee.name.as_str(), param_names, args)?;
        if !overflow.is_empty() {
            self.error(
                codes::E0204,
                format!(
                    "`strategy.{member}` expects at most {} argument(s)",
                    param_names.len()
                ),
                span,
                "too many positional arguments",
            );
            for a in overflow {
                let _ = self.analyze_expr(a);
            }
            return None;
        }
        // Every required (leading) slot must be supplied.
        for (i, name) in param_names.iter().enumerate().take(required) {
            if bound[i].is_none() {
                self.error(
                    codes::E0204,
                    format!("`strategy.{member}` is missing a value for parameter `{name}`"),
                    span,
                    "this parameter is required",
                );
                return None;
            }
        }

        // Build one operand HirExpr per slot. `String`/`Float` helpers coerce
        // and type-check; a missing optional slot becomes a typed `na`.
        let na_float = |span: Span| HirExpr {
            kind: HirExprKind::ConstNa,
            ty: Type::Float,
            qualifier: Qualifier::Const,
            span,
        };
        let const_string = |value: &str, span: Span| HirExpr {
            kind: HirExprKind::ConstString(value.to_string()),
            ty: Type::String,
            qualifier: Qualifier::Const,
            span,
        };
        let const_float = |value: f64, span: Span| HirExpr {
            kind: HirExprKind::ConstFloat(value),
            ty: Type::Float,
            qualifier: Qualifier::Const,
            span,
        };
        let const_bool = |value: bool, span: Span| HirExpr {
            kind: HirExprKind::ConstBool(value),
            ty: Type::Bool,
            qualifier: Qualifier::Const,
            span,
        };

        let mut operands: Vec<HirExpr> = Vec::with_capacity(param_names.len());
        for (i, name) in param_names.iter().enumerate() {
            let arg = bound[i];
            let operand = match *name {
                "id" | "from_entry" | "oca_name" | "comment" | "comment_profit"
                | "comment_loss" | "comment_trailing" | "alert_message" | "alert_profit"
                | "alert_loss" | "alert_trailing" => match arg {
                    None => const_string("", span),
                    Some(a) => {
                        let h = self.analyze_expr_expecting(a, Some(&Type::String));
                        if !h.ty.is_error() && h.ty != Type::String {
                            self.error(
                                codes::E0205,
                                format!(
                                    "`strategy.{member}` {name} must be a string, found {}",
                                    h.ty.describe()
                                ),
                                a.span(),
                                "expected a string",
                            );
                        }
                        h
                    }
                },
                "oca_type" => match arg {
                    None => const_string("strategy.oca.none", span),
                    Some(a) => {
                        let h = self.analyze_expr_expecting(a, Some(&Type::String));
                        self.defer_temporal_fixedness_at(
                            &h,
                            &format!("strategy.{member}"),
                            "oca_type",
                            a.span(),
                        );
                        if !h.ty.is_error() && h.ty != Type::String {
                            self.error(
                                codes::E0205,
                                format!(
                                    "`strategy.{member}` oca_type must be a string, found {}",
                                    h.ty.describe()
                                ),
                                a.span(),
                                "expected a string",
                            );
                        } else if !h.ty.is_error() && h.qualifier > Qualifier::Input {
                            self.error(
                                codes::E0206,
                                format!(
                                    "`strategy.{member}` oca_type must be input or weaker, found {}",
                                    h.qualifier.describe()
                                ),
                                a.span(),
                                "expected an input string",
                            );
                        }
                        h
                    }
                },
                "direction" => match arg {
                    None => const_string("", span),
                    Some(a) => self.analyze_order_direction(member, a),
                },
                "value" if kind == OrderKind::RiskAllowEntryIn => match arg {
                    None => const_string("", span),
                    Some(a) => self.analyze_risk_entry_direction(member, a),
                },
                "type"
                    if matches!(
                        kind,
                        OrderKind::RiskMaxDrawdown | OrderKind::RiskMaxIntradayLoss
                    ) =>
                {
                    match arg {
                        None => HirExpr {
                            kind: HirExprKind::ConstString(
                                "strategy.percent_of_equity".to_string(),
                            ),
                            ty: Type::String,
                            qualifier: Qualifier::Const,
                            span,
                        },
                        Some(a) => self.analyze_risk_drawdown_type(member, a),
                    }
                }
                "count"
                    if matches!(
                        kind,
                        OrderKind::RiskMaxIntradayFilledOrders | OrderKind::RiskMaxConsLossDays
                    ) =>
                {
                    match arg {
                        None => HirExpr {
                            kind: HirExprKind::ConstNa,
                            ty: Type::Int,
                            qualifier: Qualifier::Const,
                            span,
                        },
                        Some(a) => {
                            let h = self.analyze_expr_expecting(a, Some(&Type::Int));
                            if !h.ty.is_error() && h.ty != Type::Int {
                                self.error(
                                    codes::E0205,
                                    format!(
                                        "`strategy.{member}` count must be an int, found {}",
                                        h.ty.describe()
                                    ),
                                    a.span(),
                                    "expected an int",
                                );
                            }
                            h
                        }
                    }
                }
                "immediately" | "disable_alert" => match arg {
                    None => const_bool(false, span),
                    Some(a) => {
                        let h = self.analyze_expr_expecting(a, Some(&Type::Bool));
                        if matches!(&h.kind, HirExprKind::ConstNa) {
                            self.error(
                                codes::E0205,
                                format!(
                                    "`strategy.{member}` {name} must be true or false; bool cannot be na"
                                ),
                                a.span(),
                                "use true or false",
                            );
                            HirExpr {
                                ty: Type::Error,
                                ..h
                            }
                        } else {
                            if !h.ty.is_error() && h.ty != Type::Bool {
                                self.error(
                                    codes::E0205,
                                    format!(
                                        "`strategy.{member}` {name} must be a bool, found {}",
                                        h.ty.describe()
                                    ),
                                    a.span(),
                                    "expected a bool",
                                );
                            }
                            h
                        }
                    }
                },
                // Every remaining slot is a numeric price/quantity.
                _ => match arg {
                    None if *name == "qty_percent" => const_float(100.0, span),
                    None => na_float(span),
                    Some(a) => {
                        let h = self.analyze_expr_expecting(a, Some(&Type::Float));
                        let h = Self::coerce_int_to_float(h);
                        if !h.ty.is_error() && h.ty != Type::Float {
                            self.error(
                                codes::E0205,
                                format!(
                                    "`strategy.{member}` {name} must be a number, found {}",
                                    h.ty.describe()
                                ),
                                a.span(),
                                "expected an int or float",
                            );
                        }
                        h
                    }
                },
            };
            operands.push(operand);
        }

        // Every strategy.risk.* parameter is declared `simple` (or weaker).
        // This is also the semantic invariant that makes global behavior
        // deterministic: MIR can evaluate the
        // directive in its unconditional prelude without changing a
        // bar-varying argument's meaning. Keep the check here, beside the
        // dedicated order binding, so all six risk commands share it.
        if kind.is_risk() {
            let mut qualifiers_ok = true;
            for (index, (name, operand)) in param_names.iter().zip(&operands).enumerate() {
                self.defer_temporal_fixedness_at(
                    operand,
                    &format!("strategy.{member}"),
                    name,
                    bound[index].map_or(operand.span, ast::Expr::span),
                );
                if !operand.ty.is_error() && operand.qualifier > Qualifier::Simple {
                    self.error(
                        codes::E0206,
                        format!(
                            "`strategy.{member}` argument `{name}` must be simple or weaker, found {}",
                            operand.qualifier.describe()
                        ),
                        operand.span,
                        "expected a simple value",
                    );
                    qualifiers_ok = false;
                }
            }
            if !qualifiers_ok {
                return None;
            }
        }

        // Both loss thresholds are non-negative. In
        // percent-of-equity mode the inclusive domain is 0..=100; cash has no
        // corresponding upper bound. Reject a fully-known invalid pair here,
        // while input/simple expressions are validated by the VM once their
        // host overrides have been applied.
        if matches!(
            kind,
            OrderKind::RiskMaxDrawdown | OrderKind::RiskMaxIntradayLoss
        ) {
            let value = const_f64_hir(&operands[0]);
            let unit = const_string_hir(&operands[1]);
            if let Some(value) = value {
                let invalid = !value.is_finite()
                    || value < 0.0
                    || (unit.as_deref() == Some("strategy.percent_of_equity") && value > 100.0);
                if invalid {
                    let requirement = if unit.as_deref() == Some("strategy.percent_of_equity") {
                        "between 0 and 100 inclusive"
                    } else {
                        "a finite non-negative number"
                    };
                    self.error(
                        codes::E0205,
                        format!("`strategy.{member}` value must be {requirement}, found {value}"),
                        operands[0].span,
                        format!("expected {requirement}"),
                    );
                    return None;
                }
            }
        }

        if kind == OrderKind::Exit {
            let supplied = |name: &str| {
                param_names
                    .iter()
                    .position(|candidate| *candidate == name)
                    .is_some_and(|index| bound[index].is_some())
            };
            let fixed_exit = ["profit", "limit", "loss", "stop"]
                .iter()
                .any(|name| supplied(name));
            let trailing_exit =
                supplied("trail_offset") && (supplied("trail_price") || supplied("trail_points"));
            if !fixed_exit && !trailing_exit {
                self.error(
                    codes::E0219,
                    "`strategy.exit` must specify profit, limit, loss, stop, or a complete trailing-stop pair",
                    span,
                    "add profit/limit/loss/stop, or trail_offset with trail_price/trail_points",
                );
            }
        }

        Some(HirStmt::Order {
            kind,
            operands,
            span,
        })
    }

    /// Analyze the `series strategy_direction` argument.
    ///
    /// The two built-in constants are string-backed nominal enum values. We
    /// deliberately retain the expression instead of folding it to a sign:
    /// a series conditional or variable can select between `strategy.long`
    /// and `strategy.short` on each execution.
    pub(crate) fn analyze_order_direction(&mut self, member: &str, arg: &ast::Expr) -> HirExpr {
        let expected = Type::Enum(pinewasm_hir::STRATEGY_DIRECTION_TYPE.to_string());
        let h = self.analyze_expr_expecting(arg, Some(&expected));
        let span = arg.span();
        if h.ty.is_error() {
            return h;
        }
        if matches!(&h.kind, HirExprKind::ConstNa) {
            self.error(
                codes::E0205,
                format!("`strategy.{member}` direction cannot be na"),
                span,
                "use strategy.long or strategy.short",
            );
            return HirExpr {
                ty: Type::Error,
                ..h
            };
        }
        if h.ty == expected {
            return h;
        }
        self.error(
            codes::E0205,
            format!(
                "`strategy.{member}` direction must be `strategy.long` or `strategy.short`, found {}",
                h.ty.describe()
            ),
            span,
            "expected a strategy_direction value",
        );
        HirExpr {
            kind: HirExprKind::ConstString(String::new()),
            ty: Type::Error,
            qualifier: h.qualifier,
            span,
        }
    }

    /// Analyze `strategy.risk.allow_entry_in(value)`: it accepts only the
    /// `strategy.direction.*` entry-direction tokens.
    pub(crate) fn analyze_risk_entry_direction(
        &mut self,
        member: &str,
        arg: &ast::Expr,
    ) -> HirExpr {
        let h = self.analyze_expr_expecting(arg, Some(&Type::String));
        let span = arg.span();
        if h.ty.is_error() {
            return h;
        }
        if h.ty != Type::String {
            self.error(
                codes::E0205,
                format!(
                    "`strategy.{member}` value must be a string, found {}",
                    h.ty.describe()
                ),
                span,
                "expected a string",
            );
            return HirExpr {
                kind: HirExprKind::ConstNa,
                ty: Type::Error,
                qualifier: h.qualifier,
                span,
            };
        }

        // Preserve compile-time typo detection for a literal token. Other
        // simple strings (for example, an input-controlled ternary) are valid
        // Pine arguments and are checked against the same token set by the VM
        // after the run's input overrides have been applied.
        if let HirExprKind::ConstString(value) = &h.kind {
            if matches!(
                value.as_str(),
                "strategy.direction.long" | "strategy.direction.short" | "strategy.direction.all"
            ) {
                return h;
            }
            self.error(
                codes::E0205,
                format!(
                    "`strategy.{member}` value must be `strategy.direction.long`, `strategy.direction.short`, or `strategy.direction.all`"
                ),
                span,
                "expected a strategy.direction.* token",
            );
            return HirExpr {
                kind: HirExprKind::ConstNa,
                ty: Type::Error,
                qualifier: h.qualifier,
                span,
            };
        }

        h
    }

    /// Analyze `strategy.risk.max_drawdown(..., type)`: the risk unit is the
    /// strategy quantity-type token `strategy.percent_of_equity` or
    /// `strategy.cash`. These constants are simple strings, so a
    /// nonliteral simple-string expression is accepted and validated by the VM.
    pub(crate) fn analyze_risk_drawdown_type(&mut self, member: &str, arg: &ast::Expr) -> HirExpr {
        let h = self.analyze_expr_expecting(arg, Some(&Type::String));
        let span = arg.span();
        if h.ty.is_error() {
            return h;
        }
        if h.ty != Type::String {
            self.error(
                codes::E0205,
                format!(
                    "`strategy.{member}` type must be a string, found {}",
                    h.ty.describe()
                ),
                span,
                "expected a string",
            );
            return HirExpr {
                kind: HirExprKind::ConstNa,
                ty: Type::Error,
                qualifier: h.qualifier,
                span,
            };
        }

        if let HirExprKind::ConstString(value) = &h.kind {
            if matches!(
                value.as_str(),
                "strategy.percent_of_equity" | "strategy.cash"
            ) {
                return h;
            }
            self.error(
                codes::E0205,
                format!(
                    "`strategy.{member}` type must be `strategy.percent_of_equity` or `strategy.cash`"
                ),
                span,
                "expected a strategy risk unit token",
            );
            return HirExpr {
                kind: HirExprKind::ConstNa,
                ty: Type::Error,
                qualifier: h.qualifier,
                span,
            };
        }

        h
    }

    /// Check argument count, types, and qualifiers against a function
    /// signature. Returns HIR for the required arguments on success.
    pub(crate) fn check_call_args(
        &mut self,
        func: &BuiltinFunction,
        args: &[ast::Expr],
        callee_span: Span,
        call_span: Span,
    ) -> Option<Vec<HirExpr>> {
        let required = func.params.iter().filter(|p| p.required).count();
        if args.len() < required || args.len() > func.params.len() {
            let expected = if required == func.params.len() {
                format!("{required}")
            } else {
                format!("{required} to {}", func.params.len())
            };
            self.error(
                codes::E0204,
                format!(
                    "`{}` expects {expected} argument(s), found {}",
                    func.name,
                    args.len()
                ),
                if args.is_empty() {
                    callee_span
                } else {
                    call_span
                },
                format!("expected {expected} argument(s)"),
            );
            return None;
        }

        let mut checked = Vec::new();
        let mut ok = true;
        for (arg, param) in args.iter().zip(&func.params) {
            // The declared parameter type doubles as the expected type an
            // `na` literal adopts in this position.
            let expected = param.expected_type();
            let mut hir = self.analyze_expr_expecting(arg, Some(&expected));
            if hir.ty.is_error() {
                ok = false;
            } else {
                if hir.ty != param.ty {
                    // Float parameters accept int arguments through an
                    // explicit coercion (e.g. `plot(1)`).
                    if param.ty == Type::Float && hir.ty == Type::Int {
                        hir = Self::coerce_int_to_float(hir);
                    } else {
                        self.error(
                            codes::E0205,
                            format!(
                                "`{}` argument `{}` must be {}, found {}",
                                func.name,
                                param.name,
                                param.ty.describe(),
                                hir.ty.describe()
                            ),
                            arg.span(),
                            format!("expected {}", param.ty.describe()),
                        );
                        ok = false;
                    }
                }
                if param.max_qualifier < Qualifier::Series {
                    self.defer_temporal_fixedness_at(&hir, func.name, param.name, arg.span());
                }
                if hir.qualifier > param.max_qualifier {
                    self.error(
                        codes::E0206,
                        format!(
                            "`{}` argument `{}` must be {} or weaker, found {}",
                            func.name,
                            param.name,
                            param.max_qualifier.describe(),
                            hir.qualifier.describe()
                        ),
                        arg.span(),
                        format!("expected a {} value", param.max_qualifier.describe()),
                    );
                    ok = false;
                }
            }
            checked.push(hir);
        }
        ok.then_some(checked)
    }
}

/// True when evaluating `expr` more than once per bar gives the same value
/// and changes no state: literals, variable/builtin/input reads, operators,
/// field reads and stateless builtins. Anything else (collection calls such
/// as `array.shift`, `ta.*` call sites, `math.random`, history
/// materialization, requests, drawings) is not repeatable.
///
/// `capture_ohlc_arguments` leaves a repeatable color inline in all four OHLC
/// plot statements, after the captured siblings have run. That is correct only
/// while two things hold:
/// - an inlined user function's body statements are hoisted into the pending
///   statements, so they run once, before every plot statement, and only the
///   function's result expression reaches this check;
/// - nothing listed above as repeatable reads collection, `ta.*`, random or
///   request state, so a repeated read sees the same value on every channel.
///
/// Change the hoisting or widen this list only with a test in
/// `crates/pinewasm_compiler/tests/v6_ohlc_color_eval_once.rs` that shows all
/// four channels still agree. (A user function's hoisted body would also run
/// before an earlier argument with a side effect; `arg_order.rs` captures such
/// an argument first, for every call, so the arguments reaching this check
/// already run in parameter order.)
fn repeatable(expr: &HirExpr) -> bool {
    match &expr.kind {
        HirExprKind::ConstInt(_)
        | HirExprKind::ConstFloat(_)
        | HirExprKind::ConstBool(_)
        | HirExprKind::ConstString(_)
        | HirExprKind::ConstColor { .. }
        | HirExprKind::ConstNa
        | HirExprKind::ReadBuiltin { .. }
        | HirExprKind::ReadVar { .. }
        | HirExprKind::ReadInput { .. } => true,
        HirExprKind::ReadBuiltinDynamic { offset, .. }
        | HirExprKind::ReadVarDynamic { offset, .. } => repeatable(offset),
        HirExprKind::Unary { operand, .. } | HirExprKind::IntToFloat(operand) => {
            repeatable(operand)
        }
        HirExprKind::GetField { obj, .. } => repeatable(obj),
        HirExprKind::Binary { lhs, rhs, .. } => repeatable(lhs) && repeatable(rhs),
        HirExprKind::Ternary {
            cond,
            then_e,
            else_e,
        } => repeatable(cond) && repeatable(then_e) && repeatable(else_e),
        HirExprKind::CallPure { func, args } => {
            *func != PureFunc::MathRandom && args.iter().all(repeatable)
        }
        _ => false,
    }
}

fn has_explicit_pine_screener(expression: &pinewasm_hir::InputActiveExpr) -> bool {
    use pinewasm_hir::{display_set, InputActiveExpr};
    match expression {
        InputActiveExpr::Ternary {
            when_true,
            when_false,
            ..
        } => has_explicit_pine_screener(when_true) || has_explicit_pine_screener(when_false),
        _ => display_set::binding_mask(expression).is_some_and(|mask| {
            mask != display_set::ALL_MASK && mask & display_set::PINE_SCREENER_MASK != 0
        }),
    }
}
