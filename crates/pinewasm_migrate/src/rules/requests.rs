//! `dynamic_requests`.
//!
//! The migrated script leaves the default for dynamic requests in place; it
//! does not add `dynamic_requests = false` to ordinary global requests.
//! Nested requests can consequently inherit an outer request's context and
//! produce different values. The `Exact` profile pins `false` for a nullary
//! wrapper that establishes a nested request globally. Explicit `false` is
//! removed when dynamic execution is required to compile.

use super::{request_expression_arg, Cx};
use crate::model::{self, for_each_child, peel};
use crate::{RuleClass, Severity};
use pinewasm_syntax::{self as ast, BinaryOp, Expr, Stmt};
use std::collections::{HashMap, HashSet};

#[derive(Default)]
struct RequestFacts {
    /// Any `request.*()` call, directly or through a user function.
    any: bool,
    /// A request executed directly or through a user function from a local
    /// block or conditional expression branch, excluding a nullary wrapper
    /// already established by a direct global call.
    requires_dynamic: bool,
    /// A request whose expression depends on another request's result.
    nested: bool,
    /// Offset of one site proving `requires_dynamic`.
    dynamic_site: Option<(usize, usize)>,
    /// A direct nullary request wrapper evaluated on every bar by a global call.
    /// Later local calls to that wrapper do not need dynamic requests when
    /// the request is established globally.
    globally_called_nullary_wrappers: HashMap<String, usize>,
}

impl Cx<'_, '_> {
    pub(super) fn dynamic_requests(&mut self) {
        let program = self.model.program;
        let Some((kind, args, span)) = program.items.iter().find_map(|i| match i {
            ast::Item::Indicator(d) => Some(("indicator", &d.args, d.span)),
            ast::Item::Strategy(d) => Some(("strategy", &d.args, d.span)),
            ast::Item::Library(d) => Some(("library", &d.args, d.span)),
            _ => None,
        }) else {
            return;
        };
        let (s, e) = (span.start as usize, span.end as usize);
        if !self.allowed(s, e) {
            return;
        }
        let facts = self.request_facts();
        let explicit = args.iter().enumerate().find(|(_, a)| {
            a.name
                .as_ref()
                .is_some_and(|n| n.name == "dynamic_requests")
        });
        match explicit {
            Some((index, arg)) => {
                if matches!(peel(&arg.value), Expr::Bool { value: false, .. })
                    && facts.requires_dynamic
                {
                    if self.delete_args(args, &[index]) {
                        self.note("H1", RuleClass::Exact, Severity::Info, (s, e), "Removed `dynamic_requests = false`: this script executes a request from a local or conditional context, which needs dynamic requests.", true);
                    }
                    if facts.nested {
                        self.nested_warning(facts.dynamic_site.unwrap_or((s, e)));
                    }
                }
            }
            None => {
                if !facts.any {
                    return;
                }
                if facts.requires_dynamic {
                    if facts.nested {
                        self.nested_warning(facts.dynamic_site.unwrap_or((s, e)));
                    }
                    return;
                }
                // The migrated form keeps the default here. Pinning false is
                // applied only when a nullary wrapper has already established the
                // nested request globally and its later local call would
                // otherwise change the chart output.
                if facts.nested && !facts.globally_called_nullary_wrappers.is_empty() {
                    let Some(last) = args.last() else { return };
                    let end = last.span.end as usize;
                    if self.edits.insert_at(end, ", dynamic_requests = false") {
                        self.note("H1", RuleClass::Exact, Severity::Info, (s, e), format!("Added `dynamic_requests = false` to `{kind}()`: the globally established nested wrapper is pinned to false."), true);
                    }
                }
            }
        }
    }

    fn nested_warning(&mut self, site: (usize, usize)) {
        self.note("D-4", RuleClass::Diagnostic, Severity::Error, site, "A request's expression depends on another request, and this script needs dynamic requests (a request executes from a local or conditional context). With dynamic requests the inner request inherits the outer one's context, so its values can differ from the values without dynamic requests. Move the calls to global scope to avoid that difference.", false);
    }

    fn request_facts(&self) -> RequestFacts {
        let mut facts = RequestFacts {
            globally_called_nullary_wrappers: self.globally_called_nullary_wrappers(),
            ..RequestFacts::default()
        };
        let requesting_globals = self.requesting_globals();
        for item in &self.model.program.items {
            match item {
                ast::Item::Stmt(s) => self.stmt_requests(s, 0, &requesting_globals, &mut facts),
                ast::Item::FuncDef(f) => {
                    for s in &f.body {
                        self.stmt_requests(s, 0, &requesting_globals, &mut facts);
                    }
                }
                _ => {}
            }
        }
        facts
    }

    fn globally_called_nullary_wrappers(&self) -> HashMap<String, usize> {
        let mut calls = HashMap::new();
        for item in &self.model.program.items {
            let value = match item {
                ast::Item::Stmt(Stmt::Assign {
                    value,
                    is_var: false,
                    is_varip: false,
                    ..
                }) => peel(value),
                _ => continue,
            };
            let Expr::Call {
                callee, args, span, ..
            } = value
            else {
                continue;
            };
            let name = &callee.name;
            if args.is_empty()
                && self.model.funcs.get(name).is_some_and(|f| {
                    f.params.is_empty()
                        && matches!(
                            f.body.as_slice(),
                            [Stmt::Expr {
                                expr: Expr::Call { callee, .. },
                                ..
                            }] if callee.name.starts_with("request.")
                        )
                })
                && self.model.fn_facts.get(name).is_some_and(|f| f.requests)
            {
                calls
                    .entry(name.clone())
                    .and_modify(|start: &mut usize| *start = (*start).min(span.start as usize))
                    .or_insert(span.start as usize);
            }
        }
        calls
    }

    /// Global variables whose value comes (transitively) from a request.
    fn requesting_globals(&self) -> HashSet<String> {
        let mut set = HashSet::new();
        loop {
            let before = set.len();
            for item in &self.model.program.items {
                if let ast::Item::Stmt(s) = item {
                    let (targets, value): (Vec<&ast::Ident>, &Expr) = match s {
                        Stmt::Assign { name, value, .. } | Stmt::Reassign { name, value, .. } => {
                            (vec![name], value)
                        }
                        Stmt::Destructure { targets, value, .. } => {
                            (targets.iter().collect(), value)
                        }
                        _ => continue,
                    };
                    if self.expr_requests(value, &set) {
                        for t in targets {
                            set.insert(t.name.clone());
                        }
                    }
                }
            }
            if set.len() == before {
                return set;
            }
        }
    }

    fn expr_requests(&self, expr: &Expr, globals: &HashSet<String>) -> bool {
        let mut found = false;
        walk(expr, &mut |e| match e {
            Expr::Call { callee, .. } => {
                if callee.name.starts_with("request.")
                    || self
                        .model
                        .fn_facts
                        .get(&callee.name)
                        .is_some_and(|f| f.requests)
                {
                    found = true;
                }
            }
            Expr::Ident(i) if globals.contains(i.name.split('.').next().unwrap_or(&i.name)) => {
                found = true;
            }
            _ => {}
        });
        found
    }

    fn stmt_requests(
        &self,
        stmt: &Stmt,
        depth: usize,
        globals: &HashSet<String>,
        facts: &mut RequestFacts,
    ) {
        model::for_each_stmt_expr(stmt, &mut |e| {
            self.expr_request_facts(e, depth, globals, facts);
        });
        for block in model::child_blocks(stmt) {
            for s in block {
                self.stmt_requests(s, depth + 1, globals, facts);
            }
        }
    }

    fn expr_request_facts(
        &self,
        expr: &Expr,
        depth: usize,
        globals: &HashSet<String>,
        facts: &mut RequestFacts,
    ) {
        match expr {
            Expr::Call {
                callee, args, span, ..
            } => {
                let name = callee.name.as_str();
                if name.starts_with("request.") {
                    facts.any = true;
                    if depth > 0 {
                        facts.requires_dynamic = true;
                        facts
                            .dynamic_site
                            .get_or_insert((span.start as usize, span.end as usize));
                    }
                    if let Some(arg) = request_expression_arg(args) {
                        if self.expr_requests(arg, globals) {
                            facts.nested = true;
                        }
                    }
                } else if self.model.fn_facts.get(name).is_some_and(|f| f.requests) {
                    facts.any = true;
                    if depth > 0
                        && !(args.is_empty()
                            && facts
                                .globally_called_nullary_wrappers
                                .get(name)
                                .is_some_and(|start| *start < span.start as usize))
                    {
                        facts.requires_dynamic = true;
                        facts
                            .dynamic_site
                            .get_or_insert((span.start as usize, span.end as usize));
                    }
                }
            }
            Expr::If(i) => {
                self.expr_request_facts(&i.cond, depth, globals, facts);
                for s in i.then_branch.iter().chain(i.else_branch.iter().flatten()) {
                    self.stmt_requests(s, depth + 1, globals, facts);
                }
                return;
            }
            Expr::Ternary {
                cond,
                then_e,
                else_e,
                ..
            } => {
                self.expr_request_facts(cond, depth, globals, facts);
                self.expr_request_facts(then_e, depth + 1, globals, facts);
                self.expr_request_facts(else_e, depth + 1, globals, facts);
                return;
            }
            Expr::Binary {
                op: BinaryOp::And | BinaryOp::Or,
                lhs,
                rhs,
                ..
            } => {
                self.expr_request_facts(lhs, depth, globals, facts);
                self.expr_request_facts(rhs, depth + 1, globals, facts);
                return;
            }
            Expr::Switch(sw) => {
                if let Some(subject) = &sw.subject {
                    self.expr_request_facts(subject, depth, globals, facts);
                }
                for arm in &sw.arms {
                    for s in &arm.body {
                        self.stmt_requests(s, depth + 1, globals, facts);
                    }
                }
                return;
            }
            Expr::Loop(stmt) => {
                model::for_each_stmt_expr(stmt, &mut |e| {
                    self.expr_request_facts(e, depth, globals, facts);
                });
                for block in model::child_blocks(stmt) {
                    for s in block {
                        self.stmt_requests(s, depth + 1, globals, facts);
                    }
                }
                return;
            }
            _ => {}
        }
        for_each_child(expr, &mut |c| {
            self.expr_request_facts(c, depth, globals, facts);
        });
    }
}

fn walk(expr: &Expr, f: &mut dyn FnMut(&Expr)) {
    f(expr);
    for_each_child(expr, &mut |c| walk(c, f));
    if let Expr::Loop(stmt) = expr {
        walk_stmt_blocks(stmt, f);
    }
}

fn walk_stmt_blocks(stmt: &Stmt, f: &mut dyn FnMut(&Expr)) {
    for block in model::child_blocks(stmt) {
        for stmt in block {
            model::for_each_stmt_expr(stmt, &mut |expr| walk(expr, f));
            walk_stmt_blocks(stmt, f);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::walk;
    use pinewasm_syntax::{Expr, Stmt};

    #[test]
    fn dependency_walk_visits_expressions_in_loop_result_bodies() {
        let parsed = pinewasm_parse::parse(
            "//@version=5\nvalue = for i = 0 to 2\n    request.security(syminfo.tickerid, timeframe.period, close)\n",
        );
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let pinewasm_syntax::Item::Stmt(Stmt::Assign { value, .. }) = &parsed.program.items[0]
        else {
            panic!("expected a loop-result assignment")
        };
        assert!(matches!(value, Expr::Loop(_)));

        let mut found_request = false;
        walk(value, &mut |expr| {
            if matches!(expr, Expr::Call { callee, .. } if callee.name == "request.security") {
                found_request = true;
            }
        });
        assert!(found_request, "loop-body request should be visited");
    }
}
