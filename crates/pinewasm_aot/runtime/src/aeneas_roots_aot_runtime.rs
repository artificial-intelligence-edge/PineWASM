//! Charon/Aeneas extraction roots for the `AotRuntime` extraction.
//!
//! Compiled only with the `aeneas` feature, which no checked-in blob enables.
//! The wrappers keep `crate::arena_bounds` reachable (and warning-free) in
//! an `aeneas`-only build; the helpers themselves are the extraction targets.
//! Wrapper names differ from their callees to avoid Aeneas name capture.

/// Root for `crate::arena_bounds::allocation_bounds`.
pub fn root_allocation_bounds(cursor: u32, size: usize, align: usize) -> Option<(u32, u32)> {
    crate::arena_bounds::allocation_bounds(cursor, size, align)
}

/// Root for `crate::arena_bounds::relocation_delta`.
pub fn root_relocation_delta(arena_lo: u32, target: u32) -> Option<i32> {
    crate::arena_bounds::relocation_delta(arena_lo, target)
}

/// Root for `crate::ta_offsets::region_addresses` (needs the `ta` feature).
#[cfg(feature = "ta")]
pub fn root_region_addresses(base: u32, layout: pinewasm_ta::Layout) -> (u32, u32, u32) {
    crate::ta_offsets::region_addresses(base, layout)
}
