//! Const numeric accessor values are separate from the stored raw color value.
use super::{Analyzer, HirExpr, HirExprKind, InputValue, PureFunc, Qualifier};

/// RGB construction first quantizes opacity to the stored alpha byte, then
/// numeric const accessors return a whole transparency percent computed from
/// that byte, so the percent is not simply the raw transparency rounded.
/// This does not change the separately stored raw color value.
pub(crate) fn rgb_const_transparency(transparency: f64) -> Option<f64> {
    if !transparency.is_finite() {
        return None;
    }
    let alpha = (255.0 * (1.0 - transparency.clamp(0.0, 100.0) / 100.0)).round();
    Some((100.0 * (1.0 - alpha / 255.0)).round())
}

impl Analyzer<'_> {
    /// Generic input-layout constants may include an initial persistent value.
    /// Executable const folding must instead prove every transitive read is
    /// stable across bars. Ordinary aliases inherit this stricter provenance.
    pub(crate) fn const_accessor_expr_is_stable(&self, expr: &HirExpr) -> bool {
        if expr.qualifier != Qualifier::Const {
            return false;
        }
        match &expr.kind {
            HirExprKind::ConstNa
            | HirExprKind::ConstInt(_)
            | HirExprKind::ConstFloat(_)
            | HirExprKind::ConstBool(_)
            | HirExprKind::ConstString(_)
            | HirExprKind::ConstColor { .. } => true,
            HirExprKind::ReadVar { symbol, offset: 0 } => {
                self.const_accessor_stable_symbols.contains(symbol)
            }
            HirExprKind::IntToFloat(inner) => self.const_accessor_expr_is_stable(inner),
            HirExprKind::Unary { operand, .. } => self.const_accessor_expr_is_stable(operand),
            HirExprKind::Binary { lhs, rhs, .. } => {
                self.const_accessor_expr_is_stable(lhs) && self.const_accessor_expr_is_stable(rhs)
            }
            HirExprKind::Ternary {
                cond,
                then_e,
                else_e,
            } => {
                self.const_accessor_expr_is_stable(cond)
                    && self.const_accessor_expr_is_stable(then_e)
                    && self.const_accessor_expr_is_stable(else_e)
            }
            HirExprKind::CallPure { args, .. } => args
                .iter()
                .all(|arg| self.const_accessor_expr_is_stable(arg)),
            _ => false,
        }
    }

    /// Numeric-default rules distinguish constructor
    /// coercion: const color.new truncates its transparency, whereas const
    /// RGB accessors quantize opacity to a byte before returning a whole percent;
    /// hex values already carry byte-derived opacity. Do not rewrite the color
    /// itself or apply this rule to input/simple/series VM values.
    pub(crate) fn const_color_transparency(&self, expr: &HirExpr) -> Option<f64> {
        if !self.const_accessor_expr_is_stable(expr) {
            return None;
        }
        let numeric = |expr: &HirExpr| match self.const_input_value_from_hir(expr)? {
            InputValue::Int(value) => Some(value as f64),
            InputValue::Float(value) => Some(value),
            _ => None,
        };
        let whole = match &expr.kind {
            HirExprKind::ConstColor { t, .. } => t.round(),
            HirExprKind::ReadVar { symbol, offset: 0 } => {
                // Persistent values may carry a later assignment across bars.
                // Only nonpersistent const aliases with a stored transparency are admitted.
                return self.const_color_transparencies.get(symbol).copied();
            }
            HirExprKind::Ternary {
                cond,
                then_e,
                else_e,
            } => {
                return match self.const_input_value_from_hir(cond)? {
                    InputValue::Bool(true) => self.const_color_transparency(then_e),
                    InputValue::Bool(false) => self.const_color_transparency(else_e),
                    _ => None,
                };
            }
            HirExprKind::CallPure {
                func: PureFunc::CastColor,
                args,
            } => return self.const_color_transparency(args.first()?),
            HirExprKind::CallPure {
                func: PureFunc::ColorNew,
                args,
            } => {
                if !matches!(
                    self.const_input_value_from_hir(args.first()?)?,
                    InputValue::Color { .. }
                ) {
                    return None;
                }
                numeric(args.get(1)?)?.clamp(0.0, 100.0).trunc()
            }
            HirExprKind::CallPure {
                func: PureFunc::ColorRgb,
                args,
            } => {
                if !matches!(
                    self.const_input_value_from_hir(expr)?,
                    InputValue::Color { .. }
                ) {
                    return None;
                }
                rgb_const_transparency(numeric(args.get(3)?)?)?
            }
            _ => return None,
        };
        whole.is_finite().then_some(whole.clamp(0.0, 100.0))
    }
}
