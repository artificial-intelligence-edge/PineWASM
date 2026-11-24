//! A deliberately narrow, structural recognition of an ordered distance scan.
//!
//! Recognition uses resolved symbols and typed operations only, never source
//! text or names. The original loop remains as a runtime fallback. The match
//! covers one fixed loop shape and does not generalize to other loops: symbols
//! that are persistent or varip are excluded, and the threshold reset before
//! the shifts is part of the matched shape.

use pinewasm_hir::{
    BinOp, CollectionFunc as C, HirExpr as E, HirExprKind as K, HirProgram, HirStmt as S,
    PureFunc as P, Qualifier, SymbolId, Type,
};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq)]
enum Expr {
    Int(i64),
    Na,
    Read(SymbolId),
    Field(Box<Expr>, u32),
    Widen(Box<Expr>),
    Binary(BinOp, Type, Box<Expr>, Box<Expr>),
    Pure(P, Vec<Expr>),
    Collection(C, Vec<Expr>),
}

fn expr(e: &E, aliases: &BTreeMap<SymbolId, E>) -> Option<Expr> {
    Some(match &e.kind {
        K::ConstInt(n) => Expr::Int(*n),
        K::ConstNa => Expr::Na,
        K::ReadVar { symbol, offset: 0 } => match aliases.get(symbol) {
            Some(value) => expr(value, &BTreeMap::new())?,
            None => Expr::Read(*symbol),
        },
        K::GetField { obj, field_idx } => Expr::Field(Box::new(expr(obj, aliases)?), *field_idx),
        K::IntToFloat(e) => Expr::Widen(Box::new(expr(e, aliases)?)),
        K::Binary { op, lhs, rhs } => Expr::Binary(
            *op,
            e.ty.clone(),
            Box::new(expr(lhs, aliases)?),
            Box::new(expr(rhs, aliases)?),
        ),
        K::CallPure { func, args } => Expr::Pure(
            *func,
            args.iter()
                .map(|e| expr(e, aliases))
                .collect::<Option<_>>()?,
        ),
        K::CallCollection { func, args } => Expr::Collection(
            *func,
            args.iter()
                .map(|e| expr(e, aliases))
                .collect::<Option<_>>()?,
        ),
        _ => return None,
    })
}

fn binary(op: BinOp, ty: Type, a: Expr, b: Expr) -> Expr {
    Expr::Binary(op, ty, Box::new(a), Box::new(b))
}
fn field(obj: Expr, n: u32) -> Expr {
    Expr::Field(Box::new(obj), n)
}
fn coll(f: C, args: Vec<Expr>) -> Expr {
    Expr::Collection(f, args)
}
fn pure(f: P, args: Vec<Expr>) -> Expr {
    Expr::Pure(f, args)
}
fn widen(e: Expr) -> Expr {
    Expr::Widen(Box::new(e))
}

fn assignment(s: &S) -> Option<(SymbolId, &E)> {
    match s {
        S::Assign { symbol, value, .. } => Some((*symbol, value)),
        _ => None,
    }
}
fn effect(s: &S, expected: Expr, aliases: &BTreeMap<SymbolId, E>) -> bool {
    matches!(s, S::Eval { expr: e, .. } if expr(e, aliases) == Some(expected))
}
fn read(e: &E) -> Option<SymbolId> {
    match e.kind {
        K::ReadVar { symbol, offset: 0 } => Some(symbol),
        _ => None,
    }
}
fn stable(e: &E) -> bool {
    match &e.kind {
        K::ReadVar { offset: 0, .. } | K::ConstInt(_) => true,
        K::GetField { obj, .. } => read(obj).is_some(),
        _ => false,
    }
}

fn distance(query: Expr, arrays: Expr, counter: SymbolId, count: u32) -> Expr {
    let term = |j| {
        pure(
            P::MathLog,
            vec![binary(
                BinOp::Add,
                Type::Float,
                widen(Expr::Int(1)),
                pure(
                    P::MathAbs,
                    vec![binary(
                        BinOp::Sub,
                        Type::Float,
                        field(query.clone(), j),
                        coll(
                            C::ArrayGet,
                            vec![field(arrays.clone(), j), Expr::Read(counter)],
                        ),
                    )],
                ),
            )],
        )
    };
    let mut result = term(0);
    for j in 1..count {
        result = binary(BinOp::Add, Type::Float, result, term(j));
    }
    result
}

struct Scan {
    args: Vec<E>,
    threshold: SymbolId,
    hidden: BTreeSet<SymbolId>,
}

/// Recognize the complete body, including all inlined-function statements.
/// Every erased write is checked for observers outside this exact loop.
fn recognize(s: &S) -> Option<Scan> {
    let S::For {
        counter,
        start,
        end,
        step,
        is_int: true,
        implicit_step: true,
        body,
        ..
    } = s
    else {
        return None;
    };
    if !matches!(step.kind, K::ConstInt(1)) || !stable(start) || !stable(end) || body.len() != 8 {
        return None;
    }
    let mut aliases = BTreeMap::new();
    let mut hidden = BTreeSet::from([*counter]);
    for statement in &body[..5] {
        let (symbol, value) = assignment(statement)?;
        // Alias definitions are ordered, pure, and cannot depend on another
        // alias except the captured switch selector (the fifth assignment).
        let resolved = if let Some(other) = read(value) {
            aliases.get(&other).unwrap_or(value).clone()
        } else {
            value.clone()
        };
        if !stable(&resolved) || hidden.contains(&symbol) {
            return None;
        }
        aliases.insert(symbol, resolved);
        hidden.insert(symbol);
    }
    let alias_vec: Vec<_> = body[..5].iter().map(assignment).collect::<Option<_>>()?;
    if expr(alias_vec[0].1, &BTreeMap::new()) != Some(Expr::Read(*counter)) {
        return None;
    }
    let count = aliases.get(&alias_vec[1].0)?.clone();
    let query = aliases.get(&alias_vec[2].0)?.clone();
    let arrays = aliases.get(&alias_vec[3].0)?.clone();
    if read(&query).is_none()
        || read(&arrays).is_none()
        || expr(alias_vec[4].1, &aliases) != expr(&count, &aliases)
    {
        return None;
    }
    let q = expr(&query, &aliases)?;
    let a = expr(&arrays, &aliases)?;
    let mut switch = &body[5];
    let mut result_slot = None;
    for n in (2..=5).rev() {
        let S::If {
            cond,
            then_branch,
            else_branch,
            ..
        } = switch
        else {
            return None;
        };
        if expr(cond, &aliases)?
            != binary(BinOp::Eq, Type::Bool, expr(&count, &aliases)?, Expr::Int(n))
            || then_branch.len() != 1
            || else_branch.len() != 1
        {
            return None;
        }
        let (slot, value) = assignment(&then_branch[0])?;
        if result_slot.is_some_and(|old| old != slot)
            || expr(value, &aliases)? != distance(q.clone(), a.clone(), *counter, n as u32)
        {
            return None;
        }
        result_slot = Some(slot);
        switch = &else_branch[0];
    }
    let (slot, value) = assignment(switch)?;
    if Some(slot) != result_slot || !matches!(value.kind, K::ConstNa) {
        return None;
    }
    hidden.insert(slot);
    let (d, value) = assignment(&body[6])?;
    if read(value) != Some(slot) || hidden.contains(&d) {
        return None;
    }
    hidden.insert(d);
    let S::If {
        cond,
        then_branch: accepted,
        else_branch,
        ..
    } = &body[7]
    else {
        return None;
    };
    if !else_branch.is_empty() || accepted.len() != 4 {
        return None;
    }
    let (threshold, value) = assignment(&accepted[0])?;
    if read(value) != Some(d) || hidden.contains(&threshold) {
        return None;
    }
    let expected = binary(
        BinOp::And,
        Type::Bool,
        binary(BinOp::Ge, Type::Bool, Expr::Read(d), Expr::Read(threshold)),
        binary(
            BinOp::Ne,
            Type::Bool,
            binary(BinOp::Rem, Type::Int, Expr::Read(*counter), Expr::Int(4)),
            Expr::Int(0),
        ),
    );
    if expr(cond, &aliases)? != expected {
        return None;
    }
    let S::Eval { expr: pushed, .. } = &accepted[1] else {
        return None;
    };
    let K::CallCollection {
        func: C::ArrayPush,
        args: pushes,
    } = &pushed.kind
    else {
        return None;
    };
    if pushes.len() != 2 || read(&pushes[1]) != Some(d) || read(&pushes[0]).is_none() {
        return None;
    }
    let distances = pushes[0].clone();
    let S::Eval { expr: pushed, .. } = &accepted[2] else {
        return None;
    };
    let K::CallCollection {
        func: C::ArrayPush,
        args: pushes,
    } = &pushed.kind
    else {
        return None;
    };
    if pushes.len() != 2 || read(&pushes[0]).is_none() {
        return None;
    }
    let predictions = pushes[0].clone();
    let Expr::Widen(rounded) = expr(&pushes[1], &aliases)? else {
        return None;
    };
    let Expr::Pure(P::MathRound, rounded) = *rounded else {
        return None;
    };
    let [Expr::Collection(C::ArrayGet, get)] = rounded.as_slice() else {
        return None;
    };
    let [Expr::Read(labels), Expr::Read(i)] = get.as_slice() else {
        return None;
    };
    if *i != *counter {
        return None;
    }
    // Retain the actual typed label-array read. A float-label rounding
    // overload is not matched and the original loop runs; do not fabricate an
    // integer HIR type.
    let K::IntToFloat(rounded_label) = &pushes[1].kind else {
        return None;
    };
    let K::CallPure {
        func: P::MathRound,
        args: rounded_args,
    } = &rounded_label.kind
    else {
        return None;
    };
    let [element] = rounded_args.as_slice() else {
        return None;
    };
    let K::CallCollection {
        func: C::ArrayGet,
        args: label_args,
    } = &element.kind
    else {
        return None;
    };
    let [label_array, _] = label_args.as_slice() else {
        return None;
    };
    if label_array.ty != Type::Array(Box::new(Type::Int)) || read(label_array) != Some(*labels) {
        return None;
    }
    let labels = label_array.clone();
    let S::If {
        cond,
        then_branch: shifted,
        else_branch,
        ..
    } = &accepted[3]
    else {
        return None;
    };
    let K::Binary {
        op: BinOp::Gt,
        rhs: neighbors,
        ..
    } = &cond.kind
    else {
        return None;
    };
    if !stable(neighbors) || shifted.len() != 3 || !else_branch.is_empty() {
        return None;
    }
    let pred = expr(&predictions, &aliases)?;
    let dist = expr(&distances, &aliases)?;
    let k = expr(neighbors, &aliases)?;
    if expr(cond, &aliases)?
        != binary(
            BinOp::Gt,
            Type::Bool,
            coll(C::ArraySize, vec![pred.clone()]),
            k.clone(),
        )
    {
        return None;
    }
    let (target, reset) = assignment(&shifted[0])?;
    let index = pure(
        P::MathRound,
        vec![binary(
            BinOp::Div,
            Type::Int,
            binary(BinOp::Mul, Type::Int, k, Expr::Int(3)),
            Expr::Int(4),
        )],
    );
    if target != threshold
        || expr(reset, &aliases)? != coll(C::ArrayGet, vec![dist.clone(), index])
        || !effect(&shifted[1], coll(C::ArrayShift, vec![dist]), &aliases)
        || !effect(&shifted[2], coll(C::ArrayShift, vec![pred]), &aliases)
    {
        return None;
    }
    let threshold_read = E {
        kind: K::ReadVar {
            symbol: threshold,
            offset: 0,
        },
        ty: Type::Float,
        qualifier: Qualifier::Series,
        span: s.span(),
    };
    let args = vec![
        start.clone(),
        end.clone(),
        count,
        query,
        arrays,
        labels,
        distances,
        predictions,
        neighbors.as_ref().clone(),
        threshold_read,
    ];
    // Loop bounds and all captured operands must be invariant under the writes
    // of the original body. Array aliasing is checked by the runtime guard.
    let mut written = hidden.clone();
    written.insert(threshold);
    if args[..9].iter().any(|e| references(e, &written)) {
        return None;
    }
    Some(Scan {
        args,
        threshold,
        hidden,
    })
}

/// Walk the complete typed Serde tree, including expression-nested reads,
/// rather than maintaining a second child-list visitor. HIR variable reads
/// use ReadVar/ReadVarDynamic; serialization failure declines the pass.
fn references<T: serde::Serialize>(value: &T, symbols: &BTreeSet<SymbolId>) -> bool {
    fn walk(v: &serde_json::Value, symbols: &BTreeSet<SymbolId>) -> bool {
        match v {
            serde_json::Value::Object(fields) => {
                for name in ["ReadVar", "ReadVarDynamic"] {
                    if let Some(id) = fields
                        .get(name)
                        .and_then(|v| v.get("symbol"))
                        .and_then(serde_json::Value::as_u64)
                    {
                        if symbols.contains(&SymbolId(id as u32)) {
                            return true;
                        }
                    }
                }
                fields.values().any(|v| walk(v, symbols))
            }
            serde_json::Value::Array(values) => values.iter().any(|v| walk(v, symbols)),
            _ => false,
        }
    }
    serde_json::to_value(value).map_or(true, |v| walk(&v, symbols))
}

fn observed_outside(body: &[S], omitted: &S, hidden: &BTreeSet<SymbolId>) -> bool {
    body.iter().any(|s| {
        if std::ptr::eq(s, omitted) {
            return false;
        }
        match s {
            S::If {
                cond,
                then_branch,
                else_branch,
                ..
            } => {
                references(cond, hidden)
                    || observed_outside(then_branch, omitted, hidden)
                    || observed_outside(else_branch, omitted, hidden)
            }
            S::For {
                start,
                end,
                step,
                body,
                ..
            } => {
                references(start, hidden)
                    || references(end, hidden)
                    || references(step, hidden)
                    || observed_outside(body, omitted, hidden)
            }
            S::While { cond, body, .. } => {
                references(cond, hidden) || observed_outside(body, omitted, hidden)
            }
            _ => references(s, hidden),
        }
    })
}

fn collect(
    body: &[S],
    root: &[S],
    symbols: &[pinewasm_hir::Symbol],
    replacements: &mut BTreeMap<SymbolId, S>,
) {
    for s in body {
        if let Some(scan) = recognize(s) {
            if scan
                .hidden
                .iter()
                .chain(std::iter::once(&scan.threshold))
                .all(|id| {
                    symbols
                        .get(id.0 as usize)
                        .is_some_and(|s| !s.persistent && !s.varip)
                })
                && !observed_outside(root, s, &scan.hidden)
            {
                let call = |func, ty| E {
                    kind: K::CallCollection {
                        func,
                        args: scan.args.clone(),
                    },
                    ty,
                    qualifier: Qualifier::Series,
                    span: s.span(),
                };
                let mut guard_args = scan.args.clone();
                for position in [2, 8] {
                    let field = match &scan.args[position].kind {
                        K::GetField { obj, field_idx } => {
                            guard_args[position] = obj.as_ref().clone();
                            i64::from(*field_idx)
                        }
                        _ => -1,
                    };
                    guard_args.push(E {
                        kind: K::ConstInt(field),
                        ty: Type::Int,
                        qualifier: Qualifier::Const,
                        span: s.span(),
                    });
                }
                let guard = E {
                    kind: K::CallCollection {
                        func: C::InternalDistanceScanEligible,
                        args: guard_args,
                    },
                    ty: Type::Bool,
                    qualifier: Qualifier::Series,
                    span: s.span(),
                };
                let replacement = S::If {
                    cond: guard,
                    then_branch: vec![S::Assign {
                        symbol: scan.threshold,
                        value: call(C::InternalDistanceScan, Type::Float),
                        init: false,
                        span: s.span(),
                    }],
                    else_branch: vec![s.clone()],
                    span: s.span(),
                };
                if let S::For { counter, .. } = s {
                    replacements.insert(*counter, replacement);
                }
                continue;
            }
        }
        match s {
            S::If {
                then_branch,
                else_branch,
                ..
            } => {
                collect(then_branch, root, symbols, replacements);
                collect(else_branch, root, symbols, replacements);
            }
            S::For { body, .. } | S::While { body, .. } => {
                collect(body, root, symbols, replacements);
            }
            _ => {}
        }
    }
}

pub(crate) fn optimize(hir: &HirProgram) -> Cow<'_, HirProgram> {
    let mut replacements = BTreeMap::new();
    collect(&hir.body, &hir.body, &hir.symbols, &mut replacements);
    if replacements.is_empty() {
        return Cow::Borrowed(hir);
    }
    fn rewrite(body: &mut [S], replacements: &mut BTreeMap<SymbolId, S>) {
        for s in body {
            if let S::For { counter, .. } = s {
                if let Some(replacement) = replacements.remove(counter) {
                    *s = replacement;
                    continue;
                }
            }
            match s {
                S::If {
                    then_branch,
                    else_branch,
                    ..
                } => {
                    rewrite(then_branch, replacements);
                    rewrite(else_branch, replacements);
                }
                S::For { body, .. } | S::While { body, .. } => rewrite(body, replacements),
                _ => {}
            }
        }
    }
    let mut hir = hir.clone();
    rewrite(&mut hir.body, &mut replacements);
    Cow::Owned(hir)
}
