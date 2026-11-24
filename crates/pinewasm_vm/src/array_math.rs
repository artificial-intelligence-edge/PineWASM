//! Successful adjacent array-read/subtract/abs/add/log/add execution. This
//! preserves the original kernels and delegates every possible failure.

use crate::vm_charge::charge_counters;
use crate::{array_get, numeric_call, vm_dispatch, vm_ops, Vm};
use pinewasm_csil::{CsilInstr, CsilProgram};
use pinewasm_hir::PureFunc;
use pinewasm_mir::Value;

/// The array read is already charged. Its successful result is the sequence's
/// largest intermediate stack (one shorter than the incoming stack).
#[inline(always)]
pub(crate) fn charge_tails(
    budget: &mut u64,
    remaining: &mut u64,
    stack_len: usize,
    stack_limit: usize,
) -> bool {
    if stack_len < 5 || stack_len - 1 > stack_limit || *budget < 5 || *remaining < 5 {
        false
    } else {
        match charge_counters(budget, remaining, 5) {
            Ok(()) => true,
            Err(_) => false,
        }
    }
}

/// Replace five cells by one, without allocating or changing the prefix.
/// A short stack is unchanged, including its retained capacity.
#[inline(always)]
pub(crate) fn replace_five<T>(stack: &mut Vec<T>, result: T) -> bool {
    let len = stack.len();
    if len < 5 {
        false
    } else {
        stack[len - 5] = result;
        let _ = stack.pop();
        let _ = stack.pop();
        let _ = stack.pop();
        let _ = stack.pop();
        true
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
    pub(crate) fn exec_array_math_chain(
        &mut self,
        program: &CsilProgram,
        pc: usize,
        budget: &mut u64,
    ) -> Option<vm_dispatch::Flow> {
        if !matches!(program.main().code.get(pc + 1), Some(CsilInstr::F64Sub))
            || !matches!(
                program.main().code.get(pc + 2),
                Some(CsilInstr::CallPure {
                    func: PureFunc::MathAbs,
                    argc: 1
                })
            )
            || !matches!(program.main().code.get(pc + 3), Some(CsilInstr::F64Add))
            || !matches!(
                program.main().code.get(pc + 4),
                Some(CsilInstr::CallPure {
                    func: PureFunc::MathLog,
                    argc: 1
                })
            )
            || !matches!(program.main().code.get(pc + 5), Some(CsilInstr::F64Add))
        {
            return None;
        }
        let len = self.stack.len();
        if len < 5
            || len - 1 > self.options.max_stack_depth
            || *budget < 5
            || self.instructions_remaining < 5
        {
            return None;
        }
        // Validate every operand before interpreting missing values. The
        // successful getter neither mutates the heap nor clones a string.
        let accumulator = float(&self.stack[len - 5])?;
        let offset = float(&self.stack[len - 4])?;
        let current = float(&self.stack[len - 3])?;
        let fetched = array_get::evaluate(&self.heap, &self.stack[len - 2..])?;
        let difference = vm_ops::float_arith(vm_ops::FloatArithOp::Sub, current, float(&fetched)?);
        let abs = numeric_call::evaluate(PureFunc::MathAbs, std::slice::from_ref(&difference))?;
        let sum = vm_ops::float_arith(vm_ops::FloatArithOp::Add, offset, float(&abs)?);
        let log = numeric_call::evaluate(PureFunc::MathLog, std::slice::from_ref(&sum))?;
        let result = vm_ops::float_arith(vm_ops::FloatArithOp::Add, accumulator, float(&log)?);
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
            // The declared-lane observer sees the same five intermediate
            // instruction-entry stacks as separate dispatches.
            crate::numeric_stack::replace_pair(&mut self.stack, fetched);
            crate::tests::lane_rows::observe(self, pc + 1);
            crate::numeric_stack::replace_pair(&mut self.stack, difference);
            crate::tests::lane_rows::observe(self, pc + 2);
            self.stack[len - 3] = abs;
            crate::tests::lane_rows::observe(self, pc + 3);
            crate::numeric_stack::replace_pair(&mut self.stack, sum);
            crate::tests::lane_rows::observe(self, pc + 4);
            self.stack[len - 4] = log;
            crate::tests::lane_rows::observe(self, pc + 5);
            crate::numeric_stack::replace_pair(&mut self.stack, result);
        }
        #[cfg(not(test))]
        replace_five(&mut self.stack, result);
        if let Some(profile) = self.profile.as_mut() {
            for tail in [pc + 1, pc + 2, pc + 3, pc + 4, pc + 5] {
                profile.count[tail] += 1;
                profile.units[tail] += 1;
                profile.record_combined_dispatch(tail);
            }
        }
        // Continue through a successful adjacent store/current-read pair.
        // Refusal leaves this already committed six-op result untouched and
        // resumes at the ordinary store, including every finite-fuel stop.
        if let Some(CsilInstr::StoreUserSlotCurrent { slot }) = program.main().code.get(pc + 6) {
            if let Some(flow) =
                self.exec_store_current_read_in_mode(program, *slot, pc + 6, budget, true)
            {
                return Some(flow);
            }
        }
        Some(vm_dispatch::Flow::Jump(pc + 6))
    }
}

#[cfg(test)]
mod tests {
    use super::{charge_tails, replace_five};

    #[test]
    fn tail_charge_preserves_rejections_and_checks_peak_without_overflow() {
        for bar in [0, 1, 4, 5, 6, u64::MAX] {
            for run in [0, 1, 4, 5, 6, u64::MAX] {
                for len in [0, 1, 4, 5, 6, usize::MAX] {
                    for limit in [0, 1, 3, 4, 5, usize::MAX] {
                        let (mut b, mut r) = (bar, run);
                        let accepted = charge_tails(&mut b, &mut r, len, limit);
                        let eligible = len >= 5 && len - 1 <= limit && bar >= 5 && run >= 5;
                        assert_eq!(accepted, eligible);
                        assert_eq!(
                            (b, r),
                            if eligible {
                                (bar - 5, run - 5)
                            } else {
                                (bar, run)
                            }
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn replacement_retains_prefix_and_capacity_and_rejects_short_stacks() {
        for len in 0..=9 {
            let original: Vec<_> = (0..len).collect();
            let mut actual = original.clone();
            actual.reserve(20);
            let capacity = actual.capacity();
            assert_eq!(replace_five(&mut actual, 99), len >= 5);
            let expected = if len >= 5 {
                let mut values = original[..len - 5].to_vec();
                values.push(99);
                values
            } else {
                original
            };
            assert_eq!(actual, expected);
            assert_eq!(actual.capacity(), capacity);
        }
    }
}
