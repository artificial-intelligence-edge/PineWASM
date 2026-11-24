//! Aeneas extraction roots for the `CsilValidate` extraction: the stack-height
//! preflight (`validate::stack_heights`), the integer TA-input kernels
//! (`ta_inputs`) and the type tables (`abstract_ty_accepts`/`merge`,
//! `Req::accepts`, `pure_operand_reqs`, `pure_result_ty`). Compiled only with
//! the `aeneas` feature, which production builds never enable. Root names
//! differ from their callees so a generated pattern cannot capture the
//! callee's name.

use crate::{ta_inputs, validate, CsilInstr, PureFunc, StackEffect, StackEffectError};
use pinewasm_hir::{builtins::TaStateDecl, TaInputTarget, TaParamRule};

/// `CsilInstr::resolve_stack_effect` with the actual TA-state context.
pub fn root_resolve_stack_effect(
    instr: &CsilInstr,
    ta_states: &[TaStateDecl],
) -> Result<StackEffect, StackEffectError> {
    instr.resolve_stack_effect(ta_states)
}

/// `validate::stack_heights`.
pub fn root_stack_heights(
    code: &[CsilInstr],
    ta_arity: &[(u32, u32)],
) -> Result<Vec<Option<u32>>, validate::HeightErr> {
    validate::stack_heights(code, ta_arity)
}

/// `ta_inputs::ta_target_bounds`.
pub fn root_ta_target_bounds(
    target: TaInputTarget,
    state_rules: Option<&[TaParamRule]>,
) -> Option<(i64, i64)> {
    ta_inputs::ta_target_bounds(target, state_rules)
}

/// `ta_inputs::ta_param_rule_bounds`.
pub fn root_ta_param_rule_bounds(rule: &TaParamRule) -> Option<(i64, i64)> {
    ta_inputs::ta_param_rule_bounds(rule)
}

/// `ta_inputs::resolve_ta_int`.
pub fn root_resolve_ta_int(value: i64, negated: bool, bounds: Option<(i64, i64)>) -> Option<i64> {
    ta_inputs::resolve_ta_int(value, negated, bounds)
}

/// `validate::abstract_ty_accepts` (the `Req::Exact` wildcard rule).
pub fn root_abstract_ty_accepts(
    actual: validate::AbstractTy,
    required: validate::AbstractTy,
) -> bool {
    validate::abstract_ty_accepts(actual, required)
}

/// `validate::abstract_ty_merge` (the join at control-flow merges, `Select`
/// and `nz`).
pub fn root_abstract_ty_merge(
    left: validate::AbstractTy,
    right: validate::AbstractTy,
) -> validate::AbstractTy {
    validate::abstract_ty_merge(left, right)
}

/// `Req::accepts` (the per-operand requirement check).
pub fn root_req_accepts(req: validate::Req, ty: validate::AbstractTy) -> bool {
    req.accepts(ty)
}

/// `validate::pure_operand_reqs` (the `CALL_PURE` operand table).
pub fn root_pure_operand_reqs(func: PureFunc, argc: u8) -> Vec<validate::Req> {
    validate::pure_operand_reqs(func, argc)
}

/// `validate::pure_result_ty` (the `CALL_PURE` result table).
pub fn root_pure_result_ty(
    func: PureFunc,
    argc: u8,
    popped: &[validate::AbstractTy],
) -> validate::AbstractTy {
    validate::pure_result_ty(func, argc, popped)
}

#[cfg(test)]
mod resolver_root_tests {
    use super::*;
    use pinewasm_hir::builtins::TaFunc;

    #[test]
    fn fixed_and_payload_effects_keep_their_counts() {
        let cases = [
            (CsilInstr::PushI64(42), StackEffect { pops: 0, pushes: 1 }),
            (CsilInstr::IntDiv, StackEffect { pops: 2, pushes: 1 }),
            (
                CsilInstr::NewObject {
                    type_id: 0,
                    field_count: 5,
                },
                StackEffect { pops: 5, pushes: 1 },
            ),
        ];
        for (instr, expected) in cases {
            assert_eq!(root_resolve_stack_effect(&instr, &[]), Ok(expected));
        }
    }

    #[test]
    fn ta_state_is_resolved_or_rejected_with_context() {
        let states = [TaStateDecl {
            func: TaFunc::Change,
            params: vec![],
        }];
        assert_eq!(
            root_resolve_stack_effect(&CsilInstr::CallTa { state: 0 }, &states),
            Ok(StackEffect { pops: 1, pushes: 1 })
        );
        assert_eq!(
            root_resolve_stack_effect(&CsilInstr::CallTa { state: 1 }, &states),
            Err(StackEffectError::InvalidTaState {
                state: 1,
                state_count: 1
            })
        );
    }
}
