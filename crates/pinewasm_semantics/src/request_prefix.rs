//! Reconstruct bounded pure helper bodies on the requested time axis.
//!
//! This is a compile-time expansion into the existing, validated HTF expression
//! format, not a second interpreter. Persistent state, effects, unknown loop
//! bounds and excessive expansion remain unsupported.

use std::collections::{HashMap, HashSet};

use pinewasm_hir::builtins::{CollectionFunc, PureFunc};
use pinewasm_hir::{BinOp, HirExpr, HirExprKind, HirStmt, SymbolId, Type, UnOp};

use crate::Analyzer;

const MAX_ITERATIONS: usize = 256;
const MAX_EXPANSION: usize = 32_768;
const MAX_DEPTH: usize = 96;

struct Expansion {
    values: HashMap<SymbolId, HirExpr>,
    iterations: usize,
    remaining: usize,
    mutable: HashSet<SymbolId>,
}

impl Expansion {
    fn expression(&mut self, expr: &HirExpr, depth: usize) -> Option<HirExpr> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.remaining = self.remaining.checked_sub(1)?;
        let mut out = expr.clone();
        out.kind = match &expr.kind {
            HirExprKind::ReadVar { symbol, offset: 0 } => {
                let value = self.values.get(symbol).unwrap_or(expr);
                // Count before cloning: repeated `x := x + x` must not build
                // an exponential tree or overflow the stack before admission.
                let mut pending = vec![(value, depth)];
                let mut count = 0usize;
                while let Some((node, level)) = pending.pop() {
                    count = count.checked_add(1)?;
                    if count > self.remaining || level > MAX_DEPTH {
                        return None;
                    }
                    match &node.kind {
                        HirExprKind::Binary { lhs, rhs, .. } => {
                            pending.push((lhs, level + 1));
                            pending.push((rhs, level + 1));
                        }
                        HirExprKind::IntToFloat(inner)
                        | HirExprKind::Unary { operand: inner, .. } => {
                            pending.push((inner, level + 1));
                        }
                        HirExprKind::CallCollection { args, .. } => {
                            pending.extend(args.iter().map(|x| (x, level + 1)));
                        }
                        _ => {}
                    }
                }
                self.remaining -= count;
                return Some(value.clone());
            }
            HirExprKind::ReadVar { symbol, .. } => {
                // Loop-local mutable series history cannot be reconstructed
                // by substituting its current iteration's accumulator.
                return (!self.mutable.contains(symbol)).then_some(out);
            }
            HirExprKind::ReadBuiltin { .. }
            | HirExprKind::ReadInput { .. }
            | HirExprKind::ConstInt(_)
            | HirExprKind::ConstFloat(_)
            | HirExprKind::ConstBool(_)
            | HirExprKind::ConstNa => return Some(out),
            HirExprKind::IntToFloat(inner) => {
                let inner = self.expression(inner, depth + 1)?;
                match number(&inner) {
                    Some(n) => HirExprKind::ConstFloat(n),
                    None => HirExprKind::IntToFloat(Box::new(inner)),
                }
            }
            HirExprKind::Unary {
                op: UnOp::Neg,
                operand,
            } => {
                let operand = self.expression(operand, depth + 1)?;
                match &operand.kind {
                    HirExprKind::ConstInt(n) => HirExprKind::ConstInt(n.checked_neg()?),
                    HirExprKind::ConstFloat(n) => HirExprKind::ConstFloat(-n),
                    _ => HirExprKind::Unary {
                        op: UnOp::Neg,
                        operand: Box::new(operand),
                    },
                }
            }
            HirExprKind::Binary { op, lhs, rhs } => {
                let lhs = self.expression(lhs, depth + 1)?;
                let rhs = self.expression(rhs, depth + 1)?;
                if let Some(value) = fold_binary(*op, &lhs, &rhs) {
                    return Some(HirExpr { kind: value, ..out });
                }
                HirExprKind::Binary {
                    op: *op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                }
            }
            HirExprKind::ReadVarDynamic { symbol, offset } => {
                if self.mutable.contains(symbol) {
                    return None;
                }
                let offset = self.expression(offset, depth + 1)?;
                HirExprKind::ReadVar {
                    symbol: *symbol,
                    offset: u32::try_from(integer(&offset)?).ok()?,
                }
            }
            HirExprKind::ReadBuiltinDynamic { series, offset } => {
                let offset = self.expression(offset, depth + 1)?;
                HirExprKind::ReadBuiltin {
                    series: *series,
                    offset: u32::try_from(integer(&offset)?).ok()?,
                }
            }
            HirExprKind::CallPure {
                func: PureFunc::MathPow,
                args,
            } if args.len() == 2 => {
                let a = self.expression(&args[0], depth + 1)?;
                let b = self.expression(&args[1], depth + 1)?;
                let value = pinewasm_num::pure_pow(number(&a)?, number(&b)?);
                if !value.is_finite() {
                    return None;
                }
                HirExprKind::ConstFloat(value)
            }
            HirExprKind::CallCollection {
                func: CollectionFunc::ArrayFrom,
                args,
            } => {
                let args = args
                    .iter()
                    .map(|e| self.expression(e, depth + 1))
                    .collect::<Option<Vec<_>>>()?;
                // Cardinality is observable without retaining an allocation only
                // when evaluating its elements cannot run state or effects.
                if !args.iter().all(cardinality_element) {
                    return None;
                }
                HirExprKind::CallCollection {
                    func: CollectionFunc::ArrayFrom,
                    args,
                }
            }
            HirExprKind::CallCollection {
                func: CollectionFunc::ArraySize,
                args,
            } if args.len() == 1 => {
                let array = self.expression(&args[0], depth + 1)?;
                let HirExprKind::CallCollection {
                    func: CollectionFunc::ArrayFrom,
                    args,
                } = array.kind
                else {
                    return None;
                };
                HirExprKind::ConstInt(i64::try_from(args.len()).ok()?)
            }
            _ => return None,
        };
        Some(out)
    }

    fn statements(&mut self, analyzer: &Analyzer<'_>, body: &[HirStmt]) -> Option<()> {
        for stmt in body {
            match stmt {
                HirStmt::Assign { symbol, value, .. } => {
                    let declaration = analyzer.symbols.get(symbol.0 as usize)?;
                    if declaration.persistent || declaration.varip {
                        return None;
                    }
                    let value = self.expression(value, 0)?;
                    self.values.insert(*symbol, value);
                }
                HirStmt::Eval { expr, .. } => {
                    self.expression(expr, 0)?;
                }
                HirStmt::For {
                    counter,
                    start,
                    end,
                    step,
                    body,
                    is_int: true,
                    implicit_step,
                    ..
                } => {
                    let start = self.expression(start, 0)?;
                    let mut i = integer(&start)?;
                    let first_end = integer(&self.expression(end, 0)?)?;
                    let raw_step = integer(&self.expression(step, 0)?)?;
                    let step = if *implicit_step && i > first_end {
                        raw_step.checked_neg()?
                    } else {
                        raw_step
                    };
                    if step == 0 {
                        return None;
                    }
                    loop {
                        let bound = integer(&self.expression(end, 0)?)?;
                        if (step > 0 && i > bound) || (step < 0 && i < bound) {
                            break;
                        }
                        self.iterations = self.iterations.checked_add(1)?;
                        if self.iterations > MAX_ITERATIONS {
                            return None;
                        }
                        self.values.insert(
                            *counter,
                            HirExpr {
                                kind: HirExprKind::ConstInt(i),
                                ..start.clone()
                            },
                        );
                        self.statements(analyzer, body)?;
                        // Mutating a loop counter is outside this bounded subset.
                        if integer(self.values.get(counter)?)? != i {
                            return None;
                        }
                        i = i.checked_add(step)?;
                    }
                }
                _ => return None,
            }
        }
        Some(())
    }
}

fn cardinality_element(expr: &HirExpr) -> bool {
    matches!(
        expr.kind,
        HirExprKind::ConstInt(_)
            | HirExprKind::ConstFloat(_)
            | HirExprKind::ConstBool(_)
            | HirExprKind::ConstNa
            | HirExprKind::ReadBuiltin { .. }
            | HirExprKind::ReadInput { .. }
    )
}

fn number(expr: &HirExpr) -> Option<f64> {
    match expr.kind {
        HirExprKind::ConstInt(n) => Some(n as f64),
        HirExprKind::ConstFloat(n) => Some(n),
        _ => None,
    }
}

fn integer(expr: &HirExpr) -> Option<i64> {
    match expr.kind {
        HirExprKind::ConstInt(n) => Some(n),
        _ => None,
    }
}

fn fold_binary(op: BinOp, lhs: &HirExpr, rhs: &HirExpr) -> Option<HirExprKind> {
    if let (Some(a), Some(b)) = (integer(lhs), integer(rhs)) {
        return match op {
            BinOp::Add => a.checked_add(b).map(HirExprKind::ConstInt),
            BinOp::Sub => a.checked_sub(b).map(HirExprKind::ConstInt),
            BinOp::Mul => a.checked_mul(b).map(HirExprKind::ConstInt),
            BinOp::Rem => a.checked_rem(b).map(HirExprKind::ConstInt),
            BinOp::Div if b != 0 => Some(HirExprKind::ConstFloat(a as f64 / b as f64)),
            _ => None,
        };
    }
    let (a, b) = (number(lhs)?, number(rhs)?);
    let value = match op {
        BinOp::Add => a + b,
        BinOp::Sub => a - b,
        BinOp::Mul => a * b,
        BinOp::Div if b != 0.0 => a / b,
        _ => return None,
    };
    value.is_finite().then_some(HirExprKind::ConstFloat(value))
}

/// Recover the one value assigned by a pure conditional expression. A branch
/// with statements other than its single result store still needs evaluation
/// and cannot be discarded from the chart-bar prefix.
fn conditional_result(stmt: &HirStmt, depth: usize) -> Option<(SymbolId, HirExpr)> {
    if depth > MAX_DEPTH {
        return None;
    }
    match stmt {
        HirStmt::Assign { symbol, value, .. } => Some((*symbol, value.clone())),
        HirStmt::If {
            cond,
            then_branch,
            else_branch,
            span,
        } => {
            let [then_stmt] = then_branch.as_slice() else {
                return None;
            };
            let [else_stmt] = else_branch.as_slice() else {
                return None;
            };
            let (symbol, then_e) = conditional_result(then_stmt, depth + 1)?;
            let (other, else_e) = conditional_result(else_stmt, depth + 1)?;
            if symbol != other || then_e.ty != else_e.ty {
                return None;
            }
            let qualifier = cond.qualifier.join(then_e.qualifier).join(else_e.qualifier);
            Some((
                symbol,
                HirExpr {
                    ty: then_e.ty.clone(),
                    qualifier,
                    span: *span,
                    kind: HirExprKind::Ternary {
                        cond: Box::new(cond.clone()),
                        then_e: Box::new(then_e),
                        else_e: Box::new(else_e),
                    },
                },
            ))
        }
        _ => None,
    }
}

impl Analyzer<'_> {
    fn prepare_htf_conditional_prefix(&mut self, body: &[HirStmt]) -> bool {
        // Preserve the existing immutable and bounded-loop paths. Only this
        // path reconstructs secondary-context conditional result slots.
        if !body.iter().any(|stmt| matches!(stmt, HirStmt::If { .. })) {
            return false;
        }
        let original = self.htf_expr_origins.clone();
        let mut seen = HashSet::new();
        let supported = body.iter().all(|stmt| {
            let Some((symbol, value)) = conditional_result(stmt, 0) else {
                return false;
            };
            let declaration = &self.symbols[symbol.0 as usize];
            if declaration.persistent
                || declaration.varip
                || (matches!(stmt, HirStmt::If { .. }) && !declaration.synthetic)
                || !seen.insert(symbol)
            {
                return false;
            }
            if self.lower_htf_expr(&value, &mut 0, &mut 0).is_none() {
                // Input enums and integer length bindings are resolved from
                // their declarations, without a requested numeric series.
                return matches!(value.ty, Type::Enum(_) | Type::Int)
                    && self.selected_input_for_expr(&value).is_some();
            }
            self.htf_expr_origins.insert(symbol, value);
            true
        });
        if !supported {
            self.htf_expr_origins = original;
        }
        supported
    }

    /// Publish origins only after the whole prefix passes admission. Failure
    /// must not leave partially expanded meanings on symbols used elsewhere.
    pub(crate) fn prepare_htf_prefix(&mut self, body: &[HirStmt]) -> bool {
        // Retain all previously supported immutable helpers, including TA
        // origins with their own requested-context state slots.
        if body.iter().all(|stmt| {
            matches!(stmt, HirStmt::Assign { symbol, value, .. }
                if !self.symbols[symbol.0 as usize].persistent
                    && !self.symbols[symbol.0 as usize].varip
                    && self.lower_htf_expr(value, &mut 0, &mut 0).is_some())
        }) {
            return true;
        }
        if self.prepare_htf_conditional_prefix(body) {
            return true;
        }
        let mut mutable = HashSet::new();
        let mut writes = HashMap::<SymbolId, usize>::new();
        let mut pending = vec![(body, false)];
        while let Some((block, in_loop)) = pending.pop() {
            for statement in block {
                match statement {
                    HirStmt::Assign { symbol, .. } => {
                        let count = writes.entry(*symbol).or_default();
                        *count += 1;
                        if *count > 1 || in_loop {
                            mutable.insert(*symbol);
                        }
                    }
                    HirStmt::For { counter, body, .. } => {
                        mutable.insert(*counter);
                        pending.push((body, true));
                    }
                    _ => {}
                }
            }
        }
        let mut expansion = Expansion {
            values: HashMap::new(),
            iterations: 0,
            remaining: MAX_EXPANSION,
            mutable,
        };
        if expansion.statements(self, body).is_none() {
            return false;
        }
        let original = self.htf_expr_origins.clone();
        for (symbol, value) in &expansion.values {
            if matches!(value.ty, Type::Int | Type::Float | Type::Bool) {
                self.htf_expr_origins.insert(*symbol, value.clone());
            }
        }
        let supported = expansion.values.values().all(|value| {
            matches!(
                value.kind,
                HirExprKind::CallCollection {
                    func: CollectionFunc::ArrayFrom,
                    ..
                }
            ) || self.lower_htf_expr(value, &mut 0, &mut 0).is_some()
        });
        if !supported {
            self.htf_expr_origins = original;
        }
        supported
    }
}
