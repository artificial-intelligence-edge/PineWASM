//! Test-only copy of the `layout_footprint` constructor (from emit.rs),
//! unchanged apart from its name and visibility, kept as a cross-check for
//! the `footprint_of_counts` kernel: every test-build emit compares the two
//! (`assert_matches`), and `counts_sweep_matches_reference` compares the
//! kernel with the same arithmetic over synthetic counts.

use super::*;

pub(super) fn layout_footprint_reference(
    program: &CsilProgram,
    layout: &Layout,
    slots: &SlotLayout,
    lane_floor: u32,
) -> Result<LayoutFootprint, AotError> {
    let input_bytes = u64::try_from(program.inputs.len())
        .ok()
        .and_then(|count| count.checked_mul(8))
        .and_then(|bytes| bytes.checked_add(u64::from(header::SIZE)))
        .ok_or_else(|| AotError::UnsupportedOpcode("input-table layout overflows".into()))?;
    let base = input_bytes.max(u64::from(lane_floor));
    let builtin_lanes = u64::from(BUILTIN_LANE_COUNT)
        .checked_add(layout.source_inputs.len() as u64 + u64::from(layout.security_lanes))
        .ok_or_else(|| AotError::UnsupportedOpcode("builtin-lane count overflows".into()))?;
    let color_lanes = total_color_lanes(layout)
        .ok_or_else(|| AotError::UnsupportedOpcode("color-lane count overflows".into()))?;
    let footprint = LayoutFootprint {
        base,
        builtin_bytes_per_bar: builtin_lanes
            .checked_mul(8)
            .ok_or_else(|| AotError::UnsupportedOpcode("builtin-lane stride overflows".into()))?,
        plot_bytes_per_bar: u64::from(layout.plot_count)
            .checked_mul(8)
            .ok_or_else(|| AotError::UnsupportedOpcode("plot-lane stride overflows".into()))?,
        color_bytes_per_bar: color_lanes
            .checked_mul(4)
            .ok_or_else(|| AotError::UnsupportedOpcode("color-lane stride overflows".into()))?,
        alert_bytes_per_bar: u64::from(layout.alert_sites)
            .checked_mul(u64::from(header::ALERT_ENTRY))
            .ok_or_else(|| AotError::UnsupportedOpcode("alert-lane stride overflows".into()))?,
        slot_bytes: u64::from(slots.bytes),
        slot_count: u64::from(layout.slot_count),
        scratch_bytes: u64::from(layout.scratch_cells)
            .checked_mul(8)
            .ok_or_else(|| AotError::UnsupportedOpcode("scratch-region size overflows".into()))?,
        ta_bytes: u64::from(ta_region_bytes(layout)),
    };

    // These coefficients are emitted as nonnegative i32 constants and used
    // by i32 multiplication. If one is too large, no positive bar count can
    // fit the declared memory; reject such a plan rather than truncating a
    // compile-time factor before `prepare` can guard it.
    for (name, value) in [
        ("builtin-lane", footprint.builtin_bytes_per_bar),
        ("plot-lane", footprint.plot_bytes_per_bar),
        ("color-lane", footprint.color_bytes_per_bar),
        ("alert-lane", footprint.alert_bytes_per_bar),
    ] {
        if value > i32::MAX as u64 {
            return Err(AotError::UnsupportedOpcode(format!(
                "the {name} per-bar layout coefficient exceeds wasm32 signed immediate range"
            )));
        }
    }
    Ok(footprint)
}

/// Assert that `new`, the result of the current constructor, equals what
/// `layout_footprint_reference` returns for the same program and plan.
pub(super) fn assert_matches(
    new: &Result<LayoutFootprint, AotError>,
    program: &CsilProgram,
    layout: &Layout,
    slots: &SlotLayout,
    lane_floor: u32,
) {
    let old = layout_footprint_reference(program, layout, slots, lane_floor);
    assert_eq!(
        format!("{new:?}"),
        format!("{old:?}"),
        "footprint_of_counts diverged from layout_footprint_reference"
    );
}

/// The arithmetic of `layout_footprint_reference` over plain counts, for the
/// synthetic sweep (the same expressions, with each program/plan read
/// replaced by the corresponding count).
fn reference_of_counts(c: FootprintCounts) -> Result<LayoutFootprint, AotError> {
    let input_bytes = c
        .inputs
        .checked_mul(8)
        .and_then(|bytes| bytes.checked_add(u64::from(header::SIZE)))
        .ok_or_else(|| AotError::UnsupportedOpcode("input-table layout overflows".into()))?;
    let base = input_bytes.max(u64::from(c.lane_floor));
    let builtin_lanes = u64::from(BUILTIN_LANE_COUNT)
        .checked_add(c.source_inputs)
        .ok_or_else(|| AotError::UnsupportedOpcode("builtin-lane count overflows".into()))?;
    let color_lanes = c
        .color_lanes
        .ok_or_else(|| AotError::UnsupportedOpcode("color-lane count overflows".into()))?;
    let footprint = LayoutFootprint {
        base,
        builtin_bytes_per_bar: builtin_lanes
            .checked_mul(8)
            .ok_or_else(|| AotError::UnsupportedOpcode("builtin-lane stride overflows".into()))?,
        plot_bytes_per_bar: u64::from(c.plot_count)
            .checked_mul(8)
            .ok_or_else(|| AotError::UnsupportedOpcode("plot-lane stride overflows".into()))?,
        color_bytes_per_bar: color_lanes
            .checked_mul(4)
            .ok_or_else(|| AotError::UnsupportedOpcode("color-lane stride overflows".into()))?,
        alert_bytes_per_bar: u64::from(c.alert_sites)
            .checked_mul(u64::from(header::ALERT_ENTRY))
            .ok_or_else(|| AotError::UnsupportedOpcode("alert-lane stride overflows".into()))?,
        slot_bytes: u64::from(c.slot_bytes),
        slot_count: u64::from(c.slot_count),
        scratch_bytes: u64::from(c.scratch_cells)
            .checked_mul(8)
            .ok_or_else(|| AotError::UnsupportedOpcode("scratch-region size overflows".into()))?,
        ta_bytes: u64::from(c.ta_bytes),
    };
    for (name, value) in [
        ("builtin-lane", footprint.builtin_bytes_per_bar),
        ("plot-lane", footprint.plot_bytes_per_bar),
        ("color-lane", footprint.color_bytes_per_bar),
        ("alert-lane", footprint.alert_bytes_per_bar),
    ] {
        if value > i32::MAX as u64 {
            return Err(AotError::UnsupportedOpcode(format!(
                "the {name} per-bar layout coefficient exceeds wasm32 signed immediate range"
            )));
        }
    }
    Ok(footprint)
}

fn assert_counts_match(c: FootprintCounts) {
    let new = footprint_of_counts(c).map_err(footprint_error);
    let old = reference_of_counts(c);
    assert_eq!(format!("{new:?}"), format!("{old:?}"), "counts {c:?}");
}

#[test]
fn counts_edges_match_reference() {
    let u64_edges = [
        0,
        1,
        7,
        (i32::MAX as u64) / 8,
        (i32::MAX as u64) / 8 + 1,
        (i32::MAX as u64) / 4,
        (i32::MAX as u64) / 4 + 1,
        u64::MAX / 8,
        u64::MAX / 8 + 1,
        u64::MAX / 4 + 1,
        u64::MAX - 6,
        u64::MAX,
    ];
    let u32_edges = [
        0,
        1,
        6,
        8,
        96,
        (i32::MAX as u32) / 8,
        (i32::MAX as u32) / 8 + 1,
        i32::MAX as u32,
        u32::MAX,
    ];
    let background = FootprintCounts {
        inputs: 3,
        lane_floor: 4096,
        source_inputs: 2,
        color_lanes: Some(4),
        plot_count: 5,
        alert_sites: 1,
        slot_bytes: 800,
        slot_count: 10,
        scratch_cells: 4,
        ta_bytes: 256,
    };
    assert_counts_match(background);
    assert_counts_match(FootprintCounts {
        color_lanes: None,
        ..background
    });
    for &v in &u64_edges {
        assert_counts_match(FootprintCounts {
            inputs: v,
            ..background
        });
        assert_counts_match(FootprintCounts {
            source_inputs: v,
            ..background
        });
        assert_counts_match(FootprintCounts {
            color_lanes: Some(v),
            ..background
        });
    }
    for &v in &u32_edges {
        for field in 0..7 {
            let mut c = background;
            match field {
                0 => c.lane_floor = v,
                1 => c.plot_count = v,
                2 => c.alert_sites = v,
                3 => c.slot_bytes = v,
                4 => c.slot_count = v,
                5 => c.scratch_cells = v,
                _ => c.ta_bytes = v,
            }
            assert_counts_match(c);
        }
    }
}

#[test]
fn counts_sweep_matches_reference() {
    // xorshift64*: deterministic and dependency-free.
    let mut state = 0xD1B5_4A32_D192_ED03u64;
    let mut next = move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    };
    for _ in 0..20_000 {
        let mut wide = || {
            let raw = next();
            match raw % 4 {
                0 | 1 => raw >> 56,
                2 => raw >> 32,
                _ => raw,
            }
        };
        let inputs = wide();
        let source_inputs = wide();
        let color = wide();
        let mut narrow = || {
            let raw = next();
            match raw % 4 {
                0 | 1 => (raw >> 56) as u32,
                2 => (raw >> 36) as u32,
                _ => raw as u32,
            }
        };
        let c = FootprintCounts {
            inputs,
            lane_floor: narrow(),
            source_inputs,
            color_lanes: if color % 17 == 0 { None } else { Some(color) },
            plot_count: narrow(),
            alert_sites: narrow(),
            slot_bytes: narrow(),
            slot_count: narrow(),
            scratch_cells: narrow(),
            ta_bytes: narrow(),
        };
        assert_counts_match(c);
    }
}
