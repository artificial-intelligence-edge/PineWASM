//! Actual HTF policy adapter for the production generic function orchestration.
use super::{count_htf_history_nodes, count_htf_ta_nodes, validate_htf_expr};
use pinewasm_hir::{HtfExpr, InputDecl};

/// The production implementation must equal the unchanged HTF validation
/// against the actual program inputs. An arbitrary successful callback does
/// not establish Rust validator acceptance. Scalar proofs derive no visits.
pub(super) trait HtfPolicy {
    fn validate(&self, expr: &HtfExpr) -> Result<(), String>;
}
pub(super) struct ProgramHtfPolicy<'a> {
    pub inputs: &'a [InputDecl],
}
impl HtfPolicy for ProgramHtfPolicy<'_> {
    fn validate(&self, expr: &HtfExpr) -> Result<(), String> {
        validate_htf_expr(
            expr,
            count_htf_ta_nodes(expr),
            count_htf_history_nodes(expr),
            self.inputs,
        )
    }
}

/// Actual overflow behavior remains the unchanged standard checked conversion.
/// Bounded proofs quantify over arbitrary implementations and derive non-invocation.
pub(super) trait StackNarrowFailure {
    fn on_overflow(&self, seen: usize) -> u32;
}
pub(super) struct ProductionStackNarrowFailure;
impl StackNarrowFailure for ProductionStackNarrowFailure {
    #[track_caller]
    fn on_overflow(&self, seen: usize) -> u32 {
        u32::try_from(seen).expect("the preflighted abstract stack ceiling fits in u32")
    }
}
