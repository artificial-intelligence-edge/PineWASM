//! Expression analysis: literals, identifiers, unary/binary operators,
//! ternaries, `if`/`switch` expressions, branch-value unification, and
//! series history (`[]`) reads.

use crate::*;

impl<'r> Analyzer<'r> {
    // -- expressions ---------------------------------------------------------

    pub(crate) fn analyze_expr(&mut self, expr: &ast::Expr) -> HirExpr {
        self.analyze_expr_expecting(expr, None)
    }

    /// Analyze an expression with an optional **expected type** from the
    /// surrounding context. The expectation exists solely to give the `na`
    /// literal a type (function arguments, the operand next to a typed
    /// sibling, `:=` targets); every other expression ignores it — no
    /// bidirectional inference happens here.
    pub(crate) fn analyze_expr_expecting(
        &mut self,
        expr: &ast::Expr,
        expected: Option<&Type>,
    ) -> HirExpr {
        self.rendering_inline.begin();
        let pending_start = self.pending_stmts.len();
        let order = self.arg_order_enter(expr);
        let value = self.analyze_expr_expecting_inner(expr, expected);
        // An argument followed by one that hoists statements is captured
        // here, before the rendering metadata frame closes over it.
        let value = self.arg_order_leave(expr.span(), order, value);
        self.observe_temporal_pending(
            &self.pending_stmts[pending_start.min(self.pending_stmts.len())..],
        );
        self.rendering_inline.finish(
            &value,
            &self.pending_stmts[pending_start.min(self.pending_stmts.len())..],
        );
        value
    }

    /// Every expression the analyzer recurses into passes through here, so
    /// this is where the nesting budget is charged.
    fn analyze_expr_expecting_inner(
        &mut self,
        expr: &ast::Expr,
        expected: Option<&Type>,
    ) -> HirExpr {
        // A method call and an `if`/`switch` used as a value hold more stack
        // per level than an operator does.
        let cost = match expr {
            ast::Expr::MethodCall { .. } | ast::Expr::If(_) | ast::Expr::Switch(_) => 2,
            _ => 1,
        };
        let span = expr.span();
        if !self.enter_nesting(cost, span) {
            self.allow_tuple_expr = false;
            return HirExpr {
                kind: HirExprKind::ConstNa,
                ty: Type::Error,
                qualifier: Qualifier::Series,
                span,
            };
        }
        let value = self.analyze_expr_dispatch(expr, expected);
        self.leave_nesting(cost);
        value
    }

    fn analyze_expr_dispatch(&mut self, expr: &ast::Expr, expected: Option<&Type>) -> HirExpr {
        // A tuple expression is legal only as the *immediate* expression of a
        // destructuring RHS or a function-body tail. Consume the permission
        // here so it never propagates into sub-expressions: `[a, b] + 1`
        // analyzes `[a, b]` as an illegal (nested) tuple.
        let allow_tuple = std::mem::take(&mut self.allow_tuple_expr);
        if let ast::Expr::Tuple { elements, span } = expr {
            return self.analyze_tuple_expr(elements, *span, allow_tuple);
        }
        match expr {
            ast::Expr::Int { value, span } => HirExpr {
                kind: HirExprKind::ConstInt(*value),
                ty: Type::Int,
                qualifier: Qualifier::Const,
                span: *span,
            },
            ast::Expr::Float { value, span } => HirExpr {
                kind: HirExprKind::ConstFloat(*value),
                ty: Type::Float,
                qualifier: Qualifier::Const,
                span: *span,
            },
            ast::Expr::Bool { value, span } => HirExpr {
                kind: HirExprKind::ConstBool(*value),
                ty: Type::Bool,
                qualifier: Qualifier::Const,
                span: *span,
            },
            ast::Expr::Na { span } => self.analyze_na_literal(*span, expected),
            ast::Expr::Str { value, span } => HirExpr {
                kind: HirExprKind::ConstString(value.clone()),
                ty: Type::String,
                qualifier: Qualifier::Const,
                span: *span,
            },
            ast::Expr::Color { r, g, b, t, span } => HirExpr {
                kind: HirExprKind::ConstColor {
                    r: *r,
                    g: *g,
                    b: *b,
                    t: *t,
                },
                ty: Type::Color,
                qualifier: Qualifier::Const,
                span: *span,
            },
            ast::Expr::Ident(ident) => self.analyze_ident(ident, 0),
            // Parentheses pass the expectation through: `(na)` works
            // wherever `na` does.
            ast::Expr::Group { inner, .. } => self.analyze_expr_expecting(inner, expected),
            ast::Expr::Unary { op, operand, span } => self.analyze_unary(*op, operand, *span),
            ast::Expr::Binary { op, lhs, rhs, span } => self.analyze_binary(*op, lhs, rhs, *span),
            ast::Expr::Ternary {
                cond,
                then_e,
                else_e,
                span,
            } => self.analyze_ternary(cond, then_e, else_e, *span, expected),
            ast::Expr::Call {
                callee,
                args,
                type_args,
                span,
            } => self.analyze_call_expr(callee, args, type_args, *span),
            ast::Expr::MethodCall {
                receiver,
                method,
                args,
                span,
            } => self.analyze_method_call_expr(receiver, method, args, *span),
            ast::Expr::FieldAccess {
                receiver,
                field,
                span,
            } => self.analyze_field_access(receiver, field, *span),
            ast::Expr::History {
                target,
                offset,
                span,
            } => {
                let history = self.analyze_history(target, offset, *span);
                if history.ty == Type::Bool {
                    // The internal history lane uses a missing sentinel, so the
                    // boolean history read converts unavailable bars to false.
                    HirExpr {
                        ty: Type::Bool,
                        qualifier: history.qualifier,
                        span: *span,
                        kind: HirExprKind::CallPure {
                            func: PureFunc::CastBool,
                            args: vec![history],
                        },
                    }
                } else {
                    history
                }
            }
            ast::Expr::If(if_expr) => self.analyze_if_expr(if_expr, expected),
            ast::Expr::Switch(switch) => self.analyze_switch_expr(switch, expected),
            ast::Expr::Loop(stmt) => {
                let span = stmt.span();
                self.error(
                    codes::E0250,
                    "loop-result expressions are not yet supported by the PineWASM compiler",
                    span,
                    "this loop form is not lowered by this compiler yet",
                );
                HirExpr {
                    kind: HirExprKind::ConstNa,
                    ty: Type::Error,
                    qualifier: Qualifier::Series,
                    span,
                }
            }
            // Handled by the early return above.
            ast::Expr::Tuple { .. } => unreachable!("tuple handled before the dispatch"),
        }
    }

    /// Analyze a tuple expression `[e0, e1, ...]`. Legal only when
    /// `allow_tuple` (a destructuring RHS or a function-body tail); anywhere
    /// else is `E0225`. Elements are analyzed as ordinary scalar expressions
    /// (a tuple cannot nest inside a tuple element). The node's `ty` is
    /// `Void` — a tuple has no scalar type — and its qualifier is the join of
    /// the elements'.
    pub(crate) fn analyze_tuple_expr(
        &mut self,
        elements: &[ast::Expr],
        span: Span,
        allow_tuple: bool,
    ) -> HirExpr {
        if !allow_tuple {
            self.diagnostics.push(
                Diagnostic::error(
                    codes::E0225,
                    "a tuple `[...]` is only allowed as the right-hand side of a \
                     destructuring declaration or the last statement of a function body",
                )
                .with_label(span, "tuple not allowed here"),
            );
            for element in elements {
                let _ = self.analyze_expr(element);
            }
            return self.error_expr(span);
        }
        let mut hir_elements = Vec::with_capacity(elements.len());
        let mut qualifier = Qualifier::Const;
        let mut any_error = false;
        for element in elements {
            let hir = if self.defer_tuple_na && Self::na_literal_span(element).is_some() {
                HirExpr {
                    kind: HirExprKind::ConstNa,
                    ty: Type::Unknown,
                    qualifier: Qualifier::Const,
                    span: element.span(),
                }
            } else {
                self.analyze_expr(element)
            };
            if hir.ty.is_error() {
                any_error = true;
            }
            qualifier = qualifier.join(hir.qualifier);
            hir_elements.push(hir);
        }
        if any_error {
            return self.error_expr(span);
        }
        HirExpr {
            kind: HirExprKind::Tuple {
                elements: hir_elements,
            },
            ty: Type::Void,
            qualifier,
            span,
        }
    }

    /// The `na` literal adopts the type its context expects; with no
    /// expectation it cannot be typed (`E0220`). An `Error` expectation
    /// means the context already failed — suppress the cascade.
    pub(crate) fn analyze_na_literal(&mut self, span: Span, expected: Option<&Type>) -> HirExpr {
        match expected {
            Some(ty) if ty.is_error() => self.error_expr(span),
            Some(Type::Bool) => {
                self.diagnostics.push(
                    Diagnostic::error(codes::E0220, "`na` cannot have type bool in Pine")
                        .with_label(span, "boolean values must be true or false"),
                );
                self.error_expr(span)
            }
            Some(ty) if ty.is_text_format() => {
                self.diagnostics.push(
                    Diagnostic::error(codes::E0220, "`na` cannot have built-in text_format type")
                        .with_label(span, "use text.format_none to omit formatting"),
                );
                self.error_expr(span)
            }
            // Same for plot_display: its values are the display.* sets, and an
            // `na` would reach hosts as an invalid display binding.
            Some(ty) if ty.is_plot_display() => {
                self.diagnostics.push(
                    Diagnostic::error(codes::E0220, "`na` cannot have built-in plot_display type")
                        .with_label(span, "use display.none to hide the output"),
                );
                self.error_expr(span)
            }
            Some(ty) => HirExpr {
                kind: HirExprKind::ConstNa,
                ty: ty.clone(),
                qualifier: Qualifier::Const,
                span,
            },
            None => {
                self.diagnostics.push(
                    Diagnostic::error(codes::E0220, "`na` has no type of its own here")
                        .with_label(
                            span,
                            "the expected type cannot be inferred at this position",
                        )
                        .with_note(
                            "add a type annotation (`float x = na`) or use `na` where the type \
                             is known from context (a function argument, an operand next to a \
                             typed value, or a `:=` target)",
                        ),
                );
                self.error_expr(span)
            }
        }
    }

    /// The type an `na` operand adopts next to a typed sibling: bool for
    /// the logical operators, the sibling's type otherwise (an `Error`
    /// sibling propagates cascade suppression).
    pub(crate) fn operand_expectation(op: ast::BinaryOp, sibling: &Type) -> Type {
        match op {
            ast::BinaryOp::And | ast::BinaryOp::Or => Type::Bool,
            _ => sibling.clone(),
        }
    }

    /// The span of a (possibly parenthesized) `na` literal, or `None`.
    pub(crate) fn na_literal_span(expr: &ast::Expr) -> Option<Span> {
        let mut e = expr;
        while let ast::Expr::Group { inner, .. } = e {
            e = inner;
        }
        match e {
            ast::Expr::Na { span } => Some(*span),
            _ => None,
        }
    }

    pub(crate) fn analyze_ident(&mut self, ident: &ast::Ident, offset: u32) -> HirExpr {
        // An enum-member reference `Name.member`: `Name` names a collected
        // enum. Resolves to a const integer variant index typed as the enum.
        // Enum values have no history (`Name.member[1]` is E0209), like any
        // const.
        if let Some((enum_name, member)) = ident.name.rsplit_once('.') {
            if let Some(enum_name) = self.enum_type_name(enum_name) {
                let info = &self.enums[&enum_name];
                if let Some(index) = info.variant_index(member) {
                    if offset > 0 {
                        self.error(
                            codes::E0209,
                            "history reference target must be a series value",
                            ident.span,
                            format!("`{}` is a const enum value", ident.name),
                        );
                        return self.error_expr(ident.span);
                    }
                    return HirExpr {
                        kind: HirExprKind::ConstInt(index as i64),
                        ty: Type::Enum(enum_name.clone()),
                        qualifier: Qualifier::Const,
                        span: ident.span,
                    };
                }
                // `Name` is an enum but has no such member: a targeted E0239
                // (a bare `Name.typo`, not a chained `Name.member.x`).
                if !member.contains('.') {
                    self.error(
                        codes::E0239,
                        format!("enum `{enum_name}` has no member `{member}`"),
                        ident.span,
                        format!("`{enum_name}` has no member named `{member}`"),
                    );
                    return self.error_expr(ident.span);
                }
            }
        }
        // Object field read `obj.field` (and chained `obj.child.field`): the head
        // segment names an object-typed variable; each following segment is a
        // field access. Detected before the series/constant lookups so a dotted
        // name whose head is a local object never collides with a namespace.
        if let Some((head, rest)) = ident.name.split_once('.') {
            if let Some(id) = self.lookup_variable(head) {
                let head_ty = self.symbols[id.0 as usize].ty.clone();
                if matches!(head_ty, Type::Object(_)) {
                    return self.analyze_field_read(id, rest, offset, ident.span);
                }
            }
        }
        // Drawing `.all` accessors build a fresh array of live objects in
        // creation order. A positive literal history offset snapshots each
        // evaluated array into a hidden slot, as the dynamic-offset path does.
        // Offset zero needs only the current array.
        if let Some((head, tail)) = ident.name.split_once('.') {
            if tail == "all" {
                if let Some(kind) = DrawKind::from_tag(head).filter(|k| k.is_registered()) {
                    let value = HirExpr {
                        kind: HirExprKind::ReadDrawingsAll { kind },
                        ty: Type::Array(Box::new(Type::Object(kind.type_name().to_string()))),
                        qualifier: Qualifier::Series,
                        span: ident.span,
                    };
                    if offset == 0 {
                        return value;
                    }
                    if self.reject_stateful_in_loop("history of an expression", ident.span) {
                        return self.error_expr(ident.span);
                    }
                    self.warn_if_not_executed_every_bar(
                        "history of an expression",
                        "the hidden series backing this history",
                        ident.span,
                    );
                    let ty = value.ty.clone();
                    let slot = self.allocate_synthetic_slot(ty.clone(), ident.span);
                    return HirExpr {
                        kind: HirExprKind::MaterializeHistory {
                            slot,
                            expr: Box::new(value),
                            offset,
                        },
                        ty,
                        qualifier: Qualifier::Series,
                        span: ident.span,
                    };
                }
            }
        }
        // Variables and parameters may shadow a built-in value, so the
        // innermost binding resolves first. A read the compiler builds itself
        // (`builtin_ident`) names the built-in directly and is not a use of
        // the name by the script.
        let compiler_built = ident.name.strip_prefix(BUILTIN_IDENT_PREFIX);
        if let Some(symbol_id) = compiler_built
            .is_none()
            .then(|| self.lookup_variable(&ident.name))
            .flatten()
        {
            let symbol = &self.symbols[symbol_id.0 as usize];
            let (ty, qualifier) = if offset > 0 {
                (symbol.ty.clone(), Qualifier::Series)
            } else {
                (symbol.ty.clone(), symbol.qualifier)
            };
            return HirExpr {
                kind: HirExprKind::ReadVar {
                    symbol: symbol_id,
                    offset,
                },
                ty,
                qualifier,
                span: ident.span,
            };
        }
        let name = compiler_built.unwrap_or(&ident.name);
        if let Some(series) = self.registry.lookup_series(name) {
            // Code of an imported library is not the script's own use.
            if compiler_built.is_none() && self.lib_ctx.is_empty() {
                self.builtin_reads
                    .entry(name.to_string())
                    .or_insert(ident.span);
            }
            if compiler_built.is_none() {
                self.reject_built_in_hidden_by_a_later_library_global(ident);
            }
            // The `strategy.*` broker-state getters require a `strategy()`
            // declaration, exactly like the order primitives (E0241). Read in an
            // indicator (or an undeclared) script, they are a compile error.
            if series.is_strategy() && self.strategy.is_none() {
                self.error(
                    codes::E0241,
                    format!("`{name}` requires a `strategy(...)` declaration"),
                    ident.span,
                    "add a `strategy(\"...\")` declaration to read strategy state",
                );
                return self.error_expr(ident.span);
            }
            return HirExpr {
                kind: HirExprKind::ReadBuiltin { series, offset },
                ty: series.value_type(),
                qualifier: if offset == 0 {
                    series.current_read_qualifier()
                } else {
                    Qualifier::Series
                },
                span: ident.span,
            };
        }
        if let Some(constant) = self.registry.lookup_constant(&ident.name) {
            // The registry also stores run-stable chart-type context values.
            // Only their explicit capability permits direct history; Input
            // chart colors and ordinary constants still have none.
            if offset > 0 && !constant.permits_direct_history() {
                self.error(
                    codes::E0209,
                    "history reference target must be a series value",
                    ident.span,
                    format!("`{}` is a built-in value", constant.name),
                );
                return self.error_expr(ident.span);
            }
            let kind = match (
                constant.string_value,
                constant.color_value,
                constant.int_value,
                constant.bool_value,
            ) {
                (Some(s), _, _, _) => HirExprKind::ConstString(s.to_string()),
                (_, Some((r, g, b, t)), _, _) => HirExprKind::ConstColor {
                    r,
                    g,
                    b,
                    t: f64::from(t),
                },
                (_, _, Some(i), _) => HirExprKind::ConstInt(i),
                (_, _, _, Some(b)) => HirExprKind::ConstBool(b),
                (None, None, None, None) => HirExprKind::ConstFloat(constant.value),
            };
            let value = HirExpr {
                kind,
                ty: constant.ty.clone(),
                qualifier: constant.qualifier(),
                span: ident.span,
            };
            if offset == 0 {
                return value;
            }
            // A literal host representation is not a historical read. Store
            // the context value once per executed bar so positive offsets
            // retain warmup/missing values, as the dynamic history path does.
            if self.reject_stateful_in_loop("history of an expression", ident.span) {
                return self.error_expr(ident.span);
            }
            self.warn_if_not_executed_every_bar(
                "history of an expression",
                "the hidden series backing this history",
                ident.span,
            );
            let ty = value.ty.clone();
            let slot = self.allocate_synthetic_slot(ty.clone(), ident.span);
            return HirExpr {
                kind: HirExprKind::MaterializeHistory {
                    slot,
                    expr: Box::new(value),
                    offset,
                },
                ty,
                qualifier: Qualifier::Series,
                span: ident.span,
            };
        }
        // A user-defined function referenced without calling it.
        if self.current_functions().contains_key(&ident.name) {
            self.error(
                codes::E0216,
                format!("`{}` is a function and must be called", ident.name),
                ident.span,
                format!("write `{}(...)`", ident.name),
            );
            return self.error_expr(ident.span);
        }
        if let Some(func) = self.registry.lookup_function(&ident.name) {
            // Variable-style stateful builtins (`ta.obv`, `ta.vwap`, ...):
            // an engine function with an **empty** signature is referenced
            // as a bare name and desugars to a zero-operand call site.
            // History (`ta.obv[1]`) materializes the call into a hidden
            // series slot, like any compound history target.
            if let BuiltinFunctionKind::Ta(ta) = func.kind {
                // A bare engine variable desugars to a call site. Two shapes
                // qualify: an **empty** engine signature (`ta.obv`/`ta.vwap`)
                // desugars to a zero-arg call; and `ta.tr`,
                // whose bare variable form equals `ta.tr(false)`; the
                // `handle_na` argument is synthesized as `false`. History (`ta.obv[1]`) materializes the call into a
                // hidden series slot, like any compound history target.
                // `ta.vwap` (bare variable) is the cumulative-from-bar-0 form:
                // its registry signature is the FUNCTION form, so it does not
                // match the empty-signature rule below. Desugar it directly to
                // a zero-operand `Vwap` call site (no anchor param).
                if ta == TaFunc::Vwap {
                    let call = self.build_bare_vwap_call(func, ident.span);
                    if offset == 0 || call.ty.is_error() {
                        return call;
                    }
                    let ty = call.ty.clone();
                    let slot = self.allocate_synthetic_slot(ty.clone(), ident.span);
                    return HirExpr {
                        kind: HirExprKind::MaterializeHistory {
                            slot,
                            expr: Box::new(call),
                            offset,
                        },
                        ty,
                        qualifier: Qualifier::Series,
                        span: ident.span,
                    };
                }
                let bare_args: Option<Vec<ast::CallArg>> =
                    if func.ta_engine.as_ref().is_some_and(std::vec::Vec::is_empty) {
                        Some(Vec::new())
                    } else if ta == TaFunc::Tr {
                        Some(vec![ast::CallArg {
                            name: None,
                            value: ast::Expr::Bool {
                                value: false,
                                span: ident.span,
                            },
                            span: ident.span,
                        }])
                    } else {
                        None
                    };
                if let Some(bare_args) = bare_args {
                    let func = func.clone();
                    let call = self.analyze_ta_engine_call(&func, ta, &bare_args, ident.span);
                    if offset == 0 || call.ty.is_error() {
                        return call;
                    }
                    let ty = call.ty.clone();
                    let slot = self.allocate_synthetic_slot(ty.clone(), ident.span);
                    return HirExpr {
                        kind: HirExprKind::MaterializeHistory {
                            slot,
                            expr: Box::new(call),
                            offset,
                        },
                        ty,
                        qualifier: Qualifier::Series,
                        span: ident.span,
                    };
                }
            }
            self.error(
                codes::E0216,
                format!("`{}` is a function and must be called", ident.name),
                ident.span,
                format!("write `{}(...)`", ident.name),
            );
        } else if ident.name == DISCARD_NAME {
            self.error(
                codes::E0201,
                "a variable named `_` cannot be read",
                ident.span,
                "`_` marks an unused value; give the variable a name to use it",
            );
        } else {
            let mut diag =
                Diagnostic::error(codes::E0201, format!("unknown identifier `{}`", ident.name))
                    .with_label(ident.span, "not found in this scope");
            if let Some(suggestion) = self.suggestions.builtin(self.registry, &ident.name) {
                diag = diag.with_note(format!("did you mean `{suggestion}`?"));
            }
            let head = ident.name.split('.').next().unwrap_or(&ident.name);
            if self.global_declared_below_function(head).is_some() {
                diag = diag.with_note(declared_below_note(head));
            } else if ident.name.contains('.') {
                diag = diag.with_note(NAMESPACE_NOTE);
            }
            self.diagnostics.push(diag);
        }
        self.error_expr(ident.span)
    }

    /// The variable `name` refers to here, searching scopes innermost first.
    ///
    /// Inside an inlined function body (or a default argument, see
    /// `Self::enter_definition_scope`) the search covers the function's own
    /// scopes, from its boundary in `function_scope_bases` up, and then the
    /// global scope `scopes[0]`: the script's globals, or a library's once its
    /// namespace is swapped in. The scopes in between belong to the call sites
    /// the body was expanded into. Pine defines functions only at the top
    /// level, so a body's free names mean what they mean there, never a
    /// caller's parameter or local of the same name. Pine also lets a
    /// function read only the globals declared above its definition, so a
    /// global declared later is not found here even though the call that
    /// inlines the body comes after it; the name then falls through to a
    /// built-in or is unknown (`E0201`).
    pub(crate) fn lookup_variable(&self, name: &str) -> Option<SymbolId> {
        let boundary = self.function_scope_bases.last().copied();
        let base = boundary.map_or(0, |boundary| boundary.base);
        debug_assert!(
            base <= self.scopes.len(),
            "a function boundary outlived its scopes"
        );
        let (outer, own) = self.scopes.split_at(base.min(self.scopes.len()));
        if let Some(id) = own.iter().rev().find_map(|scope| scope.get(name).copied()) {
            return Some(id);
        }
        let id = outer.first()?.get(name).copied()?;
        match boundary {
            Some(boundary)
                if !declared_above(self.symbols[id.0 as usize].decl_span, boundary.definition) =>
            {
                None
            }
            _ => Some(id),
        }
    }

    /// The declaration of the global named exactly `name` that the current
    /// function body or default would see, when it cannot only because the
    /// global is declared below the function's definition
    /// ([`Self::lookup_variable`]). For a dotted name, callers that want the
    /// receiver pass its first segment.
    pub(crate) fn global_declared_below_function(&self, name: &str) -> Option<Span> {
        let boundary = self.function_scope_bases.last()?;
        let id = *self.scopes.first()?.get(name)?;
        let declaration = self.symbols[id.0 as usize].decl_span;
        (!declared_above(declaration, boundary.definition)).then_some(declaration)
    }

    /// Inside a library a built-in read is not recorded for the script-wide
    /// shadow-and-use check (`E0211`), so report here a library function that
    /// reads the built-in `ident` because the library's global of that name
    /// is declared below the function. Earlier releases read that global.
    pub(crate) fn reject_built_in_hidden_by_a_later_library_global(&mut self, ident: &ast::Ident) {
        if self.lib_ctx.is_empty() {
            return;
        }
        let Some(declaration) = self.global_declared_below_function(&ident.name) else {
            return;
        };
        self.diagnostics.push(
            Diagnostic::error(
                codes::E0211,
                format!(
                    "cannot use the built-in `{}`: the library declares `{}` below this function",
                    ident.name, ident.name
                ),
            )
            .with_label(ident.span, "the built-in is used here")
            .with_label(declaration, "this declaration shadows the built-in")
            .with_note(declared_below_note(&ident.name)),
        );
    }

    /// True while analyzing statements inside a series-controlled `if` or
    /// `switch` branch (at any nesting depth). Used for the path-dependence
    /// qualifier join on `:=` and for `W0004`: a series-materializing
    /// construct (a hidden history slot or a `ta.*` call site) in such a
    /// branch only stores on bars where the branch executes, so its history
    /// reads gaps as `na` for skipped bars. Input/simple-controlled branches
    /// are run-stable and do not increment the depth.
    pub(crate) fn in_conditional_branch(&self) -> bool {
        self.branch_depth > 0
    }

    /// Allocate a hidden series slot backing one `MaterializeHistory` or
    /// `TaCall` site. The synthetic symbol appears in the slot table (and
    /// dumps) under a non-colliding `%hN` name, but is never entered into
    /// any scope, so it cannot be referenced, shadowed, or redeclared
    /// (`E0211` never sees it).
    pub(crate) fn allocate_synthetic_slot(&mut self, ty: Type, span: Span) -> SeriesSlot {
        let id = SymbolId(self.symbols.len() as u32);
        let slot = SeriesSlot(id.0);
        let name = format!("%h{}", self.synthetic_slots);
        self.synthetic_slots += 1;
        self.symbols.push(Symbol {
            id,
            name,
            ty,
            qualifier: Qualifier::Series,
            slot,
            persistent: false,
            varip: false,
            synthetic: true,
            const_int: None,
            decl_span: span,
        });
        slot
    }

    pub(crate) fn analyze_unary(
        &mut self,
        op: ast::UnaryOp,
        operand: &ast::Expr,
        span: Span,
    ) -> HirExpr {
        // `not na` expects bool; `-na` stays uninferable (int or float
        // would both fit), so it is E0220 like any other bare `na`.
        let operand_hir = match op {
            ast::UnaryOp::Not => self.analyze_expr_expecting(operand, Some(&Type::Bool)),
            ast::UnaryOp::Neg | ast::UnaryOp::Pos => self.analyze_expr(operand),
        };
        if operand_hir.ty.is_error() {
            return self.error_expr(span);
        }
        // Unary `+` is a numeric identity: `+x == x`. A numeric
        // operand folds straight through (no HIR node); a non-numeric operand
        // is `E0207`, mirroring unary `-`.
        if op == ast::UnaryOp::Pos {
            if is_numeric(&operand_hir.ty) {
                return operand_hir;
            }
            self.error(
                codes::E0207,
                format!(
                    "unary `+` requires int or float, found {}",
                    operand_hir.ty.describe()
                ),
                operand.span(),
                "expected int or float",
            );
            return self.error_expr(span);
        }
        let (hir_op, result) = match op {
            // Negation is numeric-preserving: int → int (checked at
            // runtime; overflow yields na), float → float.
            ast::UnaryOp::Neg if is_numeric(&operand_hir.ty) => (UnOp::Neg, operand_hir.ty.clone()),
            ast::UnaryOp::Not if operand_hir.ty == Type::Bool => (UnOp::Not, Type::Bool),
            _ => {
                let (hir_op, expected) = match op {
                    ast::UnaryOp::Neg => (UnOp::Neg, "int or float"),
                    ast::UnaryOp::Not => (UnOp::Not, "bool"),
                    ast::UnaryOp::Pos => unreachable!("handled above"),
                };
                self.error(
                    codes::E0207,
                    format!(
                        "unary `{}` requires {expected}, found {}",
                        hir_op.symbol(),
                        operand_hir.ty.describe()
                    ),
                    operand.span(),
                    format!("expected {expected}"),
                );
                return self.error_expr(span);
            }
        };
        let qualifier = operand_hir.qualifier;
        HirExpr {
            kind: HirExprKind::Unary {
                op: hir_op,
                operand: Box::new(operand_hir),
            },
            ty: result,
            qualifier,
            span,
        }
    }

    /// `display.a + display.b` is a set union and `display.a - display.b` a
    /// set difference, never string arithmetic. Both operands must be display
    /// values. When both are known at compile time the result folds to one
    /// canonical token, so a variable, ternary or reassignment holding it
    /// carries a single display value instead of a string expression the
    /// runtime cannot subtract or a host would have to decode. Otherwise
    /// [`Self::runtime_display_set_op`] computes the canonical token per run.
    fn analyze_display_set_op(
        &mut self,
        union: bool,
        lhs: HirExpr,
        rhs: HirExpr,
        span: Span,
    ) -> HirExpr {
        use pinewasm_hir::display_set;
        let symbol = if union { "+" } else { "-" };
        if !lhs.ty.is_plot_display() || !rhs.ty.is_plot_display() {
            self.error(
                codes::E0207,
                format!(
                    "operator `{symbol}` cannot be applied to {} and {}",
                    lhs.ty.describe(),
                    rhs.ty.describe()
                ),
                span,
                "display values combine only with other display.* values",
            );
            return self.error_expr(span);
        }
        let constant_mask = |this: &Self, expr: &HirExpr| {
            if this.temporal_dependencies(expr) {
                return None;
            }
            match this.const_input_value_from_hir(expr) {
                Some(InputValue::String(token)) => display_set::mask(&token),
                _ => None,
            }
        };
        let (left_mask, right_mask) = (constant_mask(self, &lhs), constant_mask(self, &rhs));
        let (Some(left), Some(right)) = (left_mask, right_mask) else {
            return self.runtime_display_set_op(union, lhs, rhs, left_mask, right_mask, span);
        };
        let mask = if union { left | right } else { left & !right };
        let token = display_set::token(mask)
            .expect("a union or difference of display sets is a display set");
        HirExpr {
            kind: HirExprKind::ConstString(token),
            ty: Type::PlotDisplay,
            qualifier: Qualifier::Const,
            span,
        }
    }

    /// Display set union or difference with an operand known only at run
    /// time, such as `X + display.price_scale` where `X` is chosen by an
    /// input. Pine types the result like its operands (`input plot_display`
    /// for an input-chosen `X`), so it is a valid plot-family `display`
    /// argument.
    ///
    /// The engine computes the result from the operands' canonical tokens with
    /// string equality, integer remainder and ternaries only, which the
    /// interpreter and native WebAssembly already run identically:
    ///
    /// 1. evaluate each operand once into a hidden slot (`L`, `R`);
    /// 2. if either is `na`, the result is `na`, as for `text_format` unions.
    ///    An explicit `na` is E0220, but an implicit one still reaches here:
    ///    an `if` without `else`, a `switch` without a default, a missing
    ///    tuple component, or the history of the first bar (`X[1]`);
    /// 3. decode each run-time token into its destination bits (`mL`, `mR`,
    ///    pane 1 .. Data Window 8) with a chain of token comparisons;
    /// 4. combine bit by bit (`a or b` for `+`, `a and not b` for `-`) and
    ///    select the canonical token of the resulting set with a ternary per
    ///    destination.
    ///
    /// A constant operand contributes constant bits, so only the run-time side
    /// is decoded. The result is never a joined string: it is one of the 16
    /// canonical tokens, which the host binding projects to a member union
    /// (`display_binding` in the VM crate). Hosts that bind the display
    /// before a run get the same operation applied under each input choice
    /// (`display_set::combine_bindings`, via `display_set_ops`).
    fn runtime_display_set_op(
        &mut self,
        union: bool,
        lhs: HirExpr,
        rhs: HirExpr,
        left_mask: Option<u8>,
        right_mask: Option<u8>,
        span: Span,
    ) -> HirExpr {
        use pinewasm_hir::display_set;
        let ty = Type::PlotDisplay;
        let qualifier = lhs.qualifier.join(rhs.qualifier);
        let left_slot = self.allocate_synthetic_slot(ty.clone(), lhs.span);
        let right_slot = self.allocate_synthetic_slot(ty.clone(), rhs.span);
        self.temporal_binding(SymbolId(left_slot.0), &lhs);
        self.temporal_binding(SymbolId(right_slot.0), &rhs);
        self.display_set_ops.insert(SymbolId(left_slot.0), union);
        let expr = |kind: HirExprKind, ty: Type| HirExpr {
            kind,
            ty,
            qualifier,
            span,
        };
        let read = |slot: SeriesSlot, ty: Type| {
            expr(
                HirExprKind::ReadVar {
                    symbol: SymbolId(slot.0),
                    offset: 0,
                },
                ty,
            )
        };
        let binary = |op: BinOp, lhs: HirExpr, rhs: HirExpr, ty: Type| {
            expr(
                HirExprKind::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                ty,
            )
        };
        let select = |cond: HirExpr, then_e: HirExpr, else_e: HirExpr| {
            let ty = then_e.ty.clone();
            expr(
                HirExprKind::Ternary {
                    cond: Box::new(cond),
                    then_e: Box::new(then_e),
                    else_e: Box::new(else_e),
                },
                ty,
            )
        };
        let int = |value: i64| HirExpr {
            kind: HirExprKind::ConstInt(value),
            ty: Type::Int,
            qualifier: Qualifier::Const,
            span,
        };
        let token = |mask: u8| HirExpr {
            kind: HirExprKind::ConstString(
                display_set::token(mask).expect("every destination set has a canonical token"),
            ),
            ty: Type::PlotDisplay,
            qualifier: Qualifier::Const,
            span,
        };

        // Destination bits of one operand: constant, or read from its
        // decoded mask slot.
        enum Bit {
            Const(bool),
            Expr(HirExpr),
        }
        let mut decoded = Vec::new();
        let mut missing: Option<HirExpr> = None;
        let mut operand_bits = |this: &mut Self, slot: SeriesSlot, constant: Option<u8>| {
            if let Some(mask) = constant {
                return (0..display_set::ALL_MASK.count_ones())
                    .map(|bit| Bit::Const(mask & (1 << bit) != 0))
                    .collect::<Vec<_>>();
            }
            let is_na = expr(
                HirExprKind::CallPure {
                    func: PureFunc::Na,
                    args: vec![read(slot, ty.clone())],
                },
                Type::Bool,
            );
            missing = Some(match missing.take() {
                Some(other) => binary(BinOp::Or, other, is_na, Type::Bool),
                None => is_na,
            });
            let mask_slot = this.allocate_synthetic_slot(Type::Int, span);
            let chain = (0..display_set::ALL_MASK).rev().fold(
                int(i64::from(display_set::ALL_MASK)),
                |rest, mask| {
                    select(
                        binary(BinOp::Eq, read(slot, ty.clone()), token(mask), Type::Bool),
                        int(i64::from(mask)),
                        rest,
                    )
                },
            );
            decoded.push((mask_slot, chain));
            (0..display_set::ALL_MASK.count_ones())
                .map(|bit| {
                    // Bit `bit` of a destination mask: (m % 2^(bit+1)) >= 2^bit.
                    Bit::Expr(binary(
                        BinOp::Ge,
                        binary(
                            BinOp::Rem,
                            read(mask_slot, Type::Int),
                            int(2 << bit),
                            Type::Int,
                        ),
                        int(1 << bit),
                        Type::Bool,
                    ))
                })
                .collect()
        };
        let left_bits = operand_bits(self, left_slot, left_mask);
        let right_bits = operand_bits(self, right_slot, right_mask);
        let not = |operand: HirExpr| {
            expr(
                HirExprKind::Unary {
                    op: UnOp::Not,
                    operand: Box::new(operand),
                },
                Type::Bool,
            )
        };
        let result_bits: Vec<Bit> = left_bits
            .into_iter()
            .zip(right_bits)
            .map(|(a, b)| match (union, a, b) {
                (true, Bit::Const(true), _) | (true, _, Bit::Const(true)) => Bit::Const(true),
                (true, Bit::Const(false), other) | (true, other, Bit::Const(false)) => other,
                (true, Bit::Expr(a), Bit::Expr(b)) => {
                    Bit::Expr(binary(BinOp::Or, a, b, Type::Bool))
                }
                (false, Bit::Const(false), _) | (false, _, Bit::Const(true)) => Bit::Const(false),
                (false, a, Bit::Const(false)) => a,
                (false, Bit::Const(true), Bit::Expr(b)) => Bit::Expr(not(b)),
                (false, Bit::Expr(a), Bit::Expr(b)) => {
                    Bit::Expr(binary(BinOp::And, a, not(b), Type::Bool))
                }
            })
            .collect();
        fn choose(
            bits: &[Bit],
            bit: usize,
            mask: u8,
            token: &dyn Fn(u8) -> HirExpr,
            select: &dyn Fn(HirExpr, HirExpr, HirExpr) -> HirExpr,
        ) -> HirExpr {
            let Some(current) = bits.get(bit) else {
                return token(mask);
            };
            let with = mask | (1 << bit);
            match current {
                Bit::Const(true) => choose(bits, bit + 1, with, token, select),
                Bit::Const(false) => choose(bits, bit + 1, mask, token, select),
                Bit::Expr(cond) => select(
                    cond.clone(),
                    choose(bits, bit + 1, with, token, select),
                    choose(bits, bit + 1, mask, token, select),
                ),
            }
        }
        let combined = choose(&result_bits, 0, 0, &token, &select);
        let combined = if decoded.is_empty() {
            combined
        } else {
            expr(
                HirExprKind::EvalSequence {
                    bindings: decoded,
                    result: Box::new(combined),
                },
                ty.clone(),
            )
        };
        let result = match missing {
            Some(missing) => select(
                missing,
                HirExpr {
                    kind: HirExprKind::ConstNa,
                    ty: ty.clone(),
                    qualifier: Qualifier::Const,
                    span,
                },
                combined,
            ),
            None => combined,
        };
        expr(
            HirExprKind::EvalSequence {
                bindings: vec![(left_slot, lhs), (right_slot, rhs)],
                result: Box::new(result),
            },
            ty,
        )
    }

    /// Capture dynamic operands once and union their formatting flags. Our
    /// strings carry a nominal type and never enter ordinary concatenation.
    fn analyze_text_format_union(&mut self, lhs: HirExpr, rhs: HirExpr, span: Span) -> HirExpr {
        use pinewasm_hir::text_format::{text_format_union, TEXT_FORMAT_BOTH, TEXT_FORMAT_NONE};
        let ty = lhs.ty.clone();
        let qualifier = lhs.qualifier.join(rhs.qualifier);
        let fixed = !self.temporal_dependencies(&lhs) && !self.temporal_dependencies(&rhs);
        if let (Some(InputValue::String(l)), Some(InputValue::String(r))) = (
            fixed
                .then(|| self.const_input_value_from_hir(&lhs))
                .flatten(),
            fixed
                .then(|| self.const_input_value_from_hir(&rhs))
                .flatten(),
        ) {
            if let Some(value) = text_format_union(&l, &r) {
                return HirExpr {
                    kind: HirExprKind::ConstString(value.to_string()),
                    ty,
                    qualifier,
                    span,
                };
            }
        }
        let left_slot = self.allocate_synthetic_slot(ty.clone(), span);
        let right_slot = self.allocate_synthetic_slot(ty.clone(), span);
        self.temporal_binding(SymbolId(left_slot.0), &lhs);
        self.temporal_binding(SymbolId(right_slot.0), &rhs);
        let read = |slot: SeriesSlot| HirExpr {
            kind: HirExprKind::ReadVar {
                symbol: SymbolId(slot.0),
                offset: 0,
            },
            ty: ty.clone(),
            qualifier,
            span,
        };
        let left = read(left_slot);
        let right = read(right_slot);
        let constant = |value: &str| HirExpr {
            kind: HirExprKind::ConstString(value.to_string()),
            ty: ty.clone(),
            qualifier: Qualifier::Const,
            span,
        };
        let equal = |lhs: HirExpr, rhs: HirExpr| HirExpr {
            kind: HirExprKind::Binary {
                op: BinOp::Eq,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
            ty: Type::Bool,
            qualifier,
            span,
        };
        let select = |cond: HirExpr, then_e: HirExpr, else_e: HirExpr| HirExpr {
            kind: HirExprKind::Ternary {
                cond: Box::new(cond),
                then_e: Box::new(then_e),
                else_e: Box::new(else_e),
            },
            ty: ty.clone(),
            qualifier,
            span,
        };
        let union = select(
            equal(left.clone(), constant(TEXT_FORMAT_NONE)),
            right.clone(),
            select(
                equal(right.clone(), constant(TEXT_FORMAT_NONE)),
                left.clone(),
                select(
                    equal(left.clone(), right.clone()),
                    left,
                    constant(TEXT_FORMAT_BOTH),
                ),
            ),
        );
        // Tuple control-flow can supply an implicit missing component even
        // though an explicit na formatting value is rejected here (E0220).
        // Preserve that missing value in our lane; never invent bold/italic
        // flags from it.
        let missing = HirExpr {
            kind: HirExprKind::Binary {
                op: BinOp::Or,
                lhs: Box::new(HirExpr {
                    kind: HirExprKind::CallPure {
                        func: PureFunc::Na,
                        args: vec![read(left_slot)],
                    },
                    ty: Type::Bool,
                    qualifier,
                    span,
                }),
                rhs: Box::new(HirExpr {
                    kind: HirExprKind::CallPure {
                        func: PureFunc::Na,
                        args: vec![read(right_slot)],
                    },
                    ty: Type::Bool,
                    qualifier,
                    span,
                }),
            },
            ty: Type::Bool,
            qualifier,
            span,
        };
        let result = select(
            missing,
            HirExpr {
                kind: HirExprKind::ConstNa,
                ty: ty.clone(),
                qualifier: Qualifier::Const,
                span,
            },
            union,
        );
        HirExpr {
            kind: HirExprKind::EvalSequence {
                bindings: vec![(left_slot, lhs), (right_slot, rhs)],
                result: Box::new(result),
            },
            ty,
            qualifier,
            span,
        }
    }

    pub(crate) fn analyze_binary(
        &mut self,
        op: ast::BinaryOp,
        lhs: &ast::Expr,
        rhs: &ast::Expr,
        span: Span,
    ) -> HirExpr {
        // An `na` literal on one side adopts its sibling's type (`close +
        // na` is float, `1 + na` is int, `"a" == na` is string); the
        // logical operators always expect bool, so even `na and na` types.
        // `na + na` has no typed sibling: each side reports E0220.
        let lhs_na = Self::na_literal_span(lhs).is_some();
        let rhs_na = Self::na_literal_span(rhs).is_some();
        let logical = matches!(op, ast::BinaryOp::And | ast::BinaryOp::Or);
        // A lone `na` lhs is analyzed second, typed by the rhs. One pair of
        // operand slots serves every case, and everything after the operands
        // is in `finish_binary`, because an operator chain recurses through
        // this function once per term.
        let rhs_first = !logical && lhs_na && !rhs_na;
        let (first_e, second_e) = if rhs_first { (rhs, lhs) } else { (lhs, rhs) };
        let first = self.analyze_expr_expecting(first_e, logical.then_some(&Type::Bool));
        // Statements the rhs desugared into — an if/switch-expression chain, or
        // an inlined UDF's leading statements and argument temps. For a logical
        // operator these must run only when the short-circuit does not skip the
        // rhs, so they are captured here (rather than draining to the enclosing
        // statement) and re-emitted inside a guard by `short_circuit_logical`.
        let mut rhs_lazy_stmts: Vec<HirStmt> = Vec::new();
        let second = if logical {
            let series_lhs = first.ty == Type::Bool && first.qualifier == Qualifier::Series;
            let saved = std::mem::take(&mut self.pending_stmts);
            let rhs_hir = self.analyze_expr_in_conditional_context(
                rhs,
                Some(&Type::Bool),
                ConditionalStatefulContext::LazyBoolean,
                series_lhs,
            );
            rhs_lazy_stmts = std::mem::replace(&mut self.pending_stmts, saved);
            rhs_hir
        } else {
            let expected = (lhs_na != rhs_na).then(|| Self::operand_expectation(op, &first.ty));
            self.analyze_expr_expecting(second_e, expected.as_ref())
        };
        self.finish_binary(op, first, second, rhs_first, rhs_lazy_stmts, span)
    }

    /// Type an analyzed binary operation and build its expression. The
    /// operands come in the order they were analyzed: `rhs_first` when the
    /// rhs went first. Kept out of `analyze_binary`, which is on the stack
    /// once per term of an operator chain.
    #[inline(never)]
    fn finish_binary(
        &mut self,
        op: ast::BinaryOp,
        first: HirExpr,
        second: HirExpr,
        rhs_first: bool,
        rhs_lazy_stmts: Vec<HirStmt>,
        span: Span,
    ) -> HirExpr {
        let (mut lhs_hir, mut rhs_hir) = if rhs_first {
            (second, first)
        } else {
            (first, second)
        };
        let logical = matches!(op, ast::BinaryOp::And | ast::BinaryOp::Or);
        if lhs_hir.ty.is_error() || rhs_hir.ty.is_error() {
            return self.error_expr(span);
        }

        if op == ast::BinaryOp::Add && lhs_hir.ty.is_text_format() && rhs_hir.ty.is_text_format() {
            return self.analyze_text_format_union(lhs_hir, rhs_hir, span);
        }

        if matches!(op, ast::BinaryOp::Add | ast::BinaryOp::Sub)
            && (lhs_hir.ty.is_plot_display() || rhs_hir.ty.is_plot_display())
        {
            return self.analyze_display_set_op(op == ast::BinaryOp::Add, lhs_hir, rhs_hir, span);
        }

        let hir_op = match op {
            ast::BinaryOp::Add => BinOp::Add,
            ast::BinaryOp::Sub => BinOp::Sub,
            ast::BinaryOp::Mul => BinOp::Mul,
            ast::BinaryOp::Div => BinOp::Div,
            ast::BinaryOp::Rem => BinOp::Rem,
            ast::BinaryOp::Lt => BinOp::Lt,
            ast::BinaryOp::Le => BinOp::Le,
            ast::BinaryOp::Gt => BinOp::Gt,
            ast::BinaryOp::Ge => BinOp::Ge,
            ast::BinaryOp::Eq => BinOp::Eq,
            ast::BinaryOp::Ne => BinOp::Ne,
            ast::BinaryOp::And => BinOp::And,
            ast::BinaryOp::Or => BinOp::Or,
        };

        let both_numeric = is_numeric(&lhs_hir.ty) && is_numeric(&rhs_hir.ty);
        // Static arithmetic is typed int when both operands are int,
        // including division. An uneven int quotient keeps any fractional
        // runtime value, which the VM preserves separately.
        // Mixed int/float operands still promote the int side to float.
        let mixed = both_numeric && lhs_hir.ty != rhs_hir.ty;

        // String `+` is concatenation (series-capable, na-propagating). The
        // other arithmetic operators are numeric-only; Pine has no lexicographic
        // string ordering, so `< <= > >=` stay numeric-only too (they fall
        // through to `None` → E0207 for strings).
        let both_string = lhs_hir.ty == Type::String && rhs_hir.ty == Type::String;

        let result_ty = match hir_op {
            BinOp::Add if both_string => Some(Type::String),
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                if !both_numeric {
                    None
                } else if lhs_hir.ty == Type::Int && rhs_hir.ty == Type::Int {
                    // Static int arithmetic is int, including `/`. The
                    // quotient opcode retains any fractional runtime value.
                    Some(Type::Int)
                } else {
                    Some(Type::Float)
                }
            }
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => both_numeric.then_some(Type::Bool),
            BinOp::Eq | BinOp::Ne => {
                // Same-type equality over int/float/bool/string/color and over
                // matching enum types; mixed numeric equality promotes the int
                // side. Two distinct enum types never compare (nominal typing).
                if both_numeric
                    || (lhs_hir.ty.is_plot_display() && rhs_hir.ty.is_plot_display())
                    || (lhs_hir.ty == rhs_hir.ty
                        && matches!(
                            lhs_hir.ty,
                            Type::Bool
                                | Type::String
                                | Type::TextFormat
                                | Type::Color
                                | Type::Enum(_)
                        ))
                {
                    Some(Type::Bool)
                } else {
                    None
                }
            }
            BinOp::And | BinOp::Or => {
                (lhs_hir.ty == Type::Bool && rhs_hir.ty == Type::Bool).then_some(Type::Bool)
            }
        };

        match result_ty {
            Some(ty) => {
                // A logical operator whose rhs desugared into statements needs
                // real control flow to short-circuit them (Pine evaluates the
                // rhs lazily). The pure-rhs case stays a plain `Binary`, which
                // the CSIL `JUMP_IF_EXACT_*` guard short-circuits at lowering time.
                if logical && !rhs_lazy_stmts.is_empty() {
                    return self.short_circuit_logical(
                        hir_op,
                        lhs_hir,
                        rhs_hir,
                        rhs_lazy_stmts,
                        span,
                    );
                }
                if mixed {
                    lhs_hir = Self::coerce_int_to_float(lhs_hir);
                    rhs_hir = Self::coerce_int_to_float(rhs_hir);
                }
                // Color equality is series-qualified even for literal colors.
                // Keep that boundary through aliases and ternaries: otherwise
                // a theme-color comparison can incorrectly feed an input-only
                // display argument such as hline's color.
                let qualifier = if matches!(hir_op, BinOp::Eq | BinOp::Ne)
                    && lhs_hir.ty == Type::Color
                    && rhs_hir.ty == Type::Color
                {
                    Qualifier::Series
                } else {
                    lhs_hir.qualifier.join(rhs_hir.qualifier)
                };
                HirExpr {
                    kind: HirExprKind::Binary {
                        op: hir_op,
                        lhs: Box::new(lhs_hir),
                        rhs: Box::new(rhs_hir),
                    },
                    ty,
                    qualifier,
                    span,
                }
            }
            None => {
                self.error(
                    codes::E0207,
                    format!(
                        "operator `{}` cannot be applied to {} and {}",
                        hir_op.symbol(),
                        lhs_hir.ty.describe(),
                        rhs_hir.ty.describe()
                    ),
                    span,
                    "operand types do not match this operator",
                );
                self.error_expr(span)
            }
        }
    }

    /// Short-circuit a logical `and`/`or` whose rhs desugared into statements
    /// (an if/switch-expression chain, or an inlined UDF's leading statements
    /// and argument temps). Pine evaluates the rhs lazily, so those side
    /// effects must run only when the lhs does not decide the result — the
    /// plain-`Binary` + CSIL `JUMP_IF_EXACT_*` guard only skips the rhs *value*,
    /// not statements the semantics layer hoisted ahead of it.
    ///
    /// Evaluate the lhs once into a hidden slot `L`, then build a real branch:
    /// - `or`:  `if L == true  { R = true }  else { <rhs stmts>; R = L or rhs }`
    /// - `and`: `if L == false { R = false } else { <rhs stmts>; R = L and rhs }`
    ///
    /// and read `R`. The condition rides the `if`'s na-is-false rule: an `na`
    /// lhs is not equal to the decisive literal, so it takes the else branch
    /// where the eager `BOOL_*` combine restores the strong-Kleene result
    /// (`na or true = true`, `na and false = false`). The else-branch combine is
    /// itself a pure-operand `Binary`, so its own CSIL guard is a harmless no-op
    /// there (the lhs is known non-decisive on that path).
    pub(crate) fn short_circuit_logical(
        &mut self,
        op: BinOp,
        lhs_hir: HirExpr,
        rhs_hir: HirExpr,
        mut rhs_stmts: Vec<HirStmt>,
        span: Span,
    ) -> HirExpr {
        let qualifier = lhs_hir
            .qualifier
            .join(rhs_hir.qualifier)
            .join(Qualifier::Series);
        // `L`: evaluate the lhs exactly once (it may carry per-site `ta.*`
        // state, so it must not be duplicated into both the test and combine).
        let l_slot = self.allocate_synthetic_slot(Type::Bool, span);
        let l_sym = SymbolId(l_slot.0);
        self.pending_stmts.push(HirStmt::Assign {
            symbol: l_sym,
            value: lhs_hir,
            init: false,
            span,
        });
        let read_l = || HirExpr {
            kind: HirExprKind::ReadVar {
                symbol: l_sym,
                offset: 0,
            },
            ty: Type::Bool,
            qualifier: Qualifier::Series,
            span,
        };
        let bool_lit = |value: bool| HirExpr {
            kind: HirExprKind::ConstBool(value),
            ty: Type::Bool,
            qualifier: Qualifier::Const,
            span,
        };
        // `or` short-circuits to `true` when `L == true`; `and` to `false` when
        // `L == false`.
        let decisive = matches!(op, BinOp::Or);
        let r_slot = self.allocate_synthetic_slot(Type::Bool, span);
        let r_sym = SymbolId(r_slot.0);
        let cond = HirExpr {
            kind: HirExprKind::Binary {
                op: BinOp::Eq,
                lhs: Box::new(read_l()),
                rhs: Box::new(bool_lit(decisive)),
            },
            ty: Type::Bool,
            qualifier: Qualifier::Series,
            span,
        };
        let then_branch = vec![HirStmt::Assign {
            symbol: r_sym,
            value: bool_lit(decisive),
            init: false,
            span,
        }];
        // else: run the hoisted rhs statements, then the Kleene combine.
        let combine = HirExpr {
            kind: HirExprKind::Binary {
                op,
                lhs: Box::new(read_l()),
                rhs: Box::new(rhs_hir),
            },
            ty: Type::Bool,
            qualifier,
            span,
        };
        rhs_stmts.push(HirStmt::Assign {
            symbol: r_sym,
            value: combine,
            init: false,
            span,
        });
        self.pending_stmts.push(HirStmt::If {
            cond,
            then_branch,
            else_branch: rhs_stmts,
            span,
        });
        HirExpr {
            kind: HirExprKind::ReadVar {
                symbol: r_sym,
                offset: 0,
            },
            ty: Type::Bool,
            qualifier,
            span,
        }
    }

    /// The ternary conditional `cond ? a : b`. The condition always expects
    /// bool (an `na` literal condition types as bool, like the logical
    /// operators). The branch types unify: identical types stand; a mixed
    /// int/float pair coerces the int side to float; an `na` literal branch
    /// adopts the other branch's type (when both branches are `na`, the
    /// surrounding context's expected type applies, like a parenthesized
    /// `na`); anything else is `E0207`. The result qualifier is the join of
    /// the condition and both branches.
    ///
    /// Evaluation is **lazy**, as in Pine. Arm-local statements produced
    /// by inlining or expression desugaring stay under the same guard as the
    /// return value. Prefix-free ternaries retain the direct expression form;
    /// MIR lowers their non-literal arms to real branch blocks.
    /// Series-materializing constructs in an arm under a series condition
    /// therefore skip bars and warn `W0004`, like `if`/`switch` expression
    /// branches. A literal-bound integer comparison
    /// guarding `ta.valuewhen` is resolved at compile time when the other
    /// arm would otherwise have an invalid negative occurrence.
    pub(crate) fn analyze_ternary(
        &mut self,
        cond: &ast::Expr,
        then_e: &ast::Expr,
        else_e: &ast::Expr,
        span: Span,
        expected: Option<&Type>,
    ) -> HirExpr {
        let cond_hir = self.analyze_expr_expecting(cond, Some(&Type::Bool));
        if let Some(selected) = self.ternary_selected_arm(cond, &cond_hir, then_e, else_e) {
            return self.analyze_selected_ternary_arm(&cond_hir, selected, span, expected);
        }
        let series_condition = cond_hir.ty == Type::Bool && cond_hir.qualifier == Qualifier::Series;

        // An `na` literal branch adopts the other branch's type, mirroring
        // the binary-operand rule, so a lone `na` arm is analyzed second; two
        // `na` branches fall back to the outer expectation (E0220 on each side
        // when there is none).
        //
        // A nested ternary recurses through this function once per level, so
        // it holds only the condition and the two arms, in one pair of arm
        // slots for every case:
        // everything after them is in `finish_ternary`, whose frame is gone
        // before the next level.
        let then_na = Self::na_literal_span(then_e).is_some();
        let else_na = Self::na_literal_span(else_e).is_some();
        let else_first = then_na && !else_na;
        let (first_e, second_e) = if else_first {
            (else_e, then_e)
        } else {
            (then_e, else_e)
        };
        let outer = if then_na && else_na { expected } else { None };
        let first = self.analyze_ternary_arm(first_e, outer, series_condition);
        let second_expected = if then_na != else_na {
            Some(&first.0.ty)
        } else {
            outer
        };
        let second = self.analyze_ternary_arm(second_e, second_expected, series_condition);
        self.finish_ternary(cond, cond_hir, first, second, else_first, span)
    }

    /// The arm a literal-bound integer comparison selects when the else arm
    /// is a `ta.valuewhen` call, if any.
    ///
    /// A helper parameter bound to a literal can make an integer equality
    /// decidable at this call site (e.g. `iter == 0`). Pine evaluates only
    /// the selected ternary arm. Avoid analyzing an unreachable stateful
    /// call whose simple argument would otherwise be invalid (`iter - 1`
    /// becomes -1 for the zero case).
    /// A symbol may also carry an input.int default in `const_int`; that
    /// value is host-overridable and must never freeze a ternary arm.
    #[inline(never)]
    fn ternary_selected_arm<'e>(
        &self,
        cond: &ast::Expr,
        cond_hir: &HirExpr,
        then_e: &'e ast::Expr,
        else_e: &'e ast::Expr,
    ) -> Option<&'e ast::Expr> {
        let guarded_valuewhen = matches!(
            else_e,
            ast::Expr::Call { callee, .. } if callee.name == "ta.valuewhen"
        );
        match cond {
            ast::Expr::Binary { op, lhs, rhs, .. }
                if guarded_valuewhen
                    && cond_hir.qualifier == Qualifier::Const
                    && matches!(op, ast::BinaryOp::Eq | ast::BinaryOp::Ne) =>
            {
                self.ast_const_int(lhs)
                    .zip(self.ast_const_int(rhs))
                    .map(|(lhs, rhs)| {
                        let equal = lhs == rhs;
                        if equal == (*op == ast::BinaryOp::Eq) {
                            then_e
                        } else {
                            else_e
                        }
                    })
            }
            _ => None,
        }
    }

    /// Analyze only the arm `ternary_selected_arm` chose; the ternary takes
    /// its value with the condition's qualifier joined in. This recurses into
    /// the arm, but is rare enough that its frame is kept apart from
    /// `analyze_ternary`'s.
    #[inline(never)]
    fn analyze_selected_ternary_arm(
        &mut self,
        cond_hir: &HirExpr,
        selected: &ast::Expr,
        span: Span,
        expected: Option<&Type>,
    ) -> HirExpr {
        if cond_hir.ty.is_error() || cond_hir.ty != Type::Bool {
            return self.error_expr(span);
        }
        self.ternary_operand_depth += 1;
        let mut branch = self.analyze_expr_in_conditional_context(
            selected,
            expected,
            ConditionalStatefulContext::Ternary,
            false,
        );
        self.ternary_operand_depth -= 1;
        branch.qualifier = branch.qualifier.join(cond_hir.qualifier);
        branch.span = span;
        branch
    }

    /// Check and unify an analyzed ternary's condition and arms, and build
    /// its expression. The arms come in the order they were analyzed:
    /// `else_first` when the else arm went first. Kept out of
    /// `analyze_ternary`, which is on the stack once per nesting level.
    #[inline(never)]
    fn finish_ternary(
        &mut self,
        cond: &ast::Expr,
        cond_hir: HirExpr,
        first: (HirExpr, Vec<HirStmt>),
        second: (HirExpr, Vec<HirStmt>),
        else_first: bool,
        span: Span,
    ) -> HirExpr {
        let ((mut then_hir, then_stmts), (mut else_hir, else_stmts)) = if else_first {
            (second, first)
        } else {
            (first, second)
        };
        if !cond_hir.ty.is_error() && cond_hir.ty != Type::Bool {
            self.error(
                codes::E0207,
                format!(
                    "`?:` condition must be bool, found {}",
                    cond_hir.ty.describe()
                ),
                cond.span(),
                "expected a boolean expression",
            );
            return self.error_expr(span);
        }
        if cond_hir.ty.is_error() || then_hir.ty.is_error() || else_hir.ty.is_error() {
            return self.error_expr(span);
        }

        // Unify the branch types.
        let ty = if then_hir.ty == else_hir.ty {
            then_hir.ty.clone()
        } else if then_hir.ty.is_plot_display() && else_hir.ty.is_plot_display() {
            Type::PlotDisplay
        } else if is_numeric(&then_hir.ty) && is_numeric(&else_hir.ty) {
            // Mixed int/float: the int side widens to float.
            then_hir = Self::coerce_int_to_float(then_hir);
            else_hir = Self::coerce_int_to_float(else_hir);
            Type::Float
        } else {
            self.error(
                codes::E0207,
                format!(
                    "`?:` branches have mismatched types: {} and {}",
                    then_hir.ty.describe(),
                    else_hir.ty.describe()
                ),
                span,
                "both branches must produce the same type",
            );
            return self.error_expr(span);
        };

        if !then_stmts.is_empty() || !else_stmts.is_empty() {
            return self.finish_if_expr(
                cond_hir,
                then_stmts,
                then_hir,
                else_stmts,
                Some(else_hir),
                span,
            );
        }

        let qualifier = cond_hir
            .qualifier
            .join(then_hir.qualifier)
            .join(else_hir.qualifier);
        HirExpr {
            kind: HirExprKind::Ternary {
                cond: Box::new(cond_hir),
                then_e: Box::new(then_hir),
                else_e: Box::new(else_hir),
            },
            ty,
            qualifier,
            span,
        }
    }

    /// Capture all statements generated by one arm without flushing them
    /// into the surrounding expression's prefix. The caller's prefix,
    /// including condition evaluation, remains in its original order.
    fn analyze_ternary_arm(
        &mut self,
        expr: &ast::Expr,
        expected: Option<&Type>,
        series_condition: bool,
    ) -> (HirExpr, Vec<HirStmt>) {
        let saved = std::mem::take(&mut self.pending_stmts);
        self.ternary_operand_depth += 1;
        let value = self.analyze_expr_in_conditional_context(
            expr,
            expected,
            ConditionalStatefulContext::Ternary,
            series_condition,
        );
        self.ternary_operand_depth -= 1;
        let statements = std::mem::replace(&mut self.pending_stmts, saved);
        (value, statements)
    }

    /// Analyze one branch/arm **value block**: the block's leading statements
    /// (executed for effect, in a fresh scope) plus a trailing expression or
    /// assignment that supplies the branch's value. A final variable
    /// declaration or reassignment supplies its assigned value, and a final
    /// counted loop can supply the value of its last assignment. Returns the
    /// block's HIR statements and the analyzed value expression. The value
    /// expression's `na` literal (if any) adopts `expected`. Returns `None`
    /// after reporting when the block is empty or has an unsupported trailing
    /// statement (`E0227`).
    ///
    /// Nested `if`/`switch`-expression desugaring produced while analyzing the
    /// block is flushed *inside* the returned block (before each statement),
    /// never leaked to the caller.
    pub(crate) fn analyze_value_block(
        &mut self,
        stmts: &[ast::Stmt],
        block_span: Span,
        expected: Option<&Type>,
    ) -> Option<(Vec<HirStmt>, HirExpr)> {
        // A tuple tail is legal in a function or conditional value block.
        // Their callers set the flag for this block's return position.
        // Capture it here and reapply it *only* around the tail expression, so
        // leading statements and nested analysis never see a stray permission.
        let allow_tuple_tail = std::mem::take(&mut self.allow_tuple_expr);
        let defer_tuple_na_tail = std::mem::take(&mut self.defer_tuple_na);
        let Some((last, leading)) = stmts.split_last() else {
            self.error(
                codes::E0227,
                "this branch is empty; it must end with a value expression",
                block_span,
                "add an expression to supply the branch value",
            );
            return None;
        };
        if let ast::Stmt::For(for_stmt) = last {
            self.scopes.push(HashMap::new());
            let mut block = Vec::new();
            for stmt in leading {
                self.analyze_stmt_into(stmt, &mut block);
            }
            let loop_result = self.analyze_for_inner(for_stmt, true);
            self.scopes.pop();
            if let Some((loop_stmt, Some(value))) = loop_result {
                let HirExprKind::ReadVar { symbol, .. } = &value.kind else {
                    unreachable!("counted-loop result must read its synthetic slot")
                };
                block.push(HirStmt::Assign {
                    symbol: *symbol,
                    value: HirExpr {
                        kind: HirExprKind::ConstNa,
                        ty: value.ty.clone(),
                        qualifier: Qualifier::Const,
                        span: last.span(),
                    },
                    init: false,
                    span: last.span(),
                });
                block.push(loop_stmt);
                return Some((block, value));
            }
            self.error(
                codes::E0227,
                "the last statement of a value branch must produce a value",
                last.span(),
                "give the loop body a final value expression",
            );
            return None;
        }
        // The value comes from the block's last statement: an expression
        // statement, the value stored by an assignment, or a nested `if`/
        // `switch` written in statement position. An assignment must execute
        // before its target is read, including declarations local to the block.
        let assignment_tail = matches!(last, ast::Stmt::Assign { .. } | ast::Stmt::Reassign { .. });
        let value_expr: ast::Expr = match last {
            ast::Stmt::Expr { expr, .. } => expr.clone(),
            // `_` is never read, so a discard has no value to give.
            ast::Stmt::Assign { name, .. } if name.name == DISCARD_NAME => {
                self.error(
                    codes::E0227,
                    "a declaration of `_` does not produce a value",
                    last.span(),
                    "end the block with an expression, or call the helper as a statement",
                );
                self.scopes.push(HashMap::new());
                let mut throwaway = Vec::new();
                for stmt in stmts {
                    self.analyze_stmt_into(stmt, &mut throwaway);
                }
                self.scopes.pop();
                return None;
            }
            ast::Stmt::Assign { name, .. } | ast::Stmt::Reassign { name, .. } => {
                ast::Expr::Ident(name.clone())
            }
            ast::Stmt::If(if_stmt) => ast::Expr::If(Box::new(ast::IfExpr {
                cond: if_stmt.cond.clone(),
                then_branch: if_stmt.then_branch.clone(),
                else_branch: if_stmt.else_branch.clone(),
                span: if_stmt.span,
            })),
            ast::Stmt::Switch(switch) => ast::Expr::Switch(Box::new(switch.clone())),
            _ => {
                self.error(
                    codes::E0227,
                    "the last statement of a value branch must be an expression",
                    last.span(),
                    "this statement does not produce a value",
                );
                // Still analyze the whole block so inner errors surface.
                self.scopes.push(HashMap::new());
                let mut throwaway = Vec::new();
                for stmt in stmts {
                    self.analyze_stmt_into(stmt, &mut throwaway);
                }
                self.scopes.pop();
                return None;
            }
        };

        self.scopes.push(HashMap::new());
        let mut block = Vec::new();
        for stmt in leading {
            self.analyze_stmt_into(stmt, &mut block);
        }
        if assignment_tail {
            self.analyze_stmt_into(last, &mut block);
        }
        // The value expression may itself desugar nested if/switch-expressions;
        // capture their pending statements into this block, ahead of the read.
        let saved = std::mem::take(&mut self.pending_stmts);
        self.allow_tuple_expr = allow_tuple_tail;
        self.defer_tuple_na = defer_tuple_na_tail;
        let value = self.analyze_expr_expecting(&value_expr, expected);
        self.defer_tuple_na = false;
        block.append(&mut self.pending_stmts);
        self.pending_stmts = saved;
        self.scopes.pop();
        Some((block, value))
    }

    /// [`Self::analyze_value_block`] wrapped in a conditional-branch context
    /// (`branch_depth`), for `if`/`switch` **expression** branches — whose
    /// taken-branch-only evaluation makes stateful constructs warn `W0004`.
    /// (Function bodies call `analyze_value_block` directly: an inlined body is
    /// only conditional when the *call site* is, which the surrounding
    /// `branch_depth` already reflects.)
    pub(crate) fn analyze_branch_value_block(
        &mut self,
        stmts: &[ast::Stmt],
        block_span: Span,
        expected: Option<&Type>,
        control_qualifier: Qualifier,
        request_control: bool,
    ) -> Option<(Vec<HirStmt>, HirExpr)> {
        self.assignment_controls.push(control_qualifier);
        if request_control {
            self.request_branch_depth += 1;
        }
        let series_control = control_qualifier == Qualifier::Series;
        if series_control {
            self.branch_depth += 1;
        }
        // Conditional local blocks may return tuples. The enclosing value
        // consumer still decides whether a tuple is usable there.
        self.allow_tuple_expr = true;
        let saved_defer = self.defer_tuple_na;
        self.defer_tuple_na = true;
        let result = self.analyze_value_block(stmts, block_span, expected);
        self.defer_tuple_na = saved_defer;
        if series_control {
            self.branch_depth -= 1;
        }
        if request_control {
            self.request_branch_depth -= 1;
        }
        self.assignment_controls.pop();
        result
    }

    /// True when the trailing value of a branch is (parenthesized) `na` —
    /// mirrors [`Self::na_literal_span`] but reaches into the block's last
    /// statement. A trailing `if`/`switch` is never a bare `na`.
    pub(crate) fn value_block_is_na(stmts: &[ast::Stmt]) -> bool {
        match stmts.last() {
            Some(ast::Stmt::Expr { expr, .. }) => Self::na_literal_span(expr).is_some(),
            _ => false,
        }
    }

    /// `if cond ... else ...` used as a value. Desugars to a hidden
    /// non-persistent result slot: each branch stores its (type-unified)
    /// value into the slot, and the expression reads the slot at offset 0.
    /// The `HirStmt::If` that performs the stores is pushed to
    /// `pending_stmts` so it runs before the enclosing statement.
    ///
    /// Branch types unify like the ternary: identical types stand, a mixed
    /// int/float pair coerces the int side to float, an `na`-literal branch
    /// adopts the other's type, and a missing `else` yields `na` of the
    /// then-branch type. Evaluation is **lazy** — only the taken branch's
    /// value (and side effects) run, like a ternary.
    pub(crate) fn analyze_if_expr(
        &mut self,
        if_expr: &ast::IfExpr,
        expected: Option<&Type>,
    ) -> HirExpr {
        let span = if_expr.span;
        let cond_hir = self.analyze_expr_expecting(&if_expr.cond, Some(&Type::Bool));
        let control_qualifier = cond_hir.qualifier;
        let request_control = self.unresolved_request_control(&cond_hir);
        if !cond_hir.ty.is_error() && cond_hir.ty != Type::Bool {
            self.error(
                codes::E0208,
                format!(
                    "`if` condition must be bool, found {}",
                    cond_hir.ty.describe()
                ),
                if_expr.cond.span(),
                "expected a boolean expression",
            );
        }

        // An `na`-literal branch adopts the other branch's type (like the
        // ternary). A missing `else` behaves as an `na` else-branch of the
        // then-branch's type.
        let then_na = Self::value_block_is_na(&if_expr.then_branch);
        let else_na = if_expr
            .else_branch
            .as_ref()
            .is_none_or(|b| Self::value_block_is_na(b));

        let then_span = if_expr.cond.span().to(span);
        let (then_block, mut then_val) = match (then_na, else_na) {
            // then is na, else is a real value: analyze else first, use its
            // type as the then-branch expectation.
            (true, false) => {
                let else_stmts = if_expr.else_branch.as_ref().expect("else present");
                let (else_block, else_val) = match self.analyze_branch_value_block(
                    else_stmts,
                    span,
                    None,
                    control_qualifier,
                    request_control,
                ) {
                    Some(pair) => pair,
                    None => return self.error_expr(span),
                };
                let expect_ty = else_val.ty.clone();
                let (then_block, then_val) = match self.analyze_branch_value_block(
                    &if_expr.then_branch,
                    then_span,
                    Some(&expect_ty),
                    control_qualifier,
                    request_control,
                ) {
                    Some(pair) => pair,
                    None => return self.error_expr(span),
                };
                return self.finish_if_expr(
                    cond_hir,
                    then_block,
                    then_val,
                    else_block,
                    Some(else_val),
                    span,
                );
            }
            _ => match self.analyze_branch_value_block(
                &if_expr.then_branch,
                then_span,
                expected,
                control_qualifier,
                request_control,
            ) {
                Some(pair) => pair,
                None => return self.error_expr(span),
            },
        };

        // The else-branch: a real block, or a synthesized `na` of the
        // then-branch type when omitted.
        let (else_block, else_val) = match &if_expr.else_branch {
            Some(else_stmts) => {
                let expect_ty = if then_val.ty.is_error() {
                    None
                } else {
                    Some(then_val.ty.clone())
                };
                match self.analyze_branch_value_block(
                    else_stmts,
                    span,
                    expect_ty.as_ref(),
                    control_qualifier,
                    request_control,
                ) {
                    Some(pair) => (pair.0, Some(pair.1)),
                    None => return self.error_expr(span),
                }
            }
            None => {
                // Missing else => false for bool, na otherwise. When the then
                // value itself is `na`, fall back to the outer expectation.
                let na_ty = if then_na {
                    match expected {
                        Some(ty) if !ty.is_error() => ty.clone(),
                        _ => {
                            // Both sides untyped `na`: report E0220 on the
                            // then value's span like a bare `na`.
                            let _ = self.analyze_na_literal(then_val.span, None);
                            // Ensure then_val carries the error so the cascade
                            // is suppressed downstream.
                            then_val = self.error_expr(then_val.span);
                            Type::Error
                        }
                    }
                } else {
                    then_val.ty.clone()
                };
                if na_ty.is_error() {
                    return self.error_expr(span);
                }
                let na = if matches!(then_val.kind, HirExprKind::Tuple { .. }) {
                    Self::missing_tuple_value(&then_val, span)
                } else {
                    HirExpr {
                        kind: if na_ty == Type::Bool {
                            HirExprKind::ConstBool(false)
                        } else {
                            HirExprKind::ConstNa
                        },
                        ty: na_ty.clone(),
                        qualifier: Qualifier::Const,
                        span,
                    }
                };
                (Vec::new(), Some(na))
            }
        };
        self.finish_if_expr(cond_hir, then_block, then_val, else_block, else_val, span)
    }

    /// Complete an if-expression desugar: unify the branch value types, emit
    /// the per-branch store into a fresh hidden result slot, push the
    /// `HirStmt::If` to `pending_stmts`, and return the slot read.
    pub(crate) fn finish_if_expr(
        &mut self,
        cond_hir: HirExpr,
        then_block: Vec<HirStmt>,
        then_val: HirExpr,
        else_block: Vec<HirStmt>,
        else_val: Option<HirExpr>,
        span: Span,
    ) -> HirExpr {
        let else_val = else_val.expect("if-expression always has an else value (na when omitted)");
        if cond_hir.ty.is_error() || then_val.ty.is_error() || else_val.ty.is_error() {
            return self.error_expr(span);
        }
        if matches!(then_val.kind, HirExprKind::Tuple { .. })
            || matches!(else_val.kind, HirExprKind::Tuple { .. })
        {
            let mut branches = vec![(then_block, then_val), (else_block, else_val)];
            let Some(value) =
                self.prepare_tuple_value_blocks(&mut branches, cond_hir.qualifier, span)
            else {
                return self.error_expr(span);
            };
            let (else_branch, _) = branches.pop().expect("else branch");
            let (then_branch, _) = branches.pop().expect("then branch");
            self.pending_stmts.push(HirStmt::If {
                cond: cond_hir,
                then_branch,
                else_branch,
                span,
            });
            return value;
        }
        let Some((result_ty, then_val, else_val)) =
            self.unify_branch_values(then_val, else_val, span)
        else {
            return self.error_expr(span);
        };

        let qualifier = cond_hir
            .qualifier
            .join(then_val.qualifier)
            .join(else_val.qualifier);
        let slot = self.allocate_synthetic_slot(result_ty.clone(), span);
        let slot_symbol = SymbolId(slot.0);
        // The hidden slot is an implementation detail, not inherently a
        // series value. An input/simple-controlled expression with
        // input/simple branches stays input/simple; a dynamic condition or
        // branch already promotes the join to `Series`.
        self.symbols[slot_symbol.0 as usize].qualifier = qualifier;

        self.temporal_binding(slot_symbol, &cond_hir);
        self.temporal_binding(slot_symbol, &then_val);
        self.temporal_binding(slot_symbol, &else_val);
        let branches = crate::branch_prefixes::finish_branches(
            then_block,
            HirStmt::Assign {
                symbol: slot_symbol,
                value: then_val,
                init: false,
                span,
            },
            else_block,
            HirStmt::Assign {
                symbol: slot_symbol,
                value: else_val,
                init: false,
                span,
            },
        );
        self.pending_stmts.push(HirStmt::If {
            cond: cond_hir,
            then_branch: branches.then_branch,
            else_branch: branches.else_branch,
            span,
        });
        HirExpr {
            kind: HirExprKind::ReadVar {
                symbol: slot_symbol,
                offset: 0,
            },
            ty: result_ty,
            qualifier,
            span,
        }
    }

    /// `switch ...` used as a value. Desugars to a hidden result slot filled
    /// by an if/else-if chain (one arm per case, the default or an
    /// unmatched-`na` store as the tail else), then reads the slot. The
    /// subject (when present) is evaluated once into its own hidden slot. Arm
    /// values unify like the if-expression's branches; an unmatched switch
    /// yields `na`.
    pub(crate) fn analyze_switch_expr(
        &mut self,
        switch: &ast::SwitchStmt,
        expected: Option<&Type>,
    ) -> HirExpr {
        let span = switch.span;
        let subject_slot = match &switch.subject {
            Some(subject) => match self.eval_switch_subject(subject) {
                Some(s) => Some(s),
                None => {
                    self.analyze_switch_arms_for_errors(switch);
                    return self.error_expr(span);
                }
            },
            None => None,
        };

        // Analyze each arm: condition (or default) + value block. Track which
        // arm bodies are `na`-literal so the unified type can be adopted.
        struct ArmData {
            cond: Option<HirExpr>,
            is_default: bool,
            block: Vec<HirStmt>,
            value: HirExpr,
            is_na: bool,
        }
        let mut arms: Vec<ArmData> = Vec::new();
        let mut seen_default = false;
        let mut has_default = false;
        let mut ok = true;
        let mut control_qualifier = Self::initial_switch_control_qualifier(
            switch,
            subject_slot.as_ref().map(|(_, _, qualifier)| *qualifier),
        );
        let mut request_control = false;
        for arm in &switch.arms {
            let is_default = arm.label.is_none();
            // An arm condition is tested only when the earlier arms fail; its
            // pending statements run before the whole switch.
            let cond = self.without_arg_order_captures(|this| {
                this.switch_arm_condition(subject_slot.clone(), arm, &mut seen_default)
            });
            if is_default {
                has_default = true;
            } else if cond.is_none() {
                // A labeled arm whose condition failed to type.
                let _ = self.analyze_branch_value_block(
                    &arm.body,
                    arm.span,
                    None,
                    control_qualifier,
                    request_control,
                );
                ok = false;
                continue;
            }
            if let Some(condition) = &cond {
                control_qualifier = control_qualifier.join(condition.qualifier);
                request_control |= self.unresolved_request_control(condition);
            }
            let is_na = Self::value_block_is_na(&arm.body);
            // An `na`-literal arm's value is discarded (a `ConstNa` of the
            // unified type is materialized later); analyze it with a dummy
            // float expectation only to suppress a premature `E0220`, and to
            // run the arm's leading statements.
            let expect = if is_na { Some(&Type::Float) } else { None };
            let Some((block, value)) = self.analyze_branch_value_block(
                &arm.body,
                arm.span,
                expect,
                control_qualifier,
                request_control,
            ) else {
                ok = false;
                continue;
            };
            arms.push(ArmData {
                cond,
                is_default,
                block,
                value,
                is_na,
            });
        }
        if !ok {
            return self.error_expr(span);
        }

        if arms
            .iter()
            .any(|arm| matches!(arm.value.kind, HirExprKind::Tuple { .. }))
        {
            let mut qualifier = Qualifier::Const;
            let mut conds = Vec::new();
            let mut branches = Vec::new();
            let mut default = None;
            for arm in arms {
                if arm.is_default {
                    default = Some((arm.block, arm.value));
                } else {
                    let cond = arm.cond.expect("labeled arm has a condition");
                    qualifier = qualifier.join(cond.qualifier);
                    conds.push(cond);
                    branches.push((arm.block, arm.value));
                }
            }
            let default = default.unwrap_or_else(|| {
                let value = branches
                    .iter()
                    .find(|(_, value)| matches!(value.kind, HirExprKind::Tuple { .. }))
                    .expect("tuple arm");
                (Vec::new(), Self::missing_tuple_value(&value.1, span))
            });
            branches.push(default);
            let Some(value) = self.prepare_tuple_value_blocks(&mut branches, qualifier, span)
            else {
                return self.error_expr(span);
            };
            let (tail_else, _) = branches.pop().expect("default branch");
            let bodies = branches.into_iter().map(|(block, _)| block).collect();
            self.pending_stmts
                .push(Self::fold_switch_chain(conds, bodies, tail_else, span));
            return value;
        }

        // Unify the value types across all non-`na` arms (plus the outer
        // expectation as a fallback for the all-`na` / unmatched-`na` case).
        let mut unified: Option<Type> = None;
        for arm in &arms {
            if arm.is_na || arm.value.ty.is_error() {
                continue;
            }
            unified = Some(match unified {
                None => arm.value.ty.clone(),
                Some(prev) => match Self::unify_two_types(&prev, &arm.value.ty) {
                    Some(t) => t,
                    None => {
                        self.error(
                            codes::E0207,
                            format!(
                                "`switch` arms have mismatched types: {} and {}",
                                prev.describe(),
                                arm.value.ty.describe()
                            ),
                            span,
                            "all arms must produce the same type",
                        );
                        return self.error_expr(span);
                    }
                },
            });
        }
        let result_ty = match unified.or_else(|| expected.filter(|t| !t.is_error()).cloned()) {
            Some(t) => t,
            None => {
                // Every arm is `na` and there is no outer expectation: the
                // value has no type (like a bare `na`).
                self.error(
                    codes::E0220,
                    "`switch` value has no type; every arm is `na`",
                    span,
                    "add a type annotation or give an arm a typed value",
                );
                return self.error_expr(span);
            }
        };
        if result_ty.is_text_format() && !arms.iter().any(|arm| arm.is_default) {
            self.error(
                codes::E0220,
                "a text_format switch result requires an explicit default",
                span,
                "use text.format_none in the default arm",
            );
            return self.error_expr(span);
        }

        // Build the chain. Each arm stores its coerced value into the result
        // slot; the tail else stores the default arm's value or `na`.
        let slot = self.allocate_synthetic_slot(result_ty.clone(), span);
        let slot_symbol = SymbolId(slot.0);
        let mut qualifier = Qualifier::Const;

        let mut conds: Vec<HirExpr> = Vec::new();
        let mut bodies: Vec<Vec<HirStmt>> = Vec::new();
        let mut tail_else: Vec<HirStmt> = Vec::new();
        // Host-resolvable form of the whole switch, as `(cond, value)` pairs
        // plus the default. Any arm with statements of its own, or a part the
        // input expression language cannot carry, drops it.
        let mut active_arms = Some(Vec::new());
        let mut active_default: Option<InputActiveExpr> = None;
        let mut source_closed_active_origin = true;
        for arm in arms {
            let value = self.coerce_arm_value(arm.value, arm.is_na, &result_ty, span);
            self.temporal_binding(slot_symbol, &value);
            if let Some(condition) = &arm.cond {
                self.temporal_binding(slot_symbol, condition);
            }
            qualifier = qualifier.join(value.qualifier);
            if arm.block.is_empty() {
                source_closed_active_origin &= self.source_closed_active_expr_is_proven(&value);
                let active_value = self.input_active_expr_from_hir(&value);
                match (arm.is_default, &arm.cond, active_value) {
                    (true, _, Some(active_value)) => {
                        active_default.get_or_insert(active_value);
                    }
                    (false, Some(cond), Some(active_value)) => {
                        source_closed_active_origin &=
                            self.source_closed_active_expr_is_proven(cond);
                        let constant = self.source_constant_condition(cond);
                        let active_cond = self.input_active_expr_from_hir(cond);
                        match (active_arms.as_mut(), active_cond) {
                            (Some(pairs), Some(active_cond)) => {
                                pairs.push((constant, active_cond, active_value));
                            }
                            _ => active_arms = None,
                        }
                    }
                    _ => active_arms = None,
                }
            } else {
                active_arms = None;
                source_closed_active_origin = false;
            }
            let mut block = arm.block;
            block.push(HirStmt::Assign {
                symbol: slot_symbol,
                value,
                init: false,
                span,
            });
            if arm.is_default {
                if tail_else.is_empty() {
                    tail_else = block;
                }
            } else {
                let cond = arm.cond.expect("labeled arm has a condition");
                qualifier = qualifier.join(cond.qualifier);
                conds.push(cond);
                bodies.push(block);
            }
        }
        // No default arm: bool stores false; other types store missing.
        if !has_default {
            tail_else = vec![HirStmt::Assign {
                symbol: slot_symbol,
                value: HirExpr {
                    kind: if result_ty == Type::Bool {
                        HirExprKind::ConstBool(false)
                    } else {
                        HirExprKind::ConstNa
                    },
                    ty: result_ty.clone(),
                    qualifier: Qualifier::Const,
                    span,
                },
                init: false,
                span,
            }];
        }

        let chain = Self::fold_switch_chain(conds, bodies, tail_else, span);
        self.observe_temporal_pending(std::slice::from_ref(&chain));
        self.pending_stmts.push(chain);
        self.symbols[slot_symbol.0 as usize].qualifier = qualifier;
        // A switch fixed for the run (`offset = switch alignInput => 0 => -1`)
        // gets the same host-resolvable origin a ternary has, so style scalars
        // and `active` arguments that read it bind instead of falling back.
        // A literal-backed Simple result also carries this input-free tree.
        // Opaque Simple host context still fails the separate source proof.
        if qualifier <= Qualifier::Input
            || (qualifier == Qualifier::Simple && source_closed_active_origin)
        {
            if let Some(pairs) = active_arms {
                let missing = if result_ty == Type::Bool {
                    InputActiveExpr::Value {
                        value: InputValue::Bool(false),
                    }
                } else {
                    InputActiveExpr::Na
                };
                let tail = if has_default {
                    active_default
                } else {
                    Some(missing)
                };
                if let Some(tail) = tail {
                    let origin = pairs.into_iter().rev().fold(
                        tail,
                        |when_false, (constant, condition, when_true)| match constant {
                            Some(true) => when_true,
                            Some(false) => when_false,
                            None => InputActiveExpr::Ternary {
                                condition: Box::new(condition),
                                when_true: Box::new(when_true),
                                when_false: Box::new(when_false),
                            },
                        },
                    );
                    self.input_active_origins.insert(slot_symbol, origin);
                    if source_closed_active_origin {
                        self.source_closed_active_origins.insert(slot_symbol);
                    }
                }
            }
        }
        HirExpr {
            kind: HirExprKind::ReadVar {
                symbol: slot_symbol,
                offset: 0,
            },
            ty: result_ty,
            qualifier,
            span,
        }
    }

    /// The unmatched path supplies one missing value per tuple component.
    /// A missing bool result is false, while other types retain `na`.
    fn missing_tuple_value(value: &HirExpr, span: Span) -> HirExpr {
        let HirExprKind::Tuple { elements } = &value.kind else {
            unreachable!("tuple shape required")
        };
        HirExpr {
            kind: HirExprKind::Tuple {
                elements: elements
                    .iter()
                    .map(|element| HirExpr {
                        kind: if element.ty == Type::Bool {
                            HirExprKind::ConstBool(false)
                        } else {
                            HirExprKind::ConstNa
                        },
                        ty: element.ty.clone(),
                        qualifier: Qualifier::Const,
                        span,
                    })
                    .collect(),
            },
            ty: Type::Void,
            qualifier: Qualifier::Const,
            span,
        }
    }

    /// Lower a conditional tuple to component slots filled by one selected
    /// block. Conditions and block effects must not be repeated per element.
    fn prepare_tuple_value_blocks(
        &mut self,
        branches: &mut [(Vec<HirStmt>, HirExpr)],
        mut qualifier: Qualifier,
        span: Span,
    ) -> Option<HirExpr> {
        let mut types: Option<Vec<Option<Type>>> = None;
        for (_, value) in branches.iter() {
            let HirExprKind::Tuple { elements } = &value.kind else {
                self.error(
                    codes::E0225,
                    "conditional branches must all return tuples of the same size",
                    value.span,
                    "a scalar branch cannot supply a tuple result",
                );
                return None;
            };
            qualifier = qualifier.join(value.qualifier);
            if elements.iter().any(|element| element.ty.is_error()) {
                return None;
            }
            if let Some(types) = &mut types {
                if types.len() != elements.len() {
                    self.error(
                        codes::E0225,
                        "conditional branches return tuples of different sizes",
                        value.span,
                        "every branch must return the same number of values",
                    );
                    return None;
                }
                for (ty, element) in types.iter_mut().zip(elements) {
                    if element.ty == Type::Unknown && matches!(element.kind, HirExprKind::ConstNa) {
                        continue;
                    }
                    let Some(previous) = ty.as_ref() else {
                        *ty = Some(element.ty.clone());
                        continue;
                    };
                    let Some(unified) = Self::unify_two_types(previous, &element.ty) else {
                        self.error(
                            codes::E0207,
                            "conditional tuple components have mismatched types",
                            element.span,
                            "corresponding components must have compatible types",
                        );
                        return None;
                    };
                    *ty = Some(unified);
                }
            } else {
                types = Some(
                    elements
                        .iter()
                        .map(|element| {
                            (!(element.ty == Type::Unknown
                                && matches!(element.kind, HirExprKind::ConstNa)))
                            .then(|| element.ty.clone())
                        })
                        .collect(),
                );
            }
        }
        let types = types.expect("at least one tuple branch");
        let mut resolved = Vec::with_capacity(types.len());
        for (index, ty) in types.into_iter().enumerate() {
            let Some(ty) = ty else {
                self.error(
                    codes::E0220,
                    "conditional tuple component has no type; every branch is `na`",
                    span,
                    format!("give component {} a typed value", index + 1),
                );
                return None;
            };
            if ty == Type::Bool
                && branches.iter().any(|(_, value)| {
                    let HirExprKind::Tuple { elements } = &value.kind else {
                        unreachable!("validated tuple branch")
                    };
                    elements[index].ty == Type::Unknown
                        && matches!(elements[index].kind, HirExprKind::ConstNa)
                })
            {
                self.error(
                    codes::E0207,
                    "conditional tuple component cannot combine `na` and bool",
                    span,
                    "boolean components must be true or false",
                );
                return None;
            }
            resolved.push(ty);
        }
        let types = resolved;
        let mut reads = Vec::with_capacity(types.len());
        for ty in &types {
            let slot = self.allocate_synthetic_slot(ty.clone(), span);
            let symbol = SymbolId(slot.0);
            self.symbols[symbol.0 as usize].qualifier = qualifier;
            reads.push(HirExpr {
                kind: HirExprKind::ReadVar { symbol, offset: 0 },
                ty: ty.clone(),
                qualifier,
                span,
            });
        }
        for (block, value) in branches {
            let HirExprKind::Tuple { elements } = &value.kind else {
                unreachable!("validated tuple branch")
            };
            for ((element, ty), read) in elements.iter().zip(&types).zip(&reads) {
                let HirExprKind::ReadVar { symbol, .. } = read.kind else {
                    unreachable!("component slot")
                };
                block.push(HirStmt::Assign {
                    symbol,
                    value: self.coerce_arm_value(
                        element.clone(),
                        element.ty == Type::Unknown && matches!(element.kind, HirExprKind::ConstNa),
                        ty,
                        span,
                    ),
                    init: false,
                    span,
                });
            }
        }
        Some(HirExpr {
            kind: HirExprKind::Tuple { elements: reads },
            ty: Type::Void,
            qualifier,
            span,
        })
    }

    /// Coerce one switch-arm value to the unified result type: an `na`-literal
    /// arm becomes `ConstNa` of the result type; an int value widens to float
    /// when the result is float.
    pub(crate) fn coerce_arm_value(
        &mut self,
        value: HirExpr,
        is_na: bool,
        result_ty: &Type,
        span: Span,
    ) -> HirExpr {
        if is_na {
            if result_ty.is_text_format() || result_ty.is_plot_display() {
                return self.analyze_na_literal(span, Some(result_ty));
            }
            return HirExpr {
                kind: HirExprKind::ConstNa,
                ty: result_ty.clone(),
                qualifier: Qualifier::Const,
                span,
            };
        }
        if *result_ty == Type::Float && value.ty == Type::Int {
            return Self::coerce_int_to_float(value);
        }
        value
    }

    /// Unify two concrete value types for a multi-arm value: identical types
    /// stand; an int/float pair widens to float; anything else fails.
    pub(crate) fn unify_two_types(a: &Type, b: &Type) -> Option<Type> {
        if a == b {
            Some(a.clone())
        } else if a.is_plot_display() && b.is_plot_display() {
            Some(Type::PlotDisplay)
        } else if is_numeric(a) && is_numeric(b) {
            Some(Type::Float)
        } else {
            None
        }
    }

    /// Unify two branch value types (int/float coerce, identical stand,
    /// otherwise `E0207`). Returns the unified type and the (possibly
    /// coerced) branch values.
    pub(crate) fn unify_branch_values(
        &mut self,
        mut then_val: HirExpr,
        mut else_val: HirExpr,
        span: Span,
    ) -> Option<(Type, HirExpr, HirExpr)> {
        if (then_val.ty.is_text_format() && matches!(then_val.kind, HirExprKind::ConstNa))
            || (else_val.ty.is_text_format() && matches!(else_val.kind, HirExprKind::ConstNa))
        {
            self.error(
                codes::E0220,
                "a text_format branch result cannot be absent",
                span,
                "provide an explicit text.format_none result",
            );
            return None;
        }
        let ty = if then_val.ty == else_val.ty {
            then_val.ty.clone()
        } else if then_val.ty.is_plot_display() && else_val.ty.is_plot_display() {
            Type::PlotDisplay
        } else if is_numeric(&then_val.ty) && is_numeric(&else_val.ty) {
            then_val = Self::coerce_int_to_float(then_val);
            else_val = Self::coerce_int_to_float(else_val);
            Type::Float
        } else {
            self.error(
                codes::E0207,
                format!(
                    "branches have mismatched types: {} and {}",
                    then_val.ty.describe(),
                    else_val.ty.describe()
                ),
                span,
                "all branches must produce the same type",
            );
            return None;
        };
        Some((ty, then_val, else_val))
    }
}

impl<'r> Analyzer<'r> {
    /// Value qualifiers do not say whether an expression can keep history.
    /// Computed Const/Input/Simple values use the same hidden slot as Series
    /// expressions. Direct literals and built-in constants/enum members are
    /// forbidden source targets even when a computed expression lowers to the
    /// same HIR constant. Chart-type context variables have an explicit
    /// history capability; grouping does not change the source distinction.
    fn reject_unmaterializable_history_target(&mut self, target: &ast::Expr) -> bool {
        let mut target = target;
        while let ast::Expr::Group { inner, .. } = target {
            target = inner;
        }
        let literal = matches!(
            target,
            ast::Expr::Int { .. }
                | ast::Expr::Float { .. }
                | ast::Expr::Bool { .. }
                | ast::Expr::Str { .. }
                | ast::Expr::Color { .. }
                | ast::Expr::Na { .. }
        );
        let member = if let ast::Expr::Ident(ident) = target {
            // Respect ordinary/parameter bindings and registered series before
            // classifying a name as a built-in constant. Do not infer a
            // namespace from its spelling or from constant-folded HIR.
            let builtin = self.lookup_variable(&ident.name).is_none()
                && self.registry.lookup_series(&ident.name).is_none()
                && self
                    .registry
                    .lookup_constant(&ident.name)
                    .is_some_and(|constant| !constant.permits_direct_history());
            let enum_member = ident.name.rsplit_once('.').is_some_and(|(name, member)| {
                self.enum_type_name(name)
                    .is_some_and(|name| self.enums[&name].variant_index(member).is_some())
            });
            builtin || enum_member
        } else {
            false
        };
        if !(literal || member) {
            return false;
        }
        self.error(
            codes::E0209,
            "history reference target must be a variable or computed value",
            target.span(),
            "literals and direct built-in or enum members have no history",
        );
        true
    }

    fn reject_text_format_history(&mut self, value: &HirExpr, span: Span) -> bool {
        if !value.ty.is_text_format() {
            return false;
        }
        self.error(
            codes::E0209,
            "history references are not defined for built-in text_format values",
            span,
            "compute the formatting flags for the current bar",
        );
        true
    }

    /// A field read is series-qualified, but `[]` written directly on one
    /// (`obj.field[n]`, `arr.get(0).field[1]`) is `E0209`: the field keeps no
    /// per-bar history of its own. The test is on the source `target`, not on
    /// the lowered `value`, because drawing getters (`line.get_x1(id)`) and
    /// user functions or methods returning a field lower to the same field
    /// read, and the history of a call result is valid.
    fn reject_object_field_history(&mut self, target: &ast::Expr, value: &HirExpr) -> bool {
        let written_as_field = match target {
            ast::Expr::FieldAccess { .. } => true,
            ast::Expr::Ident(ident) => ident.name.contains('.'),
            _ => false,
        };
        if !written_as_field || !matches!(value.kind, HirExprKind::GetField { .. }) {
            return false;
        }
        self.error(
            codes::E0209,
            OBJECT_FIELD_HISTORY_MESSAGE,
            target.span(),
            OBJECT_FIELD_HISTORY_LABEL,
        );
        true
    }

    pub(crate) fn analyze_history(
        &mut self,
        target: &ast::Expr,
        offset: &ast::Expr,
        span: Span,
    ) -> HirExpr {
        self.arrow_history_spans.insert(span);
        // A non-negative integer literal takes the static fast path (the
        // offset is baked into the read and counts as a request toward the
        // `max_bars_back` ceiling; past the hard limit it is `E0233`).
        let offset_value = match offset {
            ast::Expr::Int { value, .. } if u32::try_from(*value).is_ok() => Some(*value as u32),
            _ => None,
        };
        let Some(offset_value) = offset_value else {
            // Not a literal. A numeric offset, including a float, truncates
            // toward zero when the read executes. Non-numeric
            // offsets are still `E0210`. The read is resolved at run time against the
            // slot's history (out-of-range / `na` offset → `na`), so the
            // static `E0233` limit does not apply.
            return self.analyze_history_dynamic(target, offset, span);
        };

        // Unwrap grouping so `(close)[1]` works.
        let mut target_expr = target;
        while let ast::Expr::Group { inner, .. } = target_expr {
            target_expr = inner;
        }

        match target_expr {
            ast::Expr::Ident(ident) => {
                let base = self.analyze_ident(ident, offset_value);
                if base.ty.is_error() {
                    return self.error_expr(span);
                }
                if self.reject_text_format_history(&base, span) {
                    return self.error_expr(span);
                }
                if self.reject_object_field_history(target_expr, &base) {
                    return self.error_expr(span);
                }
                if self.reject_unmaterializable_history_target(target_expr) {
                    return self.error_expr(span);
                }
                // Record the (valid) literal offset for the `max_bars_back`
                // ceiling, which is settled after the whole program is
                // analyzed so that it is final regardless of source order.
                // Only recorded on a resolved target, so a broken reference
                // does not also cascade `E0233`.
                self.max_bars_back_offsets
                    .push((offset_value, offset.span()));
                HirExpr {
                    span,
                    qualifier: Qualifier::Series,
                    ..base
                }
            }
            // Compound target (`(close - open)[1]`, `close[1][2]`, ...):
            // materialize the expression into a hidden series slot, then
            // read the slot at the requested offset. Computed targets keep
            // their history independently of their current value qualifier.
            other => {
                let target_hir = self.analyze_expr(other);
                if target_hir.ty.is_error() {
                    return self.error_expr(span);
                }
                if self.reject_text_format_history(&target_hir, span) {
                    return self.error_expr(span);
                }
                if self.reject_object_field_history(other, &target_hir) {
                    return self.error_expr(span);
                }
                if self.reject_unmaterializable_history_target(other) {
                    return self.error_expr(span);
                }
                // Compound-history materialization allocates a per-site hidden
                // slot that is stored once per bar; a loop would store it
                // multiple times per bar. Strict compile error (E0222).
                if self.reject_stateful_in_loop("history of an expression", span) {
                    return self.error_expr(span);
                }
                self.warn_if_not_executed_every_bar(
                    "history of an expression",
                    "the hidden series backing this history",
                    span,
                );
                // Record the (valid) literal offset for the ceiling — see
                // the ident branch above.
                self.max_bars_back_offsets
                    .push((offset_value, offset.span()));
                let ty = target_hir.ty.clone();
                let slot = self.allocate_synthetic_slot(ty.clone(), span);
                HirExpr {
                    kind: HirExprKind::MaterializeHistory {
                        slot,
                        expr: Box::new(target_hir),
                        offset: offset_value,
                    },
                    ty,
                    qualifier: Qualifier::Series,
                    span,
                }
            }
        }
    }

    /// `expr[offset]` where `offset` is not an integer literal. The offset must
    /// be a numeric expression; fractional values truncate toward zero at
    /// run time (`current_bar - offset`, out-of-range / `na` → `na`). A
    /// nonnumeric offset is `E0210`.
    pub(crate) fn analyze_history_dynamic(
        &mut self,
        target: &ast::Expr,
        offset: &ast::Expr,
        span: Span,
    ) -> HirExpr {
        self.arrow_history_spans.insert(span);
        let offset_hir = self.analyze_expr(offset);
        if offset_hir.ty.is_error() {
            // The offset itself failed to analyze; a diagnostic was already
            // emitted. Do not also cascade `E0210`.
            return self.error_expr(span);
        }
        if !is_numeric(&offset_hir.ty) {
            self.error(
                codes::E0210,
                "history offset must be numeric (int or float)",
                offset.span(),
                format!("this offset is a {} value", offset_hir.ty.describe()),
            );
            return self.error_expr(span);
        }

        // Unwrap grouping so `(close)[i]` works, exactly like the literal path.
        let mut target_expr = target;
        while let ast::Expr::Group { inner, .. } = target_expr {
            target_expr = inner;
        }

        // Evaluate the target at offset 0 to obtain its base read node, then
        // rewrite it into the matching dynamic-offset variant. Only a source
        // identifier may reuse the variable/builtin slot. A computed target
        // must capture its value here even if lowering folds it to a ReadVar
        // (e.g. unary `+`), before a later write can change that variable.
        let base = self.analyze_expr(target_expr);
        if base.ty.is_error() {
            return self.error_expr(span);
        }
        if self.reject_text_format_history(&base, span) {
            return self.error_expr(span);
        }
        if self.reject_object_field_history(target_expr, &base) {
            return self.error_expr(span);
        }
        // Direct reads already have storage; computed values receive a hidden
        // slot below. Both may carry weaker-than-Series value qualifiers.
        // Reject forbidden source targets rather than constant-folded HIR.
        if self.reject_unmaterializable_history_target(target_expr) {
            return self.error_expr(span);
        }
        let ty = base.ty.clone();
        let direct_identifier = matches!(target_expr, ast::Expr::Ident(_));
        let kind = match base.kind {
            HirExprKind::ReadBuiltin { series, offset: 0 } if direct_identifier => {
                HirExprKind::ReadBuiltinDynamic {
                    series,
                    offset: Box::new(offset_hir),
                }
            }
            HirExprKind::ReadVar { symbol, offset: 0 } if direct_identifier => {
                HirExprKind::ReadVarDynamic {
                    symbol,
                    offset: Box::new(offset_hir),
                }
            }
            _ => {
                // Compound target: materialize into a hidden slot, then read
                // it at the dynamic offset. Same store-once-per-bar contract
                // as the literal compound path, so the same loop/branch
                // guards apply.
                if self.reject_stateful_in_loop("history of an expression", span) {
                    return self.error_expr(span);
                }
                self.warn_if_not_executed_every_bar(
                    "history of an expression",
                    "the hidden series backing this history",
                    span,
                );
                let slot = self.allocate_synthetic_slot(ty.clone(), span);
                HirExprKind::MaterializeHistoryDynamic {
                    slot,
                    expr: Box::new(base),
                    offset: Box::new(offset_hir),
                }
            }
        };
        HirExpr {
            kind,
            ty,
            qualifier: Qualifier::Series,
            span,
        }
    }
}

/// Whether a global declared at `declaration` lies above the function
/// definition at `definition`. Both are spans in one source: a script's
/// functions see its globals and a library's functions see the library's.
/// A declaration in another source is never treated as above.
fn declared_above(declaration: Span, definition: Span) -> bool {
    declaration.source == definition.source && declaration.end <= definition.start
}

/// The note on a diagnostic for a global that a function cannot see because
/// it is declared below the function's definition.
pub(crate) fn declared_below_note(name: &str) -> String {
    format!(
        "the global `{name}` is declared below this function; a function can read only the globals declared above its definition"
    )
}
