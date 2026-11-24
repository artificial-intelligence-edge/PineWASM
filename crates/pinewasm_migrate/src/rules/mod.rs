//! The migration driver, the statement walker, and the hoisting machinery
//! the individual rules share.
//!
//! Rules are grouped by phase (see the crate docs). A phase runs as a loop:
//! parse the current text, build the model, walk every statement letting the
//! phase's rules propose edits, apply them, and repeat until an iteration
//! proposes nothing. A rule whose edit collides with one already proposed
//! simply tries again next iteration against the re-parsed text, so rules
//! never need to reason about each other's pending edits.

mod calls;
mod detect;
mod history;
mod modernize;
mod order;
mod requests;
mod types;
mod values;

use crate::builtins::Ty;
use crate::model::{self, for_each_child, key, peel, Model};
use crate::text::{self, EditSet, OffsetMap};
use crate::{Finding, Migration, MigrationOptions, Profile, RuleClass, Severity, Status, TextEdit};
use pinewasm_syntax::{self as ast, Expr, Stmt};
use std::collections::{BTreeSet, HashSet};

/// Iterations a phase may take before the driver gives up on it. Phases
/// settle in a few iterations; the cap guards against a rule that keeps
/// re-matching its own output.
const MAX_ITERATIONS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Call-level rules: argument binding, `when`,
    /// `strategy.exit` pairs, margins, `transp`, label text color, unique
    /// `na` arguments, `linewidth`.
    Calls,
    /// Type rules: bool casts, bool `na`, `na` comparisons, unique-type branches,
    /// integer division.
    Types,
    /// Evaluation order: lazy `and`/`or`, dynamic `for` bounds, signed steps.
    Order,
    /// Literal and UDT-field history.
    History,
    /// `timeframe.period` and recolored constants.
    Values,
    /// `dynamic_requests`.
    Requests,
    /// Optional modernization.
    Modernize,
}

const PHASES: [Phase; 7] = [
    Phase::Calls,
    Phase::Types,
    Phase::Order,
    Phase::History,
    Phase::Values,
    Phase::Requests,
    Phase::Modernize,
];

/// Helper functions a rewrite needs defined once at global scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Helper {
    /// `timeframe.period`, with a multiplier of 1 written without the number.
    TimeframePeriod,
    /// Raises a runtime error for a negative array index.
    CheckedIndex,
}

impl Helper {
    fn name(self) -> &'static str {
        match self {
            Helper::TimeframePeriod => "__v6_timeframePeriodV5",
            Helper::CheckedIndex => "__v6_checkedIndex",
        }
    }

    fn definition(self, unit: &str) -> String {
        match self {
            Helper::TimeframePeriod => format!(
                "// v5 spelled a multiplier of 1 without the number (\"D\", not \"1D\").\n\
                 {name}() =>\n\
                 {unit}timeframe.multiplier != 1 ? timeframe.period : timeframe.isdaily ? \"D\" : timeframe.isweekly ? \"W\" : timeframe.ismonthly ? \"M\" : timeframe.isseconds ? \"S\" : timeframe.period\n",
                name = self.name()
            ),
            Helper::CheckedIndex => format!(
                "// Raises a runtime error for a negative array index.\n\
                 {name}(int index) =>\n\
                 {unit}if index < 0\n\
                 {unit}{unit}runtime.error(\"negative array index\")\n\
                 {unit}index\n",
                name = self.name()
            ),
        }
    }
}

/// Fresh identifiers for temporaries: `__v6_<n>`, skipping any name that
/// already appears in the source.
pub(crate) struct TempNames {
    taken: HashSet<String>,
    next: usize,
}

impl TempNames {
    fn new(source: &str) -> Self {
        let mut taken = HashSet::new();
        let bytes = source.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'_' || bytes[i].is_ascii_alphabetic() {
                let start = i;
                while i < bytes.len() && (bytes[i] == b'_' || bytes[i].is_ascii_alphanumeric()) {
                    i += 1;
                }
                taken.insert(source[start..i].to_string());
            } else {
                i += 1;
            }
        }
        TempNames { taken, next: 1 }
    }

    pub(crate) fn fresh(&mut self) -> String {
        loop {
            let name = format!("__v6_{}", self.next);
            self.next += 1;
            if self.taken.insert(name.clone()) {
                return name;
            }
        }
    }
}

/// A finding in the coordinates of the text the current iteration sees.
#[derive(Debug, Clone)]
pub(crate) struct Note {
    pub(crate) rule: &'static str,
    pub(crate) class: RuleClass,
    pub(crate) severity: Severity,
    pub(crate) message: String,
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) applied: bool,
}

/// Where a statement sits, which decides how code can be placed before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Place {
    /// On its own line in a block: temporaries go on the lines above.
    Block,
    /// The single-line body of `f(x) => expr`.
    InlineFn,
    /// The single-line body of a switch arm `label => expr`.
    InlineArm,
    /// The `if` of an `else if`, whose condition only runs when the earlier
    /// branches failed.
    ElseIf,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Site<'s> {
    pub(crate) stmt: &'s Stmt,
    pub(crate) place: Place,
    /// Start of the line of the top-level item containing the statement.
    pub(crate) top_line: usize,
}

pub(crate) struct Cx<'a, 'p> {
    pub(crate) src: &'a str,
    pub(crate) model: &'a Model<'p>,
    pub(crate) edits: EditSet,
    pub(crate) notes: Vec<Note>,
    pub(crate) opts: &'a MigrationOptions,
    pub(crate) unit: &'static str,
    pub(crate) temps: &'a mut TempNames,
    pub(crate) helpers: &'a mut BTreeSet<Helper>,
    offsets: &'a OffsetMap,
    range: Option<(u32, u32)>,
    pub(crate) phase: Phase,
    /// Which iteration of the phase this is (0 = first).
    pub(crate) iteration: usize,
    /// Statements already restructured this iteration.
    locked: Vec<(usize, usize)>,
}

impl Cx<'_, '_> {
    // -- small utilities ---------------------------------------------------------

    pub(crate) fn text(&self, span: pinewasm_span::Span) -> &str {
        &self.src[span.start as usize..span.end as usize]
    }

    pub(crate) fn expr_text(&self, expr: &Expr) -> &str {
        self.text(expr.span())
    }

    pub(crate) fn exact(&self) -> bool {
        self.opts.profile == Profile::Exact
    }

    pub(crate) fn in_range_mode(&self) -> bool {
        self.range.is_some()
    }

    /// Whether a rewrite at `[start, end)` is in scope for this run.
    pub(crate) fn allowed(&self, start: usize, end: usize) -> bool {
        let Some(range) = self.range else {
            return true;
        };
        let s = self.offsets.to_original(start) as u32;
        let e = self.offsets.to_original(end) as u32;
        ranges_overlap(s, e, range)
    }

    fn locked(&self, start: usize, end: usize) -> bool {
        self.locked.iter().any(|(s, e)| start < *e && *s < end)
    }

    pub(crate) fn lock(&mut self, start: usize, end: usize) {
        self.locked.push((start, end));
    }

    /// Whether `[start, end)` is free for a structural rewrite.
    pub(crate) fn free(&self, start: usize, end: usize) -> bool {
        !self.edits.touches(start, end) && !self.locked(start, end)
    }

    pub(crate) fn note(
        &mut self,
        rule: &'static str,
        class: RuleClass,
        severity: Severity,
        span: (usize, usize),
        message: impl Into<String>,
        applied: bool,
    ) {
        self.notes.push(Note {
            rule,
            class,
            severity,
            message: message.into(),
            start: span.0,
            end: span.1,
            applied,
        });
    }

    pub(crate) fn indent_of(&self, pos: usize) -> String {
        text::indent_at(self.src, pos).to_string()
    }

    // -- walking -----------------------------------------------------------------

    fn walk(&mut self) {
        let program = self.model.program;
        for item in &program.items {
            let top_line = text::line_start(self.src, item.span().start as usize);
            match item {
                ast::Item::Stmt(stmt) => {
                    if self.phase == Phase::Types {
                        self.statement_if_tail(stmt);
                    }
                    self.visit_stmt(Site {
                        stmt,
                        place: Place::Block,
                        top_line,
                    });
                }
                ast::Item::FuncDef(f) => self.visit_function(f, top_line),
                ast::Item::Indicator(d) => self.declaration("indicator", &d.args, d.span),
                ast::Item::Strategy(d) => self.declaration("strategy", &d.args, d.span),
                ast::Item::Library(d) => self.declaration("library", &d.args, d.span),
                ast::Item::TypeDef(t) => self.type_def(t),
                ast::Item::EnumDef(_) | ast::Item::Import(_) => {}
            }
        }
        if self.phase == Phase::Requests {
            self.dynamic_requests();
        }
    }

    /// Whether a one-statement body follows its `=>` on the same logical
    /// line (possibly wrapped onto a continuation line).
    fn inline_body(&self, body: &[Stmt]) -> bool {
        let [only] = body else { return false };
        let start = only.span().start as usize;
        let before = &self.src[..start];
        let Some(arrow) = before.rfind("=>") else {
            return false;
        };
        let gap = &self.src[arrow + 2..start];
        if !gap.contains('\n') {
            return true;
        }
        // A body on the next line is a block only when indented by whole
        // units; any other indentation continues the `=>` line.
        let indent = text::indent_at(self.src, start);
        !indent.starts_with('\t') && indent.len() % 4 != 0
    }

    fn visit_function(&mut self, f: &ast::FuncDef, top_line: usize) {
        if self.phase == Phase::Types {
            self.function_params(f);
        }
        let inline = self.inline_body(&f.body);
        for stmt in &f.body {
            self.visit_stmt(Site {
                stmt,
                place: if inline {
                    Place::InlineFn
                } else {
                    Place::Block
                },
                top_line,
            });
        }
        // Visit nested value blocks first so their tail reads precede a
        // function-level tail read when both end at the same source offset.
        if self.phase == Phase::Types {
            self.value_block_tail(&f.body);
        }
    }

    fn visit_block(&mut self, body: &[Stmt], parent: Site<'_>) {
        for stmt in body {
            self.visit_stmt(Site {
                stmt,
                place: Place::Block,
                ..parent
            });
        }
    }

    fn visit_stmt(&mut self, site: Site<'_>) {
        match self.phase {
            Phase::Calls => self.calls_stmt(site),
            Phase::Types => self.types_stmt(site),
            Phase::Order => self.order_stmt(site),
            Phase::History => self.history_stmt(site),
            Phase::Values => self.values_stmt(site),
            Phase::Requests => {}
            Phase::Modernize => self.modernize_stmt(site),
        }
        // Blocks nested inside the statement's expressions (`x = if ...`).
        let stmt = site.stmt;
        model::for_each_stmt_expr(stmt, &mut |e| self.visit_expr_blocks(e, site));
        match stmt {
            Stmt::If(i) => {
                self.visit_block(&i.then_branch, site);
                if let Some(else_branch) = &i.else_branch {
                    self.visit_else(else_branch, site);
                }
            }
            Stmt::Once(o) => self.visit_block(&o.body, site),
            Stmt::For(f) => {
                self.visit_block(&f.body, site);
                if self.phase == Phase::Types {
                    self.value_block_tail(&f.body);
                }
            }
            Stmt::ForIn(f) => {
                self.visit_block(&f.body, site);
                if self.phase == Phase::Types {
                    self.value_block_tail(&f.body);
                }
            }
            Stmt::While(w) => {
                self.visit_block(&w.body, site);
                if self.phase == Phase::Types {
                    self.value_block_tail(&w.body);
                }
            }
            Stmt::Switch(s) => self.visit_arms(s, site),
            _ => {}
        }
    }

    fn visit_else(&mut self, body: &[Stmt], parent: Site<'_>) {
        if let [nested @ Stmt::If(_)] = body {
            if self.is_else_if(nested) {
                self.visit_stmt(Site {
                    stmt: nested,
                    place: Place::ElseIf,
                    ..parent
                });
                return;
            }
        }
        self.visit_block(body, parent);
    }

    /// Whether an `if` statement is the `if` of an `else if` (it starts on
    /// the line of its `else`).
    pub(crate) fn is_else_if(&self, stmt: &Stmt) -> bool {
        let start = stmt.span().start as usize;
        self.src[text::line_start(self.src, start)..start]
            .trim_end()
            .ends_with("else")
    }

    fn visit_arms(&mut self, s: &ast::SwitchStmt, parent: Site<'_>) {
        for arm in &s.arms {
            let inline = self.inline_body(&arm.body);
            for stmt in &arm.body {
                self.visit_stmt(Site {
                    stmt,
                    place: if inline {
                        Place::InlineArm
                    } else {
                        Place::Block
                    },
                    ..parent
                });
            }
        }
    }

    fn visit_expr_blocks(&mut self, expr: &Expr, site: Site<'_>) {
        match expr {
            Expr::If(i) => {
                self.visit_expr_blocks(&i.cond, site);
                self.visit_block(&i.then_branch, site);
                if let Some(e) = &i.else_branch {
                    self.visit_else(e, site);
                }
                if self.phase == Phase::Types {
                    self.value_block_tail(&i.then_branch);
                    if let Some(else_branch) = &i.else_branch {
                        self.value_block_tail(else_branch);
                    }
                }
            }
            Expr::Switch(s) => {
                if let Some(subject) = &s.subject {
                    self.visit_expr_blocks(subject, site);
                }
                self.visit_arms(s, site);
                if self.phase == Phase::Types {
                    for arm in &s.arms {
                        self.value_block_tail(&arm.body);
                    }
                }
            }
            Expr::Loop(stmt) => {
                model::for_each_stmt_expr(stmt, &mut |e| self.visit_expr_blocks(e, site));
                let body = match stmt.as_ref() {
                    Stmt::For(f) => &f.body,
                    Stmt::ForIn(f) => &f.body,
                    Stmt::While(w) => &w.body,
                    _ => return,
                };
                self.visit_block(body, site);
                if self.phase == Phase::Types {
                    self.value_block_tail(body);
                }
            }
            _ => for_each_child(expr, &mut |c| self.visit_expr_blocks(c, site)),
        }
    }

    // -- hoisting ------------------------------------------------------------------

    /// The expressions a statement evaluates, in evaluation order, that run
    /// every time the statement does (later `switch` labels run only when
    /// earlier ones fail, so only the first is included).
    pub(crate) fn roots(stmt: &Stmt) -> Vec<&Expr> {
        match stmt {
            Stmt::Switch(s) => {
                let mut v: Vec<&Expr> = s.subject.iter().collect();
                if s.subject.is_none() {
                    if let Some(label) = s.arms.first().and_then(|a| a.label.as_ref()) {
                        v.push(label);
                    }
                } else {
                    v.extend(s.arms.iter().filter_map(|a| a.label.as_ref()));
                }
                v
            }
            _ => {
                let mut v = Vec::new();
                model::for_each_stmt_expr(stmt, &mut |e| v.push(e));
                v
            }
        }
    }

    /// Must-evaluate subtrees that run before `target` in the same statement,
    /// in order. Hoisting `target` alone would run it before them.
    fn prefix_units<'s>(&self, stmt: &'s Stmt, target: &Expr) -> Vec<&'s Expr> {
        let tk = key(target);
        let ts = target.span();
        let mut out = Vec::new();
        let mut stop = false;
        for root in Self::roots(stmt) {
            self.collect_prefix(root, tk, ts, &mut out, &mut stop);
            if stop {
                break;
            }
        }
        out
    }

    fn collect_prefix<'s>(
        &self,
        e: &'s Expr,
        tk: model::Key,
        ts: pinewasm_span::Span,
        out: &mut Vec<&'s Expr>,
        stop: &mut bool,
    ) {
        if *stop {
            return;
        }
        let s = e.span();
        if key(e) == tk {
            *stop = true;
            return;
        }
        if s.start <= ts.start && ts.end <= s.end {
            match e {
                Expr::Ternary { cond, .. } => self.collect_prefix(cond, tk, ts, out, stop),
                Expr::If(i) => self.collect_prefix(&i.cond, tk, ts, out, stop),
                _ => for_each_child(e, &mut |c| self.collect_prefix(c, tk, ts, out, stop)),
            }
            // The target sits in a lazy part we did not descend into.
            *stop = true;
        } else if s.end <= ts.start {
            if self.model.must_evaluate(e) {
                out.push(e);
            }
        } else {
            *stop = true;
        }
    }

    /// The path of ancestors from a statement root down to `target`.
    fn path_to(root: &Expr, target: model::Key) -> Option<Vec<&Expr>> {
        if key(root) == target {
            return Some(vec![root]);
        }
        let span = root.span();
        if !(span.start <= target.0 && target.1 <= span.end) {
            return None;
        }
        let mut found = None;
        for_each_child(root, &mut |c| {
            if found.is_none() {
                found = Self::path_to(c, target);
            }
        });
        found.map(|mut p| {
            p.insert(0, root);
            p
        })
    }

    /// Evaluate `targets` into temporaries placed immediately before the
    /// statement, keeping v5's evaluation order, and replace each with its
    /// temporary. Returns the temporaries' names, or `None` when the
    /// statement first needs restructuring (done now) or the edit collides
    /// (retried next iteration).
    pub(crate) fn hoist(&mut self, site: Site<'_>, targets: &[&Expr]) -> Option<Vec<String>> {
        let stmt = site.stmt;
        let (s_start, s_end) = (stmt.span().start as usize, stmt.span().end as usize);
        let first = targets.first()?;
        // Targets inside a `request.*()` expression run in another context.
        match self.hoist_into_request(site, targets) {
            RequestHoist::NotInRequest => {}
            RequestHoist::Deferred => return None,
            RequestHoist::Done(names) => return Some(names),
        }
        // Lazy constructs between the statement and the target come first,
        // unless evaluating the targets early is harmless.
        let movable = targets.iter().all(|t| trivially_movable(t));
        for root in Self::roots(stmt).into_iter().filter(|_| !movable) {
            if let Some(path) = Self::path_to(root, key(first)) {
                for (i, node) in path.iter().enumerate() {
                    if let Expr::Ternary { cond, .. } = node {
                        let next = path.get(i + 1);
                        if next.is_some_and(|n| key(n) != key(cond)) {
                            self.extract_ternary(site, node);
                            return None;
                        }
                    }
                }
            }
        }
        match site.place {
            Place::InlineFn | Place::InlineArm => {
                self.expand_inline_body(site);
                return None;
            }
            Place::ElseIf => {
                if self.cond_of_stmt_contains(stmt, first) {
                    self.split_else_if(stmt);
                    return None;
                }
            }
            Place::Block => {}
        }
        if let Stmt::While(w) = stmt {
            if contains(&w.cond, first) {
                return self.hoist_while(w, targets);
            }
        }
        let lazy_label = |s: &ast::SwitchStmt| {
            s.subject.is_none()
                && s.arms
                    .iter()
                    .skip(1)
                    .any(|a| a.label.as_ref().is_some_and(|l| contains(l, first)))
        };
        if let Stmt::Switch(s) = stmt {
            if lazy_label(s) {
                self.switch_to_if(stmt, s);
                return None;
            }
        }
        // The same for a `switch` used as a value.
        for root in Self::roots(stmt) {
            if let Some(path) = Self::path_to(root, key(first)) {
                for node in path {
                    if let Expr::Switch(s) = node {
                        if lazy_label(s) {
                            self.switch_to_if(stmt, s);
                            return None;
                        }
                    }
                }
            }
        }
        // Everything that ran before the first target must still run first.
        let mut all: Vec<&Expr> = self.prefix_units(stmt, first);
        for t in targets {
            if !all.iter().any(|a| key(a) == key(t)) {
                all.push(t);
            }
        }
        if all
            .iter()
            .any(|t| !self.free(t.span().start as usize, t.span().end as usize))
            || self.locked(s_start, s_end)
        {
            return None;
        }
        let is_var = matches!(stmt, Stmt::Assign { is_var: true, .. });
        let is_varip = matches!(stmt, Stmt::Assign { is_varip: true, .. });
        let keyword = if is_varip {
            "varip "
        } else if is_var {
            "var "
        } else {
            ""
        };
        let indent = self.indent_of(s_start);
        let mut lines = String::new();
        let mut names = Vec::new();
        for t in &all {
            let name = self.temps.fresh();
            let annotation = if matches!(peel(t), Expr::Na { .. }) {
                "float "
            } else {
                ""
            };
            lines.push_str(&format!(
                "{indent}{keyword}{annotation}{name} = {}\n",
                self.expr_text(t)
            ));
            let span = t.span();
            self.edits
                .replace(span.start as usize, span.end as usize, name.clone());
            if targets.iter().any(|x| key(x) == key(t)) {
                names.push(name);
            }
        }
        self.edits
            .insert_lines(text::line_start(self.src, s_start), lines);
        Some(names)
    }

    /// Declare `name = text` immediately before the statement (for text that
    /// has no node of its own, such as `obj[1]` built from `obj.f[1]`). The
    /// text must be free of history and effects.
    pub(crate) fn hoist_text(&mut self, site: Site<'_>, text: &str) -> Option<String> {
        let stmt = site.stmt;
        let (s_start, s_end) = (stmt.span().start as usize, stmt.span().end as usize);
        match site.place {
            Place::InlineFn | Place::InlineArm => {
                self.expand_inline_body(site);
                return None;
            }
            Place::ElseIf => {
                self.split_else_if(stmt);
                return None;
            }
            Place::Block => {}
        }
        if self.locked(s_start, s_end) || matches!(stmt, Stmt::While(_)) {
            return None;
        }
        let keyword = match stmt {
            Stmt::Assign { is_varip: true, .. } => "varip ",
            Stmt::Assign { is_var: true, .. } => "var ",
            _ => "",
        };
        let name = self.temps.fresh();
        let indent = self.indent_of(s_start);
        let line = format!("{indent}{keyword}{name} = {text}\n");
        self.edits
            .insert_lines(text::line_start(self.src, s_start), line)
            .then_some(name)
    }

    fn cond_of_stmt_contains(&self, stmt: &Stmt, target: &Expr) -> bool {
        match stmt {
            Stmt::If(i) => contains(&i.cond, target),
            _ => false,
        }
    }

    /// Turn a single-line body (`f(x) => expr`, `label => expr`) into block
    /// form so statements can be placed before it.
    pub(crate) fn expand_inline_body(&mut self, site: Site<'_>) {
        let (start, end) = (
            site.stmt.span().start as usize,
            site.stmt.span().end as usize,
        );
        if !self.free(start, end) {
            return;
        }
        let Some(arrow) = self.src[..start].rfind("=>") else {
            return;
        };
        let header = text::line_start(self.src, arrow);
        let indent = format!("{}{}", self.indent_of(header), self.unit);
        if !self.edits.replace(arrow + 2, start, format!("\n{indent}")) {
            return;
        }
        // Continuation lines keep their position relative to the moved line.
        let mut pos = start;
        while let Some(i) = self.src[pos..end].find('\n') {
            let line = pos + i + 1;
            if line >= end {
                break;
            }
            self.edits.insert_lines(line, self.unit);
            pos = line;
        }
        self.lock(start, end);
    }

    /// `else if c` becomes `else` + a nested `if c`, so code can run between
    /// the failed earlier branches and the condition.
    fn split_else_if(&mut self, nested: &Stmt) {
        let (start, end) = (nested.span().start as usize, nested.span().end as usize);
        if !self.free(start, end) {
            return;
        }
        let else_indent = self.indent_of(start);
        // Replace the space between `else` and `if` with a line break.
        let gap = self.src[..start].len() - self.src[..start].trim_end_matches([' ', '\t']).len();
        self.edits
            .replace(start - gap, start, format!("\n{else_indent}{}", self.unit));
        let mut pos = start;
        while let Some(i) = self.src[pos..end].find('\n') {
            let line = pos + i + 1;
            if line >= end {
                break;
            }
            if !self.src[line..text::line_end(self.src, line)]
                .trim()
                .is_empty()
            {
                self.edits.insert_lines(line, self.unit);
            }
            pos = line;
        }
        self.lock(start, end);
    }

    /// `while cond` with a condition that must run in full becomes
    /// `while true` with the condition (and its temporaries) at the top of
    /// the body, so it is still re-evaluated before every iteration.
    fn hoist_while(&mut self, w: &ast::WhileStmt, targets: &[&Expr]) -> Option<Vec<String>> {
        let cond = w.cond.span();
        let (cs, ce) = (cond.start as usize, cond.end as usize);
        if !self.free(cs, ce) || w.body.is_empty() {
            return None;
        }
        let first_body = w.body[0].span().start as usize;
        let body_indent = self.indent_of(first_body);
        let mut rewritten = self.src[cs..ce].to_string();
        let mut lines = String::new();
        let mut names = Vec::new();
        // Replace from the back so earlier offsets stay valid.
        let mut ordered: Vec<&&Expr> = targets.iter().collect();
        ordered.sort_by_key(|t| std::cmp::Reverse(t.span().start));
        let mut decls = Vec::new();
        for t in ordered {
            let name = self.temps.fresh();
            let s = t.span();
            rewritten.replace_range(s.start as usize - cs..s.end as usize - cs, &name);
            decls.push(format!("{body_indent}{name} = {}\n", self.expr_text(t)));
            names.push(name);
        }
        decls.reverse();
        names.reverse();
        for d in decls {
            lines.push_str(&d);
        }
        lines.push_str(&format!(
            "{body_indent}if not ({rewritten})\n{body_indent}{}break\n",
            self.unit
        ));
        self.edits.replace(cs, ce, "true");
        self.edits
            .insert_lines(text::line_start(self.src, first_body), lines);
        self.lock(w.span.start as usize, w.span.end as usize);
        Some(names)
    }

    /// Rewrite a condition-only `switch` as an `if`/`else if` chain, so a
    /// later label's condition can be hoisted into its own `else` branch.
    fn switch_to_if(&mut self, stmt: &Stmt, s: &ast::SwitchStmt) {
        let (start, end) = (s.span.start as usize, s.span.end as usize);
        if !self.free(start, end) {
            return;
        }
        let indent = self.indent_of(match stmt {
            Stmt::Switch(_) => start,
            _ => stmt.span().start as usize,
        });
        let mut out = String::new();
        let mut first = true;
        for arm in &s.arms {
            match &arm.label {
                Some(label) => {
                    if first {
                        out.push_str("if ");
                    } else {
                        out.push_str(&format!("\n{indent}else if "));
                    }
                    out.push_str(self.expr_text(label));
                }
                None => out.push_str(&format!("\n{indent}else")),
            }
            first = false;
            out.push_str(&self.arm_body_as_block(arm, &indent));
        }
        self.edits.replace(start, end, out);
        self.lock(start, end);
    }

    /// An arm's body re-indented as the block of an `if` at `indent`.
    fn arm_body_as_block(&self, arm: &ast::SwitchArm, indent: &str) -> String {
        let Some(first) = arm.body.first() else {
            return String::new();
        };
        let last = arm.body.last().map_or(first.span(), Stmt::span);
        let (bs, be) = (first.span().start as usize, last.end as usize);
        let body_line = text::line_start(self.src, bs);
        let inline = body_line == text::line_start(self.src, arm.span.start as usize);
        let target = format!("{indent}{}", self.unit);
        if inline {
            return format!("\n{target}{}", &self.src[bs..be]);
        }
        let old = self.indent_of(bs);
        let mut out = String::new();
        for line in self.src[body_line..be].split('\n') {
            out.push('\n');
            let trimmed = line.strip_prefix(old.as_str()).map_or_else(
                || line.trim_start().to_string(),
                |rest| format!("{target}{rest}"),
            );
            out.push_str(&trimmed);
        }
        out
    }

    /// Replace a ternary with a temporary holding the equivalent `if`
    /// expression, so code can be placed inside one branch.
    fn extract_ternary(&mut self, site: Site<'_>, ternary: &Expr) {
        let Expr::Ternary {
            cond,
            then_e,
            else_e,
            span,
        } = ternary
        else {
            return;
        };
        let stmt = site.stmt;
        if !matches!(site.place, Place::Block) {
            if matches!(site.place, Place::InlineFn | Place::InlineArm) {
                self.expand_inline_body(site);
            } else if let Place::ElseIf = site.place {
                self.split_else_if(stmt);
            }
            return;
        }
        let prefix = self.prefix_units(stmt, ternary);
        let (ts, te) = (span.start as usize, span.end as usize);
        if !self.free(ts, te)
            || prefix
                .iter()
                .any(|p| !self.free(p.span().start as usize, p.span().end as usize))
        {
            return;
        }
        let s_start = stmt.span().start as usize;
        let indent = self.indent_of(s_start);
        let keyword = match stmt {
            Stmt::Assign { is_varip: true, .. } => "varip ",
            Stmt::Assign { is_var: true, .. } => "var ",
            _ => "",
        };
        let mut lines = String::new();
        for p in prefix {
            let name = self.temps.fresh();
            lines.push_str(&format!(
                "{indent}{keyword}{name} = {}\n",
                self.expr_text(p)
            ));
            self.edits
                .replace(p.span().start as usize, p.span().end as usize, name);
        }
        let name = self.temps.fresh();
        let ty = self.model.ty(ternary);
        let annotation = match ty {
            Ty::Int => "int ",
            Ty::Float => "float ",
            Ty::Bool => "bool ",
            Ty::Str => "string ",
            Ty::Color => "color ",
            _ => "",
        };
        let inner = format!("{indent}{}", self.unit);
        lines.push_str(&format!(
            "{indent}{keyword}{annotation}{name} = if {}\n{inner}{}\n{indent}else\n{inner}{}\n",
            self.expr_text(cond),
            self.expr_text(then_e),
            self.expr_text(else_e)
        ));
        self.edits.replace(ts, te, name);
        self.edits
            .insert_lines(text::line_start(self.src, s_start), lines);
        self.lock(s_start, stmt.span().end as usize);
    }

    /// Hoisting out of a `request.*()` expression argument: the temporaries
    /// must be computed in the requested context, so the expression moves
    /// into a new global function and the call passes that function's call.
    /// Returns `None` when no target is inside a request expression.
    fn hoist_into_request(&mut self, site: Site<'_>, targets: &[&Expr]) -> RequestHoist {
        let Some(first) = targets.first() else {
            return RequestHoist::NotInRequest;
        };
        let mut request_arg: Option<&Expr> = None;
        for root in Self::roots(site.stmt) {
            if let Some(path) = Self::path_to(root, key(first)) {
                for node in &path {
                    if let Expr::Call { callee, args, .. } = node {
                        if matches!(
                            callee.name.as_str(),
                            "request.security" | "request.security_lower_tf"
                        ) {
                            if let Some(arg) = request_expression_arg(args) {
                                if contains(arg, first) && key(arg) != key(first) {
                                    request_arg = Some(arg);
                                }
                            }
                        }
                    }
                }
            }
        }
        let Some(arg) = request_arg else {
            return RequestHoist::NotInRequest;
        };
        let (as_, ae) = (arg.span().start as usize, arg.span().end as usize);
        if !self.free(as_, ae) {
            return RequestHoist::Deferred;
        }
        // The wrapper lives at global scope, so it can only see globals.
        let mut local = false;
        arg_idents(arg, &mut |ident| {
            if self
                .model
                .decl_of(ident)
                .is_some_and(|d| d.kind != model::DeclKind::Global)
            {
                local = true;
            }
        });
        if local {
            self.note(
                "E1",
                RuleClass::Diagnostic,
                Severity::Warning,
                (as_, ae),
                "This `request.*()` expression now evaluates `and`/`or` lazily, and it uses local variables, so the stateful operand cannot be moved into a wrapper function; evaluate it before the condition inside the expression by hand.",
                false,
            );
            return RequestHoist::Deferred;
        }
        let name = self.temps.fresh();
        let mut body = self.src[as_..ae].to_string();
        let mut sorted: Vec<&&Expr> = targets.iter().collect();
        sorted.sort_by_key(|t| std::cmp::Reverse(t.span().start));
        let mut decls = Vec::new();
        let mut names = Vec::new();
        for t in sorted {
            let temp = self.temps.fresh();
            let s = t.span();
            body.replace_range(s.start as usize - as_..s.end as usize - as_, &temp);
            decls.push(format!("{}{temp} = {}\n", self.unit, self.expr_text(t)));
            names.push(temp);
        }
        decls.reverse();
        names.reverse();
        let mut def = format!("{name}() =>\n");
        for d in decls {
            def.push_str(&d);
        }
        def.push_str(&format!("{}{}\n", self.unit, body));
        self.edits.replace(as_, ae, format!("{name}()"));
        self.edits.insert_lines(site.top_line, def);
        self.lock(as_, ae);
        RequestHoist::Done(names)
    }

    fn declaration(&mut self, kind: &str, args: &[ast::CallArg], span: pinewasm_span::Span) {
        if self.phase == Phase::Calls {
            self.calls_declaration(kind, args, span);
        }
    }

    fn type_def(&mut self, t: &ast::TypeDecl) {
        if self.phase == Phase::Types {
            self.types_udt(t);
        }
    }
}

/// What hoisting into a request expression did.
enum RequestHoist {
    /// No target is inside a request expression.
    NotInRequest,
    /// The rewrite could not be made now (or at all; a finding says which).
    Deferred,
    /// The expression moved into a wrapper; these are the temporaries.
    Done(Vec<String>),
}

/// The `expression` argument of a request call.
pub(crate) fn request_expression_arg(args: &[ast::CallArg]) -> Option<&Expr> {
    args.iter()
        .find(|a| a.name.as_ref().is_some_and(|n| n.name == "expression"))
        .or_else(|| args.iter().take_while(|a| a.name.is_none()).nth(2))
        .map(|a| &a.value)
}

/// Expressions that can be evaluated earlier than written without any
/// observable difference: they have no history, no effects, and cannot
/// fail at runtime.
pub(crate) fn trivially_movable(expr: &Expr) -> bool {
    match peel(expr) {
        Expr::Int { .. }
        | Expr::Float { .. }
        | Expr::Bool { .. }
        | Expr::Str { .. }
        | Expr::Color { .. }
        | Expr::Na { .. }
        | Expr::Ident(_) => true,
        Expr::History { target, offset, .. } => {
            matches!(peel(target), Expr::Ident(_)) && matches!(peel(offset), Expr::Int { .. })
        }
        _ => false,
    }
}

pub(crate) fn contains(outer: &Expr, inner: &Expr) -> bool {
    let (o, i) = (outer.span(), inner.span());
    o.start <= i.start && i.end <= o.end
}

fn arg_idents(expr: &Expr, f: &mut dyn FnMut(&ast::Ident)) {
    if let Expr::Ident(i) = expr {
        f(i);
    }
    for_each_child(expr, &mut |c| arg_idents(c, f));
}

// -- driver ------------------------------------------------------------------------

struct State {
    text: String,
    offsets: OffsetMap,
    findings: Vec<Finding>,
    temps: TempNames,
    helpers: BTreeSet<Helper>,
}

fn parse_errors(text: &str) -> Vec<String> {
    error_lines(text, pinewasm_parse::parse(text).diagnostics)
}

/// The parser's errors for `text`, each with the line it is on.
///
/// An error with no label is about the text as a whole (it is too large to
/// read, or it has more errors than are reported), so it names no line. The
/// parser reports a bounded list, which bounds this one.
fn error_lines(text: &str, diagnostics: Vec<pinewasm_span::Diagnostic>) -> Vec<String> {
    diagnostics
        .into_iter()
        .filter(|d| d.severity == pinewasm_span::Severity::Error)
        .map(|d| match d.labels.first() {
            Some(label) => {
                let at = (label.span.start as usize).min(text.len());
                let line = text[..at].matches('\n').count() + 1;
                format!("line {line}: {}", d.message)
            }
            None => d.message,
        })
        .collect()
}

/// The parser's reason for not reading a text at all, when its diagnostics
/// hold one: the text is over the source size cap or the token budget
/// (`E0001`).
///
/// A rewrite only ever starts from text the parser read, so for rewritten
/// text this is the migration outgrowing the parser, not a rewrite gone
/// wrong. There is nothing to salvage from it: the phases that follow could
/// not read the text either, and a script migrated halfway is worse than one
/// left alone.
fn outgrows_the_parser(diagnostics: &[pinewasm_span::Diagnostic]) -> Option<&str> {
    diagnostics
        .iter()
        .find(|d| d.code.as_deref() == Some(pinewasm_span::codes::E0001))
        .map(|d| d.message.as_str())
}

/// The result for a script whose v6 text the parser would not read: the
/// source, unchanged, and why.
fn outgrown(source: &str, reason: &str) -> Migration {
    Migration {
        status: Status::ParseError,
        source_version: Some(5),
        output: source.to_string(),
        edits: Vec::new(),
        findings: Vec::new(),
        parse_errors: vec![format!(
            "the migrated script would be too large to parse ({reason})"
        )],
    }
}

pub(crate) fn run(
    source: &str,
    options: &MigrationOptions,
    range: Option<(u32, u32)>,
) -> Migration {
    let unchanged = |status, version, parse_errors| Migration {
        status,
        source_version: version,
        output: source.to_string(),
        edits: Vec::new(),
        findings: Vec::new(),
        parse_errors,
    };
    let errors = parse_errors(source);
    let parsed = pinewasm_parse::parse(source);
    let version = parsed.program.version.map(|v| v.version);
    if !errors.is_empty() {
        return unchanged(Status::ParseError, version, errors);
    }
    match version {
        Some(5) => {}
        Some(6) => return unchanged(Status::AlreadyV6, version, Vec::new()),
        _ => return unchanged(Status::UnsupportedVersion, version, Vec::new()),
    }
    let mut state = State {
        text: source.to_string(),
        offsets: OffsetMap::default(),
        findings: Vec::new(),
        temps: TempNames::new(source),
        helpers: BTreeSet::new(),
    };
    // Diagnostics about behavior no rewrite can keep are judged on the
    // original script.
    {
        let model = Model::build(&parsed.program);
        let mut notes = Vec::new();
        detect::run(source, &model, options, &mut notes);
        for n in notes {
            push_finding(&mut state, &n, range);
        }
    }
    for phase in PHASES {
        if phase == Phase::Modernize && !options.modernize {
            continue;
        }
        for iteration in 0..MAX_ITERATIONS {
            let parsed = pinewasm_parse::parse(&state.text);
            let model = Model::build(&parsed.program);
            let text_now = state.text.clone();
            let mut cx = Cx {
                src: &text_now,
                model: &model,
                edits: EditSet::default(),
                notes: Vec::new(),
                opts: options,
                unit: text::indent_unit(&text_now),
                temps: &mut state.temps,
                helpers: &mut state.helpers,
                offsets: &state.offsets,
                range,
                phase,
                iteration,
                locked: Vec::new(),
            };
            cx.walk();
            let Cx { edits, notes, .. } = cx;
            if std::env::var_os("PINEWASM_MIGRATE_TRACE").is_some() {
                eprintln!(
                    "migrate: {phase:?} iteration {iteration}: {} notes, edits: {}",
                    notes.len(),
                    !edits.is_empty()
                );
            }
            if edits.is_empty() {
                for n in &notes {
                    push_finding(&mut state, n, range);
                }
                break;
            }
            let (next, map) = edits.apply(&text_now);
            let diagnostics = pinewasm_parse::parse(&next).diagnostics;
            if let Some(reason) = outgrows_the_parser(&diagnostics) {
                return outgrown(source, reason);
            }
            let broken = error_lines(&next, diagnostics);
            if !broken.is_empty() {
                // A rewrite produced text the parser rejects. Keep the last
                // valid text, report the failure, and move on to the next
                // phase rather than compounding it.
                if let Some(path) = std::env::var_os("PINEWASM_MIGRATE_DUMP") {
                    let _ = std::fs::write(path, &next);
                }
                state.findings.push(Finding {
                    rule: "internal",
                    class: RuleClass::Diagnostic,
                    severity: Severity::Error,
                    message: format!(
                        "a {phase:?}-phase rewrite produced source that does not parse ({}); those rewrites were skipped, so this part of the script needs manual migration",
                        broken.join("; ")
                    ),
                    start: 0,
                    end: 0,
                    applied: false,
                });
                break;
            }
            for n in &notes {
                push_finding(&mut state, n, range);
            }
            state.text = next;
            state.offsets.push(map);
        }
    }
    finish(source, state, range)
}

/// Empty spans/selections cover the byte at their start. Widen before
/// extending that cursor so `u32::MAX + 1` remains representable.
fn ranges_overlap(start: u32, end: u32, (lo, hi): (u32, u32)) -> bool {
    let start = u64::from(start);
    let end = u64::from(end);
    let lo = u64::from(lo);
    let hi = u64::from(hi);
    start < hi.max(lo + 1) && lo < end.max(start + 1)
}

fn push_finding(state: &mut State, n: &Note, range: Option<(u32, u32)>) {
    let start = state.offsets.to_original(n.start) as u32;
    let end = (state.offsets.to_original(n.end) as u32).max(start);
    if let Some(range) = range {
        if !ranges_overlap(start, end, range) {
            return;
        }
    }
    let finding = Finding {
        rule: n.rule,
        class: n.class,
        severity: n.severity,
        message: n.message.clone(),
        start,
        end,
        applied: n.applied,
    };
    if !state.findings.contains(&finding) {
        state.findings.push(finding);
    }
}

fn finish(source: &str, mut state: State, range: Option<(u32, u32)>) -> Migration {
    // Helper functions go right after the declaration statement.
    if !state.helpers.is_empty() {
        let unit = text::indent_unit(&state.text);
        let parsed = pinewasm_parse::parse(&state.text);
        let anchor = parsed
            .program
            .items
            .iter()
            .find(|i| {
                matches!(
                    i,
                    ast::Item::Indicator(_) | ast::Item::Strategy(_) | ast::Item::Library(_)
                )
            })
            .map_or(0, |i| {
                let end = i.span().end as usize;
                (text::line_end(&state.text, end) + 1).min(state.text.len())
            });
        let mut defs = String::new();
        for h in &state.helpers {
            defs.push_str(&h.definition(unit));
        }
        let mut edits = EditSet::default();
        let needs_newline = anchor == state.text.len() && !state.text.ends_with('\n');
        edits.insert_lines(
            anchor,
            if needs_newline {
                format!("\n{defs}")
            } else {
                defs
            },
        );
        let (next, map) = edits.apply(&state.text);
        if let Some(reason) = outgrows_the_parser(&pinewasm_parse::parse(&next).diagnostics) {
            return outgrown(source, reason);
        }
        state.text = next;
        state.offsets.push(map);
    }
    if range.is_none() {
        let original = pinewasm_parse::parse(source).program.version;
        if let (Some(v), Some(o)) = (pinewasm_parse::parse(&state.text).program.version, original) {
            let (s, e) = (v.span.start as usize, v.span.end as usize);
            let mut edits = EditSet::default();
            edits.replace(s, e, "//@version=6");
            let (next, _) = edits.apply(&state.text);
            state.text = next;
            state.findings.push(Finding {
                rule: "A1",
                class: RuleClass::Exact,
                severity: Severity::Info,
                message: "Declared the script as version 6.".to_string(),
                start: o.span.start,
                end: o.span.end,
                applied: true,
            });
        }
    }
    state.findings.sort_by_key(|f| (f.start, f.end, f.rule));
    let edits = text::line_diff(source, &state.text)
        .map(|(start, end, new_text)| TextEdit {
            start: start as u32,
            end: end as u32,
            new_text,
        })
        .into_iter()
        .collect();
    Migration {
        status: Status::Migrated,
        source_version: Some(5),
        output: state.text,
        edits,
        findings: state.findings,
        parse_errors: Vec::new(),
    }
}

#[cfg(test)]
mod range_tests {
    use super::ranges_overlap;

    #[test]
    fn cursor_overlap_is_defined_at_the_largest_offset() {
        let max = u32::MAX;
        assert!(ranges_overlap(max, max, (max, max)));
        assert!(ranges_overlap(max, max, (max, 0)));
        assert!(ranges_overlap(max - 1, max, (max - 1, max)));
        assert!(!ranges_overlap(0, max, (max, max)));
        assert!(!ranges_overlap(max, max, (0, max)));
        assert!(!ranges_overlap(max - 1, max, (max, max)));
    }
}
