//! Fuse a successful Boolean/na slot read with its following branch. Elide
//! the temporary stack cell only when the original push cannot overflow or
//! allocate; every refused case keeps the original push and next dispatch.

use crate::{vm_charge, vm_dispatch, vm_ops, Vm};
use pinewasm_csil::{CsilInstr, CsilProgram};

/// The read has already completed and been charged. Refusal leaves both
/// counters untouched so the ordinary next dispatch reports exhaustion.
#[inline(always)]
pub(crate) fn charge_tail(budget: &mut u64, remaining: &mut u64) -> bool {
    if *budget == 0 || *remaining == 0 {
        false
    } else {
        match vm_charge::charge_counters(budget, remaining, 1) {
            Ok(()) => true,
            Err(_) => false,
        }
    }
}

impl Vm {
    /// `condition` is the decoded Boolean/na cell read by `ReadUserSlot`.
    /// No opcode is rewritten, so jumps into the branch keep its handler.
    #[inline(always)]
    pub(crate) fn exec_slot_branch(
        &mut self,
        program: &CsilProgram,
        condition: Option<bool>,
        pc: usize,
        budget: &mut u64,
    ) -> Option<vm_dispatch::Flow> {
        let Some(CsilInstr::JumpIfFalse { target }) = program.main().code.get(pc + 1) else {
            return None;
        };
        let len = self.stack.len();
        if len >= self.options.max_stack_depth || len >= self.stack.capacity() {
            return None;
        }
        if !charge_tail(budget, &mut self.instructions_remaining) {
            return None;
        }
        #[cfg(test)]
        {
            // Keep the original logical entry stack for the test trace observer.
            self.stack
                .push(condition.map_or(pinewasm_mir::Value::Na, pinewasm_mir::Value::Bool));
            crate::tests::lane_rows::observe(self, pc + 1);
            let _ = self.stack.pop();
        }
        if let Some(profile) = self.profile.as_mut() {
            profile.count[pc + 1] += 1;
            profile.units[pc + 1] += 1;
            profile.record_combined_dispatch(pc + 1);
        }
        Some(vm_dispatch::Flow::Jump(
            if vm_ops::jump_if_false_taken(condition) {
                *target as usize
            } else {
                pc + 2
            },
        ))
    }
}
