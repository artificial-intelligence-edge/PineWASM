//! Free root for the program-field TA rule lookup.

use crate::CsilProgram;
use pinewasm_hir::{TaInputBinding, TaParamRule};

pub fn ta_input_rule(program: &CsilProgram, binding: &TaInputBinding) -> Option<TaParamRule> {
    program.ta_input_rule(binding)
}

/// Bounds caller, without a promoted static-slice return.
pub fn ta_input_bounds(program: &CsilProgram, binding: &TaInputBinding) -> Option<(i64, i64)> {
    program.ta_input_bounds(binding)
}

/// Integer arm used by the program; default lookup and float dispatch are separate.
pub fn resolve_ta_integer_branch(
    program: &CsilProgram,
    binding: &TaInputBinding,
    default: &crate::InputValue,
    value: Option<&crate::InputValue>,
) -> Option<i64> {
    program.resolve_ta_integer_branch(binding, default, value)
}

/// Full native resolver, including declaration lookup and float dispatch.
pub fn resolve_ta_input(
    program: &CsilProgram,
    binding: &TaInputBinding,
    value: Option<&crate::InputValue>,
) -> Option<i64> {
    program.resolve_ta_input(binding, value)
}
