//! Argument binding against the migrator's built-in signatures, `when`,
//! `strategy.exit` pairs, strategy margins, and the call-level value rules
//! (`transp`, the label text color default, `na` for special constant
//! parameters, `linewidth`).

use super::{Cx, Place, Site};
use crate::builtins::{self, Ty};
use crate::model::{self, for_each_child, peel};
use crate::text;
use crate::{RuleClass, Severity};
use pinewasm_syntax::{self as ast, Expr, Stmt, UnaryOp};

/// One argument bound to a parameter name.
#[derive(Debug, Clone)]
pub(super) struct Bound<'e> {
    pub(super) index: usize,
    pub(super) param: Option<&'static str>,
    pub(super) named: bool,
    pub(super) arg: &'e ast::CallArg,
}

impl Cx<'_, '_> {
    /// Bind a built-in call's arguments to the parameter names of their built-in signature.
    pub(super) fn bind<'e>(&self, func: &str, args: &'e [ast::CallArg]) -> Vec<Bound<'e>> {
        bind_with(self.model, func, args, 0)
    }

    pub(super) fn calls_stmt(&mut self, site: Site<'_>) {
        if let Stmt::Expr { expr, .. } = site.stmt {
            if let Expr::Call { callee, args, .. } = peel(expr) {
                if builtins::has_when(&callee.name) && self.when_rule(site, &callee.name, args) {
                    return;
                }
            }
        }
        let mut roots = Vec::new();
        model::for_each_stmt_expr(site.stmt, &mut |e| roots.push(e));
        for root in roots {
            self.calls_in(root, site);
        }
    }

    fn calls_in(&mut self, expr: &Expr, site: Site<'_>) {
        if let Some((func, args, receiver)) = builtin_call(self.model, expr) {
            if receiver == 0 {
                self.call_rules(site, &func, args, expr.span());
            } else {
                // A method-style call binds its arguments after the receiver;
                // only the argument-level rules apply.
                let bound = bind_with(self.model, &func, args, receiver);
                self.unique_na_args(&func, &bound, args);
            }
        }
        for_each_child(expr, &mut |c| self.calls_in(c, site));
    }

    /// Rules that act on one built-in call's argument list.
    fn call_rules(
        &mut self,
        site: Site<'_>,
        func: &str,
        args: &[ast::CallArg],
        span: pinewasm_span::Span,
    ) {
        let bound = self.bind(func, args);
        if self.duplicates(func, &bound, args) {
            return;
        }
        if builtins::has_when(func) {
            // A `when` argument on a call that is not a statement of its own.
            if bound.iter().any(|b| b.param == Some("when")) {
                self.note(
                    "C1",
                    RuleClass::Diagnostic,
                    Severity::Error,
                    (span.start as usize, span.end as usize),
                    format!("`{func}()` passes `when`, which v6 does not have, but the call is not a statement on its own line, so it cannot be moved into an `if` block automatically."),
                    false,
                );
            }
        }
        self.unique_na_args(func, &bound, args);
        self.positional_shifts(func, &bound, args);
        self.linewidth(func, &bound);
        self.transp(func, &bound, args);
        if func == "label.new" {
            self.label_textcolor(&bound, args, span);
        }
        if func == "strategy.exit" {
            self.exit_pairs(site, &bound, args);
        }
    }

    // -- Repeated arguments ------------------------------------------------------------------------

    fn duplicates(&mut self, func: &str, bound: &[Bound<'_>], args: &[ast::CallArg]) -> bool {
        let mut seen: Vec<&str> = Vec::new();
        let mut remove = Vec::new();
        for b in bound {
            let name = match (b.param, &b.arg.name) {
                (Some(p), _) => p,
                (None, Some(n)) => n.name.as_str(),
                (None, None) => continue,
            };
            if seen.contains(&name) {
                remove.push(b.index);
            } else {
                seen.push(name);
            }
        }
        if remove.is_empty() {
            return false;
        }
        let span = args[remove[0]].span;
        if !self.allowed(span.start as usize, span.end as usize) {
            return false;
        }
        if self.delete_args(args, &remove) {
            let evaluated = remove
                .iter()
                .any(|&i| self.model.must_evaluate(&args[i].value));
            self.note(
                "B2",
                RuleClass::Exact,
                if evaluated { Severity::Warning } else { Severity::Info },
                (span.start as usize, span.end as usize),
                if evaluated {
                    format!("Removed a repeated argument to `{func}()`; only the first one applies. The removed expression has side effects or history, so check that dropping it does not change the result.")
                } else {
                    format!("Removed a repeated argument to `{func}()`; only the first one applies.")
                },
                true,
            );
        }
        true
    }

    /// Delete arguments by index, keeping separators well-formed.
    pub(super) fn delete_args(&mut self, args: &[ast::CallArg], remove: &[usize]) -> bool {
        let keep: Vec<usize> = (0..args.len()).filter(|i| !remove.contains(i)).collect();
        let span = |i: usize| (args[i].span.start as usize, args[i].span.end as usize);
        let mut ranges = Vec::new();
        match keep.first() {
            None => {
                ranges.push((span(0).0, span(args.len() - 1).1));
            }
            Some(&first_kept) => {
                if first_kept > 0 {
                    ranges.push((span(0).0, span(first_kept).0));
                }
                for &i in remove {
                    if i > first_kept {
                        ranges.push((span(i - 1).1, span(i).1));
                    }
                }
            }
        }
        if ranges.iter().any(|(s, e)| !self.free(*s, *e)) {
            return false;
        }
        ranges
            .into_iter()
            .all(|(s, e)| self.edits.replace(s, e, ""))
    }

    /// Give positional arguments from `from` on their parameter names.
    fn name_positionals(&mut self, bound: &[Bound<'_>], from: usize, skip: &[usize]) -> bool {
        for b in bound
            .iter()
            .filter(|b| b.index >= from && !b.named && !skip.contains(&b.index))
        {
            let Some(param) = b.param else {
                return false;
            };
            let start = b.arg.span.start as usize;
            if !self.edits.insert_at(start, format!("{param} = ")) {
                return false;
            }
        }
        true
    }

    // -- when ------------------------------------------------------------------------

    /// Move a `when` condition into an `if` around the call. Every argument
    /// with history or effects is evaluated first, so each one still runs
    /// on every bar whether or not the condition holds.
    fn when_rule(&mut self, site: Site<'_>, func: &str, args: &[ast::CallArg]) -> bool {
        let bound = self.bind(func, args);
        let Some(when) = bound.iter().find(|b| b.param == Some("when")) else {
            return false;
        };
        let stmt = site.stmt;
        let (s_start, s_end) = (stmt.span().start as usize, stmt.span().end as usize);
        if !self.allowed(s_start, s_end) {
            return true;
        }
        if site.place != Place::Block {
            self.expand_inline_body(site);
            return true;
        }
        if !self.free(s_start, s_end) {
            return true;
        }
        let cond = &when.arg.value;
        let when_index = when.index;
        let positional_when = !when.named;
        if let Expr::Bool { value: true, .. } = peel(cond) {
            let ok = self.delete_args(args, &[when_index])
                && (!positional_when || self.name_positionals(&bound, when_index + 1, &[]));
            if ok {
                self.note(
                    "C1",
                    RuleClass::Exact,
                    Severity::Info,
                    (s_start, s_end),
                    format!("Removed `when = true` from `{func}()`; v6 has no `when` parameter."),
                    true,
                );
            }
            self.lock(s_start, s_end);
            return true;
        }
        let indent = self.indent_of(s_start);
        let mut lines = String::new();
        let mut hoisted_cond = None;
        let mut hoisted_any = false;
        for b in &bound {
            let value = &b.arg.value;
            if !self.model.must_evaluate(value) {
                continue;
            }
            let name = self.temps.fresh();
            lines.push_str(&format!("{indent}{name} = {}\n", self.expr_text(value)));
            hoisted_any = true;
            if b.index == when_index {
                hoisted_cond = Some(name);
            } else {
                let s = value.span();
                self.edits.replace(s.start as usize, s.end as usize, name);
            }
        }
        let cond_text = match (hoisted_cond, peel(cond)) {
            (Some(name), _) => name,
            (None, Expr::Na { .. } | Expr::Bool { value: false, .. }) => "false".to_string(),
            (None, _) => self.expr_text(cond).to_string(),
        };
        lines.push_str(&format!("{indent}if {cond_text}\n"));
        if !self.delete_args(args, &[when_index])
            || (positional_when && !self.name_positionals(&bound, when_index + 1, &[]))
        {
            return true;
        }
        let line = text::line_start(self.src, s_start);
        self.edits.insert_lines(line, lines);
        self.edits.insert_lines(line, self.unit);
        let mut pos = s_start;
        while let Some(i) = self.src[pos..s_end].find('\n') {
            let next = pos + i + 1;
            if next >= s_end {
                break;
            }
            self.edits.insert_lines(next, self.unit);
            pos = next;
        }
        self.lock(s_start, s_end);
        self.note(
            "C1",
            if hoisted_any {
                RuleClass::ExactWithHoisting
            } else {
                RuleClass::Exact
            },
            Severity::Info,
            (s_start, s_end),
            format!(
                "Moved the `when` condition of `{func}()` into an `if` block{}; v6 removed `when`.",
                if hoisted_any {
                    ", evaluating arguments with history or effects first so they still run on every bar"
                } else {
                    ""
                }
            ),
            true,
        );
        true
    }

    // -- Literal na for special constants -----------------------------------------------------------------------

    /// A literal `na` passed where the parameter requires a special constant:
    /// the parameter's default is passed instead, or the argument is omitted.
    fn unique_na_args(&mut self, func: &str, bound: &[Bound<'_>], args: &[ast::CallArg]) {
        for b in bound {
            let Some(param) = b.param else { continue };
            let Some(unique) = builtins::unique_param(func, param) else {
                continue;
            };
            if !matches!(peel(&b.arg.value), Expr::Na { .. }) {
                continue;
            }
            let span = b.arg.span;
            let (s, e) = (span.start as usize, span.end as usize);
            if !self.allowed(s, e) || !self.free(s, e) {
                continue;
            }
            let last = b.index + 1 == args.len();
            let applied = if b.named || last {
                self.delete_args(args, &[b.index])
            } else if let Some(default) = unique.default_constant() {
                let v = b.arg.value.span();
                self.edits
                    .replace(v.start as usize, v.end as usize, default)
            } else {
                self.delete_args(args, &[b.index]) && self.name_positionals(bound, b.index + 1, &[])
            };
            if applied {
                self.note("D4a", RuleClass::Exact, Severity::Info, (s, e), format!("Removed `na` from `{func}({param})`: a special constant type cannot be `na`, so the parameter's default applies."), true);
            }
        }
    }

    // -- Positional shifts ------------------------------------------------------------------------

    /// Positional arguments whose v6 position means something else.
    fn positional_shifts(&mut self, func: &str, bound: &[Bound<'_>], args: &[ast::CallArg]) {
        let from = match func {
            // `active` sits before `confirm` in the signature, so later positions differ.
            "input.source" if bound.iter().filter(|b| !b.named).count() >= 7 => Some(6),
            // Name the arguments from the bars_back position on so the binding is explicit.
            "time" | "time_close" => bound
                .iter()
                .find(|b| !b.named && b.param == Some("bars_back"))
                .map(|b| b.index),
            _ => None,
        };
        let Some(from) = from else { return };
        let span = args[from].span;
        let (s, e) = (span.start as usize, span.end as usize);
        if !self.allowed(s, e) || !self.free(s, args.last().map_or(e, |a| a.span.end as usize)) {
            return;
        }
        if self.name_positionals(bound, from, &[]) {
            self.note("B3", RuleClass::Exact, Severity::Info, (s, e), format!("Named the arguments of `{func}()` from this position on, because this position binds to a different parameter in the target signature."), true);
        }
    }

    // -- Line width ------------------------------------------------------------------------

    fn linewidth(&mut self, func: &str, bound: &[Bound<'_>]) {
        if !matches!(func, "plot" | "hline") {
            return;
        }
        let Some(b) = bound.iter().find(|b| b.param == Some("linewidth")) else {
            return;
        };
        let value = &b.arg.value;
        let span = value.span();
        let (s, e) = (span.start as usize, span.end as usize);
        if !self.allowed(s, e) || !self.free(s, e) {
            return;
        }
        let replacement = match const_number(value) {
            Some(v) if v < 1.0 => Some(format!("math.max(1, {})", self.expr_text(value))),
            Some(_) => None,
            None if self.at_least_one(value) || is_clamped(value) => None,
            None => Some(format!("math.max(1, {})", self.expr_text(value))),
        };
        if let Some(r) = replacement {
            if self.edits.replace(s, e, r) {
                self.note("D8", RuleClass::Exact, Severity::Info, (s, e), format!("`{func}()` width is clamped to at least 1, because a width below 1 is not valid."), true);
            }
        }
    }

    /// Whether a width expression is provably at least 1: an `input.int`
    /// with `minval >= 1`, or a variable initialized with one.
    fn at_least_one(&self, value: &Expr) -> bool {
        match peel(value) {
            Expr::Call { callee, args, .. }
                if callee.name == "input.int" || callee.name == "input" =>
            {
                let minval = args
                    .iter()
                    .find(|a| a.name.as_ref().is_some_and(|n| n.name == "minval"))
                    .and_then(|a| const_number(&a.value));
                let defval = args.first().and_then(|a| const_number(&a.value));
                minval.is_some_and(|m| m >= 1.0) && defval.is_some_and(|d| d >= 1.0)
            }
            Expr::Ident(ident) => {
                self.model.decl_of(ident).is_some_and(|d| !d.reassigned)
                    && self.model.init_of(ident).is_some_and(|init| {
                        const_number(init).is_some_and(|v| v >= 1.0) || self.at_least_one(init)
                    })
            }
            _ => false,
        }
    }

    // -- transp ------------------------------------------------------------------------

    /// `transp` was removed in v6. The migration applies it only to colors without
    /// their own opacity. Constructor colors carry their own opacity, so the migration leaves them unchanged.
    fn transp(&mut self, func: &str, bound: &[Bound<'_>], args: &[ast::CallArg]) {
        if !builtins::accepts_transp(func) {
            return;
        }
        let Some(t) = bound
            .iter()
            .find(|b| b.arg.name.as_ref().is_some_and(|n| n.name == "transp"))
        else {
            return;
        };
        let span = t.arg.span;
        let (s, e) = (span.start as usize, span.end as usize);
        if !self.allowed(s, e) {
            return;
        }
        let color_param = match func {
            "plotarrow" => "colorup",
            "fill" if bound.iter().any(|b| b.param == Some("top_color")) => "top_color",
            _ => "color",
        };
        let t_text = self.expr_text(&t.arg.value).to_string();
        // `transp` applies to both arrow directions, so `colordown` is
        // handled along with `colorup`. Skipping it would leave the down
        // arrows without the transparency.
        if func == "plotarrow" {
            if let Some(down) = bound.iter().find(|b| b.param == Some("colordown")) {
                if matches!(self.color_opacity(&down.arg.value), Opacity::Opaque) {
                    let down_span = down.arg.value.span();
                    let (ds, de) = (down_span.start as usize, down_span.end as usize);
                    if self.allowed(ds, de) && self.free(ds, de) {
                        let wrapped =
                            format!("color.new({}, {t_text})", self.expr_text(&down.arg.value));
                        if self.edits.replace(ds, de, wrapped) {
                            self.note(
                                "G4",
                                RuleClass::Exact,
                                Severity::Info,
                                (ds, de),
                                "Preserved the down-arrow transparency with `color.new()`.",
                                true,
                            );
                        }
                    }
                }
            }
        }
        let color = bound.iter().find(|b| b.param == Some(color_param));
        match color {
            Some(c) => {
                let class = self.color_opacity(&c.arg.value);
                let cv = c.arg.value.span();
                let (cs, ce) = (cv.start as usize, cv.end as usize);
                if !self.free(cs, ce) {
                    return;
                }
                // A conditional can select colors with different opacity
                // ownership. Apply `transp` only to its plain opaque leaves,
                // without evaluating its condition twice.
                if class == Opacity::Runtime && matches!(peel(&c.arg.value), Expr::Ternary { .. }) {
                    let mut leaves = Vec::new();
                    if self.conditional_color_leaves(&c.arg.value, &mut leaves)
                        && self.delete_args(args, &[t.index])
                    {
                        for leaf in leaves {
                            if self.color_opacity(leaf) == Opacity::Opaque {
                                let span = leaf.span();
                                self.edits.replace(
                                    span.start as usize,
                                    span.end as usize,
                                    format!("color.new({}, {t_text})", self.expr_text(leaf)),
                                );
                            }
                        }
                        self.note("G4", RuleClass::Exact, Severity::Info, (s, e), "Applied `transp` to opaque conditional branches and kept constructor-defined opacity.", true);
                        return;
                    }
                }
                match class {
                    Opacity::Opaque => {
                        let wrapped =
                            format!("color.new({}, {t_text})", self.expr_text(&c.arg.value));
                        if self.delete_args(args, &[t.index]) && self.edits.replace(cs, ce, wrapped)
                        {
                            self.note("G4", RuleClass::Exact, Severity::Info, (s, e), format!("Folded `transp` into the `{func}()` color with `color.new()`; v6 removed `transp`."), true);
                        }
                    }
                    Opacity::Transparent => {
                        if self.delete_args(args, &[t.index]) {
                            self.note("G4", RuleClass::Exact, Severity::Info, (s, e), "Removed `transp`: the color defines its own opacity, so `transp` is not applied.", true);
                        }
                    }
                    Opacity::Runtime => {
                        if self.delete_args(args, &[t.index]) {
                            self.note("G4", RuleClass::Approximate, Severity::Warning, (s, e), "Removed `transp`: the opacity of this color is only known at runtime. If the transparency is needed, apply it with `color.new()`.", true);
                        }
                    }
                }
            }
            // The migrator models `color.blue` as the color of `plot*()` calls without one.
            None if matches!(func, "plot" | "plotshape" | "plotchar") => {
                if self.delete_args(args, &[t.index]) {
                    let anchor = args
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| *i != t.index)
                        .map(|(_, a)| a.span.end as usize)
                        .max();
                    if let Some(end) = anchor {
                        self.edits
                            .insert_at(end, format!(", color = color.new(color.blue, {t_text})"));
                        self.note("G4", RuleClass::Exact, Severity::Info, (s, e), "Replaced `transp` with an explicit color: `color.blue`, the color the migrator assumes for this call, with the given transparency.", true);
                    }
                }
            }
            None => {
                self.note("G4", RuleClass::Diagnostic, Severity::Error, (s, e), format!("`{func}()` sets `transp` without a color, and the migrator has no default color to apply it to; pass `color = color.new(<color>, {t_text})` and remove `transp`."), false);
                let _ = self.delete_args(args, &[t.index]);
            }
        }
    }

    fn color_opacity(&self, color: &Expr) -> Opacity {
        self.color_opacity_cached(color, &mut std::collections::HashMap::new())
    }

    fn color_opacity_cached(
        &self,
        color: &Expr,
        known: &mut std::collections::HashMap<(u32, u32), Opacity>,
    ) -> Opacity {
        let color = peel(color);
        let span = color.span();
        let key = (span.start, span.end);
        if let Some(opacity) = known.get(&key) {
            return *opacity;
        }
        // Shared alias initializers must be visited once, rather than once
        // per conditional path. The provisional value also breaks cycles.
        known.insert(key, Opacity::Runtime);
        let opacity = match color {
            // `na` draws no color, so `transp` has nothing to apply to. As a
            // conditional branch it stays `na` while the plain colors
            // beside it take the `transp` value.
            Expr::Na { .. } => Opacity::Transparent,
            Expr::Color { t, .. } => {
                if *t == 0.0 {
                    Opacity::Opaque
                } else {
                    Opacity::Transparent
                }
            }
            Expr::Ident(ident) => {
                if builtins::constant(&ident.name) == Some(Ty::Color) {
                    Opacity::Opaque
                } else if self.model.decl_of(ident).is_some_and(|d| !d.reassigned) {
                    self.model.init_of(ident).map_or(Opacity::Runtime, |init| {
                        self.color_opacity_cached(init, known)
                    })
                } else {
                    Opacity::Runtime
                }
            }
            Expr::Ternary { then_e, else_e, .. } => {
                let left = self.color_opacity_cached(then_e, known);
                let right = self.color_opacity_cached(else_e, known);
                if left == right {
                    left
                } else {
                    Opacity::Runtime
                }
            }
            Expr::Call { callee, args, .. } => match callee.name.as_str() {
                // Constructor colors define their own opacity, so the plot
                // transparency argument does not change them. The rule applies
                // the same way whether arguments are named or positional.
                "color.rgb" | "color.new" => Opacity::Transparent,
                "input.color" | "input" => args
                    .first()
                    .filter(|a| a.name.is_none())
                    .or_else(|| {
                        args.iter()
                            .find(|a| a.name.as_ref().is_some_and(|name| name.name == "defval"))
                    })
                    .map_or(Opacity::Runtime, |a| {
                        self.color_opacity_cached(&a.value, known)
                    }),
                _ => Opacity::Runtime,
            },
            _ => Opacity::Runtime,
        };
        known.insert(key, opacity);
        opacity
    }

    fn conditional_color_leaves<'e>(&self, color: &'e Expr, leaves: &mut Vec<&'e Expr>) -> bool {
        match peel(color) {
            Expr::Ternary { then_e, else_e, .. } => {
                self.conditional_color_leaves(then_e, leaves)
                    && self.conditional_color_leaves(else_e, leaves)
            }
            leaf if self.color_opacity(leaf) != Opacity::Runtime => {
                leaves.push(leaf);
                true
            }
            _ => false,
        }
    }

    // -- label text color ------------------------------------------------------------------------

    /// The migration sets the label text color to black explicitly, so it does not depend on the default.
    fn label_textcolor(
        &mut self,
        bound: &[Bound<'_>],
        args: &[ast::CallArg],
        span: pinewasm_span::Span,
    ) {
        if bound.iter().any(|b| b.param == Some("textcolor")) {
            return;
        }
        let (s, e) = (span.start as usize, span.end as usize);
        if !self.allowed(s, e) {
            return;
        }
        let Some(last) = args.last() else { return };
        let end = last.span.end as usize;
        if self.edits.insert_at(end, ", textcolor = color.black") {
            self.note("G3", RuleClass::Exact, Severity::Info, (s, e), "Set `textcolor = color.black` explicitly so the label text color does not depend on the default.", true);
        }
    }

    // -- exit levels ------------------------------------------------------------------------

    /// When `strategy.exit()` gets both an absolute level and its relative
    /// twin, the migrator models the absolute one as applying on every bar
    /// where it is not `na`. Without a guard, both levels would be active on
    /// the same bar, so the migration passes the relative level only where
    /// the absolute one is `na`.
    fn exit_pairs(&mut self, site: Site<'_>, bound: &[Bound<'_>], args: &[ast::CallArg]) {
        for (abs, rel) in [
            ("limit", "profit"),
            ("stop", "loss"),
            ("trail_price", "trail_points"),
        ] {
            let (Some(a), Some(r)) = (
                bound.iter().find(|b| b.param == Some(abs)),
                bound.iter().find(|b| b.param == Some(rel)),
            ) else {
                continue;
            };
            let (av, rv) = (&a.arg.value, &r.arg.value);
            let rs = rv.span();
            let (s, e) = (rs.start as usize, rs.end as usize);
            if !self.allowed(s, e) || !self.free(s, e) {
                continue;
            }
            if already_guarded(self.src, av, rv) {
                continue;
            }
            if matches!(peel(av), Expr::Na { .. }) {
                if self.delete_args(args, &[a.index])
                    && (a.named || self.name_positionals(bound, a.index + 1, &[]))
                {
                    self.note(
                        "C2",
                        RuleClass::Exact,
                        Severity::Info,
                        (s, e),
                        format!("Removed `{abs} = na` so `{rel}` applies alone."),
                        true,
                    );
                }
                continue;
            }
            if const_number(av).is_some() {
                if self.delete_args(args, &[r.index])
                    && (r.named || self.name_positionals(bound, r.index + 1, &[]))
                {
                    self.note("C2", RuleClass::Exact, Severity::Info, (s, e), format!("Removed `{rel}`: `{abs}` is always set, so the relative level never applies."), true);
                }
                continue;
            }
            // Expressions that must run every bar are evaluated first so the
            // ternary below never skips them.
            let mut targets = Vec::new();
            if self.model.must_evaluate(av) {
                targets.push(av);
            }
            if self.model.must_evaluate(rv) {
                targets.push(rv);
            }
            if !targets.is_empty() {
                let _ = self.hoist(site, &targets);
                continue;
            }
            let guarded = format!("na({}) ? {} : na", self.expr_text(av), self.expr_text(rv));
            if self.edits.replace(s, e, guarded) {
                self.note("C2", RuleClass::ExactWithHoisting, Severity::Info, (s, e), format!("`{rel}` now applies only where `{abs}` is `na`; otherwise whichever level is reached first closes the trade."), true);
            }
        }
    }

    // -- declarations --------------------------------------------------------------

    pub(super) fn calls_declaration(
        &mut self,
        kind: &str,
        args: &[ast::CallArg],
        span: pinewasm_span::Span,
    ) {
        let bound = self.bind(kind, args);
        if self.duplicates(kind, &bound, args) {
            return;
        }
        self.unique_na_args(kind, &bound, args);
        if kind != "strategy" {
            return;
        }
        // Omitted margins are added as 0, which the migrator models as unlimited.
        let Some(last) = args.last() else { return };
        let end = last.span.end as usize;
        let (s, e) = (span.start as usize, span.end as usize);
        if !self.allowed(s, e) {
            return;
        }
        let mut added = Vec::new();
        for param in ["margin_long", "margin_short"] {
            if !bound.iter().any(|b| b.param == Some(param)) {
                added.push(format!("{param} = 0"));
            }
        }
        if !added.is_empty() && self.edits.insert_at(end, format!(", {}", added.join(", "))) {
            self.note(
                "C3",
                RuleClass::Exact,
                Severity::Info,
                (s, e),
                format!(
                    "Added `{}`: the migrator models an omitted margin as unlimited.",
                    added.join("`, `")
                ),
                true,
            );
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opacity {
    Opaque,
    Transparent,
    Runtime,
}

/// The built-in a call expression invokes, its arguments, and how many
/// leading parameters a method-style receiver fills (0 or 1).
pub(super) fn builtin_call<'e>(
    model: &crate::model::Model<'_>,
    expr: &'e Expr,
) -> Option<(String, &'e [ast::CallArg], usize)> {
    match expr {
        Expr::Call { callee, args, .. } => {
            if let Some(name) = model.resolved_methods.get(&callee.span.start) {
                return Some((name.clone(), args, 1));
            }
            (!model.funcs.contains_key(&callee.name) && builtins::is_known_function(&callee.name))
                .then(|| (callee.name.clone(), args.as_slice(), 0))
        }
        Expr::MethodCall { method, args, .. } => model
            .resolved_methods
            .get(&method.span.start)
            .map(|name| (name.clone(), args.as_slice(), 1)),
        _ => None,
    }
}

/// Bind a built-in call's arguments to the parameter names of their built-in signature. `receiver` is 1
/// for a method-style call, whose receiver fills the first parameter.
pub(super) fn bind_with<'e>(
    model: &crate::model::Model<'_>,
    func: &str,
    args: &'e [ast::CallArg],
    receiver: usize,
) -> Vec<Bound<'e>> {
    let positional: Vec<Ty> = args
        .iter()
        .take_while(|a| a.name.is_none())
        .map(|a| model.ty(&a.value))
        .collect();
    let sig = builtins::v5_signature(func, &positional);
    let ta = builtins::ta_params(func);
    let mut out = Vec::with_capacity(args.len());
    let mut position = receiver;
    for (index, arg) in args.iter().enumerate() {
        match &arg.name {
            Some(n) => out.push(Bound {
                index,
                param: sig
                    .as_ref()
                    .and_then(|s| s.params.iter().copied().find(|p| *p == n.name))
                    .or_else(|| ta.iter().copied().find(|p| *p == n.name))
                    .or_else(|| static_param(&n.name)),
                named: true,
                arg,
            }),
            None => {
                out.push(Bound {
                    index,
                    param: sig
                        .as_ref()
                        .and_then(|s| s.params.get(position).copied())
                        .or_else(|| ta.get(position).copied()),
                    named: false,
                    arg,
                });
                position += 1;
            }
        }
    }
    out
}

/// Locate the index of an array access, including calls that put named
/// arguments in a different order from the positional signature.
pub(super) fn array_index_arg(args: &[ast::CallArg], receiver: usize) -> Option<&ast::CallArg> {
    args.iter()
        .find(|arg| arg.name.as_ref().is_some_and(|name| name.name == "index"))
        .or_else(|| args.get(1 - receiver))
}

/// Parameter names every signature shares, for named arguments on calls
/// without a modeled layout.
fn static_param(name: &str) -> Option<&'static str> {
    Some(match name {
        "when" => "when",
        "linewidth" => "linewidth",
        "textcolor" => "textcolor",
        "color" => "color",
        "style" => "style",
        "display" => "display",
        "offset" => "offset",
        "order" => "order",
        "gaps" => "gaps",
        "lookahead" => "lookahead",
        "scale" => "scale",
        "direction" => "direction",
        "linestyle" => "linestyle",
        "backadjustment" => "backadjustment",
        "settlement_as_close" => "settlement_as_close",
        "transp" => "transp",
        "title" => "title",
        _ => return None,
    })
}

/// The numeric value of a literal (or a negated literal).
pub(super) fn const_number(expr: &Expr) -> Option<f64> {
    match peel(expr) {
        Expr::Int { value, .. } => Some(*value as f64),
        Expr::Float { value, .. } => Some(*value),
        Expr::Unary {
            op: UnaryOp::Neg,
            operand,
            ..
        } => const_number(operand).map(|v| -v),
        Expr::Unary {
            op: UnaryOp::Pos,
            operand,
            ..
        } => const_number(operand),
        _ => None,
    }
}

fn is_clamped(expr: &Expr) -> bool {
    matches!(peel(expr), Expr::Call { callee, args, .. }
        if callee.name == "math.max" && args.iter().any(|a| const_number(&a.value).is_some_and(|v| v >= 1.0)))
}

/// Whether the relative level is already `na(abs) ? rel : na`.
fn already_guarded(src: &str, abs: &Expr, rel: &Expr) -> bool {
    let Expr::Ternary { cond, else_e, .. } = peel(rel) else {
        return false;
    };
    let Expr::Call { callee, args, .. } = peel(cond) else {
        return false;
    };
    let text = |e: &Expr| &src[e.span().start as usize..e.span().end as usize];
    callee.name == "na"
        && matches!(peel(else_e), Expr::Na { .. })
        && args.first().is_some_and(|a| text(&a.value) == text(abs))
}
