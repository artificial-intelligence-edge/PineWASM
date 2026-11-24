//! Flow-sensitive slot aliasing: copy propagation across branches and loops.
//!
//! Inlining a user function binds every parameter to a hidden slot, so each
//! call evaluates `READ_USER_SLOT src 0; STORE_USER_SLOT_CURRENT dst` once per
//! argument. Inside a loop (a nearest-neighbour scan that calls a helper per
//! iteration) those copies run on every iteration. [`super::entry_alias_plan`]
//! only removes copies in the entry's straight-line prefix; this pass removes
//! a copy anywhere, under a must-availability condition:
//!
//! * `dst` has exactly one store (the copy's), no read at a nonzero offset,
//!   no dynamic, window or written-flag observer, and is not `var`/`varip`;
//!   no branch targets the copy's store;
//! * at every offset-0 read of `dst`, every path from the program entry
//!   passes through the copy, and no store to `src` lies between the most
//!   recent execution of the copy and the read.
//!
//! Then `dst` holds exactly the value `src` holds at each of its reads, so the
//! reads can name `src` and the copy can be erased. A non-persistent slot
//! starts each bar as `na`, and the condition is about one bar's execution, so
//! no state crosses bars. History-indexed parameters (`p[1]`), `var` copies
//! and any slot whose written flag is observed keep their own storage.
//!
//! One round never removes a copy whose source is itself the destination of
//! a removed copy (the chain `d2 <- d1 <- s` is resolved in two rounds), so
//! the renaming table of a round has no chains. [`flow_alias_rounds`] repeats
//! rounds while a round defers such a copy (at most [`MAX_ROUNDS`]).
//!
//! [`check_flow_alias`] re-checks one round from its certificate (the keep
//! mask and renaming table) with an independent backward search per
//! destination.

use super::{mark_branch_target, persistent_slots, rename_entry_aliases, retain_instructions};
use crate::CsilInstr;
use pinewasm_mir::SlotDef;
use pinewasm_span::Span;

/// Upper bound on [`flow_alias_rounds`]. Each round resolves one level of a
/// copy chain; inlined helpers nest a few levels deep in practice.
pub(super) const MAX_ROUNDS: usize = 8;

/// Work bounds of one round. Lowering replays these passes on a serialized
/// budget input (untrusted, up to `MAX_BUDGET_REFERENCE_BYTES`), so the
/// analysis must stay bounded on any input. A round whose input exceeds a
/// bound changes nothing. Each bound is a pure function of the round's input,
/// so the compiler and a validator's replay always make the same decision.
///
/// Words in one block-by-copy bit matrix (three are allocated): 8 MiB each.
pub(super) const MAX_MATRIX_WORDS: usize = 1 << 20;
/// Copy invalidations the per-instruction transfers perform: the sum over
/// slots of (stores to the slot) x (copies reading it).
pub(super) const MAX_KILL_WORK: u64 = 1 << 24;
/// Sweeps of the availability fixed point. A reducible CFG settles in its
/// loop-nesting depth plus two.
pub(super) const MAX_SWEEPS: usize = 64;

/// The certificate of one round: its keep mask and renaming table.
pub(super) type RoundCert = (Vec<bool>, Vec<u32>);

/// Run rounds of [`flow_alias_plan`] (rename, then erase) until a round
/// removes nothing, defers no copy to a later round, or [`MAX_ROUNDS`]
/// rounds have run. Returns the certificate
/// of every round that changed the code, in order.
pub(super) fn flow_alias_rounds(
    code: &mut Vec<CsilInstr>,
    spans: &mut Vec<Span>,
    slots: &[SlotDef],
) -> Vec<RoundCert> {
    let mut rounds = Vec::new();
    while rounds.len() < MAX_ROUNDS {
        let (keep, aliases, deferred) = plan_round(code, slots);
        if keep.iter().all(|kept| *kept) {
            break;
        }
        rename_entry_aliases(code, &aliases);
        retain_instructions(code, spans, &keep);
        rounds.push((keep, aliases));
        if !deferred {
            break;
        }
    }
    rounds
}

/// Control-flow successors of the instruction at `pc` in code of length `n`:
/// up to two, `usize::MAX` for none. Targets past the end are not
/// successors (the caller rejects such code before it gets here).
fn successors(code: &[CsilInstr], pc: usize) -> (usize, usize) {
    let n = code.len();
    let next = if pc + 1 < n { pc + 1 } else { usize::MAX };
    match code[pc] {
        CsilInstr::Return => (usize::MAX, usize::MAX),
        CsilInstr::Jump { target } => (target as usize, usize::MAX),
        CsilInstr::JumpIfFalse { target }
        | CsilInstr::JumpIfExact { target, .. }
        | CsilInstr::JumpIfSlotWritten { target, .. } => (target as usize, next),
        _ => (next, usize::MAX),
    }
}

/// What one scan of the code records about every slot and instruction.
struct Scan {
    writes: Vec<u8>,
    observed: Vec<bool>,
    targeted: Vec<bool>,
}

/// Scan `code`: per slot, stores (saturating at 2) and observers (persistence,
/// nonzero-offset, dynamic, window and written-flag reads); per instruction,
/// whether a branch targets it. `None` when a slot operand is undeclared or a
/// branch target lies past the end, so the pass leaves such code alone.
fn scan(code: &[CsilInstr], slots: &[SlotDef]) -> Option<Scan> {
    let n = code.len();
    let mut writes = vec![0u8; slots.len()];
    let mut observed = persistent_slots(slots);
    let mut targeted = vec![false; n];
    let mut pc = 0;
    while pc < n {
        let (slot, store, observes, target) = match code[pc] {
            CsilInstr::ReadUserSlot { slot, offset } => (Some(slot), false, offset != 0, None),
            CsilInstr::StoreUserSlotCurrent { slot } => (Some(slot), true, false, None),
            CsilInstr::ReadUserSlotDynamic { slot }
            | CsilInstr::WindowMean { slot, .. }
            | CsilInstr::WindowMeanDynamic { slot }
            | CsilInstr::WindowExtremumDynamic { slot, .. }
            | CsilInstr::WindowStatisticDynamic { slot, .. } => (Some(slot), false, true, None),
            CsilInstr::JumpIfSlotWritten { slot, target } => {
                (Some(slot), false, true, Some(target))
            }
            CsilInstr::Jump { target }
            | CsilInstr::JumpIfFalse { target }
            | CsilInstr::JumpIfExact { target, .. } => (None, false, false, Some(target)),
            _ => (None, false, false, None),
        };
        if let Some(slot) = slot {
            let index = slot as usize;
            if index >= slots.len() {
                return None;
            }
            if store && writes[index] < 2 {
                writes[index] += 1;
            }
            if observes {
                observed[index] = true;
            }
        }
        if let Some(target) = target {
            if target as usize >= n {
                return None;
            }
            mark_branch_target(&mut targeted, target);
        }
        pc += 1;
    }
    Some(Scan {
        writes,
        observed,
        targeted,
    })
}

/// The copy `READ src 0; STORE dst` whose store is at `pc`, if the local
/// conditions of the pass hold for it.
fn local_copy(code: &[CsilInstr], pc: usize, scan: &Scan) -> Option<(u32, u32)> {
    if pc == 0 || scan.targeted[pc] {
        return None;
    }
    let CsilInstr::StoreUserSlotCurrent { slot: dst } = code[pc] else {
        return None;
    };
    let CsilInstr::ReadUserSlot {
        slot: src,
        offset: 0,
    } = code[pc - 1]
    else {
        return None;
    };
    let index = dst as usize;
    if src == dst || scan.writes[index] != 1 || scan.observed[index] {
        return None;
    }
    Some((src, dst))
}

/// One bit set per row (a block's entry state, gen or kill set), stored
/// flat: `words` words per row.
struct BitRows {
    words: usize,
    data: Vec<u64>,
}

impl BitRows {
    fn new(rows: usize, bits: usize, value: bool) -> Self {
        let words = bits.div_ceil(64);
        BitRows {
            words,
            data: vec![if value { u64::MAX } else { 0 }; rows * words],
        }
    }
    fn row(&self, row: usize) -> &[u64] {
        &self.data[row * self.words..(row + 1) * self.words]
    }
    fn row_mut(&mut self, row: usize) -> &mut [u64] {
        &mut self.data[row * self.words..(row + 1) * self.words]
    }
}

fn get(bits: &[u64], bit: usize) -> bool {
    bits[bit / 64] & (1 << (bit % 64)) != 0
}

fn set(bits: &mut [u64], bit: usize) {
    bits[bit / 64] |= 1 << (bit % 64);
}

fn clear(bits: &mut [u64], bit: usize) {
    bits[bit / 64] &= !(1 << (bit % 64));
}

/// The analysis of one round: its keep mask (`false` exactly at the read and
/// the store of every removed copy) and its renaming table (`aliases[dst]` is
/// the copy's source for a removed copy; every other slot maps to itself).
#[cfg(test)]
#[allow(clippy::ptr_arg)]
pub(super) fn flow_alias_plan(code: &Vec<CsilInstr>, slots: &[SlotDef]) -> (Vec<bool>, Vec<u32>) {
    let (keep, aliases, _) = plan_round(code, slots);
    (keep, aliases)
}

/// [`flow_alias_plan`], also reporting whether a valid copy was deferred to a
/// later round because its source is a destination removed in this one.
/// [`flow_alias_rounds`] stops after a round that deferred nothing: the
/// rounds exist to resolve such chains, and a later round could only find a
/// copy whose availability changed through a renamed source, which is rare
/// and not worth another analysis of the whole function.
fn plan_round(code: &[CsilInstr], slots: &[SlotDef]) -> (Vec<bool>, Vec<u32>, bool) {
    let n = code.len();
    let identity: Vec<u32> = (0..slots.len() as u32).collect();
    let mut keep = vec![true; n];
    let Some(scan) = scan(code, slots) else {
        return (keep, identity, false);
    };

    // Candidate copies, numbered in code order.
    let mut copies: Vec<(usize, u32, u32)> = Vec::new();
    let mut copy_of_dst = vec![usize::MAX; slots.len()];
    let mut copy_at = vec![usize::MAX; n];
    for (pc, at) in copy_at.iter_mut().enumerate().skip(1) {
        if let Some((src, dst)) = local_copy(code, pc, &scan) {
            copy_of_dst[dst as usize] = copies.len();
            *at = copies.len();
            copies.push((pc, src, dst));
        }
    }
    if copies.is_empty() {
        return (keep, identity, false);
    }
    let count = copies.len();
    // The copies a store to each slot invalidates (those reading it).
    let mut killed_by: Vec<Vec<usize>> = vec![Vec::new(); slots.len()];
    for (index, (_, src, _)) in copies.iter().enumerate() {
        killed_by[*src as usize].push(index);
    }
    let mut stores = vec![0u64; slots.len()];
    for instr in code {
        if let CsilInstr::StoreUserSlotCurrent { slot } = instr {
            stores[*slot as usize] += 1;
        }
    }
    let kill_work = stores
        .iter()
        .zip(&killed_by)
        .fold(0u64, |work, (stores, copies)| {
            work.saturating_add(stores.saturating_mul(copies.len() as u64))
        });
    if kill_work > MAX_KILL_WORK {
        return (keep, identity, false);
    }
    // The effect of the instruction at `pc` on the available copies.
    let transfer = |state: &mut [u64], pc: usize| {
        if let CsilInstr::StoreUserSlotCurrent { slot } = code[pc] {
            for copy in &killed_by[slot as usize] {
                clear(state, *copy);
            }
            if copy_at[pc] != usize::MAX {
                set(state, copy_at[pc]);
            }
        }
    };

    // Basic blocks: a leader is the entry, a branch target or the
    // instruction after a branch or `Return`.
    let mut starts = Vec::new();
    let mut block_of = vec![0usize; n];
    for pc in 0..n {
        let after_branch = pc > 0
            && matches!(
                code[pc - 1],
                CsilInstr::Return
                    | CsilInstr::Jump { .. }
                    | CsilInstr::JumpIfFalse { .. }
                    | CsilInstr::JumpIfExact { .. }
                    | CsilInstr::JumpIfSlotWritten { .. }
            );
        if pc == 0 || after_branch || scan.targeted[pc] {
            starts.push(pc);
        }
        block_of[pc] = starts.len() - 1;
    }
    let blocks = starts.len();
    if blocks
        .checked_mul(count.div_ceil(64))
        .is_none_or(|words| words > MAX_MATRIX_WORDS)
    {
        return (keep, identity, false);
    }
    let block_end = |block: usize| starts.get(block + 1).copied().unwrap_or(n);
    let block_successors = |block: usize| {
        let (first, second) = successors(code, block_end(block) - 1);
        [first, second].map(|pc| if pc < n { block_of[pc] } else { usize::MAX })
    };

    // Per-block summaries: `out = (in & !kill) | gen`.
    let mut gens = BitRows::new(blocks, count, false);
    let mut kills = BitRows::new(blocks, count, false);
    for (block, start) in starts.iter().enumerate() {
        let end = block_end(block);
        for (pc, instr) in code.iter().enumerate().take(end).skip(*start) {
            if let CsilInstr::StoreUserSlotCurrent { slot } = *instr {
                for copy in &killed_by[slot as usize] {
                    set(kills.row_mut(block), *copy);
                    clear(gens.row_mut(block), *copy);
                }
                if copy_at[pc] != usize::MAX {
                    set(gens.row_mut(block), copy_at[pc]);
                }
            }
        }
    }

    // Forward must-availability. The entry starts empty, every other block
    // full (top), and states only shrink. Blocks are visited in reverse
    // postorder of a depth-first walk from the entry (unreachable blocks
    // after, in code order), sweeping until nothing changes; a reducible CFG
    // settles in a few sweeps.
    let mut order = Vec::with_capacity(blocks);
    let mut visited = vec![false; blocks];
    let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
    visited[0] = true;
    while let Some((block, next)) = stack.pop() {
        if next < 2 {
            stack.push((block, next + 1));
            let target = block_successors(block)[next];
            if target != usize::MAX && !visited[target] {
                visited[target] = true;
                stack.push((target, 0));
            }
        } else {
            order.push(block);
        }
    }
    order.reverse();
    order.extend((0..blocks).filter(|block| !visited[*block]));
    let mut entry = BitRows::new(blocks, count, true);
    entry.row_mut(0).fill(0);
    let mut out = vec![0u64; entry.words];
    let mut changed = true;
    let mut sweeps = 0;
    while changed {
        if sweeps == MAX_SWEEPS {
            return (keep, identity, false);
        }
        sweeps += 1;
        changed = false;
        for block in &order {
            let (input, kill, gen) = (entry.row(*block), kills.row(*block), gens.row(*block));
            for word in 0..out.len() {
                out[word] = (input[word] & !kill[word]) | gen[word];
            }
            for target in block_successors(*block) {
                if target == usize::MAX || target == 0 {
                    continue;
                }
                for (word, available) in entry.row_mut(target).iter_mut().zip(&out) {
                    let next = *word & available;
                    changed |= next != *word;
                    *word = next;
                }
            }
        }
    }

    // A copy is valid when it is available at every offset-0 read of `dst`.
    let mut valid = vec![true; count];
    let mut state = vec![0u64; entry.words];
    for (block, start) in starts.iter().enumerate() {
        state.copy_from_slice(entry.row(block));
        let end = block_end(block);
        for (pc, instr) in code.iter().enumerate().take(end).skip(*start) {
            if let CsilInstr::ReadUserSlot { slot, offset: 0 } = *instr {
                let copy = copy_of_dst[slot as usize];
                if copy != usize::MAX && !get(&state, copy) {
                    valid[copy] = false;
                }
            }
            transfer(&mut state, pc);
        }
    }

    // Remove every valid copy whose source is not itself a removed
    // destination this round; defer the others.
    let mut aliases = identity;
    let mut deferred = false;
    for (index, (pc, src, dst)) in copies.iter().enumerate() {
        if !valid[index] {
            continue;
        }
        let source_copy = copy_of_dst[*src as usize];
        if source_copy != usize::MAX && valid[source_copy] {
            deferred = true;
            continue;
        }
        aliases[*dst as usize] = *src;
        keep[pc - 1] = false;
        keep[*pc] = false;
    }
    (keep, aliases, deferred)
}

/// Why [`check_flow_alias`] rejected a round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowAliasCheckError {
    /// Span vectors do not align with their code, or the input is too long.
    Shape,
    /// The renaming table does not have one entry per slot.
    TableLength,
    /// An instruction names an undeclared slot or a branch target lies past
    /// the end.
    OperandOutOfRange,
    /// A renamed slot maps to a slot that is itself renamed, or to itself.
    Chain,
    /// A renamed slot is not stored exactly once by a copy of its source
    /// that the keep mask erases, or a branch targets that store.
    NotCopy,
    /// A renamed slot is observed (history, dynamic, window, written flag,
    /// persistence).
    Observed,
    /// The keep mask erases an instruction that is not part of a certified
    /// copy.
    StrayErasure,
    /// Some read of a renamed slot is reachable from the entry, or from a
    /// store to its source, without passing through its copy.
    NotAvailable,
    /// The output is not the input renamed, erased and relocated.
    Output(crate::PairCheckError),
}

/// Check one round of the pass: `after`/`after_spans` is `before` with every
/// offset-0 read renamed through `table`, the instructions whose `keep` flag
/// is false erased and every branch relocated, and the rewrite is sound (see
/// the module documentation).
///
/// The availability condition is re-derived by a backward search from every
/// read of each renamed slot, independent of the forward analysis the pass
/// uses.
pub(super) fn check_flow_alias(
    before: &[CsilInstr],
    before_spans: &[Span],
    after: &[CsilInstr],
    after_spans: &[Span],
    keep: &[bool],
    table: &[u32],
    slots: &[SlotDef],
) -> Result<(), FlowAliasCheckError> {
    let n = before.len();
    if before_spans.len() != n
        || after_spans.len() != after.len()
        || keep.len() != n
        || n >= u32::MAX as usize
    {
        return Err(FlowAliasCheckError::Shape);
    }
    if table.len() != slots.len() {
        return Err(FlowAliasCheckError::TableLength);
    }
    let Some(scan) = scan(before, slots) else {
        return Err(FlowAliasCheckError::OperandOutOfRange);
    };

    // Each renamed slot's unique store, which must be a certified copy.
    let mut store_of = vec![usize::MAX; slots.len()];
    for (pc, instr) in before.iter().enumerate() {
        if let CsilInstr::StoreUserSlotCurrent { slot } = instr {
            store_of[*slot as usize] = pc;
        }
    }
    let mut certified = vec![false; n];
    for (dst, src) in table.iter().enumerate() {
        let src = *src as usize;
        if src == dst {
            continue;
        }
        if src >= table.len() || table[src] as usize != src {
            return Err(FlowAliasCheckError::Chain);
        }
        if scan.observed[dst] {
            return Err(FlowAliasCheckError::Observed);
        }
        let pc = store_of[dst];
        if scan.writes[dst] != 1
            || pc == 0
            || pc == usize::MAX
            || scan.targeted[pc]
            || keep[pc]
            || keep[pc - 1]
            || before[pc - 1]
                != (CsilInstr::ReadUserSlot {
                    slot: src as u32,
                    offset: 0,
                })
        {
            return Err(FlowAliasCheckError::NotCopy);
        }
        certified[pc] = true;
        certified[pc - 1] = true;
    }
    if (0..n).any(|pc| !keep[pc] && !certified[pc]) {
        return Err(FlowAliasCheckError::StrayErasure);
    }

    // Predecessors.
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    for pc in 0..n {
        let (first, second) = successors(before, pc);
        for successor in [first, second] {
            if successor < n {
                preds[successor].push(pc);
            }
        }
    }
    // Reads of each renamed slot.
    let mut reads: Vec<Vec<usize>> = vec![Vec::new(); slots.len()];
    for (pc, instr) in before.iter().enumerate() {
        if let CsilInstr::ReadUserSlot { slot, offset: 0 } = instr {
            if table[*slot as usize] != *slot {
                reads[*slot as usize].push(pc);
            }
        }
    }
    // Backward search: from each read, every path back must meet the copy's
    // store before the entry or a store to the source.
    let mut seen = vec![usize::MAX; n];
    let mut stack = Vec::new();
    for dst in 0..slots.len() {
        if reads[dst].is_empty() {
            continue;
        }
        let src = table[dst];
        let copy = store_of[dst];
        for read in &reads[dst] {
            if *read == 0 {
                return Err(FlowAliasCheckError::NotAvailable);
            }
            stack.extend(preds[*read].iter().copied());
        }
        while let Some(pc) = stack.pop() {
            if seen[pc] == dst {
                continue;
            }
            seen[pc] = dst;
            if pc == copy {
                continue;
            }
            if pc == 0 || before[pc] == (CsilInstr::StoreUserSlotCurrent { slot: src }) {
                return Err(FlowAliasCheckError::NotAvailable);
            }
            stack.extend(preds[pc].iter().copied());
        }
    }

    // The output: rename, then erase and relocate.
    let mut expected = before.to_vec();
    let mut expected_spans = before_spans.to_vec();
    rename_entry_aliases(&mut expected, &table.to_vec());
    retain_instructions(&mut expected, &mut expected_spans, keep);
    if expected.len() != after.len()
        || expected_spans != after_spans
        || expected
            .iter()
            .zip(after)
            .any(|(left, right)| !crate::instr_exact_eq(left, right))
    {
        return Err(FlowAliasCheckError::Output(
            crate::PairCheckError::OutputMismatch,
        ));
    }
    Ok(())
}
