//! `ta.*` call analysis: source/length argument checking, const-argument
//! validation, the `ta.vwap` overload family, and lowering to engine-backed
//! TA state slots.

use crate::registry::TA_LENGTH_RANGE;
use crate::*;

enum TaLengthArg {
    Fixed(u32, Option<(u32, bool)>),
    Dynamic(HirExpr),
}

impl<'r> Analyzer<'r> {
    /// Track only the integer forms that TA length positions support.
    pub(crate) fn ast_ta_input(&self, expr: &ast::Expr) -> Option<(u32, bool)> {
        match expr {
            ast::Expr::Group { inner, .. } => self.ast_ta_input(inner),
            ast::Expr::Unary {
                op: ast::UnaryOp::Neg,
                operand,
                ..
            } => self
                .ast_ta_input(operand)
                .map(|(input, negated)| (input, !negated)),
            ast::Expr::Ident(name) => self
                .ta_int_inputs
                .get(&self.lookup_variable(&name.name)?)
                .copied(),
            _ => None,
        }
    }

    /// Whether a length expression reads a name declared `series`. Such a
    /// length can still be known at compile time (a helper parameter bound to
    /// a constant argument), but it is not a `simple int`.
    fn ast_reads_series_name(&self, expr: &ast::Expr) -> bool {
        match expr {
            ast::Expr::Group { inner, .. } => self.ast_reads_series_name(inner),
            ast::Expr::Unary { operand, .. } => self.ast_reads_series_name(operand),
            ast::Expr::Binary { lhs, rhs, .. } => {
                self.ast_reads_series_name(lhs) || self.ast_reads_series_name(rhs)
            }
            ast::Expr::Ident(name) => self.lookup_variable(&name.name).is_some_and(|symbol| {
                self.symbols[symbol.0 as usize].qualifier > Qualifier::Simple
            }),
            _ => false,
        }
    }

    /// `W0010`: a compile-time argument is calculated from an input, so the
    /// call keeps the value the input defaults give. One warning for each
    /// argument and value: every call of an inlined helper analyzes its body
    /// again.
    fn warn_fixed_at_input_default(
        &mut self,
        func_name: &str,
        param_name: &str,
        value: i64,
        span: Span,
    ) {
        if !self.input_default_warnings.insert((span, value)) {
            return;
        }
        self.warning(
            codes::W0010,
            format!(
                "`{func_name}` argument `{param_name}` is calculated from an input, so this call keeps the value the input defaults give ({value})"
            ),
            span,
            "a selected input value does not change this argument; pass the input itself to follow it",
        );
    }

    /// Report each `ta.ema` length that a `request.*()` expression keeps at
    /// the value the input defaults give (`W0010`).
    pub(crate) fn report_request_default_lengths(&mut self) {
        let mut sites = std::mem::take(&mut *self.request_default_ema_lengths.borrow_mut());
        sites.sort_by_key(|(span, value)| (span.source, span.start, span.end, *value));
        sites.dedup();
        for (span, value) in sites {
            self.warning(
                codes::W0010,
                format!(
                    "`ta.ema` argument `length` is calculated from an input, so inside a `request.*()` call it keeps the value the input defaults give ({value})"
                ),
                span,
                "outside the request this length follows the selected input",
            );
        }
    }

    pub(crate) fn selected_ta_int_input(&self, expr: &HirExpr) -> Option<u32> {
        if expr.ty != Type::Int || expr.qualifier > Qualifier::Simple {
            return None;
        }
        let input = self.selected_input_for_expr(expr)?;
        (self.inputs.get(input as usize)?.kind == InputKind::Int).then_some(input)
    }

    /// A stateful `ta.*` call: check the source and length arguments, then
    /// materialize the call site as [`HirExprKind::TaCall`] over its own
    /// hidden series slot.
    pub(crate) fn analyze_ta_call(
        &mut self,
        func: &BuiltinFunction,
        ta: TaFunc,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        let param_names: Vec<&str> = func.params.iter().map(|p| p.name).collect();
        let bound = match self.bind_positional(func.name, &param_names, args, span) {
            Some(a) => a,
            None => return self.error_expr(span),
        };
        let args = bound.as_slice();
        // A stateful ta call inside a loop body is a strict compile error
        // (E0222): its per-call-site state would advance multiple times per
        // bar. Still analyze the arguments so their own errors surface.
        if self.reject_stateful_in_loop(&format!("stateful `{}`", func.name), span) {
            for arg in args {
                let _ = self.analyze_expr(arg);
            }
            return self.error_expr(span);
        }

        // Argument count (E0204), same rule as the other builtins.
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
                span,
                format!("expected {expected} argument(s)"),
            );
            for arg in args {
                let _ = self.analyze_expr(arg);
            }
            return self.error_expr(span);
        }

        // Source: series numeric; ints are coerced to float so the hidden
        // slot always stores floats (an `na` source adopts float).
        let source_param = &func.params[0];
        let source = self.analyze_expr_expecting(&args[0], Some(&Type::Float));
        if source.ty.is_error() {
            // Still validate a present length so both problems surface.
            if let Some(arg) = args.get(1) {
                let _ = self.ta_length_argument(func, &func.params[1], arg, ta == TaFunc::Sma);
            }
            return self.error_expr(span);
        }
        if !is_numeric(&source.ty) {
            self.error(
                codes::E0205,
                format!(
                    "`{}` argument `{}` must be {}, found {}",
                    func.name,
                    source_param.name,
                    source_param.ty.describe(),
                    source.ty.describe()
                ),
                args[0].span(),
                format!("expected {}", source_param.ty.describe()),
            );
            return self.error_expr(span);
        }
        if source.qualifier > source_param.max_qualifier {
            self.error(
                codes::E0206,
                format!(
                    "`{}` argument `{}` must be {} or weaker, found {}",
                    func.name,
                    source_param.name,
                    source_param.max_qualifier.describe(),
                    source.qualifier.describe()
                ),
                args[0].span(),
                format!("expected a {} value", source_param.max_qualifier.describe()),
            );
            return self.error_expr(span);
        }
        let source = Self::coerce_int_to_float(source);

        // Length: an int literal within the parameter's declared range, or
        // the declared default when omitted.
        let length_param = &func.params[1];
        let length = match args.get(1) {
            None => TaLengthArg::Fixed(
                length_param
                    .default_int
                    .expect("optional ta length declares a default") as u32,
                None,
            ),
            Some(arg) => {
                match self.ta_length_argument(func, length_param, arg, ta == TaFunc::Sma) {
                    Some(v) => v,
                    None => return self.error_expr(span),
                }
            }
        };

        self.warn_if_not_executed_every_bar(
            &format!("`{}` call", func.name),
            "this call's series state",
            span,
        );

        let slot = self.allocate_synthetic_slot(Type::Float, span);
        if let TaLengthArg::Fixed(_, direct_input) = &length {
            if let Some((input, negated)) =
                (*direct_input).or_else(|| args.get(1).and_then(|arg| self.ast_ta_input(arg)))
            {
                self.ta_input_bindings.push(pinewasm_hir::TaInputBinding {
                    input,
                    negated,
                    target: match ta {
                        TaFunc::Sma => pinewasm_hir::TaInputTarget::Sma { slot: slot.0 },
                        TaFunc::Change => pinewasm_hir::TaInputTarget::Change { slot: slot.0 },
                        _ => unreachable!("slot-based TA function"),
                    },
                });
            }
        }
        HirExpr {
            kind: match length {
                TaLengthArg::Fixed(length, _) => HirExprKind::TaCall {
                    func: ta,
                    slot,
                    source: Box::new(source),
                    length,
                },
                TaLengthArg::Dynamic(length) => HirExprKind::TaCallDynamic {
                    func: ta,
                    slot,
                    source: Box::new(source),
                    length: Box::new(length),
                },
            },
            ty: func.ret.clone(),
            qualifier: Qualifier::Series,
            span,
        }
    }

    /// Check a `ta.*` length argument: it must be an int literal or a direct
    /// scalar int input (groups and a leading unary minus are unwrapped)
    /// within the parameter's declared inclusive range. Violations are
    /// `E0219` with the exact reason; returns `None` after reporting. A direct
    /// input also returns its declaration ID so host overrides keep the same
    /// TA-state binding as an input forwarded through a variable.
    fn ta_length_argument(
        &mut self,
        func: &BuiltinFunction,
        param: &ParamSpec,
        arg: &ast::Expr,
        allow_dynamic: bool,
    ) -> Option<TaLengthArg> {
        let (min, max) = param
            .int_literal_range
            .expect("ta length parameters declare a literal range");
        let e0219 = |analyzer: &mut Self, reason: String, label: &str| {
            analyzer.error(
                codes::E0219,
                format!("invalid `{}` length: {reason}", func.name),
                arg.span(),
                label,
            );
            None
        };

        let mut expr = arg;
        while let ast::Expr::Group { inner, .. } = expr {
            expr = inner;
        }
        // A negative literal parses as unary minus around a number.
        let (negated, literal) = match expr {
            ast::Expr::Unary {
                op: ast::UnaryOp::Neg,
                operand,
                ..
            } => {
                let mut inner = operand.as_ref();
                while let ast::Expr::Group { inner: i, .. } = inner {
                    inner = i;
                }
                (true, inner)
            }
            other => (false, other),
        };
        match literal {
            ast::Expr::Int { value, .. } => {
                let value = if negated {
                    value.checked_neg()
                } else {
                    Some(*value)
                };
                match value {
                    Some(value) if value < min => e0219(
                        self,
                        format!("must be at least {min}, found {value}"),
                        "length is too small",
                    ),
                    Some(value) if value > max => e0219(
                        self,
                        format!("must be at most {max}, found {value}"),
                        "length is too large",
                    ),
                    Some(value) => Some(TaLengthArg::Fixed(value as u32, None)),
                    None => e0219(
                        self,
                        format!("must be at least {min}"),
                        "length is too small",
                    ),
                }
            }
            ast::Expr::Float { .. } => e0219(
                self,
                "must be an int literal, found a float literal".to_string(),
                "write an int-form literal like `14`",
            ),
            other => {
                // Analyze once before a cached integer erases its temporal source.
                let hir = self.analyze_expr(other);
                if hir.ty.is_error() {
                    return None;
                }
                if !allow_dynamic {
                    self.defer_temporal_fixedness_at(&hir, func.name, param.name, arg.span());
                }
                // A parameter of an inlined function bound to a literal
                // argument carries that value (`Symbol::const_int`), so a
                // helper function can forward its `simple int` length here.
                let known = if negated {
                    self.ast_const_int(other).and_then(i64::checked_neg)
                } else {
                    self.ast_const_int(other)
                };
                if let Some(value) =
                    known.filter(|_| !allow_dynamic || !self.temporal_dependencies(&hir))
                {
                    return if value < min {
                        e0219(
                            self,
                            format!("must be at least {min}, found {value}"),
                            "length is too small",
                        )
                    } else if value > max {
                        e0219(
                            self,
                            format!("must be at most {max}, found {value}"),
                            "length is too large",
                        )
                    } else {
                        if self.ast_ta_input(arg).is_none()
                            && matches!(self.ast_known_int(other), Some((_, true)))
                        {
                            self.warn_fixed_at_input_default(
                                func.name,
                                param.name,
                                value,
                                arg.span(),
                            );
                        }
                        Some(TaLengthArg::Fixed(value as u32, None))
                    };
                }
                // The normal analysis above already surfaced argument errors.
                if let Some(input) = self.selected_ta_int_input(&hir) {
                    if let InputValue::Int(default) = self.inputs[input as usize].defval {
                        let Some(value) = (if negated {
                            default.checked_neg()
                        } else {
                            Some(default)
                        }) else {
                            return e0219(
                                self,
                                format!("must be at least {min}"),
                                "length is too small",
                            );
                        };
                        return if value < min {
                            e0219(
                                self,
                                format!("must be at least {min}, found {value}"),
                                "length is too small",
                            )
                        } else if value > max {
                            e0219(
                                self,
                                format!("must be at most {max}, found {value}"),
                                "length is too large",
                            )
                        } else {
                            Some(TaLengthArg::Fixed(value as u32, Some((input, negated))))
                        };
                    }
                }
                if allow_dynamic && hir.ty == Type::Int {
                    let length = if negated {
                        HirExpr {
                            kind: HirExprKind::Unary {
                                op: UnOp::Neg,
                                operand: Box::new(hir.clone()),
                            },
                            ty: Type::Int,
                            qualifier: hir.qualifier,
                            span: arg.span(),
                        }
                    } else {
                        hir
                    };
                    return Some(TaLengthArg::Dynamic(length));
                }
                e0219(
                    self,
                    "must be an int literal".to_string(),
                    "the window size is fixed at compile time",
                )
            }
        }
    }

    /// An **engine-backed** stateful call (`ta.ema`, `ta.rsi`, `fixnan`,
    /// `math.sum`, ...): check each argument against the engine signature,
    /// bake the const parameters into a [`TaStateDecl`], allocate the next
    /// `TaStateId` (syntactic order), and emit
    /// [`HirExprKind::CallTaState`] over the runtime operands.
    pub(crate) fn analyze_ta_engine_call(
        &mut self,
        func: &BuiltinFunction,
        ta: TaFunc,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        let bound = match self.bind_ta_engine_args(func, args, span) {
            Some(a) => a,
            None => return self.error_expr(span),
        };
        let args = bound.as_slice();
        // Every timeframe, literal or not, is lowered onto `time(timeframe)`
        // (`analyze_timeframe_change`), which validates a literal one.
        if ta == TaFunc::TimeframeChange && args.len() == 1 {
            return self.analyze_timeframe_change(func, &args[0], span);
        }
        if matches!(ta, TaFunc::Highest | TaFunc::Lowest) && matches!(args.len(), 1 | 2) {
            let length = args.last().expect("checked arity");
            if self.ast_const_int(length).is_none() && self.ast_ta_input(length).is_none() {
                return self.analyze_dynamic_extremum(func, ta, args, span);
            }
        }
        if ta == TaFunc::Ema && args.len() == 2 {
            let length = &args[1];
            // An input default can determine a length at compile time even
            // when the selected input changes that length at runtime. Such a
            // length is a run-time one, with two exceptions that keep the
            // fixed lowering: a length that reads a `series` name is not a
            // `simple int`, and a length that the defaults put out of range
            // is the same `E0219` as a direct input with that default.
            let fixed = match self.ast_known_int(length) {
                Some((value, uses_input_default)) => {
                    !uses_input_default
                        || self.ast_reads_series_name(length)
                        || !(TA_LENGTH_RANGE.0..=TA_LENGTH_RANGE.1).contains(&value)
                }
                None => false,
            };
            if self.ast_ta_input(length).is_none() && !fixed {
                return self.analyze_simple_length_ema(func, args, span);
            }
        }
        if ta == TaFunc::Vwma && args.len() == 2 {
            let length = &args[1];
            if self.ast_const_int(length).is_none() && self.ast_ta_input(length).is_none() {
                return self.analyze_dynamic_vwma(func, args, span);
            }
        }
        if matches!(ta, TaFunc::Median | TaFunc::Mode | TaFunc::Range)
            && args.len() == 2
            && self.ast_const_int(&args[1]).is_none()
            && self.ast_ta_input(&args[1]).is_none()
        {
            return self.analyze_dynamic_statistic(func, ta, args, span);
        }
        // `ta.vwap(...)` shares one registry entry across three tags; the scalar
        // forms map to `VwapSource`/`VwapAnchored`, while the 3-arg banded form
        // maps to tuple-only `VwapBands` (rejected as a scalar value below).
        let ta = Self::vwap_tag_for_arity(ta, args.len());
        // A tuple-returning function is only valid as a destructuring RHS.
        if ta.is_tuple() {
            self.diagnostics.push(
                Diagnostic::error(
                    codes::E0225,
                    format!(
                        "`{}` returns a tuple; destructure it with `[a, b, c] = {}(...)`",
                        func.name, func.name
                    ),
                )
                .with_label(
                    span,
                    "a tuple-returning call cannot be used as a scalar value",
                ),
            );
            for arg in args {
                let _ = self.analyze_expr(arg);
            }
            return self.error_expr(span);
        }
        match self.build_ta_engine_call(func, ta, args, span) {
            Some((resolved_ta, state, runtime_args)) => HirExpr {
                kind: HirExprKind::CallTaState {
                    func: resolved_ta,
                    state,
                    args: runtime_args,
                },
                ty: match resolved_ta {
                    TaFunc::ValueWhenInt
                    | TaFunc::FixnanInt
                    | TaFunc::MedianInt
                    | TaFunc::ModeInt
                    | TaFunc::RangeInt => Type::Int,
                    TaFunc::ValueWhenBool => Type::Bool,
                    TaFunc::ValueWhenColor => Type::Color,
                    _ => func.ret.clone(),
                },
                qualifier: Qualifier::Series,
                span,
            },
            None => self.error_expr(span),
        }
    }

    /// `timeframe.change(timeframe)`: whether the bar opens a new bucket of
    /// `timeframe`. The call takes a `series string` timeframe and returns a
    /// `series bool`. The argument may be a series, simple, input or const
    /// string, so a computed or input timeframe is accepted.
    ///
    /// A bucket is what `time(timeframe)` opens: the interpreter's one model
    /// of timeframe buckets, which `request.security` resamples with too
    /// (`pinewasm_vm/src/resample.rs`, `TimeframeBuckets`). This crate anchors
    /// calendar-month and Monday-based-week buckets to the bar's trading day,
    /// and fixed widths to the exchange clock. So the call lowers, in ordinary
    /// statements evaluated on every execution of the call site, to
    ///
    /// ```text
    /// %tfcN_tf = <timeframe>                     // only when not a literal
    /// var int %tfcN_prev = na                    // per call site
    /// %tfcN_bucket = time(<timeframe>)
    /// %tfcN = na(%tfcN_prev) or %tfcN_bucket != %tfcN_prev
    /// %tfcN_prev := %tfcN_bucket
    /// ```
    ///
    /// The first execution reports a change. A literal timeframe, or one that
    /// folds to a `const` string, is passed to `time` as the literal itself,
    /// so a standalone WebAssembly module can serve a fixed-width one from a
    /// generated module (`pinewasm_aot`'s projection); any other timeframe is read from
    /// the temporary on every execution.
    ///
    /// The previous bucket is the opening time the call saw when it last
    /// executed, so a timeframe whose value changes between bars reports a
    /// change exactly when the bar's bucket under the new timeframe opens at
    /// a different time. This compiler's rule for that case is the one stated above.
    fn analyze_timeframe_change(
        &mut self,
        func: &BuiltinFunction,
        arg: &ast::Expr,
        span: Span,
    ) -> HirExpr {
        if self.reject_stateful_in_loop("stateful `timeframe.change`", span) {
            let _ = self.analyze_expr(arg);
            return self.error_expr(span);
        }
        // Taken before the timeframe is analysed, so a `timeframe.change`
        // nested in it mints names of its own.
        let base = format!("%tfc{}", self.timeframe_change_counter);
        self.timeframe_change_counter += 1;
        let ident = |name: String| ast::Ident { name, span };
        let read = |name: &str| {
            ast::Expr::Ident(ast::Ident {
                name: name.to_string(),
                span,
            })
        };
        let call = |callee: &str, args: Vec<ast::Expr>| ast::Expr::Call {
            callee: ast::Ident {
                name: callee.to_string(),
                span,
            },
            args: args
                .into_iter()
                .map(|value| ast::CallArg {
                    name: None,
                    value,
                    span,
                })
                .collect(),
            type_args: Vec::new(),
            span,
        };
        let binary = |op: ast::BinaryOp, lhs: ast::Expr, rhs: ast::Expr| ast::Expr::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
            span,
        };
        let declare = |name: &str, value: ast::Expr| ast::Stmt::Assign {
            name: ident(name.to_string()),
            value,
            is_var: false,
            is_varip: false,
            annotation: None,
            span,
        };

        let timeframe = if let ast::Expr::Str { .. } = arg {
            // `E0219` when the literal does not name a timeframe.
            if self
                .ta_const_timeframe(func.name, "timeframe", arg)
                .is_none()
            {
                return self.error_expr(span);
            }
            arg.clone()
        } else {
            // Evaluate the timeframe once and check its type before lowering,
            // so a wrong type names `timeframe.change`, not the call it
            // lowers to.
            let tf = format!("{base}_tf");
            let tf_stmt = self.analyze_stmt(&declare(&tf, arg.clone()));
            // A `const` timeframe (an alias such as `const string TF = "D"`,
            // or a const-folding concatenation) is known at compile time:
            // validate it as the literal would be, and pass it as one.
            let folded = match &tf_stmt {
                Some(HirStmt::Assign { value, .. }) => match self.const_input_value_from_hir(value)
                {
                    Some(InputValue::String(literal)) => Some(literal),
                    _ => None,
                },
                _ => None,
            };
            if let Some(literal) = folded {
                let arg_span = arg.span();
                let literal = ast::Expr::Str {
                    value: literal,
                    span: arg_span,
                };
                if self
                    .ta_const_timeframe(func.name, "timeframe", &literal)
                    .is_none()
                {
                    return self.error_expr(span);
                }
                literal
            } else {
                if let Some(stmt) = tf_stmt {
                    self.pending_stmts.push(stmt);
                }
                let Some(tf_id) = self.lookup_variable(&tf) else {
                    return self.error_expr(span);
                };
                let tf_ty = self.symbols[tf_id.0 as usize].ty.clone();
                if tf_ty.is_error() {
                    return self.error_expr(span);
                }
                if tf_ty != Type::String {
                    self.error(
                        codes::E0205,
                        format!(
                            "`timeframe.change` argument `timeframe` must be string, found {}",
                            tf_ty.describe()
                        ),
                        arg.span(),
                        "expected string",
                    );
                    return self.error_expr(span);
                }
                read(&tf)
            }
        };
        self.warn_if_not_executed_every_bar(
            "`timeframe.change` call",
            "this call's series state",
            span,
        );

        let prev = format!("{base}_prev");
        let bucket = format!("{base}_bucket");
        let changed = base;
        let lowered = [
            ast::Stmt::Assign {
                name: ident(prev.clone()),
                value: ast::Expr::Na { span },
                is_var: true,
                is_varip: false,
                annotation: Some(ast::TypeAnnotation {
                    kind: ast::AnnotationKind::Scalar(ast::AnnotatedType::Int),
                    qualifier: None,
                    span,
                }),
                span,
            },
            // The bucket's opening time: opening times taken under
            // different timeframes stay comparable when the timeframe
            // changes between bars.
            declare(&bucket, call("time", vec![timeframe])),
            declare(
                &changed,
                binary(
                    ast::BinaryOp::Or,
                    call("na", vec![read(&prev)]),
                    binary(ast::BinaryOp::Ne, read(&bucket), read(&prev)),
                ),
            ),
            ast::Stmt::Reassign {
                name: ident(prev.clone()),
                value: read(&bucket),
                span,
            },
        ];
        for stmt in &lowered {
            if let Some(stmt) = self.analyze_stmt(stmt) {
                self.pending_stmts.push(stmt);
            }
        }
        self.analyze_expr(&read(&changed))
    }

    fn analyze_dynamic_extremum(
        &mut self,
        func: &BuiltinFunction,
        ta: TaFunc,
        args: &[ast::Expr],
        span: Span,
    ) -> HirExpr {
        if self.reject_stateful_in_loop(&format!("stateful `{}`", func.name), span) {
            for arg in args {
                let _ = self.analyze_expr(arg);
            }
            return self.error_expr(span);
        }
        let source = if args.len() == 2 {
            match self.ta_source_argument(func.name, "source", &args[0]) {
                Some((source, _)) => source,
                None => return self.error_expr(span),
            }
        } else {
            let series = if ta == TaFunc::Highest {
                BuiltinSeries::High
            } else {
                BuiltinSeries::Low
            };
            HirExpr {
                kind: HirExprKind::ReadBuiltin { series, offset: 0 },
                ty: Type::Float,
                qualifier: Qualifier::Series,
                span,
            }
        };
        let length_arg = args.last().expect("checked arity");
        let length = self.analyze_expr(length_arg);
        if length.ty.is_error() {
            return self.error_expr(span);
        }
        if length.ty != Type::Int {
            self.e0219(
                func.name,
                "length",
                format!("must be an int, found {}", length.ty.describe()),
                length_arg.span(),
            );
            return self.error_expr(span);
        }
        self.warn_if_not_executed_every_bar(
            &format!("`{}` call", func.name),
            "this call's series state",
            span,
        );
        let slot = self.allocate_synthetic_slot(Type::Float, span);
        HirExpr {
            kind: HirExprKind::TaCallDynamic {
                func: ta,
                slot,
                source: Box::new(source),
                length: Box::new(length),
            },
            ty: Type::Float,
            qualifier: Qualifier::Series,
            span,
        }
    }

    fn analyze_dynamic_statistic(
        &mut self,
        func: &BuiltinFunction,
        ta: TaFunc,
        args: &[ast::Expr],
        span: Span,
    ) -> HirExpr {
        if self.reject_stateful_in_loop(&format!("stateful `{}`", func.name), span) {
            for arg in args {
                let _ = self.analyze_expr(arg);
            }
            return self.error_expr(span);
        }
        let Some((source, source_ty)) = self.ta_source_argument(func.name, "source", &args[0])
        else {
            return self.error_expr(span);
        };
        let length = self.analyze_expr(&args[1]);
        if length.ty.is_error() {
            return self.error_expr(span);
        }
        if length.ty != Type::Int {
            self.e0219(
                func.name,
                "length",
                format!("must be an int, found {}", length.ty.describe()),
                args[1].span(),
            );
            return self.error_expr(span);
        }
        let resolved = match (ta, source_ty) {
            (TaFunc::Median, Type::Int) => TaFunc::MedianInt,
            (TaFunc::Mode, Type::Int) => TaFunc::ModeInt,
            (TaFunc::Range, Type::Int) => TaFunc::RangeInt,
            _ => ta,
        };
        self.warn_if_not_executed_every_bar(
            &format!("`{}` call", func.name),
            "this call's series state",
            span,
        );
        let slot = self.allocate_synthetic_slot(Type::Float, span);
        HirExpr {
            kind: HirExprKind::TaCallDynamic {
                func: resolved,
                slot,
                source: Box::new(source),
                length: Box::new(length),
            },
            ty: if matches!(
                resolved,
                TaFunc::MedianInt | TaFunc::ModeInt | TaFunc::RangeInt
            ) {
                Type::Int
            } else {
                Type::Float
            },
            qualifier: Qualifier::Series,
            span,
        }
    }

    fn analyze_simple_length_ema(
        &mut self,
        func: &BuiltinFunction,
        args: &[ast::Expr],
        span: Span,
    ) -> HirExpr {
        if self.reject_stateful_in_loop("stateful `ta.ema`", span) {
            for arg in args {
                let _ = self.analyze_expr(arg);
            }
            return self.error_expr(span);
        }
        let Some((source, _)) = self.ta_source_argument(func.name, "source", &args[0]) else {
            return self.error_expr(span);
        };
        let length = self.analyze_expr(&args[1]);
        self.defer_temporal_fixedness_at(&length, func.name, "length", args[1].span());
        if length.ty.is_error() {
            return self.error_expr(span);
        }
        if length.ty != Type::Int || length.qualifier > Qualifier::Simple {
            self.e0219(
                func.name,
                "length",
                format!(
                    "must be a simple int, found {} {}",
                    length.qualifier.describe(),
                    length.ty.describe()
                ),
                args[1].span(),
            );
            return self.error_expr(span);
        }
        self.warn_if_not_executed_every_bar("`ta.ema` call", "this call's series state", span);
        let slot = self.allocate_synthetic_slot(Type::Float, span);
        if let Some((default_length, true)) = self.ast_known_int(&args[1]) {
            self.input_default_ema_lengths.insert(slot, default_length);
        }
        HirExpr {
            kind: HirExprKind::TaCallDynamic {
                func: TaFunc::Ema,
                slot,
                source: Box::new(source),
                length: Box::new(length),
            },
            ty: Type::Float,
            qualifier: Qualifier::Series,
            span,
        }
    }

    /// A series-length VWMA divides independently sampled weighted-source
    /// and volume means. Materialize the source and length once: either may
    /// have effects. Missing source skips a product observation while the
    /// denominator still observes that bar's volume.
    fn analyze_dynamic_vwma(
        &mut self,
        func: &BuiltinFunction,
        args: &[ast::Expr],
        span: Span,
    ) -> HirExpr {
        if self.reject_stateful_in_loop("stateful `ta.vwma`", span) {
            for arg in args {
                let _ = self.analyze_expr(arg);
            }
            return self.error_expr(span);
        }
        let Some((source, _)) = self.ta_source_argument(func.name, "source", &args[0]) else {
            return self.error_expr(span);
        };
        let length = self.analyze_expr(&args[1]);
        if length.ty.is_error() {
            return self.error_expr(span);
        }
        if length.ty != Type::Int {
            self.e0219(
                func.name,
                "length",
                format!("must be an int, found {}", length.ty.describe()),
                args[1].span(),
            );
            return self.error_expr(span);
        }
        self.warn_if_not_executed_every_bar("`ta.vwma` call", "this call's series state", span);
        let source_slot = self.allocate_synthetic_slot(Type::Float, span);
        let length_slot = self.allocate_synthetic_slot(Type::Int, span);
        let product_slot = self.allocate_synthetic_slot(Type::Float, span);
        let volume_slot = self.allocate_synthetic_slot(Type::Float, span);
        let source_once = HirExpr {
            kind: HirExprKind::MaterializeHistory {
                slot: source_slot,
                expr: Box::new(source),
                offset: 0,
            },
            ty: Type::Float,
            qualifier: Qualifier::Series,
            span,
        };
        let length_once = HirExpr {
            kind: HirExprKind::MaterializeHistory {
                slot: length_slot,
                expr: Box::new(length),
                offset: 0,
            },
            ty: Type::Int,
            qualifier: Qualifier::Series,
            span,
        };
        let read_length = HirExpr {
            kind: HirExprKind::ReadVar {
                symbol: SymbolId(length_slot.0),
                offset: 0,
            },
            ty: Type::Int,
            qualifier: Qualifier::Series,
            span,
        };
        let volume = HirExpr {
            kind: HirExprKind::ReadBuiltin {
                series: BuiltinSeries::Volume,
                offset: 0,
            },
            ty: Type::Float,
            qualifier: Qualifier::Series,
            span,
        };
        let product = HirExpr {
            kind: HirExprKind::Binary {
                op: BinOp::Mul,
                lhs: Box::new(source_once),
                rhs: Box::new(volume.clone()),
            },
            ty: Type::Float,
            qualifier: Qualifier::Series,
            span,
        };
        let numerator = HirExpr {
            kind: HirExprKind::TaCallDynamic {
                func: TaFunc::Sma,
                slot: product_slot,
                source: Box::new(product),
                length: Box::new(length_once),
            },
            ty: Type::Float,
            qualifier: Qualifier::Series,
            span,
        };
        let denominator = HirExpr {
            kind: HirExprKind::TaCallDynamic {
                func: TaFunc::Sma,
                slot: volume_slot,
                source: Box::new(volume),
                length: Box::new(read_length),
            },
            ty: Type::Float,
            qualifier: Qualifier::Series,
            span,
        };
        HirExpr {
            kind: HirExprKind::Binary {
                op: BinOp::Div,
                lhs: Box::new(numerator),
                rhs: Box::new(denominator),
            },
            ty: Type::Float,
            qualifier: Qualifier::Series,
            span,
        }
    }

    /// Analyze a **tuple-returning** engine call (`ta.macd`/`ta.bb`/`ta.kc`/
    /// `ta.dmi`) in a destructuring RHS. Builds the same
    /// [`HirExprKind::CallTaState`] node as the scalar path — the multi-push
    /// happens at MIR/CSIL level, driven by
    /// [`TaFunc::engine_result_arity`] — and reports the element type and
    /// arity for the caller's arity check. Every tuple element is
    /// `series float`.
    /// Resolve `ta.vwap`'s overload by its (dense, post-binding) argument
    /// count. The explicit `ta.vwap` analyzer normally handles this before the
    /// generic TA path; this stays as a defensive fallback for callers that
    /// already resolved the shared registry entry.
    pub(crate) fn vwap_tag_for_arity(ta: TaFunc, argc: usize) -> TaFunc {
        match ta {
            TaFunc::Vwap if argc >= 3 => TaFunc::VwapBands,
            TaFunc::Vwap if argc == 2 => TaFunc::VwapAnchored,
            TaFunc::Vwap if argc == 1 => TaFunc::VwapSource,
            other => other,
        }
    }

    pub(crate) fn analyze_ta_tuple_call(
        &mut self,
        func: &BuiltinFunction,
        ta: TaFunc,
        args: &[ast::CallArg],
        span: Span,
    ) -> TupleRhs {
        let bound = match self.bind_ta_engine_args(func, args, span) {
            Some(a) => a,
            None => return TupleRhs::Error,
        };
        let args = bound.as_slice();
        let ta = Self::vwap_tag_for_arity(ta, args.len());
        match self.build_ta_engine_call(func, ta, args, span) {
            Some((resolved_ta, state, runtime_args)) => TupleRhs::TaCall {
                call: HirExpr {
                    kind: HirExprKind::CallTaState {
                        func: resolved_ta,
                        state,
                        args: runtime_args,
                    },
                    // The node's scalar `ty` is unused (the Destructure store
                    // types come from the per-element type); keep it Void to
                    // mark the tuple shape.
                    ty: Type::Void,
                    qualifier: Qualifier::Series,
                    span,
                },
                elem_ty: Type::Float,
                arity: ta.engine_result_arity(),
            },
            None => TupleRhs::Error,
        }
    }

    /// Build one of the overloaded function signatures for `ta.vwap(...)`.
    /// The registry is keyed by name, so the call analyzer intercepts these
    /// overloads instead of trying to store all signatures under one key.
    pub(crate) fn ta_vwap_overload(ta: TaFunc, params: Vec<TaParam>) -> BuiltinFunction {
        BuiltinFunction {
            name: "ta.vwap",
            params: Vec::new(),
            ret: Type::Float,
            kind: BuiltinFunctionKind::Ta(ta),
            pure_result: None,
            variadic_max: None,
            ta_engine: Some(params),
        }
    }

    pub(crate) fn ta_vwap_source_param() -> TaParam {
        TaParam {
            name: VWAP_PARAM_NAMES[0],
            kind: TaParamKind::Source,
        }
    }

    pub(crate) fn ta_vwap_anchor_param() -> TaParam {
        TaParam {
            name: VWAP_PARAM_NAMES[1],
            kind: TaParamKind::Condition,
        }
    }

    pub(crate) fn ta_vwap_stdev_mult_param() -> TaParam {
        TaParam {
            name: VWAP_PARAM_NAMES[2],
            kind: TaParamKind::Source,
        }
    }

    /// `ta.vwap(source)` and `ta.vwap(source, anchor)` scalar overloads. The
    /// bare variable-style `ta.vwap` continues to resolve through the registry
    /// as a zero-operand TA engine call over `hlc3`.
    pub(crate) fn analyze_ta_vwap_function_call(
        &mut self,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        match args.len() {
            1 => {
                let func =
                    Self::ta_vwap_overload(TaFunc::VwapSource, vec![Self::ta_vwap_source_param()]);
                self.analyze_ta_engine_call(&func, TaFunc::VwapSource, args, span)
            }
            2 => {
                let func = Self::ta_vwap_overload(
                    TaFunc::VwapAnchored,
                    vec![Self::ta_vwap_source_param(), Self::ta_vwap_anchor_param()],
                );
                self.analyze_ta_engine_call(&func, TaFunc::VwapAnchored, args, span)
            }
            3 => {
                let func = Self::ta_vwap_overload(
                    TaFunc::VwapBands,
                    vec![
                        Self::ta_vwap_source_param(),
                        Self::ta_vwap_anchor_param(),
                        Self::ta_vwap_stdev_mult_param(),
                    ],
                );
                let bound = match self.bind_ta_engine_args(&func, args, span) {
                    Some(a) => a,
                    None => return self.error_expr(span),
                };
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::E0225,
                        "`ta.vwap` with `stdev_mult` returns a tuple; destructure it with `[vwap, upper, lower] = ta.vwap(...)`",
                    )
                    .with_label(
                        span,
                        "a tuple-returning call cannot be used as a scalar value",
                    ),
                );
                for arg in bound {
                    let _ = self.analyze_expr(&arg);
                }
                self.error_expr(span)
            }
            _ => {
                self.error(
                    codes::E0204,
                    format!("`ta.vwap` expects 1 to 3 argument(s), found {}", args.len()),
                    span,
                    "expected 1 to 3 argument(s)",
                );
                for arg in args {
                    let _ = self.analyze_expr(&arg.value);
                }
                self.error_expr(span)
            }
        }
    }

    pub(crate) fn analyze_ta_vwap_tuple_call(
        &mut self,
        args: &[ast::CallArg],
        span: Span,
    ) -> TupleRhs {
        let func = Self::ta_vwap_overload(
            TaFunc::VwapBands,
            vec![
                Self::ta_vwap_source_param(),
                Self::ta_vwap_anchor_param(),
                Self::ta_vwap_stdev_mult_param(),
            ],
        );
        self.analyze_ta_tuple_call(&func, TaFunc::VwapBands, args, span)
    }

    /// Shared core of the engine-backed call path: reject in-loop use,
    /// check arity, align arguments to the signature, check each runtime
    /// operand and bake each const parameter, warn on not-every-bar
    /// execution, allocate the next `TaStateId`, and return
    /// `(state, runtime_args)`. Returns `None` after reporting on any error.
    /// Shared by the scalar and tuple engine-call wrappers.
    /// Bind an engine-backed builtin's call arguments (positional + named) to
    /// its engine signature's parameter names, flattening to the dense
    /// positional list [`build_ta_engine_call`] consumes.
    pub(crate) fn bind_ta_engine_args(
        &mut self,
        func: &BuiltinFunction,
        args: &[ast::CallArg],
        span: Span,
    ) -> Option<Vec<ast::Expr>> {
        let sig = func
            .ta_engine
            .as_ref()
            .expect("engine ta functions declare a signature");
        let param_names: Vec<&str> = sig.iter().map(|p| p.name).collect();
        // Engine signatures list their required parameters last only *after*
        // an optional leading `SourceDefaulted` source (`ta.pivothigh(source =
        // high, leftbars, rightbars)`). When a named argument binds a later
        // required parameter but omits that leading source, the interior gap
        // must not be `E0204`: instead it is filled with an explicit read of
        // the parameter's default builtin series (`high`/`low`), which lowers
        // identically to the omitted-source path in `build_ta_engine_call`.
        // Every other engine optional is trailing, so it has no fillable
        // default here.
        let defaults: Vec<Option<ast::Expr>> = sig
            .iter()
            .map(|p| match p.kind {
                TaParamKind::SourceDefaulted(series) => Some(ast::Expr::Ident(ast::Ident {
                    name: series.name().to_string(),
                    span,
                })),
                _ => None,
            })
            .collect();
        let default_refs: Vec<Option<&ast::Expr>> = defaults.iter().map(|d| d.as_ref()).collect();
        self.bind_positional_with_defaults(func.name, &param_names, &default_refs, args, span)
    }

    /// Build the bare **variable** `ta.vwap` call site: a zero-operand
    /// `TaFunc::Vwap` state (empty params) reading `hlc3` cumulatively from bar
    /// 0. Applies the same in-loop rejection and not-every-bar warning as the
    /// generic engine path.
    pub(crate) fn build_bare_vwap_call(&mut self, func: &BuiltinFunction, span: Span) -> HirExpr {
        if self.reject_stateful_in_loop("stateful `ta.vwap`", span) {
            return self.error_expr(span);
        }
        self.warn_if_not_executed_every_bar("`ta.vwap`", "this call's series state", span);
        let state = self.ta_states.len() as u32;
        self.ta_states.push(TaStateDecl {
            func: TaFunc::Vwap,
            params: Vec::new(),
        });
        HirExpr {
            kind: HirExprKind::CallTaState {
                func: TaFunc::Vwap,
                state,
                args: Vec::new(),
            },
            ty: func.ret.clone(),
            qualifier: Qualifier::Series,
            span,
        }
    }

    pub(crate) fn build_ta_engine_call(
        &mut self,
        func: &BuiltinFunction,
        ta: TaFunc,
        args: &[ast::Expr],
        span: Span,
    ) -> Option<(TaFunc, u32, Vec<HirExpr>)> {
        let sig = func
            .ta_engine
            .as_ref()
            .expect("engine ta functions declare a signature");

        // A stateful ta call inside a loop body is a strict compile error
        // (E0222): its per-call-site engine state would advance multiple
        // times per bar. Analyze the arguments so their own errors surface.
        if self.reject_stateful_in_loop(&format!("stateful `{}`", func.name), span) {
            for arg in args {
                let _ = self.analyze_expr(arg);
            }
            return None;
        }

        // Argument count (E0204), same rule as the other builtins.
        let required = sig.iter().filter(|p| p.required()).count();
        if args.len() < required || args.len() > sig.len() {
            let expected = if required == sig.len() {
                format!("{required}")
            } else {
                format!("{required} to {}", sig.len())
            };
            self.error(
                codes::E0204,
                format!(
                    "`{}` expects {expected} argument(s), found {}",
                    func.name,
                    args.len()
                ),
                span,
                format!("expected {expected} argument(s)"),
            );
            for arg in args {
                let _ = self.analyze_expr(arg);
            }
            return None;
        }

        // Align arguments to parameters: trailing optional const parameters
        // pad with `None`, and an omitted leading defaulted source
        // (`ta.pivothigh(2, 2)`) shifts every argument one position right.
        let mut aligned: Vec<Option<&ast::Expr>> = vec![None; sig.len()];
        let default_source_omitted = args.len() < sig.len()
            && matches!(
                sig.first().map(|p| p.kind),
                Some(TaParamKind::SourceDefaulted(_))
            );
        let shift = usize::from(default_source_omitted);
        for (i, arg) in args.iter().enumerate() {
            aligned[i + shift] = Some(arg);
        }

        let mut runtime_args: Vec<HirExpr> = Vec::new();
        let mut resolved_ta = ta;
        let mut params: Vec<i64> = Vec::new();
        let mut ok = true;
        for (param, arg) in sig.iter().zip(aligned) {
            match param.kind {
                TaParamKind::Source => {
                    let arg = arg.expect("runtime operands are required (arity checked)");
                    let source = if func.name == "ta.valuewhen" {
                        self.ta_valuewhen_source_argument(arg)
                    } else {
                        self.ta_source_argument(func.name, param.name, arg)
                    };
                    match source {
                        Some((hir, source_ty)) => {
                            if func.name == "ta.valuewhen" {
                                resolved_ta = match source_ty {
                                    Type::Int => TaFunc::ValueWhenInt,
                                    Type::Bool => TaFunc::ValueWhenBool,
                                    Type::Color => TaFunc::ValueWhenColor,
                                    _ => resolved_ta,
                                };
                            } else if source_ty == Type::Int {
                                resolved_ta = match ta {
                                    TaFunc::ValueWhen => TaFunc::ValueWhenInt,
                                    TaFunc::Fixnan => TaFunc::FixnanInt,
                                    TaFunc::Median => TaFunc::MedianInt,
                                    TaFunc::Mode => TaFunc::ModeInt,
                                    TaFunc::Range => TaFunc::RangeInt,
                                    _ => resolved_ta,
                                };
                            }
                            runtime_args.push(hir);
                        }
                        None => ok = false,
                    }
                }
                TaParamKind::SourceDefaulted(series) => match arg {
                    // Omitted: materialize the default builtin-series read
                    // (`high` for `ta.pivothigh`, `low` for `ta.pivotlow`).
                    None => runtime_args.push(HirExpr {
                        kind: HirExprKind::ReadBuiltin { series, offset: 0 },
                        ty: Type::Float,
                        qualifier: Qualifier::Series,
                        span,
                    }),
                    Some(arg) => match self.ta_source_argument(func.name, param.name, arg) {
                        Some((hir, _)) => runtime_args.push(hir),
                        None => ok = false,
                    },
                },
                TaParamKind::Condition => {
                    let arg = arg.expect("runtime operands are required (arity checked)");
                    let hir = self.analyze_expr_expecting(arg, Some(&Type::Bool));
                    if hir.ty.is_error() {
                        ok = false;
                        continue;
                    }
                    if hir.ty != Type::Bool {
                        self.error(
                            codes::E0205,
                            format!(
                                "`{}` argument `{}` must be bool, found {}",
                                func.name,
                                param.name,
                                hir.ty.describe()
                            ),
                            arg.span(),
                            "expected bool",
                        );
                        ok = false;
                        continue;
                    }
                    runtime_args.push(hir);
                }
                TaParamKind::ConstInt { min, max, default } => match arg {
                    None => params.push(default.expect("optional const params declare a default")),
                    Some(arg) => {
                        match self.ta_const_int(func.name, param.name, arg, min..=max, true) {
                            Some((v, binding)) => {
                                if let Some((input, negated)) = binding {
                                    self.ta_input_bindings.push(pinewasm_hir::TaInputBinding {
                                        input,
                                        negated,
                                        target: pinewasm_hir::TaInputTarget::State {
                                            state: self.ta_states.len() as u32,
                                            param: params.len() as u32,
                                        },
                                    });
                                }
                                params.push(v);
                            }
                            None => ok = false,
                        }
                    }
                },
                TaParamKind::ConstFloat { positive, optional } => match arg {
                    // An omitted optional const float bakes no param (its
                    // presence/absence selects an overload, e.g. `ta.vwap`'s
                    // `stdev_mult` gating the banded tuple form).
                    None => debug_assert!(optional, "non-optional const float was omitted"),
                    Some(arg) => match self.ta_const_float(func.name, param.name, arg, positive) {
                        Some((v, input)) => {
                            if let Some((input, negated)) = input {
                                self.ta_input_bindings.push(pinewasm_hir::TaInputBinding {
                                    input,
                                    negated,
                                    target: pinewasm_hir::TaInputTarget::State {
                                        state: self.ta_states.len() as u32,
                                        param: params.len() as u32,
                                    },
                                });
                            }
                            params.push(v.to_bits() as i64);
                        }
                        None => ok = false,
                    },
                },
                TaParamKind::ConstPercent => {
                    let arg = arg.expect("const percent params are required (arity checked)");
                    match self.ta_const_float(func.name, param.name, arg, false) {
                        Some((v, _)) if !(0.0..=100.0).contains(&v) => {
                            self.e0219(
                                func.name,
                                param.name,
                                format!("must be between 0 and 100, found {v}"),
                                arg.span(),
                            );
                            ok = false;
                        }
                        Some((v, input)) => {
                            if let Some((input, negated)) = input {
                                self.ta_input_bindings.push(pinewasm_hir::TaInputBinding {
                                    input,
                                    negated,
                                    target: pinewasm_hir::TaInputTarget::State {
                                        state: self.ta_states.len() as u32,
                                        param: params.len() as u32,
                                    },
                                });
                            }
                            params.push(v.to_bits() as i64);
                        }
                        None => ok = false,
                    }
                }
                TaParamKind::ConstBool { default } => match arg {
                    None => params.push(i64::from(
                        default.expect("optional const params declare a default"),
                    )),
                    Some(arg) => match self.ta_const_bool(func.name, param.name, arg) {
                        Some(v) => params.push(i64::from(v)),
                        None => ok = false,
                    },
                },
                TaParamKind::ConstTimeframe { default } => match arg {
                    // Omitted: bake the default timeframe where the signature
                    // declares one.
                    None => params.push(default.expect("optional const params declare a default")),
                    Some(arg) => match self.ta_const_timeframe(func.name, param.name, arg) {
                        // Bake the timeframe's length in seconds (>= 1).
                        Some(secs) => params.push(secs),
                        None => ok = false,
                    },
                },
            }
        }
        if !ok {
            return None;
        }

        self.warn_if_not_executed_every_bar(
            &format!("`{}` call", func.name),
            "this call's series state",
            span,
        );

        let state = self.ta_states.len() as u32;
        self.ta_states.push(TaStateDecl {
            func: resolved_ta,
            params,
        });
        Some((resolved_ta, state, runtime_args))
    }

    /// Check one runtime **source** argument of an engine-backed stateful
    /// builtin: any numeric series, ints coerced to float (`E0205`
    /// otherwise). Returns `None` after reporting.
    pub(crate) fn ta_source_argument(
        &mut self,
        func_name: &str,
        param_name: &str,
        arg: &ast::Expr,
    ) -> Option<(HirExpr, Type)> {
        let hir = self.analyze_expr_expecting(arg, Some(&Type::Float));
        if hir.ty.is_error() {
            return None;
        }
        if !is_numeric(&hir.ty) {
            self.error(
                codes::E0205,
                format!(
                    "`{func_name}` argument `{param_name}` must be float, found {}",
                    hir.ty.describe()
                ),
                arg.span(),
                "expected float",
            );
            return None;
        }
        let source_ty = hir.ty.clone();
        Some((Self::coerce_int_to_float(hir), source_ty))
    }

    /// Check `ta.valuewhen`'s source union: numeric, bool,
    /// and color series, preserving the source type in its result.
    fn ta_valuewhen_source_argument(&mut self, arg: &ast::Expr) -> Option<(HirExpr, Type)> {
        let hir = self.analyze_expr(arg);
        if hir.ty.is_error() {
            return None;
        }
        let source_ty = hir.ty.clone();
        if is_numeric(&source_ty) {
            return Some((Self::coerce_int_to_float(hir), source_ty));
        }
        if matches!(source_ty, Type::Bool | Type::Color) {
            return Some((hir, source_ty));
        }
        self.error(
            codes::E0205,
            format!(
                "`ta.valuewhen` argument `source` must be numeric, bool, or color, found {}",
                source_ty.describe()
            ),
            arg.span(),
            "expected a numeric, bool, or color source",
        );
        None
    }

    /// Report an `E0219` for a bad const argument of a stateful builtin.
    pub(crate) fn e0219(&mut self, func_name: &str, param_name: &str, reason: String, span: Span) {
        self.error(
            codes::E0219,
            format!("invalid `{func_name}` argument `{param_name}`: {reason}"),
            span,
            "this argument is fixed at compile time",
        );
    }

    /// Unwrap grouping parentheses and one leading unary minus around a
    /// const-literal argument; returns `(negated, literal_expr)`.
    pub(crate) fn unwrap_const_literal(arg: &ast::Expr) -> (bool, &ast::Expr) {
        let mut expr = arg;
        while let ast::Expr::Group { inner, .. } = expr {
            expr = inner;
        }
        match expr {
            ast::Expr::Unary {
                op: ast::UnaryOp::Neg,
                operand,
                ..
            } => {
                let mut inner = operand.as_ref();
                while let ast::Expr::Group { inner: i, .. } = inner {
                    inner = i;
                }
                (true, inner)
            }
            other => (false, other),
        }
    }

    /// A const **int literal** argument within `min..=max` (`E0219`
    /// otherwise, with the exact reason).
    pub(crate) fn ta_const_int(
        &mut self,
        func_name: &str,
        param_name: &str,
        arg: &ast::Expr,
        bounds: std::ops::RangeInclusive<i64>,
        bind_input: bool,
    ) -> Option<(i64, Option<(u32, bool)>)> {
        let (min, max) = bounds.into_inner();
        let (negated, literal) = Self::unwrap_const_literal(arg);
        let apply_sign = |value: i64, negated| {
            if negated {
                value.checked_neg()
            } else {
                Some(value)
            }
        };
        let mut uses_input_default = false;
        let (value, binding) = match literal {
            ast::Expr::Int { value, .. } => (apply_sign(*value, negated), None),
            ast::Expr::Float { .. } => {
                self.e0219(
                    func_name,
                    param_name,
                    "must be an int literal, found a float literal".to_string(),
                    arg.span(),
                );
                return None;
            }
            other => {
                // Analyze once before a cached integer erases its temporal source.
                let hir = self.analyze_expr(other);
                if hir.ty.is_error() {
                    return None;
                }
                self.defer_temporal_fixedness_at(&hir, func_name, param_name, arg.span());
                let binding = if bind_input {
                    self.ast_ta_input(arg).or_else(|| {
                        self.selected_ta_int_input(&hir)
                            .map(|input| (input, negated))
                    })
                } else {
                    None
                };
                let value = match binding {
                    Some((input, negated)) => match self.inputs.get(input as usize)?.defval {
                        InputValue::Int(value) => apply_sign(value, negated),
                        _ => None,
                    },
                    // A literal-bound inlined parameter carries its value
                    // through Symbol::const_int.
                    None => self
                        .ast_const_int(other)
                        .and_then(|v| apply_sign(v, negated)),
                };
                // No binding reaches this argument, so a value the input
                // defaults gave is the value of every run.
                uses_input_default = bind_input
                    && binding.is_none()
                    && matches!(self.ast_known_int(other), Some((_, true)));
                (value, binding)
            }
        };
        let error = match value {
            Some(v) if v < min => format!("must be at least {min}, found {v}"),
            Some(v) if v > max => format!("must be at most {max}, found {v}"),
            Some(v) => {
                if uses_input_default {
                    self.warn_fixed_at_input_default(func_name, param_name, v, arg.span());
                }
                return Some((v, binding));
            }
            None if matches!(literal, ast::Expr::Int { .. }) => format!("must be at least {min}"),
            None => "must be an int literal".to_string(),
        };
        self.e0219(func_name, param_name, error, arg.span());
        None
    }

    /// A numeric literal or unchanged direct numeric input (including a `var`
    /// alias). Input integers are cast to floats for float
    /// parameters. Retain input provenance so a host override specializes the
    /// TA declaration for each run instead of baking the default.
    pub(crate) fn ta_const_float(
        &mut self,
        func_name: &str,
        param_name: &str,
        arg: &ast::Expr,
        positive: bool,
    ) -> Option<(f64, Option<(u32, bool)>)> {
        let (negated, literal) = Self::unwrap_const_literal(arg);
        let (value, input) = match literal {
            ast::Expr::Float { value, .. } => (Some(*value), None),
            ast::Expr::Int { value, .. } => (Some(*value as f64), None),
            other => {
                let hir = self.analyze_expr(other);
                self.defer_temporal_fixedness_at(&hir, func_name, param_name, arg.span());
                if hir.ty.is_error() {
                    return None;
                }
                let input = self.input_origin_for_expr(&hir).and_then(|origin| {
                    let decl = self.inputs.get(origin.input as usize)?;
                    if origin.inverted
                        || !matches!(
                            decl.kind,
                            InputKind::Int | InputKind::Float | InputKind::Price
                        )
                    {
                        return None;
                    }
                    match decl.defval {
                        InputValue::Float(value) => Some((value, origin.input)),
                        InputValue::Int(value) => Some((value as f64, origin.input)),
                        _ => None,
                    }
                });
                (
                    input.map(|(value, _)| value),
                    input.map(|(_, id)| (id, negated)),
                )
            }
        };
        let Some(value) = value else {
            self.e0219(
                func_name,
                param_name,
                "must be a number literal or unchanged numeric input".to_string(),
                arg.span(),
            );
            return None;
        };
        let value = if negated { -value } else { value };
        if !value.is_finite() {
            self.e0219(
                func_name,
                param_name,
                "must be a finite number".to_string(),
                arg.span(),
            );
            return None;
        }
        if positive && value <= 0.0 {
            self.e0219(
                func_name,
                param_name,
                format!("must be greater than zero, found {value}"),
                arg.span(),
            );
            return None;
        }
        Some((value, input))
    }

    /// A const `true`/`false` literal argument (`E0219` otherwise).
    pub(crate) fn ta_const_bool(
        &mut self,
        func_name: &str,
        param_name: &str,
        arg: &ast::Expr,
    ) -> Option<bool> {
        let (negated, literal) = Self::unwrap_const_literal(arg);
        match literal {
            ast::Expr::Bool { value, .. } if !negated => Some(*value),
            other => {
                let hir = self.analyze_expr(other);
                if hir.ty.is_error() {
                    return None;
                }
                self.e0219(
                    func_name,
                    param_name,
                    "must be `true` or `false`".to_string(),
                    arg.span(),
                );
                None
            }
        }
    }

    /// A const `timeframe` string literal for `timeframe.change`. Returns the
    /// timeframe's length in seconds (>= 1), or `None` with `E0219` when the
    /// string does not parse to a positive length.
    /// [`Self::analyze_timeframe_change`] calls it on a literal, or on the
    /// literal a `const` timeframe folds to, before lowering the call onto
    /// `time(timeframe)`; the `E0205` below is a defensive fallback.
    pub(crate) fn ta_const_timeframe(
        &mut self,
        func_name: &str,
        param_name: &str,
        arg: &ast::Expr,
    ) -> Option<i64> {
        let ast::Expr::Str { value, .. } = arg else {
            let hir = self.analyze_expr(arg);
            if !hir.ty.is_error() {
                self.error(
                    codes::E0205,
                    format!("`{func_name}` argument `{param_name}` must be a constant string"),
                    arg.span(),
                    "expected a timeframe string literal",
                );
            }
            return None;
        };
        // `timeframe_seconds` saturates an absurd numeral to `i64::MAX`, which
        // the CSIL validator accepts (the tf-seconds param range is 1..=MAX).
        let secs = timeframe_seconds(value);
        if secs < 1 {
            self.e0219(
                func_name,
                param_name,
                format!("`{value}` is not a valid timeframe"),
                arg.span(),
            );
            return None;
        }
        Some(secs)
    }
}
