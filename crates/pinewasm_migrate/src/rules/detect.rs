//! Review findings on the original script: source behavior that no
//! local rewrite can keep, reported instead of silently changed.

use super::calls::{array_index_arg, bind_with, builtin_call, const_number};
use super::types::may_be_negative_index;
use super::Note;
use crate::builtins::{self, Qual, Ty};
use crate::model::{self, for_each_child, peel, Model};
use crate::{MigrationOptions, RuleClass, Severity};
use pinewasm_syntax::{self as ast, BinaryOp, Expr, Stmt, UnaryOp};

struct Detector<'a, 'p> {
    src: &'a str,
    model: &'a Model<'p>,
    opts: &'a MigrationOptions,
    notes: &'a mut Vec<Note>,
}

pub(super) fn run(src: &str, model: &Model<'_>, opts: &MigrationOptions, notes: &mut Vec<Note>) {
    let mut d = Detector {
        src,
        model,
        opts,
        notes,
    };
    for item in &model.program.items {
        match item {
            ast::Item::Stmt(s) => d.stmt(s),
            ast::Item::FuncDef(f) => {
                for s in &f.body {
                    d.stmt(s);
                }
            }
            _ => {}
        }
    }
}

impl Detector<'_, '_> {
    fn note(
        &mut self,
        rule: &'static str,
        severity: Severity,
        span: pinewasm_span::Span,
        message: impl Into<String>,
    ) {
        self.notes.push(Note {
            rule,
            class: RuleClass::Diagnostic,
            severity,
            message: message.into(),
            start: span.start as usize,
            end: span.end as usize,
            applied: false,
        });
    }

    fn stmt(&mut self, stmt: &Stmt) {
        model::for_each_stmt_expr(stmt, &mut |e| self.expr(e));
        for block in model::child_blocks(stmt) {
            for s in block {
                self.stmt(s);
            }
        }
    }

    fn expr(&mut self, expr: &Expr) {
        self.boolean_tuple_na(expr);
        match expr {
            Expr::Call { span, .. } | Expr::MethodCall { span, .. } => {
                if let Some((func, args, receiver)) = builtin_call(self.model, expr) {
                    self.call(&func, args, *span, receiver);
                }
            }
            Expr::Binary {
                op: op @ (BinaryOp::Eq | BinaryOp::Ne),
                lhs,
                rhs,
                span,
            } => self.bool_literal_comparison(*op, lhs, rhs, *span),
            Expr::If(i) => {
                for s in i.then_branch.iter().chain(i.else_branch.iter().flatten()) {
                    self.stmt(s);
                }
            }
            Expr::Switch(sw) => {
                for arm in &sw.arms {
                    for s in &arm.body {
                        self.stmt(s);
                    }
                }
            }
            Expr::Loop(stmt) => {
                let body = match stmt.as_ref() {
                    Stmt::For(f) => &f.body,
                    Stmt::ForIn(f) => &f.body,
                    Stmt::While(w) => &w.body,
                    _ => &[][..],
                };
                for s in body {
                    self.stmt(s);
                }
            }
            _ => {}
        }
        for_each_child(expr, &mut |c| self.expr(c));
    }

    /// Unlike scalar bool `na`, tuple missingness cannot be silently replaced:
    /// The third state stays visible through the destructured value.
    fn boolean_tuple_na(&mut self, expr: &Expr) {
        let bodies: Vec<&[Stmt]> = match peel(expr) {
            Expr::If(i) => std::iter::once(i.then_branch.as_slice())
                .chain(i.else_branch.iter().map(Vec::as_slice))
                .collect(),
            Expr::Switch(s) => s.arms.iter().map(|a| a.body.as_slice()).collect(),
            _ => return,
        };
        let tuples: Vec<_> = bodies
            .iter()
            .filter_map(|body| match body.last() {
                Some(Stmt::Expr { expr, .. }) => match peel(expr) {
                    Expr::Tuple { elements, .. } => Some(elements),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        let Some(first) = tuples.first() else { return };
        // Mismatched arity is a separate source error, not this migration case.
        if tuples.len() != bodies.len() || tuples.iter().any(|t| t.len() != first.len()) {
            return;
        }
        for index in 0..first.len() {
            let has_na = tuples
                .iter()
                .any(|t| matches!(peel(&t[index]), Expr::Na { .. }));
            let has_bool = tuples.iter().any(|t| self.model.ty(&t[index]) == Ty::Bool);
            if has_na && has_bool {
                self.note(
                    "D-1",
                    Severity::Error,
                    expr.span(),
                    "A tuple component is `na` in one branch and `bool` in another. This third Boolean state needs manual conversion, because the tuple branches must have matching types. Choose an explicit representation for missingness before converting.",
                );
                return;
            }
        }
    }

    fn call(
        &mut self,
        func: &str,
        args: &[ast::CallArg],
        span: pinewasm_span::Span,
        receiver: usize,
    ) {
        let bound = bind_with(self.model, func, args, receiver);
        for b in &bound {
            let Some(param) = b.param else { continue };
            let value = &b.arg.value;
            // A series offset.
            if param == "offset" && builtins::has_simple_offset(func) && self.v5_series(value) {
                self.note("D-2", Severity::Error, value.span(), format!("`{func}()` receives a `series` offset, but the offset must be a `simple int`, and no rewrite can know its value. Use a fixed offset."));
            }
        }
        // An index that can be negative.
        if !self.opts.strict_indices
            && matches!(
                func,
                "array.get" | "array.set" | "array.insert" | "array.remove"
            )
        {
            if let Some(index) = array_index_arg(args, receiver)
                .filter(|a| may_be_negative_index(self.model, &a.value))
            {
                self.note("D-11", Severity::Warning, index.value.span(), "This index can be negative and is not guarded. The strict-indices option inserts a guard that raises a runtime error instead.");
            }
        }
        // Trailing-stop activation.
        if func == "strategy.exit" {
            if let Some(tp) = bound.iter().find(|b| b.param == Some("trail_points")) {
                if const_number(&tp.arg.value).is_some_and(|v| v < 0.0) {
                    self.note(
                        "D-7",
                        Severity::Warning,
                        tp.arg.value.span(),
                        "A negative `trail_points` value is flagged for review.",
                    );
                }
            }
        }
        // `bars_back` on a requested timeframe.
        if matches!(func, "time" | "time_close")
            && bound.iter().any(|b| b.param == Some("bars_back"))
        {
            let chart_tf = bound.iter().find(|b| b.param == Some("timeframe")).is_none_or(|b| {
                matches!(peel(&b.arg.value), Expr::Str { value, .. } if value.is_empty())
                    || matches!(peel(&b.arg.value), Expr::Ident(i) if i.name == "timeframe.period")
            });
            if !chart_tf {
                self.note("D-8", Severity::Warning, span, format!("`{func}()` with `bars_back` on another timeframe is flagged for review. Check which bar count the script needs."));
            }
        }
        // Colors built from input colors.
        if matches!(func, "color.new" | "color.rgb")
            && args.iter().any(|a| self.is_input_color(&a.value))
        {
            self.note(
                "D-12",
                Severity::Info,
                span,
                "Building a color from an input color: the input's color picker may not control the result.",
            );
        }
        // Printing a boolean that can be `na`.
        if func == "str.tostring" {
            if let Some(first) = args.first() {
                if self.bool_maybe_na(&first.value) {
                    self.note("D-1", Severity::Warning, first.value.span(), "This boolean can be `na` in the original script; v6 booleans are never `na`, so track the unknown state separately.");
                }
            }
        }
    }

    fn bool_literal_comparison(
        &mut self,
        op: BinaryOp,
        lhs: &Expr,
        rhs: &Expr,
        span: pinewasm_span::Span,
    ) {
        let (value, literal) = match (peel(lhs), peel(rhs)) {
            (v, Expr::Bool { value, .. }) | (Expr::Bool { value, .. }, v) => (v, *value),
            _ => return,
        };
        // `== false` and `!= true` are not true for `na`.
        let na_sensitive = (op == BinaryOp::Eq) != literal;
        if na_sensitive && self.bool_maybe_na(value) {
            let text = &self.src[span.start as usize..span.end as usize];
            self.note("D-1", Severity::Warning, span, format!("`{text}`: this boolean can be `na` in the original script, where the comparison is false; v6 booleans are never `na`, so it is true there. If the difference matters, track the unknown state separately."));
        }
    }

    /// A boolean the original script could read as `na`: history of a boolean,
    /// or a boolean requested from another context.
    fn bool_maybe_na(&self, expr: &Expr) -> bool {
        match peel(expr) {
            Expr::History { target, .. } => self.model.ty(target) == Ty::Bool,
            Expr::Call { callee, .. } if callee.name == "request.security" => {
                self.model.ty(expr) == Ty::Bool
            }
            _ => false,
        }
    }

    fn v5_series(&self, value: &Expr) -> bool {
        match peel(value) {
            Expr::Ident(ident) => self.model.decl_of(ident).map_or_else(
                || {
                    self.model
                        .max_qual(value)
                        .is_some_and(|q| q == Qual::Series)
                },
                |d| !d.reassigned && d.init_qual == Qual::Series || d.reassigned && !d.is_var,
            ),
            Expr::Int { .. } | Expr::Float { .. } => false,
            Expr::Unary {
                op: UnaryOp::Neg,
                operand,
                ..
            } => self.v5_series(operand),
            _ => self
                .model
                .max_qual(value)
                .is_some_and(|q| q == Qual::Series),
        }
    }

    fn is_input_color(&self, expr: &Expr) -> bool {
        match peel(expr) {
            Expr::Call { callee, .. } => callee.name == "input.color",
            Expr::Ident(ident) => self
                .model
                .init_of(ident)
                .is_some_and(|init| matches!(peel(init), Expr::Call { callee, .. } if callee.name == "input.color")),
            _ => false,
        }
    }
}
