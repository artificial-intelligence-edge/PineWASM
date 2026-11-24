//! Free-function extraction roots for Charon/Aeneas (`AotProjection` extraction).
//! Each root only delegates to the index-only projection kernel; production
//! builds never enable the `aeneas` feature.
#![allow(dead_code)]

use super::kernel::{self, FlowError, FlowInput, FlowState, InstrClass, ValueFact};

/// Root for the use-graph fixpoint (`FlowFacts::build`'s proof).
pub(crate) fn projection_flow_fixpoint(
    input: &FlowInput,
    state: &mut FlowState,
    cap: usize,
) -> Result<(), FlowError> {
    kernel::flow_fixpoint(input, state, cap)
}

/// Root for the data-flow initial state.
pub(crate) fn projection_flow_init(
    n_blocks: usize,
    n_code: usize,
    slot_canonical: &[bool],
    cmp_candidate: &[bool],
) -> FlowState {
    kernel::flow_init(n_blocks, n_code, slot_canonical, cmp_candidate)
}

/// Root for one round of the data flow.
pub(crate) fn projection_flow_round(
    input: &FlowInput,
    state: &mut FlowState,
) -> Result<bool, FlowError> {
    kernel::flow_round(input, state)
}

/// Root for the per-instruction transfer function.
pub(crate) fn projection_step_instr(
    input: &FlowInput,
    state: &mut FlowState,
    stack: &mut Vec<ValueFact>,
    pc: usize,
) -> Result<bool, FlowError> {
    kernel::step_instr(input, state, stack, pc)
}

/// Root for the fact join.
pub(crate) fn projection_merge_fact(dst: &mut ValueFact, src: &ValueFact) -> bool {
    kernel::merge_fact(dst, src)
}

/// Root for the backward chart-only proof of one producer.
pub(crate) fn projection_downstream_is_chart_only(
    pc: usize,
    uses: &[Vec<usize>],
    classes: &[InstrClass],
    visiting: &mut Vec<bool>,
) -> bool {
    kernel::downstream_is_chart_only(pc, uses, classes, visiting)
}

/// Root for the chart-only site classification (`finish`).
pub(crate) fn projection_chart_only_sites(
    uses: &[Vec<usize>],
    classes: &[InstrClass],
) -> Vec<bool> {
    kernel::chart_only_sites(uses, classes)
}

/// Root for the host-site erasure (`finish`).
pub(crate) fn projection_erase_host_sites(
    host_site: &mut [bool],
    projected_input: &[bool],
    canonical_cmp: &[bool],
    chart_only: &[bool],
) {
    kernel::erase_host_sites(host_site, projected_input, canonical_cmp, chart_only);
}

/// Root for the kernel-precondition checker.
pub(crate) fn projection_flow_input_wf(
    input: &FlowInput,
    n: usize,
    n_slots: usize,
    classes: &[InstrClass],
) -> bool {
    super::wf::flow_input_wf(input, n, n_slots, classes)
}

/// Root for the kernel-precondition gate of `FlowFacts::build`.
pub(crate) fn projection_kernel_input_ok(
    input: &FlowInput,
    n: usize,
    slot_canonical: &[bool],
    cmp_candidate: &[bool],
    classes: &[InstrClass],
) -> bool {
    super::wf::kernel_input_ok(input, n, slot_canonical, cmp_candidate, classes)
}
