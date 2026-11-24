//! Phase G: built-in values that changed.
//!
//! `timeframe.period` now always includes the multiplier (`"1D"`, not
//! `"D"`), and three color constants were given new values.

use super::{Cx, Helper, Site};
use crate::builtins;
use crate::model::{self, for_each_child, peel};
use crate::{RuleClass, Severity};
use pinewasm_syntax::{self as ast, BinaryOp, Expr, Stmt};

/// Where a `timeframe.period` reference is used.
#[derive(Clone, Copy)]
enum Use<'e> {
    /// Compared with `==`/`!=` against this other operand.
    Compared(&'e Expr),
    /// Passed where a timeframe string is expected (same timeframe either
    /// way).
    Timeframe,
    /// Anywhere else: its spelling may be observable.
    Other,
}

impl Cx<'_, '_> {
    pub(super) fn values_stmt(&mut self, site: Site<'_>) {
        if let Stmt::Switch(s) = site.stmt {
            if self.iteration == 0 {
                self.switch_on_period(s);
            }
        }
        let mut roots = Vec::new();
        model::for_each_stmt_expr(site.stmt, &mut |e| roots.push(e));
        for root in roots {
            // A `switch timeframe.period` subject is handled with its labels.
            let is_period_subject = matches!(site.stmt, Stmt::Switch(sw) if sw.subject.as_ref().is_some_and(|s| std::ptr::eq(s, root)))
                && matches!(peel(root), Expr::Ident(i) if i.name == "timeframe.period");
            if !is_period_subject {
                self.values_expr(root, Use::Other);
            }
        }
    }

    fn values_expr(&mut self, expr: &Expr, usage: Use<'_>) {
        match expr {
            Expr::Ident(ident) if ident.name == "timeframe.period" => {
                // Decided once, on the phase's first iteration: after a
                // literal is rewritten to "1D", the comparison looks like a
                // v5 comparison with "1D" and would be rewritten again.
                if self.iteration == 0 {
                    self.timeframe_period(ident, usage);
                }
                return;
            }
            Expr::Ident(ident) if builtins::color_changed_in_v6(&ident.name) => {
                self.recolored(ident);
                return;
            }
            Expr::Binary {
                op: BinaryOp::Eq | BinaryOp::Ne,
                lhs,
                rhs,
                ..
            } => {
                self.values_expr(lhs, Use::Compared(rhs));
                self.values_expr(rhs, Use::Compared(lhs));
                return;
            }
            Expr::Call { callee, args, .. } => {
                let bound = self.bind(&callee.name, args);
                for (i, a) in args.iter().enumerate() {
                    let param =
                        bound
                            .get(i)
                            .and_then(|b| b.param)
                            .or(match (callee.name.as_str(), i) {
                                (
                                    "timeframe.in_seconds" | "timeframe.change" | "input.timeframe",
                                    0,
                                ) => Some("timeframe"),
                                _ => None,
                            });
                    let usage = if param
                        .is_some_and(|p| builtins::is_timeframe_param(&callee.name, p))
                        || matches!(
                            (callee.name.as_str(), i),
                            (
                                "timeframe.in_seconds" | "timeframe.change" | "input.timeframe",
                                0
                            )
                        ) {
                        Use::Timeframe
                    } else {
                        Use::Other
                    };
                    self.values_expr(&a.value, usage);
                }
                return;
            }
            Expr::Group { inner, .. } => {
                self.values_expr(inner, usage);
                return;
            }
            _ => {}
        }
        for_each_child(expr, &mut |c| self.values_expr(c, Use::Other));
    }

    // -- G1 ------------------------------------------------------------------------

    fn timeframe_period(&mut self, ident: &ast::Ident, usage: Use<'_>) {
        let (s, e) = (ident.span.start as usize, ident.span.end as usize);
        if !self.allowed(s, e) {
            return;
        }
        match usage {
            Use::Timeframe => {}
            Use::Compared(other) => {
                if let Expr::Str { value, span } = peel(other) {
                    if let Some(fixed) = with_multiplier(value) {
                        if self.in_range_mode() {
                            self.period_shim(s, e);
                            return;
                        }
                        let (ls, le) = (span.start as usize, span.end as usize);
                        if self.free(ls, le) && self.edits.replace(ls, le, format!("\"{fixed}\"")) {
                            self.note("G1", RuleClass::Exact, Severity::Info, (ls, le), format!("Compared against \"{fixed}\": v6's `timeframe.period` always includes the multiplier, where v5 wrote \"{value}\"."), true);
                        }
                        return;
                    }
                    if !is_multiplied_unit(value) {
                        // Minutes ("60"), explicit seconds ("15S"), and so on
                        // are spelled the same in both versions.
                        return;
                    }
                }
                self.period_other(s, e);
            }
            Use::Other => self.period_other(s, e),
        }
    }

    fn period_other(&mut self, s: usize, e: usize) {
        if self.exact() {
            self.period_shim(s, e);
        } else {
            self.note("D-5", RuleClass::Diagnostic, Severity::Warning, (s, e), "`timeframe.period` now includes the multiplier (\"1D\" where v5 had \"D\"). This use may show or compare that text; the exact profile substitutes a helper that keeps the v5 spelling.", false);
        }
    }

    fn period_shim(&mut self, s: usize, e: usize) {
        if !self.free(s, e) {
            return;
        }
        let call = format!("{}()", Helper::TimeframePeriod.name());
        if self.edits.replace(s, e, call) {
            self.helpers.insert(Helper::TimeframePeriod);
            self.note("G1", RuleClass::Exact, Severity::Info, (s, e), "Replaced `timeframe.period` with a helper that returns v5's spelling (\"D\" rather than v6's \"1D\"), because this use can observe the text.", true);
        }
    }

    /// `switch timeframe.period` with v5 unit labels.
    fn switch_on_period(&mut self, s: &ast::SwitchStmt) {
        let Some(subject) = &s.subject else { return };
        if !matches!(peel(subject), Expr::Ident(i) if i.name == "timeframe.period") {
            return;
        }
        let labels: Vec<&Expr> = s.arms.iter().filter_map(|a| a.label.as_ref()).collect();
        let exact_ok = labels.iter().all(|l| match peel(l) {
            Expr::Str { value, .. } => {
                !is_multiplied_unit(value) || with_multiplier(value).is_some()
            }
            _ => false,
        });
        if !exact_ok || self.in_range_mode() {
            let span = subject.span();
            self.period_other(span.start as usize, span.end as usize);
            return;
        }
        for label in labels {
            if let Expr::Str { value, span } = peel(label) {
                if let Some(fixed) = with_multiplier(value) {
                    let (ls, le) = (span.start as usize, span.end as usize);
                    if self.allowed(ls, le)
                        && self.free(ls, le)
                        && self.edits.replace(ls, le, format!("\"{fixed}\""))
                    {
                        self.note("G1", RuleClass::Exact, Severity::Info, (ls, le), format!("Matched \"{fixed}\": v6's `timeframe.period` always includes the multiplier."), true);
                    }
                }
            }
        }
    }

    // -- G2 ------------------------------------------------------------------------

    fn recolored(&mut self, ident: &ast::Ident) {
        let (s, e) = (ident.span.start as usize, ident.span.end as usize);
        if !self.allowed(s, e) {
            return;
        }
        let Some(hex) = ident
            .name
            .strip_prefix("color.")
            .and_then(builtins::color_value)
        else {
            return;
        };
        if !self.exact() {
            self.note(
                "G2",
                RuleClass::Diagnostic,
                Severity::Info,
                (s, e),
                format!(
                    "`{}` was given a new value in v6; the exact profile keeps v5's {hex}.",
                    ident.name
                ),
                false,
            );
            return;
        }
        if self.free(s, e) && self.edits.replace(s, e, hex) {
            self.note(
                "G2",
                RuleClass::Exact,
                Severity::Info,
                (s, e),
                format!(
                    "Replaced `{}` with {hex}, its v5 value; v6 gave this constant a new value.",
                    ident.name
                ),
                true,
            );
        }
    }
}

/// The v6 spelling of a v5 unit-only timeframe string.
fn with_multiplier(value: &str) -> Option<&'static str> {
    Some(match value {
        "D" => "1D",
        "W" => "1W",
        "M" => "1M",
        "S" => "1S",
        _ => return None,
    })
}

/// Whether a literal is a multiplier-1 unit in v6 spelling ("1D"), which v5
/// never produced.
fn is_multiplied_unit(value: &str) -> bool {
    matches!(value, "1D" | "1W" | "1M" | "1S")
}
