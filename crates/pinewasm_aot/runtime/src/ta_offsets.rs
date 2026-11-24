//! Absolute addresses of the three `ta.*` sub-regions.
//!
//! Pure, safe and target-independent, split out of `ta::pinewasm_rt_ta_init`
//! so it can be extracted for verification (the `aeneas` feature). The
//! additions are the same unchecked `u32` additions as before; this crate's
//! release profile has `overflow-checks = false`, so they wrap if the caller
//! breaks the placement contract (`base` plus the planned region fits in
//! linear memory).

use pinewasm_ta::Layout;

/// `(states, cells, scratch)` addresses of `layout`'s regions when its
/// region starts at `base`.
#[inline(always)]
pub(crate) fn region_addresses(base: u32, layout: Layout) -> (u32, u32, u32) {
    let (states_rel, cells_rel, scratch_rel) = layout.offsets();
    (base + states_rel, base + cells_rel, base + scratch_rel)
}
