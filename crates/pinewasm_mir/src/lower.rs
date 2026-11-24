//! HIR → MIR lowering.
//!
//! Expression trees are flattened into instruction sequences in evaluation
//! order (post-order); `if` statements become diamond-shaped block graphs.
//! Lowering requires an error-free `HirProgram`, which the pipeline
//! guarantees by gating on semantic diagnostics, and always produces a body.
//! Its one diagnostic of its own is `E0251`, for a `strategy.risk.*` argument
//! too large to hoist ([`lower_with_diagnostics`]).

use crate::{BlockId, MirBody, MirInstr, MirMetadata, SlotDef, Terminator, Value, ValueId};
use pinewasm_hir::{
    HirExpr, HirExprKind, HirProgram, HirStmt, OrderKind, PlotColorRole, PureFunc, SymbolId,
    TaFunc, Type, UnOp,
};
use pinewasm_span::{codes, Diagnostic, Diagnostics, Span};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

pub(crate) mod kernel;

use kernel::{CallKind, EffectKind, ExprShape, LowerHost, LowerState, StmtShape};

/// Integer core of the literal color-channel conversion: clamp `value` to
/// `0..=max`, then narrow to `u8` with `as` (callers pass `max <= 255`).
/// Kept free of `f64` so it stays inside the Aeneas-extractable subset.
pub(crate) fn clamp_channel(value: i64, max: i64) -> u8 {
    value.clamp(0, max) as u8
}

/// Fold only literal color constructors. Requiring every operand to be a
/// literal preserves evaluation of dynamic arguments even when another
/// operand is na. Inputs, history, nested calls, and arithmetic stay live.
fn literal_color_constructor(func: PureFunc, args: &[HirExpr]) -> Option<Value> {
    fn literal(expr: &HirExpr) -> Option<Value> {
        Some(match &expr.kind {
            HirExprKind::ConstInt(v) => Value::Int(*v),
            HirExprKind::ConstFloat(v) => Value::from_f64(*v),
            HirExprKind::ConstNa => Value::Na,
            HirExprKind::ConstColor { r, g, b, t } => Value::Color {
                r: *r,
                g: *g,
                b: *b,
                t: *t,
                input_origin: None,
            },
            HirExprKind::IntToFloat(inner) => match &inner.kind {
                HirExprKind::ConstInt(v) => Value::from_f64(*v as f64),
                HirExprKind::ConstNa => Value::Na,
                _ => return None,
            },
            _ => return None,
        })
    }
    // Match the runtime's channel conversion: round finite floats, saturate
    // to i64, then clamp. Na is handled after all operands qualify as literals.
    fn channel(value: &Value, max: i64) -> Option<u8> {
        let value = match value {
            Value::Int(v) => *v,
            Value::Float(v) => v.round() as i64,
            _ => return None,
        };
        Some(clamp_channel(value, max))
    }
    fn transparency_percentage(value: &Value) -> Option<f64> {
        match value {
            Value::Int(t) => Some((*t as f64).clamp(0.0, 100.0)),
            Value::Float(t) => Some(t.clamp(0.0, 100.0)),
            _ => None,
        }
    }
    if !matches!(func, PureFunc::ColorNew | PureFunc::ColorRgb) {
        return None;
    }
    let values = args.iter().map(literal).collect::<Option<Vec<_>>>()?;
    match (func, values.as_slice()) {
        (PureFunc::ColorNew, [color, transparency]) => {
            if matches!(color, Value::Na) || matches!(transparency, Value::Na) {
                return Some(Value::Na);
            }
            let Value::Color { r, g, b, .. } = color else {
                return None;
            };
            Some(Value::Color {
                r: *r,
                g: *g,
                b: *b,
                t: transparency_percentage(transparency)?,
                input_origin: None,
            })
        }
        (PureFunc::ColorRgb, [r, g, b, t]) => {
            if values.iter().any(|v| matches!(v, Value::Na)) {
                return Some(Value::Na);
            }
            Some(Value::Color {
                r: channel(r, 255)?,
                g: channel(g, 255)?,
                b: channel(b, 255)?,
                t: transparency_percentage(t)?,
                input_origin: None,
            })
        }
        _ => None,
    }
}

pub fn lower(hir: &HirProgram) -> MirBody {
    lower_with_diagnostics(hir).0
}

/// [`lower`], with the diagnostics lowering raises itself: one `E0251` per
/// `strategy.risk.*` argument whose hoisted form is over
/// [`MAX_RISK_OPERAND_NODES`] or [`MAX_RISK_OPERAND_DEPTH`]. The body is
/// complete either way; a caller that compiles a script must not ship it
/// when a diagnostic is an error.
pub fn lower_with_diagnostics(hir: &HirProgram) -> (MirBody, Vec<Diagnostic>) {
    // strategy.risk.* directives are active for every execution, regardless of
    // where they appear in the source. Emit each syntactic directive exactly
    // once at the start of every script execution, before any regular order
    // can reach the broker (`kernel::lower_prelude`). The in-place nodes are
    // `StmtShape::Nop`, so branch/loop execution cannot duplicate or
    // deactivate them.
    let hoisted = collect_hoisted_risk_directives(hir);
    let host = HirHost {
        hir,
        risk_directives: &hoisted.directives,
    };
    let (blocks, entry, value_count, temp_slot_count) = kernel::lower_body(&host, &hir.body);
    let series_slots =
        crate::slot_table::series_slot_table(&SlotDefs, &hir.symbols, temp_slot_count);
    let diagnostics = hoisted
        .oversized
        .iter()
        .map(|(kind, span)| oversized_risk_operand(*kind, *span))
        .collect::<Diagnostics>()
        .finish();

    let body = MirBody {
        blocks,
        entry,
        value_count,
        metadata: MirMetadata {
            indicator: hir.indicator.clone(),
            title: hir.indicator.as_ref().map(|i| i.title.clone()).or_else(|| {
                hir.strategy
                    .as_ref()
                    .map(|s| s.resolved_shorttitle().to_string())
            }),
            overlay: hir
                .indicator
                .as_ref()
                .map(|i| i.overlay)
                .or_else(|| hir.strategy.as_ref().map(|s| s.overlay))
                .unwrap_or(false),
            strategy: hir.strategy.clone(),
            drawing_caps: hir
                .indicator
                .as_ref()
                .map(pinewasm_hir::IndicatorInfo::drawing_caps)
                .or_else(|| {
                    hir.strategy
                        .as_ref()
                        .map(pinewasm_hir::StrategyInfo::drawing_caps)
                })
                .unwrap_or_default(),
            max_bars_back: hir.max_bars_back,
            series_slots,
            plots: hir.plots.clone(),
            visuals: hir.visuals.clone(),
            fills: hir.fills.clone(),
            ta_states: hir.ta_states.clone(),
            ta_input_bindings: hir.ta_input_bindings.clone(),
            inputs: hir.inputs.clone(),
            object_types: hir.object_types.clone(),
        },
    };
    (body, diagnostics)
}

/// Builds the program's [`SlotDef`]s for [`crate::slot_table::series_slot_table`]:
/// a symbol keeps its name and persistence, a temporary slot is the
/// non-persistent `%t{index}`.
struct SlotDefs;

impl crate::slot_table::SlotTableHost<pinewasm_hir::Symbol, SlotDef> for SlotDefs {
    fn symbol_slot(&self, s: &pinewasm_hir::Symbol) -> SlotDef {
        SlotDef {
            name: s.name.clone(),
            persistent: s.persistent,
            varip: s.varip,
            lane: None,
        }
    }

    fn temp_slot(&self, index: u32) -> SlotDef {
        SlotDef {
            name: format!("%t{index}"),
            persistent: false,
            varip: false,
            lane: None,
        }
    }
}

/// The most expression nodes one hoisted definition or directive operand may
/// have once the definitions it reads are substituted into it.
///
/// Substitution copies a definition into every expression that reads it, so
/// a variable read twice by its own reassignment doubles at each step, and so
/// does one reassigned under an `if`, whose merged value holds both branches.
/// The limit keeps the collection linear in the size of the program: no
/// definition is copied before its size is known to fit. It also bounds what
/// the prelude evaluates on every execution.
pub const MAX_RISK_OPERAND_NODES: usize = 4096;

/// The deepest nesting one hoisted definition or directive operand may have.
/// The parser bounds the nesting of a written expression; substitution chains
/// written expressions, and the later stages recurse over the result.
pub const MAX_RISK_OPERAND_DEPTH: usize = 256;

/// The most expression nodes the hoisted definitions and operands of one
/// program may have together. Each assignment a directive can reach stores
/// its own substituted copy, so the limit per operand alone leaves the total
/// growing with the number of assignments.
pub const MAX_RISK_HOISTED_NODES: usize = 1 << 18;

/// The risk directives of a program, hoisted.
struct HoistedRisk {
    /// Each directive as an `HirStmt::Order` with the expanded operands, in
    /// source order.
    directives: Vec<HirStmt>,
    /// The operands over a limit, each with its directive's kind. Such a
    /// directive keeps its operands as written.
    oversized: Vec<(OrderKind, Span)>,
    /// What is left of [`MAX_RISK_HOISTED_NODES`].
    budget: usize,
}

fn oversized_risk_operand(kind: OrderKind, span: Span) -> Diagnostic {
    Diagnostic::error(
        codes::E0251,
        format!(
            "`strategy.{}` argument is too large once the variables it reads are substituted",
            kind.member()
        ),
    )
    .with_label(
        span,
        format!(
            "expands to more than {MAX_RISK_OPERAND_NODES} expression nodes or {MAX_RISK_OPERAND_DEPTH} nested levels"
        ),
    )
    .with_note(format!(
        "the risk arguments of one script, with the variables they read, may expand to {MAX_RISK_HOISTED_NODES} expression nodes together"
    ))
    .with_note(
        "a risk command applies from the start of every execution, so its argument is evaluated ahead of the script from the expressions that define the variables it reads; compute the value in fewer reassignments, or read it from an input",
    )
}

/// Collect every syntactic `strategy.risk.*` directive in source order and
/// substitute reads of `simple` variables with their source-order defining
/// expressions. Risk parameters are checked as `simple` or weaker in
/// semantics, so this substitution is stable across bars and lets a local
/// declaration inside a branch supply a globally-active directive without
/// executing the branch itself.
///
/// A definition is substituted when it is assigned, not when a directive
/// reads it: a variable holds the value its expression had at the assignment,
/// whatever is reassigned afterwards. Each stored definition is therefore
/// closed (it reads no defined symbol) and substituting it is one copy. A
/// reassigned `var` variable is a series, which no directive may read, so a
/// definition never has to stand for a value kept from an earlier execution.
///
/// The definitions exist only to be substituted into directive operands, so
/// the collection follows only the symbols those operands can reach
/// ([`RiskSymbols`]). A program without a directive, which is nearly every
/// program, follows none and is scanned once.
fn collect_hoisted_risk_directives(hir: &HirProgram) -> HoistedRisk {
    let mut hoisted = HoistedRisk {
        directives: Vec::new(),
        oversized: Vec::new(),
        budget: MAX_RISK_HOISTED_NODES,
    };
    if let Some(followed) = RiskSymbols::of(hir) {
        collect_risk_from_stmts(&hir.body, hir, &followed, &mut HashMap::new(), &mut hoisted);
    }
    hoisted
}

/// The symbols whose `simple` definitions [`collect_risk_from_stmts`] keeps:
/// the ones an expansion of a risk directive's operand can look up.
///
/// A substitution ([`substitute_definitions`]) looks up the symbols its
/// expression reads. A definition is a value assigned to the symbol with its
/// own reads substituted, or a ternary an `if` around such an assignment
/// builds from its condition and the branch values. The set is therefore
/// closed under "read by an assigned value" and "read by the condition of an
/// `if` around an assignment". No lookup leaves it, and the definition of a
/// symbol in it never depends on one outside it, so the directives are the
/// ones the collection finds when it keeps every symbol.
enum RiskSymbols {
    /// Every symbol: the baseline the tests compare `Only` against.
    #[cfg(test)]
    All,
    /// One flag per symbol.
    Only(Vec<bool>),
}

impl RiskSymbols {
    fn follows(&self, symbol: SymbolId) -> bool {
        match self {
            #[cfg(test)]
            RiskSymbols::All => true,
            RiskSymbols::Only(followed) => followed[symbol.0 as usize],
        }
    }

    /// The symbols to follow for `hir`, or `None` when it has no risk
    /// directive and so nothing to collect.
    fn of(hir: &HirProgram) -> Option<Self> {
        let mut pending = Vec::new();
        if !risk_operand_reads(&hir.body, &mut pending) {
            return None;
        }
        let mut followed = vec![false; hir.symbols.len()];
        if pending.is_empty() {
            // The operands read no variable: there is nothing to expand.
            return Some(RiskSymbols::Only(followed));
        }
        let mut index = AssignmentIndex {
            sites: vec![Vec::new(); hir.symbols.len()],
            ifs: Vec::new(),
        };
        index.scan(&hir.body, None);
        let mut seen = vec![false; index.ifs.len()];
        while let Some(symbol) = pending.pop() {
            if std::mem::replace(&mut followed[symbol.0 as usize], true) {
                continue;
            }
            for &(value, innermost) in &index.sites[symbol.0 as usize] {
                substituted_reads(value, &mut pending);
                let mut around = innermost;
                while let Some(id) = around {
                    if std::mem::replace(&mut seen[id], true) {
                        break;
                    }
                    let (cond, outer) = index.ifs[id];
                    substituted_reads(cond, &mut pending);
                    around = outer;
                }
            }
        }
        Some(RiskSymbols::Only(followed))
    }
}

/// Append the symbols `stmts` reads in the operands of risk directives where
/// a substitution looks up a definition, and say whether `stmts` contains a
/// risk directive at all.
fn risk_operand_reads(stmts: &[HirStmt], reads: &mut Vec<SymbolId>) -> bool {
    let mut found = false;
    for stmt in stmts {
        match stmt {
            HirStmt::Order { kind, operands, .. } if kind.is_risk() => {
                found = true;
                for operand in operands {
                    substituted_reads(operand, reads);
                }
            }
            HirStmt::If {
                then_branch,
                else_branch,
                ..
            } => {
                found |= risk_operand_reads(then_branch, reads);
                found |= risk_operand_reads(else_branch, reads);
            }
            HirStmt::For { body, .. } | HirStmt::While { body, .. } => {
                found |= risk_operand_reads(body, reads);
            }
            _ => {}
        }
    }
    found
}

/// Append the symbols [`substitute_definitions`] looks up in `expr`: the
/// current-bar reads it reaches through the forms it rebuilds.
fn substituted_reads(expr: &HirExpr, reads: &mut Vec<SymbolId>) {
    match &expr.kind {
        HirExprKind::ReadVar { symbol, offset: 0 } => reads.push(*symbol),
        HirExprKind::Unary { operand, .. } => substituted_reads(operand, reads),
        HirExprKind::Binary { lhs, rhs, .. } => {
            substituted_reads(lhs, reads);
            substituted_reads(rhs, reads);
        }
        HirExprKind::Ternary {
            cond,
            then_e,
            else_e,
        } => {
            substituted_reads(cond, reads);
            substituted_reads(then_e, reads);
            substituted_reads(else_e, reads);
        }
        HirExprKind::IntToFloat(inner) => substituted_reads(inner, reads),
        HirExprKind::CallPure { args, .. } => {
            for arg in args {
                substituted_reads(arg, reads);
            }
        }
        _ => {}
    }
}

/// Where a program assigns each symbol, for [`RiskSymbols::of`].
struct AssignmentIndex<'h> {
    /// Per symbol: each value assigned to it, with the innermost `if` around
    /// the assignment (an index into `ifs`).
    sites: Vec<Vec<(&'h HirExpr, Option<usize>)>>,
    /// Per `if`: its condition and the `if` around it.
    ifs: Vec<(&'h HirExpr, Option<usize>)>,
}

impl<'h> AssignmentIndex<'h> {
    fn scan(&mut self, stmts: &'h [HirStmt], around: Option<usize>) {
        for stmt in stmts {
            match stmt {
                HirStmt::Assign { symbol, value, .. } => {
                    self.sites[symbol.0 as usize].push((value, around));
                }
                HirStmt::If {
                    cond,
                    then_branch,
                    else_branch,
                    ..
                } => {
                    let id = self.ifs.len();
                    self.ifs.push((cond, around));
                    self.scan(then_branch, Some(id));
                    self.scan(else_branch, Some(id));
                }
                HirStmt::For { body, .. } | HirStmt::While { body, .. } => {
                    self.scan(body, around);
                }
                _ => {}
            }
        }
    }
}

/// What a followed symbol holds at one point of the source-order scan.
#[derive(Clone)]
enum Definition {
    /// Its value as an expression that reads no defined symbol. Shared:
    /// every branch environment that leaves the symbol alone holds the same
    /// allocation.
    Closed(Rc<Closed>),
    /// A value over [`MAX_RISK_OPERAND_NODES`] or [`MAX_RISK_OPERAND_DEPTH`].
    /// Whatever reads it is oversized too.
    Oversized,
}

/// An expression with every definition it read substituted, and its size.
struct Closed {
    expr: HirExpr,
    nodes: usize,
    depth: usize,
}

type Definitions = HashMap<SymbolId, Definition>;

/// Whether a branch left a definition as it was before the `if`.
fn same_definition(branch: Option<&Definition>, before: Option<&Definition>) -> bool {
    match (branch, before) {
        (None, None) | (Some(Definition::Oversized), Some(Definition::Oversized)) => true,
        (Some(Definition::Closed(branch)), Some(Definition::Closed(before))) => {
            Rc::ptr_eq(branch, before) || branch.expr == before.expr
        }
        _ => false,
    }
}

fn collect_risk_from_stmts(
    stmts: &[HirStmt],
    hir: &HirProgram,
    followed: &RiskSymbols,
    definitions: &mut Definitions,
    out: &mut HoistedRisk,
) {
    for stmt in stmts {
        match stmt {
            HirStmt::Assign { symbol, value, .. } => {
                if !followed.follows(*symbol) {
                    continue;
                }
                if hir.symbol(*symbol).qualifier <= pinewasm_hir::Qualifier::Simple {
                    // Substituted now, against the definitions in force here:
                    // a later reassignment of what `value` reads, or of the
                    // symbol itself, does not reach back into this value.
                    let definition = match substitute(value, definitions, &mut out.budget) {
                        Some(closed) => Definition::Closed(Rc::new(closed)),
                        None => Definition::Oversized,
                    };
                    definitions.insert(*symbol, definition);
                } else {
                    definitions.remove(symbol);
                }
            }
            HirStmt::Order {
                kind,
                operands,
                span,
            } if kind.is_risk() => {
                let mut expanded = Vec::with_capacity(operands.len());
                for operand in operands {
                    match substitute(operand, definitions, &mut out.budget) {
                        Some(closed) => expanded.push(closed.expr),
                        None => out.oversized.push((*kind, operand.span)),
                    }
                }
                out.directives.push(HirStmt::Order {
                    kind: *kind,
                    operands: if expanded.len() == operands.len() {
                        expanded
                    } else {
                        operands.clone()
                    },
                    span: *span,
                });
            }
            HirStmt::If {
                cond,
                then_branch,
                else_branch,
                ..
            } => {
                // Each branch has its own lexical/source-order environment.
                // Both are scanned because runtime selection cannot disable a
                // risk rule, but branch-local definitions must not leak into
                // the sibling or following scope.
                let mut then_definitions = definitions.clone();
                collect_risk_from_stmts(then_branch, hir, followed, &mut then_definitions, out);
                let mut else_definitions = definitions.clone();
                collect_risk_from_stmts(else_branch, hir, followed, &mut else_definitions, out);

                // A simple `if` expression is represented in HIR as branch
                // assignments into one hidden result slot followed by a read
                // of that slot. Preserve that value in the hoisting
                // environment by merging definitions from both paths into an
                // equivalent ternary. Series-controlled merges are irrelevant
                // here: semantics rejects them as risk arguments.
                let changed_symbols = then_definitions
                    .keys()
                    .chain(else_definitions.keys())
                    .chain(definitions.keys())
                    .copied()
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .filter(|symbol| {
                        let before = definitions.get(symbol);
                        !(same_definition(then_definitions.get(symbol), before)
                            && same_definition(else_definitions.get(symbol), before))
                    })
                    .collect::<Vec<_>>();
                if changed_symbols.is_empty() {
                    continue;
                }
                // The condition is evaluated before either branch runs, so it
                // reads the definitions in force before the `if`: substitute
                // it before any merged definition replaces one of them.
                let cond_definition =
                    (cond.qualifier <= pinewasm_hir::Qualifier::Simple).then(|| {
                        match substitute(cond, definitions, &mut out.budget) {
                            Some(closed) => Definition::Closed(Rc::new(closed)),
                            None => Definition::Oversized,
                        }
                    });
                for symbol in changed_symbols {
                    let merged = match (
                        &cond_definition,
                        then_definitions.get(&symbol),
                        else_definitions.get(&symbol),
                    ) {
                        (Some(cond_definition), Some(then_value), Some(else_value))
                            if hir.symbol(symbol).qualifier <= pinewasm_hir::Qualifier::Simple =>
                        {
                            Some(merge_definitions(
                                cond,
                                cond_definition,
                                then_value,
                                else_value,
                                &hir.symbol(symbol).ty,
                                &mut out.budget,
                            ))
                        }
                        _ => None,
                    };
                    match merged {
                        Some(merged) => definitions.insert(symbol, merged),
                        None => definitions.remove(&symbol),
                    };
                }
            }
            HirStmt::For { body, .. } | HirStmt::While { body, .. } => {
                // A loop may execute zero or many times, but a risk directive
                // in its body is one globally-active syntactic rule.
                let mut body_definitions = definitions.clone();
                collect_risk_from_stmts(body, hir, followed, &mut body_definitions, out);
            }
            HirStmt::Destructure { .. }
            | HirStmt::SetField { .. }
            | HirStmt::Plot { .. }
            | HirStmt::Visual { .. }
            | HirStmt::Fill { .. }
            | HirStmt::RuntimeError { .. }
            | HirStmt::Log { .. }
            | HirStmt::Alert { .. }
            | HirStmt::AlertCondition { .. }
            | HirStmt::Order { .. }
            | HirStmt::Eval { .. }
            | HirStmt::Break { .. }
            | HirStmt::Continue { .. } => {}
        }
    }
}

/// The value both paths of an `if` leave in a symbol: `cond ? then : else`
/// over the closed branch values, with the closed condition. `cond` is the
/// condition as written, for the qualifier and span of the ternary.
fn merge_definitions(
    cond: &HirExpr,
    cond_definition: &Definition,
    then_value: &Definition,
    else_value: &Definition,
    ty: &Type,
    budget: &mut usize,
) -> Definition {
    let (
        Definition::Closed(cond_closed),
        Definition::Closed(then_closed),
        Definition::Closed(else_closed),
    ) = (cond_definition, then_value, else_value)
    else {
        return Definition::Oversized;
    };
    let nodes = 1 + cond_closed.nodes + then_closed.nodes + else_closed.nodes;
    let depth = 1 + cond_closed
        .depth
        .max(then_closed.depth)
        .max(else_closed.depth);
    if nodes > MAX_RISK_OPERAND_NODES.min(*budget) || depth > MAX_RISK_OPERAND_DEPTH {
        return Definition::Oversized;
    }
    *budget -= nodes;
    Definition::Closed(Rc::new(Closed {
        expr: HirExpr {
            kind: HirExprKind::Ternary {
                cond: Box::new(cond_closed.expr.clone()),
                then_e: Box::new(then_closed.expr.clone()),
                else_e: Box::new(else_closed.expr.clone()),
            },
            ty: ty.clone(),
            qualifier: cond
                .qualifier
                .join(then_closed.expr.qualifier)
                .join(else_closed.expr.qualifier),
            span: cond.span,
        },
        nodes,
        depth,
    }))
}

/// `expr` with every current-bar read of a defined symbol replaced by that
/// definition, or `None` when the result is over a limit or over `budget`,
/// which it is taken from.
fn substitute(expr: &HirExpr, definitions: &Definitions, budget: &mut usize) -> Option<Closed> {
    let (mut nodes, mut depth) = (0, 0);
    let max_nodes = MAX_RISK_OPERAND_NODES.min(*budget);
    let expr = substitute_definitions(expr, definitions, max_nodes, 1, &mut nodes, &mut depth)?;
    *budget -= nodes;
    Some(Closed { expr, nodes, depth })
}

/// [`substitute`] for the subexpression `expr` at nesting `level` of the
/// result (the root is level 1). `nodes` and `depth` are the size of the
/// result so far, which may have `max_nodes`. Every definition is closed, so a substituted one is copied
/// as it is, and only when the result with it still fits the limits.
fn substitute_definitions(
    expr: &HirExpr,
    definitions: &Definitions,
    max_nodes: usize,
    level: usize,
    nodes: &mut usize,
    depth: &mut usize,
) -> Option<HirExpr> {
    let mut grow = |more: usize, reaches: usize| {
        *nodes += more;
        *depth = (*depth).max(reaches);
        *nodes <= max_nodes && *depth <= MAX_RISK_OPERAND_DEPTH
    };
    if let HirExprKind::ReadVar { symbol, offset: 0 } = &expr.kind {
        match definitions.get(symbol) {
            Some(Definition::Closed(closed)) => {
                return grow(closed.nodes, level + closed.depth - 1).then(|| closed.expr.clone());
            }
            Some(Definition::Oversized) => return None,
            None => {}
        }
    }
    if !grow(1, level) {
        return None;
    }
    let mut child = |expr: &HirExpr| {
        substitute_definitions(expr, definitions, max_nodes, level + 1, nodes, depth).map(Box::new)
    };
    let kind = match &expr.kind {
        HirExprKind::Unary { op, operand } => HirExprKind::Unary {
            op: *op,
            operand: child(operand)?,
        },
        HirExprKind::Binary { op, lhs, rhs } => HirExprKind::Binary {
            op: *op,
            lhs: child(lhs)?,
            rhs: child(rhs)?,
        },
        HirExprKind::Ternary {
            cond,
            then_e,
            else_e,
        } => HirExprKind::Ternary {
            cond: child(cond)?,
            then_e: child(then_e)?,
            else_e: child(else_e)?,
        },
        HirExprKind::IntToFloat(inner) => HirExprKind::IntToFloat(child(inner)?),
        HirExprKind::CallPure { func, args } => HirExprKind::CallPure {
            func: *func,
            args: args
                .iter()
                .map(|arg| child(arg).map(|arg| *arg))
                .collect::<Option<_>>()?,
        },
        // Other expression forms either have no child expression or produce
        // series/reference values and therefore cannot pass the risk
        // command's `simple`-qualifier check. Preserve them defensively, as
        // one node; an earlier diagnostic prevents an invalid program from
        // reaching MIR.
        _ => expr.kind.clone(),
    };
    Some(HirExpr {
        kind,
        ty: expr.ty.clone(),
        qualifier: expr.qualifier,
        span: expr.span,
    })
}

/// A block under construction. Generic over the instruction and terminator
/// types only so the allocator kernels below can be extracted and reasoned
/// about without `MirInstr`/`Terminator` (which carry `f64` and `String`);
/// lowering always uses the defaults.
pub(crate) struct BlockBuilder<I = MirInstr, T = Terminator> {
    pub(crate) id: BlockId,
    pub(crate) instrs: Vec<I>,
    pub(crate) terminator: Option<T>,
}

/// Allocate a fresh, empty block at the end of `blocks`; its id is its index.
pub(crate) fn push_block<I, T>(blocks: &mut Vec<BlockBuilder<I, T>>) -> BlockId {
    let id = BlockId(blocks.len() as u32);
    blocks.push(BlockBuilder {
        id,
        instrs: Vec::new(),
        terminator: None,
    });
    id
}

/// Allocate the next dense value id from the counter.
pub(crate) fn alloc_value_id(next_value: &mut u32) -> ValueId {
    let id = ValueId(*next_value);
    *next_value += 1;
    id
}

/// Append `instr` to block `current`, which must not be terminated yet.
pub(crate) fn emit_into<I, T>(blocks: &mut [BlockBuilder<I, T>], current: BlockId, instr: I) {
    let block = &mut blocks[current.0 as usize];
    // Expression position proves this macro cannot expand to an item.
    let () = debug_assert!(
        block.terminator.is_none(),
        "emitting into a terminated block"
    );
    block.instrs.push(instr);
}

/// Set block `current`'s terminator unless it already has one (first wins).
pub(crate) fn terminate_block<I, T>(
    blocks: &mut [BlockBuilder<I, T>],
    current: BlockId,
    terminator: T,
) {
    let block = &mut blocks[current.0 as usize];
    if block.terminator.is_none() {
        block.terminator = Some(terminator);
    }
}

/// The compiler's [`LowerHost`]: classifies `HirExpr`/`HirStmt` nodes and
/// hands their children, arguments and payloads to the lowering kernel
/// (`kernel`).
struct HirHost<'h> {
    hir: &'h HirProgram,
    /// The hoisted risk directives (`HirStmt::Order` with a risk kind),
    /// emitted by the kernel's prelude.
    risk_directives: &'h [HirStmt],
}

/// The call-like expression forms the kernel lowers with
/// [`ExprShape::Call`], or `None`.
fn call_shape(expr: &HirExpr) -> Option<ExprShape> {
    let (kind, arity) = match &expr.kind {
        HirExprKind::ReadSecurity {
            selector,
            source,
            htf_seconds,
            timeframe_expr,
            calendar_months,
            gaps_on,
            lookahead_on,
        } => (
            CallKind::ReadSecurity {
                selector: selector.clone(),
                source: *source,
                htf_seconds: *htf_seconds,
                timeframe_expr: timeframe_expr.clone(),
                calendar_months: *calendar_months,
                gaps_on: *gaps_on,
                lookahead_on: *lookahead_on,
            },
            0,
        ),
        HirExprKind::ReadSecurityExpr {
            expr: htf_expr,
            htf_seconds,
            timeframe_expr,
            calendar_months,
            gaps_on,
            lookahead_on,
        } => (
            CallKind::ReadSecurityExpr {
                expr: htf_expr.clone(),
                htf_seconds: *htf_seconds,
                timeframe_expr: timeframe_expr.clone(),
                calendar_months: *calendar_months,
                gaps_on: *gaps_on,
                lookahead_on: *lookahead_on,
            },
            0,
        ),
        HirExprKind::ReadRequest { func, selector } => (
            CallKind::ReadRequest {
                func: *func,
                selector: selector.clone(),
            },
            0,
        ),
        HirExprKind::BuildFootprint { selector } => (CallKind::BuildFootprint(selector.clone()), 0),
        HirExprKind::ReadSecurityLowerTf { selector } => {
            (CallKind::ReadSecurityLowerTf(selector.clone()), 0)
        }
        HirExprKind::ReadPivotLevels {
            system,
            anchor_seconds,
        } => (
            CallKind::ReadPivotLevels {
                system: *system,
                anchor_seconds: *anchor_seconds,
            },
            0,
        ),
        HirExprKind::CallPivotPointLevels { args } => (CallKind::CallPivotPointLevels, args.len()),
        HirExprKind::ReadTimeTf { close, tf_seconds } => (
            CallKind::ReadTimeTf {
                close: *close,
                tf_seconds: *tf_seconds,
            },
            0,
        ),
        HirExprKind::ReadDrawingsAll { kind } => (CallKind::ReadDrawingsAll(*kind), 0),
        HirExprKind::FootprintRowByPrice { .. } => (CallKind::FootprintRowByPrice, 2),
        HirExprKind::CallTaState { state, args, .. } => (CallKind::Ta(*state), args.len()),
        HirExprKind::CallPure { func, args } => (CallKind::Pure(*func), args.len()),
        HirExprKind::CallCollection { func, args } => (CallKind::Collection(*func), args.len()),
        HirExprKind::NewObject { type_id, fields } => (CallKind::NewObject(*type_id), fields.len()),
        HirExprKind::CopyObject { type_id, .. } => (CallKind::CopyObject(*type_id), 1),
        HirExprKind::GetField { field_idx, .. } => (CallKind::GetField(*field_idx), 1),
        HirExprKind::RegisterDrawing { kind, .. } => (CallKind::RegisterDrawing(*kind), 1),
        HirExprKind::DeleteDrawing { kind, .. } => (CallKind::DeleteDrawing(*kind), 1),
        _ => return None,
    };
    Some(ExprShape::Call { kind, arity })
}

/// The roles of a plot/visual/fill color list.
fn color_roles(colors: &[(PlotColorRole, HirExpr)]) -> Vec<PlotColorRole> {
    colors.iter().map(|(role, _)| *role).collect()
}

impl LowerHost<HirExpr, HirStmt> for HirHost<'_> {
    fn ternary_arms_total(&self, expr: &HirExpr) -> bool {
        // A literal cannot fail, and neither can a unary operator applied to
        // one (`-1`, `not true`): the only failing unary case, int negation
        // of `i64::MIN`, is excluded explicitly.
        let total = |arm: &HirExpr| match &arm.kind {
            HirExprKind::Unary { operand, .. } => {
                !matches!(operand.kind, HirExprKind::ConstInt(i64::MIN))
                    && matches!(self.expr_shape(operand), ExprShape::Literal)
            }
            _ => matches!(self.expr_shape(arm), ExprShape::Literal),
        };
        total(self.expr_child(expr, 1)) && total(self.expr_child(expr, 2))
    }

    fn expr_shape(&self, expr: &HirExpr) -> ExprShape {
        // Literal color constructors fold to a constant (`literal_value`).
        // Requiring every operand to be a literal preserves evaluation of
        // dynamic arguments even when another operand is na.
        if let HirExprKind::CallPure { func, args } = &expr.kind {
            if literal_color_constructor(*func, args).is_some() {
                return ExprShape::Literal;
            }
        }
        if let Some(shape) = call_shape(expr) {
            return shape;
        }
        match &expr.kind {
            HirExprKind::ConstInt(_)
            | HirExprKind::ConstFloat(_)
            | HirExprKind::ConstBool(_)
            | HirExprKind::ConstNa
            | HirExprKind::ConstString(_)
            | HirExprKind::ConstColor { .. } => ExprShape::Literal,
            HirExprKind::ReadBuiltin { series, offset } => ExprShape::ReadBuiltin {
                series: *series,
                offset: *offset,
            },
            HirExprKind::ReadVar { symbol, offset } => ExprShape::ReadVar {
                symbol: *symbol,
                offset: *offset,
            },
            HirExprKind::ReadBuiltinDynamic { series, .. } => {
                ExprShape::ReadBuiltinDynamic { series: *series }
            }
            HirExprKind::ReadVarDynamic { symbol, .. } => {
                ExprShape::ReadVarDynamic { symbol: *symbol }
            }
            HirExprKind::ReadInput { input } => ExprShape::ReadInput { input: *input },
            // Int negation has its own checked opcode; `not` is always bool.
            HirExprKind::Unary { op, .. } => ExprShape::Unary {
                op: *op,
                is_int: expr.ty == Type::Int,
            },
            HirExprKind::Binary { op, .. } => ExprShape::Binary {
                op: *op,
                // Only int arithmetic produces an int result; mixed
                // operands were already coerced to float in HIR.
                is_int: expr.ty == Type::Int,
                // A String-typed `+` is concatenation (StrConcat opcode);
                // any other operator producing a string is impossible.
                is_string: expr.ty == Type::String,
            },
            // A literal widening is independent of every runtime input. Use
            // the same i64 -> f64 conversion as IntToF64, including rounding
            // beyond 2^53, without dispatching it on every bar. Keep dynamic
            // reads and integer arithmetic on the runtime path: they can
            // carry history, overrides, or overflow to na.
            HirExprKind::IntToFloat(operand) => match &operand.kind {
                HirExprKind::ConstInt(_) | HirExprKind::ConstNa => ExprShape::Literal,
                _ => ExprShape::IntToFloat,
            },
            HirExprKind::Ternary { .. } => ExprShape::Ternary,
            HirExprKind::MaterializeHistory { slot, offset, .. } => ExprShape::MaterializeHistory {
                slot: *slot,
                offset: *offset,
            },
            HirExprKind::MaterializeHistoryDynamic { slot, .. } => {
                ExprShape::MaterializeHistoryDynamic { slot: *slot }
            }
            HirExprKind::TaCall {
                func: TaFunc::Change,
                slot,
                length,
                ..
            } => ExprShape::TaChange {
                slot: *slot,
                length: *length,
            },
            HirExprKind::TaCall {
                func: TaFunc::Sma,
                slot,
                length,
                ..
            } => ExprShape::TaSma {
                slot: *slot,
                length: *length,
            },
            // Literal color constructors fold to a constant (`literal_value`).
            // Requiring every operand to be a literal preserves evaluation of
            // dynamic arguments even when another operand is na.
            HirExprKind::SetFieldExpr { field_idx, .. } => ExprShape::SetFieldExpr {
                field_idx: *field_idx,
            },
            HirExprKind::SetFields {
                recv_slot, writes, ..
            } => ExprShape::SetFields {
                recv_slot: *recv_slot,
                field_idxs: writes.iter().map(|(field_idx, _)| *field_idx).collect(),
            },
            HirExprKind::LineGetPrice { recv_slot, .. } => ExprShape::LineGetPrice {
                recv_slot: *recv_slot,
            },
            HirExprKind::SetCell {
                recv_slot, writes, ..
            } => ExprShape::SetCell {
                recv_slot: *recv_slot,
                attrs: writes.iter().map(|(attr, _)| *attr).collect(),
            },
            HirExprKind::ClearCells { recv_slot, .. } => ExprShape::Cells {
                recv_slot: *recv_slot,
                merge: false,
            },
            HirExprKind::MergeCells { recv_slot, .. } => ExprShape::Cells {
                recv_slot: *recv_slot,
                merge: true,
            },
            // Engine-backed `ta` functions in a slot-based TaCall and bare
            // tuples never reach MIR (`lower_other_expr`).
            _ => ExprShape::Other,
        }
    }

    fn expr_child<'a>(&self, expr: &'a HirExpr, index: u32) -> &'a HirExpr {
        match (&expr.kind, index) {
            (
                HirExprKind::ReadBuiltinDynamic { offset: child, .. }
                | HirExprKind::ReadVarDynamic { offset: child, .. }
                | HirExprKind::Unary { operand: child, .. }
                | HirExprKind::Binary { lhs: child, .. }
                | HirExprKind::IntToFloat(child)
                | HirExprKind::Ternary { cond: child, .. }
                | HirExprKind::MaterializeHistory { expr: child, .. }
                | HirExprKind::MaterializeHistoryDynamic { expr: child, .. }
                | HirExprKind::TaCall { source: child, .. }
                | HirExprKind::SetFieldExpr { obj: child, .. }
                | HirExprKind::SetFields { obj: child, .. }
                | HirExprKind::LineGetPrice { obj: child, .. }
                | HirExprKind::SetCell { obj: child, .. }
                | HirExprKind::ClearCells { obj: child, .. }
                | HirExprKind::MergeCells { obj: child, .. },
                0,
            )
            | (
                HirExprKind::Binary { rhs: child, .. }
                | HirExprKind::Ternary { then_e: child, .. }
                | HirExprKind::MaterializeHistoryDynamic { offset: child, .. }
                | HirExprKind::SetFieldExpr { value: child, .. }
                | HirExprKind::LineGetPrice { x: child, .. }
                | HirExprKind::SetCell { column: child, .. }
                | HirExprKind::ClearCells {
                    start_column: child,
                    ..
                }
                | HirExprKind::MergeCells {
                    start_column: child,
                    ..
                },
                1,
            )
            | (
                HirExprKind::Ternary { else_e: child, .. }
                | HirExprKind::SetCell { row: child, .. }
                | HirExprKind::ClearCells {
                    start_row: child, ..
                }
                | HirExprKind::MergeCells {
                    start_row: child, ..
                },
                2,
            )
            | (
                HirExprKind::ClearCells {
                    end_column: child, ..
                }
                | HirExprKind::MergeCells {
                    end_column: child, ..
                },
                3,
            )
            | (
                HirExprKind::ClearCells { end_row: child, .. }
                | HirExprKind::MergeCells { end_row: child, .. },
                4,
            ) => child,
            _ => unreachable!("expression has no child {index}"),
        }
    }

    fn expr_arg<'a>(&self, expr: &'a HirExpr, index: usize) -> &'a HirExpr {
        match &expr.kind {
            HirExprKind::CallTaState { args, .. }
            | HirExprKind::CallPure { args, .. }
            | HirExprKind::CallCollection { args, .. }
            | HirExprKind::CallPivotPointLevels { args }
            | HirExprKind::NewObject { fields: args, .. } => &args[index],
            HirExprKind::SetFields { writes, .. } => &writes[index].1,
            HirExprKind::SetCell { writes, .. } => &writes[index].1,
            HirExprKind::FootprintRowByPrice { footprint, price } => match index {
                0 => footprint,
                1 => price,
                _ => unreachable!("expression has no argument {index}"),
            },
            HirExprKind::CopyObject { obj: arg, .. }
            | HirExprKind::GetField { obj: arg, .. }
            | HirExprKind::RegisterDrawing { inner: arg, .. }
            | HirExprKind::DeleteDrawing { id: arg, .. }
                if index == 0 =>
            {
                arg
            }
            _ => unreachable!("expression has no argument {index}"),
        }
    }

    fn expr_span(&self, expr: &HirExpr) -> Span {
        expr.span
    }

    fn literal_value(&self, expr: &HirExpr) -> Value {
        match &expr.kind {
            HirExprKind::ConstInt(v) => Value::Int(*v),
            HirExprKind::ConstFloat(v) => Value::from_f64(*v),
            HirExprKind::ConstBool(v) => Value::Bool(*v),
            // The `na` literal: its type lives in HIR only; at runtime it
            // is the ordinary na value.
            HirExprKind::ConstNa => Value::Na,
            HirExprKind::ConstString(v) => Value::String(v.clone()),
            HirExprKind::ConstColor { r, g, b, t } => Value::Color {
                r: *r,
                g: *g,
                b: *b,
                t: *t,
                input_origin: None,
            },
            HirExprKind::IntToFloat(operand) => match &operand.kind {
                HirExprKind::ConstInt(value) => Value::from_f64(*value as f64),
                HirExprKind::ConstNa => Value::Na,
                _ => unreachable!("non-literal widening has no literal value"),
            },
            HirExprKind::CallPure { func, args } => literal_color_constructor(*func, args)
                .unwrap_or_else(|| unreachable!("non-literal color constructor")),
            _ => unreachable!("expression has no literal value"),
        }
    }

    fn positive_literal_step(&self, expr: &HirExpr) -> bool {
        matches!(literal_step_sign(expr), Some(true))
    }

    fn lower_other_expr(&self, st: &mut LowerState, expr: &HirExpr) -> ValueId {
        match &expr.kind {
            HirExprKind::EvalSequence { bindings, result } => {
                for (slot, operand) in bindings {
                    let value = kernel::lower_expr(self, st, operand);
                    kernel::emit(
                        st,
                        MirInstr::StoreUserSeriesCurrent {
                            slot: *slot,
                            value,
                            span: operand.span,
                        },
                    );
                }
                kernel::lower_expr(self, st, result)
            }
            HirExprKind::TaCallDynamic {
                func,
                slot,
                source,
                length,
            } => {
                let value = kernel::lower_expr(self, st, source);
                if *func != TaFunc::Ema {
                    kernel::emit(
                        st,
                        MirInstr::StoreUserSeriesCurrent {
                            slot: *slot,
                            value,
                            span: expr.span,
                        },
                    );
                }
                let length = kernel::lower_expr(self, st, length);
                let dst = kernel::new_value(st);
                kernel::emit(
                    st,
                    match func {
                        TaFunc::Sma => MirInstr::WindowMeanDynamic {
                            dst,
                            slot: *slot,
                            length,
                            span: expr.span,
                        },
                        TaFunc::Highest | TaFunc::Lowest => MirInstr::WindowExtremumDynamic {
                            dst,
                            slot: *slot,
                            length,
                            highest: *func == TaFunc::Highest,
                            span: expr.span,
                        },
                        TaFunc::Median
                        | TaFunc::MedianInt
                        | TaFunc::Mode
                        | TaFunc::ModeInt
                        | TaFunc::Range
                        | TaFunc::RangeInt => MirInstr::WindowStatisticDynamic {
                            dst,
                            slot: *slot,
                            length,
                            func: *func,
                            span: expr.span,
                        },
                        TaFunc::Ema => MirInstr::EmaSimpleDynamic {
                            dst,
                            slot: *slot,
                            source: value,
                            length,
                            span: expr.span,
                        },
                        _ => unreachable!("unexpected dynamic window function"),
                    },
                );
                dst
            }
            // Engine-backed functions never reach the slot-based TaCall node
            // (semantics emits CallTaState for them). The source is lowered
            // first, before the panic below, so a malformed program reports
            // the same (first) panic.
            HirExprKind::TaCall { func, source, .. } => {
                kernel::lower_expr(self, st, source);
                unreachable!("engine function {} in a slot-based TaCall", func.name())
            }
            // A tuple expression never reaches expression lowering: the
            // Destructure statement consumes it directly.
            HirExprKind::Tuple { .. } => {
                unreachable!("Tuple is consumed by Destructure lowering, not lowered as a scalar")
            }
            _ => unreachable!("expression handled by the lowering kernel"),
        }
    }

    fn stmt_shape(&self, stmt: &HirStmt) -> StmtShape {
        match stmt {
            HirStmt::Assign { symbol, init, .. } => StmtShape::Assign {
                symbol: *symbol,
                init: *init,
            },
            HirStmt::Destructure { targets, value, .. } => match &value.kind {
                HirExprKind::CallTaState { state, args, .. } => StmtShape::Destructure {
                    state: *state,
                    arity: args.len(),
                    targets: targets.clone(),
                },
                _ => StmtShape::Other,
            },
            HirStmt::SetField { field_idx, .. } => StmtShape::Effect {
                kind: EffectKind::SetField(*field_idx),
                arity: 2,
            },
            HirStmt::Plot { slot, colors, .. } => StmtShape::Effect {
                kind: EffectKind::Plot {
                    slot: *slot,
                    roles: color_roles(colors),
                },
                arity: 1 + colors.len(),
            },
            HirStmt::Visual { slot, colors, .. } => StmtShape::Effect {
                kind: EffectKind::Visual {
                    slot: *slot,
                    roles: color_roles(colors),
                },
                arity: colors.len(),
            },
            HirStmt::Fill { slot, colors, .. } => StmtShape::Effect {
                kind: EffectKind::Fill {
                    slot: *slot,
                    roles: color_roles(colors),
                },
                arity: colors.len(),
            },
            HirStmt::RuntimeError { .. } => StmtShape::Effect {
                kind: EffectKind::RuntimeError,
                arity: 1,
            },
            HirStmt::Log { level, args, .. } => StmtShape::Effect {
                kind: EffectKind::Log(*level),
                arity: 1 + args.len(),
            },
            HirStmt::Alert { .. } => StmtShape::Effect {
                kind: EffectKind::Alert,
                arity: 2,
            },
            HirStmt::AlertCondition { .. } => StmtShape::Effect {
                kind: EffectKind::AlertCondition,
                arity: 3,
            },
            // strategy.risk.* directives are hoisted into the prelude
            // (`lower`); the in-place node emits nothing, so branch/loop
            // execution cannot duplicate or deactivate them.
            HirStmt::Order { kind, .. } if kind.is_risk() => StmtShape::Nop,
            HirStmt::Order { kind, operands, .. } => StmtShape::Effect {
                kind: EffectKind::Order(*kind),
                arity: operands.len(),
            },
            HirStmt::Eval { .. } => StmtShape::Eval,
            HirStmt::If { .. } => StmtShape::If,
            HirStmt::For {
                counter,
                end_slot,
                step_slot,
                is_int,
                implicit_step,
                ..
            } => StmtShape::For {
                counter: *counter,
                end_slot: *end_slot,
                step_slot: *step_slot,
                is_int: *is_int,
                implicit_step: *implicit_step,
            },
            HirStmt::While { .. } => StmtShape::While,
            HirStmt::Break { .. } => StmtShape::Break,
            HirStmt::Continue { .. } => StmtShape::Continue,
        }
    }

    fn stmt_expr<'a>(&self, stmt: &'a HirStmt, index: u32) -> &'a HirExpr {
        match (stmt, index) {
            (
                HirStmt::Assign { value: e, .. }
                | HirStmt::Eval { expr: e, .. }
                | HirStmt::If { cond: e, .. }
                | HirStmt::For { start: e, .. }
                | HirStmt::While { cond: e, .. },
                0,
            )
            | (HirStmt::For { end: e, .. }, 1)
            | (HirStmt::For { step: e, .. }, 2) => e,
            _ => unreachable!("statement has no expression {index}"),
        }
    }

    fn stmt_arg<'a>(&self, stmt: &'a HirStmt, index: usize) -> &'a HirExpr {
        match stmt {
            HirStmt::Destructure { value, .. } => match &value.kind {
                HirExprKind::CallTaState { args, .. } => &args[index],
                _ => unreachable!("Destructure value is always a tuple-returning CallTaState"),
            },
            HirStmt::Plot { value, colors, .. } => {
                if index == 0 {
                    value
                } else {
                    &colors[index - 1].1
                }
            }
            HirStmt::Visual { colors, .. } | HirStmt::Fill { colors, .. } => &colors[index].1,
            HirStmt::Log { format, args, .. } => {
                if index == 0 {
                    format
                } else {
                    &args[index - 1]
                }
            }
            HirStmt::Order { operands, .. } => &operands[index],
            HirStmt::RuntimeError { message: e, .. } if index == 0 => e,
            HirStmt::SetField { obj: e, .. } | HirStmt::Alert { message: e, .. } if index == 0 => e,
            HirStmt::SetField { value: e, .. } | HirStmt::Alert { freq: e, .. } if index == 1 => e,
            HirStmt::AlertCondition {
                condition,
                title,
                message,
                ..
            } => match index {
                0 => condition,
                1 => title,
                2 => message,
                _ => unreachable!("statement has no argument {index}"),
            },
            _ => unreachable!("statement has no argument {index}"),
        }
    }

    fn stmt_body<'a>(&self, stmt: &'a HirStmt, index: u32) -> &'a [HirStmt] {
        match (stmt, index) {
            (
                HirStmt::If { then_branch: b, .. }
                | HirStmt::For { body: b, .. }
                | HirStmt::While { body: b, .. },
                0,
            )
            | (HirStmt::If { else_branch: b, .. }, 1) => b,
            _ => unreachable!("statement has no body {index}"),
        }
    }

    fn stmt_span(&self, stmt: &HirStmt) -> Span {
        stmt.span()
    }

    fn lower_other_stmt(&self, _st: &mut LowerState, _stmt: &HirStmt) {
        // The value is a tuple-returning engine call.
        unreachable!("Destructure value is always a tuple-returning CallTaState")
    }

    fn slot_of(&self, symbol: SymbolId) -> pinewasm_hir::SeriesSlot {
        self.hir.symbol(symbol).slot
    }

    fn slot_count(&self) -> u32 {
        self.hir.symbols.len() as u32
    }

    fn is_persistent(&self, symbol: SymbolId) -> bool {
        self.hir.symbol(symbol).persistent
    }

    fn prelude(&self) -> &[HirStmt] {
        self.risk_directives
    }

    fn prelude_order(&self, stmt: &HirStmt) -> (OrderKind, usize) {
        match stmt {
            HirStmt::Order { kind, operands, .. } => (*kind, operands.len()),
            _ => unreachable!("a hoisted risk directive is an Order"),
        }
    }
}

/// `Value::Na` (the placeholder a void setter pushes).
fn na_value() -> Value {
    Value::Na
}

/// The constant `1` in the loop's numeric flavor (int or float).
fn one_value(is_int: bool) -> Value {
    if is_int {
        Value::Int(1)
    } else {
        Value::Float(1.0)
    }
}

/// The constant `-1` in the loop's numeric flavor.
fn neg_one_value(is_int: bool) -> Value {
    if is_int {
        Value::Int(-1)
    } else {
        Value::Float(-1.0)
    }
}

/// The constant `0` in the loop's numeric flavor (for the step-sign test).
fn zero_value(is_int: bool) -> Value {
    if is_int {
        Value::Int(0)
    } else {
        Value::Float(0.0)
    }
}

/// The message of the runtime error a nonpositive dynamic `for` step raises.
/// Built here, beside `zero_value`, so the lowering kernel never constructs a
/// `Value` itself (the Aeneas extraction keeps `Value` opaque).
fn loop_step_error_value() -> Value {
    Value::String("'step' in loop must be greater than zero.".into())
}

/// Recognize positive literal loop-step magnitudes without evaluating inputs.
fn literal_step_sign(expr: &HirExpr) -> Option<bool> {
    literal_step_value(expr).map(|value| value > 0.0)
}

fn literal_step_value(expr: &HirExpr) -> Option<f64> {
    match &expr.kind {
        HirExprKind::ConstInt(value) => Some(*value as f64),
        HirExprKind::ConstFloat(value) => Some(*value),
        HirExprKind::IntToFloat(inner) => literal_step_value(inner),
        HirExprKind::Unary {
            op: UnOp::Neg,
            operand,
        } => literal_step_value(operand).map(|value| -value),
        _ => None,
    }
}

/// Differential tests for the extracted allocator and channel kernels: the
/// original bodies are reproduced verbatim and compared with the kernels the
/// `LowerCtx` methods delegate to.
#[cfg(test)]
mod alloc_kernel_tests {
    use super::*;

    struct OldBlock {
        id: BlockId,
        instrs: Vec<MirInstr>,
        terminator: Option<Terminator>,
    }

    /// A verbatim copy of the `LowerCtx` allocator state and methods, used as
    /// the expected behaviour.
    struct OldCtx {
        blocks: Vec<OldBlock>,
        current: BlockId,
        next_value: u32,
    }

    impl OldCtx {
        fn new_block(&mut self) -> BlockId {
            let id = BlockId(self.blocks.len() as u32);
            self.blocks.push(OldBlock {
                id,
                instrs: Vec::new(),
                terminator: None,
            });
            id
        }

        fn new_value(&mut self) -> ValueId {
            let id = ValueId(self.next_value);
            self.next_value += 1;
            id
        }

        fn emit(&mut self, instr: MirInstr) {
            let block = &mut self.blocks[self.current.0 as usize];
            let () = debug_assert!(
                block.terminator.is_none(),
                "emitting into a terminated block"
            );
            block.instrs.push(instr);
        }

        fn terminate(&mut self, terminator: Terminator) {
            let block = &mut self.blocks[self.current.0 as usize];
            if block.terminator.is_none() {
                block.terminator = Some(terminator);
            }
        }
    }

    /// The new allocator state, driven through the kernels exactly as the
    /// refactored `LowerCtx` methods drive them.
    struct NewCtx {
        blocks: Vec<BlockBuilder>,
        current: BlockId,
        next_value: u32,
    }

    fn assert_same(old: &OldCtx, new: &NewCtx) {
        assert_eq!(old.current, new.current);
        assert_eq!(old.next_value, new.next_value);
        assert_eq!(old.blocks.len(), new.blocks.len());
        for (o, n) in old.blocks.iter().zip(&new.blocks) {
            assert_eq!(o.id, n.id);
            assert_eq!(o.instrs, n.instrs);
            assert_eq!(o.terminator, n.terminator);
        }
    }

    #[test]
    fn allocator_kernels_match_pre_refactor_methods() {
        let mut old = OldCtx {
            blocks: Vec::new(),
            current: BlockId(0),
            next_value: 0,
        };
        let mut new = NewCtx {
            blocks: Vec::new(),
            current: BlockId(0),
            next_value: 0,
        };
        assert_eq!(old.new_block(), push_block(&mut new.blocks));
        // Deterministic LCG-driven op sequence.
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        for step in 0..5_000u32 {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let pick = (seed >> 33) as u32;
            let span = Span::dummy();
            match pick % 5 {
                0 => assert_eq!(old.new_block(), push_block(&mut new.blocks)),
                1 => assert_eq!(old.new_value(), alloc_value_id(&mut new.next_value)),
                2 => {
                    // Emit only into open blocks: emitting into a terminated
                    // block is a debug-assert failure in both versions.
                    let idx = old.current.0 as usize;
                    if old.blocks[idx].terminator.is_none() {
                        let instr = MirInstr::Const {
                            dst: ValueId(step),
                            value: Value::Int(i64::from(step)),
                            span,
                        };
                        old.emit(instr.clone());
                        emit_into(&mut new.blocks, new.current, instr);
                    }
                }
                3 => {
                    let terminator = if pick % 2 == 0 {
                        Terminator::Return { span }
                    } else {
                        Terminator::Jump {
                            target: BlockId(step),
                            span,
                        }
                    };
                    old.terminate(terminator.clone());
                    terminate_block(&mut new.blocks, new.current, terminator);
                }
                _ => {
                    let target = BlockId((pick / 5) % old.blocks.len() as u32);
                    old.current = target;
                    new.current = target;
                }
            }
            assert_same(&old, &new);
        }
    }

    #[test]
    #[should_panic(expected = "emitting into a terminated block")]
    #[cfg(debug_assertions)]
    fn emit_into_terminated_block_still_debug_panics() {
        let mut blocks: Vec<BlockBuilder> = Vec::new();
        let id = push_block(&mut blocks);
        terminate_block(
            &mut blocks,
            id,
            Terminator::Return {
                span: Span::dummy(),
            },
        );
        emit_into(
            &mut blocks,
            id,
            MirInstr::Const {
                dst: ValueId(0),
                value: Value::Na,
                span: Span::dummy(),
            },
        );
    }

    #[test]
    fn clamp_channel_matches_pre_refactor_clamp() {
        fn old_clamp(value: i64, max: i64) -> u8 {
            value.clamp(0, max) as u8
        }
        let mut values = vec![i64::MIN, i64::MIN + 1, -1, i64::MAX - 1, i64::MAX];
        values.extend(-300..=600);
        for max in [0, 1, 100, 255] {
            for &value in &values {
                assert_eq!(clamp_channel(value, max), old_clamp(value, max));
            }
        }
    }

    #[test]
    fn channel_conversion_matches_pre_refactor_body() {
        fn old_channel(value: &Value, max: i64) -> Option<u8> {
            let value = match value {
                Value::Int(v) => *v,
                Value::Float(v) => v.round() as i64,
                _ => return None,
            };
            Some(value.clamp(0, max) as u8)
        }
        fn new_channel(value: &Value, max: i64) -> Option<u8> {
            let value = match value {
                Value::Int(v) => *v,
                Value::Float(v) => v.round() as i64,
                _ => return None,
            };
            Some(clamp_channel(value, max))
        }
        let mut inputs = vec![Value::Na, Value::Bool(true), Value::String("x".into())];
        for v in [
            -1.0e300, -0.5, -0.49, 0.49, 0.5, 99.5, 100.5, 254.5, 255.5, 1.0e300,
        ] {
            inputs.push(Value::Float(v));
        }
        for v in [i64::MIN, -1, 0, 50, 100, 101, 255, 256, i64::MAX] {
            inputs.push(Value::Int(v));
        }
        for max in [100, 255] {
            for input in &inputs {
                assert_eq!(new_channel(input, max), old_channel(input, max));
            }
        }
    }
}

#[cfg(test)]
mod series_slot_table_tests {
    use super::*;
    use pinewasm_span::has_errors;

    /// A verbatim copy of the inline `series_slots` construction in `lower`,
    /// used as the expected value.
    fn reference(hir: &HirProgram, temp_slot_count: u32) -> Vec<SlotDef> {
        let mut series_slots = hir
            .symbols
            .iter()
            .map(|s| SlotDef {
                name: s.name.clone(),
                persistent: s.persistent,
                varip: s.varip,
                lane: None,
            })
            .collect::<Vec<_>>();
        for index in 0..temp_slot_count {
            series_slots.push(SlotDef {
                name: format!("%t{index}"),
                persistent: false,
                varip: false,
                lane: None,
            });
        }
        series_slots
    }

    fn hir(source: &str) -> HirProgram {
        let parsed = pinewasm_parse::parse(source);
        assert!(!has_errors(&parsed.diagnostics), "{:?}", parsed.diagnostics);
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        assert!(
            !has_errors(&analysis.diagnostics),
            "{:?}",
            analysis.diagnostics
        );
        analysis.hir
    }

    #[test]
    fn lower_series_slots_match_reference() {
        let sources = [
            "indicator(\"t\")\nplot(close)\n",
            "indicator(\"t\")\nvar float acc = 0.0\nvarip int ticks = 0\nacc += close\nticks += 1\nplot(acc)\n",
            "indicator(\"t\")\nx = close > open ? ta.sma(close, 3) : ta.ema(close, 5)\nplot(x)\n",
            "indicator(\"t\")\na = close > open ? ta.sma(close, 3) : na\nb = high > low ? ta.rsi(close, 14) : ta.sma(open, 2)\nc = a > b ? ta.ema(a, 4) : b\nplot(a)\nplot(b)\nplot(c)\n",
            "indicator(\"t\")\nf(x) => x > 0 ? x * 2 : x\ny = 0.0\nfor i = 0 to 3\n    y += f(close - i)\nplot(y)\nplot(close[2])\n",
        ];
        let mut saw_temps = false;
        let mut saw_persistent = false;
        for source in sources {
            let hir = hir(source);
            let risk_directives = collect_hoisted_risk_directives(&hir).directives;
            let host = HirHost {
                hir: &hir,
                risk_directives: &risk_directives,
            };
            let (_, _, _, temp_slot_count) = kernel::lower_body(&host, &hir.body);
            let shipped = lower(&hir).metadata.series_slots;
            assert_eq!(shipped, reference(&hir, temp_slot_count), "{source}");
            assert_eq!(
                shipped.len(),
                hir.symbols.len() + temp_slot_count as usize,
                "{source}"
            );
            saw_temps |= temp_slot_count > 0;
            saw_persistent |= shipped.iter().any(|slot| slot.persistent);
        }
        assert!(saw_temps, "no source allocated a temporary slot");
        assert!(saw_persistent, "no source declared a persistent slot");
    }
}

/// Tests for the risk-directive collection, against three baselines:
///
/// - the same collection keeping the definition of every symbol, without
///   [`RiskSymbols`] (`unnarrowed`);
/// - an earlier collection that substitutes definitions where they are read,
///   not where they are assigned (`lazy`), on programs that assign each value
///   once, where the two orders of substitution cannot differ;
/// - running the statements before a directive on concrete input values
///   (`run`), which is what a hoisted operand has to evaluate to.
#[cfg(test)]
mod risk_symbol_tests {
    use super::*;
    use pinewasm_hir::BinOp;
    use pinewasm_span::has_errors;

    /// The collection with every symbol followed.
    fn unnarrowed(hir: &HirProgram) -> Vec<HirStmt> {
        let mut hoisted = HoistedRisk {
            directives: Vec::new(),
            oversized: Vec::new(),
            budget: MAX_RISK_HOISTED_NODES,
        };
        collect_risk_from_stmts(
            &hir.body,
            hir,
            &RiskSymbols::All,
            &mut HashMap::new(),
            &mut hoisted,
        );
        hoisted.directives
    }

    /// An earlier form of the collection: definitions stored as written and
    /// substituted when a directive or a merge reads them, with only a
    /// per-path guard against a definition that reaches itself. It follows
    /// every path through a cycle of reassignments, inlines a self-reading
    /// reassignment into itself at each merge, and gives a definition the
    /// value its reads have at the directive, not at the assignment.
    mod lazy {
        use super::*;

        pub(super) fn reference(hir: &HirProgram) -> Vec<HirStmt> {
            let mut directives = Vec::new();
            collect_risk_from_stmts(&hir.body, hir, &mut HashMap::new(), &mut directives);
            directives
        }

        fn collect_risk_from_stmts(
            stmts: &[HirStmt],
            hir: &HirProgram,
            definitions: &mut HashMap<SymbolId, HirExpr>,
            out: &mut Vec<HirStmt>,
        ) {
            for stmt in stmts {
                match stmt {
                    HirStmt::Assign { symbol, value, .. } => {
                        if hir.symbol(*symbol).qualifier <= pinewasm_hir::Qualifier::Simple {
                            definitions.insert(*symbol, value.clone());
                        } else {
                            definitions.remove(symbol);
                        }
                    }
                    HirStmt::Order {
                        kind,
                        operands,
                        span,
                    } if kind.is_risk() => {
                        let operands = operands
                            .iter()
                            .map(|operand| {
                                expand_simple_definitions(operand, definitions, &mut HashSet::new())
                            })
                            .collect();
                        out.push(HirStmt::Order {
                            kind: *kind,
                            operands,
                            span: *span,
                        });
                    }
                    HirStmt::If {
                        cond,
                        then_branch,
                        else_branch,
                        ..
                    } => {
                        // Each branch has its own lexical/source-order environment.
                        // Both are scanned because runtime selection cannot disable a
                        // risk rule, but branch-local definitions must not leak into
                        // the sibling or following scope.
                        let mut then_definitions = definitions.clone();
                        collect_risk_from_stmts(then_branch, hir, &mut then_definitions, out);
                        let mut else_definitions = definitions.clone();
                        collect_risk_from_stmts(else_branch, hir, &mut else_definitions, out);

                        // A simple `if` expression is represented in HIR as branch
                        // assignments into one hidden result slot followed by a read
                        // of that slot. Preserve that value in the hoisting
                        // environment by merging definitions from both paths into an
                        // equivalent ternary. Series-controlled merges are irrelevant
                        // here: semantics rejects them as risk arguments.
                        let mut changed_symbols = then_definitions
                            .keys()
                            .chain(else_definitions.keys())
                            .copied()
                            .collect::<HashSet<_>>();
                        changed_symbols.extend(definitions.keys().copied());
                        for symbol in changed_symbols {
                            let before = definitions.get(&symbol);
                            let then_value = then_definitions.get(&symbol);
                            let else_value = else_definitions.get(&symbol);
                            if then_value == before && else_value == before {
                                continue;
                            }
                            if cond.qualifier <= pinewasm_hir::Qualifier::Simple
                                && hir.symbol(symbol).qualifier <= pinewasm_hir::Qualifier::Simple
                            {
                                if let (Some(then_value), Some(else_value)) =
                                    (then_value, else_value)
                                {
                                    let then_value = expand_simple_definitions(
                                        then_value,
                                        &then_definitions,
                                        &mut HashSet::new(),
                                    );
                                    let else_value = expand_simple_definitions(
                                        else_value,
                                        &else_definitions,
                                        &mut HashSet::new(),
                                    );
                                    definitions.insert(
                                        symbol,
                                        HirExpr {
                                            kind: HirExprKind::Ternary {
                                                cond: Box::new(cond.clone()),
                                                then_e: Box::new(then_value.clone()),
                                                else_e: Box::new(else_value.clone()),
                                            },
                                            ty: hir.symbol(symbol).ty.clone(),
                                            qualifier: cond
                                                .qualifier
                                                .join(then_value.qualifier)
                                                .join(else_value.qualifier),
                                            span: cond.span,
                                        },
                                    );
                                    continue;
                                }
                            }
                            definitions.remove(&symbol);
                        }
                    }
                    HirStmt::For { body, .. } | HirStmt::While { body, .. } => {
                        // A loop may execute zero or many times, but a risk directive
                        // in its body is one globally-active syntactic rule.
                        let mut body_definitions = definitions.clone();
                        collect_risk_from_stmts(body, hir, &mut body_definitions, out);
                    }
                    HirStmt::Destructure { .. }
                    | HirStmt::SetField { .. }
                    | HirStmt::Plot { .. }
                    | HirStmt::Visual { .. }
                    | HirStmt::Fill { .. }
                    | HirStmt::RuntimeError { .. }
                    | HirStmt::Log { .. }
                    | HirStmt::Alert { .. }
                    | HirStmt::AlertCondition { .. }
                    | HirStmt::Order { .. }
                    | HirStmt::Eval { .. }
                    | HirStmt::Break { .. }
                    | HirStmt::Continue { .. } => {}
                }
            }
        }

        fn expand_simple_definitions(
            expr: &HirExpr,
            definitions: &HashMap<SymbolId, HirExpr>,
            visiting: &mut HashSet<SymbolId>,
        ) -> HirExpr {
            if let HirExprKind::ReadVar { symbol, offset: 0 } = &expr.kind {
                if let Some(definition) = definitions.get(symbol) {
                    if visiting.insert(*symbol) {
                        let expanded = expand_simple_definitions(definition, definitions, visiting);
                        visiting.remove(symbol);
                        return expanded;
                    }
                }
                return expr.clone();
            }

            let mut expanded = expr.clone();
            expanded.kind = match &expr.kind {
                HirExprKind::Unary { op, operand } => HirExprKind::Unary {
                    op: *op,
                    operand: Box::new(expand_simple_definitions(operand, definitions, visiting)),
                },
                HirExprKind::Binary { op, lhs, rhs } => HirExprKind::Binary {
                    op: *op,
                    lhs: Box::new(expand_simple_definitions(lhs, definitions, visiting)),
                    rhs: Box::new(expand_simple_definitions(rhs, definitions, visiting)),
                },
                HirExprKind::Ternary {
                    cond,
                    then_e,
                    else_e,
                } => HirExprKind::Ternary {
                    cond: Box::new(expand_simple_definitions(cond, definitions, visiting)),
                    then_e: Box::new(expand_simple_definitions(then_e, definitions, visiting)),
                    else_e: Box::new(expand_simple_definitions(else_e, definitions, visiting)),
                },
                HirExprKind::IntToFloat(inner) => HirExprKind::IntToFloat(Box::new(
                    expand_simple_definitions(inner, definitions, visiting),
                )),
                HirExprKind::CallPure { func, args } => HirExprKind::CallPure {
                    func: *func,
                    args: args
                        .iter()
                        .map(|arg| expand_simple_definitions(arg, definitions, visiting))
                        .collect(),
                },
                // Other expression forms either have no child expression or produce
                // series/reference values and therefore cannot pass the risk
                // command's `simple`-qualifier check. Preserve them defensively; an
                // earlier diagnostic prevents an invalid program from reaching MIR.
                _ => expr.kind.clone(),
            };
            expanded
        }
    }

    fn hir_of(source: &str) -> Option<HirProgram> {
        let parsed = pinewasm_parse::parse(source);
        if has_errors(&parsed.diagnostics) {
            return None;
        }
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        (!has_errors(&analysis.diagnostics)).then_some(analysis.hir)
    }

    fn must_analyze(source: &str) -> HirProgram {
        hir_of(source).unwrap_or_else(|| panic!("the source compiles:\n{source}"))
    }

    fn followed_count(hir: &HirProgram) -> usize {
        match RiskSymbols::of(hir).expect("the program has a risk directive") {
            RiskSymbols::All => unreachable!("`of` narrows"),
            RiskSymbols::Only(followed) => followed.iter().filter(|f| **f).count(),
        }
    }

    /// A value of the expressions the generator and the fixtures write.
    #[derive(Clone, Copy, Debug)]
    enum Scalar {
        Float(f64),
        Bool(bool),
    }

    impl PartialEq for Scalar {
        fn eq(&self, other: &Self) -> bool {
            match (self, other) {
                (Scalar::Float(a), Scalar::Float(b)) => a.to_bits() == b.to_bits(),
                (Scalar::Bool(a), Scalar::Bool(b)) => a == b,
                _ => false,
            }
        }
    }

    /// `expr` on the input values `inputs` (by input id) and the variable
    /// values `vars`, or `None` when it reads something else.
    fn eval(expr: &HirExpr, inputs: &[Scalar], vars: &HashMap<SymbolId, Scalar>) -> Option<Scalar> {
        let float = |expr: &HirExpr| match eval(expr, inputs, vars)? {
            Scalar::Float(value) => Some(value),
            Scalar::Bool(_) => None,
        };
        let bool = |expr: &HirExpr| match eval(expr, inputs, vars)? {
            Scalar::Bool(value) => Some(value),
            Scalar::Float(_) => None,
        };
        Some(match &expr.kind {
            HirExprKind::ConstFloat(value) => Scalar::Float(*value),
            HirExprKind::ConstInt(value) => Scalar::Float(*value as f64),
            HirExprKind::ConstBool(value) => Scalar::Bool(*value),
            HirExprKind::ReadInput { input } => *inputs.get(*input as usize)?,
            HirExprKind::ReadVar { symbol, offset: 0 } => *vars.get(symbol)?,
            HirExprKind::IntToFloat(inner) => Scalar::Float(float(inner)?),
            HirExprKind::Unary { op, operand } => match op {
                UnOp::Neg => Scalar::Float(-float(operand)?),
                UnOp::Not => Scalar::Bool(!bool(operand)?),
            },
            HirExprKind::Binary { op, lhs, rhs } => match op {
                BinOp::Add => Scalar::Float(float(lhs)? + float(rhs)?),
                BinOp::Sub => Scalar::Float(float(lhs)? - float(rhs)?),
                BinOp::Mul => Scalar::Float(float(lhs)? * float(rhs)?),
                BinOp::Gt => Scalar::Bool(float(lhs)? > float(rhs)?),
                BinOp::And => Scalar::Bool(bool(lhs)? && bool(rhs)?),
                BinOp::Or => Scalar::Bool(bool(lhs)? || bool(rhs)?),
                _ => return None,
            },
            HirExprKind::Ternary {
                cond,
                then_e,
                else_e,
            } => {
                if bool(cond)? {
                    eval(then_e, inputs, vars)?
                } else {
                    eval(else_e, inputs, vars)?
                }
            }
            HirExprKind::CallPure {
                func: PureFunc::MathMax,
                args,
            } => {
                let mut values = args.iter().map(float);
                let first = values.next()??;
                Scalar::Float(values.try_fold(first, |max, value| Some(max.max(value?)))?)
            }
            _ => return None,
        })
    }

    /// Run `stmts` on concrete values and append what each risk directive's
    /// operands hold when it is reached. Like the collection, it visits both
    /// branches of an `if`, each from the values before it, and continues
    /// with the branch the condition picks; a loop body is visited once and
    /// what it assigns is dropped.
    fn run(
        stmts: &[HirStmt],
        inputs: &[Scalar],
        vars: &mut HashMap<SymbolId, Scalar>,
        out: &mut Vec<Vec<Option<Scalar>>>,
    ) {
        for stmt in stmts {
            match stmt {
                HirStmt::Assign { symbol, value, .. } => match eval(value, inputs, vars) {
                    Some(value) => {
                        vars.insert(*symbol, value);
                    }
                    None => {
                        vars.remove(symbol);
                    }
                },
                HirStmt::Order { kind, operands, .. } if kind.is_risk() => {
                    out.push(operands.iter().map(|o| eval(o, inputs, vars)).collect());
                }
                HirStmt::If {
                    cond,
                    then_branch,
                    else_branch,
                    ..
                } => {
                    let taken = eval(cond, inputs, vars);
                    let mut then_vars = vars.clone();
                    run(then_branch, inputs, &mut then_vars, out);
                    let mut else_vars = vars.clone();
                    run(else_branch, inputs, &mut else_vars, out);
                    // A symbol only one branch holds was declared in it.
                    then_vars.retain(|symbol, _| else_vars.contains_key(symbol));
                    *vars = match taken {
                        Some(Scalar::Bool(true)) => then_vars,
                        Some(Scalar::Bool(false)) => {
                            else_vars.retain(|symbol, _| then_vars.contains_key(symbol));
                            else_vars
                        }
                        _ => HashMap::new(),
                    };
                }
                HirStmt::For { body, .. } | HirStmt::While { body, .. } => {
                    run(body, inputs, &mut vars.clone(), out);
                }
                _ => {}
            }
        }
    }

    /// The operand values of each directive when the program runs on
    /// `inputs`.
    fn ran(hir: &HirProgram, inputs: &[Scalar]) -> Vec<Vec<Option<Scalar>>> {
        let mut out = Vec::new();
        run(&hir.body, inputs, &mut HashMap::new(), &mut out);
        out
    }

    /// The operand values of each hoisted directive, evaluated as the
    /// prelude does: from the inputs alone, before any variable is assigned.
    fn hoisted_values(directives: &[HirStmt], inputs: &[Scalar]) -> Vec<Vec<Option<Scalar>>> {
        directives
            .iter()
            .map(|directive| match directive {
                HirStmt::Order { operands, .. } => operands
                    .iter()
                    .map(|operand| eval(operand, inputs, &HashMap::new()))
                    .collect(),
                _ => unreachable!("a hoisted risk directive is an Order"),
            })
            .collect()
    }

    /// The single operand of the single directive of `source`, hoisted and
    /// evaluated on `inputs`, after checking it is what running `source`
    /// gives.
    fn hoisted_value(source: &str, inputs: &[Scalar]) -> Scalar {
        let hir = must_analyze(source);
        let hoisted = collect_hoisted_risk_directives(&hir);
        assert!(hoisted.oversized.is_empty(), "{source}");
        let values = hoisted_values(&hoisted.directives, inputs);
        assert_eq!(values, ran(&hir, inputs), "{source}");
        assert_eq!(hoisted.directives, unnarrowed(&hir), "{source}");
        match values.as_slice() {
            [operands] => operands[0].expect("the operand reads only inputs"),
            _ => panic!("one directive:\n{source}"),
        }
    }

    const HEADER: &str =
        "//@version=6\nstrategy('Risk')\nseed = input.float(1.5)\nflag = input.bool(true)\n";
    const SEED_ON: [Scalar; 2] = [Scalar::Float(1.5), Scalar::Bool(true)];
    const SEED_OFF: [Scalar; 2] = [Scalar::Float(1.5), Scalar::Bool(false)];

    #[test]
    fn a_program_without_directives_is_not_collected() {
        let hir = must_analyze(
            "//@version=6\nstrategy('None')\nlimit = input.float(2.0) * 3\nif close > open\n    strategy.entry('L', strategy.long, qty = limit)\n",
        );
        assert!(RiskSymbols::of(&hir).is_none());
        assert!(collect_hoisted_risk_directives(&hir).directives.is_empty());
        assert!(unnarrowed(&hir).is_empty());
        assert!(lazy::reference(&hir).is_empty());
    }

    #[test]
    fn constant_operands_follow_no_symbol() {
        let hir = must_analyze(
            "//@version=6\nstrategy('Constant')\nscale = input.float(2.0)\nwide = scale * 4\nstrategy.risk.max_drawdown(25, strategy.percent_of_equity)\nstrategy.risk.max_cons_loss_days(3)\n",
        );
        assert_eq!(followed_count(&hir), 0);
        let directives = collect_hoisted_risk_directives(&hir).directives;
        assert_eq!(directives, unnarrowed(&hir));
        assert_eq!(directives, lazy::reference(&hir));
        assert_eq!(directives.len(), 2);
    }

    #[test]
    fn only_what_an_operand_reaches_is_followed() {
        let hir = must_analyze(
            "//@version=6\nstrategy('Reach')\nuse_small = input.bool(false)\nbase = input.float(2.0)\nunrelated = input.float(9.0) * 7\nother = unrelated + 1\nstep = base * 2\ncap = if use_small\n    step\nelse\n    step + 1\nstrategy.risk.max_position_size(cap)\nplot(other)\n",
        );
        let followed = followed_count(&hir);
        // `cap` and its hidden result slot, `step`, `base`, `use_small`:
        // never `unrelated` or `other`.
        assert!((4..=6).contains(&followed), "{followed} symbols followed");
        assert!(followed < hir.symbols.len());
        let directives = collect_hoisted_risk_directives(&hir).directives;
        assert_eq!(directives, unnarrowed(&hir));
        assert_eq!(directives, lazy::reference(&hir));
    }

    /// A float `NaN` constant makes a definition differ from its own copy.
    /// A branch that leaves the definition alone still holds the same
    /// allocation, so the merge does not take it for changed and the
    /// narrowing stays exact.
    #[test]
    fn a_nan_constant_does_not_widen_what_is_followed() {
        let mut hir = must_analyze(
            "//@version=6\nstrategy('NaN')\nbase = input.float(2.0)\ncap = base * 3.5\nunrelated = input.float(9.0)\nother = unrelated + base\nif input.bool(true)\n    strategy.risk.max_position_size(cap)\n",
        );
        fn poison(expr: &mut HirExpr) -> bool {
            match &mut expr.kind {
                HirExprKind::ConstFloat(value) => {
                    *value = f64::NAN;
                    true
                }
                HirExprKind::Binary { lhs, rhs, .. } => poison(lhs) || poison(rhs),
                HirExprKind::IntToFloat(inner) => poison(inner),
                _ => false,
            }
        }
        let poisoned = hir.body.iter_mut().any(|stmt| match stmt {
            HirStmt::Assign { value, .. } => poison(value),
            _ => false,
        });
        assert!(poisoned, "the fixture assigns a float constant");
        assert!(followed_count(&hir) < hir.symbols.len());
        // `NaN != NaN`, so compare the collections through their debug form.
        let directives = format!("{:?}", collect_hoisted_risk_directives(&hir).directives);
        assert_eq!(directives, format!("{:?}", unnarrowed(&hir)));
        assert!(!directives.contains("Ternary"), "{directives}");
    }

    /// `x` holds the value `a` had when `x` was assigned. Substituting when
    /// the directive reads `x` gave it the value `a` was reassigned to.
    #[test]
    fn a_definition_keeps_the_value_it_had_when_assigned() {
        let source =
            format!("{HEADER}a = seed\nx = a\na := 5.0\nstrategy.risk.max_position_size(x)\n");
        assert_eq!(hoisted_value(&source, &SEED_ON), Scalar::Float(1.5));
        let lazy = lazy::reference(&must_analyze(&source));
        assert_eq!(
            hoisted_values(&lazy, &SEED_ON),
            [[Some(Scalar::Float(5.0))]]
        );
    }

    /// The condition of an `if` is evaluated before its branches, so a
    /// branch that reassigns what the condition reads does not change it.
    #[test]
    fn a_merge_reads_its_condition_before_the_branches() {
        let source = format!(
            "{HEADER}x = seed\nif x > 1.0\n    x := 0.5\nstrategy.risk.max_position_size(x)\n"
        );
        assert_eq!(hoisted_value(&source, &SEED_ON), Scalar::Float(0.5));
        let small = [Scalar::Float(0.75), Scalar::Bool(true)];
        assert_eq!(hoisted_value(&source, &small), Scalar::Float(0.75));
    }

    fn cycle(variables: usize) -> String {
        let mut source = String::from(HEADER);
        for i in 0..variables {
            source.push_str(&format!("v{i} = seed\n"));
        }
        for i in 0..variables {
            let others = (0..variables)
                .filter(|j| *j != i)
                .map(|j| format!("v{j}"))
                .collect::<Vec<_>>();
            source.push_str(&format!("v{i} := {}\n", others.join(" + ")));
        }
        source
    }

    /// Reassignments that read each other close a cycle in the definitions
    /// as written. Substituting when the directive reads one followed every
    /// simple path through the cycle: `variables!` of them.
    #[test]
    fn a_cycle_of_reassignments_is_substituted_once() {
        // v0 = 3s, v1 = 3s + 2s, v2 = 3s + 5s + s, v3 = 3s + 5s + 9s.
        let small = format!("{}strategy.risk.max_position_size(v3)\n", cycle(4));
        assert_eq!(hoisted_value(&small, &SEED_ON), Scalar::Float(17.0 * 1.5));

        // Each variable doubles the one before it, so only the first few
        // fit; `v0` reads the declarations alone.
        let wide = format!("{}strategy.risk.max_position_size(v0)\n", cycle(40));
        assert_eq!(hoisted_value(&wide, &SEED_ON), Scalar::Float(39.0 * 1.5));
    }

    fn self_reading_ifs(ifs: usize) -> String {
        let mut source = format!("{HEADER}x = seed\n");
        for _ in 0..ifs {
            source.push_str("if flag\n    x := x + seed\n");
        }
        source.push_str("strategy.risk.max_position_size(x)\n");
        source
    }

    /// A reassignment that reads its own symbol adds to the value before
    /// it. Substituting at the merge left the read in the merged definition,
    /// and every later merge inlined that definition into itself.
    #[test]
    fn a_self_reading_reassignment_reads_the_value_before_it() {
        let source = self_reading_ifs(9);
        assert_eq!(hoisted_value(&source, &SEED_ON), Scalar::Float(15.0));
        assert_eq!(hoisted_value(&source, &SEED_OFF), Scalar::Float(1.5));

        let nested = format!(
            "{HEADER}x = seed\nif flag\n    x := x + seed\n    if flag\n        x := x + x\n        if not flag\n            x := x * 0.5\nstrategy.risk.max_position_size(x)\n"
        );
        assert_eq!(hoisted_value(&nested, &SEED_ON), Scalar::Float(6.0));
        assert_eq!(hoisted_value(&nested, &SEED_OFF), Scalar::Float(1.5));
    }

    /// A reassigned `var` variable is a series, so the front end rejects a
    /// directive that reads it and no definition has to carry a value over
    /// from an earlier execution. One that is only declared is its
    /// initializer.
    #[test]
    fn a_persistent_symbol_is_followed_only_while_it_is_not_reassigned() {
        assert!(hir_of(&format!(
            "{HEADER}var float x = seed\nx := x + 1.0\nstrategy.risk.max_position_size(x)\n"
        ))
        .is_none());
        let declared =
            format!("{HEADER}var float x = seed * 2.0\nstrategy.risk.max_position_size(x)\n");
        assert_eq!(hoisted_value(&declared, &SEED_ON), Scalar::Float(3.0));
    }

    fn codes_of(diagnostics: &[Diagnostic]) -> Vec<&str> {
        diagnostics
            .iter()
            .filter_map(|diagnostic| diagnostic.code.as_deref())
            .collect()
    }

    /// The merge after each `if` holds the value of both paths, so ten of
    /// them in a row are past the node limit. The directive is reported at
    /// its operand and kept as written.
    #[test]
    fn an_operand_over_the_node_limit_is_reported() {
        let source = self_reading_ifs(10);
        let hir = must_analyze(&source);
        let (body, diagnostics) = lower_with_diagnostics(&hir);
        assert_eq!(codes_of(&diagnostics), [codes::E0251]);
        let span = diagnostics[0].primary_span().expect("a labelled operand");
        assert_eq!(&source[span.start as usize..span.end as usize], "x");
        assert!(diagnostics[0]
            .message
            .contains("strategy.risk.max_position_size"));
        assert_eq!(body, lower(&hir));

        let hoisted = collect_hoisted_risk_directives(&hir);
        let [HirStmt::Order { operands, .. }] = hoisted.directives.as_slice() else {
            panic!("one directive");
        };
        assert!(matches!(operands[0].kind, HirExprKind::ReadVar { .. }));

        // Far past the limit costs no more than reaching it.
        for source in [self_reading_ifs(400), {
            let mut doubling = format!("{HEADER}x = seed\n");
            doubling.push_str(&"x := x + x\n".repeat(400));
            doubling + "strategy.risk.max_position_size(x)\n"
        }] {
            let (_, diagnostics) = lower_with_diagnostics(&must_analyze(&source));
            assert_eq!(codes_of(&diagnostics), [codes::E0251]);
        }
    }

    /// A definition over the limit is an error only where a directive reads
    /// it.
    #[test]
    fn an_oversized_definition_no_directive_reads_is_not_reported() {
        let mut source = format!("{HEADER}x = seed\nstrategy.risk.max_position_size(x)\n");
        source.push_str(&"x := x + x\n".repeat(40));
        source.push_str("y = seed * 2.0\nstrategy.risk.max_position_size(y)\nplot(x)\n");
        let hir = must_analyze(&source);
        assert!(lower_with_diagnostics(&hir).1.is_empty());
        let directives = collect_hoisted_risk_directives(&hir).directives;
        assert_eq!(
            hoisted_values(&directives, &SEED_ON),
            [[Some(Scalar::Float(1.5))], [Some(Scalar::Float(3.0))]]
        );
    }

    /// Every assignment stores its own copy, so many definitions that each
    /// fit are bounded together.
    #[test]
    fn the_definitions_of_one_program_share_a_budget() {
        let wide = (0..100).map(|_| "seed").collect::<Vec<_>>().join(" + ");
        let program = |definitions: usize| {
            let mut source = format!("{HEADER}x = seed\n");
            for _ in 0..definitions {
                source.push_str(&format!("x := {wide}\n"));
            }
            must_analyze(&(source + "strategy.risk.max_position_size(x)\n"))
        };
        // 199 nodes each.
        let fits = MAX_RISK_HOISTED_NODES / 199 - 1;
        assert!(lower_with_diagnostics(&program(fits)).1.is_empty());
        let over = lower_with_diagnostics(&program(fits + 2)).1;
        assert_eq!(codes_of(&over), [codes::E0251]);
    }

    fn chain(links: usize) -> String {
        let mut source = format!("{HEADER}v0 = seed\n");
        for i in 1..=links {
            source.push_str(&format!("v{i} = v{} + 1.0\n", i - 1));
        }
        source + &format!("strategy.risk.max_position_size(v{links})\n")
    }

    #[test]
    fn an_operand_over_the_depth_limit_is_reported() {
        // `links` additions over one read: `links + 1` levels.
        let fits = must_analyze(&chain(MAX_RISK_OPERAND_DEPTH - 1));
        assert!(lower_with_diagnostics(&fits).1.is_empty());
        let deep = must_analyze(&chain(MAX_RISK_OPERAND_DEPTH));
        assert_eq!(codes_of(&lower_with_diagnostics(&deep).1), [codes::E0251]);
    }

    /// The Pine programs under tests/compile-pass, tests/regression and
    /// benchmarks/workloads that analyze without errors.
    fn corpus() -> Vec<HirProgram> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut files = Vec::new();
        for dir in [
            "tests/compile-pass",
            "tests/regression",
            "benchmarks/workloads",
        ] {
            for entry in std::fs::read_dir(root.join(dir)).expect("test program directory") {
                let path = entry.expect("test program entry").path();
                if path.extension().is_some_and(|ext| ext == "pine") {
                    files.push(path);
                }
            }
        }
        files.sort();
        files
            .into_iter()
            .filter_map(|path| hir_of(&std::fs::read_to_string(path).expect("test program file")))
            .collect()
    }

    #[test]
    fn corpus_programs_collect_identically() {
        let programs = corpus();
        assert!(programs.len() >= 40, "{} test programs", programs.len());
        let mut with_directives = 0;
        for hir in &programs {
            let hoisted = collect_hoisted_risk_directives(hir);
            assert!(hoisted.oversized.is_empty());
            assert_eq!(hoisted.directives, unnarrowed(hir));
            assert_eq!(hoisted.directives, lazy::reference(hir));
            with_directives += usize::from(!hoisted.directives.is_empty());
        }
        assert!(
            with_directives >= 1,
            "the test programs include a risk directive"
        );
    }

    /// SplitMix64: a fixed stream per seed, with no dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound as u64) as usize
        }
    }

    /// A generated strategy: `simple` definitions chained through inputs,
    /// reassignments that read any variable in scope (their own included),
    /// `if` expressions and statements under `simple` conditions, loops, and
    /// risk directives reading them from any depth.
    struct Generator {
        rng: Rng,
        source: String,
        /// The float variables in scope, innermost last, each with a bound
        /// on the size of its definition once every read in it is expanded.
        vars: Vec<(String, usize)>,
        /// The expanded size of the expression being written.
        size: usize,
        /// Whether to write reassignments. Without them every value is
        /// assigned once, and substituting at the assignment or at the
        /// directive gives the same directives.
        reassign: bool,
        next: usize,
    }

    /// The largest expanded definition a variable may have and still be
    /// read. A substitution copies definitions into definitions, so without
    /// a bound a chain of declarations doubles it at every link and the
    /// sweep would mostly write operands over [`MAX_RISK_OPERAND_NODES`].
    const MAX_EXPANSION: usize = 200;

    /// The size a statement `if` condition adds to a definition its
    /// branches change: a flag, two flags, or a comparison of a variable no
    /// larger than this.
    const MAX_CONDITION: usize = 16;

    impl Generator {
        fn cond(&mut self) -> String {
            match self.rng.below(5) {
                0 => "flag0".to_string(),
                1 => "flag1".to_string(),
                2 => "not flag0".to_string(),
                3 => format!("{} > 2.0", self.value(1)),
                _ => "(flag0 and flag1)".to_string(),
            }
        }

        /// The condition of a statement `if`: the merge after it puts this
        /// condition into every definition a branch changed.
        fn branch_cond(&mut self) -> String {
            let small = self
                .vars
                .iter()
                .filter(|(_, size)| *size + 2 <= MAX_CONDITION)
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>();
            match self.rng.below(5) {
                0 => "flag0".to_string(),
                1 => "flag1".to_string(),
                2 => "not flag0".to_string(),
                3 if !small.is_empty() => {
                    format!("{} > 2.0", small[self.rng.below(small.len())])
                }
                _ => "(flag0 and flag1)".to_string(),
            }
        }

        fn value(&mut self, depth: usize) -> String {
            if depth == 0 || self.rng.below(3) == 0 {
                if !self.vars.is_empty() && self.rng.below(4) != 0 {
                    let pick = self.rng.below(self.vars.len());
                    let (name, size) = self.vars[pick].clone();
                    if self.size + size <= MAX_EXPANSION {
                        self.size += size;
                        return name;
                    }
                }
                self.size += 1;
                return ["1.0", "2.5", "seed0", "seed1"][self.rng.below(4)].to_string();
            }
            match self.rng.below(4) {
                0 => format!("({} + {})", self.value(depth - 1), self.value(depth - 1)),
                1 => format!("({} * 0.5)", self.value(depth - 1)),
                2 => format!(
                    "({} ? {} : {})",
                    self.cond(),
                    self.value(depth - 1),
                    self.value(depth - 1)
                ),
                _ => format!(
                    "math.max({}, {})",
                    self.value(depth - 1),
                    self.value(depth - 1)
                ),
            }
        }

        /// The size written since the last call, and at least one.
        fn written(&mut self) -> usize {
            std::mem::take(&mut self.size).max(1)
        }

        fn line(&mut self, indent: usize, text: &str) {
            for _ in 0..indent {
                self.source.push_str("    ");
            }
            self.source.push_str(text);
            self.source.push('\n');
        }

        /// `ifs` is the number of statement `if`s around the block, and
        /// `looped` whether a loop is: a variable reassigned in a loop is a
        /// series, which no directive may read.
        fn block(&mut self, indent: usize, ifs: usize, looped: bool, statements: usize) {
            let scope = self.vars.len();
            for _ in 0..statements {
                match self.rng.below(9) {
                    0 | 1 => {
                        let name = format!("v{}", self.next);
                        self.next += 1;
                        let value = self.value(2);
                        self.line(indent, &format!("{name} = {value}"));
                        let size = self.written();
                        self.vars.push((name, size));
                    }
                    2 => {
                        let name = format!("v{}", self.next);
                        self.next += 1;
                        let (cond, then, other) = (self.cond(), self.value(1), self.value(1));
                        self.line(indent, &format!("{name} = if {cond}"));
                        self.line(indent + 1, &then);
                        self.line(indent, "else");
                        self.line(indent + 1, &other);
                        let size = self.written();
                        self.vars.push((name, size));
                    }
                    3 if self.reassign && !looped && !self.vars.is_empty() => {
                        let pick = self.rng.below(self.vars.len());
                        let (name, before) = self.vars[pick].clone();
                        let value = self.value(2);
                        self.line(indent, &format!("{name} := {value}"));
                        // Each `if` around the reassignment merges it with
                        // the value before it, under that `if`'s condition.
                        let size = self.written() + ifs * (before + MAX_CONDITION + 1);
                        self.vars[pick].1 = size;
                    }
                    4 | 5 if indent < 3 => {
                        let cond = self.branch_cond();
                        self.line(indent, &format!("if {cond}"));
                        let inner = 1 + self.rng.below(3);
                        self.block(indent + 1, ifs + 1, looped, inner);
                        if self.rng.below(2) == 0 {
                            self.line(indent, "else");
                            let inner = 1 + self.rng.below(3);
                            self.block(indent + 1, ifs + 1, looped, inner);
                        }
                    }
                    6 if indent < 3 => {
                        let counter = format!("k{}", self.next);
                        self.next += 1;
                        self.line(indent, &format!("for {counter} = 0 to 2"));
                        let inner = 1 + self.rng.below(2);
                        self.block(indent + 1, ifs, true, inner);
                    }
                    _ => {
                        let value = self.value(2);
                        self.size = 0;
                        let directive = if self.rng.below(2) == 0 {
                            format!("strategy.risk.max_position_size({value})")
                        } else {
                            format!(
                                "strategy.risk.max_drawdown({value}, strategy.percent_of_equity)"
                            )
                        };
                        self.line(indent, &directive);
                    }
                }
            }
            self.vars.truncate(scope);
        }
    }

    fn generated(seed: u64, reassign: bool) -> String {
        let mut generator = Generator {
            rng: Rng(seed),
            source: String::from(
                "//@version=6\nstrategy('Generated')\nseed0 = input.float(1.5)\nseed1 = input.float(4.0)\nflag0 = input.bool(false)\nflag1 = input.bool(true)\n",
            ),
            vars: Vec::new(),
            size: 0,
            reassign,
            next: 0,
        };
        let statements = 4 + generator.rng.below(20);
        generator.block(0, 0, false, statements);
        generator.source
    }

    /// Input values for a generated strategy: both seeds, then every
    /// combination of the two flags.
    fn generated_inputs() -> impl Iterator<Item = [Scalar; 4]> {
        (0..4).map(|flags| {
            [
                Scalar::Float(1.5 + flags as f64),
                Scalar::Float(4.0 - flags as f64),
                Scalar::Bool(flags & 1 != 0),
                Scalar::Bool(flags & 2 != 0),
            ]
        })
    }

    /// What one sweep of generated strategies covered.
    #[derive(Default)]
    struct Sweep {
        compiled: usize,
        with_directives: usize,
        narrowed: usize,
        reassigning: usize,
        /// Operand values compared with a run of the program.
        values: usize,
        oversized: usize,
    }

    fn sweep(seeds: std::ops::Range<u64>, reassign: bool) -> Sweep {
        let mut sweep = Sweep::default();
        for seed in seeds {
            let source = generated(seed, reassign);
            let Some(hir) = hir_of(&source) else {
                continue;
            };
            sweep.compiled += 1;
            let hoisted = collect_hoisted_risk_directives(&hir);
            assert_eq!(hoisted.directives, unnarrowed(&hir), "seed {seed}");
            if !reassign {
                assert_eq!(hoisted.directives, lazy::reference(&hir), "seed {seed}");
            }
            if hoisted.directives.is_empty() {
                continue;
            }
            sweep.with_directives += 1;
            sweep.narrowed += usize::from(followed_count(&hir) < hir.symbols.len());
            sweep.reassigning += usize::from(source.contains(":="));
            if !hoisted.oversized.is_empty() {
                sweep.oversized += 1;
                continue;
            }
            for inputs in generated_inputs() {
                let values = hoisted_values(&hoisted.directives, &inputs);
                assert_eq!(values, ran(&hir, &inputs), "seed {seed}:\n{source}");
                sweep.values += values.iter().flatten().flatten().count();
            }
        }
        sweep
    }

    #[test]
    fn generated_strategies_collect_identically() {
        let swept = sweep(0..1500, true);
        // The sweep is only evidence while the front end accepts what it
        // writes, the directives read variables, and the variables are
        // reassigned.
        assert!(swept.compiled >= 500, "{} compiled", swept.compiled);
        assert!(
            swept.with_directives >= 300,
            "{} generated strategies had a directive",
            swept.with_directives
        );
        assert!(swept.narrowed >= 200, "{} narrowed", swept.narrowed);
        assert!(swept.reassigning >= 200, "{} reassigned", swept.reassigning);
        assert!(swept.values >= 2000, "{} values compared", swept.values);
        assert!(
            swept.oversized * 20 <= swept.with_directives,
            "{} of {} over a limit",
            swept.oversized,
            swept.with_directives
        );
    }

    /// Without reassignments every value is assigned once, so the directives
    /// are also the ones the collection found when it substituted at the
    /// directive.
    #[test]
    fn generated_single_assignment_strategies_match_the_lazy_collection() {
        let swept = sweep(0..600, false);
        assert!(swept.compiled >= 200, "{} compiled", swept.compiled);
        assert!(
            swept.with_directives >= 150,
            "{} generated strategies had a directive",
            swept.with_directives
        );
        assert_eq!(swept.reassigning, 0);
        assert_eq!(swept.oversized, 0);
    }
}
