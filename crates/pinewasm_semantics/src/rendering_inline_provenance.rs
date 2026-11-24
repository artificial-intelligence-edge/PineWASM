//! Analyzer-only, per-invocation rendering metadata. Executable expressions,
//! symbols, source spans and serialized HIR/CSIL are never replaced.
use crate::*;

#[derive(Default)]
pub(super) struct InlineProvenance {
    frames: Vec<Frame>,
    last: Option<(HirExpr, HirExpr)>,
    pub calls: Vec<InlineCall>,
    attachments: HashMap<Anchor, Vec<(HirExpr, HirExpr)>>,
    statements: HashMap<Anchor, Vec<(HirStmt, HirStmt)>>,
}
#[derive(Default)]
struct Frame {
    children: Vec<(HirExpr, HirExpr)>,
    result: Option<HirExpr>,
}
pub(super) struct InlineCall {
    pub parameters: Vec<SymbolId>,
    pub result: Option<HirExpr>,
}
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Anchor {
    Assignment(SymbolId, Span),
    Plot(PlotSlot, PlotColorRole),
    Condition(Span),
}

fn same(left: &HirExpr, right: &HirExpr) -> bool {
    left.kind == right.kind && left.span == right.span && left.ty == right.ty
}

/// Compose only metadata-bearing child occurrences, in evaluation order. A
/// helper returning an identical literal still has its own metadata graph node.
fn compose(expr: &HirExpr, children: &[(HirExpr, HirExpr)]) -> HirExpr {
    compose_occurrence(expr, children, &mut vec![false; children.len()])
}

fn compose_occurrence(
    expr: &HirExpr,
    children: &[(HirExpr, HirExpr)],
    used: &mut [bool],
) -> HirExpr {
    fn replace(expr: &HirExpr, children: &[(HirExpr, HirExpr)], used: &mut [bool]) -> HirExpr {
        if let Some((index, (_, shadow))) = children
            .iter()
            .enumerate()
            .find(|(index, (real, _))| !used[*index] && same(real, expr))
        {
            used[index] = true;
            return shadow.clone();
        }
        let mut result = expr.clone();
        match &mut result.kind {
            HirExprKind::Unary { operand, .. } | HirExprKind::IntToFloat(operand) => {
                **operand = replace(operand, children, used);
            }
            HirExprKind::Binary { lhs, rhs, .. } => {
                **lhs = replace(lhs, children, used);
                **rhs = replace(rhs, children, used);
            }
            HirExprKind::Ternary {
                cond,
                then_e,
                else_e,
            } => {
                **cond = replace(cond, children, used);
                **then_e = replace(then_e, children, used);
                **else_e = replace(else_e, children, used);
            }
            HirExprKind::CallPure { args, .. } | HirExprKind::Tuple { elements: args } => {
                for arg in args {
                    *arg = replace(arg, children, used);
                }
            }
            HirExprKind::MaterializeHistory { expr, .. }
            | HirExprKind::MaterializeHistoryDynamic { expr, .. } => {
                **expr = replace(expr, children, used);
            }
            _ => {}
        }
        result
    }
    replace(expr, children, used)
}

fn statement_anchor(stmt: &HirStmt) -> Option<Anchor> {
    match stmt {
        HirStmt::Assign { symbol, span, .. } => Some(Anchor::Assignment(*symbol, *span)),
        HirStmt::If { span, .. } | HirStmt::For { span, .. } | HirStmt::While { span, .. } => {
            Some(Anchor::Condition(*span))
        }
        _ => None,
    }
}

fn compose_statement(
    stmt: &HirStmt,
    children: &[(HirExpr, HirExpr)],
    used: &mut [bool],
) -> HirStmt {
    let mut result = stmt.clone();
    match &mut result {
        HirStmt::Assign { value, .. } => {
            *value = compose_occurrence(value, children, used);
        }
        HirStmt::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            *cond = compose_occurrence(cond, children, used);
            for child in then_branch.iter_mut().chain(else_branch) {
                *child = compose_statement(child, children, used);
            }
        }
        HirStmt::For { body, .. } | HirStmt::While { body, .. } => {
            for child in body {
                *child = compose_statement(child, children, used);
            }
        }
        _ => {}
    }
    result
}

fn unknown_statement(stmt: &HirStmt) -> HirStmt {
    fn unknown(expr: &mut HirExpr) {
        expr.kind = HirExprKind::ReadVar {
            symbol: SymbolId(u32::MAX),
            offset: 0,
        };
    }
    let mut result = stmt.clone();
    match &mut result {
        HirStmt::Assign { value, .. } => unknown(value),
        HirStmt::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            unknown(cond);
            for child in then_branch.iter_mut().chain(else_branch) {
                *child = unknown_statement(child);
            }
        }
        HirStmt::For { body, .. } | HirStmt::While { body, .. } => {
            for child in body {
                *child = unknown_statement(child);
            }
        }
        _ => {}
    }
    result
}

impl InlineProvenance {
    pub fn begin(&mut self) {
        self.frames.push(Frame::default());
    }
    pub fn finish(&mut self, real: &HirExpr, pending: &[HirStmt]) {
        let frame = self.frames.pop().expect("expression metadata frame");
        if !frame.children.is_empty() {
            self.pending(
                pending,
                &frame.children,
                &mut vec![false; frame.children.len()],
            );
        }
        let shadow = frame
            .result
            .or_else(|| (!frame.children.is_empty()).then(|| compose(real, &frame.children)));
        self.last = shadow.filter(|shadow| !same(real, shadow)).map(|shadow| {
            if let Some(parent) = self.frames.last_mut() {
                parent.children.push((real.clone(), shadow.clone()));
            }
            (real.clone(), shadow)
        });
    }
    fn pending(&mut self, body: &[HirStmt], children: &[(HirExpr, HirExpr)], used: &mut [bool]) {
        for stmt in body {
            let Some(anchor) = statement_anchor(stmt) else {
                continue;
            };
            let shadow = compose_statement(stmt, children, used);
            if shadow != *stmt {
                let entries = self.statements.entry(anchor).or_default();
                if !entries
                    .iter()
                    .any(|(real, saved)| real == stmt && saved == &shadow)
                {
                    entries.push((stmt.clone(), shadow));
                }
            }
        }
    }
    /// Generated branch result stores can share a symbol, span and literal.
    /// Preserve their positions inside the internal statement tree instead of
    /// merging their expression attachments at that shared assignment key.
    pub fn statement(&self, real: &HirStmt) -> Option<HirStmt> {
        let entries = self.statements.get(&statement_anchor(real)?)?;
        let mut matching = entries.iter().filter(|(value, _)| value == real);
        let (_, first) = matching.next()?;
        if matching.any(|(_, next)| next != first) {
            return Some(unknown_statement(real));
        }
        Some(first.clone())
    }
    pub fn last(&self, real: &HirExpr) -> Option<HirExpr> {
        self.last.as_ref().and_then(|(value, shadow)| {
            if same(value, real) {
                Some(shadow.clone())
            } else {
                let composed = compose(real, &[(value.clone(), shadow.clone())]);
                (!same(real, &composed)).then_some(composed)
            }
        })
    }
    pub fn call(&mut self, real: &HirExpr, parameters: Vec<SymbolId>) {
        let result = self.last(real).unwrap_or_else(|| real.clone());
        // These IDs exist only in the internal metadata graph; no Symbol is allocated.
        // The first reserved node is an explicit ambiguous-attachment failure.
        if self.calls.is_empty() {
            self.calls.push(InlineCall {
                parameters: Vec::new(),
                result: None,
            });
        }
        let index = self.calls.len();
        self.calls.push(InlineCall {
            parameters,
            result: Some(result),
        });
        let shadow = HirExpr {
            kind: HirExprKind::ReadVar {
                symbol: SymbolId(u32::MAX - index as u32),
                offset: 0,
            },
            ..real.clone()
        };
        if let Some(frame) = self.frames.last_mut() {
            frame.result = Some(shadow);
        }
    }
    pub fn get_call(&self, symbol: SymbolId) -> Option<&InlineCall> {
        self.calls.get((u32::MAX - symbol.0) as usize)
    }
    pub fn attach(&mut self, anchor: Anchor, real: &HirExpr, shadow: Option<HirExpr>) {
        if let Some(shadow) = shadow {
            let entries = self.attachments.entry(anchor).or_default();
            if !entries
                .iter()
                .any(|(value, saved)| same(value, real) && same(saved, &shadow))
            {
                entries.push((real.clone(), shadow));
            }
        }
    }
    pub fn expression(&self, anchor: Anchor, real: &HirExpr) -> HirExpr {
        let Some(entries) = self.attachments.get(&anchor) else {
            return real.clone();
        };
        let mut matching = entries.iter().filter(|(value, _)| same(value, real));
        let Some((_, first)) = matching.next() else {
            return real.clone();
        };
        if matching.any(|(_, next)| !same(first, next)) {
            // Never union qualifications from indistinguishable attachments.
            return HirExpr {
                kind: HirExprKind::ReadVar {
                    symbol: SymbolId(u32::MAX),
                    offset: 0,
                },
                ..real.clone()
            };
        }
        first.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn literal() -> HirExpr {
        HirExpr {
            kind: HirExprKind::ConstBool(true),
            ty: Type::Bool,
            qualifier: Qualifier::Const,
            span: Span::dummy(),
        }
    }

    fn read(symbol: u32) -> HirExpr {
        HirExpr {
            kind: HirExprKind::ReadVar {
                symbol: SymbolId(symbol),
                offset: 0,
            },
            qualifier: Qualifier::Series,
            ..literal()
        }
    }

    fn invocation(
        book: &mut InlineProvenance,
        real: &HirExpr,
        parameters: Vec<SymbolId>,
    ) -> HirExpr {
        book.begin();
        // Lowering the actual body tail clears an earlier invocation's `last`.
        book.begin();
        book.finish(real, &[]);
        book.call(real, parameters);
        book.finish(real, &[]);
        book.last(real).expect("internal call shadow")
    }

    fn symbol(expr: &HirExpr) -> SymbolId {
        let HirExprKind::ReadVar { symbol, offset: 0 } = expr.kind else {
            panic!("internal read node expected")
        };
        symbol
    }

    fn assignment(target: u32, value: HirExpr) -> HirStmt {
        HirStmt::Assign {
            symbol: SymbolId(target),
            value,
            init: false,
            span: Span::dummy(),
        }
    }

    fn assigned(stmt: &HirStmt) -> &HirExpr {
        let HirStmt::Assign { value, .. } = stmt else {
            panic!("assignment expected")
        };
        value
    }

    #[test]
    fn identical_body_occurrences_keep_distinct_argument_roots() {
        let mut book = InlineProvenance::default();
        let real = literal();
        let original = serde_json::to_vec(&real).unwrap();
        let first = invocation(&mut book, &real, vec![SymbolId(11)]);
        let second = invocation(&mut book, &real, vec![SymbolId(12)]);
        assert_ne!(symbol(&first), symbol(&second));
        assert_eq!(
            book.get_call(symbol(&first)).unwrap().parameters,
            [SymbolId(11)]
        );
        assert_eq!(
            book.get_call(symbol(&second)).unwrap().parameters,
            [SymbolId(12)]
        );
        let pair = HirExpr {
            kind: HirExprKind::Binary {
                op: BinOp::And,
                lhs: Box::new(real.clone()),
                rhs: Box::new(real.clone()),
            },
            ..real.clone()
        };
        let shadow = compose(
            &pair,
            &[
                (real.clone(), first.clone()),
                (real.clone(), second.clone()),
            ],
        );
        let HirExprKind::Binary { lhs, rhs, .. } = shadow.kind else {
            panic!("binary expected")
        };
        assert_eq!(*lhs, first);
        assert_eq!(*rhs, second);
        assert_eq!(serde_json::to_vec(&real).unwrap(), original);
    }

    #[test]
    fn argument_temp_and_later_generated_store_consume_separate_occurrences() {
        let mut book = InlineProvenance::default();
        let real = literal();
        book.begin();
        let argument = invocation(&mut book, &real, vec![SymbolId(11)]);
        let result = invocation(&mut book, &real, vec![SymbolId(12)]);
        let anchor = Anchor::Assignment(SymbolId(21), Span::dummy());
        book.attach(anchor, &real, Some(argument.clone()));
        let pending = vec![assignment(21, real.clone()), assignment(22, real.clone())];
        let original = serde_json::to_vec(&pending).unwrap();
        book.finish(&read(22), &pending);
        assert_eq!(book.expression(anchor, &real), argument);
        assert_eq!(*assigned(&book.statement(&pending[0]).unwrap()), argument);
        assert_eq!(*assigned(&book.statement(&pending[1]).unwrap()), result);
        assert_eq!(serde_json::to_vec(&pending).unwrap(), original);
    }

    #[test]
    fn generated_branch_stores_keep_distinct_shadows_at_one_assignment_anchor() {
        let mut book = InlineProvenance::default();
        let real = literal();
        book.begin();
        let first = invocation(&mut book, &real, vec![SymbolId(11)]);
        let second = invocation(&mut book, &real, vec![SymbolId(12)]);
        let actual = HirStmt::If {
            cond: read(10),
            then_branch: vec![assignment(22, real.clone())],
            else_branch: vec![assignment(22, real.clone())],
            span: Span::dummy(),
        };
        let original = serde_json::to_vec(&actual).unwrap();
        book.finish(&read(22), std::slice::from_ref(&actual));
        let HirStmt::If {
            then_branch,
            else_branch,
            ..
        } = book.statement(&actual).unwrap()
        else {
            panic!("if expected")
        };
        assert_eq!(*assigned(&then_branch[0]), first);
        assert_eq!(*assigned(&else_branch[0]), second);
        assert_eq!(serde_json::to_vec(&actual).unwrap(), original);
    }

    #[test]
    fn enclosing_call_uses_the_just_lowered_tail_not_first_equal_child() {
        let mut book = InlineProvenance::default();
        let real = literal();
        book.begin();
        let first = invocation(&mut book, &real, vec![SymbolId(11)]);
        let tail = invocation(&mut book, &real, vec![SymbolId(12)]);
        book.call(&real, vec![SymbolId(13)]);
        book.finish(&real, &[]);
        let outer = book.last(&real).unwrap();
        let result = book
            .get_call(symbol(&outer))
            .unwrap()
            .result
            .as_ref()
            .unwrap();
        assert_eq!(result, &tail);
        assert_ne!(result, &first);
    }

    #[test]
    fn repeated_zero_argument_condition_attachments_fail_closed_without_union() {
        let mut book = InlineProvenance::default();
        let real = literal();
        let first = invocation(&mut book, &real, vec![]);
        let second = invocation(&mut book, &real, vec![]);
        let anchor = Anchor::Condition(Span::dummy());
        book.attach(anchor, &real, Some(first.clone()));
        book.attach(anchor, &real, Some(first));
        assert_eq!(book.attachments[&anchor].len(), 1);
        book.attach(anchor, &real, Some(second));
        let result = book.expression(anchor, &real);
        assert_eq!(symbol(&result), SymbolId(u32::MAX));
        assert!(book.get_call(symbol(&result)).unwrap().result.is_none());
    }

    #[test]
    fn indistinguishable_generated_statement_trees_fail_closed_without_union() {
        let mut book = InlineProvenance::default();
        let real = literal();
        let actual = HirStmt::If {
            cond: read(10),
            then_branch: vec![assignment(22, real.clone())],
            else_branch: vec![],
            span: Span::dummy(),
        };
        let original = serde_json::to_vec(&actual).unwrap();
        for _ in 0..2 {
            book.begin();
            invocation(&mut book, &real, vec![]);
            book.finish(&read(22), std::slice::from_ref(&actual));
        }
        let HirStmt::If {
            cond, then_branch, ..
        } = book.statement(&actual).unwrap()
        else {
            panic!("if expected")
        };
        assert_eq!(symbol(&cond), SymbolId(u32::MAX));
        assert_eq!(symbol(assigned(&then_branch[0])), SymbolId(u32::MAX));
        assert_eq!(serde_json::to_vec(&actual).unwrap(), original);
    }
}
