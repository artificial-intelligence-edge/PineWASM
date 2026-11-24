//! Cell kernels behind realtime snapshot/restore (tick rollback).
//!
//! `Vm::snapshot` captures, at a realtime bar's start, the current cell of
//! every non-`varip` user slot, every slot's written flag, and the bar's
//! plot, plot-color, visual-color and fill-color cells; `Vm::restore` writes
//! them back before the next tick on that bar, leaving `varip` cells and their
//! written flags untouched. These free functions are that per-cell logic,
//! generic over the cell type so Charon/Aeneas can extract them without the
//! VM's `Value` (which carries `f64` and `String`). They use index loops only
//! and keep one loop per function, the subset Aeneas translates.
//!
//! The `Vm` methods delegate here and keep the heap, drawing-registry,
//! TA-engine and event-log handling, so the observable behavior is unchanged.
//! The ring index (`bar & slot_mask`) and the `varip` mask (read from the
//! program's `SlotDef`s) are computed by the caller.

/// The captured non-`varip` user-slot cells and written flags of one
/// snapshot.
#[derive(Clone)]
pub struct SlotCells<T> {
    /// One entry per slot: the slot's cell at the snapshot ring index, or
    /// the placeholder for a `varip` slot (restore never reads it).
    pub cells: Vec<T>,
    /// A copy of every slot's written flag, `varip` slots included.
    pub written: Vec<bool>,
}

/// Capture one cell per slot: `slots[i][cell]` for a non-`varip` slot and a
/// clone of `placeholder` for a `varip` one, so indices line up. The result
/// has `varip.len()` entries (the program's slot count). Panics if a
/// non-`varip` slot has no `cell` entry.
pub(crate) fn snapshot_slot_cells<T: Clone>(
    slots: &[Vec<T>],
    varip: &[bool],
    cell: usize,
    placeholder: &T,
) -> Vec<T> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < varip.len() {
        if varip[i] {
            out.push(placeholder.clone());
        } else {
            out.push(slots[i][cell].clone());
        }
        i += 1;
    }
    out
}

/// Copy a written-flag vector.
pub(crate) fn copy_flags(flags: &[bool]) -> Vec<bool> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < flags.len() {
        out.push(flags[i]);
        i += 1;
    }
    out
}

/// Snapshot the user slots: the non-`varip` cells at ring index `cell` and
/// all written flags.
pub(crate) fn snapshot_slots<T: Clone>(
    slots: &[Vec<T>],
    written: &[bool],
    varip: &[bool],
    cell: usize,
    placeholder: &T,
) -> SlotCells<T> {
    SlotCells {
        cells: snapshot_slot_cells(slots, varip, cell, placeholder),
        written: copy_flags(written),
    }
}

/// Restore the user slots: for every non-`varip` slot `i < varip.len()`,
/// `slots[i][cell] := snap.cells[i]` and `written[i] := snap.written[i]`.
/// `varip` cells and flags are left as they are.
pub(crate) fn restore_slots<T: Clone>(
    slots: &mut [Vec<T>],
    written: &mut [bool],
    varip: &[bool],
    cell: usize,
    snap: &SlotCells<T>,
) {
    let mut i = 0;
    while i < varip.len() {
        if !varip[i] {
            slots[i][cell] = snap.cells[i].clone();
            written[i] = snap.written[i];
        }
        i += 1;
    }
}

/// The bar-`bar` cell of every series: `[s[bar] | s <- series]`.
pub(crate) fn snapshot_bar_cells<T: Clone>(series: &[Vec<T>], bar: usize) -> Vec<T> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < series.len() {
        out.push(series[i][bar].clone());
        i += 1;
    }
    out
}

/// [`snapshot_bar_cells`] for each group of series (plot/visual/fill colors,
/// grouped by owner then declared role).
pub(crate) fn snapshot_group_cells<T: Clone>(groups: &[Vec<Vec<T>>], bar: usize) -> Vec<Vec<T>> {
    let mut out = Vec::new();
    let mut g = 0;
    while g < groups.len() {
        out.push(snapshot_bar_cells(&groups[g], bar));
        g += 1;
    }
    out
}

/// Write back `series[i][bar] := cells[i]` for every captured `i`.
pub(crate) fn restore_bar_cells<T: Clone>(series: &mut [Vec<T>], bar: usize, cells: &[T]) {
    let mut i = 0;
    while i < cells.len() {
        series[i][bar] = cells[i].clone();
        i += 1;
    }
}

/// [`restore_bar_cells`] for each captured group.
pub(crate) fn restore_group_cells<T: Clone>(
    groups: &mut [Vec<Vec<T>>],
    bar: usize,
    cells: &[Vec<T>],
) {
    let mut g = 0;
    while g < cells.len() {
        restore_bar_cells(&mut groups[g], bar, &cells[g]);
        g += 1;
    }
}

/// Every per-cell part of a realtime snapshot.
#[derive(Clone)]
pub struct CellSnapshot<T> {
    /// Non-`varip` user-slot cells and all written flags.
    pub slots: SlotCells<T>,
    /// Plot current-bar cell values, one per plot slot.
    pub plot_cells: Vec<T>,
    /// Current-bar plot colors, grouped by plot then declared role.
    pub plot_color_cells: Vec<Vec<T>>,
    /// Current-bar color-only visual values, grouped by visual then role.
    pub visual_color_cells: Vec<Vec<T>>,
    /// Current-bar fill colors, grouped by fill then declared role.
    pub fill_color_cells: Vec<Vec<T>>,
}

/// Capture every per-cell part of a realtime snapshot: user slots at ring
/// index `cell` (non-`varip` only), written flags, and the plot/color cells
/// of bar `bar`. (Plain slice arguments rather than a borrowing struct: Aeneas
/// does not translate type definitions that contain borrows.)
#[allow(clippy::too_many_arguments)]
pub(crate) fn snapshot_cells<T: Clone>(
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
    CellSnapshot {
        slots: snapshot_slots(user_slots, written, varip, cell, placeholder),
        plot_cells: snapshot_bar_cells(plots, bar),
        plot_color_cells: snapshot_group_cells(plot_colors, bar),
        visual_color_cells: snapshot_group_cells(visual_colors, bar),
        fill_color_cells: snapshot_group_cells(fill_colors, bar),
    }
}

/// Write back every per-cell part of a realtime snapshot, in this
/// order: user slots (skipping `varip`), plots, plot colors, visual colors,
/// fill colors.
#[allow(clippy::too_many_arguments)]
pub(crate) fn restore_cells<T: Clone>(
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
    restore_slots(user_slots, written, varip, cell, &snap.slots);
    restore_bar_cells(plots, bar, &snap.plot_cells);
    restore_group_cells(plot_colors, bar, &snap.plot_color_cells);
    restore_group_cells(visual_colors, bar, &snap.visual_color_cells);
    restore_group_cells(fill_colors, bar, &snap.fill_color_cells);
}

/// Test-only per-thread state of the baseline switch below (kept in a
/// `cfg(test)` module so the item-position macro stays out of the crate's
/// API inventory).
#[cfg(test)]
mod reference_state {
    thread_local! {
        pub(super) static REFERENCE_MODE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        pub(super) static REFERENCE_RESTORES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }
}
#[cfg(test)]
use reference_state::{REFERENCE_MODE, REFERENCE_RESTORES};

/// Test-only switch: while set on this thread, `Vm::snapshot`/`Vm::restore`
/// run the baseline bodies (`snapshot_reference`/`restore_reference`),
/// so whole realtime runs can be compared between the two paths.
#[cfg(test)]
pub(crate) fn reference_mode() -> bool {
    REFERENCE_MODE.with(std::cell::Cell::get)
}

/// Count one `restore_reference` call (test-only).
#[cfg(test)]
pub(crate) fn note_reference_restore() {
    REFERENCE_RESTORES.with(|c| c.set(c.get() + 1));
}

/// Run `f` with the baseline bodies switched on; return its result and the
/// number of rollbacks it performed.
#[cfg(test)]
pub(crate) fn with_reference_mode<R>(f: impl FnOnce() -> R) -> (R, usize) {
    REFERENCE_RESTORES.with(|c| c.set(0));
    REFERENCE_MODE.with(|m| m.set(true));
    let out = f();
    REFERENCE_MODE.with(|m| m.set(false));
    (out, REFERENCE_RESTORES.with(std::cell::Cell::get))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pinewasm_mir::Value;

    /// Baseline copy of the per-cell snapshot bodies, over bare state.
    struct Old {
        slot_cells: Vec<Value>,
        written: Vec<bool>,
        plot_cells: Vec<Value>,
        plot_color_cells: Vec<Vec<Value>>,
        visual_color_cells: Vec<Vec<Value>>,
        fill_color_cells: Vec<Vec<Value>>,
    }

    #[derive(Clone, Debug, PartialEq)]
    struct State {
        user_slots: Vec<Vec<Value>>,
        written: Vec<bool>,
        plots: Vec<Vec<Value>>,
        plot_colors: Vec<Vec<Vec<Value>>>,
        visual_colors: Vec<Vec<Vec<Value>>>,
        fill_colors: Vec<Vec<Vec<Value>>>,
    }

    fn old_snapshot(s: &State, varip: &[bool], bar: usize, mask: usize) -> Old {
        let mut slot_cells = Vec::with_capacity(s.user_slots.len());
        for (slot, &is_varip) in varip.iter().enumerate() {
            if is_varip {
                slot_cells.push(Value::Na);
            } else {
                slot_cells.push(s.user_slots[slot][bar & mask].clone());
            }
        }
        let group = |g: &Vec<Vec<Vec<Value>>>| -> Vec<Vec<Value>> {
            g.iter()
                .map(|channels| channels.iter().map(|values| values[bar].clone()).collect())
                .collect()
        };
        Old {
            slot_cells,
            written: s.written.clone(),
            plot_cells: s.plots.iter().map(|p| p[bar].clone()).collect(),
            plot_color_cells: group(&s.plot_colors),
            visual_color_cells: group(&s.visual_colors),
            fill_color_cells: group(&s.fill_colors),
        }
    }

    fn old_restore(s: &mut State, varip: &[bool], bar: usize, mask: usize, snap: &Old) {
        for (slot, &is_varip) in varip.iter().enumerate() {
            if is_varip {
                continue;
            }
            s.user_slots[slot][bar & mask] = snap.slot_cells[slot].clone();
            s.written[slot] = snap.written[slot];
        }
        for (plot, cell) in snap.plot_cells.iter().enumerate() {
            s.plots[plot][bar] = cell.clone();
        }
        for (g, snapg) in [
            (&mut s.plot_colors, &snap.plot_color_cells),
            (&mut s.visual_colors, &snap.visual_color_cells),
            (&mut s.fill_colors, &snap.fill_color_cells),
        ] {
            for (owner, channels) in snapg.iter().enumerate() {
                for (channel, cell) in channels.iter().enumerate() {
                    g[owner][channel][bar] = cell.clone();
                }
            }
        }
    }

    fn new_snapshot(s: &State, varip: &[bool], bar: usize, mask: usize) -> CellSnapshot<Value> {
        snapshot_cells(
            &s.user_slots,
            &s.written,
            &s.plots,
            &s.plot_colors,
            &s.visual_colors,
            &s.fill_colors,
            varip,
            bar & mask,
            bar,
            &Value::Na,
        )
    }

    fn new_restore(
        s: &mut State,
        varip: &[bool],
        bar: usize,
        mask: usize,
        snap: &CellSnapshot<Value>,
    ) {
        restore_cells(
            &mut s.user_slots,
            &mut s.written,
            &mut s.plots,
            &mut s.plot_colors,
            &mut s.visual_colors,
            &mut s.fill_colors,
            varip,
            bar & mask,
            bar,
            snap,
        );
    }

    /// A small deterministic generator (xorshift64*).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: u64) -> usize {
            (self.next() % n) as usize
        }
        fn value(&mut self) -> Value {
            match self.below(6) {
                0 => Value::Na,
                1 => Value::Bool(self.next() & 1 == 1),
                2 => Value::Int(self.next() as i64),
                3 => Value::Float((self.next() % 10_000) as f64 / 7.0),
                4 => Value::String(format!("s{}", self.below(1000))),
                _ => Value::Color {
                    r: self.below(256) as u8,
                    g: self.below(256) as u8,
                    b: self.below(256) as u8,
                    t: self.below(101) as f64,
                    input_origin: None,
                },
            }
        }
        fn series(&mut self, len: usize) -> Vec<Value> {
            (0..len).map(|_| self.value()).collect()
        }
        fn groups(&mut self, len: usize) -> Vec<Vec<Vec<Value>>> {
            (0..self.below(4))
                .map(|_| (0..self.below(4)).map(|_| self.series(len)).collect())
                .collect()
        }
        fn state(&mut self, slots: usize, ring: usize, bars: usize) -> State {
            State {
                user_slots: (0..slots).map(|_| self.series(ring)).collect(),
                written: (0..slots).map(|_| self.next() & 1 == 1).collect(),
                plots: (0..self.below(4)).map(|_| self.series(bars)).collect(),
                plot_colors: self.groups(bars),
                visual_colors: self.groups(bars),
                fill_colors: self.groups(bars),
            }
        }
        /// Overwrite every cell and flag with fresh values, keeping shapes.
        fn scramble(&mut self, s: &mut State) {
            for v in s.user_slots.iter_mut().flatten() {
                *v = self.value();
            }
            for w in &mut s.written {
                *w = self.next() & 1 == 1;
            }
            for v in s.plots.iter_mut().flatten() {
                *v = self.value();
            }
            for g in [&mut s.plot_colors, &mut s.visual_colors, &mut s.fill_colors] {
                for v in g.iter_mut().flatten().flatten() {
                    *v = self.value();
                }
            }
        }
    }

    #[test]
    fn snapshot_and_restore_match_reference_loops() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for _ in 0..2_000 {
            let slots = rng.below(6);
            let ring = 1usize << rng.below(4);
            let bars = 1 + rng.below(6);
            let state = rng.state(slots, ring, bars);
            let varip: Vec<bool> = (0..slots).map(|_| rng.below(3) == 0).collect();
            let bar = rng.below(bars as u64);
            let mask = ring - 1;

            let old = old_snapshot(&state, &varip, bar, mask);
            let new = new_snapshot(&state, &varip, bar, mask);
            assert_eq!(new.slots.cells, old.slot_cells);
            assert_eq!(new.slots.written, old.written);
            assert_eq!(new.plot_cells, old.plot_cells);
            assert_eq!(new.plot_color_cells, old.plot_color_cells);
            assert_eq!(new.visual_color_cells, old.visual_color_cells);
            assert_eq!(new.fill_color_cells, old.fill_color_cells);

            // A tick mutates everything, then both rollbacks run on copies.
            let mut ticked = state.clone();
            rng.scramble(&mut ticked);
            let mut via_old = ticked.clone();
            let mut via_new = ticked.clone();
            old_restore(&mut via_old, &varip, bar, mask, &old);
            new_restore(&mut via_new, &varip, bar, mask, &new);
            assert_eq!(via_new, via_old);
        }
    }

    #[test]
    fn restore_leaves_varip_cells_and_flags() {
        let mut s = State {
            user_slots: vec![vec![Value::Int(1)], vec![Value::Int(2)]],
            written: vec![false, false],
            plots: Vec::new(),
            plot_colors: Vec::new(),
            visual_colors: Vec::new(),
            fill_colors: Vec::new(),
        };
        let varip = [false, true];
        let snap = new_snapshot(&s, &varip, 0, 0);
        assert_eq!(snap.slots.cells, vec![Value::Int(1), Value::Na]);
        s.user_slots[0][0] = Value::Int(10);
        s.user_slots[1][0] = Value::Int(20);
        s.written = vec![true, true];
        new_restore(&mut s, &varip, 0, 0, &snap);
        assert_eq!(
            s.user_slots,
            vec![vec![Value::Int(1)], vec![Value::Int(20)]]
        );
        assert_eq!(s.written, vec![false, true]);
    }
}
