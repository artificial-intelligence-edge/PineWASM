//! Evaluation order.
//!
//! v5 evaluated both operands of `and`/`or` and computed a `for` loop's end
//! bound once, on loop entry. v6 short-circuits `and`/`or` and re-evaluates
//! the end bound before every iteration. Neither difference is visible for
//! pure expressions, so these rules only move expressions whose evaluation
//! has history or effects (the model's "must evaluate" test) or whose value
//! the loop body can change. The phase also writes an explicit unit step and
//! prints compound reassignment explicitly while preserving right-hand-side
//! grouping.

use super::calls::const_number;
use super::{Cx, Site};
use crate::builtins::{self, Effect, Qual};
use crate::model::{self, for_each_child, peel};
use crate::{RuleClass, Severity};
use pinewasm_syntax::{self as ast, BinaryOp, Expr, Stmt, UnaryOp};

impl Cx<'_, '_> {
    pub(super) fn order_stmt(&mut self, site: Site<'_>) {
        if self.lazy_operand(site) {
            return;
        }
        if let Stmt::Reassign { name, value, .. } = site.stmt {
            self.expand_compound_assignment(name, value);
        }
        if let Stmt::For(f) = site.stmt {
            self.freeze_end_bound(site, f);
            self.for_bounds(f);
        }
        // A `for` used as a value (`x = for ...`) has the same v5 entry-time
        // bound. Its header belongs to this statement; its body statements
        // are visited as their own sites.
        let mut loops = Vec::new();
        model::for_each_stmt_expr(site.stmt, &mut |e| value_loops(e, &mut loops));
        for f in loops {
            self.freeze_end_bound(site, f);
        }
    }

    /// The parser desugars `x += rhs` in the AST, while text migration keeps
    /// the original token. Spell out that desugaring in the v6 source form,
    /// retaining parentheses when the right side
    /// would otherwise be captured by the outer operator differently.
    fn expand_compound_assignment(&mut self, name: &ast::Ident, value: &Expr) {
        let Expr::Binary { op, lhs, rhs, .. } = value else {
            return;
        };
        let Expr::Ident(target) = lhs.as_ref() else {
            return;
        };
        if target.name != name.name || target.span != name.span {
            return;
        }
        let start = name.span.end as usize;
        let end = rhs.span().start as usize;
        let Some(between) = self.src.get(start..end) else {
            return;
        };
        let token = between.trim();
        let expected = match op {
            BinaryOp::Add => "+=",
            BinaryOp::Sub => "-=",
            BinaryOp::Mul => "*=",
            BinaryOp::Div => "/=",
            BinaryOp::Rem => "%=",
            _ => return,
        };
        if token != expected || !self.allowed(start, end) {
            return;
        }
        let outer_prec = if matches!(op, BinaryOp::Add | BinaryOp::Sub) {
            9
        } else {
            11
        };
        let needs_group = match rhs.as_ref() {
            Expr::Binary { op, .. } => binary_precedence(*op) <= outer_prec,
            Expr::Ternary { .. } | Expr::If(_) => true,
            // A switch expression owns its indented arm block. Wrapping it in
            // parentheses makes those arms parse as separate statements, so
            // keep the block expression directly as the right operand.
            Expr::Switch(_) => false,
            _ => false,
        };
        // Keep the original spacing on the right and leave comments inside
        // the expression untouched. Only the operator gap is rewritten.
        let replacement = if needs_group {
            format!(" := {} {} (", name.name, op.symbol())
        } else {
            format!(" := {} {} ", name.name, op.symbol())
        };
        if self.edits.replace(start, end, replacement) {
            if needs_group {
                self.edits.insert_at(rhs.span().end as usize, ")");
            }
            self.note(
                "E3",
                RuleClass::Exact,
                Severity::Info,
                (start, end),
                "Expanded compound reassignment to the explicit v6 source form.",
                true,
            );
        }
    }

    // -- lazy operands -------------------------------------------------------------

    fn lazy_operand(&mut self, site: Site<'_>) -> bool {
        let mut exprs = Vec::new();
        model::for_each_stmt_expr(site.stmt, &mut |e| exprs.push(e));
        for root in exprs {
            if let Some(rhs) = self.first_lazy_rhs(root) {
                let span = rhs.span();
                let (s, e) = (span.start as usize, span.end as usize);
                if !self.allowed(s, e) {
                    continue;
                }
                if self.hoist(site, &[rhs]).is_some() {
                    self.note("E1", RuleClass::ExactWithHoisting, Severity::Info, (s, e), "Evaluated this operand before the condition: the right side of `and`/`or` is normally skipped when the left side decides the result, but this operand keeps history or has effects, so the migrated script must still evaluate it.", true);
                }
                return true;
            }
        }
        false
    }

    /// The first (in evaluation order) right operand of `and`/`or` that must
    /// be evaluated.
    fn first_lazy_rhs<'e>(&self, expr: &'e Expr) -> Option<&'e Expr> {
        let mut found = None;
        self.find_lazy_rhs(expr, &mut found);
        found
    }

    fn find_lazy_rhs<'e>(&self, expr: &'e Expr, found: &mut Option<&'e Expr>) {
        if found.is_some() {
            return;
        }
        if let Expr::Binary {
            op: BinaryOp::And | BinaryOp::Or,
            lhs,
            rhs,
            ..
        } = expr
        {
            self.find_lazy_rhs(lhs, found);
            if found.is_none() && self.model.must_evaluate(rhs) {
                *found = Some(rhs);
            }
            if found.is_none() {
                self.find_lazy_rhs(rhs, found);
            }
            return;
        }
        for_each_child(expr, &mut |c| self.find_lazy_rhs(c, found));
    }

    // -- counted-loop step ---------------------------------------------------------

    fn for_bounds(&mut self, f: &ast::ForStmt) {
        let end = &f.end;
        let span = end.span();
        let (s, e) = (span.start as usize, span.end as usize);
        // The v6 form writes the implicit unit step explicitly.
        // In counted loops `by` is a positive magnitude: descending
        // bounds still count downward, so this leaves the v5 iteration path
        // intact.
        if f.step.is_none() && self.allowed(s, e) && self.edits.insert_at(e, " by 1") {
            self.note(
                "E2",
                RuleClass::Exact,
                Severity::Info,
                (s, e),
                "Wrote the implicit counted-loop step as `by 1`, the explicit v6 source form.",
                true,
            );
        }
        // A nonpositive evaluated step is a runtime error. Keep the expression
        // unchanged, including its sign and side effects, so migration does not
        // turn a failing script into a passing one. This holds for literal and
        // input steps.
    }

    // -- end bound -----------------------------------------------------------------

    /// v5 evaluated a counted loop's end bound once, on loop entry; v6
    /// re-evaluates it before every iteration. When the body can change the
    /// bound, compute it once into a temporary before the loop so the
    /// migrated loop runs the same iterations as v5. A bound the body cannot
    /// change stays inline.
    fn freeze_end_bound(&mut self, site: Site<'_>, f: &ast::ForStmt) {
        let end = &f.end;
        let span = end.span();
        let (s, e) = (span.start as usize, span.end as usize);
        if self.allowed(s, e) && self.end_may_change(f) && self.hoist(site, &[end]).is_some() {
            self.note("E4", RuleClass::ExactWithHoisting, Severity::Info, (s, e), "Computed the loop's end bound once before the loop: v5 fixed it at loop entry, v6 re-evaluates it before every iteration, and the loop body can change it.", true);
        }
    }

    /// Whether the loop body can change the value of the end bound, so v6's
    /// per-iteration re-evaluation would see a different value.
    fn end_may_change(&self, f: &ast::ForStmt) -> bool {
        let end = &f.end;
        // Arithmetic over immutable scalar snapshots remains fixed for this
        // loop invocation, even when the body calls a user function.
        if self.stable_bound(end) {
            return false;
        }
        if const_number(end).is_some() || self.model.max_qual(end).is_some_and(|q| q == Qual::Const)
        {
            return false;
        }
        if self.model.must_evaluate(end) {
            return true;
        }
        let mut names = Vec::new();
        idents(end, &mut names);
        let mut changes = false;
        for stmt in &f.body {
            body_changes(stmt, &names, &self.model.funcs, &mut changes);
        }
        changes
    }

    fn stable_bound(&self, expr: &Expr) -> bool {
        match peel(expr) {
            Expr::Int { .. } | Expr::Float { .. } => true,
            Expr::Ident(ident) => self.model.decl_of(ident).is_some_and(|d| !d.reassigned),
            Expr::Unary {
                op: UnaryOp::Neg | UnaryOp::Pos,
                operand,
                ..
            } => self.stable_bound(operand),
            Expr::Binary {
                op: BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem,
                lhs,
                rhs,
                ..
            } => self.stable_bound(lhs) && self.stable_bound(rhs),
            _ => false,
        }
    }
}

fn binary_precedence(op: BinaryOp) -> u8 {
    match op {
        BinaryOp::Or => 1,
        BinaryOp::And => 3,
        BinaryOp::Eq | BinaryOp::Ne => 5,
        BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => 7,
        BinaryOp::Add | BinaryOp::Sub => 9,
        BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem => 11,
    }
}

/// Counted loops used as values in `expr`, not descending into loop bodies
/// (those statements are visited as their own sites).
fn value_loops<'e>(expr: &'e Expr, out: &mut Vec<&'e ast::ForStmt>) {
    if let Expr::Loop(stmt) = expr {
        if let Stmt::For(f) = stmt.as_ref() {
            out.push(f);
        }
    }
    for_each_child(expr, &mut |c| value_loops(c, out));
}

fn idents(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Ident(i) => out.push(i.name.split('.').next().unwrap_or(&i.name).to_string()),
        Expr::Call { callee, args, .. } => {
            // A receiver-style call's head is a variable too; a built-in
            // namespace such as `array` in `array.size(a)` is not.
            if let Some((head, _)) = callee.name.split_once('.') {
                if !builtins::is_known_function(&callee.name) {
                    out.push(head.to_string());
                }
            }
            for a in args {
                idents(&a.value, out);
            }
        }
        _ => for_each_child(expr, &mut |c| idents(c, out)),
    }
}

/// Whether a loop-body statement may change a variable the bound reads:
/// a reassignment, a mutating call on it, or any user-function call (which
/// could mutate a shared collection).
fn body_changes(
    stmt: &Stmt,
    names: &[String],
    funcs: &std::collections::HashMap<String, &ast::FuncDef>,
    changes: &mut bool,
) {
    if *changes {
        return;
    }
    if let Stmt::Reassign { name, .. } = stmt {
        let head = name.name.split('.').next().unwrap_or(&name.name);
        if names.iter().any(|n| n == head) {
            *changes = true;
            return;
        }
    }
    model::for_each_stmt_expr(stmt, &mut |e| expr_changes(e, names, funcs, changes));
    for block in model::child_blocks(stmt) {
        for s in block {
            body_changes(s, names, funcs, changes);
        }
    }
}

fn expr_changes(
    expr: &Expr,
    names: &[String],
    funcs: &std::collections::HashMap<String, &ast::FuncDef>,
    changes: &mut bool,
) {
    if *changes {
        return;
    }
    match expr {
        Expr::Call { callee, args, .. } => {
            let name = callee.name.as_str();
            if funcs.contains_key(name) {
                *changes = true;
                return;
            }
            let mutating = builtins::effect(name) == Effect::Mutates
                || name.rsplit_once('.').is_some_and(|(_, m)| {
                    ["array", "map", "matrix"]
                        .iter()
                        .any(|ns| builtins::effect(&format!("{ns}.{m}")) == Effect::Mutates)
                });
            if mutating {
                let receiver = name
                    .split_once('.')
                    .filter(|_| !builtins::is_known_function(name))
                    .map(|(h, _)| h.to_string());
                let first = args.first().and_then(|a| match peel(&a.value) {
                    Expr::Ident(i) => Some(i.name.split('.').next().unwrap_or(&i.name).to_string()),
                    _ => None,
                });
                if receiver
                    .iter()
                    .chain(first.iter())
                    .any(|n| names.contains(n))
                {
                    *changes = true;
                    return;
                }
            }
        }
        Expr::MethodCall { .. } => {
            *changes = true;
            return;
        }
        // Block-valued expressions own statement bodies that the child walk
        // below does not reach.
        Expr::If(i) => {
            for s in i.then_branch.iter().chain(i.else_branch.iter().flatten()) {
                body_changes(s, names, funcs, changes);
            }
        }
        Expr::Switch(sw) => {
            for s in sw.arms.iter().flat_map(|a| a.body.iter()) {
                body_changes(s, names, funcs, changes);
            }
        }
        Expr::Loop(stmt) => {
            for block in model::child_blocks(stmt) {
                for s in block {
                    body_changes(s, names, funcs, changes);
                }
            }
        }
        _ => {}
    }
    for_each_child(expr, &mut |c| expr_changes(c, names, funcs, changes));
}
