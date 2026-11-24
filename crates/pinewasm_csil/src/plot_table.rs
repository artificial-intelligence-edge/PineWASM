//! The plot-table kernel behind `lower`'s program assembly.
//!
//! The compiled program gets one [`crate::CsilPlot`] per MIR plot, in order.
//! The VM sizes its plot rows from the length of that table (`Vm::new_owned`
//! passes `program.plots.len()` to `alloc_layout`), so the table is built here
//! by a free function generic over the entry types: Charon/Aeneas can then
//! extract it without `PlotInfo`/`CsilPlot` (which carry `String`s). The host
//! builds each entry; the assembly keeps the field copies, so observable
//! behavior is the same as building the entries directly.

/// Builds one compiled table entry from one MIR entry.
pub(crate) trait PlotTableHost<P, Q> {
    /// The compiled entry of `plot`.
    fn plot_entry(&self, plot: &P) -> Q;
}

/// One compiled entry per MIR plot, in order.
pub(crate) fn plot_table<P, Q, H: PlotTableHost<P, Q>>(host: &H, plots: &[P]) -> Vec<Q> {
    let mut table = Vec::new();
    let mut i = 0;
    while i < plots.len() {
        table.push(host.plot_entry(&plots[i]));
        i += 1;
    }
    table
}

#[cfg(test)]
mod tests {
    use super::{plot_table, PlotTableHost};

    struct Doubling;

    impl PlotTableHost<u32, (u32, u64)> for Doubling {
        fn plot_entry(&self, plot: &u32) -> (u32, u64) {
            (*plot, u64::from(*plot) * 2)
        }
    }

    /// The direct construction of the same table:
    /// `iter().map().collect()`.
    fn reference(plots: &[u32]) -> Vec<(u32, u64)> {
        plots.iter().map(|plot| Doubling.plot_entry(plot)).collect()
    }

    #[test]
    fn plot_table_matches_reference() {
        for n in (0..16u32).chain([100, 1000]) {
            let plots: Vec<u32> = (0..n).map(|i| i.wrapping_mul(2_654_435_761)).collect();
            let table = plot_table(&Doubling, &plots);
            assert_eq!(table, reference(&plots), "n={n}");
            assert_eq!(table.len(), plots.len());
        }
    }
}
