//! Aeneas extraction roots for the user-slot cell kernels (`--features aeneas` only).
//!
//! Thin, behavior-free wrappers whose only job is to make Charon translate
//! the user-slot cell kernels in `crate::vm_slots` (what the VM dispatch loop
//! calls for `ReadUserSlot`, `ReadUserSlotDynamic`, `StoreUserSlotCurrent` and
//! `JumpIfSlotWritten`). Generic over the cell type, like the kernels.

use crate::vm_slots;

/// Root for `vm_slots::slot_read`.
pub fn root_slot_read<T: Clone>(
    slots: &[Vec<T>],
    bar: usize,
    offset: usize,
    mask: usize,
    slot: usize,
) -> Option<T> {
    vm_slots::slot_read(slots, bar, offset, mask, slot)
}

/// Root for `vm_slots::slot_store`.
pub fn root_slot_store<T>(
    slots: &mut [Vec<T>],
    written: &mut [bool],
    bar: usize,
    mask: usize,
    slot: usize,
    value: T,
) -> bool {
    vm_slots::slot_store(slots, written, bar, mask, slot, value)
}

/// Root for `vm_slots::slot_written`.
pub fn root_slot_written(written: &[bool], slot: usize) -> Option<bool> {
    vm_slots::slot_written(written, slot)
}
