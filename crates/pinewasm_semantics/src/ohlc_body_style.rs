//! Source-qualified OHLC body properties are independent of runtime color lanes.
//! This preparation does not evaluate bars, change input defaults or rewrite HIR.
use crate::arrow_static_colors::{writes, Environment, Scalar};
use crate::*;
use pinewasm_hir::{OhlcBodyColor, OhlcBodyStyle};

#[derive(Clone, Debug, PartialEq, Eq)]
enum InputTrace {
    Direct(u32),
    Computed,
    Series,
}
impl InputTrace {
    fn computed(self) -> Self {
        if self == Self::Series {
            self
        } else {
            Self::Computed
        }
    }
}
/// Missing-value origin is separate from its scalar value and declared
/// qualifier: a bare-NA alias takes PineWASM's default blue, while an explicit
/// color(na) construction retains transparent black.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MissingColorOrigin {
    Literal,
    Alias,
    ExplicitCast,
    Unknown,
}

#[derive(Clone, Default)]
struct OhlcStaticFlow {
    values: Environment,
    missing: HashMap<SymbolId, MissingColorOrigin>,
    inputs: HashMap<SymbolId, Option<InputTrace>>,
    reassigned: HashSet<SymbolId>,
}

impl Analyzer<'_> {
    fn ohlc_missing_origin(&self, expr: &HirExpr, flow: &OhlcStaticFlow) -> MissingColorOrigin {
        match &expr.kind {
            HirExprKind::ConstNa => MissingColorOrigin::Literal,
            HirExprKind::ReadVar { symbol, offset: 0 } => {
                if let Some(call) = self.rendering_inline.get_call(*symbol) {
                    return call
                        .result
                        .as_ref()
                        .map_or(MissingColorOrigin::Unknown, |result| {
                            self.ohlc_missing_origin(result, flow)
                        });
                }
                flow.missing
                    .get(symbol)
                    .copied()
                    .unwrap_or(MissingColorOrigin::Unknown)
            }
            HirExprKind::CallPure {
                func: PureFunc::CastColor,
                args,
            } if matches!(args.as_slice(), [value] if matches!(value.kind, HirExprKind::ConstNa)) => {
                MissingColorOrigin::ExplicitCast
            }
            HirExprKind::Ternary {
                cond,
                then_e,
                else_e,
            } => match self.arrow_scalar(cond, &flow.values) {
                Scalar::Value(InputValue::Bool(true)) => self.ohlc_missing_origin(then_e, flow),
                Scalar::Value(InputValue::Bool(false)) => self.ohlc_missing_origin(else_e, flow),
                _ => MissingColorOrigin::Unknown,
            },
            _ => MissingColorOrigin::Unknown,
        }
    }
    fn ohlc_input_trace(&self, expr: &HirExpr, flow: &OhlcStaticFlow) -> Option<InputTrace> {
        if self.arrow_history_spans.contains(&expr.span) {
            return Some(InputTrace::Series);
        }
        let operands: Vec<&HirExpr> = match &expr.kind {
            HirExprKind::ReadInput { input } => {
                return Some(if expr.ty == Type::Color {
                    InputTrace::Direct(*input)
                } else {
                    InputTrace::Computed
                });
            }
            HirExprKind::ReadVar { symbol, offset: 0 } => {
                if let Some(call) = self.rendering_inline.get_call(*symbol) {
                    return call
                        .result
                        .as_ref()
                        .and_then(|result| self.ohlc_input_trace(result, flow));
                }
                return flow.inputs.get(symbol).cloned().flatten();
            }
            HirExprKind::ReadBuiltin { .. }
            | HirExprKind::ReadBuiltinDynamic { .. }
            | HirExprKind::ReadVar { .. }
            | HirExprKind::ReadVarDynamic { .. }
            | HirExprKind::MaterializeHistory { .. }
            | HirExprKind::MaterializeHistoryDynamic { .. } => {
                return Some(InputTrace::Series);
            }
            HirExprKind::IntToFloat(inner) => vec![inner],
            HirExprKind::Unary { operand, .. } => vec![operand],
            HirExprKind::Binary { lhs, rhs, .. } => vec![lhs, rhs],
            HirExprKind::CallPure { args, .. } | HirExprKind::Tuple { elements: args } => {
                args.iter().collect()
            }
            HirExprKind::Ternary {
                cond,
                then_e,
                else_e,
            } => match self.arrow_scalar(cond, &flow.values) {
                Scalar::Value(InputValue::Bool(true)) => {
                    return self.ohlc_input_trace(then_e, flow)
                }
                Scalar::Value(InputValue::Bool(false)) => {
                    return self.ohlc_input_trace(else_e, flow)
                }
                _ => vec![cond, then_e, else_e],
            },
            _ => Vec::new(),
        };
        let traces = operands
            .into_iter()
            .filter_map(|value| self.ohlc_input_trace(value, flow))
            .collect::<Vec<_>>();
        if traces.contains(&InputTrace::Series) {
            Some(InputTrace::Series)
        } else if !traces.is_empty() {
            Some(InputTrace::Computed)
        } else {
            None
        }
    }

    fn ohlc_static_record(
        &self,
        expr: &HirExpr,
        flow: &OhlcStaticFlow,
        kind: OutputDisplayKind,
    ) -> Result<OhlcBodyStyle, ()> {
        let mut record = OhlcBodyStyle {
            static_color_present: true,
            static_color: None,
            runtime_colorer: false,
        };
        record.static_color = Some(match self.arrow_scalar(expr, &flow.values) {
            Scalar::Value(InputValue::Color { r, g, b, t }) => OhlcBodyColor { r, g, b, t },
            Scalar::Missing => match self.ohlc_missing_origin(expr, flow) {
                MissingColorOrigin::Alias => OhlcBodyColor {
                    r: 41,
                    g: 98,
                    b: 255,
                    t: 0.0,
                },
                MissingColorOrigin::Literal | MissingColorOrigin::ExplicitCast => OhlcBodyColor {
                    r: 0,
                    g: 0,
                    b: 0,
                    t: 100.0,
                },
                MissingColorOrigin::Unknown => return Err(()),
            },
            Scalar::Dynamic => {
                if kind == OutputDisplayKind::PlotBar {
                    match self.ohlc_input_trace(expr, flow) {
                        Some(InputTrace::Direct(input)) => {
                            match self.inputs.get(input as usize).map(|decl| &decl.defval) {
                                Some(InputValue::Color { r, g, b, .. }) => OhlcBodyColor {
                                    r: *r,
                                    g: *g,
                                    b: *b,
                                    // The input's source constructor determines
                                    // its initial static property normalization.
                                    // Raw InputDecl.defval and VM lanes retain t.
                                    t: self
                                        .input_color_property_transparencies
                                        .get(&input)
                                        .copied()
                                        .ok_or(())?,
                                },
                                _ => return Err(()),
                            }
                        }
                        Some(InputTrace::Computed) => {
                            record.static_color_present = false;
                            return Ok(record);
                        }
                        _ => OhlcBodyColor {
                            r: 41,
                            g: 98,
                            b: 255,
                            t: 0.0,
                        },
                    }
                } else {
                    OhlcBodyColor {
                        r: 41,
                        g: 98,
                        b: 255,
                        t: 0.0,
                    }
                }
            }
            _ => return Err(()),
        });
        Ok(record)
    }
    fn ohlc_static_body(
        &mut self,
        body: &[HirStmt],
        env: &mut OhlcStaticFlow,
        mutable_persistent: &HashSet<SymbolId>,
        reachable_writes: &mut HashSet<SymbolId>,
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
                    let value_shadow = self.rendering_inline.expression(
                        rendering_inline_provenance::Anchor::Assignment(*symbol, *span),
                        value,
                    );
                    let value = &value_shadow;
                    if !init {
                        reachable_writes.insert(*symbol);
                    }
                    let scalar = if mutable_persistent.contains(symbol)
                        || (*init && self.ohlc_destructure_targets.contains(symbol))
                    {
                        Scalar::Dynamic
                    } else {
                        self.arrow_scalar(value, &env.values)
                    };
                    let mut input = self.ohlc_input_trace(value, env);
                    if input.is_some() {
                        if self.declared_qualifiers.get(symbol).is_some_and(|(q, _)| {
                            matches!(q, Qualifier::Simple | Qualifier::Series)
                        }) {
                            input = Some(InputTrace::Series);
                        } else if self.arrow_inline_parameters.contains(symbol)
                            || env.reassigned.contains(symbol)
                        {
                            input = input.map(InputTrace::computed);
                        }
                    }
                    if scalar == Scalar::Missing {
                        let origin = match self.ohlc_missing_origin(value, env) {
                            MissingColorOrigin::Literal => MissingColorOrigin::Alias,
                            origin => origin,
                        };
                        env.missing.insert(*symbol, origin);
                    } else {
                        env.missing.remove(symbol);
                    }
                    env.inputs.insert(*symbol, input);
                    env.values.insert(*symbol, scalar);
                }
                HirStmt::Destructure { targets, .. } => {
                    // Destructured tuple targets get a colorer
                    // even when this tuple component is a literal color.
                    for symbol in targets {
                        env.values.insert(*symbol, Scalar::Dynamic);
                        env.inputs.insert(*symbol, Some(InputTrace::Series));
                    }
                }
                HirStmt::Plot { slot, colors, .. }
                    if !collect_only
                        && matches!(
                            self.plots[slot.0 as usize].style.kind,
                            OutputDisplayKind::PlotCandle | OutputDisplayKind::PlotBar
                        ) =>
                {
                    let kind = self.plots[slot.0 as usize].style.kind;
                    let expression_shadow = colors
                        .iter()
                        .find(|(role, _)| *role == PlotColorRole::Color)
                        .map(|(_, e)| {
                            self.rendering_inline.expression(
                                rendering_inline_provenance::Anchor::Plot(
                                    *slot,
                                    PlotColorRole::Color,
                                ),
                                e,
                            )
                        });
                    let expression = expression_shadow.as_ref();
                    let result = expression.map_or_else(
                        || {
                            Ok(OhlcBodyStyle {
                                static_color_present: true,
                                static_color: None,
                                runtime_colorer: false,
                            })
                        },
                        |expr| self.ohlc_static_record(expr, env, kind),
                    );
                    let property_input = if kind == OutputDisplayKind::PlotBar {
                        expression.and_then(|expr| match self.ohlc_input_trace(expr, env) {
                            Some(InputTrace::Direct(input)) => Some(input),
                            _ => None,
                        })
                    } else {
                        None
                    };
                    let style = &mut self.plots[slot.0 as usize].style;
                    style
                        .bound_scalars
                        .retain(|binding| binding.field != "ohlc_body_color");
                    if result
                        .as_ref()
                        .is_ok_and(|body| body.static_color_present && body.static_color.is_some())
                    {
                        if let Some(input) = property_input {
                            style.bound_scalars.push(BoundStyleScalar {
                                field: "ohlc_body_color".into(),
                                expression: InputActiveExpr::Input { input },
                            });
                        }
                    }
                    match result {
                        Ok(record) => style.ohlc_body_style = Some(record),
                        Err(()) => {
                            style.ohlc_body_style = None;
                            style.unsupported.push(UnsupportedVisualOption {
                                field: "color".into(),
                                reason: "OHLC body property cannot be proven from the effective callsite expression".into(),
                            });
                        }
                    }
                }
                HirStmt::If {
                    cond,
                    then_branch,
                    else_branch,
                    span,
                } => match self.arrow_scalar(
                    &self
                        .rendering_inline
                        .expression(rendering_inline_provenance::Anchor::Condition(*span), cond),
                    &env.values,
                ) {
                    Scalar::Value(InputValue::Bool(true)) => self.ohlc_static_body(
                        then_branch,
                        env,
                        mutable_persistent,
                        reachable_writes,
                        collect_only,
                    ),
                    Scalar::Value(InputValue::Bool(false)) => self.ohlc_static_body(
                        else_branch,
                        env,
                        mutable_persistent,
                        reachable_writes,
                        collect_only,
                    ),
                    condition => {
                        let uncertain = !matches!(condition, Scalar::Dynamic);
                        let mut left = env.clone();
                        let mut right = env.clone();
                        let mut branch_writes = HashSet::new();
                        self.ohlc_static_body(
                            then_branch,
                            &mut left,
                            mutable_persistent,
                            &mut branch_writes,
                            collect_only,
                        );
                        self.ohlc_static_body(
                            else_branch,
                            &mut right,
                            mutable_persistent,
                            &mut branch_writes,
                            collect_only,
                        );
                        reachable_writes.extend(branch_writes.iter().copied());
                        for symbol in left.values.keys().chain(right.values.keys()) {
                            // A reachable conditional write makes the static color
                            // dynamic, even when both branch values agree.
                            let value = if branch_writes.contains(symbol) {
                                if uncertain
                                    || left.values.get(symbol) == Some(&Scalar::Unknown)
                                    || right.values.get(symbol) == Some(&Scalar::Unknown)
                                {
                                    Scalar::Unknown
                                } else {
                                    Scalar::Dynamic
                                }
                            } else {
                                match (left.values.get(symbol), right.values.get(symbol)) {
                                    (Some(a), Some(b)) if a == b => a.clone(),
                                    _ => Scalar::Dynamic,
                                }
                            };
                            let input = if branch_writes.contains(symbol) {
                                Some(InputTrace::Series)
                            } else if left.inputs.get(symbol) == right.inputs.get(symbol) {
                                left.inputs.get(symbol).cloned().flatten()
                            } else {
                                Some(InputTrace::Series)
                            };
                            let missing =
                                match (left.missing.get(symbol), right.missing.get(symbol)) {
                                    (Some(a), Some(b)) if a == b && value == Scalar::Missing => {
                                        Some(*a)
                                    }
                                    _ => None,
                                };
                            if let Some(origin) = missing {
                                env.missing.insert(*symbol, origin);
                            } else {
                                env.missing.remove(symbol);
                            }
                            env.inputs.insert(*symbol, input);
                            env.values.insert(*symbol, value);
                        }
                    }
                },

                HirStmt::For { body, .. } | HirStmt::While { body, .. } => {
                    let mut candidates = HashSet::new();
                    writes(body, &mut candidates);
                    // Loop-written conditions can change on later iterations.
                    // Preserve literal dead-branch pruning when discovering the
                    // effective writes; never freeze a mutable gate at entry.
                    let mut loop_env = env.clone();
                    for symbol in candidates {
                        loop_env.values.insert(symbol, Scalar::Dynamic);
                    }
                    let mut changed = HashSet::new();
                    self.ohlc_static_body(
                        body,
                        &mut loop_env,
                        mutable_persistent,
                        &mut changed,
                        true,
                    );
                    reachable_writes.extend(changed.iter().copied());
                    // Independent loop controls establish a static fallback
                    // even for an unconditional constant assignment in a loop.
                    for symbol in changed {
                        let value = if loop_env.values.get(&symbol) == Some(&Scalar::Unknown) {
                            Scalar::Unknown
                        } else {
                            Scalar::Dynamic
                        };
                        env.values.insert(symbol, value);
                        env.inputs.insert(symbol, Some(InputTrace::Series));
                    }
                }
                _ => {}
            }
        }
    }

    pub(crate) fn finalize_ohlc_body_style(&mut self, body: &[HirStmt]) {
        if !self.plots.iter().any(|plot| {
            matches!(
                plot.style.kind,
                OutputDisplayKind::PlotCandle | OutputDisplayKind::PlotBar
            )
        }) {
            return;
        }
        let mutable = self.mutable_arrow_persistent_symbols(body);
        let mut reassigned = HashSet::new();
        // Discover effective writes in source order. Literal-dead If branches
        // do not turn a direct input default into an absent OHLC body property;
        // independently qualified loop writes still participate.
        self.ohlc_static_body(
            body,
            &mut OhlcStaticFlow::default(),
            &mutable,
            &mut reassigned,
            true,
        );
        let mut static_flow = OhlcStaticFlow {
            reassigned,
            ..OhlcStaticFlow::default()
        };
        self.ohlc_static_body(body, &mut static_flow, &mutable, &mut HashSet::new(), false);
        self.ohlc_colorer_body(
            body,
            &mut OhlcRuntimeFlow::default(),
            &mutable,
            &mut HashSet::new(),
            false,
        );
    }
}

#[derive(Clone, Default)]
struct OhlcRuntimeFlow {
    values: Environment,
    /// Globally mutable persistent dependencies survive ordinary aliases, but
    /// affect paint eligibility only when crossing an inlined argument boundary.
    origins: HashMap<SymbolId, HashSet<SymbolId>>,
}

impl Analyzer<'_> {
    fn ohlc_colorer_operands<'a>(
        &self,
        expr: &'a HirExpr,
        flow: &OhlcRuntimeFlow,
    ) -> Vec<&'a HirExpr> {
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

    fn ohlc_colorer_origins(
        &self,
        expr: &HirExpr,
        flow: &OhlcRuntimeFlow,
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
                    origins.extend(self.ohlc_colorer_origins(result, flow, mutable));
                }
                return origins;
            }
            let mut result = flow.origins.get(symbol).cloned().unwrap_or_default();
            if mutable.contains(symbol) {
                result.insert(*symbol);
            }
            return result;
        }
        self.ohlc_colorer_operands(expr, flow)
            .into_iter()
            .flat_map(|operand| self.ohlc_colorer_origins(operand, flow, mutable))
            .collect()
    }

    fn ohlc_mutable_inline_result(
        &self,
        expr: &HirExpr,
        flow: &OhlcRuntimeFlow,
        mutable: &HashSet<SymbolId>,
    ) -> bool {
        (matches!(&expr.kind, HirExprKind::ReadVar { symbol, offset: 0 }
            if self.rendering_inline.get_call(*symbol).is_some())
            && !self.ohlc_colorer_origins(expr, flow, mutable).is_empty())
            || self
                .ohlc_colorer_operands(expr, flow)
                .into_iter()
                .any(|operand| self.ohlc_mutable_inline_result(operand, flow, mutable))
    }

    fn ohlc_colorer_scalar(
        &self,
        expr: &HirExpr,
        flow: &OhlcRuntimeFlow,
        mutable: &HashSet<SymbolId>,
    ) -> Scalar {
        if self.ohlc_mutable_inline_result(expr, flow, mutable) {
            Scalar::Dynamic
        } else {
            self.arrow_scalar(expr, &flow.values)
        }
    }

    fn ohlc_colorer_body(
        &mut self,
        body: &[HirStmt],
        flow: &mut OhlcRuntimeFlow,
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
                    let value_shadow = self.rendering_inline.expression(
                        rendering_inline_provenance::Anchor::Assignment(*symbol, *span),
                        value,
                    );
                    let value = &value_shadow;
                    if !init {
                        changed.insert(*symbol);
                    }
                    let origins = self.ohlc_colorer_origins(value, flow, mutable);
                    let persistent = self.symbols[symbol.0 as usize].persistent;
                    // A reachable bare-NA color reassignment gets a colorer, unlike
                    // an initial missing alias or color(na).
                    let bare_missing_write = !init
                        && self.symbols[symbol.0 as usize].ty == Type::Color
                        && matches!(value.kind, HirExprKind::ConstNa);
                    let dynamic = (*init && self.ohlc_destructure_targets.contains(symbol))
                        || (persistent && !init)
                        || bare_missing_write
                        || (self.arrow_inline_parameters.contains(symbol) && !origins.is_empty())
                        || (persistent && value.ty != Type::Color && mutable.contains(symbol));
                    let scalar = if dynamic {
                        Scalar::Dynamic
                    } else {
                        self.ohlc_colorer_scalar(value, flow, mutable)
                    };
                    flow.values.insert(*symbol, scalar);
                    flow.origins.insert(*symbol, origins);
                }
                HirStmt::Destructure { targets, value, .. } => {
                    let origins = self.ohlc_colorer_origins(value, flow, mutable);
                    for symbol in targets {
                        flow.values.insert(*symbol, Scalar::Dynamic);
                        flow.origins.insert(*symbol, origins.clone());
                    }
                }
                HirStmt::Plot { slot, colors, .. }
                    if !collect_only
                        && matches!(
                            self.plots[slot.0 as usize].style.kind,
                            OutputDisplayKind::PlotCandle | OutputDisplayKind::PlotBar
                        ) =>
                {
                    let expression_shadow = colors
                        .iter()
                        .find(|(role, _)| *role == PlotColorRole::Color)
                        .map(|(_, e)| {
                            self.rendering_inline.expression(
                                rendering_inline_provenance::Anchor::Plot(
                                    *slot,
                                    PlotColorRole::Color,
                                ),
                                e,
                            )
                        });
                    let expression = expression_shadow.as_ref();
                    let binding = expression.map_or(Some(false), |expr| {
                        match self.ohlc_colorer_scalar(expr, flow, mutable) {
                            Scalar::Dynamic => Some(true),
                            Scalar::Value(_) | Scalar::Missing => Some(false),
                            Scalar::Unknown => None,
                        }
                    });
                    let style = &mut self.plots[slot.0 as usize].style;
                    if let Some(binding) = binding {
                        if let Some(record) = &mut style.ohlc_body_style {
                            record.runtime_colorer = binding;
                        }
                    } else {
                        style
                            .bound_scalars
                            .retain(|binding| binding.field != "ohlc_body_color");
                        style.ohlc_body_style = None;
                        style.unsupported.push(UnsupportedVisualOption {
                            field: "color".into(),
                            reason: "OHLC body colorer cannot be proven from the effective callsite expression".into(),
                        });
                    }
                }
                HirStmt::If {
                    cond,
                    then_branch,
                    else_branch,
                    span,
                } => match self.ohlc_colorer_scalar(
                    &self
                        .rendering_inline
                        .expression(rendering_inline_provenance::Anchor::Condition(*span), cond),
                    flow,
                    mutable,
                ) {
                    Scalar::Value(InputValue::Bool(true)) => {
                        self.ohlc_colorer_body(then_branch, flow, mutable, changed, collect_only);
                    }
                    Scalar::Value(InputValue::Bool(false)) => {
                        self.ohlc_colorer_body(else_branch, flow, mutable, changed, collect_only);
                    }
                    condition => {
                        let uncertain = !matches!(condition, Scalar::Dynamic);
                        let mut left = flow.clone();
                        let mut right = flow.clone();
                        let mut branch_writes = HashSet::new();
                        self.ohlc_colorer_body(
                            then_branch,
                            &mut left,
                            mutable,
                            &mut branch_writes,
                            collect_only,
                        );
                        self.ohlc_colorer_body(
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
                                // A branch-dependent write gets a colorer
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
                    self.ohlc_colorer_body(body, &mut inner, mutable, &mut loop_writes, true);
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
}
