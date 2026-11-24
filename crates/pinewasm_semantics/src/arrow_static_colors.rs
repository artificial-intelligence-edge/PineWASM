//! Static plotarrow styles follow effective callsite expressions, not declared
//! qualifiers, input defaults, automatic palettes or runtime colors.
use crate::*;
use pinewasm_hir::{ArrowStaticColorOrigin as Origin, ArrowStaticRoleColor};

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Scalar {
    Value(InputValue),
    Missing,
    Dynamic,
    Unknown,
}
pub(super) type Environment = HashMap<SymbolId, Scalar>;

pub(super) fn writes(body: &[HirStmt], out: &mut HashSet<SymbolId>) {
    for stmt in body {
        match stmt {
            HirStmt::Assign {
                symbol,
                init: false,
                ..
            } => {
                out.insert(*symbol);
            }
            HirStmt::If {
                then_branch,
                else_branch,
                ..
            } => {
                writes(then_branch, out);
                writes(else_branch, out);
            }
            HirStmt::For { body, .. } | HirStmt::While { body, .. } => writes(body, out),
            _ => {}
        }
    }
}

fn literal(value: &Scalar, template: &HirExpr) -> Option<HirExpr> {
    let kind = match value {
        Scalar::Missing => HirExprKind::ConstNa,
        Scalar::Value(InputValue::Int(v)) => HirExprKind::ConstInt(*v),
        Scalar::Value(InputValue::Float(v)) => HirExprKind::ConstFloat(*v),
        Scalar::Value(InputValue::Bool(v)) => HirExprKind::ConstBool(*v),
        Scalar::Value(InputValue::String(v)) => HirExprKind::ConstString(v.clone()),
        Scalar::Value(InputValue::Color { r, g, b, t }) => HirExprKind::ConstColor {
            r: *r,
            g: *g,
            b: *b,
            t: *t,
        },
        _ => return None,
    };
    Some(HirExpr {
        kind,
        qualifier: Qualifier::Const,
        ..template.clone()
    })
}

impl Analyzer<'_> {
    pub(super) fn arrow_scalar(&self, expr: &HirExpr, env: &Environment) -> Scalar {
        use Scalar::{Dynamic, Missing, Unknown, Value};
        if self.arrow_history_spans.contains(&expr.span) {
            return Dynamic;
        }
        let operands = match &expr.kind {
            HirExprKind::ConstNa => return Missing,
            HirExprKind::ConstInt(_)
            | HirExprKind::ConstFloat(_)
            | HirExprKind::ConstBool(_)
            | HirExprKind::ConstString(_)
            | HirExprKind::ConstColor { .. } => Vec::new(),
            HirExprKind::ReadVar { symbol, offset: 0 } => {
                if let Some(call) = self.rendering_inline.get_call(*symbol) {
                    let Some(result) = &call.result else {
                        return Unknown;
                    };
                    let parameters: Vec<_> = call
                        .parameters
                        .iter()
                        .map(|parameter| env.get(parameter).cloned().unwrap_or(Unknown))
                        .collect();
                    if parameters.contains(&Dynamic) {
                        return Dynamic;
                    }
                    if parameters.contains(&Unknown) {
                        return Unknown;
                    }
                    return self.arrow_scalar(result, env);
                }
                return env.get(symbol).cloned().unwrap_or(Unknown);
            }
            HirExprKind::ReadInput { .. }
            | HirExprKind::ReadBuiltin { .. }
            | HirExprKind::ReadBuiltinDynamic { .. }
            | HirExprKind::ReadVarDynamic { .. }
            | HirExprKind::ReadVar { .. } => return Dynamic,
            HirExprKind::Ternary {
                cond,
                then_e,
                else_e,
            } => {
                return match self.arrow_scalar(cond, env) {
                    Value(InputValue::Bool(true)) => self.arrow_scalar(then_e, env),
                    Value(InputValue::Bool(false)) => self.arrow_scalar(else_e, env),
                    Dynamic => Dynamic,
                    _ => Unknown,
                }
            }
            // A materialized history value is a historical value even
            // when its current expression is an effective constant. Preserve
            // the same dynamic style origin as a direct history read.
            HirExprKind::MaterializeHistory { .. }
            | HirExprKind::MaterializeHistoryDynamic { .. } => return Dynamic,
            HirExprKind::IntToFloat(inner) => vec![inner.as_ref()],
            HirExprKind::Unary { operand, .. } => vec![operand.as_ref()],
            // Arrow constant folds absorbing Boolean values on either
            // side, independently of runtime short-circuit execution. This
            // read-only preparation does not skip semantic validation or emit
            // different instructions for either operand.
            HirExprKind::Binary {
                op: op @ (BinOp::And | BinOp::Or),
                lhs,
                rhs,
            } => {
                let left = self.arrow_scalar(lhs, env);
                if matches!(
                    (*op, &left),
                    (BinOp::And, Value(InputValue::Bool(false)))
                        | (BinOp::Or, Value(InputValue::Bool(true)))
                ) {
                    return left;
                }
                let right = self.arrow_scalar(rhs, env);
                if matches!(
                    (*op, &right),
                    (BinOp::And, Value(InputValue::Bool(false)))
                        | (BinOp::Or, Value(InputValue::Bool(true)))
                ) {
                    return right;
                }
                return match (left, right) {
                    (Value(InputValue::Bool(left)), Value(InputValue::Bool(right))) => {
                        Value(InputValue::Bool(if *op == BinOp::And {
                            left && right
                        } else {
                            left || right
                        }))
                    }
                    // Preserve existing known-dynamic precedence after the
                    // independently qualified absorbing-value checks.
                    (Dynamic, _) | (_, Dynamic) => Dynamic,
                    _ => Unknown,
                };
            }
            HirExprKind::Binary { lhs, rhs, .. } => vec![lhs.as_ref(), rhs.as_ref()],
            HirExprKind::CallPure { func, args } => {
                // A gradient has a colorer even with all-constant arguments,
                // since its colorer metadata is kept independently of constant
                // folding. Stateful functions also cannot supply a static value
                // from literal arguments.
                if *func == PureFunc::ColorFromGradient
                    || *func == PureFunc::MathRandom
                    || func.is_strategy()
                {
                    return Dynamic;
                }
                args.iter().collect()
            }
            _ => {
                return if expr.qualifier == Qualifier::Const {
                    Unknown
                } else {
                    Dynamic
                }
            }
        };
        let values: Vec<_> = operands
            .iter()
            .map(|arg| self.arrow_scalar(arg, env))
            .collect();
        if values.contains(&Dynamic) {
            return Dynamic;
        }
        if values.contains(&Unknown) {
            return Unknown;
        }
        if values.contains(&Missing) {
            // Missing color survives these color-preserving operations. Do not
            // invent semantics for other missing-valued pure operations.
            return match &expr.kind {
                // Direct bool(na) and a bool cast of a typed numeric NA alias are
                // matched as separate shapes. Preserve existing color absence and
                // keep unqualified missing shapes unsupported.
                HirExprKind::CallPure {
                    func: PureFunc::CastBool,
                    args,
                } if values.as_slice() == [Missing] => match args.as_slice() {
                    [arg] if matches!(arg.kind, HirExprKind::ConstNa) => {
                        Value(InputValue::Bool(false))
                    }
                    [arg]
                        if matches!(arg.kind, HirExprKind::ReadVar { offset: 0, .. })
                            && matches!(arg.ty, Type::Int | Type::Float) =>
                    {
                        Dynamic
                    }
                    _ => Unknown,
                },
                HirExprKind::CallPure {
                    func: PureFunc::CastFloat | PureFunc::CastInt,
                    ..
                } if values.as_slice() == [Missing] => Dynamic,
                HirExprKind::CallPure {
                    func: PureFunc::CastColor | PureFunc::ColorNew,
                    ..
                } if values.first() == Some(&Missing) => Missing,
                _ => Unknown,
            };
        }
        // This arrow-only constant fold uses the same deterministic cosine
        // kernel as the VM. It does not extend generic constant input folding
        // or alter instructions/runtime values for either operand.
        if matches!(
            expr.kind,
            HirExprKind::CallPure {
                func: PureFunc::MathCos,
                ..
            }
        ) {
            let value = match values.as_slice() {
                [Value(InputValue::Int(value))] => *value as f64,
                [Value(InputValue::Float(value))] if value.is_finite() => *value,
                _ => return Unknown,
            };
            let result = pinewasm_num::raw::cos(value);
            return if result.is_finite() {
                Value(InputValue::Float(result))
            } else {
                Unknown
            };
        }
        let Some(constants) = operands
            .iter()
            .zip(&values)
            .map(|(arg, value)| literal(value, arg))
            .collect::<Option<Vec<_>>>()
        else {
            return Unknown;
        };
        let mut constants = constants;
        if matches!(
            expr.kind,
            HirExprKind::CallPure {
                func: PureFunc::ColorNew,
                ..
            }
        ) {
            // Constant color.t truncates for color.new, unlike color.rgb/hex
            // alpha. This normalization belongs to the static producer fold,
            // never the runtime fill lane.
            let value = match values.get(1) {
                Some(Value(InputValue::Int(v))) => Some(*v as f64),
                Some(Value(InputValue::Float(v))) => Some(*v),
                _ => None,
            };
            if let (Some(value), Some(arg)) = (value, constants.get_mut(1)) {
                arg.kind = HirExprKind::ConstFloat(value.clamp(0.0, 100.0).trunc());
                arg.ty = Type::Float;
            }
        }
        if matches!(
            expr.kind,
            HirExprKind::CallPure {
                func: PureFunc::ColorRgb,
                ..
            }
        ) {
            let transparency = match values.get(3) {
                Some(Value(InputValue::Int(value))) => Some(*value as f64),
                Some(Value(InputValue::Float(value))) => Some(*value),
                _ => None,
            };
            if let (Some(value), Some(argument)) = (transparency, constants.get_mut(3)) {
                let Some(whole) = super::const_color_accessors::rgb_const_transparency(value)
                else {
                    return Unknown;
                };
                argument.kind = HirExprKind::ConstFloat(whole);
                argument.ty = Type::Float;
            }
        }
        let mut constants = constants.into_iter();
        let mut normalized = expr.clone();
        normalized.qualifier = Qualifier::Const;
        normalized.kind = match &expr.kind {
            HirExprKind::IntToFloat(_) => {
                HirExprKind::IntToFloat(Box::new(constants.next().unwrap()))
            }
            HirExprKind::Unary { op, .. } => HirExprKind::Unary {
                op: *op,
                operand: Box::new(constants.next().unwrap()),
            },
            HirExprKind::Binary { op, .. } => HirExprKind::Binary {
                op: *op,
                lhs: Box::new(constants.next().unwrap()),
                rhs: Box::new(constants.next().unwrap()),
            },
            HirExprKind::CallPure { func, .. } => HirExprKind::CallPure {
                func: *func,
                args: constants.collect(),
            },
            _ => normalized.kind,
        };
        match self.const_input_value_from_hir(&normalized) {
            // RGB opacity is quantized above; hex already carries byte-derived
            // transparency. Successful numeric and static constant evaluations
            // quantize whole-percent T separately from the raw runtime lane.
            Some(InputValue::Color { r, g, b, t }) => Value(InputValue::Color {
                r,
                g,
                b,
                t: t.round(),
            }),
            Some(value) => Value(value),
            None => Unknown,
        }
    }

    fn arrow_role(
        &self,
        expr: &HirExpr,
        env: &Environment,
        up: bool,
    ) -> Result<ArrowStaticRoleColor, ()> {
        let (origin, r, g, b, t) = match self.arrow_scalar(expr, env) {
            Scalar::Value(InputValue::Color { r, g, b, t }) => (Origin::Constant, r, g, b, t),
            Scalar::Missing => (Origin::Missing, 0, 0, 0, 100.0),
            Scalar::Dynamic => (
                Origin::Dynamic,
                if up { 0 } else { 255 },
                if up { 255 } else { 0 },
                0,
                0.0,
            ),
            _ => return Err(()),
        };
        Ok(ArrowStaticRoleColor { origin, r, g, b, t })
    }

    fn arrow_body(
        &mut self,
        body: &[HirStmt],
        env: &mut Environment,
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
                    let value = self.rendering_inline.expression(
                        rendering_inline_provenance::Anchor::Assignment(*symbol, *span),
                        value,
                    );
                    if !init {
                        reachable_writes.insert(*symbol);
                    }
                    let scalar = if mutable_persistent.contains(symbol) {
                        Scalar::Dynamic
                    } else {
                        self.arrow_scalar(&value, env)
                    };
                    env.insert(*symbol, scalar);
                }
                HirStmt::Destructure { targets, value, .. } => {
                    if let HirExprKind::Tuple { elements } = &value.kind {
                        let values: Vec<_> =
                            elements.iter().map(|e| self.arrow_scalar(e, env)).collect();
                        env.extend(targets.iter().copied().zip(values));
                    } else {
                        for symbol in targets {
                            env.insert(*symbol, Scalar::Dynamic);
                        }
                    }
                }
                HirStmt::Plot { slot, colors, .. }
                    if !collect_only
                        && self.plots[slot.0 as usize].style.kind
                            == OutputDisplayKind::PlotArrow =>
                {
                    let mut styles = self.plots[slot.0 as usize]
                        .style
                        .arrow_static_colors
                        .take()
                        .unwrap();
                    let mut unsupported = Vec::new();
                    for (role, expr) in colors {
                        let (current, up, field) = match role {
                            PlotColorRole::Up => (&mut styles.up, true, "colorup"),
                            PlotColorRole::Down => (&mut styles.down, false, "colordown"),
                            _ => continue,
                        };
                        if current.origin == Origin::Omitted {
                            continue;
                        }
                        let expr = self.rendering_inline.expression(
                            rendering_inline_provenance::Anchor::Plot(*slot, *role),
                            expr,
                        );
                        match self.arrow_role(&expr,env,up) {
                            Ok(value) => *current=value,
                            Err(()) => unsupported.push(UnsupportedVisualOption {field:field.into(),reason:"static arrow color cannot be proven from the effective callsite expression".into()}),
                        }
                    }
                    let style = &mut self.plots[slot.0 as usize].style;
                    // An incomplete static record must never masquerade as a
                    // complete producer contract. Preserve a visible diagnostic.
                    if unsupported.is_empty() {
                        style.arrow_static_colors = Some(styles);
                    }
                    style.unsupported.extend(unsupported);
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
                    env,
                ) {
                    Scalar::Value(InputValue::Bool(true)) => self.arrow_body(
                        then_branch,
                        env,
                        mutable_persistent,
                        reachable_writes,
                        collect_only,
                    ),
                    Scalar::Value(InputValue::Bool(false)) => self.arrow_body(
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
                        self.arrow_body(
                            then_branch,
                            &mut left,
                            mutable_persistent,
                            &mut branch_writes,
                            collect_only,
                        );
                        self.arrow_body(
                            else_branch,
                            &mut right,
                            mutable_persistent,
                            &mut branch_writes,
                            collect_only,
                        );
                        reachable_writes.extend(branch_writes.iter().copied());
                        for symbol in left.keys().chain(right.keys()) {
                            // A reachable conditional write keeps the static-style
                            // fallback dynamic, even when both branch values agree.
                            let value = if branch_writes.contains(symbol) {
                                if uncertain
                                    || left.get(symbol) == Some(&Scalar::Unknown)
                                    || right.get(symbol) == Some(&Scalar::Unknown)
                                {
                                    Scalar::Unknown
                                } else {
                                    Scalar::Dynamic
                                }
                            } else {
                                match (left.get(symbol), right.get(symbol)) {
                                    (Some(a), Some(b)) if a == b => a.clone(),
                                    _ => Scalar::Dynamic,
                                }
                            };
                            env.insert(*symbol, value);
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
                        loop_env.insert(symbol, Scalar::Dynamic);
                    }
                    let mut changed = HashSet::new();
                    self.arrow_body(body, &mut loop_env, mutable_persistent, &mut changed, true);
                    reachable_writes.extend(changed.iter().copied());
                    // Loop writes produce a dynamic static fallback, even for an
                    // unconditional constant assignment in a loop.
                    for symbol in changed {
                        let value = if loop_env.get(&symbol) == Some(&Scalar::Unknown) {
                            Scalar::Unknown
                        } else {
                            Scalar::Dynamic
                        };
                        env.insert(symbol, value);
                    }
                }
                _ => {}
            }
        }
    }

    pub(crate) fn finalize_arrow_static_colors(&mut self, body: &[HirStmt]) {
        if !self
            .plots
            .iter()
            .any(|p| p.style.kind == OutputDisplayKind::PlotArrow)
        {
            return;
        }
        let mutable_persistent = self.mutable_arrow_persistent_symbols(body);
        self.arrow_body(
            body,
            &mut Environment::new(),
            &mutable_persistent,
            &mut HashSet::new(),
            false,
        );
    }

    pub(super) fn mutable_arrow_persistent_symbols(
        &mut self,
        body: &[HirStmt],
    ) -> HashSet<SymbolId> {
        // Reachability may change on later bars when a persistent condition is
        // written after its first read. Grow the mutation set to a fixed point;
        // a literal dead branch remains dead throughout this analysis.
        let persistent: HashSet<_> = self
            .symbols
            .iter()
            .filter(|s| s.persistent)
            .map(|s| s.id)
            .collect();
        let mut mutable_persistent = HashSet::new();
        loop {
            let mut reachable_writes = HashSet::new();
            self.arrow_body(
                body,
                &mut Environment::new(),
                &mutable_persistent,
                &mut reachable_writes,
                true,
            );
            let before = mutable_persistent.len();
            mutable_persistent.extend(reachable_writes.intersection(&persistent).copied());
            if mutable_persistent.len() == before {
                break;
            }
        }
        mutable_persistent
    }
}
