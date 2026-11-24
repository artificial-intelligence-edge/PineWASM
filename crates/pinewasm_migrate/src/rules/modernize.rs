//! Optional modernization. Nothing here changes behavior; it only replaces
//! older spellings with the current forms.

use super::{Cx, Site};
use crate::model::{self, for_each_child, peel};
use crate::{RuleClass, Severity};
use pinewasm_syntax::{Expr, Stmt};

impl Cx<'_, '_> {
    pub(super) fn modernize_stmt(&mut self, site: Site<'_>) {
        if let Stmt::Assign {
            annotation: Some(annotation),
            ..
        } = site.stmt
        {
            self.array_suffix(annotation.span);
        }
        let mut roots = Vec::new();
        model::for_each_stmt_expr(site.stmt, &mut |e| roots.push(e));
        for root in roots {
            self.modernize_expr(root);
        }
    }

    /// `float[]` → `array<float>`.
    fn array_suffix(&mut self, span: pinewasm_span::Span) {
        let (s, e) = (span.start as usize, span.end as usize);
        let text = &self.src[s..e];
        let Some(elem) = text.strip_suffix("[]") else {
            return;
        };
        let elem = elem.trim();
        if !self.allowed(s, e) || !self.free(s, e) {
            return;
        }
        if self.edits.replace(s, e, format!("array<{elem}>")) {
            self.note(
                "I2",
                RuleClass::Exact,
                Severity::Info,
                (s, e),
                "Wrote the array type as `array<T>`; the `T[]` form is an older spelling.",
                true,
            );
        }
    }

    fn modernize_expr(&mut self, expr: &Expr) {
        if let Expr::Call {
            callee, args, span, ..
        } = expr
        {
            let (s, e) = (span.start as usize, span.end as usize);
            // `array.new_float(...)` → `array.new<float>(...)`.
            let written = self.text(callee.span).to_string();
            if let Some(elem) = written.strip_prefix("array.new_") {
                let (cs, ce) = (callee.span.start as usize, callee.span.end as usize);
                if self.allowed(cs, ce)
                    && self.free(cs, ce)
                    && self.edits.replace(cs, ce, format!("array.new<{elem}>"))
                {
                    self.note("I3", RuleClass::Exact, Severity::Info, (cs, ce), "Wrote the constructor as `array.new<T>()`; the `array.new_T()` form is an older spelling.", true);
                }
            }
            // `bool(na)` → `false`.
            if callee.name == "bool"
                && args.len() == 1
                && matches!(peel(&args[0].value), Expr::Na { .. })
                && self.allowed(s, e)
                && self.free(s, e)
                && self.edits.replace(s, e, "false")
            {
                self.note(
                    "I4",
                    RuleClass::Exact,
                    Severity::Info,
                    (s, e),
                    "`bool(na)` is rewritten to `false`.",
                    true,
                );
            }
        }
        for_each_child(expr, &mut |c| self.modernize_expr(c));
    }
}
