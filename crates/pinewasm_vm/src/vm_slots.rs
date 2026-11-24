//! User-slot cell kernels behind the CSIL slot opcodes.
//!
//! `ReadUserSlot`, `ReadUserSlotDynamic`, `StoreUserSlotCurrent` and
//! `JumpIfSlotWritten` touch the VM's per-slot history rings
//! (`user_slots[slot][bar & slot_mask]`) and written flags only through these
//! free functions. They are generic over the cell type so Charon/Aeneas can
//! extract them without the VM's `Value` (which carries `f64` and `String`),
//! and they use explicit bounds checks instead of `Option` combinators, the
//! subset Aeneas translates. The `Vm` dispatch loop keeps the error
//! construction and the `na` default, so observable behavior is unchanged.

use crate::vm_charge;

/// The ring cell holding slot `slot`'s value `offset` bars before `bar`, or
/// `None` when that bar precedes the series start or the slot/cell does not
/// exist (the VM then pushes `na`).
// `?` on `Option` would need `Try`/`FromResidual` support in the
// extraction (Aeneas has none), so the early return stays explicit.
#[allow(clippy::question_mark)]
pub(crate) fn slot_read<T: Clone>(
    slots: &[Vec<T>],
    bar: usize,
    offset: usize,
    mask: usize,
    slot: usize,
) -> Option<T> {
    let cell = match vm_charge::ring_read(bar, offset, mask) {
        Some(cell) => cell,
        None => return None,
    };
    if slot < slots.len() && cell < slots[slot].len() {
        Some(slots[slot][cell].clone())
    } else {
        None
    }
}

/// Store `value` into slot `slot`'s current cell (`bar & mask`) and set the
/// slot's written flag. Returns `false`, changing nothing, when the slot or
/// cell does not exist (the VM reports `InvalidProgram`). Panics, like the
/// original indexing, if `written` is shorter than `slots`.
pub(crate) fn slot_store<T>(
    slots: &mut [Vec<T>],
    written: &mut [bool],
    bar: usize,
    mask: usize,
    slot: usize,
    value: T,
) -> bool {
    let cell = bar & mask;
    if slot < slots.len() && cell < slots[slot].len() {
        slots[slot][cell] = value;
        written[slot] = true;
        true
    } else {
        false
    }
}

/// Slot `slot`'s written flag, or `None` when the slot does not exist (the VM
/// reports `InvalidProgram`).
pub(crate) fn slot_written(written: &[bool], slot: usize) -> Option<bool> {
    if slot < written.len() {
        Some(written[slot])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{slot_read, slot_store, slot_written};

    /// Test model of the `ReadUserSlot` cell read (without the `na` default).
    fn read_reference(
        slots: &[Vec<i64>],
        bar: usize,
        offset: usize,
        mask: usize,
        slot: usize,
    ) -> Option<i64> {
        crate::vm_charge::ring_read(bar, offset, mask).and_then(|cell| {
            slots
                .get(slot)
                .and_then(|history| history.get(cell))
                .copied()
        })
    }

    /// Test model of the `StoreUserSlotCurrent` body (success flag only).
    fn store_reference(
        slots: &mut [Vec<i64>],
        written: &mut [bool],
        bar: usize,
        mask: usize,
        slot: usize,
        value: i64,
    ) -> bool {
        let bar = bar & mask;
        let cell = slots.get_mut(slot).and_then(|history| history.get_mut(bar));
        match cell {
            Some(cell) => {
                *cell = value;
                written[slot] = true;
                true
            }
            None => false,
        }
    }

    fn rings(n: usize, ring: usize) -> Vec<Vec<i64>> {
        (0..n)
            .map(|s| (0..ring).map(|c| (s * 100 + c) as i64).collect())
            .collect()
    }

    #[test]
    fn slot_kernels_match_reference() {
        for ring in [1usize, 2, 4, 8] {
            let mask = ring - 1;
            for n in 0..4usize {
                for bar in [0usize, 1, 2, 3, 7, 8, 9, 100, usize::MAX] {
                    for slot in 0..6usize {
                        for offset in [0usize, 1, 2, 3, 7, 8, 101, usize::MAX] {
                            let s = rings(n, ring);
                            assert_eq!(
                                slot_read(&s, bar, offset, mask, slot),
                                read_reference(&s, bar, offset, mask, slot)
                            );
                        }
                        let (mut a, mut b) = (rings(n, ring), rings(n, ring));
                        let (mut wa, mut wb) = (vec![false; n], vec![false; n]);
                        let ra = slot_store(&mut a, &mut wa, bar, mask, slot, -7);
                        let rb = store_reference(&mut b, &mut wb, bar, mask, slot, -7);
                        assert_eq!((ra, a, wa.clone()), (rb, b, wb));
                        assert_eq!(slot_written(&wa, slot), wa.get(slot).copied());
                    }
                }
            }
        }
        // A ring shorter than `mask + 1` (never built by the VM) reads and
        // stores exactly like the baseline too.
        let mut s = vec![vec![1i64, 2]];
        let mut w = vec![false];
        assert_eq!(slot_read(&s, 3, 0, 3, 0), read_reference(&s, 3, 0, 3, 0));
        assert!(!slot_store(&mut s, &mut w, 3, 3, 0, 9));
        assert_eq!((s, w), (vec![vec![1, 2]], vec![false]));
    }
}
