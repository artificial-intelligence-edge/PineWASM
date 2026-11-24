//! Aeneas root for the bounded stack transition used by the VM's `PushColor` arm.

use crate::vm_stack_push;

/// Apply the same bounded push helper called by `Vm::push` to the actual VM
/// value type. The return bit distinguishes append from refusal.
pub fn root_push_within_limit<T>(stack: &mut Vec<T>, max_stack_depth: usize, value: T) -> bool {
    vm_stack_push::push_within_limit(stack, max_stack_depth, value)
}
