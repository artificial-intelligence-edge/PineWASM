//! Successful store/current-read pairs. Both logical charges and the
//! store's failure path are preserved; allocating or reference reads delegate.

use crate::{vm_charge, vm_dispatch, vm_slots, Vm};
use pinewasm_csil::{CsilInstr, CsilProgram};
use pinewasm_mir::Value;

#[inline(always)]
pub(crate) fn charge_tail(
    budget: &mut u64,
    remaining: &mut u64,
    stack_len: usize,
    stack_limit: usize,
) -> bool {
    // Store pops one cell; the read restores the incoming depth. Avoid
    // arithmetic on the depth before establishing that a value exists.
    if stack_len == 0 || stack_len > stack_limit || *budget == 0 || *remaining == 0 {
        false
    } else {
        match vm_charge::charge_counters(budget, remaining, 1) {
            Ok(()) => true,
            Err(_) => false,
        }
    }
}

/// Reserve the following current-read by requiring two units, then charge
/// only the previously undispatched store head. The caller observes that
/// instruction entry before the `charge_tail` call charges its read.
/// Complete pair eligibility is established before this function is called.
#[inline(always)]
pub(crate) fn charge_pair_after_math(
    budget: &mut u64,
    remaining: &mut u64,
    stack_len: usize,
    stack_limit: usize,
) -> bool {
    if stack_len == 0 || stack_len > stack_limit || *budget < 2 || *remaining < 2 {
        false
    } else {
        match vm_charge::charge_counters(budget, remaining, 1) {
            Ok(()) => true,
            Err(_) => false,
        }
    }
}

#[inline(always)]
fn primitive(value: &Value) -> Option<Value> {
    match value {
        Value::Na => Some(Value::Na),
        Value::Bool(value) => Some(Value::Bool(*value)),
        Value::Int(value) => Some(Value::Int(*value)),
        Value::Float(value) => Some(Value::Float(*value)),
        Value::Color {
            r,
            g,
            b,
            t,
            input_origin,
        } => Some(Value::Color {
            r: *r,
            g: *g,
            b: *b,
            t: *t,
            input_origin: *input_origin,
        }),
        _ => None,
    }
}

#[inline(always)]
// Expression position so the unit-returning assert is accepted by the lint.
#[allow(clippy::semicolon_if_nothing_returned)]
fn assert_successful_store(stored: bool) {
    debug_assert!(stored)
}

impl Vm {
    #[inline(always)]
    pub(crate) fn exec_store_current_read(
        &mut self,
        program: &CsilProgram,
        slot: u32,
        pc: usize,
        budget: &mut u64,
    ) -> Option<vm_dispatch::Flow> {
        self.exec_store_current_read_in_mode(program, slot, pc, budget, false)
    }

    /// Ordinary dispatch has already charged the store head. Composition
    /// after array math charges that head here only after every success
    /// guard, retaining the two instruction-entry boundaries.
    #[inline(always)]
    pub(crate) fn exec_store_current_read_in_mode(
        &mut self,
        program: &CsilProgram,
        slot: u32,
        pc: usize,
        budget: &mut u64,
        charge_head: bool,
    ) -> Option<vm_dispatch::Flow> {
        let Some(CsilInstr::ReadUserSlot {
            slot: read_slot,
            offset: 0,
        }) = program.main().code.get(pc + 1)
        else {
            return None;
        };
        let len = self.stack.len();
        let required = if charge_head { 2 } else { 1 };
        if len == 0
            || len > self.options.max_stack_depth
            || *budget < required
            || self.instructions_remaining < required
        {
            return None;
        }
        let cell = self.bar & self.slot_mask;
        // A refused store must pop and report its error before any
        // following charge. Validate the complete successful write footprint.
        self.user_slots.get(slot as usize)?.get(cell)?;
        self.written.get(slot as usize)?;
        let read = if *read_slot == slot {
            primitive(self.stack.last()?)?
        } else {
            let value = self
                .user_slots
                .get(*read_slot as usize)
                .and_then(|ring| ring.get(cell))
                .unwrap_or(&Value::Na);
            primitive(value)?
        };
        if charge_head {
            if !charge_pair_after_math(
                budget,
                &mut self.instructions_remaining,
                len,
                self.options.max_stack_depth,
            ) {
                return None;
            }
            #[cfg(test)]
            crate::tests::lane_rows::observe(self, pc);
            if let Some(profile) = self.profile.as_mut() {
                profile.count[pc] += 1;
                profile.units[pc] += 1;
                profile.record_combined_dispatch(pc);
            }
            // The complete success guards reserved two units. No operation
            // since the head charge can alter either counter or the depth,
            // so this tail charge cannot fail after commitment.
            let charged = charge_tail(
                budget,
                &mut self.instructions_remaining,
                len,
                self.options.max_stack_depth,
            );
            let () = debug_assert!(charged, "validated composed read charge");
        } else if !charge_tail(
            budget,
            &mut self.instructions_remaining,
            len,
            self.options.max_stack_depth,
        ) {
            return None;
        }
        let value = self.stack.pop().expect("validated store operand");
        let stored = vm_slots::slot_store(
            &mut self.user_slots,
            &mut self.written,
            self.bar,
            self.slot_mask,
            slot as usize,
            value,
        );
        assert_successful_store(stored);
        #[cfg(test)]
        crate::tests::lane_rows::observe(self, pc + 1);
        if let Some(profile) = self.profile.as_mut() {
            profile.count[pc + 1] += 1;
            profile.units[pc + 1] += 1;
            profile.record_combined_dispatch(pc + 1);
        }
        if let Value::Bool(condition) = &read {
            if let Some(flow) =
                self.exec_store_read_bool_branch(program, *condition, len, pc, budget)
            {
                return Some(flow);
            }
        }
        self.stack.push(read);
        Some(vm_dispatch::Flow::Jump(pc + 2))
    }

    /// The store and its current read are already charged and successful.
    /// A Boolean read followed by its branch needs no temporary stack cell.
    /// The read's peak is the validated incoming store depth; after
    /// the store pops, the retained backing capacity already covers that peak.
    #[inline(never)]
    pub(crate) fn exec_store_read_bool_branch(
        &mut self,
        program: &CsilProgram,
        condition: bool,
        original_depth: usize,
        pc: usize,
        budget: &mut u64,
    ) -> Option<vm_dispatch::Flow> {
        let Some(CsilInstr::JumpIfFalse { target }) = program.main().code.get(pc + 2) else {
            return None;
        };
        if !charge_tail(
            budget,
            &mut self.instructions_remaining,
            original_depth,
            self.options.max_stack_depth,
        ) {
            return None;
        }
        #[cfg(test)]
        {
            self.stack.push(Value::Bool(condition));
            crate::tests::lane_rows::observe(self, pc + 2);
            self.stack.pop();
        }
        if let Some(profile) = self.profile.as_mut() {
            profile.count[pc + 2] += 1;
            profile.units[pc + 2] += 1;
            profile.record_combined_dispatch(pc + 2);
        }
        Some(vm_dispatch::Flow::Jump(if condition {
            pc + 3
        } else {
            *target as usize
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{charge_pair_after_math, charge_tail};

    #[test]
    fn composed_store_gate_reserves_both_units_and_preserves_rejections() {
        for bar in [0, 1, 2, 3, u64::MAX] {
            for run in [0, 1, 2, 3, u64::MAX] {
                for len in [0, 1, 2, usize::MAX] {
                    for limit in [0, 1, 2, usize::MAX] {
                        let (mut b, mut r) = (bar, run);
                        let eligible = len > 0 && len <= limit && bar >= 2 && run >= 2;
                        assert_eq!(charge_pair_after_math(&mut b, &mut r, len, limit), eligible);
                        assert_eq!(
                            (b, r),
                            if eligible {
                                (bar - 1, run - 1)
                            } else {
                                (bar, run)
                            }
                        );
                        if eligible {
                            assert!(charge_tail(&mut b, &mut r, len, limit));
                            assert_eq!((b, r), (bar - 2, run - 2));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn store_read_gate_preserves_every_rejection_and_charges_one_tail() {
        for bar in [0, 1, 2, u64::MAX] {
            for run in [0, 1, 2, u64::MAX] {
                for len in [0, 1, 2, usize::MAX] {
                    for limit in [0, 1, 2, usize::MAX] {
                        let (mut b, mut r) = (bar, run);
                        let eligible = len > 0 && len <= limit && bar > 0 && run > 0;
                        assert_eq!(charge_tail(&mut b, &mut r, len, limit), eligible);
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
