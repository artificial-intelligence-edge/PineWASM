//! Call arguments run in bound parameter order, including the hoisted body of
//! an inlined user function.
//!
//! The evaluator runs each call argument once, left to right in bound
//! parameter order (named arguments too; see
//! `table_cell_reordered_named_arguments_evaluate_in_parameter_order` in
//! `pinewasm_vm`). The analyzer analyzes arguments in that order, but it does
//! not emit them in one place: an inlined user function's body statements,
//! and the statements desugared from a method receiver or an `if`/`switch`
//! expression, go to `pending_stmts` and run before the whole enclosing
//! statement, while a builtin argument stays inline in the statement. So in
//! `plot(array.pop(a), color = f())` the body of `f` ran before the earlier
//! `array.pop`.
//!
//! The fix is local to each call (and each eager binary operator). When an
//! argument has been analyzed and a sibling that is analyzed after it may
//! hoist statements, the parts of the argument whose value a hoisted
//! statement could change, or that change state themselves, are stored into
//! hidden slots by statements appended to `pending_stmts` at once, ahead of
//! the later sibling's statements. The argument then reads the slots.
//! Literals, variable, builtin and input reads, and operators and pure
//! builtins over them stay inline (a hoisted statement cannot reassign a
//! variable the caller reads: a function cannot assign a global, and the
//! desugared statements write only their own hidden slots), so static
//! rendering metadata is unchanged.
//!
//! Captures run only where the enclosing statement's pending statements run
//! once, immediately before the expression itself. They are suppressed in a
//! `var`/`varip` initializer (first bar only), a `while` condition and `for`
//! bounds (tested every iteration), a `switch` arm condition (tested only
//! when the earlier arms fail), and the arguments of calls that evaluate
//! an argument in another context or read it as metadata (`request.*`,
//! `input.*` and similar). Statements nested in a block (a function
//! body, a branch) drain their own pending statements just before themselves,
//! so captures are enabled again there.
use crate::*;

/// The direct arguments of one call (or the operands of one eager binary
/// operator) while it is being analyzed.
pub(crate) struct ArgOrderFrame {
    /// [`Analyzer::arg_order_depth`] of a direct argument's analysis.
    child_depth: u32,
    siblings: Vec<Sibling>,
}

struct Sibling {
    span: Span,
    /// The argument may append statements to `pending_stmts` when analyzed.
    may_hoist: bool,
    /// The argument has been analyzed (at least once).
    done: bool,
}

impl<'r> Analyzer<'r> {
    /// Enter one expression's analysis: push the frame for its direct
    /// arguments and suppress captures inside calls that must keep their
    /// arguments inline. Returns what [`Self::arg_order_leave`] restores.
    pub(crate) fn arg_order_enter(&mut self, expr: &ast::Expr) -> (u32, bool, bool) {
        self.arg_order_depth += 1;
        let depth = self.arg_order_depth;
        let saved_suppressed = self.arg_order_suppressed;
        if self.call_keeps_arguments_inline(expr) {
            self.arg_order_suppressed = true;
        }
        let frame = if self.arg_order_suppressed {
            None
        } else {
            self.arg_order_frame_for(expr, depth + 1)
        };
        let pushed = frame.is_some();
        if let Some(frame) = frame {
            self.arg_order_frames.push(frame);
        }
        (depth, pushed, saved_suppressed)
    }

    /// Leave one expression's analysis (see [`Self::arg_order_enter`]). When
    /// the expression is a direct argument of a call whose later argument may
    /// hoist statements, its order-sensitive parts are captured now.
    pub(crate) fn arg_order_leave(
        &mut self,
        span: Span,
        (depth, pushed, saved_suppressed): (u32, bool, bool),
        value: HirExpr,
    ) -> HirExpr {
        if pushed {
            self.arg_order_frames.pop();
        }
        self.arg_order_suppressed = saved_suppressed;
        self.arg_order_depth -= 1;
        if self.arg_order_suppressed {
            return value;
        }
        let Some(frame) = self.arg_order_frames.last_mut() else {
            return value;
        };
        if frame.child_depth != depth {
            return value;
        }
        let Some(index) = frame.siblings.iter().position(|s| s.span == span) else {
            return value;
        };
        frame.siblings[index].done = true;
        let later_may_hoist = frame
            .siblings
            .iter()
            .enumerate()
            .any(|(i, sibling)| i != index && !sibling.done && sibling.may_hoist);
        if !later_may_hoist || value.ty.is_error() {
            return value;
        }
        self.capture_order_sensitive(value)
    }

    /// Run `analyze` for a statement-level call (`plot(...)`, `label.new` as
    /// a statement, ...) whose arguments the statement analyzer analyzes
    /// directly rather than through an expression.
    pub(crate) fn with_ordered_call_args<T>(
        &mut self,
        args: &[ast::CallArg],
        analyze: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let frame = if self.arg_order_suppressed {
            None
        } else {
            self.frame_over(args.iter().map(|arg| &arg.value), self.arg_order_depth + 1)
        };
        let pushed = frame.is_some();
        if let Some(frame) = frame {
            self.arg_order_frames.push(frame);
        }
        let result = analyze(self);
        if pushed {
            self.arg_order_frames.pop();
        }
        result
    }

    /// Run `analyze` for a call that a statement analyzer dispatches itself
    /// instead of through [`Analyzer::analyze_expr`] (the right-hand side of
    /// a destructuring declaration): install the same argument-order frame,
    /// or the same inline suppression, that [`Self::arg_order_enter`] would
    /// for `call`, so its arguments run in bound parameter order.
    pub(crate) fn with_call_arg_order<T>(
        &mut self,
        call: &ast::Expr,
        analyze: impl FnOnce(&mut Self) -> T,
    ) -> T {
        if self.call_keeps_arguments_inline(call) {
            return self.without_arg_order_captures(analyze);
        }
        let frame = if self.arg_order_suppressed {
            None
        } else {
            self.arg_order_frame_for(call, self.arg_order_depth + 1)
        };
        let pushed = frame.is_some();
        if let Some(frame) = frame {
            self.arg_order_frames.push(frame);
        }
        let result = analyze(self);
        if pushed {
            self.arg_order_frames.pop();
        }
        result
    }

    /// Run `analyze` with captures suppressed (an expression that is
    /// evaluated more or less often than its statement's pending statements).
    pub(crate) fn without_arg_order_captures<T>(
        &mut self,
        analyze: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let saved = std::mem::replace(&mut self.arg_order_suppressed, true);
        let result = analyze(self);
        self.arg_order_suppressed = saved;
        result
    }

    /// Run `analyze` for a nested statement, whose pending statements run
    /// immediately before it: captures are enabled again.
    pub(crate) fn with_arg_order_captures<T>(&mut self, analyze: impl FnOnce(&mut Self) -> T) -> T {
        let saved = std::mem::replace(&mut self.arg_order_suppressed, false);
        let result = analyze(self);
        self.arg_order_suppressed = saved;
        result
    }

    fn frame_over<'e>(
        &self,
        args: impl Iterator<Item = &'e ast::Expr>,
        child_depth: u32,
    ) -> Option<ArgOrderFrame> {
        let siblings: Vec<Sibling> = args
            .map(|arg| Sibling {
                span: arg.span(),
                may_hoist: self.ast_may_hoist(arg),
                done: false,
            })
            .collect();
        // Nothing to order unless some argument may hoist and another
        // argument can be analyzed before it.
        (siblings.len() > 1 && siblings.iter().any(|s| s.may_hoist)).then_some(ArgOrderFrame {
            child_depth,
            siblings,
        })
    }

    fn arg_order_frame_for(&self, expr: &ast::Expr, child_depth: u32) -> Option<ArgOrderFrame> {
        match expr {
            ast::Expr::Call {
                callee,
                args,
                type_args,
                ..
            } if self.call_orders_its_arguments(callee, type_args) => {
                self.frame_over(args.iter().map(|arg| &arg.value), child_depth)
            }
            // The receiver is bound to a hidden variable before the arguments.
            ast::Expr::MethodCall { args, .. } => {
                self.frame_over(args.iter().map(|arg| &arg.value), child_depth)
            }
            // `and`/`or` keep their right operand lazy (its statements are
            // guarded separately).
            ast::Expr::Binary { op, lhs, rhs, .. }
                if !matches!(op, ast::BinaryOp::And | ast::BinaryOp::Or) =>
            {
                self.frame_over([&**lhs, &**rhs].into_iter(), child_depth)
            }
            _ => None,
        }
    }

    /// Calls whose arguments are evaluated in another context or read as
    /// metadata. Nothing inside them is captured.
    fn call_keeps_arguments_inline(&self, expr: &ast::Expr) -> bool {
        let ast::Expr::Call { callee, .. } = expr else {
            return false;
        };
        let name = callee.name.as_str();
        if name.starts_with("request.") || name.starts_with("input.") {
            return true;
        }
        matches!(
            self.registry.lookup_function(name).map(|f| f.kind),
            Some(
                BuiltinFunctionKind::Input(_)
                    | BuiltinFunctionKind::InputGeneric
                    | BuiltinFunctionKind::Security
                    | BuiltinFunctionKind::Request(_)
                    | BuiltinFunctionKind::SecurityLowerTf
                    | BuiltinFunctionKind::PivotPointLevels
                    | BuiltinFunctionKind::Indicator
                    | BuiltinFunctionKind::MaxBarsBack
            )
        )
    }

    /// Builtin calls that evaluate their arguments inline in the statement
    /// (`ta.*` too: `ta.sma(array.pop(a), f())` pops before `f` runs).
    /// A user function or method stores each argument into its parameter
    /// temp as it is analyzed, so its arguments are already in order.
    fn call_orders_its_arguments(&self, callee: &ast::Ident, type_args: &[ast::Ident]) -> bool {
        if !type_args.is_empty() {
            return true;
        }
        let name = callee.name.as_str();
        if let Some(func) = self.registry.lookup_function(name) {
            return matches!(
                func.kind,
                BuiltinFunctionKind::Plot
                    | BuiltinFunctionKind::Display(_)
                    | BuiltinFunctionKind::Pure(_)
                    | BuiltinFunctionKind::Ta(_)
                    | BuiltinFunctionKind::Collection(_)
                    | BuiltinFunctionKind::MapConstructor { .. }
                    | BuiltinFunctionKind::MatrixConstructor { .. }
                    | BuiltinFunctionKind::Log(_)
                    | BuiltinFunctionKind::Alert
                    | BuiltinFunctionKind::AlertCondition
                    | BuiltinFunctionKind::TimeFunction { .. }
                    | BuiltinFunctionKind::RuntimeErrorFn
                    | BuiltinFunctionKind::Order(_)
            );
        }
        if self.split_drawing_callee(name).is_some()
            || DrawKind::from_array_constructor_name(name).is_some()
        {
            return true;
        }
        // The footprint namespace is intercepted before the registry, so it
        // has no registry kind. `footprint.get_row_by_price(id, price)` is its
        // one call with more than one argument and evaluates both inline; the
        // `footprint.*` and `volume_row.*` field accessors take one argument.
        if name == "footprint.get_row_by_price" {
            return true;
        }
        let Some((prefix, method)) = name.rsplit_once('.') else {
            return false;
        };
        if self.object_type_name(prefix).is_some() {
            return matches!(method, "new" | "copy");
        }
        // `receiver.method(...)` on a variable: a builtin collection or
        // drawing method, unless a user method of that name applies.
        !prefix.contains('.')
            && self.lookup_variable(prefix).is_some()
            && !self.calls_user_method(prefix, method)
    }

    /// `prefix.method(...)` names a user-defined method on a variable.
    fn calls_user_method(&self, prefix: &str, method: &str) -> bool {
        let Some(symbol) = self.lookup_variable(prefix) else {
            return false;
        };
        let ty = &self.symbols[symbol.0 as usize].ty;
        self.find_method(&(Self::value_type_key(ty), method.to_string()))
            .is_some()
    }

    /// Whether analyzing `expr` may append statements to `pending_stmts`: a
    /// user function or method call (its body), a chained method call (its
    /// receiver), an `if`/`switch`/loop expression, or a `plot()`/`hline()`
    /// handle. Conservative: a `true` only costs a hidden slot.
    pub(crate) fn ast_may_hoist(&self, expr: &ast::Expr) -> bool {
        match expr {
            ast::Expr::Int { .. }
            | ast::Expr::Float { .. }
            | ast::Expr::Bool { .. }
            | ast::Expr::Na { .. }
            | ast::Expr::Str { .. }
            | ast::Expr::Color { .. }
            | ast::Expr::Ident(_) => false,
            ast::Expr::Unary { operand, .. } => self.ast_may_hoist(operand),
            ast::Expr::Binary { lhs, rhs, .. } => {
                self.ast_may_hoist(lhs) || self.ast_may_hoist(rhs)
            }
            ast::Expr::Ternary {
                cond,
                then_e,
                else_e,
                ..
            } => {
                self.ast_may_hoist(cond) || self.ast_may_hoist(then_e) || self.ast_may_hoist(else_e)
            }
            ast::Expr::Call {
                callee,
                args,
                type_args,
                ..
            } => {
                self.call_may_hoist(callee, type_args)
                    || args.iter().any(|arg| self.ast_may_hoist(&arg.value))
            }
            ast::Expr::FieldAccess { receiver, .. } => self.ast_may_hoist(receiver),
            ast::Expr::History { target, offset, .. } => {
                self.ast_may_hoist(target) || self.ast_may_hoist(offset)
            }
            ast::Expr::Group { inner, .. } => self.ast_may_hoist(inner),
            ast::Expr::Tuple { elements, .. } => elements.iter().any(|e| self.ast_may_hoist(e)),
            ast::Expr::MethodCall { .. }
            | ast::Expr::If(_)
            | ast::Expr::Switch(_)
            | ast::Expr::Loop(_) => true,
        }
    }

    fn call_may_hoist(&self, callee: &ast::Ident, type_args: &[ast::Ident]) -> bool {
        if !type_args.is_empty() {
            return false;
        }
        let name = callee.name.as_str();
        if let Some(func) = self.registry.lookup_function(name) {
            // A `plot()`/`hline()` used as a value emits its plot statement.
            return matches!(
                func.kind,
                BuiltinFunctionKind::Plot | BuiltinFunctionKind::Display(DisplayKind::Hline)
            );
        }
        if self.split_drawing_callee(name).is_some()
            || DrawKind::from_array_constructor_name(name).is_some()
            || is_cast_name(name)
            || name.starts_with("request.")
            || name.starts_with("footprint.")
            || name.starts_with("volume_row.")
        {
            return false;
        }
        let Some((prefix, method)) = name.rsplit_once('.') else {
            // A user function, or a method called as a function.
            return true;
        };
        if self.object_type_name(prefix).is_some() {
            return false;
        }
        if self.resolve_alias(prefix).is_some() {
            return true;
        }
        // A field chain binds its receiver; a user method inlines its body.
        prefix.contains('.') || self.calls_user_method(prefix, method)
    }

    /// Store the parts of `value` that a later sibling's hoisted statements
    /// could reorder into hidden slots, now, and return `value` reading them.
    /// Operators and pure builtins are walked so their order-insensitive
    /// parts stay inline; lazily evaluated parts (an `and`/`or` right
    /// operand, ternary arms) are never pulled out of their guard: such a
    /// node is captured whole.
    fn capture_order_sensitive(&mut self, mut value: HirExpr) -> HirExpr {
        if order_insensitive(&value) {
            return value;
        }
        match &mut value.kind {
            HirExprKind::Unary { operand, .. } | HirExprKind::IntToFloat(operand) => {
                self.capture_boxed(operand);
            }
            HirExprKind::Binary { op, lhs, rhs } if !matches!(op, BinOp::And | BinOp::Or) => {
                self.capture_boxed(lhs);
                self.capture_boxed(rhs);
            }
            HirExprKind::Binary { lhs, rhs, .. } if order_insensitive(rhs) => {
                self.capture_boxed(lhs);
            }
            HirExprKind::Ternary {
                cond,
                then_e,
                else_e,
            } if order_insensitive(then_e) && order_insensitive(else_e) => {
                self.capture_boxed(cond);
            }
            HirExprKind::CallPure { func, args } if *func != PureFunc::MathRandom => {
                for arg in args.iter_mut() {
                    let span = arg.span;
                    let inner = std::mem::replace(arg, placeholder(span));
                    *arg = self.capture_order_sensitive(inner);
                }
            }
            _ => return self.capture_into_slot(value),
        }
        value
    }

    fn capture_boxed(&mut self, slot: &mut Box<HirExpr>) {
        let span = slot.span;
        let inner = std::mem::replace(&mut **slot, placeholder(span));
        **slot = self.capture_order_sensitive(inner);
    }

    fn capture_into_slot(&mut self, value: HirExpr) -> HirExpr {
        if value.ty.is_error()
            || matches!(value.ty, Type::Void | Type::Plot | Type::Hline)
            || matches!(value.kind, HirExprKind::Tuple { .. })
        {
            return value;
        }
        let span = value.span;
        let ty = value.ty.clone();
        let qualifier = value.qualifier;
        let slot = self.allocate_synthetic_slot(ty.clone(), span);
        self.symbols[slot.0 as usize].qualifier = qualifier;
        self.pending_stmts.push(HirStmt::Assign {
            symbol: SymbolId(slot.0),
            value,
            init: false,
            span,
        });
        HirExpr {
            kind: HirExprKind::ReadVar {
                symbol: SymbolId(slot.0),
                offset: 0,
            },
            ty,
            qualifier,
            span,
        }
    }
}

fn placeholder(span: Span) -> HirExpr {
    HirExpr {
        kind: HirExprKind::ConstNa,
        ty: Type::Error,
        qualifier: Qualifier::Const,
        span,
    }
}

/// True when a statement hoisted ahead of `expr` cannot change its value and
/// evaluating it changes no state that statement can see: literals;
/// variable, builtin and input reads; and operators, pure builtins (other
/// than `math.random`) and `ta.*` calls over them (a `ta.*` call's state
/// belongs to its call site alone). Everything else (collection calls, field
/// reads, a `ta.*` call over one of those, requests, drawings, history
/// materialization) is order-sensitive.
fn order_insensitive(expr: &HirExpr) -> bool {
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
        | HirExprKind::ReadVarDynamic { offset, .. } => order_insensitive(offset),
        HirExprKind::Unary { operand, .. } | HirExprKind::IntToFloat(operand) => {
            order_insensitive(operand)
        }
        HirExprKind::Binary { lhs, rhs, .. } => order_insensitive(lhs) && order_insensitive(rhs),
        HirExprKind::Ternary {
            cond,
            then_e,
            else_e,
        } => order_insensitive(cond) && order_insensitive(then_e) && order_insensitive(else_e),
        HirExprKind::CallPure { func, args } => {
            *func != PureFunc::MathRandom && args.iter().all(order_insensitive)
        }
        // A `ta.*` call keeps its state per call site, which no other
        // statement reads or writes, so over order-insensitive arguments it
        // gives the same value before or after a hoisted user function
        // (`plot(ta.sma(close, 14), color = f())` needs no slot).
        HirExprKind::TaCall { source, .. } => order_insensitive(source),
        HirExprKind::TaCallDynamic { source, length, .. } => {
            order_insensitive(source) && order_insensitive(length)
        }
        HirExprKind::CallTaState { args, .. } => args.iter().all(order_insensitive),
        _ => false,
    }
}
