//! Up/down colorer targets are source bindings, not static origins.
//! VM lanes remain unchanged even when a fixed plot role uses static paint.
use crate::arrow_static_colors::{writes, Environment, Scalar};
use crate::*;
use pinewasm_hir::ArrowRuntimeColorers;

#[derive(Clone, Default)]
struct Flow {
    values: Environment,
    /// Globally mutable persistent dependencies survive ordinary aliases, but
    /// affect paint eligibility only when crossing an inlined argument boundary.
    origins: HashMap<SymbolId, HashSet<SymbolId>>,
}

impl Analyzer<'_> {
    fn arrow_colorer_operands<'a>(&self, expr: &'a HirExpr, flow: &Flow) -> Vec<&'a HirExpr> {
        match &expr.kind {
            HirExprKind::Unary { operand, .. } => vec![operand],
            HirExprKind::Binary {
                op: BinOp::And | BinOp::Or,
                lhs,
                rhs,
            } => {
                if matches!(
                    self.arrow_scalar(expr, &flow.values),
                    Scalar::Value(InputValue::Bool(_))
                ) {
                    Vec::new()
                } else {
                    vec![lhs, rhs]
                }
            }
            HirExprKind::Binary { lhs, rhs, .. } => vec![lhs, rhs],
            HirExprKind::Ternary {
                cond,
                then_e,
                else_e,
            } => match self.arrow_scalar(cond, &flow.values) {
                Scalar::Value(InputValue::Bool(true)) => vec![cond, then_e],
                Scalar::Value(InputValue::Bool(false)) => vec![cond, else_e],
                _ => vec![cond, then_e, else_e],
            },
            HirExprKind::IntToFloat(expr)
            | HirExprKind::MaterializeHistory { expr, .. }
            | HirExprKind::MaterializeHistoryDynamic { expr, .. } => vec![expr],
            HirExprKind::CallPure { args, .. } | HirExprKind::Tuple { elements: args } => {
                args.iter().collect()
            }
            _ => Vec::new(),
        }
    }

    fn arrow_colorer_origins(
        &self,
        expr: &HirExpr,
        flow: &Flow,
        mutable: &HashSet<SymbolId>,
    ) -> HashSet<SymbolId> {
        if let HirExprKind::ReadVar { symbol, .. } | HirExprKind::ReadVarDynamic { symbol, .. } =
            &expr.kind
        {
            if let Some(call) = self.rendering_inline.get_call(*symbol) {
                let mut origins = HashSet::new();
                for parameter in &call.parameters {
                    origins.extend(flow.origins.get(parameter).into_iter().flatten().copied());
                }
                if let Some(result) = &call.result {
                    origins.extend(self.arrow_colorer_origins(result, flow, mutable));
                }
                return origins;
            }
            let mut result = flow.origins.get(symbol).cloned().unwrap_or_default();
            if mutable.contains(symbol) {
                result.insert(*symbol);
            }
            return result;
        }
        self.arrow_colorer_operands(expr, flow)
            .into_iter()
            .flat_map(|operand| self.arrow_colorer_origins(operand, flow, mutable))
            .collect()
    }

    fn arrow_mutable_inline_result(
        &self,
        expr: &HirExpr,
        flow: &Flow,
        mutable: &HashSet<SymbolId>,
    ) -> bool {
        (matches!(&expr.kind, HirExprKind::ReadVar { symbol, offset: 0 }
            if self.rendering_inline.get_call(*symbol).is_some())
            && !self.arrow_colorer_origins(expr, flow, mutable).is_empty())
            || self
                .arrow_colorer_operands(expr, flow)
                .into_iter()
                .any(|operand| self.arrow_mutable_inline_result(operand, flow, mutable))
    }

    fn arrow_colorer_scalar(
        &self,
        expr: &HirExpr,
        flow: &Flow,
        mutable: &HashSet<SymbolId>,
    ) -> Scalar {
        if self.arrow_mutable_inline_result(expr, flow, mutable) {
            Scalar::Dynamic
        } else {
            self.arrow_scalar(expr, &flow.values)
        }
    }

    fn arrow_colorer_body(
        &mut self,
        body: &[HirStmt],
        flow: &mut Flow,
        mutable: &HashSet<SymbolId>,
        changed: &mut HashSet<SymbolId>,
        collect_only: bool,
    ) {
        for stmt in body {
            let shadow = self.rendering_inline.statement(stmt);
            let stmt = shadow.as_ref().unwrap_or(stmt);
            match stmt {
                HirStmt::Assign {
                    symbol,
                    value,
                    init,
                    span,
                } => {
                    let value = self.rendering_inline.expression(
                        rendering_inline_provenance::Anchor::Assignment(*symbol, *span),
                        value,
                    );
                    if !init {
                        changed.insert(*symbol);
                    }
                    let origins = self.arrow_colorer_origins(&value, flow, mutable);
                    let persistent = self.symbols[symbol.0 as usize].persistent;
                    let dynamic = (persistent && !init)
                        || (self.arrow_inline_parameters.contains(symbol) && !origins.is_empty())
                        || (persistent && value.ty != Type::Color && mutable.contains(symbol));
                    let scalar = if dynamic {
                        Scalar::Dynamic
                    } else {
                        self.arrow_colorer_scalar(&value, flow, mutable)
                    };
                    flow.values.insert(*symbol, scalar);
                    flow.origins.insert(*symbol, origins);
                }
                HirStmt::Destructure { targets, value, .. } => {
                    if let HirExprKind::Tuple { elements } = &value.kind {
                        let values: Vec<_> = elements
                            .iter()
                            .map(|e| {
                                (
                                    self.arrow_colorer_scalar(e, flow, mutable),
                                    self.arrow_colorer_origins(e, flow, mutable),
                                )
                            })
                            .collect();
                        for (symbol, (scalar, origins)) in targets.iter().zip(values) {
                            flow.values.insert(*symbol, scalar);
                            flow.origins.insert(*symbol, origins);
                        }
                    } else {
                        for symbol in targets {
                            flow.values.insert(*symbol, Scalar::Dynamic);
                        }
                    }
                }
                HirStmt::Plot { slot, colors, .. }
                    if !collect_only
                        && self.plots[slot.0 as usize].style.kind
                            == OutputDisplayKind::PlotArrow =>
                {
                    let mut binding = ArrowRuntimeColorers {
                        up: false,
                        down: false,
                    };
                    let mut unsupported = Vec::new();
                    for (role, expr) in colors {
                        let (field, target) = match role {
                            PlotColorRole::Up => ("colorup", &mut binding.up),
                            PlotColorRole::Down => ("colordown", &mut binding.down),
                            _ => continue,
                        };
                        let expr = self.rendering_inline.expression(
                            rendering_inline_provenance::Anchor::Plot(*slot, *role),
                            expr,
                        );
                        match self.arrow_colorer_scalar(&expr, flow, mutable) {
                            Scalar::Dynamic => *target = true,
                            Scalar::Value(_) | Scalar::Missing => {},
                            Scalar::Unknown => unsupported.push(UnsupportedVisualOption {
                                field: field.into(),
                                reason: "arrow runtime colorer cannot be proven from the effective callsite expression".into(),
                            }),
                        }
                    }
                    let style = &mut self.plots[slot.0 as usize].style;
                    if unsupported.is_empty() && style.arrow_static_colors.is_some() {
                        style.arrow_runtime_colorers = Some(binding);
                    }
                    style.unsupported.extend(unsupported);
                }
                HirStmt::If {
                    cond,
                    then_branch,
                    else_branch,
                    span,
                } => match self.arrow_colorer_scalar(
                    &self
                        .rendering_inline
                        .expression(rendering_inline_provenance::Anchor::Condition(*span), cond),
                    flow,
                    mutable,
                ) {
                    Scalar::Value(InputValue::Bool(true)) => {
                        self.arrow_colorer_body(then_branch, flow, mutable, changed, collect_only);
                    }
                    Scalar::Value(InputValue::Bool(false)) => {
                        self.arrow_colorer_body(else_branch, flow, mutable, changed, collect_only);
                    }
                    condition => {
                        let uncertain = !matches!(condition, Scalar::Dynamic);
                        let mut left = flow.clone();
                        let mut right = flow.clone();
                        let mut branch_writes = HashSet::new();
                        self.arrow_colorer_body(
                            then_branch,
                            &mut left,
                            mutable,
                            &mut branch_writes,
                            collect_only,
                        );
                        self.arrow_colorer_body(
                            else_branch,
                            &mut right,
                            mutable,
                            &mut branch_writes,
                            collect_only,
                        );
                        changed.extend(branch_writes.iter().copied());
                        for symbol in left.values.keys().chain(right.values.keys()) {
                            let scalar = match (left.values.get(symbol), right.values.get(symbol)) {
                                _ if branch_writes.contains(symbol)
                                    && (uncertain
                                        || left.values.get(symbol) == Some(&Scalar::Unknown)
                                        || right.values.get(symbol) == Some(&Scalar::Unknown)) =>
                                {
                                    Scalar::Unknown
                                }
                                // Branch-dependent writes make the colorer dynamic,
                                // even when both computed colors compare equal.
                                (Some(a), Some(b)) if a == b && !branch_writes.contains(symbol) => {
                                    a.clone()
                                }
                                _ => Scalar::Dynamic,
                            };
                            flow.values.insert(*symbol, scalar);
                            let mut origins = left.origins.get(symbol).cloned().unwrap_or_default();
                            origins
                                .extend(right.origins.get(symbol).into_iter().flatten().copied());
                            flow.origins.insert(*symbol, origins);
                        }
                    }
                },

                HirStmt::For { body, .. } | HirStmt::While { body, .. } => {
                    let mut candidates = HashSet::new();
                    writes(body, &mut candidates);
                    let mut inner = flow.clone();
                    for symbol in candidates {
                        inner.values.insert(symbol, Scalar::Dynamic);
                    }
                    let mut loop_writes = HashSet::new();
                    self.arrow_colorer_body(body, &mut inner, mutable, &mut loop_writes, true);
                    changed.extend(loop_writes.iter().copied());
                    for symbol in loop_writes {
                        let scalar = if inner.values.get(&symbol) == Some(&Scalar::Unknown) {
                            Scalar::Unknown
                        } else {
                            Scalar::Dynamic
                        };
                        flow.values.insert(symbol, scalar);
                        if let Some(origins) = inner.origins.remove(&symbol) {
                            flow.origins.entry(symbol).or_default().extend(origins);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    pub(crate) fn finalize_arrow_runtime_colorers(&mut self, body: &[HirStmt]) {
        if !self
            .plots
            .iter()
            .any(|plot| plot.style.kind == OutputDisplayKind::PlotArrow)
        {
            return;
        }
        let mutable = self.mutable_arrow_persistent_symbols(body);
        self.arrow_colorer_body(
            body,
            &mut Flow::default(),
            &mutable,
            &mut HashSet::new(),
            false,
        );
    }
}
