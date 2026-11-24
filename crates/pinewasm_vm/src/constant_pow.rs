//! A constant exponent and successful scalar pow, with both logical charges.

use crate::{numeric_call, store_read, vm_dispatch, vm_ops, Vm};
use pinewasm_csil::{CsilInstr, CsilProgram};
use pinewasm_hir::PureFunc;
use pinewasm_mir::Value;

impl Vm {
    #[inline(always)]
    pub(crate) fn exec_constant_pow(
        &mut self,
        program: &CsilProgram,
        exponent: f64,
        pc: usize,
        budget: &mut u64,
    ) -> Option<vm_dispatch::Flow> {
        if !matches!(
            program.main().code.get(pc + 1),
            Some(CsilInstr::CallPure {
                func: PureFunc::MathPow,
                argc: 2,
            })
        ) {
            return None;
        }
        let len = self.stack.len();
        // The original push must succeed without growing backing storage.
        // Refused pushes, type errors and short fuel retain their first error
        // and original intermediate frame through ordinary dispatch.
        if len == 0
            || len >= self.options.max_stack_depth
            || len >= self.stack.capacity()
            || *budget == 0
            || self.instructions_remaining == 0
        {
            return None;
        }
        let base = match &self.stack[len - 1] {
            Value::Na => Value::Na,
            Value::Int(value) => Value::Int(*value),
            Value::Float(value) => Value::Float(*value),
            _ => return None,
        };
        if !store_read::charge_tail(
            budget,
            &mut self.instructions_remaining,
            len + 1,
            self.options.max_stack_depth,
        ) {
            return None;
        }
        let exponent = vm_ops::const_value(vm_ops::VmConst::Float(exponent));
        let result = numeric_call::evaluate(PureFunc::MathPow, &[base, exponent.clone()])
            .expect("validated scalar pow operands");
        #[cfg(test)]
        {
            self.stack.push(exponent);
            crate::tests::lane_rows::observe(self, pc + 1);
            self.stack.pop();
        }
        self.stack[len - 1] = result;
        if let Some(profile) = self.profile.as_mut() {
            profile.count[pc + 1] += 1;
            profile.units[pc + 1] += 1;
            profile.record_combined_dispatch(pc + 1);
        }
        Some(vm_dispatch::Flow::Jump(pc + 2))
    }
}
