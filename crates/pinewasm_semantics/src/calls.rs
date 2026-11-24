//! Call dispatch and expansion: collection method-call desugaring, call-site
//! resolution, user-function and library-call inlining, and the cast /
//! pure-builtin / `timestamp` call families.

use crate::*;
use pinewasm_hir::TickerFunc;

/// Resolve the fast path for a user-function call with exactly one collected
/// candidate. Its parameter and argument checks still run during expansion.
pub(crate) fn singleton_overload_index(candidate_count: usize) -> Option<usize> {
    if candidate_count == 1 {
        Some(0)
    } else {
        None
    }
}

impl<'r> Analyzer<'r> {
    /// Try to interpret a dotted `callee.name` (`receiver.method`) as a
    /// method call on a collection value. Returns `Some(result)` when the
    /// receiver prefix resolves to an array/matrix/map-typed variable — the
    /// whole call is then desugared to the namespace form
    /// (`a.push(x)` ≡ `array.push(a, x)`) — and `None` when the prefix is not
    /// a collection variable (so the caller falls through to its normal
    /// unknown-function handling). A dotted name whose prefix *is* a
    /// collection value but whose method is unknown for that receiver type is
    /// a reported `E0235` (returned as `Some(error_expr)`).
    pub(crate) fn try_method_call(
        &mut self,
        callee: &ast::Ident,
        args: &[ast::CallArg],
        span: Span,
    ) -> Option<HirExpr> {
        // Split on the LAST dot: `receiver.method`. The receiver may itself be
        // a plain identifier (dotted receivers are not values here).
        let (prefix, method) = callee.name.rsplit_once('.')?;
        // A field chain on a value, or an enum member (`Side.up.label()`), is
        // an expression receiver, not a namespace.
        if prefix.contains('.') {
            let head = prefix.split('.').next()?;
            if self.lookup_variable(head).is_none() {
                // The receiver is an enum member: everything before its last
                // segment names the enum, possibly through an import alias
                // (`lib.Trend.up.sign()`).
                let (enum_name, _) = prefix.rsplit_once('.')?;
                self.enum_type_name(enum_name)?;
            }
            return Some(self.analyze_method_call_expr(
                &ast::Expr::Ident(ast::Ident {
                    name: prefix.to_string(),
                    span: callee.span,
                }),
                &ast::Ident {
                    name: method.to_string(),
                    span: callee.span,
                },
                args,
                span,
            ));
        }
        let symbol_id = self.lookup_variable(prefix)?;
        let recv_ty = self.symbols[symbol_id.0 as usize].ty.clone();

        // A user-defined method declared on the receiver's type wins over
        // a builtin of the same name: `obj.f(args)` inlines the method template
        // with the receiver prepended as the first argument. This covers UDT
        // receivers (`Point.length()`) and user methods on collections
        // (`array<float>` receivers).
        let method_key = (Self::value_type_key(&recv_ty), method.to_string());
        if let Some(found) = self.find_method(&method_key) {
            return Some(self.expand_method_call(prefix, &method_key, found, args, span));
        }

        if let Some(result) =
            self.try_drawing_method_call(prefix, callee.span, &recv_ty, method, args, span)
        {
            return Some(result);
        }

        let namespace = match &recv_ty {
            Type::Array(_) => "array",
            Type::Matrix(_) => "matrix",
            Type::Map(..) => "map",
            // An object receiver with no matching user-defined method: report an
            // unknown-method error with a did-you-mean over the type's methods.
            Type::Object(type_name) => {
                let type_name = type_name.clone();
                return Some(
                    self.object_method_fallback(&type_name, prefix, method, args, callee, span),
                );
            }
            // The prefix is a value but not a collection: not a method call.
            _ => return None,
        };
        // Reconstruct the namespace function name and resolve it. The method
        // name is exactly the segment after the namespace dot in the builtin's
        // name (`array.push` -> method `push`), so this reuses the same
        // registry entries as the namespace form — identical lowering.
        let full_name = format!("{namespace}.{method}");
        match self.registry.lookup_function(&full_name) {
            Some(func) => {
                if let BuiltinFunctionKind::Collection(cf) = func.kind {
                    // Prepend the receiver as the first positional argument,
                    // then hand off to the shared collection analyzer so the
                    // method form lowers identically to the namespace form.
                    let receiver_arg = ast::CallArg {
                        name: None,
                        value: ast::Expr::Ident(ast::Ident {
                            name: prefix.to_string(),
                            span: callee.span,
                        }),
                        span: callee.span,
                    };
                    let mut full_args = Vec::with_capacity(args.len() + 1);
                    full_args.push(receiver_arg);
                    full_args.extend_from_slice(args);
                    Some(self.analyze_collection_call(cf, &full_args, span))
                } else {
                    // The reconstructed name resolves to a non-collection
                    // builtin (should not happen for these namespaces): treat
                    // as not-a-method-call.
                    None
                }
            }
            None => {
                // The receiver IS a collection value but the method is unknown
                // for its type: E0235 with a did-you-mean over that type's
                // methods.
                let mut diag = Diagnostic::error(
                    codes::E0235,
                    format!("unknown method `{method}` on {}", recv_ty.describe()),
                )
                .with_label(callee.span, "not a method of this collection type");
                if let Some(suggestion) = self.suggest_method(namespace, method) {
                    diag = diag.with_note(format!("did you mean `{suggestion}`?"));
                }
                self.diagnostics.push(diag);
                for arg in args {
                    let _ = self.analyze_expr(&arg.value);
                }
                Some(self.error_expr(span))
            }
        }
    }

    /// Route a drawing object's method form through the same analyzer as its
    /// namespace function (`ln.set_xy1(x, y)` → `line.set_xy1(ln, x, y)`).
    /// Only members whose first parameter is the receiver object qualify;
    /// constructors and unrelated namespace functions remain non-methods.
    pub(crate) fn try_drawing_method_call(
        &mut self,
        receiver_name: &str,
        receiver_span: Span,
        receiver_ty: &Type,
        method: &str,
        args: &[ast::CallArg],
        span: Span,
    ) -> Option<HirExpr> {
        let Type::Object(type_name) = receiver_ty else {
            return None;
        };
        let full_name = format!("{type_name}.{method}");
        let spec = pinewasm_hir::drawing::drawing_function_spec(&full_name)?;
        if !spec.overloads.iter().any(|overload| {
            overload
                .first()
                .is_some_and(|param| param.ty == *receiver_ty)
        }) {
            return None;
        }
        let receiver_arg = ast::CallArg {
            name: None,
            value: ast::Expr::Ident(ast::Ident {
                name: receiver_name.to_string(),
                span: receiver_span,
            }),
            span: receiver_span,
        };
        let mut full_args = Vec::with_capacity(args.len() + 1);
        full_args.push(receiver_arg);
        full_args.extend_from_slice(args);
        let callee = ast::Ident {
            name: full_name,
            span,
        };
        self.analyze_drawing_call(&callee, &full_args, span)
    }

    /// A method on an object receiver with no user-defined method of that
    /// name. `obj.copy()` is the method form of `Type.copy(obj)`; any
    /// other name is an unknown method.
    fn object_method_fallback(
        &mut self,
        type_name: &str,
        receiver: &str,
        method: &str,
        args: &[ast::CallArg],
        callee: &ast::Ident,
        span: Span,
    ) -> HirExpr {
        if method == "copy" && args.is_empty() {
            let receiver = ast::CallArg {
                name: None,
                value: ast::Expr::Ident(ast::Ident {
                    name: receiver.to_string(),
                    span: callee.span,
                }),
                span: callee.span,
            };
            return self.analyze_object_copy(type_name, &[receiver], span);
        }
        self.unknown_object_method(type_name, method, args, callee, span)
    }

    /// Report an unknown method on an object receiver (`E0237`), analyzing the
    /// argument expressions for their own errors and offering a did-you-mean
    /// over the methods declared for this receiver type.
    pub(crate) fn unknown_object_method(
        &mut self,
        type_name: &str,
        method: &str,
        args: &[ast::CallArg],
        callee: &ast::Ident,
        span: Span,
    ) -> HirExpr {
        let mut diag = Diagnostic::error(
            codes::E0237,
            format!("type `{type_name}` has no method `{method}`"),
        )
        .with_label(callee.span, "unknown method on this object type");
        let methods = &self.methods;
        if let Some(suggestion) = self.suggestions.among(method, || {
            methods
                .keys()
                .map(|(ty, name)| (ty == type_name).then_some(name.as_str()))
        }) {
            diag = diag.with_note(format!("did you mean `{suggestion}`?"));
        }
        self.diagnostics.push(diag);
        for arg in args {
            let _ = self.analyze_expr(&arg.value);
        }
        self.error_expr(span)
    }

    /// Best-effort "did you mean" over the method names available on a
    /// collection namespace (`array`/`map`/`matrix`), for an unknown method.
    pub(crate) fn suggest_method(&mut self, namespace: &str, method: &str) -> Option<String> {
        let registry = self.registry;
        // The search works over the bare method names (the part after the
        // namespace dot); reconstruct the full form for the message.
        self.suggestions
            .among(method, || registry.collection_methods(namespace))
            .map(|bare| format!("{namespace}.{bare}"))
    }

    pub(crate) fn analyze_call_expr(
        &mut self,
        callee: &ast::Ident,
        args: &[ast::CallArg],
        type_args: &[ast::Ident],
        span: Span,
    ) -> HirExpr {
        if matches!(
            self.registry.lookup_function(&callee.name).map(|f| f.kind),
            Some(BuiltinFunctionKind::Ta(_))
        ) {
            self.warn_stateful_call_in_conditional(&callee.name, span);
        }
        if callee.name.starts_with("request.")
            && self.reject_non_dynamic_request_context(&callee.name, span)
        {
            for arg in args {
                let _ = self.analyze_expr(&arg.value);
            }
            return self.error_expr(span);
        }
        // Collections-of-UDTs: a generic constructor whose element type is
        // a user-defined type carries structured `type_args` on the call node
        // (`array.new<Point>`, `map.new<string, Point>`); all-scalar generics
        // were rewritten by the parser to their named registry constructors
        // (`array.new_float`) and carry none. Intercepted before the registry
        // lookup; the UDT name is validated here.
        if !type_args.is_empty() {
            return self.analyze_object_collection_ctor(type_args, args, callee, span);
        }
        // Named drawing-array shorthand constructors: `array.new_line()`,
        // `array.new_label()`, `array.new_box()`, `array.new_table()`,
        // `array.new_linefill()` — thin aliases of the generic
        // `array.new<line>()` etc., producing a fresh `array<drawing>`.
        // Intercepted before the registry lookup (these are not registered
        // functions); reuses the object-array constructor with the drawing
        // type's `type_id`.
        if let Some(kind) = DrawKind::from_array_constructor_name(&callee.name) {
            let type_name = kind.type_name();
            let type_id = self
                .types
                .get(type_name)
                .expect("drawing types are seeded")
                .type_id;
            return self.analyze_object_array_constructor(type_id, type_name, args, span);
        }
        // Stateful drawing namespaces (`chart.point.*`, `line.*`, `label.*`,
        // `box.*`): intercepted before the user-defined-type path and the registry lookup so
        // the drawing member forms (`new`/setters/getters/`delete`/`copy`) win
        // over the generic `Name.new`/`Name.copy` handling for the seeded
        // synthetic drawing types.
        if let Some(result) = self.analyze_drawing_call(callee, args, span) {
            return result;
        }
        // Order-flow footprint namespaces (`request.footprint`, `footprint.*`,
        // `volume_row.*`): intercepted before the registry lookup so the
        // object-accessor forms win. These read the optional host footprint
        // table (or `na`/empty/`false`).
        if let Some(result) = self.analyze_footprint_call(callee, args, span) {
            return result;
        }
        // Type-cast builtins (`int`/`float`/`bool`/`string`/`color`
        // and the drawing casts `line`/`label`/`box`/`table`/`linefill`).
        // Intercepted before the registry lookup: the scalar casts share names
        // with the type keywords / `color`, and the drawing casts share names
        // with the seeded drawing object types. `line.new(...)` etc. keep their
        // dot, so `split_drawing_callee` already claimed those above; only the
        // bare `line(x)` cast form reaches here.
        if is_cast_name(&callee.name) {
            return self.analyze_cast_call(&callee.name, args, span);
        }
        if callee.name == "ta.vwap" && !args.is_empty() {
            return self.analyze_ta_vwap_function_call(args, span);
        }
        // Object construction: `Name.new(...)` / `Name.copy(obj)` where `Name` is a
        // collected user-defined type. Intercepted before the registry lookup
        // (these are not registered functions) so the type name wins.
        if let Some((type_name, method)) = callee.name.rsplit_once('.') {
            if let Some(type_name) = self.object_type_name(type_name) {
                let type_name = type_name.as_str();
                match method {
                    "new" => return self.analyze_object_new(type_name, args, span),
                    "copy" => return self.analyze_object_copy(type_name, args, span),
                    _ => {
                        self.error(
                            codes::E0237,
                            format!("type `{type_name}` has no method `{method}`"),
                            callee.span,
                            "a user-defined type supports `.new(...)` and `.copy(obj)`",
                        );
                        for arg in args {
                            let _ = self.analyze_expr(&arg.value);
                        }
                        return self.error_expr(span);
                    }
                }
            }
        }
        match self.registry.lookup_function(&callee.name) {
            Some(func) => {
                match func.kind {
                    // `plot()`/`hline()` are the two effect calls that also
                    // yield a value: a plot id, whose only use is as a
                    // `fill()` endpoint. Capturing one emits the plot and
                    // returns a `plot`-typed handle.
                    BuiltinFunctionKind::Plot
                    | BuiltinFunctionKind::Display(DisplayKind::Hline) => {
                        self.analyze_plot_handle_call(func.kind, callee, args, span)
                    }
                    // Global-only calls in a local block's tail position get
                    // the same diagnostic as in statement position.
                    BuiltinFunctionKind::AlertCondition | BuiltinFunctionKind::Display(_)
                        if self.in_local_scope() =>
                    {
                        self.error(
                            codes::E0254,
                            format!("Cannot use \"{}\" in local scope", func.name),
                            span,
                            "plot-family calls are only allowed in global scope",
                        );
                        for arg in args {
                            let _ = self.analyze_expr(&arg.value);
                        }
                        self.error_expr(span)
                    }
                    BuiltinFunctionKind::Indicator
                    | BuiltinFunctionKind::MaxBarsBack
                    | BuiltinFunctionKind::Log(_)
                    | BuiltinFunctionKind::Alert
                    | BuiltinFunctionKind::AlertCondition
                    | BuiltinFunctionKind::Display(_)
                    | BuiltinFunctionKind::RuntimeErrorFn
                    | BuiltinFunctionKind::Order(_) => {
                        // Effect functions produce void; using them inside an
                        // expression is an error.
                        self.error(
                            codes::E0212,
                            format!("`{}` does not produce a value", func.name),
                            span,
                            "effect calls cannot be used inside expressions",
                        );
                        for arg in args {
                            let _ = self.analyze_expr(&arg.value);
                        }
                        self.error_expr(span)
                    }
                    BuiltinFunctionKind::Ta(ta) if ta.is_engine() => {
                        self.analyze_ta_engine_call(func, ta, args, span)
                    }
                    BuiltinFunctionKind::Ta(ta) => self.analyze_ta_call(func, ta, args, span),
                    BuiltinFunctionKind::Pure(PureFunc::Timestamp) => {
                        self.analyze_timestamp_call(args, span)
                    }
                    BuiltinFunctionKind::Pure(pure) => {
                        self.analyze_pure_call(func, pure, args, span)
                    }
                    BuiltinFunctionKind::Collection(cf) => {
                        self.analyze_collection_call(cf, args, span)
                    }
                    BuiltinFunctionKind::MapConstructor { key, val } => {
                        self.analyze_map_constructor(key.to_type(), val.to_type(), args, span)
                    }
                    BuiltinFunctionKind::MatrixConstructor { elem } => {
                        self.analyze_matrix_constructor(elem.to_type(), args, span)
                    }
                    BuiltinFunctionKind::Input(kind) => {
                        self.analyze_input_call(func, kind, args, span)
                    }
                    BuiltinFunctionKind::InputGeneric => {
                        self.analyze_input_generic_call(func, args, span)
                    }
                    BuiltinFunctionKind::Security => self.analyze_security_call(args, span),
                    BuiltinFunctionKind::Request(func) => {
                        self.analyze_request_call(func, args, span)
                    }
                    BuiltinFunctionKind::SecurityLowerTf => {
                        self.analyze_security_lower_tf_call(args, span)
                    }
                    BuiltinFunctionKind::PivotPointLevels => {
                        self.analyze_pivot_point_levels_call(args, span)
                    }
                    BuiltinFunctionKind::TimeFunction { close } => self.analyze_time_function_call(
                        if close { "time_close" } else { "time" },
                        if close {
                            PureFunc::TimeClose
                        } else {
                            PureFunc::TimeOpen
                        },
                        args,
                        span,
                    ),
                }
            }
            None => {
                // Method-call syntax: `receiver.method(args)` where the
                // receiver is a collection **value** (an array/matrix/map-typed
                // variable). It desugars to the namespace form with the
                // receiver as the first argument (`a.push(x)` ≡
                // `array.push(a, x)`). Detected here (after the registry miss)
                // so the namespace form always wins for a dotted name that is
                // itself a registered builtin.
                if let Some(result) = self.try_method_call(callee, args, span) {
                    return result;
                }
                // An `alias.name(...)` call through an import: the alias
                // resolves in the *current* namespace (the main script's
                // imports, or — inside a library body — that library's own
                // imports). The exported function's body is inlined with the
                // library's namespace active, so its free names never resolve
                // against the importer. A private (non-exported) or unknown
                // name is `E0243`.
                if let Some((alias, fname)) = callee.name.split_once('.') {
                    if let Some(unit) = self.resolve_alias(alias) {
                        return self.expand_lib_call(unit, alias, fname, callee, args, span);
                    }
                }
                // A user-defined function in the current namespace: inline-expand
                // it at this call site.
                if self.current_functions().contains_key(&callee.name) {
                    let result = self.expand_function_call(callee, args, span);
                    // A function whose body ends in a tuple cannot be used as a
                    // scalar value: it must be destructured (`[a, b] = f(...)`).
                    if matches!(result.kind, HirExprKind::Tuple { .. }) {
                        self.diagnostics.push(
                            Diagnostic::error(
                                codes::E0225,
                                format!(
                                    "`{}` returns a tuple; destructure it with `[...] = {}(...)`",
                                    callee.name, callee.name
                                ),
                            )
                            .with_label(
                                span,
                                "a tuple-returning call cannot be used as a scalar value",
                            ),
                        );
                        return self.error_expr(span);
                    }
                    return result;
                }
                // A method called as a function (`m(x, ...)`): the receiver is
                // the first argument.
                if let Some(result) = self.try_method_as_function(callee, args, span) {
                    return result;
                }
                if self.registry.lookup_series(&callee.name).is_some()
                    || self.registry.lookup_constant(&callee.name).is_some()
                    || self.lookup_variable(&callee.name).is_some()
                {
                    self.error(
                        codes::E0203,
                        format!("`{}` is not a function", callee.name),
                        callee.span,
                        "this name is a value, not a function",
                    );
                } else {
                    let mut diag = Diagnostic::error(
                        codes::E0202,
                        format!("unknown function `{}`", callee.name),
                    )
                    .with_label(callee.span, "not a known built-in function");
                    if let Some(suggestion) = self.suggestions.builtin(self.registry, &callee.name)
                    {
                        diag = diag.with_note(format!("did you mean `{suggestion}`?"));
                    }
                    let head = callee.name.split('.').next().unwrap_or(&callee.name);
                    if self.global_declared_below_function(head).is_some() {
                        diag = diag.with_note(crate::exprs::declared_below_note(head));
                    } else if callee.name.contains('.') {
                        diag = diag.with_note(NAMESPACE_NOTE);
                    }
                    self.diagnostics.push(diag);
                }
                for arg in args {
                    let _ = self.analyze_expr(&arg.value);
                }
                self.error_expr(span)
            }
        }
    }

    /// Analyze a chained method call `receiver.method(args)`, where the
    /// receiver is an arbitrary expression (typically the result of a preceding
    /// call — `obj.a().b()`). The receiver is bound to a hidden `%recvN`
    /// variable so it evaluates exactly once and can be referenced by name, then
    /// the call dispatches exactly like `obj.method(args)`. A method call on a
    /// non-object / non-collection receiver, or an unknown method, is `E0237`.
    pub(crate) fn analyze_method_call_expr(
        &mut self,
        receiver: &ast::Expr,
        method: &ast::Ident,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        let Some((recv_name, recv_ty)) = self.bind_method_receiver(receiver, span) else {
            for arg in args {
                let _ = self.analyze_expr(&arg.value);
            }
            return self.error_expr(span);
        };
        let key = (Self::value_type_key(&recv_ty), method.name.clone());
        if let Some(found) = self.find_method(&key) {
            return self.expand_method_call(&recv_name, &key, found, args, span);
        }
        if let Some(result) = self.try_drawing_method_call(
            &recv_name,
            method.span,
            &recv_ty,
            &method.name,
            args,
            span,
        ) {
            return result;
        }
        let namespace = match &recv_ty {
            Type::Array(_) => "array",
            Type::Matrix(_) => "matrix",
            Type::Map(..) => "map",
            _ => "",
        };
        if self
            .registry
            .lookup_function(&format!("{namespace}.{}", method.name))
            .is_some()
        {
            let callee = ast::Ident {
                name: format!("{recv_name}.{}", method.name),
                span: method.span,
            };
            if let Some(result) = self.try_method_call(&callee, args, span) {
                return result;
            }
        }
        match &recv_ty {
            Type::Object(type_name) => {
                let type_name = type_name.clone();
                self.object_method_fallback(
                    &type_name,
                    &recv_name,
                    &method.name,
                    args,
                    method,
                    span,
                )
            }
            other => {
                let desc = other.describe();
                self.error(
                    codes::E0237,
                    format!("type `{desc}` has no method `{}`", method.name),
                    method.span,
                    "unknown method on this value",
                );
                for arg in args {
                    let _ = self.analyze_expr(&arg.value);
                }
                self.error_expr(span)
            }
        }
    }

    /// Bind a method receiver expression to a synthetic `%recvN` variable so it
    /// evaluates exactly once and is nameable (the dispatch path re-reads it by
    /// name; a `%`-prefixed name never collides with a user identifier).
    /// Returns that name and the receiver's type, or `None` when the receiver
    /// does not analyze (its own errors are already reported).
    pub(crate) fn bind_method_receiver(
        &mut self,
        receiver: &ast::Expr,
        span: Span,
    ) -> Option<(String, Type)> {
        let uid = self.method_recv_counter;
        self.method_recv_counter += 1;
        let recv_name = format!("%recv{uid}");
        let bind = ast::Stmt::Assign {
            name: ast::Ident {
                name: recv_name.clone(),
                span,
            },
            value: receiver.clone(),
            is_var: false,
            is_varip: false,
            annotation: None,
            span,
        };
        if let Some(s) = self.analyze_stmt(&bind) {
            self.pending_stmts.push(s);
        }
        let recv_id = self.lookup_variable(&recv_name)?;
        let recv_ty = self.symbols[recv_id.0 as usize].ty.clone();
        (!recv_ty.is_error()).then_some((recv_name, recv_ty))
    }
}

#[cfg(test)]
mod singleton_overload_index_tests {
    use super::singleton_overload_index;

    #[test]
    fn selects_only_a_single_collected_candidate() {
        assert_eq!(singleton_overload_index(0), None);
        assert_eq!(singleton_overload_index(1), Some(0));
        assert_eq!(singleton_overload_index(2), None);
        assert_eq!(singleton_overload_index(usize::MAX), None);
    }
}

impl<'r> Analyzer<'r> {
    /// The function table of the *current* namespace: the active library
    /// unit's functions while inlining a library body, else the main script's
    /// functions.
    pub(crate) fn current_functions(&self) -> &HashMap<String, Vec<ast::FuncDef>> {
        match self.lib_ctx.last() {
            Some(frame) => &self.libs.units[frame.unit].functions,
            None => &self.functions,
        }
    }

    /// Resolve an import alias in the *current* namespace to its arena unit
    /// index: the active library's own imports while inlining a library
    /// body, else the main script's import aliases. Returns `None` when the
    /// alias is not a resolved import here.
    pub(crate) fn resolve_alias(&self, alias: &str) -> Option<usize> {
        match self.lib_ctx.last() {
            Some(frame) => self.libs.units[frame.unit].imports.get(alias).copied(),
            None => self.libs.main_aliases.get(alias).copied(),
        }
    }

    /// Expand an imported `alias.fn(...)` call. The exported function's
    /// body is inlined with the callee library's namespace active — its own
    /// function table, its own imports, and its own global scope — so free names
    /// in the body resolve against the library, never the importer. Among a
    /// name's overloads only exported ones are callable, and the call picks one
    /// with [`Self::select_overload`]. A name with no exported function may name
    /// an exported method, called as a function with its receiver first
    /// (`alias.m(x, ...)`); the receiver's type picks the method, as for
    /// `x.m(...)`. A name that exists but is not exported, or is unknown in the
    /// library, is `E0243`.
    /// Arguments are evaluated in the *caller's* scope (the importer's, or the
    /// enclosing library's), matching a local call.
    pub(crate) fn expand_lib_call(
        &mut self,
        unit: usize,
        alias: &str,
        fname: &str,
        callee: &ast::Ident,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        let functions = self.libs.units[unit]
            .functions
            .get(fname)
            .cloned()
            .unwrap_or_default();
        let candidates: Vec<ast::FuncDef> =
            functions.iter().filter(|d| d.exported).cloned().collect();
        let methods: Vec<bool> = self
            .lib_methods
            .get(unit)
            .into_iter()
            .flat_map(|table| table.iter())
            .filter(|((_, method), _)| method == fname)
            .flat_map(|(_, defs)| defs.iter().map(|def| def.exported))
            .collect();
        if candidates.is_empty() && methods.contains(&true) {
            return self.expand_lib_method_as_function(unit, alias, fname, callee, args, span);
        }
        if candidates.is_empty() {
            if functions.is_empty() && methods.is_empty() {
                self.error(
                    codes::E0243,
                    format!("library `{alias}` has no exported function `{fname}`"),
                    callee.span,
                    "the library does not define a function of this name",
                );
            } else {
                self.error(
                    codes::E0243,
                    format!("`{fname}` is not exported by library `{alias}`"),
                    callee.span,
                    "only functions declared with `export` are callable through an import alias",
                );
            }
            for arg in args {
                let _ = self.analyze_expr(&arg.value);
            }
            return self.error_expr(span);
        }
        let display = format!("{alias}.{fname}");
        let index = match self.select_overload(&candidates, args) {
            Ok(index) => index,
            Err(miss) => return self.overload_miss(&display, miss, args, span),
        };
        // Build (once, lazily) the library's global scope. The library
        // namespace is activated by `expand_def_scoped` for the body only — not
        // for the argument evaluation, which must resolve in the caller's
        // namespace (the importer's, or the enclosing library's).
        let global_scope = self.library_global_scope(unit);
        self.expand_def_scoped(
            &display,
            &candidates[index],
            args,
            span,
            Some((unit, global_scope)),
        )
    }

    /// Expand `alias.m(x, ...)`, an exported library method called as a
    /// function: the receiver `x` is bound once, and its type picks the method
    /// exactly as `x.m(...)` would. A missing receiver, or no exported method
    /// of that name for the receiver's type, is an error.
    fn expand_lib_method_as_function(
        &mut self,
        unit: usize,
        alias: &str,
        fname: &str,
        callee: &ast::Ident,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        let Some((receiver, rest)) = args.split_first().filter(|(r, _)| r.name.is_none()) else {
            self.error(
                codes::E0224,
                format!("`{alias}.{fname}` is a method; pass its receiver as the first argument"),
                span,
                "call it as `x.method(...)` or `alias.method(x, ...)`",
            );
            for arg in args {
                let _ = self.analyze_expr(&arg.value);
            }
            return self.error_expr(span);
        };
        let Some((recv_name, recv_ty)) = self.bind_method_receiver(&receiver.value, span) else {
            for arg in rest {
                let _ = self.analyze_expr(&arg.value);
            }
            return self.error_expr(span);
        };
        let key = (Self::value_type_key(&recv_ty), fname.to_string());
        let defs: Vec<ast::FuncDef> = self
            .lib_methods
            .get(unit)
            .and_then(|table| table.get(&key))
            .into_iter()
            .flatten()
            .filter(|def| def.exported)
            .cloned()
            .collect();
        if defs.is_empty() {
            self.error(
                codes::E0243,
                format!(
                    "library `{alias}` has no exported method `{fname}` for {}",
                    recv_ty.describe()
                ),
                callee.span,
                "no exported method of this name takes a receiver of this type",
            );
            for arg in rest {
                let _ = self.analyze_expr(&arg.value);
            }
            return self.error_expr(span);
        }
        self.expand_method_call(&recv_name, &key, (defs, Some(unit)), rest, span)
    }

    /// Return the library unit's global scope, analyzing its top-level
    /// global statements into the main HIR body on first use and caching the
    /// resulting name→symbol bindings. A library global is analyzed **once** so
    /// its slot is shared across every call site (library-scoped state, like a
    /// module-level `var`), and its init statements run once in the main body.
    /// The globals are analyzed with the library's own namespace active, so a
    /// global initializer that calls a sibling function or references another
    /// global resolves against the library.
    pub(crate) fn library_global_scope(&mut self, unit: usize) -> HashMap<String, SymbolId> {
        if let Some(cached) = self.lib_globals.get(&unit) {
            return cached.clone();
        }
        // Guard against an import cycle reached through a global initializer:
        // seed an empty cache entry before analyzing, so a re-entrant request
        // for the same unit returns the (partial) scope instead of recursing.
        self.lib_globals.insert(unit, HashMap::new());
        let globals = self.libs.units[unit].globals.clone();
        // Analyze the globals with the library namespace active and its scope as
        // the sole (global) scope, so names resolve library-locally.
        let saved_scopes = std::mem::replace(&mut self.scopes, vec![HashMap::new()]);
        // These statements belong to the library's top level, even when first
        // requested by a helper. The caller's function boundary belongs to the
        // saved scope stack, not to this independent namespace initialization.
        let saved_function_scope_bases = std::mem::take(&mut self.function_scope_bases);
        // Library globals are emitted before all caller statements. Analyze
        // them in their own unconditional context, then restore the caller's
        // scheduling and assignment context before expanding the actual body.
        let saved_branch_depth = std::mem::replace(&mut self.branch_depth, 0);
        let saved_request_branch_depth = std::mem::replace(&mut self.request_branch_depth, 0);
        let saved_conditional_stateful_context = self.conditional_stateful_context.take();
        let saved_ternary_branch_depth = std::mem::replace(&mut self.ternary_branch_depth, 0);
        let saved_loop_depth = std::mem::replace(&mut self.loop_depth, 0);
        let saved_assignment_controls = std::mem::take(&mut self.assignment_controls);
        let saved_in_var_initializer = std::mem::replace(&mut self.in_var_initializer, false);
        let saved_defer_tuple_na = std::mem::replace(&mut self.defer_tuple_na, false);
        self.lib_ctx.push(LibFrame { unit });
        let mut stmts = Vec::new();
        for stmt in &globals {
            self.analyze_stmt_into(stmt, &mut stmts);
        }
        self.lib_ctx.pop();
        let scope = std::mem::replace(&mut self.scopes, saved_scopes)
            .pop()
            .unwrap_or_default();
        self.function_scope_bases = saved_function_scope_bases;
        self.branch_depth = saved_branch_depth;
        self.request_branch_depth = saved_request_branch_depth;
        self.conditional_stateful_context = saved_conditional_stateful_context;
        self.ternary_branch_depth = saved_ternary_branch_depth;
        self.loop_depth = saved_loop_depth;
        self.assignment_controls = saved_assignment_controls;
        self.in_var_initializer = saved_in_var_initializer;
        self.defer_tuple_na = saved_defer_tuple_na;
        // The library's global init statements run at the very top of the main
        // body (before the main script's own statements), so their slots are
        // initialized before any call site reads them.
        self.lib_global_stmts.extend(stmts);
        self.lib_globals.insert(unit, scope.clone());
        scope
    }

    /// Inline-expand a user-defined function call at this call site
    /// (per-call-site instantiation). Each argument is evaluated
    /// **once** into a hidden temp bound to the parameter; the body is analyzed
    /// afresh in a scope where params resolve to those temps, so every stateful
    /// construct in the body (`ta.*`, materialized history, `var`, `if`/`switch`
    /// result slots) gets fresh per-site state; the body's last-statement value
    /// is returned to the caller. The body's statements are pushed to
    /// `pending_stmts` so they run before the enclosing statement.
    pub(crate) fn expand_function_call(
        &mut self,
        callee: &ast::Ident,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        let defs = self.current_functions()[&callee.name].clone();
        let index = match self.select_overload(&defs, args) {
            Ok(index) => index,
            Err(miss) => return self.overload_miss(&callee.name, miss, args, span),
        };
        self.expand_def(&callee.name, &defs[index], args, span)
    }

    /// Expand a user-defined method call `obj.f(args)`. The receiver `recv` (a variable
    /// name) is prepended as the first positional argument, the call picks one
    /// of the overloads [`Self::find_method`] returned with
    /// [`Self::select_overload`], and that method template is inlined exactly like a plain
    /// function — so a method body gets fresh per-call-site state (`var`/`ta.*`)
    /// and reference semantics on the receiver object flow through the ordinary
    /// argument temp. A method imported from a library runs in that library's
    /// namespace, like `alias.fn(...)`.
    pub(crate) fn expand_method_call(
        &mut self,
        recv: &str,
        key: &(String, String),
        found: (Vec<ast::FuncDef>, Option<usize>),
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        let (defs, unit) = found;
        // Prepend the receiver as the first argument (binds the typed receiver
        // parameter). Method arguments are positional after the receiver.
        let receiver_arg = ast::CallArg {
            name: None,
            value: ast::Expr::Ident(ast::Ident {
                name: recv.to_string(),
                span,
            }),
            span,
        };
        let mut full_args = Vec::with_capacity(args.len() + 1);
        full_args.push(receiver_arg);
        full_args.extend_from_slice(args);
        // A distinct call-stack display name (`Type.method`) so a method that
        // recurses on itself is caught, without colliding with a plain function.
        let display = format!("{}.{}", key.0, key.1);
        let mut selected = self.select_overload(&defs, &full_args);
        if selected == Err(OverloadMiss::Ambiguous) {
            // Overloads such as `Scene.add(Mesh)` and `Scene.add(Label3D)` are
            // told apart only by argument types that may not follow from the
            // source alone (`s.add(Mesh.new())`, `s.add(f())`). Evaluate each
            // such argument once, in order, into a hidden variable whose type
            // is then known, and choose again.
            full_args = self.bind_untyped_args(full_args, span);
            selected = self.select_overload(&defs, &full_args);
        }
        let def = match selected {
            Ok(index) => defs[index].clone(),
            Err(miss) => return self.overload_miss(&display, miss, &full_args[1..], span),
        };
        match unit {
            Some(unit) => {
                let global_scope = self.library_global_scope(unit);
                self.expand_def_scoped(&display, &def, &full_args, span, Some((unit, global_scope)))
            }
            None => self.expand_def(&display, &def, &full_args, span),
        }
    }

    /// `args` with every argument whose type [`Self::inferred_arg_type`]
    /// cannot tell (other than a bare `na`) evaluated once, in order, into a
    /// hidden variable and replaced by a reference to it. Each binding runs
    /// before the enclosing statement, as a method receiver's does, so every
    /// argument is still evaluated exactly once and left to right.
    fn bind_untyped_args(&mut self, args: Vec<ast::CallArg>, span: Span) -> Vec<ast::CallArg> {
        args.into_iter()
            .map(|arg| {
                if self.inferred_arg_type(&arg.value).is_some()
                    || matches!(arg.value, ast::Expr::Na { .. })
                {
                    return arg;
                }
                match self.bind_method_receiver(&arg.value, span) {
                    Some((name, _)) => ast::CallArg {
                        value: ast::Expr::Ident(ast::Ident {
                            name,
                            span: arg.value.span(),
                        }),
                        ..arg
                    },
                    None => arg,
                }
            })
            .collect()
    }

    /// The methods (overloads) a receiver of key `key.0` calls as `key.1`. The current
    /// namespace is searched first (the main script's methods, or the active
    /// library's own methods), then the methods exported by the libraries it
    /// imports, in arena order. The unit is `Some` for an imported method,
    /// whose body must run in that library's namespace.
    pub(crate) fn find_method(
        &self,
        key: &(String, String),
    ) -> Option<(Vec<ast::FuncDef>, Option<usize>)> {
        let own = match self.lib_ctx.last() {
            Some(frame) => self.lib_methods.get(frame.unit).and_then(|m| m.get(key)),
            None => self.methods.get(key),
        };
        if let Some(defs) = own.filter(|defs| !defs.is_empty()) {
            return Some((defs.clone(), None));
        }
        self.imported_units().into_iter().find_map(|unit| {
            let defs: Vec<ast::FuncDef> = self
                .lib_methods
                .get(unit)?
                .get(key)?
                .iter()
                .filter(|def| def.exported)
                .cloned()
                .collect();
            (!defs.is_empty()).then_some((defs, Some(unit)))
        })
    }

    /// The arena units the current namespace imports, ascending and distinct.
    fn imported_units(&self) -> Vec<usize> {
        let mut units: Vec<usize> = match self.lib_ctx.last() {
            Some(frame) => self.libs.units[frame.unit]
                .imports
                .values()
                .copied()
                .collect(),
            None => self.libs.main_aliases.values().copied().collect(),
        };
        units.sort_unstable();
        units.dedup();
        units
    }

    /// Expand `m(x, ...)` when `m` names no function but a method of the
    /// current namespace (the main script's methods, or the active library's
    /// own): Pine lets a method be called as a function, with its receiver as
    /// the first argument. The receiver is bound once and its type picks the
    /// method, exactly as for `x.m(...)`, so a receiver of the wrong type is an
    /// error rather than a call to another type's method. `None` when the
    /// namespace has no method of that name.
    pub(crate) fn try_method_as_function(
        &mut self,
        callee: &ast::Ident,
        args: &[ast::CallArg],
        span: Span,
    ) -> Option<HirExpr> {
        if callee.name.contains('.') {
            return None;
        }
        let has_method = match self.lib_ctx.last() {
            Some(frame) => self
                .lib_methods
                .get(frame.unit)
                .is_some_and(|table| table.keys().any(|(_, m)| *m == callee.name)),
            None => self.methods.keys().any(|(_, m)| *m == callee.name),
        };
        if !has_method {
            return None;
        }
        let Some((receiver, rest)) = args.split_first().filter(|(r, _)| r.name.is_none()) else {
            self.error(
                codes::E0224,
                format!(
                    "`{}` is a method; pass its receiver as the first argument",
                    callee.name
                ),
                span,
                "call it as `x.method(...)` or `method(x, ...)`",
            );
            for arg in args {
                let _ = self.analyze_expr(&arg.value);
            }
            return Some(self.error_expr(span));
        };
        Some(self.analyze_method_call_expr(&receiver.value, callee, rest, span))
    }

    /// The overload in `defs` that a call with `args` resolves to. A lone
    /// definition is always chosen (its own checks report a mismatch). Among
    /// several, only those whose parameters can bind `args` qualify, and when
    /// more than one does, the declared parameter types decide against the
    /// argument types [`Self::inferred_arg_type`] can tell without analysis: a
    /// known mismatch disqualifies, and an exact match outranks an `int`
    /// argument for a `float` parameter. The choice must not be a guess: a tie,
    /// or remaining candidates that declare different types for an argument
    /// whose type is not known, is [`OverloadMiss::Ambiguous`].
    pub(crate) fn select_overload(
        &self,
        defs: &[ast::FuncDef],
        args: &[ast::CallArg],
    ) -> Result<usize, OverloadMiss> {
        if let Some(index) = singleton_overload_index(defs.len()) {
            return Ok(index);
        }
        let fitting: Vec<usize> = (0..defs.len())
            .filter(|&i| accepts_call_args(&defs[i], args))
            .collect();
        match fitting.as_slice() {
            [] => return Err(OverloadMiss::NoneFits),
            [only] => return Ok(*only),
            _ => {}
        }
        let types: Vec<Option<Type>> = args
            .iter()
            .map(|arg| self.inferred_arg_type(&arg.value))
            .collect();
        let scored: Vec<(usize, u32)> = fitting
            .iter()
            .filter_map(|&i| Some((i, overload_type_score(&defs[i], args, &types)?)))
            .collect();
        let Some(top) = scored.iter().map(|&(_, score)| score).max() else {
            return Err(OverloadMiss::NoneFits);
        };
        let mut best = scored.iter().filter(|&&(_, score)| score == top);
        let (Some(&(winner, _)), None) = (best.next(), best.next()) else {
            return Err(OverloadMiss::Ambiguous);
        };
        // An argument of unknown type could still overturn the choice when the
        // remaining candidates declare different types for it.
        for (j, arg) in args.iter().enumerate() {
            if types[j].is_some() {
                continue;
            }
            let mut declared = scored
                .iter()
                .map(|&(i, _)| param_for_arg(&defs[i], j, arg).and_then(|p| p.ty.as_ref()));
            let first = declared.next().flatten().map(fmt_field_type_key);
            if declared.any(|ty| ty.map(fmt_field_type_key) != first) {
                return Err(OverloadMiss::Ambiguous);
            }
        }
        Ok(winner)
    }

    /// The type of a call argument when it follows from the source alone,
    /// without analyzing it (which would evaluate it): literals, variables in
    /// scope, built-in series and constants, casts, pure built-ins with a fixed
    /// result type, history references, and arithmetic, comparison, logic and
    /// ternaries over those, typed as the analyzer types them (`int / int` is
    /// `int`). `None` otherwise, including `na`.
    fn inferred_arg_type(&self, expr: &ast::Expr) -> Option<Type> {
        match expr {
            ast::Expr::Int { .. } => Some(Type::Int),
            ast::Expr::Float { .. } => Some(Type::Float),
            ast::Expr::Bool { .. } => Some(Type::Bool),
            ast::Expr::Str { .. } => Some(Type::String),
            ast::Expr::Color { .. } => Some(Type::Color),
            ast::Expr::Group { inner, .. } => self.inferred_arg_type(inner),
            ast::Expr::History { target, .. } => self.inferred_arg_type(target),
            ast::Expr::Ident(ident) => {
                if let Some(id) = self.lookup_variable(&ident.name) {
                    let ty = self.symbols[id.0 as usize].ty.clone();
                    return (!ty.is_error()).then_some(ty);
                }
                if let Some(series) = self.registry.lookup_series(&ident.name) {
                    return Some(if series.is_bool() {
                        Type::Bool
                    } else if series.is_int() {
                        Type::Int
                    } else if series.is_string() {
                        Type::String
                    } else {
                        Type::Float
                    });
                }
                self.registry
                    .lookup_constant(&ident.name)
                    .map(|constant| constant.ty.clone())
            }
            ast::Expr::Unary { op, operand, .. } => match op {
                ast::UnaryOp::Not => Some(Type::Bool),
                _ => self
                    .inferred_arg_type(operand)
                    .filter(|ty| matches!(ty, Type::Int | Type::Float)),
            },
            ast::Expr::Binary { op, lhs, rhs, .. } => match op {
                ast::BinaryOp::Lt
                | ast::BinaryOp::Le
                | ast::BinaryOp::Gt
                | ast::BinaryOp::Ge
                | ast::BinaryOp::Eq
                | ast::BinaryOp::Ne
                | ast::BinaryOp::And
                | ast::BinaryOp::Or => Some(Type::Bool),
                ast::BinaryOp::Add
                | ast::BinaryOp::Sub
                | ast::BinaryOp::Mul
                | ast::BinaryOp::Div
                | ast::BinaryOp::Rem => {
                    match (self.inferred_arg_type(lhs)?, self.inferred_arg_type(rhs)?) {
                        (Type::Int, Type::Int) => Some(Type::Int),
                        (Type::Int | Type::Float, Type::Int | Type::Float) => Some(Type::Float),
                        (Type::String, Type::String) if *op == ast::BinaryOp::Add => {
                            Some(Type::String)
                        }
                        _ => None,
                    }
                }
            },
            ast::Expr::Ternary { then_e, else_e, .. } => {
                match (
                    self.inferred_arg_type(then_e)?,
                    self.inferred_arg_type(else_e)?,
                ) {
                    (a, b) if a == b => Some(a),
                    (Type::Int | Type::Float, Type::Int | Type::Float) => Some(Type::Float),
                    _ => None,
                }
            }
            ast::Expr::Call { callee, .. } => {
                if let Some(ty) = cast_result_type(&callee.name) {
                    return Some(ty);
                }
                let function = self.registry.lookup_function(&callee.name)?;
                match function.pure_result? {
                    PureResult::Float => Some(Type::Float),
                    PureResult::Bool => Some(Type::Bool),
                    PureResult::Int => Some(Type::Int),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Report a call that [`Self::select_overload`] could not resolve, and
    /// analyze its arguments for their own errors.
    fn overload_miss(
        &mut self,
        name: &str,
        miss: OverloadMiss,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        let (message, label) = match miss {
            OverloadMiss::NoneFits => (
                format!("no overload of `{name}` takes these {} argument(s)", args.len()),
                "check the number, names and types of the arguments",
            ),
            OverloadMiss::Ambiguous => (
                format!("cannot tell which overload of `{name}` this call means"),
                "give the deciding argument an explicit type, e.g. assign it to a `float` or `int` variable first",
            ),
        };
        self.error(codes::E0224, message, span, label);
        for arg in args {
            let _ = self.analyze_expr(&arg.value);
        }
        self.error_expr(span)
    }

    /// Build [`Self::lib_methods`] from every library unit's methods, keyed by
    /// receiver like a main-script method. A method whose receiver is a type
    /// the analyzer does not know at this point (a type the library declares
    /// itself) is left out. A second method with the same receiver type, name
    /// and signature is `E0224`, a redefinition. A method overloaded by its
    /// other parameters is kept beside the others, and a call picks one with
    /// [`Self::select_overload`].
    pub(crate) fn collect_library_methods(&mut self) {
        let mut tables = Vec::with_capacity(self.libs.units.len());
        let mut redefinitions = Diagnostics::new();
        for unit_index in 0..self.libs.units.len() {
            let unit = self.libs.units[unit_index].clone();
            self.lib_ctx.push(LibFrame { unit: unit_index });
            let mut table: HashMap<(String, String), Vec<ast::FuncDef>> = HashMap::new();
            for def in &unit.methods {
                let Some(receiver) = &def.receiver else {
                    continue;
                };
                let Some(key) = self.receiver_type_key(receiver) else {
                    continue;
                };
                let map_key = (key, def.name.name.clone());
                let overloads = table.entry(map_key.clone()).or_default();
                if let Some(first) = overloads.iter().find(|first| same_signature(first, def)) {
                    redefinitions.push(
                        Diagnostic::error(
                            codes::E0224,
                            format!(
                                "method `{}` is already defined for `{}`",
                                def.name.name, map_key.0
                            ),
                        )
                        .with_label(def.name.span, "redefined here")
                        .with_label(first.name.span, "first defined here"),
                    );
                    continue;
                }
                overloads.push(def.clone());
            }
            tables.push(table);
            self.lib_ctx.pop();
        }
        self.diagnostics.extend(redefinitions.finish());
        self.lib_methods = tables;
    }

    /// Hold to its declared type every typed parameter default that no call
    /// reached, in the script and in every linked library.
    ///
    /// A call that omits an argument evaluates the default and checks it
    /// against the declared type ([`Self::expand_def_scoped`]), so a default
    /// of the wrong type used to be an error only when some call omitted it:
    /// `f(int n = 2.5)` compiled when `f` was never called, or always called
    /// with `n`. A method's receiver cannot be omitted, so its default was
    /// never checked at all. This checks the defaults that are left, after
    /// the statements, when every call has been analyzed, so that a default
    /// is reported by the calls that reach it or here, never by both. A field
    /// default of the wrong type is likewise an error at its declaration
    /// (`collect_types`).
    ///
    /// Evaluating a default here would give it effects no call asked for, so
    /// only a default built from literals is typed ([`literal_type`]). Any
    /// other one is still checked only by a call that omits the argument.
    pub(crate) fn check_unevaluated_defaults(&mut self) {
        /// A literal default that no call reached.
        struct Unevaluated {
            /// The library unit that defines the function; `None` for the
            /// script's own.
            unit: Option<usize>,
            function: String,
            param: String,
            declared: ast::FieldType,
            default: Span,
            found: Type,
        }

        /// The literal defaults of `defs`, in source order, that are not in
        /// `evaluated`. A method's first parameter is declared by its
        /// receiver type.
        fn unevaluated(
            unit: Option<usize>,
            mut defs: Vec<&ast::FuncDef>,
            evaluated: &HashSet<Span>,
            out: &mut Vec<Unevaluated>,
        ) {
            // The definitions come out of hash maps.
            defs.sort_by_key(|def| def.span.start);
            for def in defs {
                for (index, param) in def.params.iter().enumerate() {
                    let receiver = if index == 0 {
                        def.receiver.as_ref()
                    } else {
                        None
                    };
                    let (Some(declared), Some(default)) =
                        (param.ty.as_ref().or(receiver), &param.default)
                    else {
                        continue;
                    };
                    if evaluated.contains(&default.span()) {
                        continue;
                    }
                    if let Some(found) = literal_type(default) {
                        out.push(Unevaluated {
                            unit,
                            function: def.name.name.clone(),
                            param: param.name.name.clone(),
                            declared: declared.clone(),
                            default: default.span(),
                            found,
                        });
                    }
                }
            }
        }

        // The definitions a call can expand: a rejected redefinition is in
        // none of these tables.
        let mut defaults = Vec::new();
        unevaluated(
            None,
            self.functions
                .values()
                .chain(self.methods.values())
                .flatten()
                .collect(),
            &self.evaluated_defaults,
            &mut defaults,
        );
        for (unit, library) in self.libs.units.iter().enumerate() {
            unevaluated(
                Some(unit),
                library
                    .functions
                    .values()
                    .chain(
                        self.lib_methods
                            .get(unit)
                            .into_iter()
                            .flat_map(HashMap::values),
                    )
                    .flatten()
                    .collect(),
                &self.evaluated_defaults,
                &mut defaults,
            );
        }

        for default in defaults {
            // A library's signature names its types in its own namespace.
            if let Some(unit) = default.unit {
                self.lib_ctx.push(LibFrame { unit });
            }
            let expected = self.resolve_declared_type_quietly(&default.declared);
            if default.unit.is_some() {
                self.lib_ctx.pop();
            }
            // A declared type that does not resolve holds the default to
            // nothing; a call that reaches the parameter reports the type.
            let Some(expected) = expected else {
                continue;
            };
            let widens = expected == Type::Float && default.found == Type::Int;
            if default.found != expected && !widens {
                self.report_mistyped_default(
                    &default.function,
                    &default.param,
                    default.default,
                    &expected,
                    &default.found,
                );
            }
        }
    }

    /// [`Self::resolve_field_type`] for a parameter's declared type, without
    /// its diagnostics: `None` when the type does not resolve.
    fn resolve_declared_type_quietly(&mut self, ty: &ast::FieldType) -> Option<Type> {
        let mark = self.diagnostics.mark();
        let resolved = self.resolve_field_type(ty, "");
        self.diagnostics.rollback(mark);
        resolved
    }

    /// Report a parameter default that does not have its parameter's declared
    /// type (`E0205`), at the default.
    fn report_mistyped_default(
        &mut self,
        function: &str,
        param: &str,
        default: Span,
        expected: &Type,
        found: &Type,
    ) {
        self.error(
            codes::E0205,
            format!(
                "`{function}` default for parameter `{param}` must be {}, found {}",
                expected.describe(),
                found.describe()
            ),
            default,
            format!("expected {}", expected.describe()),
        );
    }

    /// Shared inlining core for user functions and methods. `name` is the
    /// display/recursion-guard name; `def` is the AST template. See
    /// [`Self::expand_function_call`] for the semantics.
    pub(crate) fn expand_def(
        &mut self,
        name: &str,
        def: &ast::FuncDef,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        self.expand_def_scoped(name, def, args, span, None)
    }

    /// The compile-time integer value of an expression, when statically
    /// known: an int literal, checked integer arithmetic over known values,
    /// or a symbol carrying a propagated literal (`const_int` — an inlined
    /// function parameter bound to a literal argument). `None` for anything
    /// else; this is deliberately not a general constant folder.
    pub(crate) fn ast_const_int(&self, expr: &ast::Expr) -> Option<i64> {
        match expr {
            ast::Expr::Group { inner, .. } => self.ast_const_int(inner),
            ast::Expr::Unary {
                op: ast::UnaryOp::Neg,
                operand,
                ..
            } => self.ast_const_int(operand).and_then(i64::checked_neg),
            ast::Expr::Unary {
                op: ast::UnaryOp::Pos,
                operand,
                ..
            } => self.ast_const_int(operand),
            ast::Expr::Binary { op, lhs, rhs, .. } => {
                let lhs = self.ast_const_int(lhs)?;
                let rhs = self.ast_const_int(rhs)?;
                match op {
                    ast::BinaryOp::Add => lhs.checked_add(rhs),
                    ast::BinaryOp::Sub => lhs.checked_sub(rhs),
                    ast::BinaryOp::Mul => lhs.checked_mul(rhs),
                    ast::BinaryOp::Rem => lhs.checked_rem(rhs),
                    // Int division can retain a fractional runtime value.
                    _ => None,
                }
            }
            ast::Expr::Int { value, .. } => Some(*value),
            ast::Expr::Ident(ident) => {
                let id = self.lookup_variable(&ident.name)?;
                self.symbols[id.0 as usize].const_int
            }
            _ => None,
        }
    }

    /// Inlining core shared by local functions, methods, and library functions.
    /// The body and every omitted parameter's default belong to the
    /// definition, so they resolve names in its scope
    /// ([`Self::enter_definition_scope`]): its parameters and locals, then the
    /// globals of the namespace it was written in, never the caller's locals.
    /// `lib`, when `Some((unit, global_scope))`, names a library definition's
    /// namespace (unit index + its global scope), so those names resolve
    /// against the library, not the importer. Arguments the caller writes are
    /// evaluated first, in the caller's scope and namespace. `None` inlines a
    /// local function/method.
    pub(crate) fn expand_def_scoped(
        &mut self,
        name: &str,
        def: &ast::FuncDef,
        args: &[ast::CallArg],
        span: Span,
        lib: Option<(usize, HashMap<String, SymbolId>)>,
    ) -> HirExpr {
        let discard_result = self.discarded_call == Some(span);
        if discard_result {
            // Consume only this call's marker; argument calls still need values.
            self.discarded_call = None;
        }
        // Bind positional + named arguments to the definition's parameters
        // (E0228 on a duplicate or unknown parameter name), then the guards:
        // an expansion budget blown by an earlier call short-circuits (the
        // structured diagnostic was already reported); direct or mutual
        // recursion is E0224; a deep *acyclic* call chain (`f0` calls `f1` …
        // calls `fN`) inlines to few statements — under the node budget — yet
        // recurses once per link through the analyzer, so the inline nesting
        // depth is bounded; the argument count must lie between the count of
        // non-default params and all params (`binder::plan_def_args` also
        // decides, per parameter, whether it takes its argument or its
        // default); and a pathological deep/fan-out call graph must hit the
        // expansion budget, never a hang or OOM. The order of these steps is
        // the generic `binder::expand_protocol`, which is extracted and
        // proved; the diagnostics are `ExpandCtx`'s.
        let def_for_bind = def.clone();
        let param_names: Vec<&str> = def_for_bind
            .params
            .iter()
            .map(|param| param.name.name.as_str())
            .collect();
        let defaults: Vec<Option<&ast::Expr>> = def_for_bind
            .params
            .iter()
            .map(|p| p.default.as_ref())
            .collect();
        let shapes = binder_param_shapes(&def_for_bind.params, args);
        let has_default = binder_param_has_defaults(&def_for_bind.params);
        // The call-stack frame names the definition, not just the name, so one
        // overload may call another of the same name while direct or mutual
        // recursion of one definition is still caught.
        let frame = format!("{name}@{:?}", def.span);
        let mut ctx = ExpandCtx {
            an: self,
            name,
            frame: &frame,
            def,
            param_names: &param_names,
            args,
            span,
        };
        let (dense, plan) = match binder::expand_protocol(
            &mut ctx,
            has_default.len(),
            &shapes,
            &has_default,
            def.body.len(),
        ) {
            binder::Expansion::Error => return self.error_expr(span),
            binder::Expansion::Inline(dense, plan) => (dense, plan),
        };
        let bound = Self::dense_exprs(args, &defaults, &dense);
        let args = bound.as_slice();

        // Evaluate each argument (or its default) **once** into a hidden temp,
        // and bind the parameter name to that temp. Real arguments evaluate in
        // the caller's scope. An omitted parameter's default is part of the
        // definition, so it evaluates in the definition's scope, like the body;
        // it still runs at this call site, before any parameter is bound.
        let mut bindings: Vec<(String, SymbolId)> = Vec::with_capacity(def.params.len());
        let mut ok = true;
        for (i, param) in def.params.iter().enumerate() {
            // The binder returns one plan entry for each parameter.
            let selected = plan[i];
            // A default arrives directly (the parameter lies past the dense
            // arguments) or through the dense list (a later named argument
            // skipped this parameter).
            let from_default = match selected {
                binder::ParamSource::Arg(j) => matches!(dense[j], binder::Choice::Default(_)),
                binder::ParamSource::Default => true,
            };
            let arg_expr: &ast::Expr = match selected {
                binder::ParamSource::Arg(j) => &args[j],
                binder::ParamSource::Default => param
                    .default
                    .as_ref()
                    .expect("plan_def_args chooses a default only for a parameter that has one"),
            };
            if from_default && param.ty.is_some() {
                // This call holds the default to the declared type below, or
                // reports why it cannot, so `check_unevaluated_defaults`
                // leaves the default alone.
                self.evaluated_defaults.insert(arg_expr.span());
            }
            let declared_type = if let Some(ty) = &param.ty {
                match self.resolve_field_type(ty, "") {
                    Some(ty) => Some(ty),
                    None => {
                        ok = false;
                        continue;
                    }
                }
            } else {
                None
            };
            let caller_scopes =
                from_default.then(|| self.enter_definition_scope(def.span, lib.clone()));
            let mut value = self.analyze_expr_expecting(arg_expr, declared_type.as_ref());
            // Facts read from the source expression must resolve its names in
            // the same scope as its value did.
            let arg_const_int = self.ast_const_int(arg_expr);
            let arg_ta_input = self.ast_ta_input(arg_expr);
            if let Some(caller_scopes) = caller_scopes {
                self.leave_definition_scope(caller_scopes);
            }
            let rendering_value = self.rendering_inline.last(&value);
            if value.ty.is_error() {
                ok = false;
                continue;
            }
            if value.ty == Type::Void {
                self.error(
                    codes::E0205,
                    format!("`{name}` argument `{}` cannot be void", param.name.name),
                    arg_expr.span(),
                    "expected a value",
                );
                ok = false;
                continue;
            }
            if let Some(expected) = declared_type {
                if expected == Type::Float && value.ty == Type::Int {
                    value = Self::coerce_int_to_float(value);
                } else if value.ty != expected {
                    if from_default {
                        // The mistake is in the definition, not in this call.
                        self.report_mistyped_default(
                            &def.name.name,
                            &param.name.name,
                            arg_expr.span(),
                            &expected,
                            &value.ty,
                        );
                    } else {
                        self.error(
                            codes::E0205,
                            format!(
                                "`{name}` argument `{}` must be {}, found {}",
                                param.name.name,
                                expected.describe(),
                                value.ty.describe()
                            ),
                            arg_expr.span(),
                            format!("expected {}", expected.describe()),
                        );
                    }
                    ok = false;
                    continue;
                }
            }
            // Preserve source closure before a declared Simple/Series ceiling
            // strengthens the argument's qualifier. Host-valued literals are
            // not source-closed even when their binding has no input leaves.
            let source_closed_active_origin = self.source_closed_active_expr_is_proven(&value);
            // An explicit value-type qualifier is both an argument ceiling
            // and the parameter's qualifier inside the function. Inheriting
            // a const argument for a `series` parameter would incorrectly
            // let its result enter simple/const-only builtin parameters.
            // Reference parameters retain reference qualification regardless
            // of a written qualifier. Only scalar/color/enum declarations enter
            // this value-type branch.
            let qualified_value_parameter = param.ty.as_ref().is_some_and(|ty| {
                matches!(
                    &ty.kind,
                    ast::FieldTypeKind::Scalar(_) | ast::FieldTypeKind::Color
                ) || matches!(&ty.kind, ast::FieldTypeKind::Named(name) if self.enums.contains_key(name))
            });
            if qualified_value_parameter {
                if let Some(declared) = param.qualifier {
                    let qualifier = match declared {
                        ast::ParamQualifier::Const => Qualifier::Const,
                        ast::ParamQualifier::Input => Qualifier::Input,
                        ast::ParamQualifier::Simple => Qualifier::Simple,
                        ast::ParamQualifier::Series => Qualifier::Series,
                    };
                    if qualifier < Qualifier::Series {
                        self.defer_temporal_fixedness_at(
                            &value,
                            name,
                            &param.name.name,
                            arg_expr.span(),
                        );
                    }
                    if value.qualifier > qualifier {
                        self.error(
                            codes::E0206,
                            format!(
                                "`{name}` argument `{}` must be {} or weaker, found {}",
                                param.name.name,
                                qualifier.describe(),
                                value.qualifier.describe()
                            ),
                            arg_expr.span(),
                            format!("expected a {} value", qualifier.describe()),
                        );
                        ok = false;
                        continue;
                    }
                    value.qualifier = qualifier;
                }
            }
            let current_feed_symbol = self.current_feed_symbol_for_expr(&value);
            let current_chart_timeframe = self.current_chart_timeframe_for_expr(&value);
            let input_origin = if matches!(&value.kind, HirExprKind::ReadVar { symbol, .. }
                if self.symbols[symbol.0 as usize].persistent)
            {
                None
            } else {
                self.input_origin_for_expr(&value)
            };
            // A hidden temp that stores the argument value once per bar. The
            // parameter reads it (history on a parameter reads the temp's
            // slot, so `p[1]` works).
            let temp = self.allocate_synthetic_slot(value.ty.clone(), param.span);
            let temp_id = SymbolId(temp.0);
            self.assignment_declaration_depths
                .insert(temp_id, self.assignment_controls.len());
            self.arrow_inline_parameters.insert(temp_id);
            // The temp adopts the argument's qualifier (parameters take the
            // join of their argument qualifiers).
            self.symbols[temp_id.0 as usize].qualifier = value.qualifier;
            // An int-literal argument (or one forwarded through another
            // literal-bound parameter) stamps its value on the temp, so
            // literal-required positions inside the body — `ta.*` engine
            // lengths — see through the inlining.
            self.symbols[temp_id.0 as usize].const_int = arg_const_int;
            // Preserve typed constant values across parameter binding too;
            // request selectors and const metadata must see the same value
            // through a helper as they do through an ordinary alias.
            let persistent_argument = matches!(&value.kind, HirExprKind::ReadVar { symbol, .. }
                if self.symbols[symbol.0 as usize].persistent);
            if !persistent_argument {
                if self.const_accessor_expr_is_stable(&value) {
                    self.const_accessor_stable_symbols.insert(temp_id);
                }
                if let Some(constant) = self.const_input_value_from_hir(&value) {
                    self.const_input_values.insert(temp_id, constant);
                }
                if let Some(transparency) = self.const_color_transparency(&value) {
                    self.const_color_transparencies
                        .insert(temp_id, transparency);
                }
                if let Some(origin) = self.input_active_expr_from_hir(&value) {
                    self.input_active_origins.insert(temp_id, origin);
                    if source_closed_active_origin {
                        self.source_closed_active_origins.insert(temp_id);
                    }
                }
                if let Some(origin) = self.request_input_expr_from_hir(&value) {
                    self.request_input_expr_origins.insert(temp_id, origin);
                }
            }
            // A request may reevaluate a supported numeric or boolean
            // expression in its secondary context. Keep the original rather
            // than treating the chart-bar temporary as an HTF price value.
            // Unsupported or persistent origins still fail normal lowering.
            if matches!(value.ty, Type::Bool | Type::Int | Type::Float) {
                self.htf_expr_origins.insert(temp_id, value.clone());
            }
            if let Some(origin) = arg_ta_input.or_else(|| {
                self.selected_ta_int_input(&value)
                    .map(|input| (input, false))
            }) {
                self.ta_int_inputs.insert(temp_id, origin);
            }
            if let Some(series) = current_feed_symbol {
                self.current_feed_symbols.insert(temp_id, series);
            }
            if current_chart_timeframe {
                self.current_chart_timeframes.insert(temp_id);
            }
            if let Some(origin) = input_origin {
                self.input_origins.insert(temp_id, origin);
            }
            // Preserve dependency and source proof before the parameter body.
            self.temporal_binding(temp_id, &value);
            self.rendering_inline.attach(
                rendering_inline_provenance::Anchor::Assignment(temp_id, param.span),
                &value,
                rendering_value,
            );
            self.pending_stmts.push(HirStmt::Assign {
                symbol: temp_id,
                value,
                init: false,
                span: param.span,
            });
            bindings.push((param.name.name.clone(), temp_id));
        }
        if !ok {
            return self.error_expr(span);
        }

        // Analyze the body in the definition's scope, with a fresh scope binding
        // the parameters. The body's last statement supplies the return value;
        // its leading statements run for effect (pushed into the pending block
        // ahead of the value). Arguments were already evaluated above, in the
        // caller's scope and namespace.
        let caller_scopes = self.enter_definition_scope(def.span, lib);
        // A parameter named `_` is bound but, like a variable, never enters
        // the scope.
        for (name, id) in &bindings {
            if name != DISCARD_NAME {
                self.scopes.last_mut().unwrap().insert(name.clone(), *id);
            }
            self.function_parameters.insert(*id);
        }
        self.call_stack.push(frame);
        if self.temporal_summarized_functions.insert(def.span) {
            self.persistent_write_plan.potential_carried_reads.extend(
                persistent_writes::function_potential_reads(&def.body, self.registry),
            );
        }
        // A function body's trailing statement may be a tuple expression
        // (`... => [hi, lo]`); the caller decides whether the tuple is usable
        // (a destructure accepts it, a scalar context rejects it below).
        let result = if (discard_result && self.block_has_statement_tail(&def.body))
            || matches!(def.body.last(), Some(ast::Stmt::Once(_)))
        {
            // A helper called as a statement may end in a statement that has
            // no value (see `block_has_statement_tail`). Its effects still run
            // in the function's local scope. Once is always effect-only,
            // including in value contexts, where its Void result will be
            // rejected by the caller.
            let block = self.analyze_block(&def.body, Qualifier::Const, false);
            Some((
                block,
                HirExpr {
                    kind: HirExprKind::ConstNa,
                    ty: Type::Void,
                    qualifier: Qualifier::Const,
                    span,
                },
            ))
        } else {
            self.allow_tuple_expr = true;
            self.analyze_value_block(&def.body, def.span, None)
        };
        self.call_stack.pop();
        self.leave_definition_scope(caller_scopes);
        for (_, id) in &bindings {
            self.function_parameters.remove(id);
        }

        let Some((block, value)) = result else {
            return self.error_expr(span);
        };
        // The body statements run before the enclosing statement, after the
        // argument-temp stores already pushed above.
        self.pending_stmts.extend(block);
        self.rendering_inline
            .call(&value, bindings.iter().map(|(_, id)| *id).collect());
        value
    }

    /// Enter a definition's scope at one of its call sites, so its body or a
    /// default resolves names where the function was written rather than
    /// where it is called. This records the function's boundary and pushes an
    /// empty scope for its parameters; [`Self::lookup_variable`] then skips
    /// every caller scope below the boundary and continues in the global
    /// scope. A local definition was written at the top level of the caller's
    /// namespace, so that is the caller's global scope. A library definition
    /// (`lib`) swaps the stack to `[library globals]` and pushes its unit on
    /// `lib_ctx`, so its variables, calls and imports resolve in the library,
    /// never the importer. Pair with [`Self::leave_definition_scope`].
    fn enter_definition_scope(
        &mut self,
        definition: Span,
        lib: Option<(usize, HashMap<String, SymbolId>)>,
    ) -> CallerScopes {
        let saved = lib.map(|(unit, globals)| {
            self.lib_ctx.push(LibFrame { unit });
            std::mem::replace(&mut self.scopes, vec![globals])
        });
        // The boundary indexes the stack the definition sees: after a
        // namespace swap, before its parameter scope.
        self.function_scope_bases.push(FunctionBoundary {
            base: self.scopes.len(),
            definition,
        });
        self.scopes.push(HashMap::new());
        CallerScopes(saved)
    }

    /// Undo [`Self::enter_definition_scope`], restoring the caller's scopes
    /// and namespace.
    fn leave_definition_scope(&mut self, caller: CallerScopes) {
        self.scopes.pop();
        self.function_scope_bases.pop();
        if let Some(saved) = caller.0 {
            self.scopes = saved;
            self.lib_ctx.pop();
        }
    }

    /// Whether a helper called as a statement runs its body as statements:
    /// its tail is a loop (loop results are not yet modeled as scalar values),
    /// a `once` structure, a declaration of `_`, a call to a void runtime
    /// effect (`log.*`, `alert`, `runtime.error`, `strategy.*` orders), a call
    /// to a user function, method or library function, or an `if`/`switch`
    /// with such a tail. A helper called as a statement may return void; its effects still
    /// run in the function's local scope. A helper's result is discarded
    /// here, so a called helper is analyzed as a statement call too and
    /// decides its own tail when it is expanded: the check never follows
    /// another helper, and its cost is the size of this body's tail.
    fn block_has_statement_tail(&self, stmts: &[ast::Stmt]) -> bool {
        match stmts.last() {
            Some(
                ast::Stmt::For(_) | ast::Stmt::ForIn(_) | ast::Stmt::While(_) | ast::Stmt::Once(_),
            ) => true,
            // A declaration of `_` discards its value.
            Some(ast::Stmt::Assign { name, .. }) if name.name == DISCARD_NAME => true,
            Some(ast::Stmt::Expr {
                expr: ast::Expr::MethodCall { .. },
                ..
            }) => true,
            Some(ast::Stmt::Expr {
                expr: ast::Expr::Call { callee, .. },
                ..
            }) => self
                .registry
                .lookup_function(&callee.name)
                .is_none_or(|func| {
                    matches!(
                        func.kind,
                        BuiltinFunctionKind::Log(_)
                            | BuiltinFunctionKind::Alert
                            | BuiltinFunctionKind::RuntimeErrorFn
                            | BuiltinFunctionKind::Order(_)
                    )
                }),
            Some(ast::Stmt::If(branch)) => {
                self.block_has_statement_tail(&branch.then_branch)
                    || branch
                        .else_branch
                        .as_deref()
                        .is_some_and(|stmts| self.block_has_statement_tail(stmts))
            }
            Some(ast::Stmt::Switch(switch)) => switch
                .arms
                .iter()
                .any(|arm| self.block_has_statement_tail(&arm.body)),
            _ => false,
        }
    }
}

impl<'r> Analyzer<'r> {
    /// `str.tostring(enum)`: the member's title, which is its source
    /// `= "title"` or else its name. Enums are member indices at run time, so
    /// the call selects among the titles, evaluating the value once; a const
    /// member folds to its title, and an `na` value gives "NaN".
    fn enum_member_title(&mut self, value: HirExpr, name: &str, span: Span) -> HirExpr {
        let titles: Vec<String> = self
            .enums
            .get(name)
            .map(|info| {
                info.variants
                    .iter()
                    .map(|variant| {
                        variant
                            .title
                            .clone()
                            .unwrap_or_else(|| variant.name.clone())
                    })
                    .collect()
            })
            .unwrap_or_default();
        let text = |text: String| HirExpr {
            kind: HirExprKind::ConstString(text),
            ty: Type::String,
            qualifier: Qualifier::Const,
            span,
        };
        // A member known at compile time, directly or through a const alias,
        // folds to its title.
        if let Some(InputValue::Enum { index, .. }) = self.const_input_value_from_hir(&value) {
            if let Some(title) = titles.get(index as usize) {
                return text(title.clone());
            }
        }
        let ty = value.ty.clone();
        let qualifier = value.qualifier;
        let slot = self.allocate_synthetic_slot(ty.clone(), value.span);
        self.temporal_binding(SymbolId(slot.0), &value);
        // A balanced selection on the member index: its depth grows with
        // log2 of the member count, so a large enum cannot nest the
        // expression deep enough to exhaust the stack. An `na` value fails
        // every comparison and reaches a leaf's "NaN".
        let read = HirExpr {
            kind: HirExprKind::ReadVar {
                symbol: SymbolId(slot.0),
                offset: 0,
            },
            ty: ty.clone(),
            qualifier,
            span,
        };
        let compare = |op: BinOp, index: usize| HirExpr {
            kind: HirExprKind::Binary {
                op,
                lhs: Box::new(read.clone()),
                rhs: Box::new(HirExpr {
                    kind: HirExprKind::ConstInt(index as i64),
                    ty: ty.clone(),
                    qualifier: Qualifier::Const,
                    span,
                }),
            },
            ty: Type::Bool,
            qualifier,
            span,
        };
        let choose = |cond: HirExpr, then_e: HirExpr, else_e: HirExpr| HirExpr {
            kind: HirExprKind::Ternary {
                cond: Box::new(cond),
                then_e: Box::new(then_e),
                else_e: Box::new(else_e),
            },
            ty: Type::String,
            qualifier,
            span,
        };
        fn select(
            lo: usize,
            hi: usize,
            titles: &[String],
            text: &dyn Fn(String) -> HirExpr,
            compare: &dyn Fn(BinOp, usize) -> HirExpr,
            choose: &dyn Fn(HirExpr, HirExpr, HirExpr) -> HirExpr,
        ) -> HirExpr {
            match hi - lo {
                0 => text("NaN".to_string()),
                1 => choose(
                    compare(BinOp::Eq, lo),
                    text(titles[lo].clone()),
                    text("NaN".to_string()),
                ),
                width => {
                    let mid = lo + width / 2;
                    choose(
                        compare(BinOp::Lt, mid),
                        select(lo, mid, titles, text, compare, choose),
                        select(mid, hi, titles, text, compare, choose),
                    )
                }
            }
        }
        let result = select(0, titles.len(), &titles, &text, &compare, &choose);
        HirExpr {
            kind: HirExprKind::EvalSequence {
                bindings: vec![(slot, value)],
                result: Box::new(result),
            },
            ty: Type::String,
            qualifier,
            span,
        }
    }
}

/// The three `timestamp` overloads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TimestampForm {
    /// `timestamp(year, month, day, hour, minute, second)`, in UTC.
    Utc,
    /// `timestamp(timezone, year, month, day, hour, minute, second)`.
    Timezone,
    /// `timestamp(dateString)`, folded to a const int.
    DateString,
}

impl TimestampForm {
    fn param_names(self) -> &'static [&'static str] {
        match self {
            TimestampForm::Utc => &TIMESTAMP_UTC_PARAM_NAMES,
            TimestampForm::Timezone => &TIMESTAMP_TZ_PARAM_NAMES,
            TimestampForm::DateString => &TIMESTAMP_DATE_STRING_PARAM_NAMES,
        }
    }
}

impl<'r> Analyzer<'r> {
    /// A type-cast builtin: the scalar casts
    /// `int`/`float`/`bool`/`string`/`color` and the drawing casts
    /// `line`/`label`/`box`/`table`/`linefill`. Each takes exactly one argument
    /// (`E0204` otherwise), positionally or by its name `x`.
    ///
    /// - `int(x)`   — `x` is numeric or `na`; truncates a float toward zero.
    /// - `float(x)` — `x` is numeric or `na`; widens an int.
    /// - `bool(x)`  — `x` is numeric/bool or `na`; `na`/`0`/`false` → `false`.
    /// - `string(x)`— `x` is any scalar; equals `str.tostring(x)` (reuses that
    ///   `PureFunc`).
    /// - `color(x)` — `x` is a color or `na`; identity.
    /// - `line(x)`/… — `x` is `na` or the matching drawing object; a
    ///   compile-time-checked identity (returns `x` retyped; no runtime op).
    pub(crate) fn analyze_cast_call(
        &mut self,
        name: &str,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        let Some(bound) = self.bind_positional(name, &["x"], args, span) else {
            for a in args {
                let _ = self.analyze_expr(&a.value);
            }
            return self.error_expr(span);
        };
        if bound.len() != 1 {
            self.error(
                codes::E0204,
                format!("`{name}` expects 1 argument, found {}", bound.len()),
                span,
                "a type cast takes exactly one value",
            );
            for a in &bound {
                let _ = self.analyze_expr(a);
            }
            return self.error_expr(span);
        }
        let arg = &bound[0];

        // Drawing casts: `na` or a matching drawing object passes through; a
        // wrong type is `E0205`. The cast is an identity — return the analyzed
        // argument retyped to the target object type (or, for `na`, adopt it).
        if let Some(kind) = DrawKind::from_cast_name(name) {
            let type_name = kind.type_name();
            let target = Type::Object(type_name.to_string());
            let expected = target.clone();
            let mut h = self.analyze_expr_expecting(arg, Some(&expected));
            if matches!(h.kind, HirExprKind::ConstNa) || h.ty.is_error() {
                // `na` (or an already-errored operand): adopt the target type.
                h.ty = target;
                return h;
            }
            if h.ty != target {
                self.error(
                    codes::E0205,
                    format!(
                        "`{name}` expects `{type_name}` or `na`, found {}",
                        h.ty.describe()
                    ),
                    arg.span(),
                    format!("cannot cast this to `{type_name}`"),
                );
                return self.error_expr(span);
            }
            return h;
        }

        // The literal missing operand has the constant string-cast value "".
        // Preserve typed-missing variable storage and the existing
        // present-scalar formatting path separately.
        if name == "string" {
            let h = self.analyze_expr_expecting(arg, Some(&Type::Float));
            if !h.ty.is_error()
                && !matches!(h.ty, Type::Int | Type::Float | Type::Bool | Type::String)
            {
                self.error(
                    codes::E0205,
                    format!("`string` expects a scalar value, found {}", h.ty.describe()),
                    arg.span(),
                    "string() casts int/float/bool/string (or na)",
                );
                return self.error_expr(span);
            }
            let qualifier = h.qualifier;
            if matches!(h.kind, HirExprKind::ConstNa) {
                return HirExpr {
                    kind: HirExprKind::ConstString(String::new()),
                    ty: Type::String,
                    qualifier,
                    span,
                };
            }
            return HirExpr {
                kind: HirExprKind::CallPure {
                    func: PureFunc::StrTostring,
                    args: vec![h],
                },
                ty: Type::String,
                qualifier,
                span,
            };
        }

        // Scalar casts: validate the input type, then emit a `CallPure`. The
        // `na_hint` types a bare `na` operand: `int`/`float` widen it to float
        // (na is uninferable between int/float on its own), `bool` to bool,
        // `color` to color — so `int(na)`/`bool(na)`/`color(na)` are all
        // well-typed rather than `E0220`.
        let (pure, out_ty, accepts, na_hint): (PureFunc, Type, &[Type], Type) = match name {
            "int" => (
                PureFunc::CastInt,
                Type::Int,
                &[Type::Int, Type::Float],
                Type::Float,
            ),
            "float" => (
                PureFunc::CastFloat,
                Type::Float,
                &[Type::Int, Type::Float],
                Type::Float,
            ),
            "bool" => (
                PureFunc::CastBool,
                Type::Bool,
                &[Type::Int, Type::Float, Type::Bool],
                // Explicit bool(na) converts a missing numeric value to false.
                Type::Float,
            ),
            "color" => (
                PureFunc::CastColor,
                Type::Color,
                &[Type::Color],
                Type::Color,
            ),
            _ => unreachable!("is_cast_name gate"),
        };
        let h = self.analyze_expr_expecting(arg, Some(&na_hint));
        if !h.ty.is_error() && !accepts.contains(&h.ty) {
            let list = accepts
                .iter()
                .map(Type::describe)
                .collect::<Vec<_>>()
                .join("/");
            self.error(
                codes::E0205,
                format!("`{name}` expects {list} (or na), found {}", h.ty.describe()),
                arg.span(),
                format!("cannot cast this to `{}`", out_ty.describe()),
            );
            return self.error_expr(span);
        }
        let qualifier = h.qualifier;
        HirExpr {
            kind: HirExprKind::CallPure {
                func: pure,
                args: vec![h],
            },
            ty: out_ty,
            qualifier,
            span,
        }
    }

    /// A pure builtin call (`math.*`, `na`, `nz`): check arity against the
    /// table (including small variadics), check each argument against its
    /// parameter's type rule, then apply the function's result rule —
    /// inserting `IntToFloat` coercions and materializing optional-argument
    /// defaults so the emitted [`HirExprKind::CallPure`] is fully explicit.
    pub(crate) fn analyze_pure_call(
        &mut self,
        func: &BuiltinFunction,
        pure: PureFunc,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        // The `strategy.*` trade accessors and conversions read broker state and
        // require a `strategy()` declaration (E0241), like the `strategy.*` series getters.
        if pure.is_strategy() && self.strategy.is_none() {
            self.error(
                codes::E0241,
                format!("`{}` requires a `strategy(...)` declaration", func.name),
                span,
                "add a `strategy(\"...\")` declaration to read strategy state",
            );
            return self.error_expr(span);
        }
        // A `ticker.kagi` call with three arguments, or one naming `param` or
        // `style`, selects the style-bearing overload, which renames `reversal`
        // to `param` and adds a `style` parameter. Select the signature before binding;
        // treating these names as aliases would incorrectly accept `param`
        // without a style or `reversal` with one.
        let kagi_style_signature;
        let func = if pure == PureFunc::Ticker(TickerFunc::Kagi)
            && (args.len() >= 3
                || args.iter().any(|arg| {
                    arg.name
                        .as_ref()
                        .is_some_and(|name| matches!(name.name.as_str(), "param" | "style"))
                })) {
            let mut signature = func.clone();
            signature.params[1].name = "param";
            let mut style = signature.params[0].clone();
            style.name = "style";
            signature.params.push(style);
            kagi_style_signature = signature;
            &kagi_style_signature
        } else {
            func
        };
        let param_names: Vec<&str> = func.params.iter().map(|p| p.name).collect();
        // A named argument may bind a later parameter and leave an earlier
        // optional one unbound. The emitted `CallPure` needs a dense argument
        // list, so fill each such interior gap with the parameter's declared
        // default; an optional parameter without one is still `E0204`.
        let gap_defaults: Vec<Option<ast::Expr>> = func
            .params
            .iter()
            .map(|p| gap_default_expr(p, span))
            .collect();
        let gap_refs: Vec<Option<&ast::Expr>> = gap_defaults.iter().map(Option::as_ref).collect();
        let bound = match self.bind_positional_with_defaults(
            func.name,
            &param_names,
            &gap_refs,
            args,
            span,
        ) {
            Some(a) => a,
            None => return self.error_expr(span),
        };
        let args = bound.as_slice();
        // Arity (E0204): variadic functions repeat their last parameter up
        // to `variadic_max` total arguments.
        let required = func.params.iter().filter(|p| p.required).count();
        let max = func.variadic_max.unwrap_or(func.params.len());
        if args.len() < required || args.len() > max {
            let expected = if required == max {
                format!("{required}")
            } else {
                format!("{required} to {max}")
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

        // Arguments: analyze with the parameter-driven expected type (for
        // `na` literals) and check each parameter's type rule.
        let mut checked: Vec<HirExpr> = Vec::with_capacity(args.len());
        let mut ok = true;
        for (i, arg) in args.iter().enumerate() {
            let param = &func.params[i.min(func.params.len() - 1)];
            let expected = param.expected_type();
            let hir = self.analyze_expr_expecting(arg, Some(&expected));
            if param.max_qualifier < Qualifier::Series {
                self.defer_temporal_fixedness_at(&hir, func.name, param.name, arg.span());
            }
            if hir.ty.is_error() {
                ok = false;
                checked.push(hir);
                continue;
            }
            // `str.tostring` also writes arrays and matrices: of
            // int/float/bool/string without a format, of int/float with one.
            let collection_of = |ty: &Type, elems: &[Type]| match ty {
                Type::Array(elem) | Type::Matrix(elem) => elems.contains(elem.as_ref()),
                _ => false,
            };
            let type_ok = if pure == PureFunc::StrTostring && args.len() == 2 && i == 0 {
                // The formatted overload accepts numeric values only.
                is_numeric(&hir.ty) || collection_of(&hir.ty, &[Type::Int, Type::Float])
            } else {
                match param.type_rule {
                    ParamTypeRule::Declared => {
                        hir.ty == param.ty || (param.ty == Type::Float && hir.ty == Type::Int)
                    }
                    ParamTypeRule::Numeric => is_numeric(&hir.ty),
                    ParamTypeRule::NumericOrBool => is_numeric(&hir.ty) || hir.ty == Type::Bool,
                    ParamTypeRule::NumericOrColor => is_numeric(&hir.ty) || hir.ty == Type::Color,
                    ParamTypeRule::AnyValue => hir.ty != Type::Void,
                    ParamTypeRule::NonBooleanValue => {
                        !matches!(hir.ty, Type::Void | Type::Bool) && !hir.ty.is_text_format()
                    }
                    ParamTypeRule::StringOnly => hir.ty == Type::String,
                    ParamTypeRule::Stringable => {
                        is_numeric(&hir.ty)
                            || matches!(hir.ty, Type::Bool | Type::String)
                            || (pure == PureFunc::StrTostring
                                && args.len() == 1
                                && (matches!(hir.ty, Type::Enum(_))
                                    || collection_of(
                                        &hir.ty,
                                        &[Type::Int, Type::Float, Type::Bool, Type::String],
                                    )))
                    }
                }
            };
            if !type_ok {
                let expected_desc = if pure == PureFunc::StrTostring && args.len() == 2 && i == 0 {
                    "int or float, or an array or matrix of them".to_string()
                } else {
                    match param.type_rule {
                        ParamTypeRule::Declared => param.ty.describe(),
                        other => other.describe().to_string(),
                    }
                };
                // Fixed-type violations are the ordinary argument-type
                // error; a void argument to an any-typed parameter has no
                // fixed expected type, so it gets the pure-builtin code.
                let (code, label) = if param.type_rule == ParamTypeRule::AnyValue {
                    (codes::E0221, "effect calls produce no value".to_string())
                } else {
                    (codes::E0205, format!("expected {expected_desc}"))
                };
                self.error(
                    code,
                    format!(
                        "`{}` argument `{}` must be {expected_desc}, found {}",
                        func.name,
                        param.name,
                        hir.ty.describe()
                    ),
                    arg.span(),
                    label,
                );
                ok = false;
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
            checked.push(hir);
        }
        if !ok {
            return self.error_expr(span);
        }
        if pure == PureFunc::StrTostring && checked.len() == 1 {
            if let Type::Enum(name) = checked[0].ty.clone() {
                return self.enum_member_title(checked.remove(0), &name, span);
            }
        }

        // math.round precision: a statically-known precision outside
        // A statically known precision outside the `[0, 100]` range is a compile
        // error, not a clamp: a literal (or const-folded) precision is rejected
        // at compile time (E0248); only a genuinely runtime-computed precision
        // reaches the clamp in `pinewasm_num::clamp_precision`.
        //
        // Both halves of the guard are load-bearing. `ast_const_int` folds
        // literals, parentheses, negation, and inlined const parameters, and
        // nothing else. The `Const` qualifier then excludes a value that is
        // merely *foldable* without being fixed at compile time: `analyze_declare`
        // stamps an `input.int(...)` default into `Symbol::const_int` (so a
        // `ta.*` length can bake it), but the host can override that input, so
        // its default must reach the runtime clamp rather than be rejected here.
        // Without the qualifier test, `p = input.int(-1)` would error while the
        // equivalent inline `input.int(-1)` would not.
        if pure == PureFunc::MathRound
            && args.len() >= 2
            && checked[1].qualifier == Qualifier::Const
        {
            if let Some(p) = self.ast_const_int(&args[1]) {
                if !(0..=100).contains(&p) {
                    self.error(
                        codes::E0248,
                        format!(
                            "Invalid value \"{p}\" for \"{}\" parameter of the \"{}()\" function. It must be between 0 and 100",
                            func.params[1].name, func.name
                        ),
                        args[1].span(),
                        "precision must be between 0 and 100",
                    );
                    return self.error_expr(span);
                }
            }
        }

        // Result rule: decide the result type, coerce int arguments where
        // the rule calls for floats, and materialize omitted defaults.
        let result = func
            .pure_result
            .expect("pure functions declare a result rule");
        let coerce_numeric_args = |analyzer_args: Vec<HirExpr>| -> Vec<HirExpr> {
            analyzer_args
                .into_iter()
                .enumerate()
                .map(|(i, a)| {
                    let param = &func.params[i.min(func.params.len() - 1)];
                    if param.type_rule == ParamTypeRule::Numeric {
                        Self::coerce_int_to_float(a)
                    } else {
                        a
                    }
                })
                .collect()
        };
        let (mut final_args, ty) = match result {
            PureResult::Float => (coerce_numeric_args(checked), Type::Float),
            PureResult::Bool => (checked, Type::Bool),
            PureResult::Int => (checked, Type::Int),
            PureResult::String => (checked, Type::String),
            PureResult::Preserve => {
                let ty = checked[0].ty.clone();
                (checked, ty)
            }
            PureResult::IntIfAllInt => {
                if checked.iter().all(|a| a.ty == Type::Int) {
                    (checked, Type::Int)
                } else {
                    (coerce_numeric_args(checked), Type::Float)
                }
            }
            PureResult::Round => {
                if checked.len() == 1 {
                    // No precision: int result, int input passes through.
                    (checked, Type::Int)
                } else {
                    // With precision: float result; the value is coerced,
                    // the precision stays int.
                    (coerce_numeric_args(checked), Type::Float)
                }
            }
            PureResult::Nz => match self.nz_arguments(func, args, checked, span) {
                Some(pair) => pair,
                None => return self.error_expr(span),
            },
            // color.* constructors: coerce any numeric (int) arguments to
            // float where the parameter is `Numeric` (color.from_gradient's
            // value/bottom/top positions); the color and int parameters pass
            // through unchanged.
            PureResult::Color => (coerce_numeric_args(checked), Type::Color),
        };

        // Materialize omitted optional arguments that declare a default
        // (`math.random`'s float bounds, `str.replace`'s int occurrence),
        // keeping the argument list a plain positional prefix of the
        // parameter list. Optional parameters *without* a default (e.g.
        // `str.substring`'s `end`, `str.repeat`'s `separator`) are left
        // omitted — their absence changes the emitted `argc` and thus the
        // VM's behavior — so the loop stops at the first defaultless param.
        for param in func.params.iter().skip(final_args.len()) {
            if let Some(series) = param.default_series {
                // Context defaults retain the type and qualifier of the
                // same builtin read written out by hand.
                final_args.push(HirExpr {
                    kind: HirExprKind::ReadBuiltin { series, offset: 0 },
                    ty: series.value_type(),
                    qualifier: series.current_read_qualifier(),
                    span,
                });
            } else if let Some(default) = param.default_int {
                final_args.push(HirExpr {
                    kind: HirExprKind::ConstInt(default),
                    ty: Type::Int,
                    qualifier: Qualifier::Const,
                    span,
                });
            } else if let Some(default) = param.default_float {
                final_args.push(HirExpr {
                    kind: HirExprKind::ConstFloat(default),
                    ty: Type::Float,
                    qualifier: Qualifier::Const,
                    span,
                });
            } else {
                break;
            }
        }

        let mut qualifier = final_args
            .iter()
            .fold(Qualifier::Const, |q, a| q.join(a.qualifier));
        if pure == PureFunc::Nz {
            qualifier = qualifier.join(Qualifier::Simple);
        }
        if pure == PureFunc::Na {
            // The numeric form of na() returns at least simple; other argument
            // types return series.
            qualifier = if matches!(
                final_args.first().map(|arg| &arg.ty),
                Some(Type::Int | Type::Float)
            ) {
                qualifier.join(Qualifier::Simple)
            } else {
                Qualifier::Series
            };
        }
        if matches!(
            pure,
            PureFunc::StrTostring
                | PureFunc::StrFormat
                | PureFunc::StrReplaceAll
                | PureFunc::StrMatch
        ) {
            // These string functions return at least simple; the join below
            // applies that minimum.
            qualifier = qualifier.join(Qualifier::Simple);
        }
        if qualifier == Qualifier::Input
            && matches!(
                pure,
                PureFunc::StrContains
                    | PureFunc::StrSubstring
                    | PureFunc::StrReplace
                    | PureFunc::StrLower
                    | PureFunc::StrUpper
                    | PureFunc::StrStartswith
                    | PureFunc::StrEndswith
                    | PureFunc::StrPos
                    | PureFunc::StrLength
            )
        {
            // For these functions, input-qualified arguments give a simple
            // result.
            qualifier = Qualifier::Simple;
        }
        if pure == PureFunc::Ticker(TickerFunc::Kagi) {
            // Both `ticker.kagi` forms return at least simple. The style
            // argument accepts series in both forms; the first two arguments
            // set the result qualifier.
            qualifier = final_args[..2]
                .iter()
                .fold(Qualifier::Simple, |q, a| q.join(a.qualifier));
        } else if matches!(
            pure,
            PureFunc::Ticker(_) | PureFunc::SyminfoPrefix | PureFunc::SyminfoTicker
        ) {
            // `ticker.*`, `syminfo.prefix` and `syminfo.ticker` take string
            // arguments. Const or input arguments still give at least a simple
            // result.
            qualifier = qualifier.join(Qualifier::Simple);
        }
        if matches!(pure, PureFunc::MathRandom | PureFunc::StrFormatTime) || pure.is_strategy() {
            // Random draws and value-returning strategy accessors depend on
            // execution or broker state, so their results are always
            // series-qualified.
            qualifier = qualifier.join(Qualifier::Series);
        }
        if pure == PureFunc::ColorFromGradient {
            // `color.from_gradient` results are always series-qualified, including
            // for an all-literal call. `dynamic_color_calculation`, set below,
            // turns off automatic style color selectors.
            qualifier = Qualifier::Series;
        }
        if pure == PureFunc::ColorFromGradient
            || (matches!(pure, PureFunc::ColorNew | PureFunc::ColorRgb)
                && final_args
                    .iter()
                    .any(|arg| arg.qualifier > Qualifier::Const))
        {
            self.dynamic_color_calculation = true;
        }
        if pure == PureFunc::ColorTFloat && qualifier == Qualifier::Const {
            if let Some(transparency) = final_args
                .first()
                .and_then(|color| self.const_color_transparency(color))
            {
                return HirExpr {
                    kind: HirExprKind::ConstFloat(transparency),
                    ty,
                    qualifier,
                    span,
                };
            }
        }
        HirExpr {
            kind: HirExprKind::CallPure {
                func: pure,
                args: final_args,
            },
            ty,
            qualifier,
            span,
        }
    }

    /// Analyze a `timestamp(...)` call, dispatching among the three
    /// overloads by arity and the first argument's type. One string is folded
    /// as the const date-string form. A string followed by numeric fields is
    /// the leading `timezone` form ([`PureFunc::TimestampTz`]); a numeric first
    /// argument selects the UTC form ([`PureFunc::Timestamp`]).
    ///
    /// All numeric fields must be `series int` (int only); the timezone is a
    /// string. Trailing optional fields default to `0`.
    pub(crate) fn analyze_timestamp_call(&mut self, args: &[ast::CallArg], span: Span) -> HirExpr {
        if args.iter().any(|a| a.name.is_some()) {
            return self.analyze_named_timestamp_call(args, span);
        }
        let values: Vec<&ast::Expr> = args.iter().map(|a| &a.value).collect();
        // Peek at the first argument's type to select the overload.
        let Some(first) = values.first().map(|value| self.analyze_expr(value)) else {
            return self.timestamp_without_fields(span);
        };
        self.timestamp_from_fields(first, &values, None, span)
    }

    /// A `timestamp(...)` call with named arguments. A leading positional
    /// argument fills the overload's first slot, so its type selects between
    /// the field and timezone forms exactly as in a positional call; it is
    /// analyzed once and that analysis is reused. Otherwise the names select
    /// the overload: `dateString` the date-string form, `timezone` the timezone
    /// form, anything else the field form. The arguments are bound to that
    /// overload's parameter names and then checked as a positional call.
    fn analyze_named_timestamp_call(&mut self, args: &[ast::CallArg], span: Span) -> HirExpr {
        let named = |param: &str| {
            args.iter()
                .any(|a| a.name.as_ref().is_some_and(|name| name.name == param))
        };
        let leading = args
            .first()
            .filter(|a| a.name.is_none())
            .map(|a| self.analyze_expr(&a.value));
        let form = if named(TIMESTAMP_DATE_STRING_PARAM_NAMES[0]) {
            TimestampForm::DateString
        } else if named("timezone") || leading.as_ref().is_some_and(|h| h.ty == Type::String) {
            TimestampForm::Timezone
        } else {
            TimestampForm::Utc
        };
        let param_names = form.param_names();
        // A name may skip the optional clock fields, which default to 0.
        let zero = ast::Expr::Int { value: 0, span };
        let defaults: Vec<Option<&ast::Expr>> = param_names
            .iter()
            .map(|name| matches!(*name, "hour" | "minute" | "second").then_some(&zero))
            .collect();
        let Some(bound) =
            self.bind_positional_with_defaults("timestamp", param_names, &defaults, args, span)
        else {
            for a in args.iter().skip(usize::from(leading.is_some())) {
                let _ = self.analyze_expr(&a.value);
            }
            return self.error_expr(span);
        };
        let values: Vec<&ast::Expr> = bound.iter().collect();
        let first = match leading {
            Some(first) => first,
            None => match values.first() {
                Some(value) => self.analyze_expr(value),
                None => return self.timestamp_without_fields(span),
            },
        };
        self.timestamp_from_fields(first, &values, Some(form), span)
    }

    fn timestamp_without_fields(&mut self, span: Span) -> HirExpr {
        self.error(
            codes::E0204,
            "`timestamp` requires at least a year, month, and day",
            span,
            "timestamp(year, month, day) or timestamp(timezone, year, month, day)",
        );
        self.error_expr(span)
    }

    /// The overload dispatch and field checks shared by positional and named
    /// calls. `first` is the analysis of `values[0]`, done once by the caller.
    /// A named call passes the form its names selected, and the arguments are
    /// checked against that form; a positional call passes `None`, and the
    /// first argument's type and the arity select the form.
    fn timestamp_from_fields(
        &mut self,
        first: HirExpr,
        values: &[&ast::Expr],
        form: Option<TimestampForm>,
        span: Span,
    ) -> HirExpr {
        let form = form.unwrap_or(match (first.ty == Type::String, values.len()) {
            (true, 1) => TimestampForm::DateString,
            (true, _) => TimestampForm::Timezone,
            (false, _) => TimestampForm::Utc,
        });
        // The one-string overload is a const expression. Fold it
        // during analysis so it is valid anywhere a const int is required —
        // notably `input.time(timestamp("1 Jan 2025 00:00 UTC"), ...)` —
        // and so no runtime/CSIL opcode needs to carry locale-sensitive parsing.
        if form == TimestampForm::DateString {
            if first.ty.is_error() {
                return self.error_expr(span);
            }
            if first.ty != Type::String {
                self.error(
                    codes::E0205,
                    format!(
                        "`timestamp` argument `dateString` must be string, found {}",
                        first.ty.describe()
                    ),
                    values[0].span(),
                    "expected a date string",
                );
                return self.error_expr(span);
            }
            self.defer_temporal_fixedness_at(&first, "timestamp", "dateString", values[0].span());
            let parsed = self
                .const_input_value_from_hir(&first)
                .and_then(|value| match value {
                    InputValue::String(value) => Self::parse_timestamp_date_string(&value),
                    _ => None,
                });
            return match parsed {
                Some(value) => HirExpr {
                    kind: HirExprKind::ConstInt(value),
                    ty: Type::Int,
                    qualifier: Qualifier::Const,
                    span,
                },
                None => {
                    self.error(
                        codes::E0204,
                        "`timestamp` date string is not a recognized calendar date",
                        values[0].span(),
                        "use a date such as \"1 Jan 2025 00:00 UTC\"",
                    );
                    self.error_expr(span)
                }
            };
        }
        let tz_overload = form == TimestampForm::Timezone;
        // (variant, field parameter names, required-field count) per overload.
        // The clock fields are optional in both forms.
        let (variant, field_names, required): (PureFunc, &[&str], usize) = if tz_overload {
            (PureFunc::TimestampTz, &TIMESTAMP_TZ_PARAM_NAMES, 4)
        } else {
            (PureFunc::Timestamp, &TIMESTAMP_UTC_PARAM_NAMES, 3)
        };
        if values.len() < required || values.len() > field_names.len() {
            self.error(
                codes::E0204,
                format!(
                    "`timestamp` expects {required} to {} argument(s), found {}",
                    field_names.len(),
                    values.len()
                ),
                span,
                format!("expected {required} to {} argument(s)", field_names.len()),
            );
            // `values[0]` was analyzed as `first`.
            for value in values.iter().skip(1) {
                let _ = self.analyze_expr(value);
            }
            return self.error_expr(span);
        }
        // Type-check each provided argument: field 0 is a string for the tz
        // overload, every other field is a `series int`.
        let mut checked: Vec<HirExpr> = Vec::with_capacity(field_names.len());
        let mut ok = true;
        for (i, value) in values.iter().enumerate() {
            let hir = if i == 0 {
                first.clone()
            } else {
                self.analyze_expr(value)
            };
            if hir.ty.is_error() {
                ok = false;
                checked.push(hir);
                continue;
            }
            let expect_string = tz_overload && i == 0;
            let type_ok = if expect_string {
                hir.ty == Type::String
            } else {
                hir.ty == Type::Int
            };
            if !type_ok {
                self.error(
                    codes::E0205,
                    format!(
                        "`timestamp` argument `{}` must be {}, found {}",
                        field_names[i],
                        if expect_string { "string" } else { "int" },
                        hir.ty.describe()
                    ),
                    value.span(),
                    if expect_string {
                        "expected a timezone string"
                    } else {
                        "expected an int"
                    },
                );
                ok = false;
            }
            checked.push(hir);
        }
        if !ok {
            return self.error_expr(span);
        }
        // Materialize the omitted trailing int fields as `0`.
        while checked.len() < field_names.len() {
            checked.push(HirExpr {
                kind: HirExprKind::ConstInt(0),
                ty: Type::Int,
                qualifier: Qualifier::Const,
                span,
            });
        }
        let qualifier = checked
            .iter()
            .fold(Qualifier::Const, |q, a| q.join(a.qualifier));
        HirExpr {
            kind: HirExprKind::CallPure {
                func: variant,
                args: checked,
            },
            ty: Type::Int,
            qualifier,
            span,
        }
    }

    /// Parse the English date-string timestamp form. Month names
    /// are locale-independent English abbreviations, the clock is optional,
    /// and a trailing numeric offset may be `-0500`, `+05:30`, `UTC-5`, or
    /// `GMT+0530`. Omitted timezone means UTC.
    fn parse_timestamp_date_string(source: &str) -> Option<i64> {
        // The parser also accepts the ISO 8601 and numeric year-first forms.
        // Keep these strict: an unrecognized
        // spelling must not silently become a different calendar date.
        if let Some((fields, offset)) = Self::parse_year_first_timestamp(source) {
            return Self::epoch_ms_from_civil(fields, offset);
        }
        let normalized = source.replace(',', " ");
        let parts: Vec<&str> = normalized.split_whitespace().collect();
        if parts.len() < 3 || parts.len() > 5 {
            return None;
        }
        let month_number = |month: &str| {
            Some(match month.to_ascii_lowercase().as_str() {
                "jan" | "january" => 1,
                "feb" | "february" => 2,
                "mar" | "march" => 3,
                "apr" | "april" => 4,
                "may" => 5,
                "jun" | "june" => 6,
                "jul" | "july" => 7,
                "aug" | "august" => 8,
                "sep" | "sept" | "september" => 9,
                "oct" | "october" => 10,
                "nov" | "november" => 11,
                "dec" | "december" => 12,
                _ => return None,
            })
        };
        let (day, month, year) = if let Some(month) = month_number(parts[1]) {
            (
                parts[0].parse::<i64>().ok()?,
                month,
                parts[2].parse::<i64>().ok()?,
            )
        } else {
            let month = month_number(parts[0])?;
            (
                parts[1].parse::<i64>().ok()?,
                month,
                parts[2].parse::<i64>().ok()?,
            )
        };
        if !(1..=31).contains(&day) {
            return None;
        }
        let mut hour = 0;
        let mut minute = 0;
        let mut second = 0;
        if let Some(clock) = parts.get(3) {
            let fields: Vec<&str> = clock.split(':').collect();
            if !(2..=3).contains(&fields.len()) {
                return None;
            }
            hour = fields[0].parse().ok()?;
            minute = fields[1].parse().ok()?;
            second = fields.get(2).map_or(Some(0), |value| value.parse().ok())?;
            if !(0..=23).contains(&hour)
                || !(0..=59).contains(&minute)
                || !(0..=59).contains(&second)
            {
                return None;
            }
        }
        let timezone = parts.get(4).copied().unwrap_or("UTC");
        Self::epoch_ms_in_timezone([year, month, day, hour, minute, second], timezone)
    }

    /// Parse the year-first date strings: the ISO 8601 form and the numeric
    /// `YYYY MM DD` form. Returns the civil fields and the UTC offset in
    /// seconds, which is zero unless the ISO form carries one.
    fn parse_year_first_timestamp(source: &str) -> Option<([i64; 6], i64)> {
        let (fields, offset) = if let Some(iso) = Self::parse_iso_8601_timestamp(source) {
            iso
        } else {
            let parts: Vec<_> = source.split_whitespace().collect();
            if parts.len() != 3 || parts[0].len() != 4 {
                return None;
            }
            (
                [
                    parts[0].parse().ok()?,
                    parts[1].parse().ok()?,
                    parts[2].parse().ok()?,
                    0,
                    0,
                    0,
                ],
                0,
            )
        };
        let [year, month, day, hour, minute, second] = fields;
        let leap_year = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let max_day = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if leap_year => 29,
            2 => 28,
            _ => return None,
        };
        if !(1..=max_day).contains(&day)
            || !(0..=23).contains(&hour)
            || !(0..=59).contains(&minute)
            || !(0..=59).contains(&second)
        {
            return None;
        }
        Some((fields, offset))
    }

    /// Split the ISO 8601 shape `YYYY-MM-DDThh:mm:ss±hh:mm` into
    /// civil fields and a UTC offset in seconds. The clock is optional, as are
    /// its seconds, and an offset may only follow a clock. Every field is
    /// fixed-width ASCII digits; ranges are checked by the caller. The `Z`
    /// designator and a space in place of `T` are deliberately not accepted.
    fn parse_iso_8601_timestamp(source: &str) -> Option<([i64; 6], i64)> {
        let field = |text: &str, width: usize| -> Option<i64> {
            if text.len() != width || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            text.parse().ok()
        };
        let (date, clock) = match source.split_once('T') {
            Some((date, clock)) => (date, Some(clock)),
            None => (source, None),
        };
        let date: Vec<&str> = date.split('-').collect();
        let [year, month, day] = date[..] else {
            return None;
        };
        let mut fields = [field(year, 4)?, field(month, 2)?, field(day, 2)?, 0, 0, 0];
        let Some(clock) = clock else {
            return Some((fields, 0));
        };
        let (clock, offset) = match clock.find(['+', '-']) {
            Some(sign) => {
                let (clock, offset) = clock.split_at(sign);
                // `fixed_timezone_offset_seconds` maps a malformed offset to
                // UTC, so require the exact `±hh:mm` shape and range first.
                let (hours, minutes) = offset[1..].split_once(':')?;
                let (hours, minutes) = (field(hours, 2)?, field(minutes, 2)?);
                if !(0..=59).contains(&minutes) || hours * 60 + minutes > 14 * 60 {
                    return None;
                }
                (clock, Self::fixed_timezone_offset_seconds(offset))
            }
            None => (clock, 0),
        };
        let clock: Vec<&str> = clock.split(':').collect();
        if !(2..=3).contains(&clock.len()) {
            return None;
        }
        for (slot, text) in fields[3..].iter_mut().zip(clock) {
            *slot = field(text, 2)?;
        }
        Some((fields, offset))
    }
}

/// The synthetic AST literal that fills an interior gap left by a named
/// argument binding a later parameter. `None` for a parameter with no declared
/// default: such a gap stays `E0204`, because the emitted call needs a value
/// and inventing one would silently change the result.
fn gap_default_expr(param: &ParamSpec, span: Span) -> Option<ast::Expr> {
    if let Some(value) = param.default_string {
        return Some(ast::Expr::Str {
            value: value.to_string(),
            span,
        });
    }
    if let Some(value) = param.default_bool {
        return Some(ast::Expr::Bool { value, span });
    }
    if let Some(series) = param.default_series {
        return Some(ast::Expr::Ident(ast::Ident {
            name: series.name().to_string(),
            span,
        }));
    }
    if let Some(value) = param.default_int {
        return Some(ast::Expr::Int { value, span });
    }
    param
        .default_float
        .map(|value| ast::Expr::Float { value, span })
}

/// The caller's scope stack that [`Analyzer::enter_definition_scope`] set
/// aside for a library definition (`None` for a local one).
#[must_use = "pass it to `leave_definition_scope` to restore the caller's scopes"]
struct CallerScopes(Option<Vec<HashMap<String, SymbolId>>>);

/// `expand_def_scoped`'s view of the analyzer for the generic
/// [`binder::expand_protocol`]: the expansion counters, the call-stack
/// queries, and the diagnostics (each guard's diagnostic also analyzes the
/// call's arguments, as before).
struct ExpandCtx<'a, 'r, 'd> {
    an: &'a mut Analyzer<'r>,
    name: &'d str,
    /// This definition's call-stack frame (`name@span`).
    frame: &'d str,
    def: &'d ast::FuncDef,
    param_names: &'d [&'d str],
    args: &'d [ast::CallArg],
    span: Span,
}

impl ExpandCtx<'_, '_, '_> {
    /// Analyze the arguments the caller wrote, for their diagnostics only.
    /// A default the binder chose belongs to the definition and resolves in
    /// its scope; the call is not expanded, so the default is left out rather
    /// than analyzed against the caller's scopes and namespace.
    fn analyze_dense(&mut self, dense: &[binder::Choice]) {
        for choice in dense {
            if let binder::Choice::Arg(j) = *choice {
                let _ = self.an.analyze_expr(&self.args[j].value);
            }
        }
    }
}

impl binder::ExpandHost for ExpandCtx<'_, '_, '_> {
    fn aborted(&mut self) -> &mut bool {
        &mut self.an.expansion_aborted
    }

    fn nodes(&mut self) -> &mut usize {
        &mut self.an.expansion_nodes
    }

    fn is_recursive(&mut self) -> bool {
        self.an.call_stack.iter().any(|n| n == self.frame)
    }

    fn depth(&mut self) -> usize {
        self.an.call_stack.len()
    }

    fn report_bind(&mut self, err: binder::BindErr) {
        self.an
            .report_bind_error(self.name, self.param_names, self.args, self.span, err);
    }

    fn report_recursive(&mut self, dense: &[binder::Choice]) {
        let name = self.name;
        self.an.error(
            codes::E0224,
            format!("function `{name}` is recursive"),
            self.span,
            "recursion (direct or mutual) is not supported",
        );
        self.analyze_dense(dense);
    }

    fn report_depth(&mut self, dense: &[binder::Choice]) {
        self.an.error(
            codes::E0224,
            format!("function call nesting exceeded the {MAX_INLINE_DEPTH}-level limit"),
            self.span,
            "the call graph nests too deeply to inline",
        );
        self.analyze_dense(dense);
    }

    fn report_arity(&mut self, err: binder::ArityErr, dense: &[binder::Choice]) {
        match err {
            binder::ArityErr::Count { required, total } => {
                let expected = if required == total {
                    format!("{required}")
                } else {
                    format!("{required} to {total}")
                };
                self.an.error(
                    codes::E0224,
                    format!(
                        "`{}` expects {expected} argument(s), found {}",
                        self.def.name.name,
                        dense.len()
                    ),
                    self.span,
                    format!("expected {expected} argument(s)"),
                );
            }
            // The count is in range, but a parameter without a default
            // follows one with a default and was left out:
            // `f(a = 1.0, b)` called as `f(5.0)`.
            binder::ArityErr::OmittedWithoutDefault { param } => {
                self.an.error(
                    codes::E0224,
                    format!(
                        "`{}` is missing required argument `{}`",
                        self.def.name.name, self.def.params[param].name.name
                    ),
                    self.span,
                    "required argument is missing",
                );
            }
        }
        self.analyze_dense(dense);
    }

    fn report_budget(&mut self) {
        self.an.error(
            codes::E0224,
            format!("function expansion exceeded the {MAX_EXPANSION_NODES}-node limit"),
            self.span,
            "the call graph inlines to too many statements",
        );
    }
}

/// Resolve call argument names against the selected definition's parameter
/// slice for the extracted binder protocol. Borrowed names stay local: only
/// the owned argument-shape vector crosses this helper boundary.
pub(crate) fn binder_param_shapes(
    params: &[ast::FuncParam],
    args: &[ast::CallArg],
) -> Vec<binder::ArgShape> {
    let mut shapes = Vec::with_capacity(args.len());
    let mut k = 0;
    while k < args.len() {
        let shape = if <ast::CallArg as binder::ArgName<&str>>::is_named(&args[k]) {
            let mut found = None;
            let mut i = 0;
            while i < params.len() {
                let param_name = params[i].name.name.as_str();
                if <ast::CallArg as binder::ArgName<&str>>::names(&args[k], &param_name) {
                    found = Some(i);
                    break;
                }
                i += 1;
            }
            binder::ArgShape::Named(found)
        } else {
            binder::ArgShape::Positional
        };
        shapes.push(shape);
        k += 1;
    }
    shapes
}

/// Return whether each selected parameter has a default, in source order.
pub(crate) fn binder_param_has_defaults(params: &[ast::FuncParam]) -> Vec<bool> {
    let mut has_defaults = Vec::with_capacity(params.len());
    let mut i = 0;
    while i < params.len() {
        has_defaults.push(params[i].default.is_some());
        i += 1;
    }
    has_defaults
}

#[cfg(test)]
mod binder_parameter_projection_tests {
    use super::{binder_param_has_defaults, binder_param_shapes};
    use pinewasm_syntax::ast::{self, Item};

    fn assert_shapes_match_binder(params: &[ast::FuncParam], args: &[ast::CallArg]) {
        let names: Vec<&str> = params
            .iter()
            .map(|param| param.name.name.as_str())
            .collect();
        assert_eq!(
            binder_param_shapes(params, args),
            crate::binder::arg_shapes(&names, args)
        );
    }

    fn function(source: &str, name: &str) -> ast::FuncDef {
        let parsed = pinewasm_parse::parse(source);
        assert!(
            parsed.diagnostics.is_empty(),
            "projection fixture must parse: {:?}",
            parsed.diagnostics
        );
        parsed
            .program
            .items
            .into_iter()
            .find_map(|item| match item {
                Item::FuncDef(def) if def.name.name == name => Some(def),
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing function {name}"))
    }

    fn call_args(source: &str) -> Vec<ast::CallArg> {
        let parsed = pinewasm_parse::parse(source);
        assert!(
            parsed.diagnostics.is_empty(),
            "argument fixture must parse: {:?}",
            parsed.diagnostics
        );
        parsed
            .program
            .items
            .into_iter()
            .find_map(|item| match item {
                Item::Stmt(ast::Stmt::Expr {
                    expr: ast::Expr::Call { args, .. },
                    ..
                }) => Some(args),
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing call expression in {source:?}"))
    }

    #[test]
    fn empty_and_required_parameter_slices_project_empty_and_none_controls() {
        let empty = function(
            "//@version=6\nempty() => 1\nrequired(first, second) => first\n",
            "empty",
        );
        let no_args = call_args("//@version=6\nempty()\n");
        assert!(binder_param_shapes(&empty.params, &no_args).is_empty());
        assert_shapes_match_binder(&empty.params, &no_args);
        let named_against_empty = call_args("//@version=6\nempty(unknown = 1)\n");
        assert_eq!(
            binder_param_shapes(&empty.params, &named_against_empty),
            [crate::binder::ArgShape::Named(None)]
        );
        assert_shapes_match_binder(&empty.params, &named_against_empty);
        assert!(binder_param_has_defaults(&empty.params).is_empty());

        let required = function(
            "//@version=6\nempty() => 1\nrequired(first, second) => first\n",
            "required",
        );
        assert_shapes_match_binder(&required.params, &no_args);
        assert_eq!(binder_param_has_defaults(&required.params), [false, false]);
    }

    #[test]
    fn mixed_parameters_map_positional_named_and_unknown_arguments_in_order() {
        let mixed = function(
            "//@version=6\nmixed(alpha, beta = 1.0, gamma, delta = \"x\") => alpha\n",
            "mixed",
        );
        assert_eq!(
            binder_param_has_defaults(&mixed.params),
            [false, true, false, true]
        );
        assert_eq!(
            binder_param_has_defaults(&mixed.params).len(),
            mixed.params.len()
        );
        let args = call_args("//@version=6\nmixed(10, gamma = 30, missing = 40)\n");
        assert_shapes_match_binder(&mixed.params, &args);
        assert_eq!(
            binder_param_shapes(&mixed.params, &args),
            [
                crate::binder::ArgShape::Positional,
                crate::binder::ArgShape::Named(Some(2)),
                crate::binder::ArgShape::Named(None),
            ]
        );

        let reordered = function(
            "//@version=6\nmixed(gamma, beta = 2.0, alpha, delta = \"x\") => alpha\n",
            "mixed",
        );
        assert_eq!(
            binder_param_has_defaults(&reordered.params),
            [false, true, false, true]
        );
        assert_eq!(
            binder_param_shapes(&reordered.params, &args),
            [
                crate::binder::ArgShape::Positional,
                crate::binder::ArgShape::Named(Some(0)),
                crate::binder::ArgShape::Named(None),
            ]
        );
        assert_shapes_match_binder(&reordered.params, &args);
    }

    #[test]
    fn repeated_argument_names_and_duplicate_parameters_keep_first_match_order() {
        let params = function("//@version=6\nf(first, second) => first\n", "f");
        let repeated = call_args("//@version=6\nf(second = 1, second = 2)\n");
        assert_eq!(
            binder_param_shapes(&params.params, &repeated),
            [
                crate::binder::ArgShape::Named(Some(1)),
                crate::binder::ArgShape::Named(Some(1)),
            ]
        );
        assert_shapes_match_binder(&params.params, &repeated);

        let duplicate_params = function("//@version=6\nf(first, first) => first\n", "f");
        let first = call_args("//@version=6\nf(first = 1)\n");
        assert_eq!(
            binder_param_shapes(&duplicate_params.params, &first),
            [crate::binder::ArgShape::Named(Some(0))]
        );
        assert_shapes_match_binder(&duplicate_params.params, &first);
    }

    #[test]
    fn parameter_name_matching_is_case_sensitive_and_unicode_exact() {
        let mut params = function("//@version=6\nf(name) => name\n", "f").params;
        let mut args = call_args("//@version=6\nf(name = 1)\n");
        params[0].name.name = "CaseName".to_owned();
        assert_shapes_match_binder(&params, &args);
        args[0].name.as_mut().unwrap().name = "casename".to_owned();
        assert_eq!(
            binder_param_shapes(&params, &args),
            [crate::binder::ArgShape::Named(None)]
        );

        params[0].name.name = "café".to_owned();
        args[0].name.as_mut().unwrap().name = "café".to_owned();
        assert_eq!(
            binder_param_shapes(&params, &args),
            [crate::binder::ArgShape::Named(Some(0))]
        );
        args[0].name.as_mut().unwrap().name = "cafe\u{301}".to_owned();
        assert_eq!(
            binder_param_shapes(&params, &args),
            [crate::binder::ArgShape::Named(None)]
        );
        assert_shapes_match_binder(&params, &args);
    }

    #[test]
    fn distinct_default_expressions_both_project_as_present() {
        let numeric = function("//@version=6\nnumeric(value = 17) => value\n", "numeric");
        let string = function(
            "//@version=6\nstring(value = \"payload\") => value\n",
            "string",
        );
        assert_eq!(binder_param_has_defaults(&numeric.params), [true]);
        assert_eq!(binder_param_has_defaults(&string.params), [true]);
    }
}
