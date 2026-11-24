//! Index-only kernel of the standalone WebAssembly chart-only projection.
//!
//! [`super::StandaloneWasmProjection::finish`] classifies every instruction
//! once (strings, constant pools and input maps stay in that wrapper) and
//! hands this module plain tables: an [`InstrClass`] and a [`FlowOp`] per pc,
//! stack arities, and the analysis' blocks and terminators. The kernel then
//! runs the forward use-graph data flow, the backward chart-only proof and
//! the host-site erasure over `Vec`/`bool`/`usize` data only.
//!
//! The code deliberately stays inside the Charon/Aeneas subset (see the
//! `aeneas` feature): no maps or sets, no closures, no iterator adaptors, one
//! loop per function, and early exits only out of single loops. Sets of pcs
//! are strictly increasing `Vec<usize>`s.

use crate::analysis::{Block, Term};

/// What the chart-only proof needs to know about one instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InstrClass {
    /// Any instruction the proof does not name; a use by it is a real use.
    Other,
    /// `Pop`: discards its operand, so it is a chart-only use.
    Pop,
    /// `SetCell`, `ClearCells`, `MergeCells`, `DeleteDrawing`: result-less
    /// chart effects. A use by one is chart-only, and the site is erased.
    ChartSink,
    /// `CallPure StrTostring`, `StrConcat`, `NewObject`: an erasure
    /// candidate whose own uses must all be chart-only.
    ChartProducer,
    /// `RegisterDrawing`: value identity whose uses must all be chart-only;
    /// its registry effect is always erased.
    RegisterDrawing,
}

/// How the use-graph data flow treats one instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FlowOp {
    /// `Dup`: transparent copy of the top value.
    Dup,
    /// `ReadUserSlot`: pushes the slot's merged fact.
    ReadSlot(usize),
    /// `ReadUserSlotDynamic`: consumes an offset, pushes the slot's fact.
    ReadSlotDynamic(usize),
    /// `StoreUserSlotCurrent`: merges the popped fact into the slot.
    StoreSlot(usize),
    /// Everything else: pops its operands and produces its results.
    Generic,
}

/// Canonical-string status of one result of a [`FlowOp::Generic`] instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResultCanon {
    /// Not a canonical string handle.
    No,
    /// Always a canonical string handle (`PushStr`, `PushNa`, a projected
    /// string input).
    Yes,
    /// Result 0 of a string `Select`: canonical iff both arms (operands 1
    /// and 2) are.
    SelectArms,
}

/// Why the data flow refused the program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FlowError {
    StackUnderflow(usize),
    MissingTerminator(usize),
    InconsistentDepth,
    NoConvergence,
}

/// Abstract stack value: the pcs that may have produced it (strictly
/// increasing) and whether it is certainly a canonical string handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValueFact {
    pub producers: Vec<usize>,
    pub canonical_string: bool,
}

/// The per-program tables the data flow reads. The tables are owned (not
/// borrowed slices): the pinned Aeneas revision fails with an internal error when it has
/// to join contexts holding a struct of borrowed slices.
pub(crate) struct FlowInput {
    pub ops: Vec<FlowOp>,
    /// Operand count per pc (`operand_tys[pc].len()`).
    pub pops: Vec<usize>,
    /// One entry per result per pc (`result_tys[pc]`).
    pub results: Vec<Vec<ResultCanon>>,
    pub blocks: Vec<Block>,
    pub terms: Vec<Term>,
    /// Reachable blocks in reverse postorder.
    pub rpo_order: Vec<usize>,
}

/// Mutable data-flow state.
pub(crate) struct FlowState {
    /// Stack at each block entry; meaningful only where `entry_set` is true.
    pub entry: Vec<Vec<ValueFact>>,
    pub entry_set: Vec<bool>,
    pub slots: Vec<ValueFact>,
    /// Consumers of each producer pc, strictly increasing.
    pub uses: Vec<Vec<usize>>,
    /// String `CmpEq`/`CmpNe` sites still proven canonical.
    pub cmp_ok: Vec<bool>,
}

/// Inserts `x` into the strictly increasing `v`, keeping it so.
///
/// The vector is rebuilt with `push` rather than `Vec::insert`: Aeneas
/// The pinned Aeneas revision models `Vec::insert(i, x)` as an overwrite of element `i` that
/// fails when `i == len`, which is not what the standard library does.
pub(crate) fn sorted_insert(v: &mut Vec<usize>, x: usize) {
    let mut i = 0;
    while i < v.len() && v[i] < x {
        i += 1;
    }
    if i < v.len() && v[i] == x {
        return;
    }
    let mut out = Vec::new();
    let mut j = 0;
    while j < i {
        out.push(v[j]);
        j += 1;
    }
    out.push(x);
    while j < v.len() {
        out.push(v[j]);
        j += 1;
    }
    *v = out;
}

/// Union of two strictly increasing vectors.
pub(crate) fn sorted_union(a: &[usize], b: &[usize]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < a.len() || j < b.len() {
        if j >= b.len() || (i < a.len() && a[i] < b[j]) {
            out.push(a[i]);
            i += 1;
        } else if i >= a.len() || b[j] < a[i] {
            out.push(b[j]);
            j += 1;
        } else {
            out.push(a[i]);
            i += 1;
            j += 1;
        }
    }
    out
}

/// Joins `src` into `dst`; true when `dst` changed.
pub(crate) fn merge_fact(dst: &mut ValueFact, src: &ValueFact) -> bool {
    let old_len = dst.producers.len();
    dst.producers = sorted_union(&dst.producers, &src.producers);
    let old_canonical = dst.canonical_string;
    dst.canonical_string = dst.canonical_string && src.canonical_string;
    dst.producers.len() != old_len || dst.canonical_string != old_canonical
}

/// Records `consumer` as a use of every producer of `value`.
pub(crate) fn record_uses(uses: &mut [Vec<usize>], value: &ValueFact, consumer: usize) {
    let mut i = 0;
    while i < value.producers.len() {
        sorted_insert(&mut uses[value.producers[i]], consumer);
        i += 1;
    }
}

fn pop_fact(stack: &mut Vec<ValueFact>, pc: usize) -> Result<ValueFact, FlowError> {
    match stack.pop() {
        Some(value) => Ok(value),
        None => Err(FlowError::StackUnderflow(pc)),
    }
}

/// Pops `n` values; the result is in stack order (deepest first).
fn pop_args(stack: &mut Vec<ValueFact>, n: usize) -> Vec<ValueFact> {
    let mut rev = Vec::new();
    let mut k = 0;
    while k < n {
        if let Some(value) = stack.pop() {
            rev.push(value);
        }
        k += 1;
    }
    let mut args = Vec::new();
    let mut i = rev.len();
    while i > 0 {
        args.push(rev[i - 1].clone());
        i -= 1;
    }
    args
}

fn record_all_uses(uses: &mut [Vec<usize>], args: &[ValueFact], consumer: usize) {
    let mut i = 0;
    while i < args.len() {
        record_uses(uses, &args[i], consumer);
        i += 1;
    }
}

fn all_canonical(args: &[ValueFact]) -> bool {
    let mut ok = true;
    let mut i = 0;
    while i < args.len() {
        ok = ok && args[i].canonical_string;
        i += 1;
    }
    ok
}

fn arg_canonical(args: &[ValueFact], i: usize) -> bool {
    i < args.len() && args[i].canonical_string
}

fn result_canonical(canon: ResultCanon, index: usize, args: &[ValueFact]) -> bool {
    match canon {
        ResultCanon::No => false,
        ResultCanon::Yes => true,
        ResultCanon::SelectArms => index == 0 && arg_canonical(args, 1) && arg_canonical(args, 2),
    }
}

/// `{pc}` as a one-element set. Spelled with `push` rather than `vec![pc]`
/// so the extracted code needs no boxed-array model.
#[allow(clippy::vec_init_then_push)]
fn singleton(pc: usize) -> Vec<usize> {
    let mut producers = Vec::new();
    producers.push(pc);
    producers
}

fn push_results(
    stack: &mut Vec<ValueFact>,
    pc: usize,
    results: &[ResultCanon],
    args: &[ValueFact],
) {
    let mut r = 0;
    while r < results.len() {
        stack.push(ValueFact {
            producers: singleton(pc),
            canonical_string: result_canonical(results[r], r, args),
        });
        r += 1;
    }
}

/// Transfer of one [`FlowOp::Generic`] instruction.
fn step_generic(
    input: &FlowInput,
    state: &mut FlowState,
    stack: &mut Vec<ValueFact>,
    pc: usize,
) -> Result<(), FlowError> {
    let pops = input.pops[pc];
    if stack.len() < pops {
        return Err(FlowError::StackUnderflow(pc));
    }
    let args = pop_args(stack, pops);
    record_all_uses(&mut state.uses, &args, pc);
    if state.cmp_ok[pc] {
        state.cmp_ok[pc] = args.len() == 2 && all_canonical(&args);
    }
    push_results(stack, pc, &input.results[pc], &args);
    Ok(())
}

/// Transfer of one instruction; true when a slot fact changed.
pub(crate) fn step_instr(
    input: &FlowInput,
    state: &mut FlowState,
    stack: &mut Vec<ValueFact>,
    pc: usize,
) -> Result<bool, FlowError> {
    // Matched from a local copy: Aeneas cannot expand an enum matched in
    // place behind an index.
    let op = input.ops[pc];
    match op {
        FlowOp::Dup => {
            let value = pop_fact(stack, pc)?;
            stack.push(value.clone());
            stack.push(value);
            Ok(false)
        }
        FlowOp::ReadSlot(slot) => {
            stack.push(state.slots[slot].clone());
            Ok(false)
        }
        FlowOp::ReadSlotDynamic(slot) => {
            let offset = pop_fact(stack, pc)?;
            record_uses(&mut state.uses, &offset, pc);
            stack.push(state.slots[slot].clone());
            Ok(false)
        }
        FlowOp::StoreSlot(slot) => {
            let value = pop_fact(stack, pc)?;
            Ok(merge_fact(&mut state.slots[slot], &value))
        }
        FlowOp::Generic => {
            step_generic(input, state, stack, pc)?;
            Ok(false)
        }
    }
}

/// Joins a block-exit stack into a successor's entry; true on change.
pub(crate) fn merge_entry(
    state: &mut FlowState,
    target: usize,
    src: &[ValueFact],
) -> Result<bool, FlowError> {
    if !state.entry_set[target] {
        state.entry_set[target] = true;
        state.entry[target] = src.to_vec();
        return Ok(true);
    }
    let current = &mut state.entry[target];
    if current.len() != src.len() {
        return Err(FlowError::InconsistentDepth);
    }
    let mut changed = false;
    let mut i = 0;
    while i < src.len() {
        let merged = merge_fact(&mut current[i], &src[i]);
        changed = merged || changed;
        i += 1;
    }
    Ok(changed)
}

fn transfer_instrs(
    input: &FlowInput,
    state: &mut FlowState,
    stack: &mut Vec<ValueFact>,
    start: usize,
    end: usize,
) -> Result<bool, FlowError> {
    let mut changed = false;
    let mut pc = start;
    while pc < end {
        let step = step_instr(input, state, stack, pc)?;
        changed = step || changed;
        pc += 1;
    }
    Ok(changed)
}

/// One pass over one block; true when anything changed.
pub(crate) fn transfer_block(
    input: &FlowInput,
    state: &mut FlowState,
    block_id: usize,
) -> Result<bool, FlowError> {
    if !state.entry_set[block_id] {
        return Ok(false);
    }
    let mut stack = state.entry[block_id].clone();
    let block = &input.blocks[block_id];
    let mut changed = transfer_instrs(input, state, &mut stack, block.start, block.end)?;
    if block_id >= input.terms.len() {
        return Err(FlowError::MissingTerminator(block_id));
    }
    let term = input.terms[block_id];
    match term {
        Term::Branch { taken, fall } => {
            let a = merge_entry(state, taken, &stack)?;
            let b = merge_entry(state, fall, &stack)?;
            changed = changed || a || b;
        }
        Term::Fall(next) => {
            let a = merge_entry(state, next, &stack)?;
            changed = changed || a;
        }
        Term::Jump(next) => {
            let a = merge_entry(state, next, &stack)?;
            changed = changed || a;
        }
        Term::Return => {}
    }
    Ok(changed)
}

/// One round over every reachable block in reverse postorder.
pub(crate) fn flow_round(input: &FlowInput, state: &mut FlowState) -> Result<bool, FlowError> {
    let mut changed = false;
    let mut i = 0;
    while i < input.rpo_order.len() {
        let block_changed = transfer_block(input, state, input.rpo_order[i])?;
        changed = block_changed || changed;
        i += 1;
    }
    Ok(changed)
}

/// Iterates [`flow_round`] to a fixpoint, giving up after `cap` rounds.
pub(crate) fn flow_fixpoint(
    input: &FlowInput,
    state: &mut FlowState,
    cap: usize,
) -> Result<(), FlowError> {
    let mut round = 0;
    let mut converged = false;
    while !converged && round < cap {
        let changed = flow_round(input, state)?;
        converged = !changed;
        round += 1;
    }
    if converged {
        Ok(())
    } else {
        Err(FlowError::NoConvergence)
    }
}

/// The initial state: block 0 entered with an empty stack, slots at their
/// `na` facts, no uses, and every string comparison still a candidate.
pub(crate) fn flow_init(
    n_blocks: usize,
    n_code: usize,
    slot_canonical: &[bool],
    cmp_candidate: &[bool],
) -> FlowState {
    let mut entry = Vec::new();
    let mut entry_set = Vec::new();
    let mut b = 0;
    while b < n_blocks {
        entry.push(Vec::new());
        entry_set.push(false);
        b += 1;
    }
    // Indexes (and so panics on a block-less analysis) exactly like the
    // direct set-based code's `entry[0] = Some(Vec::new())`.
    entry_set[0] = true;
    let mut slots = Vec::new();
    let mut s = 0;
    while s < slot_canonical.len() {
        slots.push(ValueFact {
            producers: Vec::new(),
            canonical_string: slot_canonical[s],
        });
        s += 1;
    }
    let mut uses = Vec::new();
    let mut p = 0;
    while p < n_code {
        uses.push(Vec::new());
        p += 1;
    }
    FlowState {
        entry,
        entry_set,
        slots,
        uses,
        cmp_ok: cmp_candidate.to_vec(),
    }
}

/// True only when every use chain from `pc` ends in a discarded chart
/// effect (`Pop` or a [`InstrClass::ChartSink`]) and passes only through
/// chart producers and `RegisterDrawing`. Cycles fail closed. `visiting`
/// holds the current path; like the direct set-based proof, a pc with no
/// uses is left marked (harmless: revisiting it fails either way).
pub(crate) fn downstream_is_chart_only(
    pc: usize,
    uses: &[Vec<usize>],
    classes: &[InstrClass],
    visiting: &mut Vec<bool>,
) -> bool {
    if visiting[pc] {
        return false;
    }
    visiting[pc] = true;
    let consumers: &[usize] = &uses[pc];
    if consumers.is_empty() {
        return false;
    }
    let mut ok = true;
    let mut i = 0;
    while ok && i < consumers.len() {
        let consumer = consumers[i];
        let class = classes[consumer];
        ok = match class {
            InstrClass::Pop | InstrClass::ChartSink => true,
            InstrClass::ChartProducer | InstrClass::RegisterDrawing => {
                downstream_is_chart_only(consumer, uses, classes, visiting)
            }
            InstrClass::Other => false,
        };
        i += 1;
    }
    visiting[pc] = false;
    ok
}

/// Chart-only sites: every chart producer whose uses are all chart-only,
/// plus every chart sink and `RegisterDrawing` (whose producers are erased
/// only when their own proof succeeds).
pub(crate) fn chart_only_sites(uses: &[Vec<usize>], classes: &[InstrClass]) -> Vec<bool> {
    let n = classes.len();
    let mut out = vec![false; n];
    let mut pc = 0;
    while pc < n {
        let class = classes[pc];
        out[pc] = match class {
            InstrClass::ChartProducer => {
                let mut visiting = vec![false; n];
                downstream_is_chart_only(pc, uses, classes, &mut visiting)
            }
            InstrClass::ChartSink | InstrClass::RegisterDrawing => true,
            InstrClass::Pop | InstrClass::Other => false,
        };
        pc += 1;
    }
    out
}

/// Clears the host-site flag of every projected input, canonical string
/// comparison and chart-only site.
pub(crate) fn erase_host_sites(
    host_site: &mut [bool],
    projected_input: &[bool],
    canonical_cmp: &[bool],
    chart_only: &[bool],
) {
    let mut pc = 0;
    while pc < projected_input.len() {
        if projected_input[pc] || canonical_cmp[pc] || chart_only[pc] {
            host_site[pc] = false;
        }
        pc += 1;
    }
}
