//! Pure layout arithmetic behind the `prepare` bar cap.
//!
//! [`LayoutFootprint`] holds the exact compile-time coefficients of the memory
//! regions `emit_prepare` lays out, and derives from them the largest bar count
//! whose layout (and, for standalone WebAssembly, relocation target) fits the
//! declared memory. The module deliberately has no `wasm-encoder`, `CsilProgram`
//! or `AotError` dependency and stays inside the subset of Rust that Aeneas
//! can translate to Lean: no floats, closures, `?` on `Option`, or
//! `Option`/`Result` combinators.
//! [`footprint_of_counts`] computes the footprint from the program's layout
//! counts; the wrapper that reads those counts off `CsilProgram`/`Layout`
//! (`layout_footprint`) and the `AotError` mapping live in `emit.rs`.

/// Declared `maximum` on the emitted memory, in 64 KiB pages (256 MiB).
///
/// A module without a declared `maximum` is open-ended, which a sandboxing
/// host is entitled to reject outright: guidance for running untrusted wasm is
/// to refuse or pre-bound growth-sensitive constructs, and a module that never
/// says how far it will grow cannot be statically checked against a host's
/// limits. Declaring a bound makes the artifact self-describing, so a loader
/// can compare it against its own `StoreLimits` before instantiating rather
/// than discovering the answer at `memory.grow` time.
///
/// This is a *policy* capacity. The per-module `prepare` bar limit is derived
/// from the exact layout and this declared capacity; the separate wasm32
/// address-space limit and page-rounding arithmetic are also checked. The
/// declared capacity is independent of workload size or host process memory.
/// Each candidate layout must fit the checked bound; admission does not
/// guarantee that the host engine can satisfy a later growth request.
///
/// Two things this does **not** change. The host's `StoreLimits` remains the
/// operative enforcement, since a declared maximum binds only the guest. And
/// the finer-grained bound is still the loader-written element budget
/// (`OFF_MAX_ELEMS` / `OFF_MAX_OBJS`), which can reject allocations before
/// linear memory is exhausted. Both limits must be respected.
///
/// `prepare` checks the exact program-specific layout before any i32 address
/// arithmetic, then returns [`crate::status::ALLOC_FAILED`] if a host engine still
/// refuses a growth request within the module's declared maximum.
pub(crate) const MEMORY_MAX_PAGES: u64 = 4096;

/// Linked desktop/chart modules also carry the deserialized host plan and
/// high-resolution graphics heaps. Bound their dynamic growth at 1 GiB;
/// standalone artifacts and the checked static lane layout keep 256 MiB.
pub(crate) const CHART_MEMORY_MAX_PAGES: u64 = 16_384;

/// Gap kept between the run layout's high-water mark and the relocated
/// arena band, so off-by-slack layout arithmetic cannot clip the band.
pub(crate) const STANDALONE_WASM_ARENA_PAD: u32 = 64;

/// Bytes in one WebAssembly linear-memory page.
const WASM_PAGE_BYTES: u64 = 65_536;
/// Bytes in the full wasm32 address space.
const WASM32_BYTES: u64 = 1u64 << 32;

/// `n` rounded up to a multiple of eight, or `None` if that overflows `u64`.
// An explicit `match` rather than `Option::map`, which Aeneas does not model.
#[allow(clippy::manual_map)]
pub(crate) fn align8_u64(n: u64) -> Option<u64> {
    match n.checked_add(7) {
        Some(value) => Some(value & !7),
        None => None,
    }
}

/// Unwrap an `Option`, returning `None` from the enclosing function when it
/// is empty. Expands to a plain `match`, unlike `?`, which goes through the
/// `Try` trait.
macro_rules! some_or_return_none {
    ($value:expr) => {
        match $value {
            Some(value) => value,
            None => return None,
        }
    };
}

/// Exact compile-time coefficients for the memory regions computed by
/// `emit_prepare`. Keeping these as u64 until the final admission decision
/// lets us derive a per-program bar cap without reproducing Wasm i32 wrap.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LayoutFootprint {
    pub(crate) base: u64,
    pub(crate) builtin_bytes_per_bar: u64,
    pub(crate) plot_bytes_per_bar: u64,
    pub(crate) color_bytes_per_bar: u64,
    pub(crate) alert_bytes_per_bar: u64,
    pub(crate) slot_bytes: u64,
    pub(crate) slot_count: u64,
    pub(crate) scratch_bytes: u64,
    pub(crate) ta_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct StandaloneRelocationBound {
    pub(crate) per_bar: u32,
    pub(crate) fixed: u32,
}

/// Why [`LayoutFootprint::max_bars`] admits no bar count at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MaxBarsError {
    /// Even zero bars exceed the declared memory (or overflow `u64`).
    FixedLayoutExceedsMemory,
}

impl LayoutFootprint {
    pub(crate) fn heap_end(self, bars: u64) -> Option<u64> {
        let builtin_bytes = some_or_return_none!(self.builtin_bytes_per_bar.checked_mul(bars));
        let builtin_end = some_or_return_none!(self.base.checked_add(builtin_bytes));
        let slots_end = some_or_return_none!(builtin_end.checked_add(self.slot_bytes));
        let written_end = some_or_return_none!(slots_end.checked_add(self.slot_count));
        let plots_base = some_or_return_none!(align8_u64(written_end));
        let plot_bytes = some_or_return_none!(self.plot_bytes_per_bar.checked_mul(bars));
        let colors_base = some_or_return_none!(plots_base.checked_add(plot_bytes));
        let color_bytes = some_or_return_none!(self.color_bytes_per_bar.checked_mul(bars));
        let colors_end = some_or_return_none!(colors_base.checked_add(color_bytes));
        let scratch_base = some_or_return_none!(align8_u64(colors_end));
        let scratch_end = some_or_return_none!(scratch_base.checked_add(self.scratch_bytes));
        let alert_bytes = some_or_return_none!(self.alert_bytes_per_bar.checked_mul(bars));
        let alerts_end = some_or_return_none!(scratch_end.checked_add(alert_bytes));
        alerts_end.checked_add(self.ta_bytes)
    }

    pub(crate) fn standalone_relocation(self) -> Option<StandaloneRelocationBound> {
        let builtin_plot = some_or_return_none!(self
            .builtin_bytes_per_bar
            .checked_add(self.plot_bytes_per_bar));
        let with_colors = some_or_return_none!(builtin_plot.checked_add(self.color_bytes_per_bar));
        let per_bar = some_or_return_none!(with_colors.checked_add(self.alert_bytes_per_bar));
        let with_slots = some_or_return_none!(self.base.checked_add(self.slot_bytes));
        let with_slot_count = some_or_return_none!(with_slots.checked_add(self.slot_count));
        let with_scratch = some_or_return_none!(with_slot_count.checked_add(self.scratch_bytes));
        let with_ta = some_or_return_none!(with_scratch.checked_add(self.ta_bytes));
        // Two eight-byte alignments bound the exact layout's combined
        // alignment slack; the pad separates that layout from the input
        // arena relocated above it.
        let with_slack = some_or_return_none!(with_ta.checked_add(16));
        let fixed = some_or_return_none!(with_slack.checked_add(STANDALONE_WASM_ARENA_PAD as u64));
        if per_bar > u32::MAX as u64 || fixed > u32::MAX as u64 {
            return None;
        }
        Some(StandaloneRelocationBound {
            per_bar: per_bar as u32,
            fixed: fixed as u32,
        })
    }

    /// Whether `bars` bars fit `memory_limit`: the exact layout's heap end
    /// and, when present, the standalone relocation target.
    fn fits(
        self,
        relocation: Option<StandaloneRelocationBound>,
        memory_limit: u64,
        bars: u64,
    ) -> bool {
        let heap_fits = match self.heap_end(bars) {
            Some(end) => end <= memory_limit,
            None => false,
        };
        if !heap_fits {
            return false;
        }
        match relocation {
            None => true,
            Some(bound) => match (bound.per_bar as u64).checked_mul(bars) {
                None => false,
                Some(scaled) => match (bound.fixed as u64).checked_add(scaled) {
                    None => false,
                    Some(end) => end <= memory_limit,
                },
            },
        }
    }

    /// The largest bar count in `0..=u32::MAX` that [`Self::fits`] admits.
    pub(crate) fn max_bars(
        self,
        relocation: Option<StandaloneRelocationBound>,
    ) -> Result<u32, MaxBarsError> {
        // `prepare` rounds heap_end up by adding 0xFFFF in i32. Keep that
        // addition representable even if the declared memory limit is later
        // raised toward the wasm32 ceiling.
        let page_rounding_limit = WASM32_BYTES - WASM_PAGE_BYTES;
        let memory_limit = (MEMORY_MAX_PAGES * WASM_PAGE_BYTES).min(page_rounding_limit);

        if !self.fits(relocation, memory_limit, 0) {
            return Err(MaxBarsError::FixedLayoutExceedsMemory);
        }

        // The exact layout and (for standalone WebAssembly) its relocation
        // target are monotone in `bars`, so binary search finds the largest
        // valid value without introducing another hard-coded bar threshold.
        let mut low = 0u64;
        let mut high = u32::MAX as u64;
        while low < high {
            // `low + ceil((high - low) / 2)`, spelled without `div_ceil`.
            let span = high - low;
            let middle = low + span / 2 + span % 2;
            if self.fits(relocation, memory_limit, middle) {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        Ok(low as u32)
    }
}

/// The six builtin lanes (open, high, low, close, volume, time) before the
/// `input.source` lanes.
pub(crate) const BUILTIN_LANE_COUNT: u32 = 6;

/// The program and plan counts the footprint is computed from, read off
/// `CsilProgram`, `Layout` and `SlotLayout` by `layout_footprint` in
/// `emit.rs` (each field names its source).
#[derive(Debug, Clone, Copy)]
pub(crate) struct FootprintCounts {
    /// `program.inputs.len()`.
    pub(crate) inputs: u64,
    /// The lane floor `emit_module` computes above a linked runtime.
    pub(crate) lane_floor: u32,
    /// `layout.source_inputs.len()`.
    pub(crate) source_inputs: u64,
    /// `total_color_lanes(layout)`; `None` when that count overflows.
    pub(crate) color_lanes: Option<u64>,
    /// `layout.plot_count`.
    pub(crate) plot_count: u32,
    /// `layout.alert_sites`.
    pub(crate) alert_sites: u32,
    /// `SlotLayout::bytes`.
    pub(crate) slot_bytes: u32,
    /// `layout.slot_count`.
    pub(crate) slot_count: u32,
    /// `layout.scratch_cells`.
    pub(crate) scratch_cells: u32,
    /// `ta_region_bytes(layout)`.
    pub(crate) ta_bytes: u32,
}

/// Which per-bar stride [`FootprintError::StrideOutOfRange`] refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StrideName {
    Builtin,
    Plot,
    Color,
    Alert,
}

/// Why [`footprint_of_counts`] builds no footprint. `emit.rs` maps each
/// variant to its `AotError` message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FootprintError {
    InputTableOverflow,
    BuiltinLaneCountOverflow,
    ColorLaneCountOverflow,
    BuiltinStrideOverflow,
    PlotStrideOverflow,
    ColorStrideOverflow,
    AlertStrideOverflow,
    ScratchSizeOverflow,
    /// A per-bar coefficient exceeds `i32::MAX`: `prepare` emits it as a
    /// nonnegative `i32.const` multiplied in `i32`.
    StrideOutOfRange(StrideName),
}

/// `i32::MAX` as a `u64`, spelled as a literal rather than a cast.
const I32_MAX_U64: u64 = 0x7FFF_FFFF;

/// Unwrap an `Option`, returning `Err($error)` from the enclosing function
/// when it is empty. Expands to a plain `match`, unlike `ok_or(..)?`.
macro_rules! some_or_return_err {
    ($value:expr, $error:expr) => {
        match $value {
            Some(value) => value,
            None => return Err($error),
        }
    };
}

/// The exact layout coefficients of a program with `counts`, checked in the
/// order `emit.rs`'s `layout_footprint` uses: every product and sum in `u64`,
/// and every per-bar stride at most `i32::MAX`.
pub(crate) fn footprint_of_counts(
    counts: FootprintCounts,
) -> Result<LayoutFootprint, FootprintError> {
    let input_table = some_or_return_err!(
        counts.inputs.checked_mul(8),
        FootprintError::InputTableOverflow
    );
    let input_bytes = some_or_return_err!(
        input_table.checked_add(crate::header::SIZE as u64),
        FootprintError::InputTableOverflow
    );
    let lane_floor = counts.lane_floor as u64;
    let base = if input_bytes < lane_floor {
        lane_floor
    } else {
        input_bytes
    };
    let builtin_lanes = some_or_return_err!(
        (BUILTIN_LANE_COUNT as u64).checked_add(counts.source_inputs),
        FootprintError::BuiltinLaneCountOverflow
    );
    let color_lanes =
        some_or_return_err!(counts.color_lanes, FootprintError::ColorLaneCountOverflow);
    let builtin_bytes_per_bar = some_or_return_err!(
        builtin_lanes.checked_mul(8),
        FootprintError::BuiltinStrideOverflow
    );
    let plot_bytes_per_bar = some_or_return_err!(
        (counts.plot_count as u64).checked_mul(8),
        FootprintError::PlotStrideOverflow
    );
    let color_bytes_per_bar = some_or_return_err!(
        color_lanes.checked_mul(4),
        FootprintError::ColorStrideOverflow
    );
    let alert_bytes_per_bar = some_or_return_err!(
        (counts.alert_sites as u64).checked_mul(crate::header::ALERT_ENTRY as u64),
        FootprintError::AlertStrideOverflow
    );
    let scratch_bytes = some_or_return_err!(
        (counts.scratch_cells as u64).checked_mul(8),
        FootprintError::ScratchSizeOverflow
    );
    // These coefficients are emitted as nonnegative i32 constants and used
    // by i32 multiplication. If one is too large, no positive bar count can
    // fit the declared memory; reject such a plan rather than truncating a
    // compile-time factor before `prepare` can guard it.
    if builtin_bytes_per_bar > I32_MAX_U64 {
        return Err(FootprintError::StrideOutOfRange(StrideName::Builtin));
    }
    if plot_bytes_per_bar > I32_MAX_U64 {
        return Err(FootprintError::StrideOutOfRange(StrideName::Plot));
    }
    if color_bytes_per_bar > I32_MAX_U64 {
        return Err(FootprintError::StrideOutOfRange(StrideName::Color));
    }
    if alert_bytes_per_bar > I32_MAX_U64 {
        return Err(FootprintError::StrideOutOfRange(StrideName::Alert));
    }
    Ok(LayoutFootprint {
        base,
        builtin_bytes_per_bar,
        plot_bytes_per_bar,
        color_bytes_per_bar,
        alert_bytes_per_bar,
        slot_bytes: counts.slot_bytes as u64,
        slot_count: counts.slot_count as u64,
        scratch_bytes,
        ta_bytes: counts.ta_bytes as u64,
    })
}

/// Free-function extraction roots for Charon/Aeneas, which cannot start from
/// inherent methods at the pinned Charon. Each one only delegates.
#[cfg(feature = "aeneas")]
#[allow(dead_code)]
pub(crate) mod aeneas_roots {
    use super::{
        align8_u64, FootprintCounts, FootprintError, LayoutFootprint, MaxBarsError,
        StandaloneRelocationBound,
    };

    pub(crate) fn footprint_of_counts(
        counts: FootprintCounts,
    ) -> Result<LayoutFootprint, FootprintError> {
        super::footprint_of_counts(counts)
    }

    pub(crate) fn align8(n: u64) -> Option<u64> {
        align8_u64(n)
    }

    pub(crate) fn heap_end(footprint: LayoutFootprint, bars: u64) -> Option<u64> {
        footprint.heap_end(bars)
    }

    pub(crate) fn standalone_relocation(
        footprint: LayoutFootprint,
    ) -> Option<StandaloneRelocationBound> {
        footprint.standalone_relocation()
    }

    pub(crate) fn fits(
        footprint: LayoutFootprint,
        relocation: Option<StandaloneRelocationBound>,
        memory_limit: u64,
        bars: u64,
    ) -> bool {
        footprint.fits(relocation, memory_limit, bars)
    }

    pub(crate) fn max_bars(
        footprint: LayoutFootprint,
        relocation: Option<StandaloneRelocationBound>,
    ) -> Result<u32, MaxBarsError> {
        footprint.max_bars(relocation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An independent implementation using `?` and `Option` combinators,
    /// returning `()` instead of `AotError`, kept as the differential check
    /// for the Aeneas-friendly version above.
    mod reference {
        use super::super::{
            LayoutFootprint, StandaloneRelocationBound, MEMORY_MAX_PAGES, STANDALONE_WASM_ARENA_PAD,
        };

        pub(super) fn align8_u64(n: u64) -> Option<u64> {
            n.checked_add(7).map(|value| value & !7)
        }

        pub(super) fn heap_end(footprint: LayoutFootprint, bars: u64) -> Option<u64> {
            let builtin_end = footprint
                .base
                .checked_add(footprint.builtin_bytes_per_bar.checked_mul(bars)?)?;
            let written_end = builtin_end
                .checked_add(footprint.slot_bytes)?
                .checked_add(footprint.slot_count)?;
            let plots_base = align8_u64(written_end)?;
            let colors_base =
                plots_base.checked_add(footprint.plot_bytes_per_bar.checked_mul(bars)?)?;
            let scratch_base = align8_u64(
                colors_base.checked_add(footprint.color_bytes_per_bar.checked_mul(bars)?)?,
            )?;
            scratch_base
                .checked_add(footprint.scratch_bytes)?
                .checked_add(footprint.alert_bytes_per_bar.checked_mul(bars)?)?
                .checked_add(footprint.ta_bytes)
        }

        pub(super) fn standalone_relocation(
            footprint: LayoutFootprint,
        ) -> Option<StandaloneRelocationBound> {
            let per_bar = footprint
                .builtin_bytes_per_bar
                .checked_add(footprint.plot_bytes_per_bar)?
                .checked_add(footprint.color_bytes_per_bar)?
                .checked_add(footprint.alert_bytes_per_bar)?;
            let fixed = footprint
                .base
                .checked_add(footprint.slot_bytes)?
                .checked_add(footprint.slot_count)?
                .checked_add(footprint.scratch_bytes)?
                .checked_add(footprint.ta_bytes)?
                .checked_add(16)?
                .checked_add(u64::from(STANDALONE_WASM_ARENA_PAD))?;
            Some(StandaloneRelocationBound {
                per_bar: u32::try_from(per_bar).ok()?,
                fixed: u32::try_from(fixed).ok()?,
            })
        }

        pub(super) fn max_bars(
            footprint: LayoutFootprint,
            relocation: Option<StandaloneRelocationBound>,
        ) -> Result<u32, ()> {
            const WASM_PAGE_BYTES: u64 = 65_536;
            const WASM32_BYTES: u64 = 1u64 << 32;

            let page_rounding_limit = WASM32_BYTES - WASM_PAGE_BYTES;
            let memory_limit = (MEMORY_MAX_PAGES * WASM_PAGE_BYTES).min(page_rounding_limit);
            let fits = |bars: u64| {
                heap_end(footprint, bars).is_some_and(|end| end <= memory_limit)
                    && relocation.is_none_or(|bound| {
                        u64::from(bound.per_bar)
                            .checked_mul(bars)
                            .and_then(|scaled| u64::from(bound.fixed).checked_add(scaled))
                            .is_some_and(|end| end <= memory_limit)
                    })
            };

            if !fits(0) {
                return Err(());
            }

            let mut low = 0u64;
            let mut high = u64::from(u32::MAX);
            while low < high {
                let middle = low + (high - low).div_ceil(2);
                if fits(middle) {
                    low = middle;
                } else {
                    high = middle - 1;
                }
            }
            Ok(low as u32)
        }
    }

    fn footprint(values: [u64; 9]) -> LayoutFootprint {
        LayoutFootprint {
            base: values[0],
            builtin_bytes_per_bar: values[1],
            plot_bytes_per_bar: values[2],
            color_bytes_per_bar: values[3],
            alert_bytes_per_bar: values[4],
            slot_bytes: values[5],
            slot_count: values[6],
            scratch_bytes: values[7],
            ta_bytes: values[8],
        }
    }

    fn relocation_pair(bound: Option<StandaloneRelocationBound>) -> Option<(u32, u32)> {
        bound.map(|bound| (bound.per_bar, bound.fixed))
    }

    /// Compare every item against the baseline on one footprint.
    fn assert_matches_reference(fp: LayoutFootprint, bar_samples: &[u64]) {
        for &bars in bar_samples {
            assert_eq!(
                fp.heap_end(bars),
                reference::heap_end(fp, bars),
                "heap_end({bars}) for {fp:?}"
            );
        }
        let relocation = fp.standalone_relocation();
        assert_eq!(
            relocation_pair(relocation),
            relocation_pair(reference::standalone_relocation(fp)),
            "standalone_relocation for {fp:?}"
        );
        let candidates = [
            None,
            relocation,
            Some(StandaloneRelocationBound {
                per_bar: 0,
                fixed: 0,
            }),
            Some(StandaloneRelocationBound {
                per_bar: u32::MAX,
                fixed: u32::MAX,
            }),
            Some(StandaloneRelocationBound {
                per_bar: 1,
                fixed: 0,
            }),
        ];
        for candidate in candidates {
            assert_eq!(
                fp.max_bars(candidate).ok(),
                reference::max_bars(fp, candidate).ok(),
                "max_bars({:?}) for {fp:?}",
                relocation_pair(candidate)
            );
        }
    }

    /// Bar counts around every boundary the layout arithmetic can hit.
    fn bar_samples(extra: &[u64]) -> Vec<u64> {
        let mut probes = vec![
            0,
            1,
            2,
            7,
            8,
            4_793_486,
            4_793_487,
            u64::from(u32::MAX) - 1,
            u64::from(u32::MAX),
            u64::from(u32::MAX) + 1,
            u64::MAX / 8,
            u64::MAX / 2,
            u64::MAX - 1,
            u64::MAX,
        ];
        probes.extend_from_slice(extra);
        probes
    }

    #[test]
    fn align8_matches_reference_on_edges() {
        for n in (0..64).chain(u64::MAX - 16..=u64::MAX) {
            assert_eq!(align8_u64(n), reference::align8_u64(n), "align8_u64({n})");
        }
    }

    #[test]
    fn edge_footprints_match_reference() {
        let fixed_limit = MEMORY_MAX_PAGES * WASM_PAGE_BYTES;
        let edges = [
            0,
            1,
            7,
            8,
            9,
            16,
            64,
            i32::MAX as u64,
            u64::from(u32::MAX) - 80,
            u64::from(u32::MAX),
            u64::from(u32::MAX) + 1,
            fixed_limit - 1,
            fixed_limit,
            fixed_limit + 1,
            u64::MAX - 7,
            u64::MAX,
        ];
        // Each coefficient in turn takes each edge value over a small,
        // realistic background layout.
        let background = [96, 48, 8, 4, 0, 64, 3, 32, 0];
        for field in 0..9 {
            for &edge in &edges {
                let mut values = background;
                values[field] = edge;
                let fp = footprint(values);
                assert_matches_reference(fp, &bar_samples(&[]));
            }
        }
        assert_matches_reference(footprint([0; 9]), &bar_samples(&[]));
        assert_matches_reference(footprint([u64::MAX; 9]), &bar_samples(&[]));
        // A small, realistic layout checked as a fixed case.
        assert_matches_reference(footprint([96, 48, 8, 0, 0, 0, 0, 32, 0]), &bar_samples(&[]));
    }

    #[test]
    fn deterministic_sweep_matches_reference() {
        // xorshift64*: deterministic and dependency-free.
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        for _ in 0..4_000 {
            let mut values = [0u64; 9];
            for value in &mut values {
                let raw = next();
                // Mix magnitudes: most coefficients realistic, some near the
                // memory limit, a few arbitrary u64 values.
                *value = match raw % 8 {
                    0..=3 => raw >> 52,
                    4 | 5 => raw >> 36,
                    6 => raw >> 28,
                    _ => raw,
                };
            }
            // Per-bar strides are multiples of the lane widths in production.
            values[1] &= !7;
            values[2] &= !7;
            values[3] &= !3;
            let fp = footprint(values);
            let random_bars = [next() >> 40, next() >> 20, next()];
            let mut probes = bar_samples(&random_bars);
            if let Ok(maximum) = reference::max_bars(fp, None) {
                probes.push(u64::from(maximum));
                probes.push(u64::from(maximum) + 1);
            }
            assert_matches_reference(fp, &probes);
        }
    }

    #[test]
    fn fixed_layout_overflow_reports_the_error_variant() {
        let fp = footprint([u64::MAX, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            fp.max_bars(None),
            Err(MaxBarsError::FixedLayoutExceedsMemory)
        );
    }
}
