//! Checker for the preconditions of the projection [`super::kernel`].
//!
//! The kernel assumes that the tables `FlowFacts::build` hands it are well
//! formed: one entry per pc, block ends in range, terminator and
//! reverse-postorder targets are blocks, slot operands are slots, and room in
//! `usize` for the deepest abstract stack. It also assumes that `Pop` and
//! chart sinks have no results. [`flow_input_wf`] decides exactly that, in the
//! Charon/Aeneas subset. [`kernel_input_ok`] adds the remaining side
//! conditions of the kernel pipeline (at least one block, one comparison flag
//! and one class per pc); `FlowFacts::build` refuses tables that fail it, so
//! the kernel only ever runs on tables that meet these conditions.

use super::kernel::{FlowInput, FlowOp, InstrClass};
use crate::analysis::Term;

/// The largest number of results of any pc.
fn max_results(input: &FlowInput) -> usize {
    let mut r = 0;
    let mut q = 0;
    while q < input.results.len() {
        if input.results[q].len() > r {
            r = input.results[q].len();
        }
        q += 1;
    }
    r
}

/// Whether a slot operand of `op` names a slot. (A separate function: the
/// code the pinned Aeneas revision generates for a comparison inside a `match` arm
/// bound by `let` elaborates it as a `Prop`, not a `bool`.)
fn slot_op_ok(op: FlowOp, n_slots: usize) -> bool {
    match op {
        FlowOp::ReadSlot(s) | FlowOp::ReadSlotDynamic(s) | FlowOp::StoreSlot(s) => s < n_slots,
        FlowOp::Dup | FlowOp::Generic => true,
    }
}

/// Every slot operand names a slot.
fn slots_ok(input: &FlowInput, n_slots: usize) -> bool {
    let mut ok = true;
    let mut q = 0;
    while ok && q < input.ops.len() {
        ok = slot_op_ok(input.ops[q], n_slots);
        q += 1;
    }
    ok
}

/// Every block ends at or before `n`.
fn blocks_ok(input: &FlowInput, n: usize) -> bool {
    let mut ok = true;
    let mut b = 0;
    while ok && b < input.blocks.len() {
        ok = input.blocks[b].end <= n;
        b += 1;
    }
    ok
}

/// Whether every target of `term` is a block (see [`slot_op_ok`]).
fn term_ok(term: Term, nb: usize) -> bool {
    match term {
        Term::Fall(t) | Term::Jump(t) => t < nb,
        Term::Branch { taken, fall } => taken < nb && fall < nb,
        Term::Return => true,
    }
}

/// Every terminator target is a block.
fn terms_ok(input: &FlowInput, nb: usize) -> bool {
    let mut ok = true;
    let mut b = 0;
    while ok && b < input.terms.len() {
        ok = term_ok(input.terms[b], nb);
        b += 1;
    }
    ok
}

/// Every reverse-postorder entry is a block.
fn rpo_ok(input: &FlowInput, nb: usize) -> bool {
    let mut ok = true;
    let mut i = 0;
    while ok && i < input.rpo_order.len() {
        ok = input.rpo_order[i] < nb;
        i += 1;
    }
    ok
}

/// `Pop` and chart sinks have no results.
fn terminal_ok(input: &FlowInput, classes: &[InstrClass]) -> bool {
    let mut ok = true;
    let mut q = 0;
    while ok && q < input.results.len() {
        let terminal = q < classes.len()
            && (classes[q] == InstrClass::Pop || classes[q] == InstrClass::ChartSink);
        ok = !terminal || input.results[q].is_empty();
        q += 1;
    }
    ok
}

/// `(nb + 1) * (n * (r + 1)) + r + 2` fits in `usize`.
fn cap_ok(nb: usize, n: usize, r: usize) -> bool {
    match r.checked_add(1) {
        None => false,
        Some(r1) => match n.checked_mul(r1) {
            None => false,
            Some(g) => match nb.checked_add(1) {
                None => false,
                Some(nb1) => match nb1.checked_mul(g) {
                    None => false,
                    Some(m) => match m.checked_add(r) {
                        None => false,
                        Some(m1) => m1.checked_add(2).is_some(),
                    },
                },
            },
        },
    }
}

/// Whether `input` (for `n` pcs, `n_slots` slots and the per-pc `classes`)
/// satisfies the kernel's preconditions.
pub(crate) fn flow_input_wf(
    input: &FlowInput,
    n: usize,
    n_slots: usize,
    classes: &[InstrClass],
) -> bool {
    let nb = input.blocks.len();
    input.ops.len() == n
        && input.pops.len() == n
        && input.results.len() == n
        && slots_ok(input, n_slots)
        && blocks_ok(input, n)
        && terms_ok(input, nb)
        && rpo_ok(input, nb)
        && terminal_ok(input, classes)
        && cap_ok(nb, n, max_results(input))
}

/// The whole precondition of the kernel pipeline (`flow_init`,
/// `flow_fixpoint`, `chart_only_sites`) for `n` pcs: [`flow_input_wf`] with
/// one slot per `slot_canonical` entry, at least one block, and one
/// comparison flag and one class per pc.
pub(crate) fn kernel_input_ok(
    input: &FlowInput,
    n: usize,
    slot_canonical: &[bool],
    cmp_candidate: &[bool],
    classes: &[InstrClass],
) -> bool {
    !input.blocks.is_empty()
        && cmp_candidate.len() == n
        && classes.len() == n
        && flow_input_wf(input, n, slot_canonical.len(), classes)
}
