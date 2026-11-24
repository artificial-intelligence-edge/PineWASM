//! Successful adjacent slot/field pairs. The original CSIL,
//! logical instruction charges, profile entries and failure paths are unchanged.

use crate::vm_charge::charge_counters;
use crate::{vm_dispatch, Vm};
use pinewasm_csil::{CsilInstr, CsilProgram};
use pinewasm_mir::heap::{Heap, HeapObj};
use pinewasm_mir::{HandleId, Value};

/// The first dispatch unit has already been charged. A failed eligibility
/// check changes no counter; the ordinary read and following dispatch keep
/// their exact error order. Stack space is needed by the original slot read.
#[inline(always)]
pub(crate) fn charge_tail(
    budget: &mut u64,
    remaining: &mut u64,
    stack_len: usize,
    stack_limit: usize,
) -> bool {
    if stack_len >= stack_limit || *budget == 0 || *remaining == 0 {
        false
    } else {
        match charge_counters(budget, remaining, 1) {
            Ok(()) => true,
            Err(_) => false,
        }
    }
}

/// A non-string field with no allocation or mutation. Invalid handles,
/// receivers, indices and string clones stay with the original handler.
#[inline(always)]
pub(crate) fn evaluate(heap: &Heap, handle: HandleId, field_idx: u32) -> Option<Value> {
    let HeapObj::Object { fields, .. } = heap.get(handle)? else {
        return None;
    };
    let field = fields.get(field_idx as usize)?;
    if matches!(field, Value::String(_)) {
        return None;
    }
    Some(field.clone())
}

impl Vm {
    /// `ReadUserSlot` has already been charged and profiled, but its operand
    /// has not been pushed. Only a successful, allocation-free tail can enter
    /// this path. Otherwise the original two dispatches decide every error.
    #[inline(always)]
    pub(crate) fn exec_slot_pair(
        &mut self,
        program: &CsilProgram,
        value: &Value,
        pc: usize,
        budget: &mut u64,
    ) -> Option<vm_dispatch::Flow> {
        // Only an object reference followed by GetField is eligible.
        // Other slot reads keep their original path.
        let (field, target) = match value {
            Value::Ref(handle) => match program.main().code.get(pc + 1) {
                Some(CsilInstr::GetField { field_idx }) => {
                    (Some(evaluate(&self.heap, *handle, *field_idx)?), pc + 2)
                }
                _ => return None,
            },
            _ => return None,
        };
        if !charge_tail(
            budget,
            &mut self.instructions_remaining,
            self.stack.len(),
            self.options.max_stack_depth,
        ) {
            return None;
        }
        #[cfg(test)]
        {
            // The test trace observer sees the same logical intermediate
            // stack that the original pair produces before its second opcode.
            self.stack.push(value.clone());
            crate::tests::lane_rows::observe(self, pc + 1);
            self.stack.pop();
        }
        if let Some(profile) = self.profile.as_mut() {
            profile.count[pc + 1] += 1;
            profile.units[pc + 1] += 1;
            profile.record_combined_dispatch(pc + 1);
        }
        if let Some(field) = field {
            // Eligibility established that the slot read has stack space;
            // its following GetField has the same peak and final length.
            self.stack.push(field);
        }
        Some(vm_dispatch::Flow::Jump(target))
    }
}

#[cfg(test)]
mod tests {
    use super::charge_tail;

    #[test]
    fn pair_tail_charges_only_when_both_counters_and_stack_space_allow_it() {
        for bar in [0, 1, 2, 10, u64::MAX] {
            for run in [0, 1, 2, 10, u64::MAX] {
                for len in [0, 1, 2, usize::MAX] {
                    for limit in [0, 1, 2, usize::MAX] {
                        let (mut b, mut r) = (bar, run);
                        let success = charge_tail(&mut b, &mut r, len, limit);
                        let eligible = bar > 0 && run > 0 && len < limit;
                        assert_eq!(success, eligible);
                        assert_eq!(
                            (b, r),
                            if eligible {
                                (bar - 1, run - 1)
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
