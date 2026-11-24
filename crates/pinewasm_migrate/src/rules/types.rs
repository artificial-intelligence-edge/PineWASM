//! Phase D: types and `na`.
//!
//! v6 removed three implicit behaviors: numbers no longer convert to
//! `bool`, `bool` has no `na` state, and dividing two `const int` values
//! keeps the fraction. Each rewrite here reproduces the v5 value
//! explicitly.

use super::calls::const_number;
use super::{text, Cx, Helper, Site};
use crate::builtins::{self, Ty};
use crate::model::{self, for_each_child, key, peel, DeclKind};
use crate::{RuleClass, Severity};
use pinewasm_syntax::{self as ast, BinaryOp, Expr};

impl Cx<'_, '_> {
    /// The v6 form makes a v5 value block's final assignment
    /// explicit in v6. Only value-producing contexts call this; a side-effect
    /// `if` elsewhere in the script must not acquire an extra read.
    pub(super) fn value_block_tail(&mut self, body: &[ast::Stmt]) {
        let Some(last) = body.last() else { return };
        match last {
            ast::Stmt::Assign {
                name, value, span, ..
            }
            | ast::Stmt::Reassign { name, value, span } => {
                // A multiline conditional RHS owns its own indented value
                // block. Its statement span can end before the final arm, so
                // a read inserted here would land inside that expression.
                if matches!(value, Expr::If(_) | Expr::Switch(_)) {
                    return;
                }
                let start = span.start as usize;
                let end = text::line_end(self.src, span.end as usize);
                let line_start = text::line_start(self.src, start);
                let indent = text::indent_at(self.src, start);
                // A same-line `=> x = ...` needs structural reformatting;
                // inserting an unindented line would change the program.
                // So does a body wrapped onto the next line: an indentation
                // that is not a multiple of four columns continues the
                // `=>` line instead of opening a block, and a second line at
                // that indentation would continue it again, yielding
                // `=> x = value x`.
                let columns: usize = indent.chars().map(|c| if c == '\t' { 4 } else { 1 }).sum();
                if indent.is_empty()
                    || columns % 4 != 0
                    || !self.src[line_start..start].trim().is_empty()
                    || name.name.contains('.')
                    || !self.allowed(start, end)
                    || !self.free(start, end)
                {
                    return;
                }
                if self
                    .edits
                    .insert_at(end, format!("\n{indent}{}", name.name))
                {
                    self.note(
                        "D9",
                        RuleClass::Exact,
                        Severity::Info,
                        (start, end),
                        format!(
                            "Added an explicit `{}` value after the final assignment in a value block.",
                            name.name
                        ),
                        true,
                    );
                }
            }
            ast::Stmt::If(if_stmt) => {
                if if_stmt.else_branch.is_none() {
                    if let Some(u) = self.unique_tail(std::iter::once(&if_stmt.then_branch)) {
                        self.close_unique_conditional(u, last, if_stmt.span);
                    }
                }
                self.value_block_tail(&if_stmt.then_branch);
                if let Some(else_branch) = &if_stmt.else_branch {
                    self.value_block_tail(else_branch);
                }
            }
            ast::Stmt::Switch(switch) => {
                if !switch.arms.iter().any(|a| a.label.is_none()) {
                    if let Some(u) = self.unique_tail(switch.arms.iter().map(|a| &a.body)) {
                        self.close_unique_conditional(u, last, switch.span);
                    }
                }
                for arm in &switch.arms {
                    self.value_block_tail(&arm.body);
                }
            }
            _ => {}
        }
    }

    /// The special constant type a value block's branches produce, when
    /// every branch that ends in an expression agrees on one. A statement
    /// `if`/`switch` ending a function body is that function's value, the
    /// same as the expression form handled in `unique_branches`.
    fn unique_tail<'b>(
        &self,
        bodies: impl Iterator<Item = &'b Vec<ast::Stmt>>,
    ) -> Option<builtins::Unique> {
        let mut found = None;
        for body in bodies {
            let Some(ast::Stmt::Expr { expr, .. }) = body.last() else {
                return None;
            };
            match self.model.ty(expr) {
                Ty::Unique(u) if found.is_none_or(|f| f == u) => found = Some(u),
                Ty::Na => {}
                _ => return None,
            }
        }
        found
    }

    /// A declaration assignment at the end of a statement `if` branch is
    /// also a value-producing branch in the v6 form. Reassignments
    /// to an existing series remain side effects and do not get a tail read.
    pub(super) fn statement_if_tail(&mut self, stmt: &ast::Stmt) {
        let ast::Stmt::If(if_stmt) = stmt else {
            return;
        };
        for body in std::iter::once(&if_stmt.then_branch).chain(if_stmt.else_branch.iter()) {
            match body.last() {
                Some(ast::Stmt::Assign { .. }) => self.value_block_tail(body),
                Some(nested @ ast::Stmt::If(_)) => self.statement_if_tail(nested),
                _ => {}
            }
        }
    }

    pub(super) fn types_stmt(&mut self, site: Site<'_>) {
        let mut roots = Vec::new();
        model::for_each_stmt_expr(site.stmt, &mut |e| roots.push(e));
        for root in roots {
            self.types_expr(root, site, None);
        }
    }

    pub(super) fn function_params(&mut self, f: &ast::FuncDef) {
        for p in &f.params {
            if let Some(default) = &p.default {
                self.na_literal(default);
            }
        }
    }

    pub(super) fn types_udt(&mut self, t: &ast::TypeDecl) {
        for field in &t.fields {
            if let Some(default) = &field.default {
                self.na_literal(default);
            }
        }
    }

    fn types_expr(&mut self, expr: &Expr, site: Site<'_>, parent: Option<&Expr>) {
        self.na_literal(expr);
        self.bool_cast(expr, parent);
        self.na_functions(expr);
        self.na_comparison(expr, site);
        self.const_division(expr, parent);
        self.unique_branches(expr);
        self.first_bar_constant(expr);
        if self.opts.strict_indices {
            self.checked_index(expr);
        }
        for_each_child(expr, &mut |c| self.types_expr(c, site, Some(expr)));
    }

    // -- D2 / D4b: `na` literals with a bool or special-constant type --------------

    fn na_literal(&mut self, expr: &Expr) {
        let Expr::Na { span } = expr else { return };
        let Some(adopted) = self.model.na_adopted.get(&key(expr)) else {
            return;
        };
        let (s, e) = (span.start as usize, span.end as usize);
        if !self.allowed(s, e) || !self.free(s, e) {
            return;
        }
        if adopted.contains(&Ty::Bool) {
            if self.edits.replace(s, e, "false") {
                self.note("D2", RuleClass::Exact, Severity::Info, (s, e), "Replaced a `bool` `na` with `false`: v6 booleans have no `na` state, and v5 treated a `na` condition as false.", true);
            }
            return;
        }
        let unique = adopted.iter().find_map(|t| match t {
            Ty::Unique(u) => Some(*u),
            _ => None,
        });
        if let Some(u) = unique {
            match (u.default_constant(), u.assumed_default()) {
                (Some(default), _) => {
                    if self.edits.replace(s, e, default) {
                        self.note("D4b", RuleClass::Exact, Severity::Info, (s, e), format!("Replaced `na` with `{default}`, the value v5 used for it; v6 does not allow `na` for this constant type."), true);
                    }
                }
                // The same assumption D4c makes for a missing branch: an
                // explicit `na` branch selects the solid style, which is the
                // assumed default for `hline`.
                (None, Some(default)) => {
                    if self.edits.replace(s, e, default) {
                        self.note("D4b", RuleClass::Approximate, Severity::Warning, (s, e), format!("Replaced `na` with `{default}`: v6 does not allow `na` for this constant type, and the solid style is assumed as its default."), true);
                    }
                }
                (None, None) => self.note("D-13", RuleClass::Diagnostic, Severity::Error, (s, e), "This `na` stands for a special constant whose default v6 cannot spell (a strategy direction or a script's scale); choose an explicit constant.", false),
            }
        }
    }

    // -- D1 ------------------------------------------------------------------------

    fn bool_cast(&mut self, expr: &Expr, parent: Option<&Expr>) {
        if !self.model.bool_context.contains(&key(expr)) || !self.model.may_be_numeric(expr) {
            return;
        }
        if parent.is_some_and(|p| is_call_to(p, "bool")) {
            return;
        }
        // Wrap inside grouping parentheses: `if(x)` must not become
        // `ifbool((x))`.
        let span = peel(expr).span();
        let (s, e) = (span.start as usize, span.end as usize);
        if !self.allowed(s, e) {
            return;
        }
        if self.edits.wrap(s, e, "bool(", ")") {
            self.note("D1", RuleClass::Exact, Severity::Info, (s, e), "Wrapped a number used as a condition in `bool()`: v5 converted it implicitly (0 and `na` are false), v6 requires the explicit cast.", true);
        }
    }

    // -- D2g-j: `nz`, `fixnan`, and `na` applied to booleans -------------------------

    fn na_functions(&mut self, expr: &Expr) {
        let Expr::Call {
            callee, args, span, ..
        } = expr
        else {
            return;
        };
        let (s, e) = (span.start as usize, span.end as usize);
        let Some(first) = args.first() else { return };
        if self.model.ty(&first.value) != Ty::Bool || !self.allowed(s, e) || !self.free(s, e) {
            return;
        }
        let arg_text = self.expr_text(&first.value).to_string();
        match callee.name.as_str() {
            "nz" | "fixnan" => {
                let replacement_false = args
                    .get(1)
                    .is_none_or(|r| matches!(peel(&r.value), Expr::Bool { value: false, .. }));
                let atomic = matches!(
                    peel(&first.value),
                    Expr::Ident(_) | Expr::History { .. } | Expr::Call { .. } | Expr::Group { .. }
                );
                let text = if atomic {
                    arg_text
                } else {
                    format!("({arg_text})")
                };
                if self.edits.replace(s, e, text) {
                    if replacement_false {
                        self.note("D2g", RuleClass::Exact, Severity::Info, (s, e), format!("Removed `{}()` around a `bool`: a v6 bool is never `na`, and v5 replaced `na` with `false` here.", callee.name), true);
                    } else {
                        self.note("D-1", RuleClass::Approximate, Severity::Warning, (s, e), format!("Removed `{}()` around a `bool`. Where the value was `na` in v5, this returned the replacement; a v6 bool is never `na`, so the value is `false` instead.", callee.name), true);
                    }
                }
            }
            "na" => {
                let inner = peel(&first.value);
                if let Expr::Binary { op, lhs, rhs, .. } = inner {
                    if matches!(
                        op,
                        BinaryOp::Lt
                            | BinaryOp::Le
                            | BinaryOp::Gt
                            | BinaryOp::Ge
                            | BinaryOp::Eq
                            | BinaryOp::Ne
                    ) && self.model.ty(lhs) != Ty::Bool
                        && self.model.ty(rhs) != Ty::Bool
                    {
                        let text = format!(
                            "(na({}) or na({}))",
                            self.expr_text(lhs),
                            self.expr_text(rhs)
                        );
                        if self.edits.replace(s, e, text) {
                            self.note("D2h", RuleClass::Exact, Severity::Info, (s, e), "A comparison is `na` in v5 exactly when an operand is `na`; v6 comparisons are never `na`, so the operands are tested directly.", true);
                        }
                        return;
                    }
                }
                if let Expr::History { target, offset, .. } = inner {
                    if let (Expr::Ident(ident), Some(n)) = (peel(target), const_number(offset)) {
                        if self.never_na_global_bool(ident) {
                            if self
                                .edits
                                .replace(s, e, format!("(bar_index < {})", n as i64))
                            {
                                self.note("D2i", RuleClass::Exact, Severity::Info, (s, e), "This boolean is never `na` in v5 except before its history exists, so the history test becomes a bar-count test.", true);
                            }
                            return;
                        }
                    }
                }
                if self.edits.replace(s, e, "false") {
                    self.note("D-1", RuleClass::Approximate, Severity::Warning, (s, e), "`na()` of a boolean is always false in v6 (booleans are never `na`). Where v5 produced a `na` boolean here, the script's behavior may differ.", true);
                }
            }
            _ => {}
        }
    }

    fn never_na_global_bool(&self, ident: &ast::Ident) -> bool {
        let Some(decl) = self.model.decl_of(ident) else {
            return false;
        };
        if decl.kind != DeclKind::Global || decl.reassigned {
            return false;
        }
        self.model
            .init_of(ident)
            .is_some_and(|init| match peel(init) {
                Expr::Bool { .. } => true,
                Expr::Ident(i) => {
                    i.name.starts_with("barstate.") || i.name.starts_with("session.is")
                }
                Expr::Call { callee, .. } => matches!(
                    callee.name.as_str(),
                    "ta.crossover" | "ta.crossunder" | "ta.cross"
                ),
                _ => false,
            })
    }

    // -- D3 ------------------------------------------------------------------------

    fn na_comparison(&mut self, expr: &Expr, site: Site<'_>) {
        let Expr::Binary { op, lhs, rhs, span } = expr else {
            return;
        };
        if !matches!(
            op,
            BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge | BinaryOp::Eq | BinaryOp::Ne
        ) {
            return;
        }
        let other = match (peel(lhs), peel(rhs)) {
            (Expr::Na { .. }, Expr::Na { .. }) => return,
            (Expr::Na { .. }, _) => rhs,
            (_, Expr::Na { .. }) => lhs,
            _ => return,
        };
        let (s, e) = (span.start as usize, span.end as usize);
        if !self.allowed(s, e) || !self.free(s, e) {
            return;
        }
        if self.exact() {
            if self.model.must_evaluate(other) {
                // Keep the operand's evaluation; the comparison itself
                // becomes `false` on the next pass.
                let _ = self.hoist(site, &[other]);
                return;
            }
            if self.edits.replace(s, e, "false") {
                self.note("D3", RuleClass::Exact, Severity::Warning, (s, e), "A comparison with the `na` literal is always false, so it is replaced with `false`. The author probably meant `na(x)`; the idiomatic profile writes that instead, which changes the script's behavior.", true);
            }
        } else {
            let text = match op {
                BinaryOp::Eq => format!("na({})", self.expr_text(other)),
                BinaryOp::Ne => format!("not na({})", self.expr_text(other)),
                _ => "false".to_string(),
            };
            if self.edits.replace(s, e, text) {
                self.note("D3", RuleClass::Approximate, Severity::Warning, (s, e), "Rewrote a comparison with `na` as an `na()` test. A comparison with `na` is always false, so this changes behavior toward what the code evidently intended.", true);
            }
        }
    }

    // -- D5 ------------------------------------------------------------------------

    fn const_division(&mut self, expr: &Expr, parent: Option<&Expr>) {
        let Expr::Binary {
            op: BinaryOp::Div,
            lhs,
            rhs,
            span,
        } = expr
        else {
            return;
        };
        if self.model.ty(lhs) != Ty::Int || self.model.ty(rhs) != Ty::Int {
            return;
        }
        let (s, e) = (span.start as usize, span.end as usize);
        if parent.is_some_and(|p| is_call_to(p, "int")) || !self.allowed(s, e) {
            return;
        }
        if self.model.always_const_int(lhs) && self.model.always_const_int(rhs) {
            // `(a / b)` becomes `int(a / b)`, reusing the source's own
            // parentheses instead of nesting a second pair inside them.
            let group = match parent {
                Some(Expr::Group { span: g, .. })
                    if !self.src[..g.start as usize]
                        .ends_with(|c: char| c == '_' || c.is_alphanumeric()) =>
                {
                    Some((g.start as usize, g.end as usize))
                }
                _ => None,
            };
            let wrapped = match group {
                Some((gs, ge)) => self.allowed(gs, ge) && self.edits.wrap(gs, ge, "int", ""),
                None => self.edits.wrap(s, e, "int(", ")"),
            };
            if wrapped {
                self.note("D5", RuleClass::Exact, Severity::Info, (s, e), "Wrapped a division of two `const int` values in `int()`: v5 discarded the fraction (rounding toward zero), v6 keeps it.", true);
            }
        } else if self.model.sometimes_const_int(lhs) && self.model.sometimes_const_int(rhs) {
            self.note("D5", RuleClass::Diagnostic, Severity::Warning, (s, e), "This integer division is `const int / const int` for some calls of the function and not others; v5 truncated only the constant calls. Check which result each call needs.", false);
        }
    }

    // -- D4c -----------------------------------------------------------------------

    /// An `if`/`switch` expression of a special constant type that can fall
    /// through produced `na` in v5; v6 needs an explicit default branch.
    fn unique_branches(&mut self, expr: &Expr) {
        let Ty::Unique(u) = self.model.ty(expr) else {
            return;
        };
        let span = expr.span();
        let (s, e) = (span.start as usize, span.end as usize);
        if !self.allowed(s, e) {
            return;
        }
        let open = match expr {
            Expr::If(i) => i.else_branch.is_none(),
            Expr::Switch(sw) => !sw.arms.iter().any(|a| a.label.is_none()),
            _ => false,
        };
        let conditional = match expr {
            Expr::If(i) => Conditional::If(&i.then_branch, i.else_branch.is_none()),
            Expr::Switch(sw) => Conditional::Switch(&sw.arms),
            _ => return,
        };
        self.close_conditional(u, open, conditional, (s, e));
    }

    /// The statement form of `unique_branches`: a function body or value
    /// block that ends in an `if` without `else` or a `switch` without a
    /// default arm.
    fn close_unique_conditional(
        &mut self,
        u: builtins::Unique,
        stmt: &ast::Stmt,
        span: pinewasm_span::Span,
    ) {
        let (s, e) = (span.start as usize, span.end as usize);
        if !self.allowed(s, e) {
            return;
        }
        let conditional = match stmt {
            ast::Stmt::If(i) => Conditional::If(&i.then_branch, i.else_branch.is_none()),
            ast::Stmt::Switch(sw) => Conditional::Switch(&sw.arms),
            _ => return,
        };
        self.close_conditional(u, true, conditional, (s, e));
    }

    fn close_conditional(
        &mut self,
        u: builtins::Unique,
        open: bool,
        conditional: Conditional<'_>,
        (s, e): (usize, usize),
    ) {
        let (default, approximate) = match (u.default_constant(), u.assumed_default()) {
            (Some(d), _) => (d, false),
            (None, Some(d)) => (d, true),
            (None, None) => {
                if open {
                    self.note("D-13", RuleClass::Diagnostic, Severity::Error, (s, e), "This conditional can fall through to `na`, which v6 does not allow for this constant type and which has no documented default; add an explicit final branch.", false);
                }
                return;
            }
        };
        let (class, severity, why) = if approximate {
            (RuleClass::Approximate, Severity::Warning, "the solid style is assumed as the value used when no branch ran, which matters only when no branch matches")
        } else {
            (
                RuleClass::Exact,
                Severity::Info,
                "the value v5 used when no branch ran",
            )
        };
        let indent = self.indent_of(s);
        match conditional {
            Conditional::If(then_branch, true) => {
                let Some(last) = then_branch.last() else {
                    return;
                };
                let end = last.span().end as usize;
                if self.edits.insert_at(
                    end,
                    format!("\n{indent}else\n{indent}{}{default}", self.unit),
                ) {
                    self.note(
                        "D4c",
                        class,
                        severity,
                        (s, e),
                        format!("Added `else` returning `{default}`: {why}."),
                        true,
                    );
                }
            }
            Conditional::Switch(arms) if !arms.iter().any(|a| a.label.is_none()) => {
                let (Some(first), Some(last)) = (arms.first(), arms.last()) else {
                    return;
                };
                let arm_indent = self.indent_of(first.span.start as usize);
                let end = last.span.end as usize;
                if self
                    .edits
                    .insert_at(end, format!("\n{arm_indent}=> {default}"))
                {
                    self.note(
                        "D4c",
                        class,
                        severity,
                        (s, e),
                        format!("Added a default arm returning `{default}`: {why}."),
                        true,
                    );
                }
            }
            _ => {}
        }
    }

    // -- D-11 (strict indices) -------------------------------------------------------

    fn checked_index(&mut self, expr: &Expr) {
        let Some((func, args, receiver)) = super::calls::builtin_call(self.model, expr) else {
            return;
        };
        if !matches!(
            func.as_str(),
            "array.get" | "array.set" | "array.insert" | "array.remove"
        ) {
            return;
        }
        let Some(index) = super::calls::array_index_arg(args, receiver) else {
            return;
        };
        if !may_be_negative_index(self.model, &index.value) {
            return;
        }
        let span = index.value.span();
        let (s, e) = (span.start as usize, span.end as usize);
        if !self.allowed(s, e) {
            return;
        }
        if self.edits.wrap(s, e, "__v6_checkedIndex(", ")") {
            self.helpers.insert(Helper::CheckedIndex);
            self.note("D-11", RuleClass::Exact, Severity::Info, (s, e), "Guarded an index that can be negative: the guard stops with a runtime error, as v5 did, instead of reading from the end of the array.", true);
        }
    }
}

fn is_call_to(expr: &Expr, name: &str) -> bool {
    matches!(expr, Expr::Call { callee, args, .. } if callee.name == name && args.len() == 1)
}

/// Whether a known-negative literal or search result reaches this expression
/// directly or through immutable aliases. Resolve declarations by source
/// position rather than spelling so a shadowed local does not inherit another
/// variable's origin.
pub(super) fn may_be_negative_index(model: &crate::model::Model<'_>, expr: &Expr) -> bool {
    let mut current = peel(expr);
    let mut visited = std::collections::HashSet::new();
    loop {
        if const_number(current).is_some_and(|value| value < 0.0) {
            return true;
        }
        if super::calls::builtin_call(model, current).is_some_and(|(func, ..)| {
            matches!(
                func.as_str(),
                "array.indexof" | "array.lastindexof" | "array.binary_search" | "str.pos"
            )
        }) {
            return true;
        }
        let Expr::Ident(ident) = current else {
            return false;
        };
        let Some(decl) = model.decl_of(ident) else {
            return false;
        };
        if decl.reassigned || !visited.insert(ident.span.start) {
            return false;
        }
        let Some(init) = model.init_of(ident) else {
            return false;
        };
        current = peel(init);
    }
}

// -- D6: a reassigned variable where a constant is required -------------------

impl Cx<'_, '_> {
    /// A persistent or locally reassigned v5 variable can reach a parameter
    /// that requires `simple` and use its first-bar value. A non-persistent
    /// global variable reassigned only at global scope retains its weak
    /// qualifier in v6, so it needs neither a rewrite nor a review finding.
    /// For the remaining cases, pass a statically known first-bar value or
    /// report the site.
    fn first_bar_constant(&mut self, expr: &Expr) {
        let Expr::Call { callee, args, .. } = expr else {
            return;
        };
        if self.model.funcs.contains_key(&callee.name) {
            return;
        }
        let bound = self.bind(&callee.name, args);
        for b in &bound {
            let Some(param) = b.param else { continue };
            if !builtins::requires_non_series(&callee.name, param) {
                continue;
            }
            let Expr::Ident(ident) = peel(&b.arg.value) else {
                continue;
            };
            let Some(decl) = self.model.decl_of(ident) else {
                continue;
            };
            if !decl.reassigned || decl.v5_qual() == builtins::Qual::Series {
                continue;
            }
            if decl.kind == DeclKind::Global && !decl.is_var && !decl.nested_reassign {
                continue;
            }
            let span = b.arg.value.span();
            let (s, e) = (span.start as usize, span.end as usize);
            if !self.allowed(s, e) || !self.free(s, e) {
                continue;
            }
            let Some(value) = self.fold_first_bar(ident, span.start) else {
                self.note("D-3", RuleClass::Diagnostic, Severity::Error, (s, e), format!("`{}` is reassigned, so it is a `series` in v6, which `{}({param})` does not accept. The migrator needs its first-bar value, which cannot be computed here; pass a constant or input instead.", ident.name, callee.name), false);
                continue;
            };
            if self.edits.replace(s, e, value.literal()) {
                self.note("D6", RuleClass::Exact, Severity::Warning, (s, e), format!("Passed `{}` here, the first-bar value of `{}`: v6 rejects a reassigned (`series`) variable for `{}({param})`, so the migrated call uses the value v5 used on the first bar.", value.literal(), ident.name, callee.name), true);
            }
        }
    }

    /// The value a global variable holds, on the first bar, just before the
    /// statement at `use_at`.
    fn fold_first_bar(&self, ident: &ast::Ident, use_at: u32) -> Option<Folded> {
        let decl_id = *self.model.refs.get(&ident.span.start)?;
        let decl = self.model.decls.get(&decl_id)?;
        if decl.kind != DeclKind::Global || decl.nested_reassign {
            return None;
        }
        let mut value = None;
        for item in &self.model.program.items {
            let ast::Item::Stmt(stmt) = item else {
                continue;
            };
            if stmt.span().start >= use_at {
                break;
            }
            match stmt {
                ast::Stmt::Assign {
                    name, value: init, ..
                } if name.span.start == decl_id => {
                    value = Some(self.fold(init, None)?);
                }
                ast::Stmt::Reassign {
                    name, value: next, ..
                } if self.model.refs.get(&name.span.start) == Some(&decl_id) => {
                    value = Some(self.fold(next, Some((decl_id, value?)))?);
                }
                _ => {}
            }
        }
        value
    }

    /// Constant-fold an expression under v5 arithmetic (`int / int` of
    /// constants truncates). `current` supplies the folded variable's value
    /// for self-references (`n := n + 1`).
    fn fold(&self, expr: &Expr, current: Option<(u32, Folded)>) -> Option<Folded> {
        use pinewasm_syntax::UnaryOp;
        match peel(expr) {
            Expr::Int { value, .. } => Some(Folded::Int(*value)),
            Expr::Float { value, .. } => Some(Folded::Float(*value)),
            Expr::Unary {
                op: UnaryOp::Neg,
                operand,
                ..
            } => match self.fold(operand, current)? {
                Folded::Int(v) => Some(Folded::Int(-v)),
                Folded::Float(v) => Some(Folded::Float(-v)),
            },
            Expr::Ident(i) => {
                let id = *self.model.refs.get(&i.span.start)?;
                match current {
                    Some((cid, v)) if cid == id => Some(v),
                    _ => {
                        let decl = self.model.decls.get(&id)?;
                        if decl.reassigned {
                            return None;
                        }
                        self.fold(self.model.init_of(i)?, None)
                    }
                }
            }
            Expr::Binary { op, lhs, rhs, .. } => {
                let (a, b) = (self.fold(lhs, current)?, self.fold(rhs, current)?);
                match (a, b) {
                    (Folded::Int(x), Folded::Int(y)) => Some(Folded::Int(match op {
                        BinaryOp::Add => x.checked_add(y)?,
                        BinaryOp::Sub => x.checked_sub(y)?,
                        BinaryOp::Mul => x.checked_mul(y)?,
                        BinaryOp::Div => x.checked_div(y)?,
                        BinaryOp::Rem => x.checked_rem(y)?,
                        _ => return None,
                    })),
                    (x, y) => {
                        let (x, y) = (x.as_f64(), y.as_f64());
                        Some(Folded::Float(match op {
                            BinaryOp::Add => x + y,
                            BinaryOp::Sub => x - y,
                            BinaryOp::Mul => x * y,
                            BinaryOp::Div if y != 0.0 => x / y,
                            _ => return None,
                        }))
                    }
                }
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Folded {
    Int(i64),
    Float(f64),
}

impl Folded {
    fn as_f64(self) -> f64 {
        match self {
            Folded::Int(v) => v as f64,
            Folded::Float(v) => v,
        }
    }

    fn literal(self) -> String {
        match self {
            Folded::Int(v) => v.to_string(),
            Folded::Float(v) if v.fract() == 0.0 && v.abs() < 1e15 => format!("{v:.1}"),
            Folded::Float(v) => v.to_string(),
        }
    }
}

/// The branches of a conditional that may need a default for a special
/// constant type, in either its expression or its statement form.
enum Conditional<'a> {
    /// The `then` block, and whether the `else` is missing.
    If(&'a [ast::Stmt], bool),
    Switch(&'a [ast::SwitchArm]),
}
