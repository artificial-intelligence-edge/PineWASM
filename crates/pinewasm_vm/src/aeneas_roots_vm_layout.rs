//! Aeneas extraction roots for the layout allocation kernels (`--features aeneas` only).
//!
//! Thin, behavior-free wrappers whose only job is to make Charon translate the
//! allocation kernels in `crate::vm_layout` (what `Vm::new_owned` calls to size
//! the slot rings, the ring mask, the written flags and the plot rows, and what
//! the realtime bar append calls to grow the plot rows). Generic over the cell
//! type, like the kernels.

#![allow(private_interfaces)]

use crate::vm_layout::{self, LayoutCells};

/// Root for `vm_layout::run_bars`.
pub fn root_run_bars(max_bars: Option<usize>, data_bars: usize) -> usize {
    vm_layout::run_bars(max_bars, data_bars)
}

/// Root for `vm_layout::ring_cells`.
pub fn root_ring_cells(depth: u32) -> Option<usize> {
    vm_layout::ring_cells(depth)
}

/// Root for `vm_layout::ring_mask`.
pub fn root_ring_mask(ring: usize) -> usize {
    vm_layout::ring_mask(ring)
}

/// Root for `vm_layout::alloc_lanes`.
pub fn root_alloc_lanes<T: Clone>(count: usize, len: usize, fill: T) -> Vec<Vec<T>> {
    vm_layout::alloc_lanes(count, len, fill)
}

/// Root for `vm_layout::alloc_flags`.
pub fn root_alloc_flags(count: usize) -> Vec<bool> {
    vm_layout::alloc_flags(count)
}

/// Root for `vm_layout::alloc_layout`.
pub fn root_alloc_layout<T: Clone>(
    nslots: usize,
    ring: usize,
    nplots: usize,
    bars: usize,
    fill: T,
) -> LayoutCells<T> {
    vm_layout::alloc_layout(nslots, ring, nplots, bars, fill)
}

/// Root for `vm_layout::append_cells`.
pub fn root_append_cells<T: Clone>(lanes: &mut [Vec<T>], fill: T) {
    vm_layout::append_cells(lanes, fill);
}
