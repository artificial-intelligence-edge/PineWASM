//! Aeneas extraction roots for the snapshot kernels (`--features aeneas` only).
//!
//! Each function is a thin, behavior-free wrapper whose only job is to make
//! Charon translate the real snapshot/restore cell kernels in
//! `crate::vm_snapshot`. They are generic over the cell type because the
//! kernels are (the VM instantiates them at `Value`, which stays out of the
//! extraction).

use crate::vm_snapshot::{self, CellSnapshot, SlotCells};

/// Root for `vm_snapshot::snapshot_cells` (what `Vm::snapshot` calls).
#[allow(clippy::too_many_arguments)]
pub fn root_snapshot_cells<T: Clone>(
    user_slots: &[Vec<T>],
    written: &[bool],
    plots: &[Vec<T>],
    plot_colors: &[Vec<Vec<T>>],
    visual_colors: &[Vec<Vec<T>>],
    fill_colors: &[Vec<Vec<T>>],
    varip: &[bool],
    cell: usize,
    bar: usize,
    placeholder: &T,
) -> CellSnapshot<T> {
    vm_snapshot::snapshot_cells(
        user_slots,
        written,
        plots,
        plot_colors,
        visual_colors,
        fill_colors,
        varip,
        cell,
        bar,
        placeholder,
    )
}

/// Root for `vm_snapshot::restore_cells` (what `Vm::restore` calls).
#[allow(clippy::too_many_arguments)]
pub fn root_restore_cells<T: Clone>(
    user_slots: &mut [Vec<T>],
    written: &mut [bool],
    plots: &mut [Vec<T>],
    plot_colors: &mut [Vec<Vec<T>>],
    visual_colors: &mut [Vec<Vec<T>>],
    fill_colors: &mut [Vec<Vec<T>>],
    varip: &[bool],
    cell: usize,
    bar: usize,
    snap: &CellSnapshot<T>,
) {
    vm_snapshot::restore_cells(
        user_slots,
        written,
        plots,
        plot_colors,
        visual_colors,
        fill_colors,
        varip,
        cell,
        bar,
        snap,
    );
}

/// Root for `vm_snapshot::snapshot_slots` (user slots and written flags).
pub fn root_snapshot_slots<T: Clone>(
    slots: &[Vec<T>],
    written: &[bool],
    varip: &[bool],
    cell: usize,
    placeholder: &T,
) -> SlotCells<T> {
    vm_snapshot::snapshot_slots(slots, written, varip, cell, placeholder)
}

/// Root for `vm_snapshot::restore_slots` (user slots and written flags).
pub fn root_restore_slots<T: Clone>(
    slots: &mut [Vec<T>],
    written: &mut [bool],
    varip: &[bool],
    cell: usize,
    snap: &SlotCells<T>,
) {
    vm_snapshot::restore_slots(slots, written, varip, cell, snap);
}

/// Root for `vm_snapshot::snapshot_group_cells` (plot/visual/fill colors).
pub fn root_snapshot_group_cells<T: Clone>(groups: &[Vec<Vec<T>>], bar: usize) -> Vec<Vec<T>> {
    vm_snapshot::snapshot_group_cells(groups, bar)
}

/// Root for `vm_snapshot::restore_group_cells` (plot/visual/fill colors).
pub fn root_restore_group_cells<T: Clone>(
    groups: &mut [Vec<Vec<T>>],
    bar: usize,
    cells: &[Vec<T>],
) {
    vm_snapshot::restore_group_cells(groups, bar, cells);
}
