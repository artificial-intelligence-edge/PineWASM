//! Allocation kernels for the VM's slot rings, written flags and plot rows.
//!
//! `Vm::new_owned` sizes the per-slot history rings, the per-slot written
//! flags and the per-plot output rows, and the realtime driver grows every
//! plot row by one cell when it appends a bar. Those shapes are what the
//! slot and plot kernels (`vm_slots`, `vm_plot_step`) index into, so they
//! live here as free functions generic over the cell type, written in an
//! Aeneas-extractable style that does not depend on the VM's `Value` (which
//! carries `f64` and `String`). The `Vm` keeps constructing `Value::Na` and the
//! `RuntimeError`s, so observable behavior is unchanged.

use crate::vm_charge;

/// The slot rings, written flags and plot rows of a fresh run.
pub(crate) struct LayoutCells<T> {
    /// One ring of `ring` cells per user series slot.
    pub(crate) user_slots: Vec<Vec<T>>,
    /// `ring - 1`, the mask for `bar & slot_mask`.
    pub(crate) slot_mask: usize,
    /// One `false` flag per user series slot.
    pub(crate) written: Vec<bool>,
    /// One row of `bars` cells per plot.
    pub(crate) plots: Vec<Vec<T>>,
}

/// Effective bar count of a run: `min(max_bars, data_bars)`, or `data_bars`
/// when no cap is set.
pub(crate) fn run_bars(max_bars: Option<usize>, data_bars: usize) -> usize {
    match max_bars {
        Some(cap) => cap.min(data_bars),
        None => data_bars,
    }
}

/// History-ring capacity (a power of two) for a slot-history depth, or
/// `None` when the size arithmetic overflows (the VM reports a memory-limit
/// error).
// `Option::map` with a closure would need a closure model in the extraction,
// so the match stays explicit.
#[allow(clippy::manual_map)]
pub(crate) fn ring_cells(depth: u32) -> Option<usize> {
    match vm_charge::ring_size(depth) {
        Some(ring) => Some(ring as usize),
        None => None,
    }
}

/// The ring mask `ring - 1` for `bar & slot_mask`. `ring` is the non-zero
/// result of [`ring_cells`].
#[inline]
pub(crate) fn ring_mask(ring: usize) -> usize {
    ring - 1
}

/// `count` lanes of `len` copies of `fill`.
pub(crate) fn alloc_lanes<T: Clone>(count: usize, len: usize, fill: T) -> Vec<Vec<T>> {
    vec![vec![fill; len]; count]
}

/// `count` cleared written flags.
pub(crate) fn alloc_flags(count: usize) -> Vec<bool> {
    vec![false; count]
}

/// The slot rings (`nslots` rings of `ring` cells), the ring mask, the
/// written flags (one per slot) and the plot rows (`nplots` rows of `bars`
/// cells), every cell `fill`.
pub(crate) fn alloc_layout<T: Clone>(
    nslots: usize,
    ring: usize,
    nplots: usize,
    bars: usize,
    fill: T,
) -> LayoutCells<T> {
    let user_slots = alloc_lanes(nslots, ring, fill.clone());
    let slot_mask = ring_mask(ring);
    let written = alloc_flags(nslots);
    let plots = alloc_lanes(nplots, bars, fill);
    LayoutCells {
        user_slots,
        slot_mask,
        written,
        plots,
    }
}

/// Append one `fill` cell to every lane (a realtime bar append grows every
/// plot row; slot rings are fixed-size and are not grown).
pub(crate) fn append_cells<T: Clone>(lanes: &mut [Vec<T>], fill: T) {
    let mut i = 0;
    while i < lanes.len() {
        lanes[i].push(fill.clone());
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        alloc_flags, alloc_lanes, alloc_layout, append_cells, ring_cells, ring_mask, run_bars,
    };
    use crate::vm_charge;

    /// Reference copy of the inline `Vm::new_owned` bar count.
    fn run_bars_reference(max_bars: Option<usize>, data_bars: usize) -> usize {
        max_bars.map_or(data_bars, |m| m.min(data_bars))
    }

    /// Reference copy of the inline `Vm::new_owned` ring size (without the error).
    fn ring_cells_reference(depth: u32) -> Option<usize> {
        vm_charge::ring_size(depth).map(|ring| ring as usize)
    }

    /// Reference copy of the inline `Vm::new_owned` slot/written/plot allocation.
    #[allow(clippy::type_complexity)]
    fn layout_reference(
        nslots: usize,
        ring: usize,
        nplots: usize,
        bars: usize,
        fill: i64,
    ) -> (Vec<Vec<i64>>, usize, Vec<bool>, Vec<Vec<i64>>) {
        (
            vec![vec![fill; ring]; nslots],
            ring - 1,
            vec![false; nslots],
            vec![vec![fill; bars]; nplots],
        )
    }

    /// Reference copy of the inline realtime-append plot growth.
    fn append_reference(lanes: &mut [Vec<i64>], fill: i64) {
        for series in lanes {
            series.push(fill);
        }
    }

    /// Small deterministic xorshift so the test needs no extra dependency.
    fn next(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[test]
    fn run_bars_matches_reference() {
        let values = [0_usize, 1, 2, 7, 100, 49_000, usize::MAX - 1, usize::MAX];
        for &data in &values {
            assert_eq!(run_bars(None, data), run_bars_reference(None, data));
            for &cap in &values {
                assert_eq!(
                    run_bars(Some(cap), data),
                    run_bars_reference(Some(cap), data),
                    "cap={cap} data={data}"
                );
            }
        }
    }

    #[test]
    fn ring_cells_matches_reference_and_is_pow2() {
        let mut depths: Vec<u32> = (0..=4096).collect();
        for shift in 0..32 {
            let p = 1_u32 << shift;
            depths.extend([p - 1, p, p.saturating_add(1)]);
        }
        depths.extend([u32::MAX - 1, u32::MAX]);
        for depth in depths {
            let got = ring_cells(depth);
            assert_eq!(got, ring_cells_reference(depth), "depth={depth}");
            if let Some(ring) = got {
                assert!(ring.is_power_of_two(), "depth={depth}");
                assert!(ring > depth as usize, "depth={depth}");
                assert_eq!(ring_mask(ring), ring - 1);
            }
        }
    }

    #[test]
    fn alloc_layout_matches_reference() {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut cases = vec![(0, 1, 0, 0), (1, 1, 1, 1), (3, 64, 2, 1000), (0, 8, 5, 0)];
        for _ in 0..200 {
            cases.push((
                (next(&mut state) % 20) as usize,
                1_usize << (next(&mut state) % 8),
                (next(&mut state) % 12) as usize,
                (next(&mut state) % 300) as usize,
            ));
        }
        for (nslots, ring, nplots, bars) in cases {
            let fill = (next(&mut state) % 1000) as i64 - 500;
            let got = alloc_layout(nslots, ring, nplots, bars, fill);
            let want = layout_reference(nslots, ring, nplots, bars, fill);
            assert_eq!(got.user_slots, want.0);
            assert_eq!(got.slot_mask, want.1);
            assert_eq!(got.written, want.2);
            assert_eq!(got.plots, want.3);
            assert_eq!(alloc_lanes(nplots, bars, fill), want.3);
            assert_eq!(alloc_flags(nslots), want.2);
            // The shape the slot and plot kernels rely on.
            assert_eq!(got.user_slots.len(), nslots);
            assert!(got.user_slots.iter().all(|r| r.len() == got.slot_mask + 1));
            assert_eq!(got.written.len(), nslots);
            assert!(got.plots.iter().all(|r| r.len() == bars));
        }
    }

    #[test]
    fn append_cells_matches_reference() {
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        for _ in 0..200 {
            let count = (next(&mut state) % 10) as usize;
            let mut lanes: Vec<Vec<i64>> = (0..count)
                .map(|_| {
                    let len = (next(&mut state) % 20) as usize;
                    (0..len).map(|_| next(&mut state) as i64).collect()
                })
                .collect();
            let mut want = lanes.clone();
            let fill = next(&mut state) as i64;
            append_cells(&mut lanes, fill);
            append_reference(&mut want, fill);
            assert_eq!(lanes, want);
        }
    }
}
