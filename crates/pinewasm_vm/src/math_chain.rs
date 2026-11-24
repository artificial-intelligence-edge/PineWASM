//! Guarded execution of an adjacent abs/add/log sequence. Each numeric
//! kernel and logical instruction charge is retained.

use crate::vm_charge::charge_counters;
use crate::{numeric_call, numeric_stack, vm_dispatch, vm_ops, Vm};
use pinewasm_csil::{CsilInstr, CsilProgram};
use pinewasm_hir::PureFunc;
use pinewasm_mir::Value;

/// The abs dispatch is already charged. Only a wholly successful sequence
/// may reserve its two following dispatch units in advance. Otherwise the
/// ordinary handlers retain intermediate stacks and the first failure.
#[inline(always)]
pub(crate) fn charge_tails(
    budget: &mut u64,
    remaining: &mut u64,
    stack_len: usize,
    stack_limit: usize,
) -> bool {
    if stack_len < 2 || stack_len > stack_limit || *budget < 2 || *remaining < 2 {
        false
    } else {
        match charge_counters(budget, remaining, 2) {
            Ok(()) => true,
            Err(_) => false,
        }
    }
}

#[inline(always)]
#[allow(clippy::option_option)]
fn float(value: &Value) -> Option<Option<f64>> {
    match value {
        Value::Na => Some(None),
        Value::Float(value) => Some(Some(*value)),
        _ => None,
    }
}

impl Vm {
    #[inline(always)]
    pub(crate) fn exec_math_chain(
        &mut self,
        program: &CsilProgram,
        pc: usize,
        budget: &mut u64,
    ) -> Option<vm_dispatch::Flow> {
        if !matches!(program.main().code.get(pc + 1), Some(CsilInstr::F64Add))
            || !matches!(
                program.main().code.get(pc + 2),
                Some(CsilInstr::CallPure {
                    func: PureFunc::MathLog,
                    argc: 1,
                })
            )
        {
            return None;
        }
        let len = self.stack.len();
        // Do not compute an uncharged tail when either instruction ceiling
        // is short. Nothing in the numeric kernels changes these counters.
        if len < 2
            || len > self.options.max_stack_depth
            || *budget < 2
            || self.instructions_remaining < 2
        {
            return None;
        }
        // Integer abs has an integer result, which the following F64Add
        // rejects. Every such operand stays with the general error path.
        let lhs = float(&self.stack[len - 2])?;
        float(&self.stack[len - 1])?;
        let abs = numeric_call::evaluate(PureFunc::MathAbs, &self.stack[len - 1..])?;
        let sum = vm_ops::float_arith(vm_ops::FloatArithOp::Add, lhs, float(&abs)?);
        let log = numeric_call::evaluate(PureFunc::MathLog, std::slice::from_ref(&sum))?;
        if !charge_tails(
            budget,
            &mut self.instructions_remaining,
            len,
            self.options.max_stack_depth,
        ) {
            return None;
        }
        #[cfg(test)]
        {
            self.stack[len - 1] = abs;
            crate::tests::lane_rows::observe(self, pc + 1);
            numeric_stack::replace_pair(&mut self.stack, sum);
            crate::tests::lane_rows::observe(self, pc + 2);
            self.stack[len - 2] = log;
        }
        #[cfg(not(test))]
        numeric_stack::replace_pair(&mut self.stack, log);
        if let Some(profile) = self.profile.as_mut() {
            for tail in [pc + 1, pc + 2] {
                profile.count[tail] += 1;
                profile.units[tail] += 1;
                profile.record_combined_dispatch(tail);
            }
        }
        Some(vm_dispatch::Flow::Jump(pc + 3))
    }
}

#[cfg(test)]
mod tests {
    use super::charge_tails;

    #[test]
    fn math_tail_charge_preserves_rejections_and_charges_exactly_two() {
        for bar in [0, 1, 2, 3, 10, u64::MAX] {
            for run in [0, 1, 2, 3, 10, u64::MAX] {
                for len in [0, 1, 2, 3, 10, usize::MAX] {
                    for limit in [0, 1, 2, 3, 10, usize::MAX] {
                        let (mut b, mut r) = (bar, run);
                        let accepted = charge_tails(&mut b, &mut r, len, limit);
                        let eligible = len >= 2 && len <= limit && bar >= 2 && run >= 2;
                        assert_eq!(accepted, eligible);
                        assert_eq!(
                            (b, r),
                            if eligible {
                                (bar - 2, run - 2)
                            } else {
                                (bar, run)
                            }
                        );
                    }
                }
            }
        }
    }
}
