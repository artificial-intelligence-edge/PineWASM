//! The history-referencing operator.
//!
//! This module rewrites history references whose target is a literal, a
//! built-in constant, or a UDT field (`obj.field[1]`) into the forms the
//! migrator emits.

use super::{Cx, Site};
use crate::builtins::{self, Ty};
use crate::model::{self, for_each_child, peel, DeclKind};
use crate::{RuleClass, Severity};
use pinewasm_syntax::{self as ast, Expr, Stmt};

impl Cx<'_, '_> {
    pub(super) fn history_stmt(&mut self, site: Site<'_>) {
        let mut roots = Vec::new();
        model::for_each_stmt_expr(site.stmt, &mut |e| roots.push(e));
        for root in roots {
            if self.history_in(root, site) {
                return;
            }
        }
    }

    fn history_in(&mut self, expr: &Expr, site: Site<'_>) -> bool {
        if let Expr::History { target, span, .. } = expr {
            let (s, e) = (span.start as usize, span.end as usize);
            if self.allowed(s, e) {
                if is_constant_target(target) {
                    return self.literal_history(site, expr, target);
                }
                if let Expr::Ident(ident) = peel(target) {
                    if self.is_udt_field(ident) {
                        return self.field_history(site, expr, target, ident);
                    }
                }
            }
        }
        let mut done = false;
        for_each_child(expr, &mut |c| {
            if !done {
                done = self.history_in(c, site);
            }
        });
        done
    }

    // -- literal targets -----------------------------------------------------------

    fn literal_history(&mut self, site: Site<'_>, history: &Expr, target: &Expr) -> bool {
        let span = history.span();
        let (s, e) = (span.start as usize, span.end as usize);
        if self.exact() {
            // A variable holding the literal keeps `na` for the first bars,
            // where the history does not exist yet.
            if self.hoist(site, &[target]).is_some() {
                self.note("F1", RuleClass::ExactWithHoisting, Severity::Info, (s, e), "Stored the constant in a variable and took the variable's history, which keeps `na` on the first bars, where the history does not exist yet.", true);
            }
            return true;
        }
        if self.free(s, e) {
            let text = self.expr_text(target).to_string();
            if self.edits.replace(s, e, text) {
                self.note("F1", RuleClass::Approximate, Severity::Warning, (s, e), "Dropped `[]` from a constant, which has no history to read. The migrator models the original form as `na` on the first bars, where the history did not exist yet; this now gives the constant.", true);
            }
        }
        true
    }

    // -- field targets ------------------------------------------------------------

    fn is_udt_field(&self, ident: &ast::Ident) -> bool {
        let Some((head, _)) = ident.name.split_once('.') else {
            return false;
        };
        self.model.decls.values().any(|d| {
            d.name == head
                && d.tys
                    .iter()
                    .any(|t| matches!(t, Ty::Named(n) if self.model.udts.contains_key(n)))
        }) && builtins::variable(&ident.name).is_none()
    }

    fn field_history(
        &mut self,
        site: Site<'_>,
        history: &Expr,
        target: &Expr,
        ident: &ast::Ident,
    ) -> bool {
        let Expr::History { offset, span, .. } = history else {
            return false;
        };
        let (s, e) = (span.start as usize, span.end as usize);
        let (head, fields) = ident.name.split_once('.').unwrap_or((&ident.name, ""));
        if self.fresh_object(ident, head) {
            // `(obj[n]).field`, spelled with a temporary for the object at that
            // offset so the field read is on a plain variable.
            if !self.free(s, e) {
                return true;
            }
            let object = format!("{head}[{}]", self.expr_text(offset));
            if let Some(name) = self.hoist_text(site, &object) {
                if self.edits.replace(s, e, format!("{name}.{fields}")) {
                    self.note("F2", RuleClass::Exact, Severity::Info, (s, e), "Took the history of the object and then read the field; the object is created fresh on every bar and never modified, so this reads the same value.", true);
                }
            }
            return true;
        }
        if self.hoist(site, &[target]).is_some() {
            self.note("F2", RuleClass::Approximate, Severity::Warning, (s, e), "Saved the field in a variable and took that variable's history, because the object is persistent or modified, so `(obj[n]).field` would read today's value.", true);
        }
        true
    }

    /// Whether `head` names a variable that receives a new object on every
    /// execution and is never modified or passed anywhere.
    fn fresh_object(&self, ident: &ast::Ident, head: &str) -> bool {
        let Some(decl) = self.model.decl_of(ident) else {
            return false;
        };
        if decl.is_var || decl.reassigned || decl.kind == DeclKind::Param {
            return false;
        }
        let head_ident = ast::Ident {
            name: head.to_string(),
            span: ident.span,
        };
        let fresh_init = self.model.init_of(&head_ident).is_some_and(|init| {
            matches!(peel(init), Expr::Call { callee, .. } if callee.name.rsplit_once('.').is_some_and(|(_, m)| m == "new"))
        });
        fresh_init && !self.object_escapes(head)
    }

    fn object_escapes(&self, head: &str) -> bool {
        let prefix = format!("{head}.");
        let mut escapes = false;
        for item in &self.model.program.items {
            let stmts: Vec<&Stmt> = match item {
                ast::Item::Stmt(s) => vec![s],
                ast::Item::FuncDef(f) => f.body.iter().collect(),
                _ => Vec::new(),
            };
            for s in stmts {
                stmt_escapes(s, head, &prefix, &mut escapes);
            }
        }
        escapes
    }
}

fn stmt_escapes(stmt: &Stmt, head: &str, prefix: &str, escapes: &mut bool) {
    if *escapes {
        return;
    }
    if let Stmt::Reassign { name, .. } = stmt {
        if name.name.starts_with(prefix) {
            *escapes = true;
            return;
        }
    }
    model::for_each_stmt_expr(stmt, &mut |e| expr_escapes(e, head, prefix, escapes));
    for block in model::child_blocks(stmt) {
        for s in block {
            stmt_escapes(s, head, prefix, escapes);
        }
    }
}

fn expr_escapes(expr: &Expr, head: &str, prefix: &str, escapes: &mut bool) {
    match expr {
        Expr::Call { callee, args, .. } => {
            // A method call on the object, or the object passed as an
            // argument, can modify it.
            if callee.name.starts_with(prefix) && !callee.name[prefix.len()..].contains('.') {
                *escapes = true;
            }
            if args
                .iter()
                .any(|a| matches!(peel(&a.value), Expr::Ident(i) if i.name == head))
            {
                *escapes = true;
            }
        }
        Expr::MethodCall { receiver, .. } => {
            if matches!(peel(receiver), Expr::Ident(i) if i.name == head || i.name.starts_with(prefix))
            {
                *escapes = true;
            }
        }
        _ => {}
    }
    for_each_child(expr, &mut |c| expr_escapes(c, head, prefix, escapes));
}

/// A literal or built-in constant as the target of `[]`.
fn is_constant_target(target: &Expr) -> bool {
    match peel(target) {
        Expr::Int { .. }
        | Expr::Float { .. }
        | Expr::Bool { .. }
        | Expr::Str { .. }
        | Expr::Color { .. } => true,
        Expr::Ident(i) => builtins::constant(&i.name).is_some(),
        _ => false,
    }
}
