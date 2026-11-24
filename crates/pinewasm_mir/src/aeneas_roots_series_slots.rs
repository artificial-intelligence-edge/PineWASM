//! Aeneas extraction roots for the series-slot table (`--features aeneas`
//! only).
//!
//! A thin, behavior-free wrapper whose only job is to make Charon translate
//! `crate::slot_table::series_slot_table` (the series-slot table `lower`
//! builds, whose length the VM sizes its slot rings from). Generic over the
//! symbol and entry types, like the kernel.

use crate::slot_table::{self, SlotTableHost};

/// Root for `slot_table::series_slot_table`.
pub(crate) fn root_series_slot_table<S, D, H: SlotTableHost<S, D>>(
    host: &H,
    symbols: &[S],
    temp_slot_count: u32,
) -> Vec<D> {
    slot_table::series_slot_table(host, symbols, temp_slot_count)
}
