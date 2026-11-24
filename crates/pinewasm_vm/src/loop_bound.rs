//! Successful loop-direction comparisons with the per-instruction logical charges.
//! Malformed, allocating and resource-limited cases use the general handlers.

use crate::{vm_charge, vm_dispatch, vm_ops, Vm};
use pinewasm_csil::{CsilInstr, CsilProgram};
use pinewasm_mir::Value;

#[inline(always)]
pub(crate) fn charge_tails(
    budget: &mut u64,
    remaining: &mut u64,
    stack_len: usize,
    stack_limit: usize,
) -> bool {
    // The zero head is already charged but unpushed. The unfused sequence
    // reaches three cells above the incoming step operand before Select.
    if stack_len == 0
        || stack_len > stack_limit
        || stack_limit - stack_len < 3
        || *budget < 8
        || *remaining < 8
    {
        false
    } else {
        match vm_charge::charge_counters(budget, remaining, 8) {
            Ok(()) => true,
            Err(_) => false,
        }
    }
}

#[inline(always)]
fn number(value: &Value) -> Option<Value> {
    match value {
        Value::Na => Some(Value::Na),
        Value::Int(value) => Some(Value::Int(*value)),
        Value::Float(value) => Some(Value::Float(*value)),
        _ => None,
    }
}

impl Vm {
    #[inline(never)]
    pub(crate) fn exec_loop_bound(
        &mut self,
        program: &CsilProgram,
        zero: Value,
        pc: usize,
        budget: &mut u64,
    ) -> Option<vm_dispatch::Flow> {
        let [CsilInstr::CmpGe, CsilInstr::ReadUserSlot {
            slot: counter,
            offset: 0,
        }, CsilInstr::ReadUserSlot {
            slot: end,
            offset: 0,
        }, CsilInstr::CmpLe, CsilInstr::ReadUserSlot {
            slot: counter_again,
            offset: 0,
        }, CsilInstr::ReadUserSlot {
            slot: end_again,
            offset: 0,
        }, CsilInstr::CmpGe, CsilInstr::Select] = program.main().code.get(pc..)?.get(1..9)?
        else {
            return None;
        };
        if counter != counter_again || end != end_again {
            return None;
        }
        let len = self.stack.len();
        if len == 0
            || len > self.options.max_stack_depth
            || self.options.max_stack_depth - len < 3
            || self.stack.capacity() - len < 3
            || *budget < 8
            || self.instructions_remaining < 8
        {
            return None;
        }
        let step = number(self.stack.last()?)?;
        let cell = self.bar & self.slot_mask;
        let current = |slot: u32| {
            number(
                self.user_slots
                    .get(slot as usize)
                    .and_then(|ring| ring.get(cell))
                    .unwrap_or(&Value::Na),
            )
        };
        let counter_value = current(*counter)?;
        let end_value = current(*end)?;
        let Value::Bool(ascending) = vm_ops::cmp_values(vm_ops::CmpOp::Ge, &step, &zero).ok()?
        else {
            return None;
        };
        // Both comparisons succeed on the same validated numeric operands.
        // The unused pure comparison has no observable effect or type error.
        let result = vm_ops::cmp_values(
            if ascending {
                vm_ops::CmpOp::Le
            } else {
                vm_ops::CmpOp::Ge
            },
            &counter_value,
            &end_value,
        )
        .ok()?;
        if !charge_tails(
            budget,
            &mut self.instructions_remaining,
            len,
            self.options.max_stack_depth.min(self.stack.capacity()),
        ) {
            return None;
        }
        #[cfg(test)]
        {
            // Test-only: record the stack state at each intermediate instruction.
            self.stack.push(zero);
            crate::tests::lane_rows::observe(self, pc + 1);
            crate::numeric_stack::replace_pair(&mut self.stack, Value::Bool(ascending));
            crate::tests::lane_rows::observe(self, pc + 2);
            self.stack.push(counter_value.clone());
            crate::tests::lane_rows::observe(self, pc + 3);
            self.stack.push(end_value.clone());
            crate::tests::lane_rows::observe(self, pc + 4);
            let up = vm_ops::cmp_values(vm_ops::CmpOp::Le, &counter_value, &end_value).ok()?;
            crate::numeric_stack::replace_pair(&mut self.stack, up);
            crate::tests::lane_rows::observe(self, pc + 5);
            self.stack.push(counter_value.clone());
            crate::tests::lane_rows::observe(self, pc + 6);
            self.stack.push(end_value.clone());
            crate::tests::lane_rows::observe(self, pc + 7);
            let down = vm_ops::cmp_values(vm_ops::CmpOp::Ge, &counter_value, &end_value).ok()?;
            crate::numeric_stack::replace_pair(&mut self.stack, down);
            crate::tests::lane_rows::observe(self, pc + 8);
            let _ = self.stack.pop();
            let _ = self.stack.pop();
        }
        *self.stack.last_mut()? = result;
        if let Some(profile) = self.profile.as_mut() {
            for tail in pc + 1..=pc + 8 {
                profile.count[tail] += 1;
                profile.units[tail] += 1;
                profile.record_combined_dispatch(tail);
            }
        }
        Some(vm_dispatch::Flow::Jump(pc + 9))
    }
}

#[cfg(test)]
mod tests {
    use super::charge_tails;

    #[test]
    fn charge_preserves_rejections_and_original_peak_without_overflow() {
        for bar in [0, 1, 7, 8, 9, u64::MAX] {
            for run in [0, 1, 7, 8, 9, u64::MAX] {
                for len in [0, 1, 2, usize::MAX - 3, usize::MAX] {
                    for limit in [0, 1, 3, 4, 5, usize::MAX] {
                        let eligible =
                            len > 0 && len <= limit && limit - len >= 3 && bar >= 8 && run >= 8;
                        let (mut b, mut r) = (bar, run);
                        assert_eq!(charge_tails(&mut b, &mut r, len, limit), eligible);
                        assert_eq!(
                            (b, r),
                            if eligible {
                                (bar - 8, run - 8)
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
