//! Charon/Aeneas extraction roots for the `AotPrepare` extraction: the pure derivation
//! of `prepare`'s layout immediates ([`crate::prepare_consts`]). Free
//! functions that only delegate, so the roots are nameable with
//! `--start-from` and these roots stay out of shared files.

#![allow(dead_code)]

use crate::layout_footprint::LayoutFootprint;
use crate::prepare_consts::{self, PrepareConsts, PrepareConstsError, PrepareGlobals, PrepareOp};

pub(crate) fn immediate(value: u64) -> Result<i32, PrepareConstsError> {
    prepare_consts::immediate(value)
}

pub(crate) fn prepare_consts(
    footprint: LayoutFootprint,
) -> Result<PrepareConsts, PrepareConstsError> {
    prepare_consts::prepare_consts(footprint)
}

pub(crate) fn lane_offset(lane: u64, width: u64, stride: i32) -> Result<i32, PrepareConstsError> {
    prepare_consts::lane_offset(lane, width, stride)
}

pub(crate) fn total_color_lanes(
    plot_roles: &[u32],
    visual_roles: &[u32],
    fill_roles: &[u32],
) -> Option<u64> {
    prepare_consts::total_color_lanes(plot_roles, visual_roles, fill_roles)
}

pub(crate) fn prepare_layout_ops(
    consts: PrepareConsts,
    max_bars: u32,
    globals: PrepareGlobals,
) -> Vec<PrepareOp> {
    prepare_consts::prepare_layout_ops(consts, max_bars, globals)
}
