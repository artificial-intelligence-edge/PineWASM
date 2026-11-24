//! The series-slot table kernel behind [`crate::lower`].
//!
//! `lower` gives the program one series slot per HIR symbol, in symbol order,
//! then one per temporary slot the builder allocated (`temp_slot_count`, the
//! lazily lowered ternaries' result slots, named `%t{index}`). The VM sizes
//! its slot rings from the length of that table (`Vm::new_owned` passes
//! `program.series_slots.len()` to `alloc_layout`), so the table is built
//! here by a free function generic over the symbol and slot types:
//! Charon/Aeneas can then extract it without `SlotDef` (which carries a
//! `String`). The host builds each entry; `lower` keeps the `SlotDef`
//! construction and the `format!`, so observable behavior is unchanged.

/// Builds one table entry per symbol and per temporary slot.
pub(crate) trait SlotTableHost<S, D> {
    /// The entry of `symbol`.
    fn symbol_slot(&self, symbol: &S) -> D;
    /// The entry of the temporary slot numbered `index` (from 0).
    fn temp_slot(&self, index: u32) -> D;
}

/// One entry per symbol (in order), then one per temporary slot
/// `0..temp_slot_count` (in order).
pub(crate) fn series_slot_table<S, D, H: SlotTableHost<S, D>>(
    host: &H,
    symbols: &[S],
    temp_slot_count: u32,
) -> Vec<D> {
    let mut slots = Vec::new();
    let mut i = 0;
    while i < symbols.len() {
        slots.push(host.symbol_slot(&symbols[i]));
        i += 1;
    }
    let mut index = 0;
    while index < temp_slot_count {
        slots.push(host.temp_slot(index));
        index += 1;
    }
    slots
}

#[cfg(test)]
mod tests {
    use super::{series_slot_table, SlotTableHost};

    /// Tags each entry with where it came from, so order and provenance are
    /// both checked.
    struct Tagging;

    impl SlotTableHost<u32, (bool, u32)> for Tagging {
        fn symbol_slot(&self, symbol: &u32) -> (bool, u32) {
            (true, *symbol)
        }

        fn temp_slot(&self, index: u32) -> (bool, u32) {
            (false, index)
        }
    }

    /// The direct construction in `lower`: `iter().map().collect()`,
    /// then a `for` loop pushing one entry per temporary.
    fn reference(symbols: &[u32], temp_slot_count: u32) -> Vec<(bool, u32)> {
        let mut series_slots = symbols
            .iter()
            .map(|s| Tagging.symbol_slot(s))
            .collect::<Vec<_>>();
        for index in 0..temp_slot_count {
            series_slots.push(Tagging.temp_slot(index));
        }
        series_slots
    }

    #[test]
    fn series_slot_table_matches_reference() {
        for nsyms in 0..12u32 {
            let symbols: Vec<u32> = (0..nsyms).map(|i| i.wrapping_mul(2_654_435_761)).collect();
            for temps in (0..12).chain([100, 1000]) {
                let table = series_slot_table(&Tagging, &symbols, temps);
                assert_eq!(
                    table,
                    reference(&symbols, temps),
                    "nsyms={nsyms} temps={temps}"
                );
                assert_eq!(table.len(), nsyms as usize + temps as usize);
            }
        }
    }
}
