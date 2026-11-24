//! Aeneas extraction roots for the plot table (`--features aeneas`
//! only).
//!
//! A thin, behavior-free wrapper whose only job is to make Charon translate
//! `crate::plot_table::plot_table` (the compiled plot table, whose length the
//! VM sizes its plot rows from). Generic over the entry types, like the
//! kernel.

use crate::plot_table::{self, PlotTableHost};

/// Root for `plot_table::plot_table`.
pub(crate) fn root_plot_table<P, Q, H: PlotTableHost<P, Q>>(host: &H, plots: &[P]) -> Vec<Q> {
    plot_table::plot_table(host, plots)
}
