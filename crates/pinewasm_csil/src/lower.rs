//! MIR → CSIL lowering.
//!
//! MIR is a register machine; CSIL is a stack machine. Because MIR is
//! emitted from expression trees in evaluation order, every register is
//! defined immediately before its single use, so the operand stack mirrors
//! the register definitions exactly. Lowering tracks that correspondence
//! with a simulated stack of [`ValueId`]s and reports an internal error if
//! the discipline is ever violated (which would indicate a compiler bug,
//! not a user error).

use crate::{CsilFunction, CsilHeader, CsilInstr, CsilPlot, CsilProgram, F64Bits, ProgramMetadata};
use pinewasm_hir::PlotColorRole;
use pinewasm_hir::{BinOp, UnOp};
use pinewasm_mir::{MirBlock, MirBody, MirInstr, Terminator, Value, ValueId};
use pinewasm_span::Span;
use thiserror::Error;

/// Extraction roots for the Aeneas extraction. Built only with
/// `--features aeneas`.
#[cfg(feature = "aeneas")]
pub mod aeneas_roots_csil_stackify;

mod flow_alias;
#[cfg(test)]
mod flow_pass_tests;
mod jump_thread;

/// Internal lowering failure. Surfaced by the pipeline as an internal-error
/// diagnostic; user programs cannot legitimately trigger it.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum LowerError {
    /// The simulated stack held a different register than the instruction
    /// consumes: the found register's id, or `None` for an empty stack.
    #[error(
        "stack/register mismatch at {0:?}: expected v{1}, found {found}",
        found = found_description(.2.as_ref())
    )]
    StackMismatch(Span, u32, Option<u32>),
    #[error("operand stack not empty at block boundary ({0} values left)")]
    UnbalancedBlock(usize),
    /// A `LazyBoolGuard` whose operator is not `and`/`or` (the operator it
    /// carried).
    #[error(
        "lazy boolean guard at {0:?} is malformed: `{op}` is not a lazy boolean operator",
        op = .1.symbol()
    )]
    MalformedLazyGuard(Span, BinOp),
    #[error("{0} lazy boolean guard(s) still open at a block boundary")]
    UnclosedLazyGuard(usize),
    /// A `MirInstr::Const` carried a runtime-only value (a collection handle).
    /// Handles are produced by collection opcodes at runtime, never as
    /// compile-time constants, so this is an internal-invariant violation, not
    /// a user error.
    #[error("cannot lower a heap reference as a constant at {0:?}")]
    NonConstValue(Span),
    /// A block whose id is not a block of the body (`>= blocks.len()`).
    #[error("block id b{0} is out of range")]
    BlockIdOutOfRange(u32),
    /// The block list contains the same id more than once.
    #[error("block id b{0} occurs more than once")]
    DuplicateBlockId(u32),
    /// The dense block-id range contains an id with no corresponding block.
    #[error("block id b{0} is missing")]
    MissingBlockId(u32),
    /// A terminator naming a block that is not in the body.
    #[error("branch target b{1} at {0:?} is out of range")]
    BranchTargetOutOfRange(Span, u32),
    /// Two control-flow predecessors disagree about the exact live register
    /// stack at a block entry. Equal heights alone are insufficient: every
    /// register must occupy the same stack position on every incoming edge.
    #[error("predecessors disagree about the operand stack at block b{0}")]
    StackMergeMismatch(u32),
    /// Predecessors disagree about which lazy boolean guards remain open.
    #[error("predecessors disagree about open lazy guards at block b{0}")]
    GuardMergeMismatch(u32),
    /// An instruction defining a register that is still live on the
    /// simulated stack below its operands (or twice in one `CallTa`).
    #[error("register v{1} redefined while live at {0:?}")]
    RedefinedRegister(Span, u32),
    /// An instruction with more operands than its `u8` count encodes.
    #[error("{1} operands at {0:?} exceed the u8 operand count")]
    TooManyOperands(Span, usize),
    /// An `and`/`or` combine closing a lazy guard opened for the other
    /// operator.
    #[error("lazy boolean guard closed by `{op}` at {0:?} was opened for the other operator", op = .1.symbol())]
    LazyGuardOperatorMismatch(Span, BinOp),
    /// [`lower_mir_validated`] only: the in-process translation validator
    /// rejected the output of an optimization pass (a compiler bug).
    #[error("translation validation rejected the output of `{pass}`: {error:?}")]
    PassRejected {
        pass: &'static str,
        error: crate::PairCheckError,
    },
}

/// What [`LowerError::StackMismatch`] found on the simulated stack.
fn found_description(found: Option<&u32>) -> String {
    match found {
        Some(value) => format!("v{value}"),
        None => "empty stack".to_string(),
    }
}

/// The exact instruction sequence and source spans at one lowering boundary.
/// Other program fields are still taken from the typed MIR and must be bound
/// before certificate admission.
#[derive(Debug, Clone, PartialEq)]
pub struct PassSnapshot {
    pub code: Vec<CsilInstr>,
    pub source_map: Vec<Span>,
}

/// The input and output of the existing single-use slot coalescing pass,
/// and the keep mask the pass emitted for this rewrite (`false` exactly at
/// both instructions of every erased store/reload pair). The mask is the
/// pass's own certificate, the one the validated pipeline checks; a consumer
/// need not reconstruct it.
#[derive(Debug, Clone, PartialEq)]
pub struct CoalescingCapture {
    pub source: PassSnapshot,
    pub target: PassSnapshot,
    pub keep_mask: Vec<bool>,
}

/// Ordinary lowered program plus an opt-in intermediate pass capture.
#[derive(Debug, Clone, PartialEq)]
pub struct LoweredWithPassRecord {
    pub program: CsilProgram,
    pub coalescing: CoalescingCapture,
}

impl LoweredWithPassRecord {
    /// Materialize the complete programs at the existing pass boundary.
    /// Every non-code field is constructed by the same lowering invocation;
    /// the two snapshots replace only the main function's code and spans.
    /// This opt-in operation clones the program and is not called by ordinary
    /// lowering. Certificate admission must still validate and decode both
    /// bodies.
    pub fn coalescing_programs(&self) -> (CsilProgram, CsilProgram) {
        let materialize = |snapshot: &PassSnapshot| {
            let mut program = self.program.clone();
            let main = &mut program.functions[0];
            main.code.clone_from(&snapshot.code);
            main.source_map.clone_from(&snapshot.source_map);
            main.budget_reference = None;
            program
        };
        (
            materialize(&self.coalescing.source),
            materialize(&self.coalescing.target),
        )
    }
}

pub fn lower_mir(mir: &MirBody) -> Result<CsilProgram, LowerError> {
    lower_mir_with(mir, optimize)
}

/// [`lower_mir`] with in-process translation validation (opt-in; off in the
/// default pipeline). Each of the five optimization passes runs under its
/// checker: the dead-literal and single-use passes under
/// [`crate::check_pair_erasure`], the entry-alias pass under
/// `translation_validation::check_alias`, every round of the flow-alias pass under
/// `flow_alias::check_flow_alias` and jump threading under
/// `jump_thread::check_jump_thread`. A checker must accept the pass's input,
/// its output and the certificate the pass itself emitted (its keep mask, and
/// for the alias passes their renaming tables, for jump threading its new
/// branch targets), or lowering fails with [`LowerError::PassRejected`]. On
/// success the program is identical to [`lower_mir`]'s.
pub fn lower_mir_validated(mir: &MirBody) -> Result<CsilProgram, LowerError> {
    let mut rejected = None;
    let program = lower_mir_with(mir, |code, spans, slots| {
        rejected = optimize_validated(code, spans, slots).err();
    })?;
    match rejected {
        Some(error) => Err(error),
        None => Ok(program),
    }
}

/// [`optimize`], validating every stage. Each stage runs exactly as in
/// [`optimize_stages`] ([`optimize_stage`]: the pass's analysis emits its
/// certificate, then the rewrite is applied), on the output of the previous
/// stage; [`check_stage`] must then accept the stage's input, its output and
/// the certificate. There is no second run of any pass. Stops at the first
/// rejected stage.
fn optimize_validated(
    code: &mut Vec<CsilInstr>,
    spans: &mut Vec<Span>,
    slots: &[crate::SlotDef],
) -> Result<(), LowerError> {
    let mut stage = 0;
    while stage < OPT_STAGES {
        let (before, before_spans) = (code.clone(), spans.clone());
        let cert = optimize_stage(stage, code, spans, slots);
        if let Err(error) = check_stage(&before, &before_spans, code, spans, &cert, slots) {
            return Err(LowerError::PassRejected {
                pass: OPT_STAGE_PASSES[stage],
                error,
            });
        }
        stage += 1;
    }
    Ok(())
}

/// Check one stage of [`optimize_stages`] against the certificate it emitted:
/// a pair-erasure keep mask through [`crate::check_pair_erasure`] (its pairs
/// read back by [`crate::translation_validation::pair_starts`]), an entry-alias keep mask and
/// renaming table through [`crate::translation_validation::check_alias`] (its copies read back by
/// [`crate::translation_validation::alias_pairs`]). The certificate is untrusted: a wrong one can
/// only make the checker reject.
fn check_stage(
    before: &[CsilInstr],
    before_spans: &[Span],
    after: &[CsilInstr],
    after_spans: &[Span],
    cert: &StageCert,
    slots: &[crate::SlotDef],
) -> Result<(), crate::PairCheckError> {
    match cert {
        StageCert::Pairs(keep) => {
            let starts = crate::translation_validation::pair_starts(keep);
            crate::check_pair_erasure(before, before_spans, after, after_spans, &starts, slots)
        }
        StageCert::Aliases(keep, aliases) => {
            let pairs = crate::translation_validation::alias_pairs(before, keep);
            crate::translation_validation::check_alias(
                before,
                before_spans,
                after,
                after_spans,
                &pairs,
                aliases,
                slots,
            )
        }
        StageCert::FlowAliases(rounds) => {
            // Replay the rounds: each must be accepted on the previous
            // round's output, and the last output must be the stage's.
            let (mut code, mut spans) = (before.to_vec(), before_spans.to_vec());
            for (keep, aliases) in rounds {
                let (input, input_spans) = (code.clone(), spans.clone());
                rename_entry_aliases(&mut code, aliases);
                retain_instructions(&mut code, &mut spans, keep);
                if flow_alias::check_flow_alias(
                    &input,
                    &input_spans,
                    &code,
                    &spans,
                    keep,
                    aliases,
                    slots,
                )
                .is_err()
                {
                    return Err(crate::PairCheckError::FlowAliasRejected);
                }
            }
            let same = code.len() == after.len()
                && spans == after_spans
                && code
                    .iter()
                    .zip(after)
                    .all(|(left, right)| crate::instr_exact_eq(left, right));
            if same {
                Ok(())
            } else {
                Err(crate::PairCheckError::FlowAliasRejected)
            }
        }
        StageCert::Threads(cert) => {
            jump_thread::check_jump_thread(before, before_spans, after, after_spans, cert)
                .map_err(|_| crate::PairCheckError::JumpThreadRejected)
        }
    }
}

/// Lower with the same pass pipeline as [`lower_mir`] (the stages of
/// `optimize_stages`), retaining both sides of `coalesce_single_use_slots`
/// and the keep mask that stage emitted, for offline verification.
pub fn lower_mir_with_pass_record(mir: &MirBody) -> Result<LoweredWithPassRecord, LowerError> {
    let mut coalescing = None;
    let program = lower_mir_with(mir, |code, spans, slots| {
        optimize_stage(0, code, spans, slots);
        let source = PassSnapshot {
            code: code.clone(),
            source_map: spans.clone(),
        };
        let keep_mask = optimize_stage(1, code, spans, slots).into_keep_mask();
        coalescing = Some(CoalescingCapture {
            source,
            target: PassSnapshot {
                code: code.clone(),
                source_map: spans.clone(),
            },
            keep_mask,
        });
        let mut stage = 2;
        while stage < OPT_STAGES {
            optimize_stage(stage, code, spans, slots);
            stage += 1;
        }
    })?;
    Ok(LoweredWithPassRecord {
        program,
        coalescing: coalescing.expect("capture is constructed when enabled"),
    })
}

/// [`lower_mir`] with the optimization pipeline supplied by the caller, so
/// tests can lower the same MIR through the `cfg(test)` reference passes.
fn lower_mir_with(
    mir: &MirBody,
    optimize: impl FnOnce(&mut Vec<CsilInstr>, &mut Vec<Span>, &[crate::SlotDef]),
) -> Result<CsilProgram, LowerError> {
    lower_mir_with_outcome(mir, optimize).map(|(program, _)| program)
}

/// [`lower_mir`] together with what the whole-part rewrite did to this
/// program, so a test can assert that the shipped lowering of a real program
/// did not skip its analysis.
#[cfg(test)]
pub(crate) fn lower_mir_outcome_for_tests(
    mir: &MirBody,
) -> Result<(CsilProgram, crate::fraction::Outcome), LowerError> {
    lower_mir_with_outcome(mir, optimize)
}

/// [`lower_mir_with`], also returning the outcome of the whole-part rewrite.
fn lower_mir_with_outcome(
    mir: &MirBody,
    optimize: impl FnOnce(&mut Vec<CsilInstr>, &mut Vec<Span>, &[crate::SlotDef]),
) -> Result<(CsilProgram, crate::fraction::Outcome), LowerError> {
    let mut ctx = stackify(&mir.blocks)?;
    if let Some(code_index) = apply_fixups(&mut ctx.code, &ctx.block_starts, &ctx.fixups) {
        let other = &ctx.code[code_index];
        unreachable!("fixup points at non-jump instruction {other:?}")
    }

    // An int that may hold a fraction (Pine Script® `int / int`) reaches an
    // integer-only consumer as its whole part; see `fraction.rs`. This runs
    // before the budget reference is taken, so the reference and the
    // optimized code both carry the inserted casts. A rewrite skipped
    // because the code could not be analyzed is counted
    // (`fraction::not_analyzed_count`).
    let outcome = crate::fraction::truncate_lowered(
        &mut ctx.code,
        &mut ctx.source_map,
        &mir.metadata.ta_states,
        mir.metadata.series_slots.len(),
    );

    let reference = crate::BudgetReference {
        version: crate::BUDGET_REFERENCE_VERSION,
        code: ctx.code.clone(),
        source_map: ctx.source_map.clone(),
        max_stack: ctx.max_stack,
    };

    optimize(
        &mut ctx.code,
        &mut ctx.source_map,
        &mir.metadata.series_slots,
    );

    let mut program = assemble_program(mir, ctx);
    if program.main().code != reference.code && crate::budget_reference_within_limit(&reference) {
        let mut reference = reference;
        reference.version =
            reference_version(&reference, program.main(), &mir.metadata.series_slots);
        program.functions[0].budget_reference = Some(reference);
    }
    Ok((program, outcome))
}

/// The version a retained budget reference is emitted with: the lowest whose
/// pipeline reproduces the optimized function. A program the flow-alias and
/// jump-threading stages leave unchanged keeps the three-stage version 1, so
/// its serialized artifact has the three-stage layout; any other program gets
/// version 2.
fn reference_version(
    reference: &crate::BudgetReference,
    target: &CsilFunction,
    slots: &[crate::SlotDef],
) -> u32 {
    let mut code = reference.code.clone();
    let mut spans = reference.source_map.clone();
    optimize_three_stage(&mut code, &mut spans, slots);
    if code == target.code && spans == target.source_map {
        crate::BUDGET_REFERENCE_VERSION_THREE_STAGE
    } else {
        crate::BUDGET_REFERENCE_VERSION
    }
}

/// The stackifier's patched output for `mir`, before the whole-part rewrite
/// and the optimizer: what [`lower_mir_with`] hands to
/// [`crate::fraction::truncate_for_integer_consumers`].
#[cfg(test)]
pub(crate) fn stackified_for_tests(mir: &MirBody) -> (Vec<CsilInstr>, Vec<Span>) {
    let mut ctx = stackify(&mir.blocks).expect("stackify");
    let unpatched = apply_fixups(&mut ctx.code, &ctx.block_starts, &ctx.fixups);
    // No macro here.
    unpatched
        .map_or(Some(()), |_| None)
        .expect("every fixup points at a jump");
    (ctx.code, ctx.source_map)
}

/// Stackify every MIR block into one CSIL instruction stream, leaving the
/// branch targets unpatched (`u32::MAX`, recorded in `fixups`). A worklist
/// propagates the exact ordered register stack and open lazy guards from each
/// predecessor to its successors. Nested expression diamonds need not have
/// topological block ids, so a block is lowered as soon as its first incoming
/// state is known; later predecessors must agree with it exactly.
///
/// This and the [`LowerCtx`] methods it calls are written in the subset
/// Aeneas extracts (index `while` loops with at most one level of early
/// exit, no `format!`, no string processing); MIR constants, whose payloads
/// carry `f64` and `String`, cross into CSIL only through [`const_instr`].
/// `cfg(test)` keeps the prior form as a differential reference.
fn stackify(blocks: &[MirBlock]) -> Result<LowerCtx, LowerError> {
    let count = blocks.len();
    if count == 0 {
        return Ok(LowerCtx::new(0));
    }

    // MIR block ids are dense, but the slice may be in any order. Build an
    // explicit id-to-slice mapping and reject duplicates or holes before a
    // target can be used as an index.
    let block_positions = stackify_block_positions(blocks)?;
    stackify_check_dense(&block_positions)?;

    let (analysis, code_ranges) = stackify_worklist(blocks, &block_positions)?;

    let mut unclosed_guards = 0;
    let mut code_index = 0;
    while code_index < analysis.code.len() {
        if matches!(
            analysis.code[code_index],
            CsilInstr::JumpIfExact {
                target: u32::MAX,
                ..
            }
        ) {
            unclosed_guards += 1;
        }
        code_index += 1;
    }
    if unclosed_guards > 0 {
        return Err(LowerError::UnclosedLazyGuard(unclosed_guards));
    }

    // The analysis order follows CFG readiness, which may differ from the
    // original MIR layout for nested or non-topological diamonds. Restore the
    // established input layout after checking every incoming state so code,
    // constants, and source maps remain deterministic.
    let mut ctx = LowerCtx::new(count);
    let analysis_code = analysis.code;
    let analysis_source_map = analysis.source_map;
    let analysis_fixups = analysis.fixups;
    let mut old_to_new = vec![usize::MAX; analysis_code.len() + 1];
    let mut reordered_code = Vec::with_capacity(analysis_code.len());
    let mut reordered_source_map = Vec::with_capacity(analysis_source_map.len());
    let mut index = 0;
    while index < count {
        let block = &blocks[index];
        let id = block.id.0 as usize;
        // Every block was lowered by the worklist. `unwrap` rather than
        // `expect`: Aeneas cannot extract string literals without a
        // native-decide axiom.
        let (start, end) = code_ranges[id].unwrap();
        ctx.block_starts[id] = reordered_code.len() as u32;
        let mut code_index = start;
        while code_index < end {
            old_to_new[code_index] = reordered_code.len();
            reordered_code.push(analysis_code[code_index].clone());
            reordered_source_map.push(analysis_source_map[code_index]);
            code_index += 1;
        }
        index += 1;
    }

    old_to_new[analysis_code.len()] = reordered_code.len();
    // Lazy-boolean jumps target an instruction position and were patched in
    // analysis order. Relocate those positions into the established layout.
    let mut code_index = 0;
    while code_index < reordered_code.len() {
        if let CsilInstr::JumpIfExact { target, .. } = &mut reordered_code[code_index] {
            let old_target = *target as usize;
            if old_target < old_to_new.len() && old_to_new[old_target] != usize::MAX {
                *target = old_to_new[old_target] as u32;
            }
        }
        code_index += 1;
    }
    for (old_index, target) in analysis_fixups {
        let new_index = old_to_new[old_index as usize];
        ctx.fixups.push((new_index as u32, target));
    }

    // String constants were interned while blocks were visited in CFG order.
    // Re-intern the emitted pushes in MIR layout order to keep the serialized
    // constant pool byte-for-byte stable.
    for instr in &mut reordered_code {
        if let CsilInstr::PushStr(index) = instr {
            let value = &analysis.constants[*index as usize];
            *index = intern_string(&mut ctx.constants, value);
        }
    }

    ctx.code = reordered_code;
    ctx.source_map = reordered_source_map;
    ctx.max_stack = analysis.max_stack;

    Ok(ctx)
}

/// Id-to-slice mapping of [`stackify`].
fn stackify_block_positions(blocks: &[MirBlock]) -> Result<Vec<usize>, LowerError> {
    let count = blocks.len();
    let mut block_positions = vec![usize::MAX; count];
    let mut index = 0;
    while index < count {
        let id = blocks[index].id.0 as usize;
        if id >= count {
            return Err(LowerError::BlockIdOutOfRange(blocks[index].id.0));
        }
        if block_positions[id] != usize::MAX {
            return Err(LowerError::DuplicateBlockId(blocks[index].id.0));
        }
        block_positions[id] = index;
        index += 1;
    }
    Ok(block_positions)
}

/// Hole check of [`stackify`].
fn stackify_check_dense(block_positions: &[usize]) -> Result<(), LowerError> {
    let mut id = 0;
    while id < block_positions.len() {
        if block_positions[id] == usize::MAX {
            return Err(LowerError::MissingBlockId(id as u32));
        }
        id += 1;
    }
    Ok(())
}

/// Analysis-order `[start, end)` code range of each lowered block, by id.
type BlockCodeRanges = Vec<Option<(usize, usize)>>;

/// Worklist phase of [`stackify`].
fn stackify_worklist(
    blocks: &[MirBlock],
    block_positions: &[usize],
) -> Result<(LowerCtx, BlockCodeRanges), LowerError> {
    let count = blocks.len();
    let mut entries: Vec<Option<Vec<ValueId>>> = vec![None; count];
    let mut guard_entries: Vec<Option<Vec<(u32, ValueId)>>> = vec![None; count];
    let mut code_ranges: Vec<Option<(usize, usize)>> = vec![None; count];
    let mut processed = vec![false; count];
    // The current MIR builder creates entry block b0. Keep this invariant
    // explicit here because the stackifier API (also used by its extraction
    // root) receives the block slice rather than a separate entry id.
    entries[0] = Some(Vec::new());
    guard_entries[0] = Some(Vec::new());
    let mut processed_count = 0;
    let mut analysis = LowerCtx::new(count);

    while processed_count < count {
        let mut next_id = 0;
        while next_id < count && (processed[next_id] || entries[next_id].is_none()) {
            next_id += 1;
        }

        if next_id == count {
            // Preserve validation of dead MIR blocks too. Seed an unprocessed
            // source in the remaining graph with an empty stack. Prefer a
            // node without an unprocessed predecessor so block-id ordering
            // cannot seed a join before its incoming paths are examined; a
            // cycle with no source uses its lowest id as a deterministic root.
            next_id = unprocessed_root(blocks, block_positions, &processed);
            entries[next_id] = Some(Vec::new());
            guard_entries[next_id] = Some(Vec::new());
        }

        let block = &blocks[block_positions[next_id]];
        let code_start = analysis.code.len();
        // A ready block has an incoming stack and guard state (see `expect`
        // note above for why this is `unwrap`).
        analysis
            .sim_stack
            .clone_from(entries[next_id].as_ref().unwrap());
        analysis
            .pending_guards
            .clone_from(guard_entries[next_id].as_ref().unwrap());
        analysis.update_max_stack();
        analysis.lower_block(block)?;
        code_ranges[next_id] = Some((code_start, analysis.code.len()));
        processed[next_id] = true;
        processed_count += 1;

        let outgoing = analysis.sim_stack.clone();
        let outgoing_guards = analysis.pending_guards.clone();
        match &block.terminator {
            Terminator::Return { .. } => {}
            Terminator::Jump { target, .. } => {
                propagate_stack_state(&mut entries, target.0, &outgoing)?;
                propagate_guard_state(&mut guard_entries, target.0, &outgoing_guards)?;
            }
            Terminator::Branch {
                then_block,
                else_block,
                ..
            } => {
                propagate_stack_state(&mut entries, then_block.0, &outgoing)?;
                propagate_stack_state(&mut entries, else_block.0, &outgoing)?;
                propagate_guard_state(&mut guard_entries, then_block.0, &outgoing_guards)?;
                propagate_guard_state(&mut guard_entries, else_block.0, &outgoing_guards)?;
            }
            Terminator::BranchIfSlotWritten {
                written_block,
                unwritten_block,
                ..
            } => {
                propagate_stack_state(&mut entries, written_block.0, &outgoing)?;
                propagate_stack_state(&mut entries, unwritten_block.0, &outgoing)?;
                propagate_guard_state(&mut guard_entries, written_block.0, &outgoing_guards)?;
                propagate_guard_state(&mut guard_entries, unwritten_block.0, &outgoing_guards)?;
            }
        }
    }

    Ok((analysis, code_ranges))
}

/// Choose a deterministic root for an as-yet-unreached part of the CFG.
/// Prefer a node with no predecessor among the remaining nodes; if the
/// remaining graph is cyclic, choose its lowest block id.
fn unprocessed_root(blocks: &[MirBlock], block_positions: &[usize], processed: &[bool]) -> usize {
    let mut candidate = 0;
    while candidate < processed.len() {
        if !processed[candidate] {
            let mut has_unprocessed_predecessor = false;
            let mut source_id = 0;
            while source_id < processed.len() && !has_unprocessed_predecessor {
                if !processed[source_id] {
                    let source = &blocks[block_positions[source_id]];
                    has_unprocessed_predecessor =
                        terminator_targets(&source.terminator, candidate as u32);
                }
                source_id += 1;
            }
            if !has_unprocessed_predecessor {
                return candidate;
            }
        }
        candidate += 1;
    }

    let mut first = 0;
    while first < processed.len() && processed[first] {
        first += 1;
    }
    first
}

fn terminator_targets(terminator: &Terminator, target: u32) -> bool {
    match terminator {
        Terminator::Return { .. } => false,
        Terminator::Jump {
            target: successor, ..
        } => successor.0 == target,
        Terminator::Branch {
            then_block,
            else_block,
            ..
        } => then_block.0 == target || else_block.0 == target,
        Terminator::BranchIfSlotWritten {
            written_block,
            unwritten_block,
            ..
        } => written_block.0 == target || unwritten_block.0 == target,
    }
}

fn propagate_stack_state(
    entries: &mut [Option<Vec<ValueId>>],
    target: u32,
    outgoing: &[ValueId],
) -> Result<(), LowerError> {
    let Some(entry) = entries.get_mut(target as usize) else {
        // `lower_block` validates targets before this edge is propagated.
        return Err(LowerError::BlockIdOutOfRange(target));
    };
    match entry {
        None => *entry = Some(outgoing.to_vec()),
        Some(existing) if existing.as_slice() == outgoing => {}
        Some(_) => return Err(LowerError::StackMergeMismatch(target)),
    }
    Ok(())
}

fn propagate_guard_state(
    entries: &mut [Option<Vec<(u32, ValueId)>>],
    target: u32,
    outgoing: &[(u32, ValueId)],
) -> Result<(), LowerError> {
    let Some(entry) = entries.get_mut(target as usize) else {
        // `lower_block` validates targets before this edge is propagated.
        return Err(LowerError::BlockIdOutOfRange(target));
    };
    match entry {
        None => *entry = Some(outgoing.to_vec()),
        Some(existing) if existing.as_slice() == outgoing => {}
        Some(_) => return Err(LowerError::GuardMergeMismatch(target)),
    }
    Ok(())
}

/// Patch jump targets now that every block's start offset is known. Returns
/// the code index of the first fixup that names a non-jump instruction (the
/// caller reports it as an internal invariant violation) and stops there;
/// `None` when every fixup was patched.
fn apply_fixups(
    code: &mut [CsilInstr],
    block_starts: &[u32],
    fixups: &[(u32, u32)],
) -> Option<usize> {
    let mut index = 0;
    while index < fixups.len() {
        let (code_index, block) = fixups[index];
        let target = block_starts[block as usize];
        match &mut code[code_index as usize] {
            CsilInstr::Jump { target: t } => *t = target,
            CsilInstr::JumpIfFalse { target: t } => *t = target,
            CsilInstr::JumpIfSlotWritten { target: t, .. } => *t = target,
            _ => return Some(code_index as usize),
        }
        index += 1;
    }
    None
}

/// Builds the compiled [`CsilPlot`]s for [`crate::plot_table::plot_table`]:
/// each keeps the MIR plot's title, style and color roles.
struct CsilPlots;

impl crate::plot_table::PlotTableHost<pinewasm_hir::PlotInfo, CsilPlot> for CsilPlots {
    fn plot_entry(&self, plot: &pinewasm_hir::PlotInfo) -> CsilPlot {
        CsilPlot {
            title: plot.title.clone(),
            style: plot.style.clone(),
            color_roles: plot.color_roles.clone(),
        }
    }
}

/// Assemble the program around the lowered `main` function: the header and
/// the metadata tables copied from the MIR.
fn assemble_program(mir: &MirBody, ctx: LowerCtx) -> CsilProgram {
    CsilProgram {
        header: CsilHeader {
            runtime_abi_version: if ctx.code.iter().any(|i| {
                matches!(
                    i,
                    CsilInstr::CallCollection {
                        func: crate::CollectionFunc::InternalDistanceScan
                            | crate::CollectionFunc::InternalDistanceScanEligible,
                        ..
                    }
                )
            }) {
                crate::INDEXED_SCAN_ABI_VERSION
            } else if mir.metadata.ta_input_bindings.is_empty() {
                0
            } else {
                crate::TA_INPUT_ABI_VERSION
            },
            ..CsilHeader::default()
        },
        constants: ctx.constants,
        series_slots: mir.metadata.series_slots.clone(),
        ta_states: mir.metadata.ta_states.clone(),
        ta_input_bindings: mir.metadata.ta_input_bindings.clone(),
        inputs: mir.metadata.inputs.clone(),
        object_types: mir.metadata.object_types.clone(),
        plots: crate::plot_table::plot_table(&CsilPlots, &mir.metadata.plots),
        visuals: mir
            .metadata
            .visuals
            .iter()
            .map(|visual| crate::CsilVisual {
                title: visual.title.clone(),
                style: visual.style.clone(),
                color_roles: visual.color_roles.clone(),
            })
            .collect(),
        fills: mir
            .metadata
            .fills
            .iter()
            .map(|fill| crate::CsilFill {
                plot1: fill.plot1.0,
                plot2: fill.plot2.0,
                title: fill.title.clone(),
                style: fill.style.clone(),
                color_roles: fill.color_roles.clone(),
            })
            .collect(),
        functions: vec![CsilFunction {
            name: "main".to_string(),
            code: ctx.code,
            source_map: ctx.source_map,
            budget_reference: None,
            max_stack: ctx.max_stack,
        }],
        metadata: ProgramMetadata {
            rendering_contract_version: crate::RENDERING_CONTRACT_VERSION,
            indicator: mir.metadata.indicator.clone(),
            title: mir.metadata.title.clone(),
            overlay: mir.metadata.overlay,
            strategy: mir.metadata.strategy.clone(),
            drawing_caps: mir.metadata.drawing_caps,
            max_bars_back: mir.metadata.max_bars_back,
        },
    }
}

/// The CSIL optimization pipeline, in the order `lower_mir` applies it:
/// dead literal stores, then single-use slot transfers, then entry-prefix
/// aliases, then flow-sensitive aliases (copy propagation through branches
/// and loops), then jump threading. Each pass sees the previous pass's
/// output. It is [`optimize_stages`] with the certificates dropped.
fn optimize(code: &mut Vec<CsilInstr>, spans: &mut Vec<Span>, slots: &[crate::SlotDef]) {
    optimize_stages(code, spans, slots);
}

/// The number of stages of [`optimize_stages`].
const OPT_STAGES: usize = 5;

/// The pass each stage of [`optimize_stages`] runs, for
/// [`LowerError::PassRejected`].
const OPT_STAGE_PASSES: [&str; OPT_STAGES] = [
    "elide_dead_constant_stores",
    "coalesce_single_use_slots",
    "coalesce_entry_aliases",
    "flow_alias_rounds",
    "thread_jumps",
];

/// The certificate one optimization stage emits for its rewrite: the keep
/// mask of a pair-erasure pass, the keep mask and the renaming table of the
/// entry-alias pass, the keep mask and renaming table of every round of the
/// flow-alias pass, or the keep mask and new branch targets of jump
/// threading.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StageCert {
    Pairs(Vec<bool>),
    Aliases(Vec<bool>, Vec<u32>),
    FlowAliases(Vec<flow_alias::RoundCert>),
    Threads(jump_thread::ThreadCert),
}

impl StageCert {
    /// The keep mask of a single-rewrite stage. The flow-alias stage applies
    /// one mask per round and has no single mask; it yields an empty one.
    fn into_keep_mask(self) -> Vec<bool> {
        match self {
            StageCert::Pairs(keep)
            | StageCert::Aliases(keep, _)
            | StageCert::Threads((keep, _)) => keep,
            StageCert::FlowAliases(_) => Vec::new(),
        }
    }
}

/// The optimization pipeline as a list of stages: runs every stage of
/// [`optimize_stage`] in order on `code`/`spans` and returns the certificates
/// they emitted, in order. The final code and spans are [`optimize`]'s.
///
/// Written in the subset Aeneas extracts (a straight-line list, no closures).
fn optimize_stages(
    code: &mut Vec<CsilInstr>,
    spans: &mut Vec<Span>,
    slots: &[crate::SlotDef],
) -> Vec<StageCert> {
    let mut certs = Vec::with_capacity(OPT_STAGES);
    certs.push(optimize_stage(0, code, spans, slots));
    certs.push(optimize_stage(1, code, spans, slots));
    certs.push(optimize_stage(2, code, spans, slots));
    certs.push(optimize_stage(3, code, spans, slots));
    certs.push(optimize_stage(4, code, spans, slots));
    certs
}

/// Run stage `stage` of the pipeline in place and return its certificate:
/// stage 0 is [`elide_dead_constant_stores`], stage 1
/// [`coalesce_single_use_slots`], stage 2 [`coalesce_entry_aliases`],
/// stage 3 the flow-alias rounds ([`flow_alias::flow_alias_rounds`]) and any
/// later stage jump threading ([`jump_thread::thread_jumps`]) (each pass's
/// analysis, then its rewrite).
fn optimize_stage(
    stage: usize,
    code: &mut Vec<CsilInstr>,
    spans: &mut Vec<Span>,
    slots: &[crate::SlotDef],
) -> StageCert {
    if stage == 0 {
        let keep = dead_constant_store_keep(code, slots);
        retain_instructions(code, spans, &keep);
        StageCert::Pairs(keep)
    } else if stage == 1 {
        let keep = single_use_slot_keep(code, slots);
        retain_instructions(code, spans, &keep);
        StageCert::Pairs(keep)
    } else if stage == 2 {
        let (keep, aliases) = entry_alias_plan(code, slots);
        rename_entry_aliases(code, &aliases);
        retain_instructions(code, spans, &keep);
        StageCert::Aliases(keep, aliases)
    } else if stage == 3 {
        StageCert::FlowAliases(flow_alias::flow_alias_rounds(code, spans, slots))
    } else {
        StageCert::Threads(jump_thread::thread_jumps(code, spans))
    }
}

/// Check that a serialized reference deterministically produces this target
/// under the pipeline its version names: the three-stage pipeline for
/// [`crate::BUDGET_REFERENCE_VERSION_THREE_STAGE`] (the pipeline without the
/// flow-alias and jump-threading stages), the full pipeline for
/// [`crate::BUDGET_REFERENCE_VERSION`]. Any other version never matches.
/// Validation also checks that the reference is executable CSIL.
pub(crate) fn matches_budget_reference(
    reference: &crate::BudgetReference,
    target: &CsilFunction,
    slots: &[crate::SlotDef],
) -> bool {
    let mut code = reference.code.clone();
    let mut spans = reference.source_map.clone();
    if reference.version == crate::BUDGET_REFERENCE_VERSION_THREE_STAGE {
        optimize_three_stage(&mut code, &mut spans, slots);
    } else if reference.version == crate::BUDGET_REFERENCE_VERSION {
        optimize(&mut code, &mut spans, slots);
    } else {
        return false;
    }
    code == target.code && spans == target.source_map
}

/// The pipeline of version-1 budget references: the first three stages of
/// [`optimize_stages`], without the flow-alias and jump-threading stages.
fn optimize_three_stage(
    code: &mut Vec<CsilInstr>,
    spans: &mut Vec<Span>,
    slots: &[crate::SlotDef],
) {
    optimize_stage(0, code, spans, slots);
    optimize_stage(1, code, spans, slots);
    optimize_stage(2, code, spans, slots);
}

/// One flag per slot: set when the slot is `var`/`varip`, whose value
/// outlives the bar and is therefore always observed.
fn persistent_slots(slots: &[crate::SlotDef]) -> Vec<bool> {
    let mut observed = Vec::with_capacity(slots.len());
    let mut index = 0;
    while index < slots.len() {
        observed.push(slots[index].persistent || slots[index].varip);
        index += 1;
    }
    observed
}

/// Flag `target` as a branch destination. Out-of-range targets (a branch to
/// the end of the code) have no entry and are ignored.
fn mark_branch_target(targets: &mut [bool], target: u32) {
    let index = target as usize;
    if index < targets.len() {
        targets[index] = true;
    }
}

/// Drop literal-to-slot transfers whose destination is never observed. Input
/// group labels commonly survive semantic analysis as unused runtime strings;
/// their UI metadata is already resolved. Keep the metadata, constant pool,
/// and slot numbering stable, and leave every nonliteral producer untouched.
///
/// The pass is its analysis, [`dead_constant_store_keep`], followed by
/// [`retain_instructions`]; the validated pipeline keeps the analysis's keep
/// mask as the certificate of this rewrite.
///
/// Lowering runs this pass as a stage of [`optimize_stage`]; the named
/// function stays as the pass's reference form (an extraction entry point, and the
/// tests' differential reference).
#[cfg_attr(not(test), allow(dead_code))]
fn elide_dead_constant_stores(
    code: &mut Vec<CsilInstr>,
    spans: &mut Vec<Span>,
    slots: &[crate::SlotDef],
) {
    let keep = dead_constant_store_keep(code, slots);
    retain_instructions(code, spans, &keep);
}

/// The keep mask of [`elide_dead_constant_stores`]: `false` exactly at both
/// instructions of every erased literal store.
///
/// Written in the subset Aeneas extracts (index `while` loops, one match arm
/// per opcode, explicit bounds checks); `cfg(test)` keeps the
/// iterator-based form of the pass as a differential reference. `code` is a
/// `&Vec` so the extracted indexing is the vector's.
#[allow(clippy::ptr_arg)]
fn dead_constant_store_keep(code: &Vec<CsilInstr>, slots: &[crate::SlotDef]) -> Vec<bool> {
    let mut observed = persistent_slots(slots);
    let mut targets = vec![false; code.len()];
    let mut pc = 0;
    while pc < code.len() {
        // The loop guard keeps this index in bounds; retain the extractable form.
        let instruction = &code[pc];
        match *instruction {
            CsilInstr::ReadUserSlot { slot, .. } => observed[slot as usize] = true,
            CsilInstr::ReadUserSlotDynamic { slot } => observed[slot as usize] = true,
            CsilInstr::WindowMean { slot, .. }
            | CsilInstr::WindowMeanDynamic { slot }
            | CsilInstr::WindowExtremumDynamic { slot, .. }
            | CsilInstr::WindowStatisticDynamic { slot, .. } => observed[slot as usize] = true,
            CsilInstr::JumpIfSlotWritten { slot, target } => {
                observed[slot as usize] = true;
                mark_branch_target(&mut targets, target);
            }
            CsilInstr::Jump { target } => mark_branch_target(&mut targets, target),
            CsilInstr::JumpIfFalse { target } => mark_branch_target(&mut targets, target),
            CsilInstr::JumpIfExact { target, .. } => mark_branch_target(&mut targets, target),
            _ => {}
        }
        pc += 1;
    }
    let mut keep = vec![true; code.len()];
    let mut pc = 1;
    while pc < code.len() {
        if let CsilInstr::StoreUserSlotCurrent { slot } = code[pc] {
            if !observed[slot as usize]
                && !targets[pc]
                && matches!(
                    code[pc - 1],
                    CsilInstr::PushNa
                        | CsilInstr::PushBool(_)
                        | CsilInstr::PushI64(_)
                        | CsilInstr::PushF64(_)
                        | CsilInstr::PushStr(_)
                        | CsilInstr::PushColor { .. }
                )
            {
                keep[pc - 1] = false;
                keep[pc] = false;
            }
        }
        pc += 1;
    }
    keep
}

/// Keep a produced value on the operand stack when its sole use immediately
/// reloads the destination. These two transfers add no information: the
/// producer's stack value and the slot's value are equivalent. Restrict this
/// to one-definition, one-use slots with no history or persistence observer.
/// No runtime plan, type check, extra stack cell, or bytecode opcode is needed.
///
/// The pass is its analysis, [`single_use_slot_keep`], followed by
/// [`retain_instructions`], like [`elide_dead_constant_stores`].
///
/// Lowering runs this pass as a stage of [`optimize_stage`]; the named
/// function stays as the pass's reference form (an extraction entry point, and the
/// tests' differential reference).
#[cfg_attr(not(test), allow(dead_code))]
fn coalesce_single_use_slots(
    code: &mut Vec<CsilInstr>,
    spans: &mut Vec<Span>,
    slots: &[crate::SlotDef],
) {
    let keep = single_use_slot_keep(code, slots);
    retain_instructions(code, spans, &keep);
}

/// The keep mask of [`coalesce_single_use_slots`]: `false` exactly at both
/// instructions of every erased store/reload pair. Written in the Aeneas
/// subset, like [`dead_constant_store_keep`].
#[allow(clippy::ptr_arg)]
fn single_use_slot_keep(code: &Vec<CsilInstr>, slots: &[crate::SlotDef]) -> Vec<bool> {
    let mut reads = vec![0usize; slots.len()];
    let mut writes = vec![0usize; slots.len()];
    let mut observed = persistent_slots(slots);
    let mut targets = vec![false; code.len()];
    let mut pc = 0;
    while pc < code.len() {
        // The loop guard keeps this index in bounds; retain the extractable form.
        let instruction = &code[pc];
        match *instruction {
            CsilInstr::ReadUserSlot { slot, offset } => {
                reads[slot as usize] += 1;
                if offset != 0 {
                    observed[slot as usize] = true;
                }
            }
            CsilInstr::StoreUserSlotCurrent { slot } => writes[slot as usize] += 1,
            CsilInstr::ReadUserSlotDynamic { slot } => observed[slot as usize] = true,
            CsilInstr::WindowMean { slot, .. }
            | CsilInstr::WindowMeanDynamic { slot }
            | CsilInstr::WindowExtremumDynamic { slot, .. }
            | CsilInstr::WindowStatisticDynamic { slot, .. } => observed[slot as usize] = true,
            CsilInstr::JumpIfSlotWritten { slot, target } => {
                observed[slot as usize] = true;
                mark_branch_target(&mut targets, target);
            }
            CsilInstr::Jump { target } => mark_branch_target(&mut targets, target),
            CsilInstr::JumpIfFalse { target } => mark_branch_target(&mut targets, target),
            CsilInstr::JumpIfExact { target, .. } => mark_branch_target(&mut targets, target),
            _ => {}
        }
        pc += 1;
    }
    let mut keep = vec![true; code.len()];
    let end = code.len().saturating_sub(1);
    let mut pc = 0;
    while pc < end {
        if let CsilInstr::StoreUserSlotCurrent { slot } = code[pc] {
            let index = slot as usize;
            if !targets[pc + 1] && !observed[index] && reads[index] == 1 && writes[index] == 1 {
                if let CsilInstr::ReadUserSlot {
                    slot: source,
                    offset,
                } = code[pc + 1]
                {
                    if source == slot && offset == 0 {
                        keep[pc] = false;
                        keep[pc + 1] = false;
                    }
                }
            }
        }
        pc += 1;
    }
    keep
}

/// Coalesce immutable copies in the entry's straight-line prefix. A source
/// defined earlier in that prefix cannot change without executing its copies
/// again: the prefix has no intervening branch. Later reads can therefore name
/// the source directly. Keep sources, effects, and evaluation order in place.
///
/// The pass is its analysis, [`entry_alias_plan`] (the keep mask and the
/// renaming table), then [`rename_entry_aliases`] and
/// [`retain_instructions`]; the validated pipeline keeps the mask and the
/// table as the certificate of this rewrite.
///
/// Lowering runs this pass as a stage of [`optimize_stage`]; the named
/// function stays as the pass's reference form (an extraction entry point, and the
/// tests' differential reference).
#[cfg_attr(not(test), allow(dead_code))]
fn coalesce_entry_aliases(
    code: &mut Vec<CsilInstr>,
    spans: &mut Vec<Span>,
    slots: &[crate::SlotDef],
) {
    let (keep, aliases) = entry_alias_plan(code, slots);
    rename_entry_aliases(code, &aliases);
    retain_instructions(code, spans, &keep);
}

/// The analysis of [`coalesce_entry_aliases`]: its keep mask (`false` exactly
/// at the read and the store of every coalesced copy) and its renaming table
/// (`aliases[s]` is the slot every offset-0 read of `s` names afterwards).
/// Written in the Aeneas subset, like [`dead_constant_store_keep`].
#[allow(clippy::ptr_arg)]
fn entry_alias_plan(code: &Vec<CsilInstr>, slots: &[crate::SlotDef]) -> (Vec<bool>, Vec<u32>) {
    let mut writes = vec![0usize; slots.len()];
    let mut definition = vec![usize::MAX; slots.len()];
    let mut first_read = vec![usize::MAX; slots.len()];
    let mut observed = persistent_slots(slots);
    let mut targets = vec![false; code.len()];
    let mut prefix_end = code.len();
    let mut pc = 0;
    while pc < code.len() {
        // The loop guard keeps this index in bounds; retain the extractable form.
        let instruction = &code[pc];
        match *instruction {
            CsilInstr::ReadUserSlot { slot, offset } => {
                first_read[slot as usize] = first_read[slot as usize].min(pc);
                if offset != 0 {
                    observed[slot as usize] = true;
                }
            }
            CsilInstr::StoreUserSlotCurrent { slot } => {
                writes[slot as usize] += 1;
                definition[slot as usize] = pc;
            }
            CsilInstr::ReadUserSlotDynamic { slot } => observed[slot as usize] = true,
            CsilInstr::WindowMean { slot, .. }
            | CsilInstr::WindowMeanDynamic { slot }
            | CsilInstr::WindowExtremumDynamic { slot, .. }
            | CsilInstr::WindowStatisticDynamic { slot, .. } => observed[slot as usize] = true,
            CsilInstr::JumpIfSlotWritten { slot, target } => {
                observed[slot as usize] = true;
                prefix_end = prefix_end.min(pc);
                mark_branch_target(&mut targets, target);
            }
            CsilInstr::Jump { target } => {
                prefix_end = prefix_end.min(pc);
                mark_branch_target(&mut targets, target);
            }
            CsilInstr::JumpIfFalse { target } => {
                prefix_end = prefix_end.min(pc);
                mark_branch_target(&mut targets, target);
            }
            CsilInstr::JumpIfExact { target, .. } => {
                prefix_end = prefix_end.min(pc);
                mark_branch_target(&mut targets, target);
            }
            CsilInstr::Return => prefix_end = prefix_end.min(pc),
            _ => {}
        }
        pc += 1;
    }
    // `slots.len() as u32` truncates exactly like the `(0..n as u32)` range
    // this replaces.
    let slot_count = slots.len() as u32;
    let mut aliases: Vec<u32> = Vec::with_capacity(slot_count as usize);
    let mut alias = 0u32;
    while alias < slot_count {
        aliases.push(alias);
        alias += 1;
    }
    let mut keep = vec![true; code.len()];
    let mut pc = 1;
    while pc < prefix_end {
        if let CsilInstr::StoreUserSlotCurrent { slot: dst } = code[pc] {
            if let CsilInstr::ReadUserSlot { slot: src, offset } = code[pc - 1] {
                let (src_index, dst_index) = (src as usize, dst as usize);
                if offset == 0
                    && !targets[pc]
                    && writes[src_index] == 1
                    && writes[dst_index] == 1
                    && definition[src_index] < pc - 1
                    && !observed[dst_index]
                    && first_read[dst_index] > pc
                {
                    aliases[dst_index] = aliases[src_index];
                    keep[pc - 1] = false;
                    keep[pc] = false;
                }
            }
        }
        pc += 1;
    }
    (keep, aliases)
}

/// Point every offset-0 read of a slot at its alias (`aliases[slot]`).
#[allow(clippy::ptr_arg)]
fn rename_entry_aliases(code: &mut Vec<CsilInstr>, aliases: &Vec<u32>) {
    let mut pc = 0;
    while pc < code.len() {
        if let CsilInstr::ReadUserSlot { slot, offset } = &mut code[pc] {
            if *offset == 0 {
                *slot = aliases[*slot as usize];
            }
        }
        pc += 1;
    }
}

/// Drop every instruction (and its source-map span) whose `keep` flag is
/// false, and rewrite each surviving jump target to its post-removal address.
///
/// Written in the subset Aeneas extracts (index `while` loops, one match arm
/// per jump opcode, fresh vectors instead of `Vec::retain` closures) so that the
/// extracted code is exactly this code; `cfg(test)` keeps the `retain`-based
/// form as a differential reference.
fn retain_instructions(code: &mut Vec<CsilInstr>, spans: &mut Vec<Span>, keep: &[bool]) {
    let mut all_kept = true;
    let mut index = 0;
    while all_kept && index < keep.len() {
        all_kept = keep[index];
        index += 1;
    }
    if all_kept {
        return;
    }
    let mut addresses = Vec::with_capacity(code.len() + 1);
    let mut next = 0u32;
    let mut index = 0;
    while index < keep.len() {
        addresses.push(next);
        next += u32::from(keep[index]);
        index += 1;
    }
    addresses.push(next);
    let mut pc = 0;
    while pc < code.len() {
        match &mut code[pc] {
            CsilInstr::Jump { target } => *target = addresses[*target as usize],
            CsilInstr::JumpIfFalse { target } => *target = addresses[*target as usize],
            CsilInstr::JumpIfExact { target, .. } => *target = addresses[*target as usize],
            CsilInstr::JumpIfSlotWritten { target, .. } => {
                *target = addresses[*target as usize];
            }
            _ => {}
        }
        pc += 1;
    }
    *code = retain_kept(code, keep, next as usize);
    *spans = retain_kept(spans, keep, next as usize);
}

/// The elements of `items` whose `keep` flag is set, in order. Moves every
/// element out of `items` (leaving it empty) and panics, like the
/// `Vec::retain` form it replaces, when `keep` is shorter than `items`.
/// `capacity` only sizes the result (the number of set flags).
fn retain_kept<T>(items: &mut Vec<T>, keep: &[bool], capacity: usize) -> Vec<T> {
    // `mem::take` has no Aeneas model; `mem::replace` does.
    #[allow(clippy::mem_replace_with_default)]
    let items = std::mem::replace(items, Vec::new());
    let mut kept = Vec::with_capacity(capacity);
    for (index, item) in items.into_iter().enumerate() {
        if keep[index] {
            kept.push(item);
        }
    }
    kept
}

/// Stackification state for one MIR body. Public (inside the private
/// `lower` module) only so the `aeneas` feature can re-export it for the
/// extraction roots.
pub struct LowerCtx {
    code: Vec<CsilInstr>,
    source_map: Vec<Span>,
    constants: Vec<String>,
    /// Which MIR register each stack cell corresponds to.
    sim_stack: Vec<ValueId>,
    max_stack: u32,
    block_starts: Vec<u32>,
    /// (code index, target block) pairs needing target patching.
    fixups: Vec<(u32, u32)>,
    /// Open [`MirInstr::LazyBoolGuard`]s: the emitted `JUMP_IF_EXACT_*`'s
    /// code index plus the lhs register it guards. Registers are single-use,
    /// so the downstream `Binary { op: And | Or }` sharing the register is
    /// the guard's unique combine; reaching it pops the entry and patches
    /// the jump to land just past the combine opcode. A ternary branch may
    /// split a guarded expression, so this state follows CFG edges and must
    /// agree exactly at joins.
    pending_guards: Vec<(u32, ValueId)>,
}

/// Index of `s` in the constant pool, appending it when absent.
///
/// The string boundary of stackification: it compares `String`s, which the
/// Aeneas subset does not model, so extraction treats it (through
/// [`const_instr`]) as an opaque kernel.
fn intern_string(constants: &mut Vec<String>, s: &str) -> u32 {
    if let Some(i) = constants.iter().position(|c| c == s) {
        return i as u32;
    }
    constants.push(s.to_string());
    (constants.len() - 1) as u32
}

/// The CSIL push instruction for a MIR constant, interning string payloads
/// into `constants`.
///
/// The value boundary of stackification: MIR [`Value`]s carry `f64` and
/// `String` payloads outside the Aeneas subset, so extraction treats this
/// function (and the `Value` type) as opaque. Everything else in
/// [`LowerCtx::lower_instr`] is extracted.
fn const_instr(
    constants: &mut Vec<String>,
    value: &Value,
    span: Span,
) -> Result<CsilInstr, LowerError> {
    Ok(match value {
        Value::Na => CsilInstr::PushNa,
        Value::Bool(b) => CsilInstr::PushBool(*b),
        Value::Int(v) => CsilInstr::PushI64(*v),
        Value::Float(v) => CsilInstr::PushF64(F64Bits::new(*v)),
        Value::String(s) => CsilInstr::PushStr(intern_string(constants, s)),
        Value::Color { r, g, b, t, .. } => CsilInstr::PushColor {
            r: *r,
            g: *g,
            b: *b,
            t: (*t).into(),
        },
        // Heap handles and displaced-table provenance are runtime-only.
        Value::Ref(_) | Value::DisplacedTable(_) => return Err(LowerError::NonConstValue(span)),
    })
}

#[cfg(test)]
mod displaced_table_constant_tests {
    use super::*;

    #[test]
    fn runtime_displacement_provenance_cannot_be_a_csil_constant() {
        assert!(matches!(
            const_instr(
                &mut Vec::new(),
                &Value::DisplacedTable(std::num::NonZeroU64::new(7).unwrap()),
                Span::dummy()
            ),
            Err(LowerError::NonConstValue(_))
        ));
    }
}

/// A `u8` operand count (`CALL_PURE`/`CALL_COLLECTION`/`LOG argc`, a
/// `color_count`) must fit: `as u8` would silently truncate it.
fn check_count(count: usize, span: Span) -> Result<(), LowerError> {
    if count > 255 {
        Err(LowerError::TooManyOperands(span, count))
    } else {
        Ok(())
    }
}

/// `Ok` iff `target` names one of the `count` blocks.
fn check_target(count: usize, target: u32, span: Span) -> Result<(), LowerError> {
    if target as usize >= count {
        Err(LowerError::BranchTargetOutOfRange(span, target))
    } else {
        Ok(())
    }
}

/// Whether `dsts[index]` already occurs in `dsts[..index]`.
fn repeats_earlier(dsts: &[ValueId], index: usize) -> bool {
    let mut earlier = 0;
    while earlier < index {
        if dsts[earlier].0 == dsts[index].0 {
            return true;
        }
        earlier += 1;
    }
    false
}

impl LowerCtx {
    /// A fresh context for a body of `block_count` blocks; every block start
    /// is `u32::MAX` until the block is laid out.
    fn new(block_count: usize) -> Self {
        LowerCtx {
            code: Vec::new(),
            source_map: Vec::new(),
            constants: Vec::new(),
            sim_stack: Vec::new(),
            max_stack: 0,
            block_starts: vec![u32::MAX; block_count],
            fixups: Vec::new(),
            pending_guards: Vec::new(),
        }
    }

    /// Record the block's start offset, then lower its instructions and its
    /// terminator.
    ///
    /// Before lowering, each instruction and the terminator are checked for
    /// the input conditions the lowering itself does not enforce
    /// ([`Self::check_instr`], [`Self::check_targets`]), and the block id
    /// must name a block of the body: every such violation is a
    /// [`LowerError`] instead of a panic or a silently wrong program.
    fn lower_block(&mut self, block: &MirBlock) -> Result<(), LowerError> {
        if block.id.0 as usize >= self.block_starts.len() {
            return Err(LowerError::BlockIdOutOfRange(block.id.0));
        }
        self.block_starts[block.id.0 as usize] = self.code.len() as u32;
        let mut index = 0;
        while index < block.instrs.len() {
            self.check_instr(&block.instrs[index])?;
            self.lower_instr(&block.instrs[index])?;
            index += 1;
        }
        self.check_targets(&block.terminator)?;
        self.lower_terminator(&block.terminator)
    }

    /// `Ok` iff `dst` is not live on the simulated stack below the top
    /// `uses` cells (the operands the instruction pops before it pushes
    /// `dst`). With fewer than `uses` cells, `Ok`: the lowering reports the
    /// underflow itself.
    fn check_fresh(&self, uses: usize, dst: ValueId, span: Span) -> Result<(), LowerError> {
        let len = self.sim_stack.len();
        if uses > len {
            return Ok(());
        }
        let keep = len - uses;
        let mut index = 0;
        while index < keep {
            if self.sim_stack[index].0 == dst.0 {
                return Err(LowerError::RedefinedRegister(span, dst.0));
            }
            index += 1;
        }
        Ok(())
    }

    /// [`Self::check_fresh`] with `uses = base + extra`; a sum that does not
    /// fit `usize` exceeds any stack, so it is `Ok` (the lowering reports the
    /// underflow).
    fn check_fresh_plus(
        &self,
        base: usize,
        extra: usize,
        dst: ValueId,
        span: Span,
    ) -> Result<(), LowerError> {
        match extra.checked_add(base) {
            Some(uses) => self.check_fresh(uses, dst, span),
            None => Ok(()),
        }
    }

    /// `CallTa`: every result register is fresh, and no two coincide.
    fn check_fresh_all(&self, uses: usize, dsts: &[ValueId], span: Span) -> Result<(), LowerError> {
        let mut index = 0;
        while index < dsts.len() {
            self.check_fresh(uses, dsts[index], span)?;
            if repeats_earlier(dsts, index) {
                return Err(LowerError::RedefinedRegister(span, dsts[index].0));
            }
            index += 1;
        }
        Ok(())
    }

    /// An `and`/`or` combine over `lhs` closes the most recent guard opened
    /// over `lhs` (see [`Self::take_guard`]); that guard must have been
    /// opened for the same operator (`JUMP_IF_EXACT on` is `true` for `or`).
    fn check_guard_op(&self, op: BinOp, lhs: ValueId, span: Span) -> Result<(), LowerError> {
        let on = match op {
            BinOp::Or => true,
            BinOp::And => false,
            _ => return Ok(()),
        };
        let mut index = self.pending_guards.len();
        while index > 0 {
            index -= 1;
            if self.pending_guards[index].1 .0 == lhs.0 {
                let code_index = self.pending_guards[index].0 as usize;
                if code_index < self.code.len() {
                    if let CsilInstr::JumpIfExact { on: opened, .. } = &self.code[code_index] {
                        if *opened != on {
                            return Err(LowerError::LazyGuardOperatorMismatch(span, op));
                        }
                    }
                }
                return Ok(());
            }
        }
        Ok(())
    }

    /// The input checks for one instruction, run before it is lowered:
    /// defined registers are fresh ([`Self::check_fresh`]), `u8` operand
    /// counts fit, and a lazy combine matches its guard's operator. They
    /// hold for every body the MIR builder emits.
    fn check_instr(&self, instr: &MirInstr) -> Result<(), LowerError> {
        let span = instr.span();
        match instr {
            MirInstr::Const { dst, .. }
            | MirInstr::ReadBuiltinSeries { dst, .. }
            | MirInstr::ReadUserSeries { dst, .. }
            | MirInstr::ReadInput { dst, .. }
            | MirInstr::ReadSecurity { dst, .. }
            | MirInstr::ReadSecurityExpr { dst, .. }
            | MirInstr::ReadRequest { dst, .. }
            | MirInstr::BuildFootprint { dst, .. }
            | MirInstr::ReadSecurityLowerTf { dst, .. }
            | MirInstr::ReadPivotLevels { dst, .. }
            | MirInstr::ReadTimeTf { dst, .. }
            | MirInstr::ReadDrawingsAll { dst, .. }
            | MirInstr::WindowMean { dst, .. } => self.check_fresh(0, *dst, span),
            MirInstr::CallPivotPointLevels { dst, args, .. } => {
                check_count(args.len(), span)?;
                self.check_fresh(args.len(), *dst, span)
            }
            MirInstr::ReadBuiltinSeriesDynamic { dst, .. }
            | MirInstr::ReadUserSeriesDynamic { dst, .. }
            | MirInstr::WindowMeanDynamic { dst, .. }
            | MirInstr::WindowExtremumDynamic { dst, .. }
            | MirInstr::WindowStatisticDynamic { dst, .. }
            | MirInstr::Unary { dst, .. }
            | MirInstr::IntToFloat { dst, .. }
            | MirInstr::CopyObject { dst, .. }
            | MirInstr::GetField { dst, .. }
            | MirInstr::RegisterDrawing { dst, .. }
            | MirInstr::DeleteDrawing { dst, .. } => self.check_fresh(1, *dst, span),
            MirInstr::FootprintRowByPrice { dst, .. } | MirInstr::EmaSimpleDynamic { dst, .. } => {
                self.check_fresh(2, *dst, span)
            }
            MirInstr::Binary { dst, op, lhs, .. } => {
                self.check_fresh(2, *dst, span)?;
                self.check_guard_op(*op, *lhs, span)
            }
            MirInstr::Select { dst, .. } => self.check_fresh(3, *dst, span),
            MirInstr::ClearCells { dst, .. } | MirInstr::MergeCells { dst, .. } => {
                self.check_fresh(5, *dst, span)
            }
            MirInstr::CallPure { dst, args, .. } | MirInstr::CallCollection { dst, args, .. } => {
                check_count(args.len(), span)?;
                self.check_fresh(args.len(), *dst, span)
            }
            MirInstr::CallTa { dsts, args, .. } => self.check_fresh_all(args.len(), dsts, span),
            MirInstr::NewObject { dst, fields, .. } => self.check_fresh(fields.len(), *dst, span),
            MirInstr::SetCell { dst, values, .. } => {
                self.check_fresh_plus(3, values.len(), *dst, span)
            }
            MirInstr::Plot { colors, .. }
            | MirInstr::Visual { colors, .. }
            | MirInstr::Fill { colors, .. } => check_count(colors.len(), span),
            MirInstr::Log { operands, .. } => check_count(operands.len(), span),
            MirInstr::StoreUserSeriesCurrent { .. }
            | MirInstr::LazyBoolGuard { .. }
            | MirInstr::SetField { .. }
            | MirInstr::RuntimeError { .. }
            | MirInstr::Alert { .. }
            | MirInstr::AlertCondition { .. }
            | MirInstr::Order { .. }
            | MirInstr::Discard { .. } => Ok(()),
        }
    }

    /// Every block a terminator names is a block of the body, so the fixup
    /// pass can index `block_starts` with it.
    fn check_targets(&self, terminator: &Terminator) -> Result<(), LowerError> {
        let count = self.block_starts.len();
        match terminator {
            Terminator::Return { .. } => Ok(()),
            Terminator::Jump { target, span } => check_target(count, target.0, *span),
            Terminator::Branch {
                then_block,
                else_block,
                span,
                ..
            } => {
                check_target(count, then_block.0, *span)?;
                check_target(count, else_block.0, *span)
            }
            Terminator::BranchIfSlotWritten {
                written_block,
                unwritten_block,
                span,
                ..
            } => {
                check_target(count, written_block.0, *span)?;
                check_target(count, unwritten_block.0, *span)
            }
        }
    }

    fn emit(&mut self, instr: CsilInstr, span: Span) {
        self.code.push(instr);
        self.source_map.push(span);
    }

    fn update_max_stack(&mut self) {
        let depth = self.sim_stack.len() as u32;
        if depth > self.max_stack {
            self.max_stack = depth;
        }
    }

    fn push_value(&mut self, value: ValueId) {
        self.sim_stack.push(value);
        self.update_max_stack();
    }

    fn pop_value(&mut self, expected: ValueId, span: Span) -> Result<(), LowerError> {
        match self.sim_stack.pop() {
            Some(top) => {
                if top.0 == expected.0 {
                    Ok(())
                } else {
                    Err(LowerError::StackMismatch(span, expected.0, Some(top.0)))
                }
            }
            None => Err(LowerError::StackMismatch(span, expected.0, None)),
        }
    }

    /// Pop `values` last to first: they were pushed in order, so the last
    /// one is on top.
    fn pop_values_rev(&mut self, values: &[ValueId], span: Span) -> Result<(), LowerError> {
        let mut index = values.len();
        while index > 0 {
            index -= 1;
            self.pop_value(values[index], span)?;
        }
        Ok(())
    }

    /// Pop the color registers of `colors` last to first, like
    /// [`Self::pop_values_rev`].
    fn pop_colors_rev(
        &mut self,
        colors: &[(PlotColorRole, ValueId)],
        span: Span,
    ) -> Result<(), LowerError> {
        let mut index = colors.len();
        while index > 0 {
            index -= 1;
            self.pop_value(colors[index].1, span)?;
        }
        Ok(())
    }

    /// Remove the most recently opened pending guard over `lhs` and return
    /// its jump's code index, or `None` when no guard is open over `lhs`.
    /// The other guards keep their order.
    fn take_guard(&mut self, lhs: ValueId) -> Option<u32> {
        let count = self.pending_guards.len();
        let mut entry = count;
        let mut index = count;
        while entry == count && index > 0 {
            index -= 1;
            if self.pending_guards[index].1 .0 == lhs.0 {
                entry = index;
            }
        }
        if entry == count {
            return None;
        }
        let (code_index, _) = self.pending_guards[entry];
        let mut index = entry;
        while index + 1 < count {
            self.pending_guards[index] = self.pending_guards[index + 1];
            index += 1;
        }
        self.pending_guards.pop();
        Some(code_index)
    }

    fn lower_instr(&mut self, instr: &MirInstr) -> Result<(), LowerError> {
        let span = instr.span();
        match instr {
            MirInstr::Const { dst, value, .. } => {
                let csil = const_instr(&mut self.constants, value, span)?;
                self.emit(csil, span);
                self.push_value(*dst);
            }
            MirInstr::ReadBuiltinSeries {
                dst,
                source,
                offset,
                ..
            } => {
                self.emit(
                    CsilInstr::ReadBuiltin {
                        source: *source,
                        offset: *offset,
                    },
                    span,
                );
                self.push_value(*dst);
            }
            MirInstr::ReadUserSeries {
                dst, slot, offset, ..
            } => {
                self.emit(
                    CsilInstr::ReadUserSlot {
                        slot: slot.0,
                        offset: *offset,
                    },
                    span,
                );
                self.push_value(*dst);
            }
            MirInstr::ReadBuiltinSeriesDynamic {
                dst,
                source,
                offset,
                ..
            } => {
                // The offset operand is on the stack; the dynamic read pops it
                // and pushes the sampled value.
                self.pop_value(*offset, span)?;
                self.emit(CsilInstr::ReadBuiltinDynamic { source: *source }, span);
                self.push_value(*dst);
            }
            MirInstr::ReadUserSeriesDynamic {
                dst, slot, offset, ..
            } => {
                self.pop_value(*offset, span)?;
                self.emit(CsilInstr::ReadUserSlotDynamic { slot: slot.0 }, span);
                self.push_value(*dst);
            }
            MirInstr::ReadInput { dst, input, .. } => {
                self.emit(CsilInstr::ReadInput { input: *input }, span);
                self.push_value(*dst);
            }
            MirInstr::ReadSecurity {
                dst,
                selector,
                source,
                htf_seconds,
                timeframe_expr,
                calendar_months,
                gaps_on,
                lookahead_on,
                ..
            } => {
                self.emit(
                    CsilInstr::ReadSecurity {
                        selector: selector.clone(),
                        source: *source,
                        htf_seconds: *htf_seconds,
                        timeframe_expr: timeframe_expr.clone(),
                        calendar_months: *calendar_months,
                        gaps_on: *gaps_on,
                        lookahead_on: *lookahead_on,
                    },
                    span,
                );
                self.push_value(*dst);
            }
            MirInstr::ReadSecurityExpr {
                dst,
                expr,
                htf_seconds,
                timeframe_expr,
                calendar_months,
                gaps_on,
                lookahead_on,
                ..
            } => {
                self.emit(
                    CsilInstr::ReadSecurityExpr {
                        expr: expr.clone(),
                        htf_seconds: *htf_seconds,
                        timeframe_expr: timeframe_expr.clone(),
                        calendar_months: *calendar_months,
                        gaps_on: *gaps_on,
                        lookahead_on: *lookahead_on,
                    },
                    span,
                );
                self.push_value(*dst);
            }
            MirInstr::ReadRequest {
                dst,
                func,
                selector,
                ..
            } => {
                self.emit(
                    CsilInstr::ReadRequest {
                        func: *func,
                        selector: selector.clone(),
                    },
                    span,
                );
                self.push_value(*dst);
            }
            MirInstr::BuildFootprint { dst, selector, .. } => {
                self.emit(
                    CsilInstr::BuildFootprint {
                        selector: selector.clone(),
                    },
                    span,
                );
                self.push_value(*dst);
            }
            MirInstr::ReadSecurityLowerTf { dst, selector, .. } => {
                self.emit(
                    CsilInstr::ReadSecurityLowerTf {
                        selector: selector.clone(),
                    },
                    span,
                );
                self.push_value(*dst);
            }
            MirInstr::ReadPivotLevels {
                dst,
                system,
                anchor_seconds,
                ..
            } => {
                self.emit(
                    CsilInstr::ReadPivotLevels {
                        system: *system,
                        anchor_seconds: *anchor_seconds,
                    },
                    span,
                );
                self.push_value(*dst);
            }
            MirInstr::CallPivotPointLevels { dst, args, .. } => {
                self.pop_values_rev(args, span)?;
                self.emit(
                    CsilInstr::CallPivotPointLevels {
                        argc: args.len() as u8,
                    },
                    span,
                );
                self.push_value(*dst);
            }
            MirInstr::ReadTimeTf {
                dst,
                close,
                tf_seconds,
                ..
            } => {
                self.emit(
                    CsilInstr::ReadTimeTf {
                        close: *close,
                        tf_seconds: *tf_seconds,
                    },
                    span,
                );
                self.push_value(*dst);
            }
            MirInstr::StoreUserSeriesCurrent { slot, value, .. } => {
                self.pop_value(*value, span)?;
                self.emit(CsilInstr::StoreUserSlotCurrent { slot: slot.0 }, span);
            }
            MirInstr::Unary {
                dst,
                op,
                operand,
                is_int,
                ..
            } => {
                self.pop_value(*operand, span)?;
                let csil = match (op, is_int) {
                    (UnOp::Neg, true) => CsilInstr::IntNeg,
                    (UnOp::Neg, false) => CsilInstr::F64Neg,
                    (UnOp::Not, _) => CsilInstr::BoolNot,
                };
                self.emit(csil, span);
                self.push_value(*dst);
            }
            MirInstr::LazyBoolGuard { op, lhs, .. } => {
                // `DUP` the lhs for the test; the retained copy is the
                // expression's result on the jump path. The jump lands just
                // past the combining `BOOL_AND`/`BOOL_OR` — patched when the
                // downstream `Binary` sharing the lhs register is reached.
                let on = match op {
                    BinOp::Or => true,
                    BinOp::And => false,
                    other => return Err(LowerError::MalformedLazyGuard(span, *other)),
                };
                self.pop_value(*lhs, span)?;
                self.push_value(*lhs);
                self.emit(CsilInstr::Dup, span);
                self.push_value(*lhs);
                let index = self.code.len() as u32;
                self.emit(
                    CsilInstr::JumpIfExact {
                        on,
                        target: u32::MAX,
                    },
                    span,
                );
                self.pop_value(*lhs, span)?;
                self.pending_guards.push((index, *lhs));
            }
            MirInstr::Binary {
                dst,
                op,
                lhs,
                rhs,
                is_int,
                is_string,
                ..
            } => {
                self.pop_value(*rhs, span)?;
                self.pop_value(*lhs, span)?;
                // String `+` concatenation has its own opcode; it never
                // coincides with the int flag.
                if matches!(op, BinOp::Add) && *is_string {
                    self.emit(CsilInstr::StrConcat, span);
                    self.push_value(*dst);
                    return Ok(());
                }
                let csil = match (op, is_int) {
                    (BinOp::Add, true) => CsilInstr::IntAdd,
                    (BinOp::Add, false) => CsilInstr::F64Add,
                    (BinOp::Sub, true) => CsilInstr::IntSub,
                    (BinOp::Sub, false) => CsilInstr::F64Sub,
                    (BinOp::Mul, true) => CsilInstr::IntMul,
                    (BinOp::Mul, false) => CsilInstr::F64Mul,
                    (BinOp::Div, true) => CsilInstr::PineIntDiv,
                    (BinOp::Div, false) => CsilInstr::F64Div,
                    (BinOp::Rem, true) => CsilInstr::IntRem,
                    (BinOp::Rem, false) => CsilInstr::F64Rem,
                    (BinOp::Gt, _) => CsilInstr::CmpGt,
                    (BinOp::Ge, _) => CsilInstr::CmpGe,
                    (BinOp::Lt, _) => CsilInstr::CmpLt,
                    (BinOp::Le, _) => CsilInstr::CmpLe,
                    (BinOp::Eq, _) => CsilInstr::CmpEq,
                    (BinOp::Ne, _) => CsilInstr::CmpNe,
                    (BinOp::And, _) => CsilInstr::BoolAnd,
                    (BinOp::Or, _) => CsilInstr::BoolOr,
                };
                self.emit(csil, span);
                self.push_value(*dst);
                // Close the lazy guard opened over this combine's lhs, if
                // any: its skip jump lands here, just past the combine. An
                // unguarded `and`/`or` (a serialized program with no guard) simply
                // stays eager.
                if matches!(op, BinOp::And | BinOp::Or) {
                    if let Some(index) = self.take_guard(*lhs) {
                        let landing = self.code.len() as u32;
                        match &mut self.code[index as usize] {
                            CsilInstr::JumpIfExact { target, .. } => *target = landing,
                            // Every pending guard indexes the `JumpIfExact`
                            // emitted when it was opened, and emitted code is
                            // never rewritten during stackification. No
                            // message: a `&str` literal leaves the subset.
                            _ => unreachable!(),
                        }
                    }
                }
            }
            MirInstr::IntToFloat { dst, operand, .. } => {
                self.pop_value(*operand, span)?;
                self.emit(CsilInstr::IntToF64, span);
                self.push_value(*dst);
            }
            MirInstr::Select {
                dst,
                cond,
                lhs,
                rhs,
                ..
            } => {
                // Pushed cond, lhs, rhs in evaluation order; SELECT pops
                // them in reverse.
                self.pop_value(*rhs, span)?;
                self.pop_value(*lhs, span)?;
                self.pop_value(*cond, span)?;
                self.emit(CsilInstr::Select, span);
                self.push_value(*dst);
            }
            MirInstr::WindowMean {
                dst, slot, length, ..
            } => {
                self.emit(
                    CsilInstr::WindowMean {
                        slot: slot.0,
                        length: *length,
                    },
                    span,
                );
                self.push_value(*dst);
            }
            MirInstr::WindowMeanDynamic {
                dst, slot, length, ..
            } => {
                self.pop_value(*length, span)?;
                self.emit(CsilInstr::WindowMeanDynamic { slot: slot.0 }, span);
                self.push_value(*dst);
            }
            MirInstr::WindowExtremumDynamic {
                dst,
                slot,
                length,
                highest,
                ..
            } => {
                self.pop_value(*length, span)?;
                self.emit(
                    CsilInstr::WindowExtremumDynamic {
                        slot: slot.0,
                        highest: *highest,
                    },
                    span,
                );
                self.push_value(*dst);
            }
            MirInstr::EmaSimpleDynamic {
                dst,
                slot,
                source,
                length,
                ..
            } => {
                self.pop_value(*length, span)?;
                self.pop_value(*source, span)?;
                self.emit(CsilInstr::EmaSimpleDynamic { slot: slot.0 }, span);
                self.push_value(*dst);
            }
            MirInstr::CallPure {
                dst, func, args, ..
            } => {
                // Arguments were pushed in order; the stack pops them in
                // reverse (last argument on top).
                self.pop_values_rev(args, span)?;
                self.emit(
                    CsilInstr::CallPure {
                        func: *func,
                        argc: args.len() as u8,
                    },
                    span,
                );
                self.push_value(*dst);
            }
            MirInstr::CallCollection {
                dst, func, args, ..
            } => {
                // Arguments were pushed in order (receiver first); the stack
                // pops them in reverse (last argument on top).
                self.pop_values_rev(args, span)?;
                self.emit(
                    CsilInstr::CallCollection {
                        func: *func,
                        argc: args.len() as u8,
                    },
                    span,
                );
                self.push_value(*dst);
            }
            MirInstr::CallTa {
                dsts, state, args, ..
            } => {
                // Runtime source operands were pushed in order; the stack
                // pops them in reverse (last operand on top).
                self.pop_values_rev(args, span)?;
                self.emit(CsilInstr::CallTa { state: *state }, span);
                // The call pushes `dsts.len()` results left to right (scalar
                // functions push one; tuple functions push their whole tuple).
                let mut index = 0;
                while index < dsts.len() {
                    self.push_value(dsts[index]);
                    index += 1;
                }
            }
            MirInstr::NewObject {
                dst,
                type_id,
                fields,
                ..
            } => {
                // Field values were pushed in order; pop in reverse (last field
                // on top).
                self.pop_values_rev(fields, span)?;
                self.emit(
                    CsilInstr::NewObject {
                        type_id: *type_id,
                        field_count: fields.len() as u32,
                    },
                    span,
                );
                self.push_value(*dst);
            }
            MirInstr::CopyObject {
                dst, type_id, obj, ..
            } => {
                self.pop_value(*obj, span)?;
                self.emit(CsilInstr::CopyObject { type_id: *type_id }, span);
                self.push_value(*dst);
            }
            MirInstr::GetField {
                dst,
                obj,
                field_idx,
                ..
            } => {
                self.pop_value(*obj, span)?;
                self.emit(
                    CsilInstr::GetField {
                        field_idx: *field_idx,
                    },
                    span,
                );
                self.push_value(*dst);
            }
            MirInstr::FootprintRowByPrice {
                dst,
                footprint,
                price,
                ..
            } => {
                self.pop_value(*price, span)?;
                self.pop_value(*footprint, span)?;
                self.emit(CsilInstr::FootprintRowByPrice, span);
                self.push_value(*dst);
            }
            MirInstr::SetField {
                obj,
                field_idx,
                value,
                ..
            } => {
                // The object handle was pushed first, then the value; pop the
                // value (on top) then the handle.
                self.pop_value(*value, span)?;
                self.pop_value(*obj, span)?;
                self.emit(
                    CsilInstr::SetField {
                        field_idx: *field_idx,
                    },
                    span,
                );
            }
            MirInstr::WindowStatisticDynamic {
                dst,
                slot,
                length,
                func,
                span,
            } => {
                self.pop_value(*length, *span)?;
                self.emit(
                    CsilInstr::WindowStatisticDynamic {
                        slot: slot.0,
                        func: *func,
                    },
                    *span,
                );
                self.push_value(*dst);
            }
            MirInstr::RegisterDrawing { dst, kind, obj, .. } => {
                self.pop_value(*obj, span)?;
                self.emit(CsilInstr::RegisterDrawing { kind: *kind }, span);
                self.push_value(*dst);
            }
            MirInstr::ReadDrawingsAll { dst, kind, .. } => {
                self.emit(CsilInstr::ReadDrawingsAll { kind: *kind }, span);
                self.push_value(*dst);
            }
            MirInstr::DeleteDrawing { dst, kind, id, .. } => {
                self.pop_value(*id, span)?;
                self.emit(CsilInstr::DeleteDrawing { kind: *kind }, span);
                // Push a placeholder `na` so the void mutator has a value for
                // the enclosing `Discard` to pop (uniform stack discipline).
                self.emit(CsilInstr::PushNa, span);
                self.push_value(*dst);
            }
            MirInstr::SetCell {
                dst,
                obj,
                column,
                row,
                attrs,
                values,
                ..
            } => {
                // Pushed obj, column, row, then each attribute value in order;
                // pop values (last on top) in reverse, then row, column, obj.
                self.pop_values_rev(values, span)?;
                self.pop_value(*row, span)?;
                self.pop_value(*column, span)?;
                self.pop_value(*obj, span)?;
                self.emit(
                    CsilInstr::SetCell {
                        attrs: attrs.clone(),
                    },
                    span,
                );
                self.emit(CsilInstr::PushNa, span);
                self.push_value(*dst);
            }
            MirInstr::ClearCells {
                dst,
                obj,
                start_column,
                start_row,
                end_column,
                end_row,
                ..
            } => {
                self.pop_value(*end_row, span)?;
                self.pop_value(*end_column, span)?;
                self.pop_value(*start_row, span)?;
                self.pop_value(*start_column, span)?;
                self.pop_value(*obj, span)?;
                self.emit(CsilInstr::ClearCells, span);
                self.emit(CsilInstr::PushNa, span);
                self.push_value(*dst);
            }
            MirInstr::MergeCells {
                dst,
                obj,
                start_column,
                start_row,
                end_column,
                end_row,
                ..
            } => {
                self.pop_value(*end_row, span)?;
                self.pop_value(*end_column, span)?;
                self.pop_value(*start_row, span)?;
                self.pop_value(*start_column, span)?;
                self.pop_value(*obj, span)?;
                self.emit(CsilInstr::MergeCells, span);
                self.emit(CsilInstr::PushNa, span);
                self.push_value(*dst);
            }
            MirInstr::Plot {
                plot_slot,
                value,
                colors,
                ..
            } => {
                self.pop_colors_rev(colors, span)?;
                self.pop_value(*value, span)?;
                let instr = if colors.is_empty() {
                    CsilInstr::PlotF64 {
                        plot_slot: plot_slot.0,
                    }
                } else {
                    CsilInstr::PlotStyledF64 {
                        plot_slot: plot_slot.0,
                        color_count: colors.len() as u8,
                    }
                };
                self.emit(instr, span);
            }
            MirInstr::Visual {
                visual_slot,
                colors,
                ..
            } => {
                self.pop_colors_rev(colors, span)?;
                self.emit(
                    CsilInstr::VisualColor {
                        visual_slot: visual_slot.0,
                        color_count: colors.len() as u8,
                    },
                    span,
                );
            }
            MirInstr::Fill {
                fill_slot, colors, ..
            } => {
                self.pop_colors_rev(colors, span)?;
                self.emit(
                    CsilInstr::FillColor {
                        fill_slot: fill_slot.0,
                        color_count: colors.len() as u8,
                    },
                    span,
                );
            }
            MirInstr::RuntimeError { message, .. } => {
                self.pop_value(*message, span)?;
                self.emit(CsilInstr::RuntimeError, span);
            }
            MirInstr::Log {
                level, operands, ..
            } => {
                // Operands (format string, then substitution args) were pushed
                // in order; pop in reverse (last arg on top).
                self.pop_values_rev(operands, span)?;
                self.emit(
                    CsilInstr::Log {
                        level: *level,
                        argc: operands.len() as u8,
                    },
                    span,
                );
            }
            MirInstr::Alert { message, freq, .. } => {
                // Pushed message then freq; pop freq (top) then message.
                self.pop_value(*freq, span)?;
                self.pop_value(*message, span)?;
                self.emit(CsilInstr::Alert, span);
            }
            MirInstr::AlertCondition {
                condition,
                title,
                message,
                ..
            } => {
                // Pushed condition, title, message; pop message, title, cond.
                self.pop_value(*message, span)?;
                self.pop_value(*title, span)?;
                self.pop_value(*condition, span)?;
                self.emit(CsilInstr::AlertCondition, span);
            }
            MirInstr::Order { kind, operands, .. } => {
                // Operands were pushed in slot order; pop in reverse (last slot
                // on top) so the VM sees a clean stack after the op.
                self.pop_values_rev(operands, span)?;
                self.emit(CsilInstr::Order { kind: *kind }, span);
            }
            MirInstr::Discard { value, .. } => {
                self.pop_value(*value, span)?;
                self.emit(CsilInstr::Pop, span);
            }
        }
        Ok(())
    }

    fn lower_terminator(&mut self, terminator: &Terminator) -> Result<(), LowerError> {
        match terminator {
            Terminator::Return { span } => {
                self.check_return_balanced()?;
                self.emit(CsilInstr::Return, *span);
            }
            Terminator::Jump { target, span } => {
                let index = self.code.len() as u32;
                self.emit(CsilInstr::Jump { target: u32::MAX }, *span);
                self.fixups.push((index, target.0));
            }
            Terminator::Branch {
                cond,
                then_block,
                else_block,
                span,
            } => {
                self.pop_value(*cond, *span)?;
                let jif_index = self.code.len() as u32;
                self.emit(CsilInstr::JumpIfFalse { target: u32::MAX }, *span);
                self.fixups.push((jif_index, else_block.0));
                let jump_index = self.code.len() as u32;
                self.emit(CsilInstr::Jump { target: u32::MAX }, *span);
                self.fixups.push((jump_index, then_block.0));
            }
            Terminator::BranchIfSlotWritten {
                slot,
                written_block,
                unwritten_block,
                span,
            } => {
                let jsw_index = self.code.len() as u32;
                self.emit(
                    CsilInstr::JumpIfSlotWritten {
                        slot: slot.0,
                        target: u32::MAX,
                    },
                    *span,
                );
                self.fixups.push((jsw_index, written_block.0));
                let jump_index = self.code.len() as u32;
                self.emit(CsilInstr::Jump { target: u32::MAX }, *span);
                self.fixups.push((jump_index, unwritten_block.0));
            }
        }
        Ok(())
    }

    /// A return is the only MIR terminator that requires an empty operand
    /// stack. Internal branches may preserve a live expression prefix as long
    /// as every predecessor delivers the same ordered registers.
    fn check_return_balanced(&self) -> Result<(), LowerError> {
        if !self.sim_stack.is_empty() {
            return Err(LowerError::UnbalancedBlock(self.sim_stack.len()));
        }
        if !self.pending_guards.is_empty() {
            return Err(LowerError::UnclosedLazyGuard(self.pending_guards.len()));
        }
        Ok(())
    }

    /// Compatibility entry point for the Aeneas extraction; a
    /// return still requires both the value stack and guard state to be empty.
    #[cfg(feature = "aeneas")]
    fn check_balanced(&self) -> Result<(), LowerError> {
        self.check_return_balanced()
    }
}

#[cfg(test)]
mod dead_constant_tests {
    use super::*;

    #[test]
    fn removed_branch_destination_is_forwarded_with_its_source_map() {
        let mut code = vec![
            CsilInstr::Jump { target: 1 },
            CsilInstr::PushStr(0),
            CsilInstr::StoreUserSlotCurrent { slot: 0 },
            CsilInstr::Return,
        ];
        let mut spans: Vec<_> = (0..4)
            .map(|index| Span::new(pinewasm_span::SourceId(3), index, index + 1))
            .collect();
        let original_spans = spans.clone();
        let slots = [crate::SlotDef {
            name: "unused".into(),
            persistent: false,
            varip: false,
            lane: None,
        }];
        elide_dead_constant_stores(&mut code, &mut spans, &slots);
        assert_eq!(code, vec![CsilInstr::Jump { target: 1 }, CsilInstr::Return]);
        assert_eq!(spans, vec![original_spans[0], original_spans[3]]);
    }

    #[test]
    fn independently_targeted_store_keeps_its_stack_producer() {
        let mut code = vec![
            CsilInstr::PushI64(7),
            CsilInstr::Jump { target: 3 },
            CsilInstr::PushI64(9),
            CsilInstr::StoreUserSlotCurrent { slot: 0 },
            CsilInstr::Return,
        ];
        let original = code.clone();
        let mut spans = vec![Span::dummy(); code.len()];
        let slots = [crate::SlotDef {
            name: "unused".into(),
            persistent: false,
            varip: false,
            lane: None,
        }];
        elide_dead_constant_stores(&mut code, &mut spans, &slots);
        assert_eq!(code, original);
    }
}

#[cfg(test)]
mod coalescing_tests {
    use super::*;

    fn slot() -> crate::SlotDef {
        crate::SlotDef {
            name: "value".into(),
            persistent: false,
            varip: false,
            lane: None,
        }
    }

    fn pair() -> Vec<CsilInstr> {
        vec![
            CsilInstr::ReadBuiltin {
                source: pinewasm_hir::BuiltinSeries::Close,
                offset: 0,
            },
            CsilInstr::StoreUserSlotCurrent { slot: 0 },
            CsilInstr::ReadUserSlot { slot: 0, offset: 0 },
            CsilInstr::PlotF64 { plot_slot: 0 },
            CsilInstr::Return,
        ]
    }

    fn optimize(code: &mut Vec<CsilInstr>, slot: crate::SlotDef) {
        let mut spans = vec![Span::dummy(); code.len()];
        coalesce_single_use_slots(code, &mut spans, &[slot]);
    }

    #[test]
    fn producer_reaches_consumer_without_slot_transfers() {
        let mut code = pair();
        optimize(&mut code, slot());
        assert_eq!(
            code,
            vec![
                CsilInstr::ReadBuiltin {
                    source: pinewasm_hir::BuiltinSeries::Close,
                    offset: 0
                },
                CsilInstr::PlotF64 { plot_slot: 0 },
                CsilInstr::Return
            ]
        );
    }

    #[test]
    fn captured_pass_matches_actual_source_lowering() {
        const SOURCE: &str = "//@version=6\nindicator(\"Sample\")\nx = close\nplot(x)\n";
        let parsed = pinewasm_parse::parse(SOURCE);
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        assert!(
            analysis.diagnostics.is_empty(),
            "{:?}",
            analysis.diagnostics
        );
        let mir = pinewasm_mir::lower(&analysis.hir);

        let ordinary = lower_mir(&mir).expect("ordinary lowering succeeds");
        let captured = lower_mir_with_pass_record(&mir).expect("captured lowering succeeds");
        assert_eq!(captured.program, ordinary);
        assert_eq!(
            captured.coalescing.source.code.len(),
            captured.coalescing.source.source_map.len()
        );
        assert_eq!(
            captured.coalescing.target.code.len(),
            captured.coalescing.target.source_map.len()
        );
        assert!(
            captured.coalescing.source.code.len() > captured.coalescing.target.code.len(),
            "fixture must exercise the existing single-use optimization"
        );

        let mut replay_code = captured.coalescing.source.code.clone();
        let mut replay_spans = captured.coalescing.source.source_map.clone();
        coalesce_single_use_slots(
            &mut replay_code,
            &mut replay_spans,
            &mir.metadata.series_slots,
        );
        assert_eq!(replay_code, captured.coalescing.target.code);
        assert_eq!(replay_spans, captured.coalescing.target.source_map);

        // The capture carries the mask the pass emitted, equal to the
        // hand-written mask asserted below.
        assert_eq!(
            captured.coalescing.keep_mask,
            single_use_slot_keep(&captured.coalescing.source.code, &mir.metadata.series_slots)
        );
        assert_eq!(
            captured.coalescing.keep_mask,
            vec![true, false, false, true, true, true]
        );
        let mut masked_code = captured.coalescing.source.code.clone();
        let mut masked_spans = captured.coalescing.source.source_map.clone();
        retain_instructions(
            &mut masked_code,
            &mut masked_spans,
            &captured.coalescing.keep_mask,
        );
        assert_eq!(masked_code, captured.coalescing.target.code);
        assert_eq!(masked_spans, captured.coalescing.target.source_map);

        let (source_program, target_program) = captured.coalescing_programs();
        assert_eq!(source_program.main().code, captured.coalescing.source.code);
        assert_eq!(target_program.main().code, captured.coalescing.target.code);
        assert_eq!(
            source_program.main().source_map,
            captured.coalescing.source.source_map
        );
        assert_eq!(
            target_program.main().source_map,
            captured.coalescing.target.source_map
        );
        assert!(crate::validate(&source_program).is_ok());
        assert!(crate::validate(&target_program).is_ok());
        assert_eq!(
            crate::CsilProgram::from_json(&source_program.to_json()).unwrap(),
            source_program
        );
        assert_eq!(
            crate::CsilProgram::from_json(&target_program.to_json()).unwrap(),
            target_program
        );
    }

    #[test]
    fn history_persistence_dynamic_reads_and_written_flags_keep_the_slot() {
        for observer in [
            CsilInstr::ReadUserSlot { slot: 0, offset: 1 },
            CsilInstr::ReadUserSlotDynamic { slot: 0 },
            CsilInstr::WindowMean { slot: 0, length: 2 },
            CsilInstr::JumpIfSlotWritten { slot: 0, target: 4 },
            CsilInstr::ReadUserSlot { slot: 0, offset: 0 },
            CsilInstr::StoreUserSlotCurrent { slot: 0 },
        ] {
            let mut code = pair();
            code.insert(4, observer);
            let original = code.clone();
            optimize(&mut code, slot());
            assert_eq!(code, original);
        }
        for (persistent, varip) in [(true, false), (true, true)] {
            let mut code = pair();
            let original = code.clone();
            optimize(
                &mut code,
                crate::SlotDef {
                    persistent,
                    varip,
                    ..slot()
                },
            );
            assert_eq!(code, original);
        }
    }

    #[test]
    fn independently_entered_read_keeps_both_transfers() {
        let mut code = pair();
        code.push(CsilInstr::Jump { target: 2 });
        let original = code.clone();
        optimize(&mut code, slot());
        assert_eq!(code, original);
    }

    #[test]
    fn target_at_store_moves_to_consumer_with_original_diagnostic_span() {
        let mut code = pair();
        code.insert(1, CsilInstr::Jump { target: 2 });
        let mut spans: Vec<_> = (0..code.len() as u32)
            .map(|i| Span::new(pinewasm_span::SourceId(1), i, i + 1))
            .collect();
        let original = spans.clone();
        coalesce_single_use_slots(&mut code, &mut spans, &[slot()]);
        assert_eq!(code[1], CsilInstr::Jump { target: 2 });
        assert_eq!(code[2], CsilInstr::PlotF64 { plot_slot: 0 });
        assert_eq!(
            spans,
            vec![original[0], original[1], original[4], original[5]]
        );
    }

    #[test]
    fn nonadjacent_branch_to_removed_store_reaches_retained_continuation() {
        let mut code = pair();
        code.insert(1, CsilInstr::Jump { target: 3 });
        code.insert(2, CsilInstr::PushF64(crate::F64Bits::new(999.0)));
        let mut spans: Vec<_> = (0..code.len() as u32)
            .map(|i| Span::new(pinewasm_span::SourceId(1), i, i + 1))
            .collect();
        let original_spans = spans.clone();
        coalesce_single_use_slots(&mut code, &mut spans, &[slot()]);
        assert_eq!(
            code,
            vec![
                CsilInstr::ReadBuiltin {
                    source: pinewasm_hir::BuiltinSeries::Close,
                    offset: 0
                },
                CsilInstr::Jump { target: 3 },
                CsilInstr::PushF64(crate::F64Bits::new(999.0)),
                CsilInstr::PlotF64 { plot_slot: 0 },
                CsilInstr::Return
            ]
        );
        assert_eq!(
            spans,
            vec![
                original_spans[0],
                original_spans[1],
                original_spans[2],
                original_spans[5],
                original_spans[6]
            ]
        );
    }

    #[test]
    fn branch_entry_full_program_export_uses_actual_coalescing_pass() {
        // The bound scalar view deliberately excludes colour instructions.
        // Keep this serialized program unchanged rather than stripping paint from
        // a current program: the fixture retains the serialized instruction
        // layout. Styled lowering is checked by the next test.
        let mut before =
            crate::CsilProgram::from_json(include_str!("fixtures/coalescing_scalar_before.json"))
                .expect("scalar fixture must decode");
        before.functions[0]
            .code
            .insert(1, CsilInstr::Jump { target: 3 });
        before.functions[0].source_map.insert(1, Span::dummy());
        before.functions[0]
            .code
            .insert(2, CsilInstr::PushF64(crate::F64Bits::new(999.0)));
        before.functions[0].source_map.insert(2, Span::dummy());
        let mut after = before.clone();
        let main = &mut after.functions[0];
        coalesce_single_use_slots(&mut main.code, &mut main.source_map, &after.series_slots);
        assert_eq!(before.functions[0].code.len(), 7);
        assert_eq!(after.functions[0].code.len(), 5);
        assert_eq!(after.functions[0].code[1], CsilInstr::Jump { target: 3 });
        assert_eq!(
            after.functions[0].code[3],
            CsilInstr::PlotF64 { plot_slot: 0 }
        );
        crate::validate(&before).expect("synthetic pre-pass branch program validates");
        crate::validate(&after).expect("actual coalescing output validates");
        let before_bound = crate::ProofBoundBranchView::from_program(&before)
            .expect("branch source should export a typed bound view");
        let after_bound = crate::ProofBoundBranchView::from_program(&after)
            .expect("branch target should export a typed bound view");
        assert_eq!(
            before_bound.branch_view.function.code[2],
            crate::ProofBranchInstr::PushFiniteF64 {
                bits: format!("{:016x}", 999.0_f64.to_bits())
            }
        );
        assert_eq!(
            before_bound.branch_view.function.code[2],
            after_bound.branch_view.function.code[2]
        );
    }

    #[test]
    fn styled_branch_entry_uses_actual_coalescing_without_claiming_a_scalar_proof() {
        const SOURCE: &str = "//@version=6\nindicator(\"Sample\")\nx = close\nplot(x)\n";
        let parsed = pinewasm_parse::parse(SOURCE);
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        assert!(
            analysis.diagnostics.is_empty(),
            "{:?}",
            analysis.diagnostics
        );
        let mir = pinewasm_mir::lower(&analysis.hir);
        let captured = lower_mir_with_pass_record(&mir).expect("capture should lower");
        let (mut before, _) = captured.coalescing_programs();
        before.functions[0]
            .code
            .insert(1, CsilInstr::Jump { target: 3 });
        before.functions[0].source_map.insert(1, Span::dummy());
        before.functions[0]
            .code
            .insert(2, CsilInstr::PushF64(crate::F64Bits::new(999.0)));
        before.functions[0].source_map.insert(2, Span::dummy());
        let mut after = before.clone();
        let main = &mut after.functions[0];
        coalesce_single_use_slots(&mut main.code, &mut main.source_map, &after.series_slots);
        assert_eq!(before.functions[0].code.len(), 8);
        assert_eq!(after.functions[0].code.len(), 6);
        assert_eq!(after.functions[0].code[1], CsilInstr::Jump { target: 3 });
        assert_eq!(
            after.functions[0].code[3],
            CsilInstr::PushColor {
                r: 33,
                g: 150,
                b: 243,
                t: crate::F64Bits::new(0.0)
            }
        );
        assert_eq!(
            after.functions[0].code[4],
            CsilInstr::PlotStyledF64 {
                plot_slot: 0,
                color_count: 1
            }
        );
        crate::validate(&before).expect("styled pre-pass branch program validates");
        crate::validate(&after).expect("styled coalescing output validates");
        assert!(crate::ProofBoundBranchView::from_program(&before).is_err());
        assert!(crate::ProofBoundBranchView::from_program(&after).is_err());
    }
}

#[cfg(test)]
mod alias_tests {
    use super::*;

    fn fixture() -> (Vec<CsilInstr>, Vec<crate::SlotDef>) {
        let code = vec![
            CsilInstr::ReadBuiltin {
                source: pinewasm_hir::BuiltinSeries::Close,
                offset: 0,
            },
            CsilInstr::StoreUserSlotCurrent { slot: 0 },
            CsilInstr::ReadUserSlot { slot: 0, offset: 0 },
            CsilInstr::StoreUserSlotCurrent { slot: 1 },
            CsilInstr::PushF64(F64Bits::new(2.0)),
            CsilInstr::ReadUserSlot { slot: 1, offset: 0 },
            CsilInstr::F64Mul,
            CsilInstr::PlotF64 { plot_slot: 0 },
            CsilInstr::Return,
        ];
        let slots = (0..3)
            .map(|i| crate::SlotDef {
                name: format!("v{i}"),
                persistent: false,
                varip: false,
                lane: None,
            })
            .collect();
        (code, slots)
    }

    fn optimize(code: &mut Vec<CsilInstr>, slots: &[crate::SlotDef]) {
        let mut spans = vec![Span::dummy(); code.len()];
        coalesce_entry_aliases(code, &mut spans, slots);
    }

    #[test]
    fn immutable_copy_uses_the_source_slot_directly() {
        let (mut code, slots) = fixture();
        let original = code.clone();
        optimize(&mut code, &slots);
        assert_eq!(code.len(), original.len() - 2);
        assert_eq!(code[1], CsilInstr::StoreUserSlotCurrent { slot: 0 });
        assert_eq!(code[2], CsilInstr::PushF64(F64Bits::new(2.0)));
        assert_eq!(code[3], CsilInstr::ReadUserSlot { slot: 0, offset: 0 });
    }

    #[test]
    fn alias_chain_is_resolved_before_code_is_rewritten() {
        let (mut code, slots) = fixture();
        code.splice(
            4..4,
            [
                CsilInstr::ReadUserSlot { slot: 1, offset: 0 },
                CsilInstr::StoreUserSlotCurrent { slot: 2 },
            ],
        );
        code[7] = CsilInstr::ReadUserSlot { slot: 2, offset: 0 };
        optimize(&mut code, &slots);
        assert_eq!(code.len(), 7);
        assert_eq!(code[3], CsilInstr::ReadUserSlot { slot: 0, offset: 0 });
    }

    #[test]
    fn redefinitions_and_observers_prevent_aliasing() {
        for extra in [
            CsilInstr::StoreUserSlotCurrent { slot: 0 },
            CsilInstr::StoreUserSlotCurrent { slot: 1 },
            CsilInstr::ReadUserSlot { slot: 1, offset: 1 },
            CsilInstr::ReadUserSlotDynamic { slot: 1 },
            CsilInstr::WindowMean { slot: 1, length: 2 },
            CsilInstr::JumpIfSlotWritten { slot: 1, target: 8 },
            CsilInstr::Jump { target: 3 },
        ] {
            let (mut code, slots) = fixture();
            code.insert(8, extra);
            let original = code.clone();
            optimize(&mut code, &slots);
            assert_eq!(code, original);
        }
        for (persistent, varip) in [(true, false), (true, true)] {
            let (mut code, mut slots) = fixture();
            slots[1].persistent = persistent;
            slots[1].varip = varip;
            let original = code.clone();
            optimize(&mut code, &slots);
            assert_eq!(code, original);
        }
    }

    #[test]
    fn copies_after_a_branch_and_reads_before_a_definition_stay_materialized() {
        for (position, instruction) in [
            (2, CsilInstr::Jump { target: 3 }),
            (2, CsilInstr::ReadUserSlot { slot: 1, offset: 0 }),
        ] {
            let (mut code, slots) = fixture();
            code.insert(position, instruction);
            let original = code.clone();
            optimize(&mut code, &slots);
            assert_eq!(code, original);
        }
    }
}

/// Differential tests pinning the Aeneas-extractable `retain_instructions`
/// to a `Vec::retain` form kept below as the reference, including which
/// inputs panic.
#[cfg(test)]
mod retain_reference_tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    /// The `Vec::retain` implementation, kept as the reference.
    fn retain_instructions_reference(
        code: &mut Vec<CsilInstr>,
        spans: &mut Vec<Span>,
        keep: &[bool],
    ) {
        if keep.iter().all(|keep| *keep) {
            return;
        }
        let mut addresses = Vec::with_capacity(code.len() + 1);
        let mut next = 0u32;
        for keep in keep {
            addresses.push(next);
            next += u32::from(*keep);
        }
        addresses.push(next);
        for instruction in code.iter_mut() {
            match instruction {
                CsilInstr::Jump { target }
                | CsilInstr::JumpIfFalse { target }
                | CsilInstr::JumpIfExact { target, .. }
                | CsilInstr::JumpIfSlotWritten { target, .. } => {
                    *target = addresses[*target as usize];
                }
                _ => {}
            }
        }
        let mut index = 0;
        code.retain(|_| {
            let retain = keep[index];
            index += 1;
            retain
        });
        let mut index = 0;
        spans.retain(|_| {
            let retain = keep[index];
            index += 1;
            retain
        });
    }

    type Outcome = Option<(Vec<CsilInstr>, Vec<Span>)>;

    fn run(
        f: fn(&mut Vec<CsilInstr>, &mut Vec<Span>, &[bool]),
        code: &[CsilInstr],
        spans: &[Span],
        keep: &[bool],
    ) -> Outcome {
        let mut code = code.to_vec();
        let mut spans = spans.to_vec();
        catch_unwind(AssertUnwindSafe(|| f(&mut code, &mut spans, keep)))
            .ok()
            .map(|()| (code, spans))
    }

    fn assert_same(code: &[CsilInstr], spans: &[Span], keep: &[bool]) -> Outcome {
        let expected = run(retain_instructions_reference, code, spans, keep);
        let actual = run(retain_instructions, code, spans, keep);
        assert_eq!(actual, expected, "code={code:?} keep={keep:?}");
        actual
    }

    fn spans_for(len: usize) -> Vec<Span> {
        (0..len as u32)
            .map(|index| Span::new(pinewasm_span::SourceId(7), index, index + 1))
            .collect()
    }

    fn jumps(target: u32) -> [CsilInstr; 4] {
        [
            CsilInstr::Jump { target },
            CsilInstr::JumpIfFalse { target },
            CsilInstr::JumpIfExact { on: true, target },
            CsilInstr::JumpIfSlotWritten { slot: 3, target },
        ]
    }

    #[test]
    fn empty_and_all_kept_inputs_are_untouched() {
        assert_eq!(assert_same(&[], &[], &[]), Some((vec![], vec![])));
        // An all-true mask returns before any target is inspected, so even an
        // out-of-range target survives unchanged in both forms.
        let code = vec![CsilInstr::Jump { target: 99 }, CsilInstr::Return];
        let spans = spans_for(2);
        assert_eq!(
            assert_same(&code, &spans, &[true, true]),
            Some((code.clone(), spans.clone()))
        );
        // A mask longer than the code is still "all kept".
        assert_same(&code, &spans, &[true, true, true]);
    }

    #[test]
    fn every_jump_opcode_is_forwarded_past_removed_instructions() {
        for (jump, forwarded) in jumps(2).into_iter().zip(jumps(1)) {
            let code = vec![
                jump,
                CsilInstr::PushF64(F64Bits::new(1.5)),
                CsilInstr::Pop,
                CsilInstr::Return,
            ];
            let spans = spans_for(code.len());
            let (kept, kept_spans) =
                assert_same(&code, &spans, &[true, false, false, true]).expect("no panic");
            assert_eq!(kept.len(), 2);
            assert_eq!(kept_spans, vec![spans[0], spans[3]]);
            // Target 2 was removed: it forwards to the next survivor (index 1).
            assert_eq!(kept[0], forwarded);
        }
    }

    #[test]
    fn nothing_kept_and_end_targets() {
        let code: Vec<_> = jumps(4).into_iter().collect();
        let spans = spans_for(4);
        assert_eq!(
            assert_same(&code, &spans, &[false; 4]),
            Some((vec![], vec![]))
        );
        // A target equal to the mask length maps to the new end.
        let (kept, _) = assert_same(&code, &spans, &[true, false, true, true]).expect("no panic");
        assert_eq!(kept.len(), 3);
    }

    #[test]
    fn panicking_inputs_panic_in_both_forms() {
        // Target past the address table.
        let code = vec![CsilInstr::Jump { target: 5 }, CsilInstr::Return];
        assert_eq!(assert_same(&code, &spans_for(2), &[true, false]), None);
        // A removed instruction with a bad target still panics (targets are
        // rewritten before removal).
        let code = vec![CsilInstr::Return, CsilInstr::JumpIfFalse { target: 9 }];
        assert_eq!(assert_same(&code, &spans_for(2), &[true, false]), None);
        // Mask shorter than the code.
        let code = vec![CsilInstr::Pop, CsilInstr::Pop, CsilInstr::Return];
        assert_eq!(assert_same(&code, &spans_for(3), &[false, true]), None);
        // Mask shorter than the spans only.
        let code = vec![CsilInstr::Pop];
        assert_eq!(assert_same(&code, &spans_for(3), &[false, true]), None);
    }

    /// Small deterministic xorshift generator; no external dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }
    }

    fn instruction(rng: &mut Rng, target_bound: u64) -> CsilInstr {
        let target = rng.below(target_bound) as u32;
        match rng.below(12) {
            0 => CsilInstr::Jump { target },
            1 => CsilInstr::JumpIfFalse { target },
            2 => CsilInstr::JumpIfExact {
                on: rng.below(2) == 0,
                target,
            },
            3 => CsilInstr::JumpIfSlotWritten {
                slot: rng.below(4) as u32,
                target,
            },
            4 => CsilInstr::PushF64(F64Bits::new(rng.next() as f64 / 7.0)),
            5 => CsilInstr::PushI64(rng.next() as i64),
            6 => CsilInstr::StoreUserSlotCurrent {
                slot: rng.below(4) as u32,
            },
            7 => CsilInstr::ReadUserSlot {
                slot: rng.below(4) as u32,
                offset: rng.below(3) as u32,
            },
            8 => CsilInstr::ReadRequest {
                func: pinewasm_hir::RequestFunc::Seed,
                selector: format!("sel{}", rng.below(5)),
            },
            9 => CsilInstr::SetCell { attrs: vec![] },
            10 => CsilInstr::Pop,
            _ => CsilInstr::Return,
        }
    }

    #[test]
    fn deterministic_sweep_matches_reference() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut changed = 0;
        for _ in 0..20_000 {
            let len = rng.below(12) as usize;
            let extra = if rng.below(4) == 0 {
                rng.below(3) as usize
            } else {
                0
            };
            let keep_len = len + extra;
            let code: Vec<_> = (0..len)
                .map(|_| instruction(&mut rng, keep_len as u64 + 1))
                .collect();
            let spans = spans_for(len);
            let density = rng.below(4);
            let keep: Vec<bool> = (0..keep_len).map(|_| rng.below(4) >= density).collect();
            let outcome = assert_same(&code, &spans, &keep).expect("in-range inputs never panic");
            if outcome.0 != code {
                changed += 1;
            }
        }
        assert!(
            changed > 1_000,
            "sweep exercised too few removals: {changed}"
        );
    }
}

/// Differential tests pinning the Aeneas-extractable passes and pipeline to
/// the iterator-based forms kept below as the reference (they call the current
/// `retain_instructions`, which `retain_reference_tests` pins separately),
/// including which inputs panic.
#[cfg(test)]
pub(crate) mod pass_reference_tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    // Iterator-based passes, kept as the reference; their names end in `_reference`.

    /// Drop literal-to-slot transfers whose destination is never observed. Input
    /// group labels commonly survive semantic analysis as unused runtime strings;
    /// their UI metadata is already resolved. Keep the metadata, constant pool,
    /// and slot numbering stable, and leave every nonliteral producer untouched.
    fn elide_dead_constant_stores_reference(
        code: &mut Vec<CsilInstr>,
        spans: &mut Vec<Span>,
        slots: &[crate::SlotDef],
    ) {
        let mut observed: Vec<bool> = slots
            .iter()
            .map(|slot| slot.persistent || slot.varip)
            .collect();
        let mut targets = vec![false; code.len()];
        for instruction in code.iter() {
            match instruction {
                CsilInstr::ReadUserSlot { slot, .. }
                | CsilInstr::ReadUserSlotDynamic { slot }
                | CsilInstr::WindowMean { slot, .. }
                | CsilInstr::WindowMeanDynamic { slot }
                | CsilInstr::WindowExtremumDynamic { slot, .. }
                | CsilInstr::WindowStatisticDynamic { slot, .. }
                | CsilInstr::JumpIfSlotWritten { slot, .. } => observed[*slot as usize] = true,
                _ => {}
            }
            match instruction {
                CsilInstr::Jump { target }
                | CsilInstr::JumpIfFalse { target }
                | CsilInstr::JumpIfExact { target, .. }
                | CsilInstr::JumpIfSlotWritten { target, .. } => {
                    if let Some(target) = targets.get_mut(*target as usize) {
                        *target = true;
                    }
                }
                _ => {}
            }
        }
        let mut keep = vec![true; code.len()];
        for pc in 1..code.len() {
            let CsilInstr::StoreUserSlotCurrent { slot } = code[pc] else {
                continue;
            };
            if observed[slot as usize] || targets[pc] {
                continue;
            }
            if matches!(
                code[pc - 1],
                CsilInstr::PushNa
                    | CsilInstr::PushBool(_)
                    | CsilInstr::PushI64(_)
                    | CsilInstr::PushF64(_)
                    | CsilInstr::PushStr(_)
                    | CsilInstr::PushColor { .. }
            ) {
                keep[pc - 1] = false;
                keep[pc] = false;
            }
        }
        retain_instructions(code, spans, &keep);
    }

    /// Keep a produced value on the operand stack when its sole use immediately
    /// reloads the destination. These two transfers add no information: the
    /// producer's stack value and the slot's value are equivalent. Restrict this
    /// to one-definition, one-use slots with no history or persistence observer.
    /// No runtime plan, type check, extra stack cell, or bytecode opcode is needed.
    fn coalesce_single_use_slots_reference(
        code: &mut Vec<CsilInstr>,
        spans: &mut Vec<Span>,
        slots: &[crate::SlotDef],
    ) {
        let mut reads = vec![0usize; slots.len()];
        let mut writes = vec![0usize; slots.len()];
        let mut observed: Vec<bool> = slots
            .iter()
            .map(|slot| slot.persistent || slot.varip)
            .collect();
        let mut targets = vec![false; code.len()];
        for instruction in code.iter() {
            match instruction {
                CsilInstr::ReadUserSlot { slot, offset } => {
                    reads[*slot as usize] += 1;
                    observed[*slot as usize] |= *offset != 0;
                }
                CsilInstr::StoreUserSlotCurrent { slot } => writes[*slot as usize] += 1,
                CsilInstr::ReadUserSlotDynamic { slot }
                | CsilInstr::WindowMean { slot, .. }
                | CsilInstr::WindowMeanDynamic { slot }
                | CsilInstr::WindowExtremumDynamic { slot, .. }
                | CsilInstr::WindowStatisticDynamic { slot, .. }
                | CsilInstr::JumpIfSlotWritten { slot, .. } => observed[*slot as usize] = true,
                _ => {}
            }
            match instruction {
                CsilInstr::Jump { target }
                | CsilInstr::JumpIfFalse { target }
                | CsilInstr::JumpIfExact { target, .. }
                | CsilInstr::JumpIfSlotWritten { target, .. } => {
                    if let Some(entry) = targets.get_mut(*target as usize) {
                        *entry = true;
                    }
                }
                _ => {}
            }
        }
        let mut keep = vec![true; code.len()];
        for pc in 0..code.len().saturating_sub(1) {
            let CsilInstr::StoreUserSlotCurrent { slot } = code[pc] else {
                continue;
            };
            if targets[pc + 1]
                || observed[slot as usize]
                || reads[slot as usize] != 1
                || writes[slot as usize] != 1
            {
                continue;
            }
            if matches!(code[pc + 1], CsilInstr::ReadUserSlot { slot: source, offset: 0 } if source == slot)
            {
                keep[pc] = false;
                keep[pc + 1] = false;
            }
        }
        retain_instructions(code, spans, &keep);
    }

    /// Coalesce immutable copies in the entry's straight-line prefix. A source
    /// defined earlier in that prefix cannot change without executing its copies
    /// again: the prefix has no intervening branch. Later reads can therefore name
    /// the source directly. Keep sources, effects, and evaluation order in place.
    fn coalesce_entry_aliases_reference(
        code: &mut Vec<CsilInstr>,
        spans: &mut Vec<Span>,
        slots: &[crate::SlotDef],
    ) {
        let mut writes = vec![0usize; slots.len()];
        let mut definition = vec![usize::MAX; slots.len()];
        let mut first_read = vec![usize::MAX; slots.len()];
        let mut observed: Vec<bool> = slots
            .iter()
            .map(|slot| slot.persistent || slot.varip)
            .collect();
        let mut targets = vec![false; code.len()];
        let mut prefix_end = code.len();
        for (pc, instruction) in code.iter().enumerate() {
            match instruction {
                CsilInstr::ReadUserSlot { slot, offset } => {
                    first_read[*slot as usize] = first_read[*slot as usize].min(pc);
                    observed[*slot as usize] |= *offset != 0;
                }
                CsilInstr::StoreUserSlotCurrent { slot } => {
                    writes[*slot as usize] += 1;
                    definition[*slot as usize] = pc;
                }
                CsilInstr::ReadUserSlotDynamic { slot }
                | CsilInstr::WindowMean { slot, .. }
                | CsilInstr::WindowMeanDynamic { slot }
                | CsilInstr::WindowExtremumDynamic { slot, .. }
                | CsilInstr::WindowStatisticDynamic { slot, .. }
                | CsilInstr::JumpIfSlotWritten { slot, .. } => observed[*slot as usize] = true,
                _ => {}
            }
            match instruction {
                CsilInstr::Jump { target }
                | CsilInstr::JumpIfFalse { target }
                | CsilInstr::JumpIfExact { target, .. }
                | CsilInstr::JumpIfSlotWritten { target, .. } => {
                    prefix_end = prefix_end.min(pc);
                    if let Some(entry) = targets.get_mut(*target as usize) {
                        *entry = true;
                    }
                }
                CsilInstr::Return => prefix_end = prefix_end.min(pc),
                _ => {}
            }
        }
        let mut aliases: Vec<u32> = (0..slots.len() as u32).collect();
        let mut keep = vec![true; code.len()];
        for pc in 1..prefix_end {
            let CsilInstr::StoreUserSlotCurrent { slot: dst } = code[pc] else {
                continue;
            };
            let CsilInstr::ReadUserSlot {
                slot: src,
                offset: 0,
            } = code[pc - 1]
            else {
                continue;
            };
            let (src_index, dst_index) = (src as usize, dst as usize);
            if targets[pc]
                || writes[src_index] != 1
                || writes[dst_index] != 1
                || definition[src_index] >= pc - 1
                || observed[dst_index]
                || first_read[dst_index] <= pc
            {
                continue;
            }
            aliases[dst_index] = aliases[src_index];
            keep[pc - 1] = false;
            keep[pc] = false;
        }
        for instruction in code.iter_mut() {
            if let CsilInstr::ReadUserSlot { slot, offset: 0 } = instruction {
                *slot = aliases[*slot as usize];
            }
        }
        retain_instructions(code, spans, &keep);
    }

    /// The reference pipeline order, as `lower_mir` applies it. The flow-alias
    /// and jump-threading passes have no reference form; they run unchanged.
    fn optimize_reference(
        code: &mut Vec<CsilInstr>,
        spans: &mut Vec<Span>,
        slots: &[crate::SlotDef],
    ) {
        elide_dead_constant_stores_reference(code, spans, slots);
        coalesce_single_use_slots_reference(code, spans, slots);
        coalesce_entry_aliases_reference(code, spans, slots);
        super::flow_alias::flow_alias_rounds(code, spans, slots);
        super::jump_thread::thread_jumps(code, spans);
    }

    /// `lower_mir` through the reference pipeline.
    pub(crate) fn lower_mir_reference(mir: &MirBody) -> Result<CsilProgram, LowerError> {
        lower_mir_with(mir, optimize_reference)
    }

    type Pass = fn(&mut Vec<CsilInstr>, &mut Vec<Span>, &[crate::SlotDef]);
    type Outcome = Option<(Vec<CsilInstr>, Vec<Span>)>;

    const PASSES: [(&str, Pass, Pass); 4] = [
        (
            "dead literal",
            elide_dead_constant_stores,
            elide_dead_constant_stores_reference,
        ),
        (
            "single use",
            coalesce_single_use_slots,
            coalesce_single_use_slots_reference,
        ),
        (
            "entry alias",
            coalesce_entry_aliases,
            coalesce_entry_aliases_reference,
        ),
        ("pipeline", optimize, optimize_reference),
    ];

    fn run(f: Pass, code: &[CsilInstr], spans: &[Span], slots: &[crate::SlotDef]) -> Outcome {
        let mut code = code.to_vec();
        let mut spans = spans.to_vec();
        catch_unwind(AssertUnwindSafe(|| f(&mut code, &mut spans, slots)))
            .ok()
            .map(|()| (code, spans))
    }

    /// Runs every pass and the pipeline in both forms and asserts equal
    /// outcomes (including panics); returns how many of the four changed the
    /// input without panicking.
    fn assert_same(code: &[CsilInstr], spans: &[Span], slots: &[crate::SlotDef]) -> usize {
        let mut changed = 0;
        for (name, new, reference) in PASSES {
            let expected = run(reference, code, spans, slots);
            let actual = run(new, code, spans, slots);
            assert_eq!(actual, expected, "{name}: code={code:?} slots={slots:?}");
            if actual.is_some_and(|(out, _)| out != code) {
                changed += 1;
            }
        }
        changed
    }

    fn spans_for(len: usize) -> Vec<Span> {
        (0..len as u32)
            .map(|index| Span::new(pinewasm_span::SourceId(5), index, index + 1))
            .collect()
    }

    fn slot_defs(flags: &[(bool, bool)]) -> Vec<crate::SlotDef> {
        flags
            .iter()
            .enumerate()
            .map(|(index, &(persistent, varip))| crate::SlotDef {
                name: format!("s{index}"),
                persistent,
                varip,
                lane: None,
            })
            .collect()
    }

    #[test]
    fn unit_fixtures_match_reference() {
        let plain = slot_defs(&[(false, false); 3]);
        let close = CsilInstr::ReadBuiltin {
            source: pinewasm_hir::BuiltinSeries::Close,
            offset: 0,
        };
        let fixtures = [
            vec![
                CsilInstr::Jump { target: 1 },
                CsilInstr::PushStr(0),
                CsilInstr::StoreUserSlotCurrent { slot: 0 },
                CsilInstr::Return,
            ],
            vec![
                close.clone(),
                CsilInstr::StoreUserSlotCurrent { slot: 0 },
                CsilInstr::ReadUserSlot { slot: 0, offset: 0 },
                CsilInstr::PlotF64 { plot_slot: 0 },
                CsilInstr::Return,
            ],
            vec![
                close,
                CsilInstr::StoreUserSlotCurrent { slot: 0 },
                CsilInstr::ReadUserSlot { slot: 0, offset: 0 },
                CsilInstr::StoreUserSlotCurrent { slot: 1 },
                CsilInstr::ReadUserSlot { slot: 1, offset: 0 },
                CsilInstr::StoreUserSlotCurrent { slot: 2 },
                CsilInstr::PushF64(F64Bits::new(2.0)),
                CsilInstr::ReadUserSlot { slot: 2, offset: 0 },
                CsilInstr::F64Mul,
                CsilInstr::PlotF64 { plot_slot: 0 },
                CsilInstr::Return,
            ],
        ];
        for code in fixtures {
            assert!(assert_same(&code, &spans_for(code.len()), &plain) > 0);
        }
        assert_eq!(assert_same(&[], &[], &[]), 0);
        assert_eq!(assert_same(&[CsilInstr::Return], &spans_for(1), &[]), 0);
    }

    #[test]
    fn out_of_range_slots_panic_in_both_forms() {
        let slots = slot_defs(&[(false, false)]);
        for code in [
            vec![CsilInstr::ReadUserSlot { slot: 1, offset: 0 }],
            vec![CsilInstr::ReadUserSlot { slot: 4, offset: 2 }],
            vec![CsilInstr::Pop, CsilInstr::StoreUserSlotCurrent { slot: 1 }],
            vec![CsilInstr::ReadUserSlotDynamic { slot: 1 }],
            vec![CsilInstr::WindowMean { slot: 9, length: 2 }],
            vec![CsilInstr::JumpIfSlotWritten { slot: 1, target: 0 }],
            vec![
                CsilInstr::PushI64(1),
                CsilInstr::StoreUserSlotCurrent { slot: 3 },
            ],
        ] {
            let spans = spans_for(code.len());
            for (name, new, reference) in PASSES {
                let expected = run(reference, &code, &spans, &slots);
                assert_eq!(expected, None, "{name}: {code:?}");
                assert_eq!(run(new, &code, &spans, &slots), None, "{name}: {code:?}");
            }
        }
    }

    /// Small deterministic xorshift generator; no external dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }
    }

    /// Instructions weighted towards the slot transfers, literals and
    /// branches the passes inspect. `slot_bound` may exceed the slot count so
    /// the panicking indexes are exercised too.
    fn instruction(rng: &mut Rng, slot_bound: u64, target_bound: u64) -> CsilInstr {
        let slot = rng.below(slot_bound) as u32;
        let target = rng.below(target_bound) as u32;
        match rng.below(22) {
            0 => CsilInstr::Jump { target },
            1 => CsilInstr::JumpIfFalse { target },
            2 => CsilInstr::JumpIfExact {
                on: rng.below(2) == 0,
                target,
            },
            3 => CsilInstr::JumpIfSlotWritten { slot, target },
            4..=6 => CsilInstr::StoreUserSlotCurrent { slot },
            7..=9 => CsilInstr::ReadUserSlot {
                slot,
                offset: u32::from(rng.below(4) == 0),
            },
            10 => CsilInstr::ReadUserSlotDynamic { slot },
            11 => CsilInstr::WindowMean { slot, length: 3 },
            12 => CsilInstr::PushNa,
            13 => CsilInstr::PushBool(rng.below(2) == 0),
            14 => CsilInstr::PushI64(rng.next() as i64),
            15 => CsilInstr::PushF64(F64Bits::new(rng.next() as f64 / 3.0)),
            16 => CsilInstr::PushStr(rng.below(4) as u32),
            17 => CsilInstr::PushColor {
                r: 1,
                g: 2,
                b: 3,
                t: crate::F64Bits::new(4.0),
            },
            18 => CsilInstr::ReadBuiltin {
                source: pinewasm_hir::BuiltinSeries::Close,
                offset: 0,
            },
            19 => CsilInstr::F64Mul,
            20 => CsilInstr::Pop,
            _ => CsilInstr::Return,
        }
    }

    #[test]
    fn deterministic_sweep_matches_reference() {
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);
        let (mut changed, mut panicked) = (0, 0);
        for _ in 0..30_000 {
            let slot_count = rng.below(5) as usize;
            let flags: Vec<_> = (0..slot_count)
                .map(|_| (rng.below(6) == 0, rng.below(12) == 0))
                .collect();
            let slots = slot_defs(&flags);
            // Mostly in-range slots; occasionally one past the end.
            let slot_bound = (slot_count as u64 + u64::from(rng.below(8) == 0)).max(1);
            let len = rng.below(14) as usize;
            let target_bound = len as u64 + 1 + u64::from(rng.below(10) == 0);
            let code: Vec<_> = (0..len)
                .map(|_| instruction(&mut rng, slot_bound, target_bound))
                .collect();
            let spans = spans_for(len);
            changed += assert_same(&code, &spans, &slots);
            if run(optimize_reference, &code, &spans, &slots).is_none() {
                panicked += 1;
            }
        }
        assert!(
            changed > 2_000,
            "sweep exercised too few rewrites: {changed}"
        );
        assert!(panicked > 100, "sweep exercised too few panics: {panicked}");
    }

    /// Every Pine program in the repository's test and benchmark fixtures
    /// that analyzes without libraries lowers to the same `CsilProgram`
    /// through the new and the reference pipelines.
    #[test]
    fn corpus_programs_lower_identically() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut files = Vec::new();
        for dir in [
            "tests/compile-pass",
            "tests/compile-fail",
            "tests/regression",
            "benchmarks/workloads",
        ] {
            for entry in std::fs::read_dir(root.join(dir)).expect("fixture directory") {
                let path = entry.expect("fixture entry").path();
                if path.extension().is_some_and(|ext| ext == "pine") {
                    files.push(path);
                }
            }
        }
        files.sort();
        let (mut compared, mut optimized) = (0, 0);
        for path in &files {
            let source = std::fs::read_to_string(path).expect("fixture file");
            let parsed = pinewasm_parse::parse(&source);
            if pinewasm_span::has_errors(&parsed.diagnostics) {
                continue;
            }
            let analysis = pinewasm_semantics::analyze(&parsed.program);
            if pinewasm_span::has_errors(&analysis.diagnostics) {
                continue;
            }
            let mir = pinewasm_mir::lower(&analysis.hir);
            let actual = lower_mir(&mir);
            assert_eq!(actual, lower_mir_reference(&mir), "{}", path.display());
            if actual != lower_mir_with(&mir, |_, _, _| {}) {
                optimized += 1;
            }
            compared += 1;
        }
        eprintln!(
            "fixtures: {compared} of {} programs compared, {optimized} changed by the passes",
            files.len()
        );
        assert!(
            compared >= 40,
            "only {compared} of {} fixture programs compiled",
            files.len()
        );
        assert!(optimized >= 10, "passes fired on only {optimized} programs");
    }
}

#[cfg(test)]
pub(crate) mod stackify_reference_tests {
    //! Differential tests for the Aeneas-subset stackification: copies of
    //! the reference `LowerError`, `LowerCtx` and `lower_mir_with` are kept
    //! below with `Reference` names, and every test
    //! compares the two forms outcome for outcome (programs, errors with
    //! their messages, and panics with their messages).

    use super::*;
    use pinewasm_hir::{
        BuiltinSeries, CellAttr, CollectionFunc, DrawKind, FillSlot, HtfExpr, LogLevel, OrderKind,
        PivotSystem, PlotSlot, PureFunc, RequestFunc, SeriesSlot, VisualSlot,
    };
    use pinewasm_mir::{BlockId, HandleId};
    use std::panic::{catch_unwind, AssertUnwindSafe};

    // ---- reference copies of the stackifier, with `Reference` names ----

    #[derive(Debug, Error, PartialEq, Eq)]
    pub(super) enum LowerErrorReference {
        #[error("stack/register mismatch at {0:?}: expected v{1}, found {2}")]
        StackMismatch(Span, u32, String),
        #[error("operand stack not empty at block boundary ({0} values left)")]
        UnbalancedBlock(usize),
        #[error("lazy boolean guard at {0:?} is malformed: {1}")]
        MalformedLazyGuard(Span, String),
        #[error("{0} lazy boolean guard(s) still open at a block boundary")]
        UnclosedLazyGuard(usize),
        /// A `MirInstr::Const` carried a runtime-only value (a collection handle).
        /// Handles are produced by collection opcodes at runtime, never as
        /// compile-time constants, so this is an internal-invariant violation, not
        /// a user error.
        #[error("cannot lower a heap reference as a constant at {0:?}")]
        NonConstValue(Span),
        /// New stackifier validation failures have no reference equivalent.
        #[error("block id b{0} occurs more than once")]
        DuplicateBlockId(u32),
        #[error("block id b{0} is missing")]
        MissingBlockId(u32),
        #[error("predecessors disagree about the operand stack at block b{0}")]
        StackMergeMismatch(u32),
        #[error("predecessors disagree about open lazy guards at block b{0}")]
        GuardMergeMismatch(u32),
    }

    fn lower_mir_with_reference(
        mir: &MirBody,
        optimize: impl FnOnce(&mut Vec<CsilInstr>, &mut Vec<Span>, &[crate::SlotDef]),
    ) -> Result<CsilProgram, LowerErrorReference> {
        let mut ctx = LowerCtxReference {
            code: Vec::new(),
            source_map: Vec::new(),
            constants: Vec::new(),
            sim_stack: Vec::new(),
            max_stack: 0,
            block_starts: vec![u32::MAX; mir.blocks.len()],
            fixups: Vec::new(),
            pending_guards: Vec::new(),
        };

        // Lay blocks out in id order (the order lowering created them, which is
        // also a topological order for the diamonds `if` produces).
        for block in &mir.blocks {
            ctx.block_starts[block.id.0 as usize] = ctx.code.len() as u32;
            for instr in &block.instrs {
                ctx.lower_instr(instr)?;
            }
            ctx.lower_terminator(&block.terminator)?;
        }

        // Patch jump targets now that every block's start offset is known.
        for (code_index, block) in ctx.fixups.clone() {
            let target = ctx.block_starts[block as usize];
            match &mut ctx.code[code_index as usize] {
                CsilInstr::Jump { target: t }
                | CsilInstr::JumpIfFalse { target: t }
                | CsilInstr::JumpIfSlotWritten { target: t, .. } => *t = target,
                other => unreachable!("fixup points at non-jump instruction {other:?}"),
            }
        }

        crate::fraction::truncate_lowered(
            &mut ctx.code,
            &mut ctx.source_map,
            &mir.metadata.ta_states,
            mir.metadata.series_slots.len(),
        );

        let reference = crate::BudgetReference {
            version: crate::BUDGET_REFERENCE_VERSION,
            code: ctx.code.clone(),
            source_map: ctx.source_map.clone(),
            max_stack: ctx.max_stack,
        };

        optimize(
            &mut ctx.code,
            &mut ctx.source_map,
            &mir.metadata.series_slots,
        );

        let mut program = CsilProgram {
            header: CsilHeader {
                runtime_abi_version: if mir.metadata.ta_input_bindings.is_empty() {
                    0
                } else {
                    crate::TA_INPUT_ABI_VERSION
                },
                ..CsilHeader::default()
            },
            constants: ctx.constants,
            series_slots: mir.metadata.series_slots.clone(),
            ta_states: mir.metadata.ta_states.clone(),
            ta_input_bindings: mir.metadata.ta_input_bindings.clone(),
            inputs: mir.metadata.inputs.clone(),
            object_types: mir.metadata.object_types.clone(),
            plots: mir
                .metadata
                .plots
                .iter()
                .map(|plot| CsilPlot {
                    title: plot.title.clone(),
                    style: plot.style.clone(),
                    color_roles: plot.color_roles.clone(),
                })
                .collect(),
            visuals: mir
                .metadata
                .visuals
                .iter()
                .map(|visual| crate::CsilVisual {
                    title: visual.title.clone(),
                    style: visual.style.clone(),
                    color_roles: visual.color_roles.clone(),
                })
                .collect(),
            fills: mir
                .metadata
                .fills
                .iter()
                .map(|fill| crate::CsilFill {
                    plot1: fill.plot1.0,
                    plot2: fill.plot2.0,
                    title: fill.title.clone(),
                    style: fill.style.clone(),
                    color_roles: fill.color_roles.clone(),
                })
                .collect(),
            functions: vec![CsilFunction {
                name: "main".to_string(),
                code: ctx.code,
                source_map: ctx.source_map,
                budget_reference: None,
                max_stack: ctx.max_stack,
            }],
            metadata: ProgramMetadata {
                rendering_contract_version: crate::RENDERING_CONTRACT_VERSION,
                indicator: mir.metadata.indicator.clone(),
                title: mir.metadata.title.clone(),
                overlay: mir.metadata.overlay,
                strategy: mir.metadata.strategy.clone(),
                drawing_caps: mir.metadata.drawing_caps,
                max_bars_back: mir.metadata.max_bars_back,
            },
        };
        if program.main().code != reference.code {
            let mut reference = reference;
            reference.version =
                super::reference_version(&reference, program.main(), &mir.metadata.series_slots);
            program.functions[0].budget_reference = Some(reference);
        }
        Ok(program)
    }

    struct LowerCtxReference {
        code: Vec<CsilInstr>,
        source_map: Vec<Span>,
        constants: Vec<String>,
        /// Which MIR register each stack cell corresponds to.
        sim_stack: Vec<ValueId>,
        max_stack: u32,
        block_starts: Vec<u32>,
        /// (code index, target block) pairs needing target patching.
        fixups: Vec<(u32, u32)>,
        /// Open [`MirInstr::LazyBoolGuard`]s: the emitted `JUMP_IF_EXACT_*`'s
        /// code index plus the lhs register it guards. Registers are single-use,
        /// so the downstream `Binary { op: And | Or }` sharing the register is
        /// the guard's unique combine; reaching it pops the entry and patches
        /// the jump to land just past the combine opcode. Guard/combine pairs
        /// never span blocks (they live inside one expression), so the list is
        /// empty at every block boundary.
        pending_guards: Vec<(u32, ValueId)>,
    }

    impl LowerCtxReference {
        fn emit(&mut self, instr: CsilInstr, span: Span) {
            self.code.push(instr);
            self.source_map.push(span);
        }

        fn push_value(&mut self, value: ValueId) {
            self.sim_stack.push(value);
            self.max_stack = self.max_stack.max(self.sim_stack.len() as u32);
        }

        fn pop_value(&mut self, expected: ValueId, span: Span) -> Result<(), LowerErrorReference> {
            match self.sim_stack.pop() {
                Some(top) if top == expected => Ok(()),
                Some(top) => Err(LowerErrorReference::StackMismatch(
                    span,
                    expected.0,
                    format!("v{}", top.0),
                )),
                None => Err(LowerErrorReference::StackMismatch(
                    span,
                    expected.0,
                    "empty stack".to_string(),
                )),
            }
        }

        fn intern_string(&mut self, s: &str) -> u32 {
            if let Some(i) = self.constants.iter().position(|c| c == s) {
                return i as u32;
            }
            self.constants.push(s.to_string());
            (self.constants.len() - 1) as u32
        }

        fn lower_instr(&mut self, instr: &MirInstr) -> Result<(), LowerErrorReference> {
            let span = instr.span();
            match instr {
                MirInstr::Const { dst, value, .. } => {
                    let csil = match value {
                        Value::Na => CsilInstr::PushNa,
                        Value::Bool(b) => CsilInstr::PushBool(*b),
                        Value::Int(v) => CsilInstr::PushI64(*v),
                        Value::Float(v) => CsilInstr::PushF64(F64Bits::new(*v)),
                        Value::String(s) => {
                            let id = self.intern_string(s);
                            CsilInstr::PushStr(id)
                        }
                        Value::Color { r, g, b, t, .. } => CsilInstr::PushColor {
                            r: *r,
                            g: *g,
                            b: *b,
                            t: crate::F64Bits::new(*t),
                        },
                        // Heap handles are never compile-time constants.
                        Value::Ref(_) | Value::DisplacedTable(_) => {
                            return Err(LowerErrorReference::NonConstValue(span))
                        }
                    };
                    self.emit(csil, span);
                    self.push_value(*dst);
                }
                MirInstr::ReadBuiltinSeries {
                    dst,
                    source,
                    offset,
                    ..
                } => {
                    self.emit(
                        CsilInstr::ReadBuiltin {
                            source: *source,
                            offset: *offset,
                        },
                        span,
                    );
                    self.push_value(*dst);
                }
                MirInstr::ReadUserSeries {
                    dst, slot, offset, ..
                } => {
                    self.emit(
                        CsilInstr::ReadUserSlot {
                            slot: slot.0,
                            offset: *offset,
                        },
                        span,
                    );
                    self.push_value(*dst);
                }
                MirInstr::ReadBuiltinSeriesDynamic {
                    dst,
                    source,
                    offset,
                    ..
                } => {
                    // The offset operand is on the stack; the dynamic read pops it
                    // and pushes the sampled value.
                    self.pop_value(*offset, span)?;
                    self.emit(CsilInstr::ReadBuiltinDynamic { source: *source }, span);
                    self.push_value(*dst);
                }
                MirInstr::ReadUserSeriesDynamic {
                    dst, slot, offset, ..
                } => {
                    self.pop_value(*offset, span)?;
                    self.emit(CsilInstr::ReadUserSlotDynamic { slot: slot.0 }, span);
                    self.push_value(*dst);
                }
                MirInstr::ReadInput { dst, input, .. } => {
                    self.emit(CsilInstr::ReadInput { input: *input }, span);
                    self.push_value(*dst);
                }
                MirInstr::ReadSecurity {
                    dst,
                    selector,
                    source,
                    htf_seconds,
                    timeframe_expr,
                    calendar_months,
                    gaps_on,
                    lookahead_on,
                    ..
                } => {
                    self.emit(
                        CsilInstr::ReadSecurity {
                            selector: selector.clone(),
                            source: *source,
                            htf_seconds: *htf_seconds,
                            timeframe_expr: timeframe_expr.clone(),
                            calendar_months: *calendar_months,
                            gaps_on: *gaps_on,
                            lookahead_on: *lookahead_on,
                        },
                        span,
                    );
                    self.push_value(*dst);
                }
                MirInstr::ReadSecurityExpr {
                    dst,
                    expr,
                    htf_seconds,
                    timeframe_expr,
                    calendar_months,
                    gaps_on,
                    lookahead_on,
                    ..
                } => {
                    self.emit(
                        CsilInstr::ReadSecurityExpr {
                            expr: expr.clone(),
                            htf_seconds: *htf_seconds,
                            timeframe_expr: timeframe_expr.clone(),
                            calendar_months: *calendar_months,
                            gaps_on: *gaps_on,
                            lookahead_on: *lookahead_on,
                        },
                        span,
                    );
                    self.push_value(*dst);
                }
                MirInstr::ReadRequest {
                    dst,
                    func,
                    selector,
                    ..
                } => {
                    self.emit(
                        CsilInstr::ReadRequest {
                            func: *func,
                            selector: selector.clone(),
                        },
                        span,
                    );
                    self.push_value(*dst);
                }
                MirInstr::BuildFootprint { dst, selector, .. } => {
                    self.emit(
                        CsilInstr::BuildFootprint {
                            selector: selector.clone(),
                        },
                        span,
                    );
                    self.push_value(*dst);
                }
                MirInstr::ReadSecurityLowerTf { dst, selector, .. } => {
                    self.emit(
                        CsilInstr::ReadSecurityLowerTf {
                            selector: selector.clone(),
                        },
                        span,
                    );
                    self.push_value(*dst);
                }
                MirInstr::ReadPivotLevels {
                    dst,
                    system,
                    anchor_seconds,
                    ..
                } => {
                    self.emit(
                        CsilInstr::ReadPivotLevels {
                            system: *system,
                            anchor_seconds: *anchor_seconds,
                        },
                        span,
                    );
                    self.push_value(*dst);
                }
                MirInstr::CallPivotPointLevels { dst, args, .. } => {
                    for arg in args.iter().rev() {
                        self.pop_value(*arg, span)?;
                    }
                    self.emit(
                        CsilInstr::CallPivotPointLevels {
                            argc: args.len() as u8,
                        },
                        span,
                    );
                    self.push_value(*dst);
                }
                MirInstr::ReadTimeTf {
                    dst,
                    close,
                    tf_seconds,
                    ..
                } => {
                    self.emit(
                        CsilInstr::ReadTimeTf {
                            close: *close,
                            tf_seconds: *tf_seconds,
                        },
                        span,
                    );
                    self.push_value(*dst);
                }
                MirInstr::StoreUserSeriesCurrent { slot, value, .. } => {
                    self.pop_value(*value, span)?;
                    self.emit(CsilInstr::StoreUserSlotCurrent { slot: slot.0 }, span);
                }
                MirInstr::Unary {
                    dst,
                    op,
                    operand,
                    is_int,
                    ..
                } => {
                    self.pop_value(*operand, span)?;
                    let csil = match (op, is_int) {
                        (UnOp::Neg, true) => CsilInstr::IntNeg,
                        (UnOp::Neg, false) => CsilInstr::F64Neg,
                        (UnOp::Not, _) => CsilInstr::BoolNot,
                    };
                    self.emit(csil, span);
                    self.push_value(*dst);
                }
                MirInstr::LazyBoolGuard { op, lhs, .. } => {
                    // `DUP` the lhs for the test; the retained copy is the
                    // expression's result on the jump path. The jump lands just
                    // past the combining `BOOL_AND`/`BOOL_OR` — patched when the
                    // downstream `Binary` sharing the lhs register is reached.
                    let on = match op {
                        BinOp::Or => true,
                        BinOp::And => false,
                        other => {
                            return Err(LowerErrorReference::MalformedLazyGuard(
                                span,
                                format!("`{}` is not a lazy boolean operator", other.symbol()),
                            ))
                        }
                    };
                    self.pop_value(*lhs, span)?;
                    self.push_value(*lhs);
                    self.emit(CsilInstr::Dup, span);
                    self.push_value(*lhs);
                    let index = self.code.len() as u32;
                    self.emit(
                        CsilInstr::JumpIfExact {
                            on,
                            target: u32::MAX,
                        },
                        span,
                    );
                    self.pop_value(*lhs, span)?;
                    self.pending_guards.push((index, *lhs));
                }
                MirInstr::Binary {
                    dst,
                    op,
                    lhs,
                    rhs,
                    is_int,
                    is_string,
                    ..
                } => {
                    self.pop_value(*rhs, span)?;
                    self.pop_value(*lhs, span)?;
                    // String `+` concatenation has its own opcode; it never
                    // coincides with the int flag.
                    if *op == BinOp::Add && *is_string {
                        self.emit(CsilInstr::StrConcat, span);
                        self.push_value(*dst);
                        return Ok(());
                    }
                    let csil = match (op, is_int) {
                        (BinOp::Add, true) => CsilInstr::IntAdd,
                        (BinOp::Add, false) => CsilInstr::F64Add,
                        (BinOp::Sub, true) => CsilInstr::IntSub,
                        (BinOp::Sub, false) => CsilInstr::F64Sub,
                        (BinOp::Mul, true) => CsilInstr::IntMul,
                        (BinOp::Mul, false) => CsilInstr::F64Mul,
                        (BinOp::Div, true) => CsilInstr::PineIntDiv,
                        (BinOp::Div, false) => CsilInstr::F64Div,
                        (BinOp::Rem, true) => CsilInstr::IntRem,
                        (BinOp::Rem, false) => CsilInstr::F64Rem,
                        (BinOp::Gt, _) => CsilInstr::CmpGt,
                        (BinOp::Ge, _) => CsilInstr::CmpGe,
                        (BinOp::Lt, _) => CsilInstr::CmpLt,
                        (BinOp::Le, _) => CsilInstr::CmpLe,
                        (BinOp::Eq, _) => CsilInstr::CmpEq,
                        (BinOp::Ne, _) => CsilInstr::CmpNe,
                        (BinOp::And, _) => CsilInstr::BoolAnd,
                        (BinOp::Or, _) => CsilInstr::BoolOr,
                    };
                    self.emit(csil, span);
                    self.push_value(*dst);
                    // Close the lazy guard opened over this combine's lhs, if
                    // any: its skip jump lands here, just past the combine. An
                    // unguarded `and`/`or` (a serialized program with no guard) simply
                    // stays eager.
                    if matches!(op, BinOp::And | BinOp::Or) {
                        if let Some(entry) = self
                            .pending_guards
                            .iter()
                            .rposition(|(_, guarded)| guarded == lhs)
                        {
                            let (index, _) = self.pending_guards.remove(entry);
                            let landing = self.code.len() as u32;
                            match &mut self.code[index as usize] {
                                CsilInstr::JumpIfExact { target, .. } => *target = landing,
                                other => unreachable!(
                                    "pending guard points at non-guard instruction {other:?}"
                                ),
                            }
                        }
                    }
                }
                MirInstr::IntToFloat { dst, operand, .. } => {
                    self.pop_value(*operand, span)?;
                    self.emit(CsilInstr::IntToF64, span);
                    self.push_value(*dst);
                }
                MirInstr::Select {
                    dst,
                    cond,
                    lhs,
                    rhs,
                    ..
                } => {
                    // Pushed cond, lhs, rhs in evaluation order; SELECT pops
                    // them in reverse.
                    self.pop_value(*rhs, span)?;
                    self.pop_value(*lhs, span)?;
                    self.pop_value(*cond, span)?;
                    self.emit(CsilInstr::Select, span);
                    self.push_value(*dst);
                }
                MirInstr::WindowMean {
                    dst, slot, length, ..
                } => {
                    self.emit(
                        CsilInstr::WindowMean {
                            slot: slot.0,
                            length: *length,
                        },
                        span,
                    );
                    self.push_value(*dst);
                }
                MirInstr::WindowMeanDynamic {
                    dst, slot, length, ..
                } => {
                    self.pop_value(*length, span)?;
                    self.emit(CsilInstr::WindowMeanDynamic { slot: slot.0 }, span);
                    self.push_value(*dst);
                }
                MirInstr::WindowExtremumDynamic {
                    dst,
                    slot,
                    length,
                    highest,
                    ..
                } => {
                    self.pop_value(*length, span)?;
                    self.emit(
                        CsilInstr::WindowExtremumDynamic {
                            slot: slot.0,
                            highest: *highest,
                        },
                        span,
                    );
                    self.push_value(*dst);
                }
                MirInstr::EmaSimpleDynamic {
                    dst,
                    slot,
                    source,
                    length,
                    ..
                } => {
                    self.pop_value(*length, span)?;
                    self.pop_value(*source, span)?;
                    self.emit(CsilInstr::EmaSimpleDynamic { slot: slot.0 }, span);
                    self.push_value(*dst);
                }
                MirInstr::CallPure {
                    dst, func, args, ..
                } => {
                    // Arguments were pushed in order; the stack pops them in
                    // reverse (last argument on top).
                    for arg in args.iter().rev() {
                        self.pop_value(*arg, span)?;
                    }
                    self.emit(
                        CsilInstr::CallPure {
                            func: *func,
                            argc: args.len() as u8,
                        },
                        span,
                    );
                    self.push_value(*dst);
                }
                MirInstr::CallCollection {
                    dst, func, args, ..
                } => {
                    // Arguments were pushed in order (receiver first); the stack
                    // pops them in reverse (last argument on top).
                    for arg in args.iter().rev() {
                        self.pop_value(*arg, span)?;
                    }
                    self.emit(
                        CsilInstr::CallCollection {
                            func: *func,
                            argc: args.len() as u8,
                        },
                        span,
                    );
                    self.push_value(*dst);
                }
                MirInstr::CallTa {
                    dsts, state, args, ..
                } => {
                    // Runtime source operands were pushed in order; the stack
                    // pops them in reverse (last operand on top).
                    for arg in args.iter().rev() {
                        self.pop_value(*arg, span)?;
                    }
                    self.emit(CsilInstr::CallTa { state: *state }, span);
                    // The call pushes `dsts.len()` results left to right (scalar
                    // functions push one; tuple functions push their whole tuple).
                    for dst in dsts {
                        self.push_value(*dst);
                    }
                }
                MirInstr::NewObject {
                    dst,
                    type_id,
                    fields,
                    ..
                } => {
                    // Field values were pushed in order; pop in reverse (last field
                    // on top).
                    for field in fields.iter().rev() {
                        self.pop_value(*field, span)?;
                    }
                    self.emit(
                        CsilInstr::NewObject {
                            type_id: *type_id,
                            field_count: fields.len() as u32,
                        },
                        span,
                    );
                    self.push_value(*dst);
                }
                MirInstr::CopyObject {
                    dst, type_id, obj, ..
                } => {
                    self.pop_value(*obj, span)?;
                    self.emit(CsilInstr::CopyObject { type_id: *type_id }, span);
                    self.push_value(*dst);
                }
                MirInstr::GetField {
                    dst,
                    obj,
                    field_idx,
                    ..
                } => {
                    self.pop_value(*obj, span)?;
                    self.emit(
                        CsilInstr::GetField {
                            field_idx: *field_idx,
                        },
                        span,
                    );
                    self.push_value(*dst);
                }
                MirInstr::FootprintRowByPrice {
                    dst,
                    footprint,
                    price,
                    ..
                } => {
                    self.pop_value(*price, span)?;
                    self.pop_value(*footprint, span)?;
                    self.emit(CsilInstr::FootprintRowByPrice, span);
                    self.push_value(*dst);
                }
                MirInstr::SetField {
                    obj,
                    field_idx,
                    value,
                    ..
                } => {
                    // The object handle was pushed first, then the value; pop the
                    // value (on top) then the handle.
                    self.pop_value(*value, span)?;
                    self.pop_value(*obj, span)?;
                    self.emit(
                        CsilInstr::SetField {
                            field_idx: *field_idx,
                        },
                        span,
                    );
                }
                MirInstr::WindowStatisticDynamic {
                    dst,
                    slot,
                    length,
                    func,
                    span,
                } => {
                    self.pop_value(*length, *span)?;
                    self.emit(
                        CsilInstr::WindowStatisticDynamic {
                            slot: slot.0,
                            func: *func,
                        },
                        *span,
                    );
                    self.push_value(*dst);
                }
                MirInstr::RegisterDrawing { dst, kind, obj, .. } => {
                    self.pop_value(*obj, span)?;
                    self.emit(CsilInstr::RegisterDrawing { kind: *kind }, span);
                    self.push_value(*dst);
                }
                MirInstr::ReadDrawingsAll { dst, kind, .. } => {
                    self.emit(CsilInstr::ReadDrawingsAll { kind: *kind }, span);
                    self.push_value(*dst);
                }
                MirInstr::DeleteDrawing { dst, kind, id, .. } => {
                    self.pop_value(*id, span)?;
                    self.emit(CsilInstr::DeleteDrawing { kind: *kind }, span);
                    // Push a placeholder `na` so the void mutator has a value for
                    // the enclosing `Discard` to pop (uniform stack discipline).
                    self.emit(CsilInstr::PushNa, span);
                    self.push_value(*dst);
                }
                MirInstr::SetCell {
                    dst,
                    obj,
                    column,
                    row,
                    attrs,
                    values,
                    ..
                } => {
                    // Pushed obj, column, row, then each attribute value in order;
                    // pop values (last on top) in reverse, then row, column, obj.
                    for value in values.iter().rev() {
                        self.pop_value(*value, span)?;
                    }
                    self.pop_value(*row, span)?;
                    self.pop_value(*column, span)?;
                    self.pop_value(*obj, span)?;
                    self.emit(
                        CsilInstr::SetCell {
                            attrs: attrs.clone(),
                        },
                        span,
                    );
                    self.emit(CsilInstr::PushNa, span);
                    self.push_value(*dst);
                }
                MirInstr::ClearCells {
                    dst,
                    obj,
                    start_column,
                    start_row,
                    end_column,
                    end_row,
                    ..
                } => {
                    self.pop_value(*end_row, span)?;
                    self.pop_value(*end_column, span)?;
                    self.pop_value(*start_row, span)?;
                    self.pop_value(*start_column, span)?;
                    self.pop_value(*obj, span)?;
                    self.emit(CsilInstr::ClearCells, span);
                    self.emit(CsilInstr::PushNa, span);
                    self.push_value(*dst);
                }
                MirInstr::MergeCells {
                    dst,
                    obj,
                    start_column,
                    start_row,
                    end_column,
                    end_row,
                    ..
                } => {
                    self.pop_value(*end_row, span)?;
                    self.pop_value(*end_column, span)?;
                    self.pop_value(*start_row, span)?;
                    self.pop_value(*start_column, span)?;
                    self.pop_value(*obj, span)?;
                    self.emit(CsilInstr::MergeCells, span);
                    self.emit(CsilInstr::PushNa, span);
                    self.push_value(*dst);
                }
                MirInstr::Plot {
                    plot_slot,
                    value,
                    colors,
                    ..
                } => {
                    for (_, color) in colors.iter().rev() {
                        self.pop_value(*color, span)?;
                    }
                    self.pop_value(*value, span)?;
                    let instr = if colors.is_empty() {
                        CsilInstr::PlotF64 {
                            plot_slot: plot_slot.0,
                        }
                    } else {
                        CsilInstr::PlotStyledF64 {
                            plot_slot: plot_slot.0,
                            color_count: colors.len() as u8,
                        }
                    };
                    self.emit(instr, span);
                }
                MirInstr::Visual {
                    visual_slot,
                    colors,
                    ..
                } => {
                    for (_, color) in colors.iter().rev() {
                        self.pop_value(*color, span)?;
                    }
                    self.emit(
                        CsilInstr::VisualColor {
                            visual_slot: visual_slot.0,
                            color_count: colors.len() as u8,
                        },
                        span,
                    );
                }
                MirInstr::Fill {
                    fill_slot, colors, ..
                } => {
                    for (_, color) in colors.iter().rev() {
                        self.pop_value(*color, span)?;
                    }
                    self.emit(
                        CsilInstr::FillColor {
                            fill_slot: fill_slot.0,
                            color_count: colors.len() as u8,
                        },
                        span,
                    );
                }
                MirInstr::RuntimeError { message, .. } => {
                    self.pop_value(*message, span)?;
                    self.emit(CsilInstr::RuntimeError, span);
                }
                MirInstr::Log {
                    level, operands, ..
                } => {
                    // Operands (format string, then substitution args) were pushed
                    // in order; pop in reverse (last arg on top).
                    for op in operands.iter().rev() {
                        self.pop_value(*op, span)?;
                    }
                    self.emit(
                        CsilInstr::Log {
                            level: *level,
                            argc: operands.len() as u8,
                        },
                        span,
                    );
                }
                MirInstr::Alert { message, freq, .. } => {
                    // Pushed message then freq; pop freq (top) then message.
                    self.pop_value(*freq, span)?;
                    self.pop_value(*message, span)?;
                    self.emit(CsilInstr::Alert, span);
                }
                MirInstr::AlertCondition {
                    condition,
                    title,
                    message,
                    ..
                } => {
                    // Pushed condition, title, message; pop message, title, cond.
                    self.pop_value(*message, span)?;
                    self.pop_value(*title, span)?;
                    self.pop_value(*condition, span)?;
                    self.emit(CsilInstr::AlertCondition, span);
                }
                MirInstr::Order { kind, operands, .. } => {
                    // Operands were pushed in slot order; pop in reverse (last slot
                    // on top) so the VM sees a clean stack after the op.
                    for op in operands.iter().rev() {
                        self.pop_value(*op, span)?;
                    }
                    self.emit(CsilInstr::Order { kind: *kind }, span);
                }
                MirInstr::Discard { value, .. } => {
                    self.pop_value(*value, span)?;
                    self.emit(CsilInstr::Pop, span);
                }
            }
            Ok(())
        }

        fn lower_terminator(&mut self, terminator: &Terminator) -> Result<(), LowerErrorReference> {
            match terminator {
                Terminator::Return { span } => {
                    self.check_balanced()?;
                    self.emit(CsilInstr::Return, *span);
                }
                Terminator::Jump { target, span } => {
                    self.check_balanced()?;
                    let index = self.code.len() as u32;
                    self.emit(CsilInstr::Jump { target: u32::MAX }, *span);
                    self.fixups.push((index, target.0));
                }
                Terminator::Branch {
                    cond,
                    then_block,
                    else_block,
                    span,
                } => {
                    self.pop_value(*cond, *span)?;
                    self.check_balanced()?;
                    let jif_index = self.code.len() as u32;
                    self.emit(CsilInstr::JumpIfFalse { target: u32::MAX }, *span);
                    self.fixups.push((jif_index, else_block.0));
                    let jump_index = self.code.len() as u32;
                    self.emit(CsilInstr::Jump { target: u32::MAX }, *span);
                    self.fixups.push((jump_index, then_block.0));
                }
                Terminator::BranchIfSlotWritten {
                    slot,
                    written_block,
                    unwritten_block,
                    span,
                } => {
                    self.check_balanced()?;
                    let jsw_index = self.code.len() as u32;
                    self.emit(
                        CsilInstr::JumpIfSlotWritten {
                            slot: slot.0,
                            target: u32::MAX,
                        },
                        *span,
                    );
                    self.fixups.push((jsw_index, written_block.0));
                    let jump_index = self.code.len() as u32;
                    self.emit(CsilInstr::Jump { target: u32::MAX }, *span);
                    self.fixups.push((jump_index, unwritten_block.0));
                }
            }
            Ok(())
        }

        /// Statement-level invariant: the operand stack is empty at every block
        /// boundary, and every lazy boolean guard has met its combine (the pair
        /// lives inside a single expression, which never spans blocks).
        fn check_balanced(&self) -> Result<(), LowerErrorReference> {
            if !self.sim_stack.is_empty() {
                return Err(LowerErrorReference::UnbalancedBlock(self.sim_stack.len()));
            }
            if !self.pending_guards.is_empty() {
                return Err(LowerErrorReference::UnclosedLazyGuard(
                    self.pending_guards.len(),
                ));
            }
            Ok(())
        }
    }
    // ---- comparison harness ----

    /// The reference shape of a current error: the same variant, with the found
    /// register or the operator formatted the way the reference formats it.
    fn as_reference(error: &LowerError) -> LowerErrorReference {
        match error {
            LowerError::StackMismatch(span, expected, Some(found)) => {
                LowerErrorReference::StackMismatch(*span, *expected, format!("v{found}"))
            }
            LowerError::StackMismatch(span, expected, None) => {
                LowerErrorReference::StackMismatch(*span, *expected, "empty stack".to_string())
            }
            LowerError::UnbalancedBlock(left) => LowerErrorReference::UnbalancedBlock(*left),
            LowerError::DuplicateBlockId(block) => LowerErrorReference::DuplicateBlockId(*block),
            LowerError::MissingBlockId(block) => LowerErrorReference::MissingBlockId(*block),
            LowerError::StackMergeMismatch(block) => {
                LowerErrorReference::StackMergeMismatch(*block)
            }
            LowerError::GuardMergeMismatch(block) => {
                LowerErrorReference::GuardMergeMismatch(*block)
            }
            LowerError::MalformedLazyGuard(span, op) => LowerErrorReference::MalformedLazyGuard(
                *span,
                format!("`{}` is not a lazy boolean operator", op.symbol()),
            ),
            LowerError::UnclosedLazyGuard(open) => LowerErrorReference::UnclosedLazyGuard(*open),
            LowerError::NonConstValue(span) => LowerErrorReference::NonConstValue(*span),
            LowerError::BlockIdOutOfRange(..)
            | LowerError::BranchTargetOutOfRange(..)
            | LowerError::RedefinedRegister(..)
            | LowerError::TooManyOperands(..)
            | LowerError::LazyGuardOperatorMismatch(..) => {
                unreachable!("input check {error:?} has no reference form")
            }
            LowerError::PassRejected { .. } => {
                unreachable!("stackification never validates passes")
            }
        }
    }

    /// The input checks, which the reference stackifier does not perform.
    fn is_input_check(error: &LowerError) -> bool {
        matches!(
            error,
            LowerError::BlockIdOutOfRange(..)
                | LowerError::DuplicateBlockId(..)
                | LowerError::MissingBlockId(..)
                | LowerError::BranchTargetOutOfRange(..)
                | LowerError::RedefinedRegister(..)
                | LowerError::TooManyOperands(..)
                | LowerError::LazyGuardOperatorMismatch(..)
                | LowerError::StackMergeMismatch(..)
                | LowerError::GuardMergeMismatch(..)
        )
    }

    /// Compare with the baseline lowering. Where an input check fires, the new
    /// lowering returns that error in both pipelines (never a panic) and the
    /// reference outcome is returned for classification; everywhere else the
    /// outcomes are equal ([`assert_same`]).
    #[allow(clippy::result_large_err)]
    fn compare(mir: &MirBody) -> Result<Outcome, (LowerError, Outcome)> {
        let unoptimized = catch_unwind(AssertUnwindSafe(|| lower_mir_with(mir, |_, _, _| {})))
            .expect("the lowering never panics here");
        if let Err(error) = unoptimized {
            if is_input_check(&error) {
                let optimized =
                    catch_unwind(AssertUnwindSafe(|| lower_mir(mir))).expect("never panics here");
                assert_eq!(optimized.as_ref().err(), Some(&error));
                return Err((error, reference_outcome(mir, false)));
            }
        }
        Ok(assert_same(mir))
    }

    /// A lowering outcome: the program, or the error with its message, or
    /// the panic message.
    type Outcome = Result<Result<CsilProgram, (LowerErrorReference, String)>, String>;

    fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
        if let Some(message) = payload.downcast_ref::<String>() {
            message.clone()
        } else if let Some(message) = payload.downcast_ref::<&str>() {
            (*message).to_string()
        } else {
            "<non-string panic payload>".to_string()
        }
    }

    fn new_outcome(mir: &MirBody, optimized: bool) -> Outcome {
        catch_unwind(AssertUnwindSafe(|| {
            let result = if optimized {
                lower_mir(mir)
            } else {
                lower_mir_with(mir, |_, _, _| {})
            };
            result.map_err(|error| (as_reference(&error), error.to_string()))
        }))
        .map_err(|payload| panic_message(payload.as_ref()))
    }

    fn reference_outcome(mir: &MirBody, optimized: bool) -> Outcome {
        catch_unwind(AssertUnwindSafe(|| {
            let result = if optimized {
                lower_mir_with_reference(mir, optimize)
            } else {
                lower_mir_with_reference(mir, |_, _, _| {})
            };
            result.map_err(|error| {
                let message = error.to_string();
                (error, message)
            })
        }))
        .map_err(|payload| panic_message(payload.as_ref()))
    }

    /// Asserts equal outcomes (with and without the optimization passes)
    /// and returns the unoptimized one.
    fn assert_same(mir: &MirBody) -> Outcome {
        let expected = reference_outcome(mir, true);
        let actual = new_outcome(mir, true);
        assert_equivalent_outcomes(&actual, &expected, "optimized", &mir.blocks);
        let expected = reference_outcome(mir, false);
        let actual = new_outcome(mir, false);
        assert_equivalent_outcomes(&actual, &expected, "unoptimized", &mir.blocks);
        actual
    }

    /// The reference stackifier carries state linearly in block order and
    /// requires an empty stack at every edge. The CFG-aware lowerer may accept
    /// a valid preserved prefix, or may reject malformed unreachable blocks
    /// with a different stack-shape diagnostic. In either case, successful
    /// programs must still match the reference byte-for-byte.
    fn assert_equivalent_outcomes(
        actual: &Outcome,
        expected: &Outcome,
        mode: &str,
        blocks: &[MirBlock],
    ) {
        if actual == expected
            || matches!(
                (actual, expected),
                (
                    Ok(Ok(_)),
                    Ok(Err((
                        LowerErrorReference::UnbalancedBlock(_)
                            | LowerErrorReference::UnclosedLazyGuard(_),
                        _,
                    )))
                )
            )
            || matches!(
                (actual, expected),
                (
                    Ok(Err((actual_error, _))),
                    Ok(Err((expected_error, _)))
                ) if is_stack_shape_error(actual_error) && is_stack_shape_error(expected_error)
            )
            || matches!(
                (actual, expected),
                (
                    Ok(Err(_)),
                    Ok(Err((expected_error, _)))
                ) if is_baseline_edge_rejection(expected_error)
            )
            || matches!(
                (actual, expected),
                (
                    Ok(Err((actual_error, _))),
                    Ok(Err(_))
                ) if is_baseline_edge_rejection(actual_error)
            )
        {
            return;
        }
        assert_eq!(actual, expected, "{mode}: {blocks:#?}");
    }

    fn is_stack_shape_error(error: &LowerErrorReference) -> bool {
        matches!(
            error,
            LowerErrorReference::StackMismatch(..)
                | LowerErrorReference::UnbalancedBlock(..)
                | LowerErrorReference::UnclosedLazyGuard(..)
                | LowerErrorReference::StackMergeMismatch(..)
                | LowerErrorReference::GuardMergeMismatch(..)
        )
    }

    fn is_baseline_edge_rejection(error: &LowerErrorReference) -> bool {
        matches!(
            error,
            LowerErrorReference::UnbalancedBlock(_) | LowerErrorReference::UnclosedLazyGuard(_)
        )
    }

    /// [`assert_same`] for the crate's other tests.
    pub(crate) fn assert_matches_reference(mir: &MirBody) {
        let _outcome = assert_same(mir);
    }

    fn corpus_mirs() -> Vec<(std::path::PathBuf, MirBody)> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut files = Vec::new();
        for dir in [
            "tests/compile-pass",
            "tests/compile-fail",
            "tests/regression",
            "benchmarks/workloads",
        ] {
            for entry in std::fs::read_dir(root.join(dir)).expect("fixture directory") {
                let path = entry.expect("fixture entry").path();
                if path.extension().is_some_and(|ext| ext == "pine") {
                    files.push(path);
                }
            }
        }
        files.sort();
        let mut mirs = Vec::new();
        for path in files {
            let source = std::fs::read_to_string(&path).expect("fixture file");
            let parsed = pinewasm_parse::parse(&source);
            if pinewasm_span::has_errors(&parsed.diagnostics) {
                continue;
            }
            let analysis = pinewasm_semantics::analyze(&parsed.program);
            if pinewasm_span::has_errors(&analysis.diagnostics) {
                continue;
            }
            mirs.push((path, pinewasm_mir::lower(&analysis.hir)));
        }
        mirs
    }

    /// Every fixture program that analyzes without libraries lowers to the
    /// same `CsilProgram` (and the same serialized bytes) through the new and
    /// the reference stackification.
    #[test]
    fn corpus_programs_stackify_identically() {
        let mirs = corpus_mirs();
        let mut guarded = 0;
        for (path, mir) in &mirs {
            let expected = reference_outcome(mir, true);
            let actual = new_outcome(mir, true);
            assert_equivalent_outcomes(&actual, &expected, "optimized", &mir.blocks);
            let program = actual
                .expect("fixture programs do not panic")
                .unwrap_or_else(|error| panic!("{}: {error:?}", path.display()));
            match expected {
                Ok(Ok(expected)) => assert_eq!(
                    serde_json::to_string(&program).unwrap(),
                    serde_json::to_string(&expected).unwrap(),
                    "{}",
                    path.display()
                ),
                Ok(Err((LowerErrorReference::UnbalancedBlock(_), _))) => {}
                other => panic!(
                    "unexpected reference stackifier outcome for {}: {other:#?}",
                    path.display()
                ),
            }
            let expected_unoptimized = reference_outcome(mir, false);
            let actual_unoptimized = new_outcome(mir, false);
            assert_equivalent_outcomes(
                &actual_unoptimized,
                &expected_unoptimized,
                "unoptimized",
                &mir.blocks,
            );
            if program.functions[0]
                .code
                .iter()
                .any(|instr| matches!(instr, CsilInstr::JumpIfExact { .. }))
            {
                guarded += 1;
            }
        }
        eprintln!("fixtures: {} programs stackified identically", mirs.len());
        assert!(
            mirs.len() >= 40,
            "only {} fixture programs compiled",
            mirs.len()
        );
        assert!(guarded >= 1, "no fixture program exercised a lazy guard");
    }

    fn span(index: u32) -> Span {
        Span::new(pinewasm_span::SourceId(1), index, index + 1)
    }

    fn body(blocks: Vec<MirBlock>) -> MirBody {
        let parsed = pinewasm_parse::parse("//@version=6\nindicator(\"t\")\nplot(close)\n");
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        let mut mir = pinewasm_mir::lower(&analysis.hir);
        // The synthetic MIR sweep emits slot-zero reads to exercise every
        // instruction form; keep that fixture's metadata internally sized.
        mir.metadata.series_slots.push(crate::SlotDef {
            name: "stackifier-test".to_string(),
            persistent: false,
            varip: false,
            lane: None,
        });
        mir.blocks = blocks;
        mir
    }

    fn block(id: u32, instrs: Vec<MirInstr>, terminator: Terminator) -> MirBlock {
        MirBlock {
            id: BlockId(id),
            instrs,
            terminator,
        }
    }

    fn konst(dst: u32, value: Value) -> MirInstr {
        MirInstr::Const {
            dst: ValueId(dst),
            value,
            span: span(dst),
        }
    }

    fn ret() -> Terminator {
        Terminator::Return { span: span(99) }
    }

    #[test]
    fn nested_ternary_preserves_left_operand_across_cfg_edges() {
        let source = r#"
//@version=6
indicator("nested ternary stack prefix")
plot(10.0 + (close > open ? (high > low ? high : low) : close))
"#;
        let parsed = pinewasm_parse::parse(source);
        assert!(
            !pinewasm_span::has_errors(&parsed.diagnostics),
            "parse diagnostics: {:#?}",
            parsed.diagnostics
        );
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        assert!(
            !pinewasm_span::has_errors(&analysis.diagnostics),
            "semantic diagnostics: {:#?}",
            analysis.diagnostics
        );
        let mir = pinewasm_mir::lower(&analysis.hir);
        let program = lower_mir(&mir).expect("nested ternary lowers with a live prefix");
        crate::validate(&program).expect("nested ternary CSIL validates");

        let code = &program.functions[0].code;
        assert!(
            code.iter()
                .filter(|instr| matches!(instr, CsilInstr::JumpIfFalse { .. }))
                .count()
                >= 2,
            "both nested ternaries must lower to conditional control flow: {code:#?}"
        );
        assert!(
            program.functions[0].max_stack >= 3,
            "the left addend and comparison operands should coexist on the stack"
        );
    }

    #[test]
    fn nested_non_topological_diamond_propagates_exact_stack_prefixes() {
        let mut mir = body(Vec::new());
        // The outer else arm (b1) and nested branch (b3) are ready together.
        // The join b5 is discovered from b1 before the nested arms b2/b4,
        // which then have to agree on both preserved v0 and result register v6.
        // Store blocks out of id order as well to exercise the id lookup table.
        mir.blocks = vec![
            block(
                0,
                vec![konst(0, Value::Int(10)), konst(1, Value::Bool(true))],
                Terminator::Branch {
                    cond: ValueId(1),
                    then_block: BlockId(3),
                    else_block: BlockId(1),
                    span: span(10),
                },
            ),
            block(
                5,
                vec![
                    MirInstr::Binary {
                        dst: ValueId(7),
                        op: BinOp::Add,
                        lhs: ValueId(0),
                        rhs: ValueId(6),
                        is_int: true,
                        is_string: false,
                        span: span(20),
                    },
                    MirInstr::Discard {
                        value: ValueId(7),
                        span: span(21),
                    },
                ],
                ret(),
            ),
            block(
                4,
                vec![konst(6, Value::Int(13))],
                Terminator::Jump {
                    target: BlockId(5),
                    span: span(14),
                },
            ),
            block(
                2,
                vec![konst(6, Value::Int(12))],
                Terminator::Jump {
                    target: BlockId(5),
                    span: span(12),
                },
            ),
            block(
                3,
                vec![konst(2, Value::Bool(false))],
                Terminator::Branch {
                    cond: ValueId(2),
                    then_block: BlockId(4),
                    else_block: BlockId(2),
                    span: span(13),
                },
            ),
            block(
                1,
                vec![konst(6, Value::Int(11))],
                Terminator::Jump {
                    target: BlockId(5),
                    span: span(11),
                },
            ),
        ];

        let program = lower_mir(&mir).expect("exact stack prefixes agree at b5");
        crate::validate(&program).expect("non-topological diamond CSIL validates");
        assert_eq!(program.functions[0].max_stack, 2);
    }

    #[test]
    fn stack_merge_rejects_different_registers_at_equal_depth() {
        let mir = body(vec![
            block(
                0,
                vec![konst(0, Value::Bool(true))],
                Terminator::Branch {
                    cond: ValueId(0),
                    then_block: BlockId(1),
                    else_block: BlockId(2),
                    span: span(1),
                },
            ),
            block(
                1,
                vec![konst(1, Value::Int(10))],
                Terminator::Jump {
                    target: BlockId(3),
                    span: span(2),
                },
            ),
            block(
                2,
                vec![konst(2, Value::Int(20))],
                Terminator::Jump {
                    target: BlockId(3),
                    span: span(3),
                },
            ),
            block(3, vec![], ret()),
        ]);
        assert!(matches!(
            lower_mir(&mir),
            Err(LowerError::StackMergeMismatch(3))
        ));
    }

    #[test]
    fn stack_merge_rejects_different_depths() {
        let mir = body(vec![
            block(
                0,
                vec![konst(0, Value::Bool(true))],
                Terminator::Branch {
                    cond: ValueId(0),
                    then_block: BlockId(1),
                    else_block: BlockId(2),
                    span: span(1),
                },
            ),
            block(
                1,
                vec![konst(1, Value::Int(10))],
                Terminator::Jump {
                    target: BlockId(3),
                    span: span(2),
                },
            ),
            block(
                2,
                vec![],
                Terminator::Jump {
                    target: BlockId(3),
                    span: span(3),
                },
            ),
            block(3, vec![], ret()),
        ]);
        assert!(matches!(
            lower_mir(&mir),
            Err(LowerError::StackMergeMismatch(3))
        ));
    }

    #[test]
    fn guard_merge_rejects_different_open_guards_at_equal_stack_shape() {
        let mir = body(vec![
            block(
                0,
                vec![konst(1, Value::Bool(true)), konst(2, Value::Bool(true))],
                Terminator::Branch {
                    cond: ValueId(2),
                    then_block: BlockId(1),
                    else_block: BlockId(2),
                    span: span(1),
                },
            ),
            block(
                1,
                vec![MirInstr::LazyBoolGuard {
                    op: BinOp::And,
                    lhs: ValueId(1),
                    span: span(2),
                }],
                Terminator::Jump {
                    target: BlockId(3),
                    span: span(3),
                },
            ),
            block(
                2,
                vec![],
                Terminator::Jump {
                    target: BlockId(3),
                    span: span(4),
                },
            ),
            block(
                3,
                vec![MirInstr::Discard {
                    value: ValueId(1),
                    span: span(5),
                }],
                ret(),
            ),
        ]);
        assert!(matches!(
            lower_mir(&mir),
            Err(LowerError::GuardMergeMismatch(3))
        ));
    }

    #[test]
    fn return_still_requires_an_empty_operand_stack() {
        let mir = body(vec![block(0, vec![konst(0, Value::Int(10))], ret())]);
        assert!(matches!(
            lower_mir(&mir),
            Err(LowerError::UnbalancedBlock(1))
        ));
    }

    fn lowered_error(outcome: Outcome) -> (LowerErrorReference, String) {
        outcome.expect("no panic").expect_err("an error")
    }

    #[test]
    fn stack_mismatch_messages_are_unchanged() {
        let discard = |value| MirInstr::Discard {
            value: ValueId(value),
            span: span(7),
        };
        let (_, message) =
            lowered_error(assert_same(&body(vec![block(0, vec![discard(3)], ret())])));
        assert!(
            message.ends_with("expected v3, found empty stack"),
            "{message}"
        );
        let (_, message) = lowered_error(assert_same(&body(vec![block(
            0,
            vec![konst(4, Value::Na), discard(3)],
            ret(),
        )])));
        assert!(message.ends_with("expected v3, found v4"), "{message}");
    }

    #[test]
    fn malformed_guard_message_is_unchanged() {
        let guard = MirInstr::LazyBoolGuard {
            op: BinOp::Add,
            lhs: ValueId(0),
            span: span(3),
        };
        let (_, message) = lowered_error(assert_same(&body(vec![block(
            0,
            vec![konst(0, Value::Bool(true)), guard],
            ret(),
        )])));
        assert!(
            message.ends_with("malformed: `+` is not a lazy boolean operator"),
            "{message}"
        );
    }

    #[test]
    fn constants_intern_and_heap_refs_are_rejected() {
        let program = assert_same(&body(vec![block(
            0,
            vec![
                konst(0, Value::String("a".into())),
                konst(1, Value::String("b".into())),
                konst(2, Value::String("a".into())),
                konst(3, Value::Float(1.5)),
                MirInstr::Discard {
                    value: ValueId(3),
                    span: span(4),
                },
                MirInstr::Discard {
                    value: ValueId(2),
                    span: span(5),
                },
                MirInstr::Discard {
                    value: ValueId(1),
                    span: span(6),
                },
                MirInstr::Discard {
                    value: ValueId(0),
                    span: span(7),
                },
            ],
            ret(),
        )]))
        .unwrap()
        .unwrap();
        assert_eq!(program.constants, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(program.functions[0].max_stack, 4);
        let (error, _) = lowered_error(assert_same(&body(vec![block(
            0,
            vec![konst(0, Value::Ref(HandleId(0)))],
            ret(),
        )])));
        assert_eq!(error, LowerErrorReference::NonConstValue(span(0)));
    }

    #[test]
    fn nested_guards_close_innermost_first_and_unclosed_guards_are_reported() {
        let guard = |op, lhs| MirInstr::LazyBoolGuard {
            op,
            lhs: ValueId(lhs),
            span: span(lhs),
        };
        let combine = |op, dst, lhs, rhs| MirInstr::Binary {
            dst: ValueId(dst),
            op,
            lhs: ValueId(lhs),
            rhs: ValueId(rhs),
            is_int: false,
            is_string: false,
            span: span(dst),
        };
        // `a and (b or c)`, then a second guard over the same register
        // that the combine closes (the most recent one wins).
        let instrs = vec![
            konst(0, Value::Bool(true)),
            guard(BinOp::And, 0),
            konst(1, Value::Bool(false)),
            guard(BinOp::Or, 1),
            konst(2, Value::Bool(true)),
            combine(BinOp::Or, 3, 1, 2),
            combine(BinOp::And, 4, 0, 3),
            MirInstr::Discard {
                value: ValueId(4),
                span: span(9),
            },
        ];
        let program = assert_same(&body(vec![block(0, instrs, ret())]))
            .unwrap()
            .unwrap();
        let targets: Vec<_> = program.functions[0]
            .code
            .iter()
            .filter_map(|instr| match instr {
                CsilInstr::JumpIfExact { target, .. } => Some(*target),
                _ => None,
            })
            .collect();
        assert_eq!(targets.len(), 2);
        assert!(targets.iter().all(|target| *target != u32::MAX));
        let (error, _) = lowered_error(assert_same(&body(vec![block(
            0,
            vec![
                konst(0, Value::Bool(true)),
                guard(BinOp::And, 0),
                guard(BinOp::Or, 0),
                MirInstr::Discard {
                    value: ValueId(0),
                    span: span(5),
                },
            ],
            ret(),
        )])));
        assert_eq!(error, LowerErrorReference::UnclosedLazyGuard(2));
    }

    /// The baseline lowering panicked on an out-of-range block id or branch
    /// target; the current one reports it.
    #[test]
    fn out_of_range_block_ids_are_errors() {
        let jump = Terminator::Jump {
            target: BlockId(5),
            span: span(1),
        };
        let (error, reference) = compare(&body(vec![block(0, vec![], jump)])).unwrap_err();
        assert_eq!(error, LowerError::BranchTargetOutOfRange(span(1), 5));
        assert!(reference.unwrap_err().contains("index out of bounds"));
        let (error, reference) = compare(&body(vec![block(3, vec![], ret())])).unwrap_err();
        assert_eq!(error, LowerError::BlockIdOutOfRange(3));
        assert!(reference.unwrap_err().contains("index out of bounds"));
        let branch = Terminator::Branch {
            cond: ValueId(0),
            then_block: BlockId(0),
            else_block: BlockId(1),
            span: span(2),
        };
        let (error, _) = compare(&body(vec![block(
            0,
            vec![konst(0, Value::Bool(true))],
            branch,
        )]))
        .unwrap_err();
        assert_eq!(error, LowerError::BranchTargetOutOfRange(span(2), 1));
    }

    /// The three input cases the baseline lowering accepted (it returned a
    /// program) are errors now.
    #[test]
    fn input_holes_are_errors() {
        let discard = |value| MirInstr::Discard {
            value: ValueId(value),
            span: span(8),
        };
        // A live register redefined: `v0` is still on the stack.
        let redefined = body(vec![block(
            0,
            vec![
                konst(0, Value::Int(1)),
                konst(0, Value::Int(2)),
                discard(0),
                discard(0),
            ],
            ret(),
        )]);
        let (error, reference) = compare(&redefined).unwrap_err();
        assert_eq!(error, LowerError::RedefinedRegister(span(0), 0));
        assert!(
            matches!(reference, Ok(Ok(_))),
            "the baseline lowering accepted it"
        );
        // A `CallTa` defining the same register twice.
        let twice = body(vec![block(
            0,
            vec![
                MirInstr::CallTa {
                    dsts: vec![ValueId(1), ValueId(1)],
                    state: 0,
                    args: vec![],
                    span: span(4),
                },
                discard(1),
                discard(1),
            ],
            ret(),
        )]);
        let (error, _) = compare(&twice).unwrap_err();
        assert_eq!(error, LowerError::RedefinedRegister(span(4), 1));
        // 256 arguments: `argc` would truncate to 0.
        let args: Vec<ValueId> = (0..256).map(ValueId).collect();
        let mut instrs: Vec<MirInstr> = (0..256).map(|i| konst(i, Value::Int(0))).collect();
        instrs.push(MirInstr::CallPure {
            dst: ValueId(300),
            func: pinewasm_hir::PureFunc::MathMax,
            args,
            span: span(300),
        });
        instrs.push(discard(300));
        let (error, reference) = compare(&body(vec![block(0, instrs, ret())])).unwrap_err();
        assert_eq!(error, LowerError::TooManyOperands(span(300), 256));
        assert!(
            matches!(reference, Ok(Ok(_))),
            "the baseline lowering truncated"
        );
        // 255 arguments are fine.
        let args: Vec<ValueId> = (0..255).map(ValueId).collect();
        let mut instrs: Vec<MirInstr> = (0..255).map(|i| konst(i, Value::Int(0))).collect();
        instrs.push(MirInstr::CallPure {
            dst: ValueId(300),
            func: pinewasm_hir::PureFunc::MathMax,
            args,
            span: span(300),
        });
        instrs.push(discard(300));
        assert!(matches!(
            compare(&body(vec![block(0, instrs, ret())])),
            Ok(Ok(Ok(_)))
        ));
        // A guard opened for `and`, closed by `or`.
        let mixed = body(vec![block(
            0,
            vec![
                konst(0, Value::Bool(true)),
                MirInstr::LazyBoolGuard {
                    op: BinOp::And,
                    lhs: ValueId(0),
                    span: span(1),
                },
                konst(1, Value::Bool(false)),
                MirInstr::Binary {
                    dst: ValueId(2),
                    op: BinOp::Or,
                    lhs: ValueId(0),
                    rhs: ValueId(1),
                    is_int: false,
                    is_string: false,
                    span: span(2),
                },
                discard(2),
            ],
            ret(),
        )]);
        let (error, reference) = compare(&mixed).unwrap_err();
        assert_eq!(
            error,
            LowerError::LazyGuardOperatorMismatch(span(2), BinOp::Or)
        );
        assert!(
            matches!(reference, Ok(Ok(_))),
            "the baseline lowering accepted it"
        );
    }

    // ---- deterministic random sweep over every `MirInstr` variant ----

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }

        fn chance(&mut self, numerator: u64, denominator: u64) -> bool {
            self.below(denominator) < numerator
        }
    }

    /// Generates MIR that mostly follows the register discipline (so long
    /// runs lower successfully) and sometimes breaks it (so every error and
    /// panic path is reached).
    struct Gen {
        rng: Rng,
        stack: Vec<ValueId>,
        next: u32,
        blocks: u32,
    }

    const BIN_OPS: [BinOp; 13] = [
        BinOp::Add,
        BinOp::Sub,
        BinOp::Mul,
        BinOp::Div,
        BinOp::Rem,
        BinOp::Gt,
        BinOp::Ge,
        BinOp::Lt,
        BinOp::Le,
        BinOp::Eq,
        BinOp::Ne,
        BinOp::And,
        BinOp::Or,
    ];

    impl Gen {
        fn fresh(&mut self) -> ValueId {
            let value = ValueId(self.next);
            self.next += 1;
            self.stack.push(value);
            value
        }

        /// Pop the operand the model stack expects, or (rarely) a wrong one.
        fn take(&mut self) -> ValueId {
            if self.rng.chance(1, 400) {
                return ValueId(self.rng.below(u64::from(self.next) + 2) as u32);
            }
            match self.stack.pop() {
                Some(value) => value,
                None => ValueId(self.rng.below(u64::from(self.next) + 2) as u32),
            }
        }

        /// `count` operands in push order.
        fn take_many(&mut self, count: u64) -> Vec<ValueId> {
            let mut values: Vec<_> = (0..count).map(|_| self.take()).collect();
            values.reverse();
            values
        }

        fn colors(&mut self) -> Vec<(PlotColorRole, ValueId)> {
            let count = self.rng.below(3);
            self.take_many(count)
                .into_iter()
                .map(|value| (PlotColorRole::Color, value))
                .collect()
        }

        fn value(&mut self) -> Value {
            match self.rng.below(8) {
                0 => Value::Na,
                1 => Value::Bool(self.rng.chance(1, 2)),
                2 => Value::Int(self.rng.below(5) as i64 - 2),
                3 => Value::Float(self.rng.below(5) as f64 / 4.0),
                4 | 5 => Value::String(["a", "b", "c"][self.rng.below(3) as usize].into()),
                6 => Value::Color {
                    r: 1,
                    g: 2,
                    b: 3,
                    t: 4.0,
                    input_origin: None,
                },
                _ if self.rng.chance(1, 30) => Value::Ref(HandleId(0)),
                _ => Value::Na,
            }
        }

        fn block_id(&mut self) -> BlockId {
            // One past the last block, now and then, reaches the panics.
            let past_end = u64::from(self.rng.chance(1, 60));
            BlockId(self.rng.below(u64::from(self.blocks) + past_end) as u32)
        }

        #[allow(clippy::too_many_lines)]
        fn instr(&mut self, out: &mut Vec<MirInstr>) {
            // Keep enough operands around that consumers usually match.
            while self.stack.len() < 5 && !self.rng.chance(1, 200) {
                let value = Value::Int(i64::from(self.next));
                let dst = self.fresh();
                out.push(MirInstr::Const {
                    dst,
                    value,
                    span: span(dst.0),
                });
            }
            let span = span(self.next);
            let instr = match self.rng.below(45) {
                0 | 1 => {
                    let value = self.value();
                    let dst = self.fresh();
                    MirInstr::Const { dst, value, span }
                }
                2 => MirInstr::ReadBuiltinSeries {
                    dst: self.fresh(),
                    source: BuiltinSeries::Close,
                    offset: 1,
                    span,
                },
                3 => MirInstr::ReadUserSeries {
                    dst: self.fresh(),
                    slot: SeriesSlot(0),
                    offset: 0,
                    span,
                },
                4 => {
                    let offset = self.take();
                    MirInstr::ReadBuiltinSeriesDynamic {
                        dst: self.fresh(),
                        source: BuiltinSeries::High,
                        offset,
                        span,
                    }
                }
                5 => {
                    let offset = self.take();
                    MirInstr::ReadUserSeriesDynamic {
                        dst: self.fresh(),
                        slot: SeriesSlot(0),
                        offset,
                        span,
                    }
                }
                6 => MirInstr::ReadInput {
                    dst: self.fresh(),
                    input: 0,
                    span,
                },
                7 => MirInstr::ReadSecurity {
                    dst: self.fresh(),
                    selector: self.rng.chance(1, 2).then(|| "sym".to_string()),
                    source: BuiltinSeries::Close,
                    htf_seconds: 3600,
                    timeframe_expr: None,
                    calendar_months: None,
                    gaps_on: false,
                    lookahead_on: true,
                    span,
                },
                8 => MirInstr::ReadSecurityExpr {
                    dst: self.fresh(),
                    expr: HtfExpr::Const(2.5),
                    htf_seconds: 60,
                    timeframe_expr: None,
                    calendar_months: Some(1),
                    gaps_on: true,
                    lookahead_on: false,
                    span,
                },
                9 => MirInstr::ReadRequest {
                    dst: self.fresh(),
                    func: RequestFunc::Dividends,
                    selector: "div".into(),
                    span,
                },
                10 => MirInstr::BuildFootprint {
                    dst: self.fresh(),
                    selector: "fp".into(),
                    span,
                },
                11 => MirInstr::ReadSecurityLowerTf {
                    dst: self.fresh(),
                    selector: "ltf".into(),
                    span,
                },
                12 => MirInstr::ReadPivotLevels {
                    dst: self.fresh(),
                    system: PivotSystem::Fibonacci,
                    anchor_seconds: 86400,
                    span,
                },
                13 => MirInstr::ReadTimeTf {
                    dst: self.fresh(),
                    close: true,
                    tf_seconds: 60,
                    span,
                },
                14 => {
                    let price = self.take();
                    let footprint = self.take();
                    MirInstr::FootprintRowByPrice {
                        dst: self.fresh(),
                        footprint,
                        price,
                        span,
                    }
                }
                15 => MirInstr::StoreUserSeriesCurrent {
                    slot: SeriesSlot(0),
                    value: self.take(),
                    span,
                },
                16 => {
                    let operand = self.take();
                    MirInstr::Unary {
                        dst: self.fresh(),
                        op: if self.rng.chance(1, 2) {
                            UnOp::Neg
                        } else {
                            UnOp::Not
                        },
                        operand,
                        is_int: self.rng.chance(1, 2),
                        span,
                    }
                }
                17 | 18 => {
                    let rhs = self.take();
                    let lhs = self.take();
                    MirInstr::Binary {
                        dst: self.fresh(),
                        op: BIN_OPS[self.rng.below(13) as usize],
                        lhs,
                        rhs,
                        is_int: self.rng.chance(1, 2),
                        is_string: self.rng.chance(1, 3),
                        span,
                    }
                }
                19 | 20 => {
                    // A whole lazy expression `lhs op rhs`, or (rarely) a
                    // stray guard with any operator.
                    let lhs = match self.stack.last() {
                        Some(lhs) => *lhs,
                        None => {
                            let dst = self.fresh();
                            out.push(MirInstr::Const {
                                dst,
                                value: Value::Bool(true),
                                span,
                            });
                            dst
                        }
                    };
                    let op = if self.rng.chance(1, 100) {
                        BIN_OPS[self.rng.below(13) as usize]
                    } else if self.rng.chance(1, 2) {
                        BinOp::And
                    } else {
                        BinOp::Or
                    };
                    out.push(MirInstr::LazyBoolGuard { op, lhs, span });
                    if self.rng.chance(1, 60) {
                        return;
                    }
                    if self.rng.chance(1, 4) {
                        self.instr(out);
                    } else {
                        let dst = self.fresh();
                        out.push(MirInstr::Const {
                            dst,
                            value: Value::Bool(false),
                            span,
                        });
                    }
                    let rhs = self.take();
                    let lhs = self.take();
                    MirInstr::Binary {
                        dst: self.fresh(),
                        op,
                        lhs,
                        rhs,
                        is_int: false,
                        is_string: false,
                        span,
                    }
                }
                21 => {
                    let operand = self.take();
                    MirInstr::IntToFloat {
                        dst: self.fresh(),
                        operand,
                        span,
                    }
                }
                22 => {
                    let rhs = self.take();
                    let lhs = self.take();
                    let cond = self.take();
                    MirInstr::Select {
                        dst: self.fresh(),
                        cond,
                        lhs,
                        rhs,
                        span,
                    }
                }
                23 => MirInstr::WindowMean {
                    dst: self.fresh(),
                    slot: SeriesSlot(0),
                    length: 3,
                    span,
                },
                24 => {
                    let count = self.rng.below(4);
                    let args = self.take_many(count);
                    MirInstr::CallPure {
                        dst: self.fresh(),
                        func: PureFunc::Na,
                        args,
                        span,
                    }
                }
                25 => {
                    let count = self.rng.below(4);
                    let args = self.take_many(count);
                    MirInstr::CallCollection {
                        dst: self.fresh(),
                        func: CollectionFunc::ArrayNewFloat,
                        args,
                        span,
                    }
                }
                26 => {
                    let count = self.rng.below(3);
                    let args = self.take_many(count);
                    let results = 1 + self.rng.below(3);
                    MirInstr::CallTa {
                        dsts: (0..results).map(|_| self.fresh()).collect(),
                        state: 0,
                        args,
                        span,
                    }
                }
                27 => {
                    let count = self.rng.below(4);
                    let fields = self.take_many(count);
                    MirInstr::NewObject {
                        dst: self.fresh(),
                        type_id: 0,
                        fields,
                        span,
                    }
                }
                28 => {
                    let obj = self.take();
                    MirInstr::CopyObject {
                        dst: self.fresh(),
                        type_id: 0,
                        obj,
                        span,
                    }
                }
                29 => {
                    let obj = self.take();
                    MirInstr::GetField {
                        dst: self.fresh(),
                        obj,
                        field_idx: 1,
                        span,
                    }
                }
                30 => {
                    let value = self.take();
                    let obj = self.take();
                    MirInstr::SetField {
                        obj,
                        field_idx: 0,
                        value,
                        span,
                    }
                }
                31 => {
                    let obj = self.take();
                    MirInstr::RegisterDrawing {
                        dst: self.fresh(),
                        kind: DrawKind::Label,
                        obj,
                        span,
                    }
                }
                32 => MirInstr::ReadDrawingsAll {
                    dst: self.fresh(),
                    kind: DrawKind::Line,
                    span,
                },
                33 => {
                    let id = self.take();
                    MirInstr::DeleteDrawing {
                        dst: self.fresh(),
                        kind: DrawKind::Line,
                        id,
                        span,
                    }
                }
                34 => {
                    let count = self.rng.below(3);
                    let values = self.take_many(count);
                    let row = self.take();
                    let column = self.take();
                    let obj = self.take();
                    MirInstr::SetCell {
                        dst: self.fresh(),
                        obj,
                        column,
                        row,
                        attrs: vec![CellAttr::Text; count as usize],
                        values,
                        span,
                    }
                }
                35 | 36 => {
                    let end_row = self.take();
                    let end_column = self.take();
                    let start_row = self.take();
                    let start_column = self.take();
                    let obj = self.take();
                    let dst = self.fresh();
                    if self.rng.chance(1, 2) {
                        MirInstr::ClearCells {
                            dst,
                            obj,
                            start_column,
                            start_row,
                            end_column,
                            end_row,
                            span,
                        }
                    } else {
                        MirInstr::MergeCells {
                            dst,
                            obj,
                            start_column,
                            start_row,
                            end_column,
                            end_row,
                            span,
                        }
                    }
                }
                37 => {
                    let colors = self.colors();
                    MirInstr::Plot {
                        plot_slot: PlotSlot(0),
                        value: self.take(),
                        colors,
                        span,
                    }
                }
                38 => MirInstr::Visual {
                    visual_slot: VisualSlot(0),
                    colors: self.colors(),
                    span,
                },
                39 => MirInstr::Fill {
                    fill_slot: FillSlot(0),
                    colors: self.colors(),
                    span,
                },
                40 => MirInstr::RuntimeError {
                    message: self.take(),
                    span,
                },
                41 => {
                    let count = self.rng.below(4);
                    MirInstr::Log {
                        level: LogLevel::Warning,
                        operands: self.take_many(count),
                        span,
                    }
                }
                42 => {
                    let freq = self.take();
                    let message = self.take();
                    MirInstr::Alert {
                        message,
                        freq,
                        span,
                    }
                }
                43 => {
                    let message = self.take();
                    let title = self.take();
                    let condition = self.take();
                    MirInstr::AlertCondition {
                        condition,
                        title,
                        message,
                        span,
                    }
                }
                _ => {
                    if self.rng.chance(1, 2) {
                        let count = self.rng.below(4);
                        MirInstr::Order {
                            kind: OrderKind::Entry,
                            operands: self.take_many(count),
                            span,
                        }
                    } else {
                        MirInstr::Discard {
                            value: self.take(),
                            span,
                        }
                    }
                }
            };
            out.push(instr);
        }

        fn terminator(&mut self) -> Terminator {
            let span = span(1000 + self.next);
            match self.rng.below(31) {
                0..=9 => Terminator::Return { span },
                10..=19 => Terminator::Jump {
                    target: self.block_id(),
                    span,
                },
                20 => Terminator::Branch {
                    // The stack is balanced here: usually an empty-stack
                    // mismatch.
                    cond: self.take(),
                    then_block: self.block_id(),
                    else_block: self.block_id(),
                    span,
                },
                _ => Terminator::BranchIfSlotWritten {
                    slot: SeriesSlot(0),
                    written_block: self.block_id(),
                    unwritten_block: self.block_id(),
                    span,
                },
            }
        }

        fn block(&mut self, index: u32) -> MirBlock {
            let mut instrs = Vec::new();
            let length = self.rng.below(10);
            for _ in 0..length {
                self.instr(&mut instrs);
            }
            let terminator = if matches!(self.rng.below(4), 2) {
                // A branch condition: produce it, then balance the rest.
                let cond = ValueId(self.next);
                self.next += 1;
                let mut rest = Vec::new();
                while !self.stack.is_empty() && !self.rng.chance(1, 150) {
                    let value = self.take();
                    rest.push(MirInstr::Discard {
                        value,
                        span: span(self.next),
                    });
                }
                instrs.extend(rest);
                instrs.push(MirInstr::Const {
                    dst: cond,
                    value: Value::Bool(true),
                    span: span(cond.0),
                });
                Terminator::Branch {
                    cond: if self.rng.chance(1, 200) {
                        ValueId(cond.0 + 1)
                    } else {
                        cond
                    },
                    then_block: self.block_id(),
                    else_block: self.block_id(),
                    span: span(2000 + index),
                }
            } else {
                while !self.stack.is_empty() && !self.rng.chance(1, 150) {
                    let value = self.take();
                    instrs.push(MirInstr::Discard {
                        value,
                        span: span(self.next),
                    });
                }
                self.terminator()
            };
            self.stack.clear();
            let id = if self.rng.chance(1, 200) {
                self.block_id().0
            } else {
                index
            };
            block(id, instrs, terminator)
        }
    }

    #[test]
    fn deterministic_sweep_matches_reference() {
        let template = body(Vec::new());
        let mut gen = Gen {
            rng: Rng(0x9E37_79B9_7F4A_7C15),
            stack: Vec::new(),
            next: 0,
            blocks: 0,
        };
        let (mut ok, mut errors, mut panics, mut guarded) = (0, 0, 0, 0);
        let mut seen = std::collections::BTreeSet::new();
        // Bodies where an input check fired: the check, and what the baseline
        // lowering did there (`panic`, `ok` or its error kind).
        let mut checks = std::collections::BTreeMap::new();
        for _ in 0..4000 {
            gen.blocks = 1 + gen.rng.below(4) as u32;
            gen.next = 0;
            let blocks = (0..gen.blocks).map(|index| gen.block(index)).collect();
            let mut mir = template.clone();
            mir.blocks = blocks;
            let outcome = match compare(&mir) {
                Ok(outcome) => outcome,
                Err((error, reference)) => {
                    let kind = |d: String| d.split('(').next().unwrap().to_string();
                    let before = match reference {
                        Err(_) => "panic".to_string(),
                        Ok(Ok(_)) => "ok".to_string(),
                        Ok(Err((e, _))) => kind(format!("{e:?}")),
                    };
                    *checks
                        .entry((kind(format!("{error:?}")), before))
                        .or_insert(0) += 1;
                    continue;
                }
            };
            match outcome {
                Ok(Ok(program)) => {
                    ok += 1;
                    if program.functions[0]
                        .code
                        .iter()
                        .any(|instr| matches!(instr, CsilInstr::JumpIfExact { .. }))
                    {
                        guarded += 1;
                    }
                }
                Ok(Err((error, _))) => {
                    errors += 1;
                    seen.insert(format!("{error:?}").split('(').next().unwrap().to_string());
                }
                Err(_) => panics += 1,
            }
        }
        eprintln!(
            "sweep: {ok} ok, {errors} errors {seen:?}, {panics} panics, {guarded} guarded, \
             input checks {checks:?}"
        );
        // The current lowering never panics; the baseline panics are now
        // out-of-range block id or target errors.
        assert_eq!(panics, 0);
        assert!(ok > 400 && errors > 400 && guarded > 100);
        assert_eq!(seen.len(), 5, "not every error kind was reached: {seen:?}");
        let fired: u32 = checks.values().sum();
        assert!(fired > 20, "{checks:?}");
    }
}

/// The in-process translation validator (`crate::translation_validation`) on real pass outputs:
/// it accepts every output of the dead-literal and single-use passes (fixture programs,
/// every `compile_to_csil` program via `tests.rs`, a random sweep) and
/// rejects mutated outputs and certificates.
#[cfg(test)]
mod pair_erasure_tests {
    use super::*;
    use crate::{check_pair_erasure, instr_exact_eq, PairCheckError};
    use pinewasm_hir::HtfExpr;
    use pinewasm_span::SourceId;
    use std::time::{Duration, Instant};

    type Pass = fn(&mut Vec<CsilInstr>, &mut Vec<Span>, &[crate::SlotDef]);
    type Keep = fn(&Vec<CsilInstr>, &[crate::SlotDef]) -> Vec<bool>;

    /// The two pair-erasure passes with the analyses whose keep masks they
    /// apply (the certificates the validated pipeline checks).
    const PASSES: [(&str, Pass, Keep); 2] = [
        (
            "elide_dead_constant_stores",
            elide_dead_constant_stores,
            dead_constant_store_keep,
        ),
        (
            "coalesce_single_use_slots",
            coalesce_single_use_slots,
            single_use_slot_keep,
        ),
    ];

    /// One validated pass run: input, output, certificate.
    struct Run {
        before: Vec<CsilInstr>,
        before_spans: Vec<Span>,
        after: Vec<CsilInstr>,
        after_spans: Vec<Span>,
        starts: Vec<u32>,
    }

    impl Run {
        fn check(&self, slots: &[crate::SlotDef]) -> Result<(), PairCheckError> {
            check_pair_erasure(
                &self.before,
                &self.before_spans,
                &self.after,
                &self.after_spans,
                &self.starts,
                slots,
            )
        }
    }

    fn spans_for(len: usize) -> Vec<Span> {
        (0..len as u32)
            .map(|index| Span::new(SourceId(3), 10 * index, 10 * index + 4))
            .collect()
    }

    /// One stage as the validated pipeline runs it: the pass's keep mask,
    /// applied by `retain_instructions`, and the certificate read off the mask.
    /// Also checks that the pass itself produces the same output.
    fn stage(
        (pass, keep_of): (Pass, Keep),
        code: &[CsilInstr],
        spans: &[Span],
        slots: &[crate::SlotDef],
    ) -> Run {
        let keep = keep_of(&code.to_vec(), slots);
        let mut after = code.to_vec();
        let mut after_spans = spans.to_vec();
        retain_instructions(&mut after, &mut after_spans, &keep);
        let mut by_pass = code.to_vec();
        let mut by_pass_spans = spans.to_vec();
        pass(&mut by_pass, &mut by_pass_spans, slots);
        assert_eq!((&by_pass, &by_pass_spans), (&after, &after_spans));
        Run {
            before: code.to_vec(),
            before_spans: spans.to_vec(),
            after,
            after_spans,
            starts: crate::translation_validation::pair_starts(&keep),
        }
    }

    fn slot_defs(flags: &[(bool, bool)]) -> Vec<crate::SlotDef> {
        flags
            .iter()
            .enumerate()
            .map(|(index, &(persistent, varip))| crate::SlotDef {
                name: format!("s{index}"),
                persistent,
                varip,
                lane: None,
            })
            .collect()
    }

    fn plain(count: usize) -> Vec<crate::SlotDef> {
        slot_defs(&vec![(false, false); count])
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }
    }

    /// Admissible random instructions (slots and targets in range; the passes
    /// panic otherwise), weighted towards what the passes inspect.
    fn instruction(rng: &mut Rng, slot_count: u64, len: u64) -> CsilInstr {
        let slot = rng.below(slot_count.max(1)) as u32;
        let target = rng.below(len + 1) as u32;
        let literal = match rng.below(6) {
            0 => CsilInstr::PushNa,
            1 => CsilInstr::PushBool(rng.below(2) == 0),
            2 => CsilInstr::PushI64(rng.next() as i64),
            3 => CsilInstr::PushF64(F64Bits::new(rng.next() as f64 / 3.0)),
            4 => CsilInstr::PushStr(rng.below(4) as u32),
            _ => CsilInstr::PushColor {
                r: 1,
                g: 2,
                b: 3,
                t: crate::F64Bits::new(4.0),
            },
        };
        if slot_count == 0 {
            return match rng.below(4) {
                0 => CsilInstr::Jump { target },
                1 => literal,
                2 => CsilInstr::F64Mul,
                _ => CsilInstr::Return,
            };
        }
        match rng.below(20) {
            0 => CsilInstr::Jump { target },
            1 => CsilInstr::JumpIfFalse { target },
            2 => CsilInstr::JumpIfExact {
                on: rng.below(2) == 0,
                target,
            },
            3 => CsilInstr::JumpIfSlotWritten { slot, target },
            4..=7 => CsilInstr::StoreUserSlotCurrent { slot },
            8..=10 => CsilInstr::ReadUserSlot {
                slot,
                offset: u32::from(rng.below(5) == 0),
            },
            11 => CsilInstr::ReadUserSlotDynamic { slot },
            12 => CsilInstr::WindowMean { slot, length: 3 },
            13..=15 => literal,
            16 => CsilInstr::ReadBuiltin {
                source: pinewasm_hir::BuiltinSeries::Close,
                offset: 0,
            },
            17 => CsilInstr::F64Mul,
            18 => CsilInstr::Pop,
            _ => CsilInstr::Return,
        }
    }

    /// Random admissible programs whose passes erased at least one pair.
    fn sweep(seed: u64, programs: usize) -> Vec<(Run, Vec<crate::SlotDef>)> {
        let mut rng = Rng(seed);
        let mut runs = Vec::new();
        for _ in 0..programs {
            let slot_count = rng.below(5) as usize;
            let flags: Vec<_> = (0..slot_count)
                .map(|_| (rng.below(6) == 0, rng.below(12) == 0))
                .collect();
            let slots = slot_defs(&flags);
            let len = rng.below(16) as usize;
            let code: Vec<_> = (0..len)
                .map(|_| instruction(&mut rng, slot_count as u64, len as u64))
                .collect();
            let spans = spans_for(len);
            for (_, pass, keep) in PASSES {
                let run = stage((pass, keep), &code, &spans, &slots);
                runs.push((run, slots.clone()));
            }
        }
        runs
    }

    #[test]
    fn accepts_every_pass_output_in_random_sweep() {
        let (mut pairs, mut rewritten) = (0, 0);
        for (run, slots) in sweep(0x9E37_79B9_7F4A_7C15, 40_000) {
            assert_eq!(
                run.check(&slots),
                Ok(()),
                "before={:?} after={:?} starts={:?}",
                run.before,
                run.after,
                run.starts
            );
            pairs += run.starts.len();
            rewritten += usize::from(!run.starts.is_empty());
        }
        eprintln!("sweep: {rewritten} rewritten programs, {pairs} certified pairs accepted");
        assert!(rewritten > 1_000, "too few rewrites: {rewritten}");
    }

    fn corpus_mirs() -> Vec<(std::path::PathBuf, MirBody)> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut files = Vec::new();
        for dir in [
            "tests/compile-pass",
            "tests/compile-fail",
            "tests/regression",
            "benchmarks/workloads",
        ] {
            for entry in std::fs::read_dir(root.join(dir)).expect("fixture directory") {
                let path = entry.expect("fixture entry").path();
                if path.extension().is_some_and(|ext| ext == "pine") {
                    files.push(path);
                }
            }
        }
        files.sort();
        let mut mirs = Vec::new();
        for path in files {
            let source = std::fs::read_to_string(&path).expect("fixture file");
            let parsed = pinewasm_parse::parse(&source);
            if pinewasm_span::has_errors(&parsed.diagnostics) {
                continue;
            }
            let analysis = pinewasm_semantics::analyze(&parsed.program);
            if pinewasm_span::has_errors(&analysis.diagnostics) {
                continue;
            }
            mirs.push((path, pinewasm_mir::lower(&analysis.hir)));
        }
        mirs
    }

    /// The code and slots each checked pass sees on a real body: the stackified
    /// and patched code, then the dead-literal pass's output.
    fn pass_inputs(mir: &MirBody) -> (Vec<CsilInstr>, Vec<Span>) {
        let mut ctx = stackify(&mir.blocks).expect("stackify");
        assert_eq!(
            apply_fixups(&mut ctx.code, &ctx.block_starts, &ctx.fixups),
            None
        );
        (ctx.code, ctx.source_map)
    }

    #[test]
    fn corpus_programs_validate() {
        let (mut bodies, mut pairs) = (0, [0usize; 3]);
        for (path, mir) in corpus_mirs() {
            let validated = lower_mir_validated(&mir);
            assert_eq!(validated, lower_mir(&mir), "{}", path.display());
            assert!(validated.is_ok(), "{}", path.display());
            let (mut code, mut spans) = pass_inputs(&mir);
            let slots = &mir.metadata.series_slots;
            for (index, (_, pass, keep)) in PASSES.iter().enumerate() {
                let run = stage((*pass, *keep), &code, &spans, slots);
                assert_eq!(run.check(slots), Ok(()), "{}", path.display());
                pairs[index] += run.starts.len();
                code = run.after;
                spans = run.after_spans;
            }
            let run = alias_stage(&code, &spans, slots);
            assert_eq!(run.check(slots), Ok(()), "{}", path.display());
            pairs[2] += run.pairs.len();
            bodies += 1;
        }
        eprintln!(
            "fixtures: {bodies} bodies validated; certified pairs: dead {} single {} alias {}",
            pairs[0], pairs[1], pairs[2]
        );
        assert!(bodies >= 40, "only {bodies} fixture bodies");
        assert!(
            pairs.iter().all(|&count| count > 0),
            "passes never fired: {pairs:?}"
        );
    }

    /// The flow-alias and jump-threading stages fire on the fixture programs
    /// (their checkers run on every fixture body in `corpus_programs_validate`).
    #[test]
    fn corpus_flow_alias_and_jump_threading_fire() {
        let (mut aliased, mut erased_jumps) = (0usize, 0usize);
        for (_, mir) in corpus_mirs() {
            let (mut code, mut spans) = pass_inputs(&mir);
            let certs = optimize_stages(&mut code, &mut spans, &mir.metadata.series_slots);
            for cert in certs {
                match cert {
                    StageCert::FlowAliases(rounds) => {
                        for (keep, _) in rounds {
                            aliased += keep.iter().filter(|kept| !**kept).count() / 2;
                        }
                    }
                    StageCert::Threads((keep, _)) => {
                        erased_jumps += keep.iter().filter(|kept| !**kept).count();
                    }
                    _ => {}
                }
            }
        }
        eprintln!("fixtures: {aliased} copies aliased, {erased_jumps} jumps erased");
        assert!(aliased > 100, "flow alias removed only {aliased} copies");
        assert!(
            erased_jumps > 100,
            "threading erased only {erased_jumps} jumps"
        );
    }

    /// `optimize_stages` is the pass sequence (each pass on the previous
    /// pass's output), and each certificate is the analysis of its stage's
    /// input.
    #[test]
    fn corpus_optimize_stages_is_the_pass_sequence() {
        let mut bodies = 0;
        for (path, mir) in corpus_mirs() {
            let (code0, spans0) = pass_inputs(&mir);
            let slots = &mir.metadata.series_slots;
            let (mut code, mut spans) = (code0.clone(), spans0.clone());
            let certs = optimize_stages(&mut code, &mut spans, slots);
            let (mut c, mut s) = (code0.clone(), spans0.clone());
            let dead = dead_constant_store_keep(&c, slots);
            elide_dead_constant_stores(&mut c, &mut s, slots);
            let single = single_use_slot_keep(&c, slots);
            coalesce_single_use_slots(&mut c, &mut s, slots);
            let (alias_keep, aliases) = entry_alias_plan(&c, slots);
            coalesce_entry_aliases(&mut c, &mut s, slots);
            let rounds = flow_alias::flow_alias_rounds(&mut c, &mut s, slots);
            let threads = jump_thread::jump_thread_plan(&c);
            jump_thread::thread_jumps(&mut c, &mut s);
            assert_eq!((&code, &spans), (&c, &s), "{}", path.display());
            assert_eq!(
                certs,
                vec![
                    StageCert::Pairs(dead),
                    StageCert::Pairs(single),
                    StageCert::Aliases(alias_keep, aliases),
                    StageCert::FlowAliases(rounds),
                    StageCert::Threads(threads),
                ],
                "{}",
                path.display()
            );
            bodies += 1;
        }
        assert!(bodies >= 40, "only {bodies} fixture bodies");
    }

    /// Wall time of the validated pipeline against the plain one, over the
    /// fixture programs (run with `--release -- --ignored --nocapture`). The
    /// certificates are the passes' own keep masks, so the validation overhead
    /// is the input copies, the certificate read-back and the three checkers.
    #[test]
    #[ignore = "timing report, not a correctness check"]
    fn corpus_checker_timing() {
        let mirs = corpus_mirs();
        let rounds = 50;
        let (mut plain_time, mut validated_time) = (Duration::ZERO, Duration::ZERO);
        let (mut instructions, mut largest) = (0usize, 0usize);
        let mut worst = (
            Duration::ZERO,
            Duration::ZERO,
            0usize,
            std::path::PathBuf::new(),
        );
        for (path, mir) in &mirs {
            let (code0, spans0) = pass_inputs(mir);
            let slots = &mir.metadata.series_slots;
            largest = largest.max(code0.len());
            // Per body, the fastest of `rounds` runs of each pipeline (robust
            // against a loaded machine), run alternately.
            let (mut plain_best, mut validated_best) = (Duration::MAX, Duration::MAX);
            for _ in 0..rounds {
                let (mut code, mut spans) = (code0.clone(), spans0.clone());
                let started = Instant::now();
                optimize(&mut code, &mut spans, slots);
                plain_best = plain_best.min(started.elapsed());
                let (mut checked, mut checked_spans) = (code0.clone(), spans0.clone());
                let started = Instant::now();
                assert_eq!(
                    optimize_validated(&mut checked, &mut checked_spans, slots),
                    Ok(())
                );
                validated_best = validated_best.min(started.elapsed());
                assert_eq!((&checked, &checked_spans), (&code, &spans));
            }
            plain_time += plain_best;
            validated_time += validated_best;
            instructions += code0.len();
            if code0.len() > worst.2 {
                worst = (plain_best, validated_best, code0.len(), path.clone());
            }
        }
        let overhead = validated_time.saturating_sub(plain_time);
        eprintln!(
            "timing over {} fixture bodies, fastest of {rounds} runs each ({instructions} input instructions, largest body {largest}):",
            mirs.len(),
        );
        eprintln!("  passes (dead+single+alias): {plain_time:?}");
        eprintln!("  validated pipeline:         {validated_time:?}");
        eprintln!(
            "  validation overhead:        {overhead:?} ({:.2}x the passes, {:.1} ns/instruction)",
            overhead.as_secs_f64() / plain_time.as_secs_f64(),
            overhead.as_nanos() as f64 / instructions as f64
        );
        eprintln!(
            "  largest body: passes {:?}, validated {:?} ({} instructions, {})",
            worst.0,
            worst.1,
            worst.2,
            worst.3.display()
        );
    }

    /// A small program on which both passes fire and that has a branch.
    fn fixture() -> (Vec<CsilInstr>, Vec<crate::SlotDef>) {
        let close = CsilInstr::ReadBuiltin {
            source: pinewasm_hir::BuiltinSeries::Close,
            offset: 0,
        };
        (
            vec![
                CsilInstr::PushStr(0),                          // 0 dead pair (0, 1)
                CsilInstr::StoreUserSlotCurrent { slot: 0 },    // 1
                close.clone(),                                  // 2
                CsilInstr::StoreUserSlotCurrent { slot: 1 },    // 3 single pair (3, 4)
                CsilInstr::ReadUserSlot { slot: 1, offset: 0 }, // 4
                CsilInstr::JumpIfFalse { target: 8 },           // 5
                CsilInstr::PushF64(F64Bits::new(0.0)),          // 6
                CsilInstr::PlotF64 { plot_slot: 0 },            // 7
                close,                                          // 8
                CsilInstr::PushF64(F64Bits::new(-0.0)),         // 9
                CsilInstr::F64Mul,                              // 10
                CsilInstr::PlotF64 { plot_slot: 1 },            // 11
                CsilInstr::Jump { target: 12 },                 // 12
                CsilInstr::Return,                              // 13
            ],
            plain(2),
        )
    }

    /// The composed dead+single rewrite of [`fixture`], as one pair-erasure
    /// run (both kinds certified together).
    fn fixture_run() -> (Run, Vec<crate::SlotDef>) {
        let (code, slots) = fixture();
        let spans = spans_for(code.len());
        let dead = stage(
            (elide_dead_constant_stores, dead_constant_store_keep),
            &code,
            &spans,
            &slots,
        );
        assert_eq!(dead.starts, vec![0]);
        let single = stage(
            (coalesce_single_use_slots, single_use_slot_keep),
            &code,
            &spans,
            &slots,
        );
        assert_eq!(single.starts, vec![3]);
        let mut after = code.clone();
        let mut after_spans = spans.clone();
        let keep: Vec<bool> = (0..code.len())
            .map(|i| ![0, 1, 3, 4].contains(&i))
            .collect();
        retain_instructions(&mut after, &mut after_spans, &keep);
        let run = Run {
            before: code,
            before_spans: spans,
            after,
            after_spans,
            starts: vec![0, 3],
        };
        assert_eq!(run.check(&slots), Ok(()));
        (run, slots)
    }

    fn rejected(run: &Run, slots: &[crate::SlotDef]) -> PairCheckError {
        run.check(slots).expect_err("mutation must be rejected")
    }

    #[test]
    fn rejects_mutated_outputs() {
        let (run, slots) = fixture_run();
        let n = run.after.len();
        // Swap every pair of adjacent distinct output instructions.
        let mut swaps = 0;
        for i in 0..n - 1 {
            if run.after[i] != run.after[i + 1] {
                let mut bad = Run {
                    after: run.after.clone(),
                    ..run.clone_run()
                };
                bad.after.swap(i, i + 1);
                assert_eq!(
                    rejected(&bad, &slots),
                    PairCheckError::OutputMismatch,
                    "swap {i}"
                );
                swaps += 1;
            }
        }
        assert!(swaps >= 6);
        // Drop any one output instruction (and its span).
        for i in 0..n {
            let mut bad = run.clone_run();
            bad.after.remove(i);
            bad.after_spans.remove(i);
            assert_eq!(
                rejected(&bad, &slots),
                PairCheckError::OutputLength,
                "drop {i}"
            );
        }
        // Retarget each output jump to every other address.
        let mut retargets = 0;
        for i in 0..n {
            for moved in 0..=n as u32 + 1 {
                let mut bad = run.clone_run();
                match &mut bad.after[i] {
                    CsilInstr::Jump { target } | CsilInstr::JumpIfFalse { target } => {
                        if *target == moved {
                            continue;
                        }
                        *target = moved;
                    }
                    _ => continue,
                }
                assert_eq!(rejected(&bad, &slots), PairCheckError::OutputMismatch);
                retargets += 1;
            }
        }
        assert!(retargets >= 20);
        // Flip the sign of a zero literal: derived `==` calls these equal.
        let zero = run
            .after
            .iter()
            .position(|i| matches!(i, CsilInstr::PushF64(_)))
            .expect("a float literal");
        let mut bad = run.clone_run();
        let flipped = match &bad.after[zero] {
            CsilInstr::PushF64(bits) => CsilInstr::PushF64(F64Bits::new(-bits.get())),
            _ => unreachable!(),
        };
        assert_eq!(
            flipped, bad.after[zero],
            "IEEE equality identifies 0.0 and -0.0"
        );
        bad.after[zero] = flipped;
        assert_eq!(rejected(&bad, &slots), PairCheckError::OutputMismatch);
        // A span that does not follow its instruction.
        let mut bad = run.clone_run();
        bad.after_spans.swap(0, 1);
        assert_eq!(rejected(&bad, &slots), PairCheckError::SpanMismatch);
        let mut bad = run.clone_run();
        bad.after_spans.pop();
        assert_eq!(rejected(&bad, &slots), PairCheckError::OutputSpanLength);
    }

    #[test]
    fn rejects_bad_certificates_and_alias_breaks() {
        let (run, slots) = fixture_run();
        let with_starts = |starts: Vec<u32>| Run {
            starts,
            ..run.clone_run()
        };
        // Misaligned or missing pairs.
        assert_eq!(
            rejected(&with_starts(vec![1, 3]), &slots),
            PairCheckError::NotErasablePair
        );
        assert_eq!(
            rejected(&with_starts(vec![3, 0]), &slots),
            PairCheckError::CertificateOrder
        );
        assert_eq!(
            rejected(&with_starts(vec![0, 1]), &slots),
            PairCheckError::CertificateOrder
        );
        assert_eq!(
            rejected(&with_starts(vec![0, 13]), &slots),
            PairCheckError::CertificateRange
        );
        assert_eq!(
            rejected(&with_starts(vec![0, 99]), &slots),
            PairCheckError::CertificateRange
        );
        assert_eq!(
            rejected(&with_starts(vec![0]), &slots),
            PairCheckError::OutputLength
        );
        assert_eq!(
            rejected(&with_starts(vec![]), &slots),
            PairCheckError::OutputLength
        );
        // Persistent / varip / undeclared slots.
        for flags in [(true, false), (true, true)] {
            let persistent = slot_defs(&[flags, (false, false)]);
            assert_eq!(rejected(&run, &persistent), PairCheckError::PersistentSlot);
        }
        assert_eq!(rejected(&run, &plain(1)), PairCheckError::SlotOutOfRange);
        // Alias breaks: the erased slot is observed or accessed elsewhere. Each
        // mutation is applied to the input and, relocated, to the output, so the
        // output still matches and only the slot condition fails.
        let observers = [
            (
                0,
                CsilInstr::ReadUserSlot { slot: 0, offset: 1 },
                PairCheckError::SlotObserved,
            ),
            (
                0,
                CsilInstr::ReadUserSlotDynamic { slot: 0 },
                PairCheckError::SlotObserved,
            ),
            (
                0,
                CsilInstr::WindowMean { slot: 0, length: 2 },
                PairCheckError::SlotObserved,
            ),
            (
                1,
                CsilInstr::ReadUserSlot { slot: 1, offset: 0 },
                PairCheckError::SlotNotSingleUse,
            ),
            (
                1,
                CsilInstr::ReadUserSlot { slot: 1, offset: 2 },
                PairCheckError::SlotNotSingleUse,
            ),
            (
                1,
                CsilInstr::StoreUserSlotCurrent { slot: 1 },
                PairCheckError::SlotNotSingleUse,
            ),
            (
                1,
                CsilInstr::WindowMean { slot: 1, length: 2 },
                PairCheckError::SlotNotSingleUse,
            ),
        ];
        for (_, observer, error) in observers {
            // Replace the kept `PlotF64 { plot_slot: 0 }` (input 7, output 3).
            let mut bad = run.clone_run();
            bad.before[7] = observer.clone();
            bad.after[3] = observer.clone();
            assert_eq!(rejected(&bad, &slots), error, "{observer:?}");
        }
        // A branch into the middle of the single-use pair.
        let mut bad = run.clone_run();
        bad.before[12] = CsilInstr::Jump { target: 4 };
        bad.after[8] = CsilInstr::Jump { target: 3 };
        assert_eq!(rejected(&bad, &slots), PairCheckError::TargetIntoPair);
        // A branch past the end.
        let mut bad = run.clone_run();
        bad.before[12] = CsilInstr::Jump { target: 15 };
        assert_eq!(rejected(&bad, &slots), PairCheckError::TargetOutOfRange);
        // A branch to the FIRST instruction of a pair is fine (it relocates past it).
        let mut ok = run.clone_run();
        ok.before[12] = CsilInstr::Jump { target: 3 };
        ok.after[8] = CsilInstr::Jump { target: 1 };
        assert_eq!(ok.check(&slots), Ok(()));
        // Misaligned spans on the input.
        let mut bad = run.clone_run();
        bad.before_spans.pop();
        assert_eq!(rejected(&bad, &slots), PairCheckError::SpanLength);
    }

    impl Run {
        fn clone_run(&self) -> Run {
            Run {
                before: self.before.clone(),
                before_spans: self.before_spans.clone(),
                after: self.after.clone(),
                after_spans: self.after_spans.clone(),
                starts: self.starts.clone(),
            }
        }
    }

    /// Random single-point mutations of real sweep outputs: whenever the mutated
    /// output differs from the true output, the checker rejects it.
    #[test]
    fn random_output_mutations_are_rejected() {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        let mut rejected_count = 0;
        for (run, slots) in sweep(0xC0FF_EE00_1234_5678, 6_000) {
            if run.after.is_empty() {
                continue;
            }
            let mut bad = run.clone_run();
            let i = rng.below(bad.after.len() as u64) as usize;
            match rng.below(4) {
                0 => {
                    let j = rng.below(bad.after.len() as u64) as usize;
                    bad.after.swap(i, j);
                }
                1 => {
                    bad.after.remove(i);
                    bad.after_spans.remove(i);
                }
                2 => {
                    bad.after[i] =
                        instruction(&mut rng, slots.len() as u64, run.after.len() as u64);
                }
                _ => bad.after_spans[i].end += 1,
            }
            let changed = (0..bad.after.len().max(run.after.len())).any(|k| {
                match (bad.after.get(k), run.after.get(k)) {
                    (Some(a), Some(b)) => !instr_exact_eq(a, b),
                    _ => true,
                }
            }) || bad.after_spans != run.after_spans;
            if changed {
                assert!(
                    bad.check(&slots).is_err(),
                    "accepted {bad:?}",
                    bad = bad.after
                );
                rejected_count += 1;
            } else {
                assert_eq!(bad.check(&slots), Ok(()));
            }
        }
        assert!(rejected_count > 5_000, "{rejected_count}");
    }

    #[test]
    fn exact_eq_compares_floats_by_bits() {
        let push = |x: f64| CsilInstr::PushF64(F64Bits::new(x));
        assert!(instr_exact_eq(&push(1.5), &push(1.5)));
        assert!(!instr_exact_eq(&push(0.0), &push(-0.0)));
        assert_eq!(push(0.0), push(-0.0), "derived equality is IEEE equality");
        assert!(instr_exact_eq(&push(f64::NAN), &push(f64::NAN)));
        assert_ne!(
            push(f64::NAN),
            push(f64::NAN),
            "derived equality: NaN != NaN"
        );
        let other_nan = f64::from_bits(f64::NAN.to_bits() ^ 1);
        assert!(!instr_exact_eq(&push(f64::NAN), &push(other_nan)));
        let expr = |x: f64| CsilInstr::ReadSecurityExpr {
            expr: HtfExpr::Binary {
                op: pinewasm_hir::BinOp::Add,
                int_result: false,
                lhs: Box::new(HtfExpr::Const(x)),
                rhs: Box::new(HtfExpr::Neg(Box::new(HtfExpr::Source(
                    pinewasm_hir::BuiltinSeries::Close,
                )))),
            },
            htf_seconds: 86_400,
            timeframe_expr: None,
            calendar_months: None,
            gaps_on: false,
            lookahead_on: false,
        };
        assert!(instr_exact_eq(&expr(2.0), &expr(2.0)));
        assert!(instr_exact_eq(&expr(f64::NAN), &expr(f64::NAN)));
        assert!(!instr_exact_eq(&expr(0.0), &expr(-0.0)));
        assert_eq!(expr(0.0), expr(-0.0));
        assert!(!instr_exact_eq(&expr(1.0), &push(1.0)));
    }

    #[test]
    fn exact_eq_agrees_with_derived_equality_off_floats() {
        let mut rng = Rng(0x1234_5678_9ABC_DEF1);
        for _ in 0..20_000 {
            let a = instruction(&mut rng, 3, 5);
            let b = instruction(&mut rng, 3, 5);
            let bits = |i: &CsilInstr| match i {
                CsilInstr::PushF64(x) => Some(x.to_bits()),
                _ => None,
            };
            let expected = match (bits(&a), bits(&b)) {
                (Some(x), Some(y)) => x == y,
                _ => a == b,
            };
            assert_eq!(instr_exact_eq(&a, &b), expected, "{a:?} {b:?}");
            assert!(instr_exact_eq(&a, &a));
        }
        let text = |s: &str| CsilInstr::ReadRequest {
            func: pinewasm_hir::RequestFunc::Seed,
            selector: s.to_string(),
        };
        assert!(instr_exact_eq(&text("a"), &text("a")));
        assert!(!instr_exact_eq(&text("a"), &text("b")));
    }

    #[test]
    fn pair_starts_groups_erasures_in_pairs() {
        let keep = |erased: &[usize], len: usize| -> Vec<bool> {
            (0..len).map(|i| !erased.contains(&i)).collect()
        };
        assert_eq!(
            crate::translation_validation::pair_starts(&keep(&[1, 2], 6)),
            vec![1]
        );
        assert_eq!(
            crate::translation_validation::pair_starts(&keep(&[0, 1, 3, 4], 6)),
            vec![0, 3]
        );
        assert_eq!(
            crate::translation_validation::pair_starts(&keep(&[], 3)),
            Vec::<u32>::new()
        );
        assert_eq!(
            crate::translation_validation::pair_starts(&keep(&[0, 1], 2)),
            vec![0]
        );
        // A lone trailing erasure still opens a (then out-of-range) pair.
        assert_eq!(
            crate::translation_validation::pair_starts(&keep(&[2], 3)),
            vec![2]
        );
    }

    /// `check_stage` rejects a stage output that does not match its
    /// certificate.
    #[test]
    fn check_stage_rejects_a_broken_output() {
        let (code, slots) = fixture();
        let spans = spans_for(code.len());
        let keep = single_use_slot_keep(&code, &slots);
        let (mut after, mut after_spans) = (code.clone(), spans.clone());
        retain_instructions(&mut after, &mut after_spans, &keep);
        let cert = StageCert::Pairs(keep);
        assert_eq!(
            check_stage(&code, &spans, &after, &after_spans, &cert, &slots),
            Ok(())
        );
        after.swap(0, 1);
        assert_eq!(
            check_stage(&code, &spans, &after, &after_spans, &cert, &slots),
            Err(PairCheckError::OutputMismatch)
        );
    }

    // ---- The entry-alias checker ----

    /// One validated entry-alias run: input, output, certificate.
    struct AliasRun {
        before: Vec<CsilInstr>,
        before_spans: Vec<Span>,
        after: Vec<CsilInstr>,
        after_spans: Vec<Span>,
        pairs: Vec<AliasPair>,
        table: Vec<u32>,
    }

    impl AliasRun {
        fn check(&self, slots: &[crate::SlotDef]) -> Result<(), PairCheckError> {
            crate::translation_validation::check_alias(
                &self.before,
                &self.before_spans,
                &self.after,
                &self.after_spans,
                &self.pairs,
                &self.table,
                slots,
            )
        }

        fn clone_run(&self) -> AliasRun {
            AliasRun {
                before: self.before.clone(),
                before_spans: self.before_spans.clone(),
                after: self.after.clone(),
                after_spans: self.after_spans.clone(),
                pairs: self.pairs.clone(),
                table: self.table.clone(),
            }
        }
    }

    /// The alias stage as the validated pipeline runs it, also checked against
    /// the pass itself.
    fn alias_stage(code: &[CsilInstr], spans: &[Span], slots: &[crate::SlotDef]) -> AliasRun {
        let before = code.to_vec();
        let (keep, table) = entry_alias_plan(&before, slots);
        let (mut after, mut after_spans) = (before.clone(), spans.to_vec());
        rename_entry_aliases(&mut after, &table);
        retain_instructions(&mut after, &mut after_spans, &keep);
        let (mut by_pass, mut by_pass_spans) = (before.clone(), spans.to_vec());
        coalesce_entry_aliases(&mut by_pass, &mut by_pass_spans, slots);
        assert_eq!((&by_pass, &by_pass_spans), (&after, &after_spans));
        AliasRun {
            pairs: crate::translation_validation::alias_pairs(&before, &keep),
            before,
            before_spans: spans.to_vec(),
            after,
            after_spans,
            table,
        }
    }

    use crate::translation_validation::AliasPair;

    /// Random admissible programs shaped for the alias pass: a straight-line
    /// prefix of definitions and copies (with occasional redefinitions,
    /// observers and early reads), then a random tail with branches.
    fn alias_sweep(seed: u64, programs: usize) -> Vec<(AliasRun, Vec<crate::SlotDef>)> {
        let mut rng = Rng(seed);
        let mut runs = Vec::new();
        for _ in 0..programs {
            let slot_count = 1 + rng.below(6) as usize;
            let flags: Vec<_> = (0..slot_count)
                .map(|_| (rng.below(8) == 0, rng.below(16) == 0))
                .collect();
            let slots = slot_defs(&flags);
            let prefix = rng.below(10) as usize;
            let tail = rng.below(10) as usize;
            let len = (3 * prefix + tail) as u64;
            let mut code = Vec::new();
            for _ in 0..prefix {
                let a = rng.below(slot_count as u64) as u32;
                let b = rng.below(slot_count as u64) as u32;
                match rng.below(8) {
                    0..=2 => {
                        code.push(CsilInstr::ReadBuiltin {
                            source: pinewasm_hir::BuiltinSeries::Close,
                            offset: 0,
                        });
                        code.push(CsilInstr::StoreUserSlotCurrent { slot: a });
                    }
                    3..=6 => {
                        code.push(CsilInstr::ReadUserSlot { slot: a, offset: 0 });
                        code.push(CsilInstr::StoreUserSlotCurrent { slot: b });
                    }
                    _ => code.push(instruction(&mut rng, slot_count as u64, len)),
                }
            }
            for _ in 0..tail {
                code.push(instruction(&mut rng, slot_count as u64, len));
            }
            // Targets were drawn against the planned length; keep them in range.
            let n = code.len() as u32;
            for instr in &mut code {
                match instr {
                    CsilInstr::Jump { target }
                    | CsilInstr::JumpIfFalse { target }
                    | CsilInstr::JumpIfExact { target, .. }
                    | CsilInstr::JumpIfSlotWritten { target, .. } => *target %= n + 1,
                    _ => {}
                }
            }
            let spans = spans_for(code.len());
            runs.push((alias_stage(&code, &spans, &slots), slots));
        }
        runs
    }

    #[test]
    fn alias_checker_accepts_every_pass_output_in_random_sweep() {
        let (mut pairs, mut rewritten) = (0, 0);
        for (run, slots) in alias_sweep(0x5851_F42D_4C95_7F2D, 40_000) {
            assert_eq!(
                run.check(&slots),
                Ok(()),
                "before={:?} after={:?} pairs={:?} table={:?}",
                run.before,
                run.after,
                run.pairs,
                run.table
            );
            pairs += run.pairs.len();
            rewritten += usize::from(!run.pairs.is_empty());
        }
        eprintln!("alias sweep: {rewritten} rewritten programs, {pairs} certified copies accepted");
        assert!(rewritten > 500, "too few rewrites: {rewritten}");
    }

    /// A program with an alias chain `c = b = a`, a back-edge onto an erased
    /// read, a history read of the source and a kept branch.
    fn alias_fixture() -> (AliasRun, Vec<crate::SlotDef>) {
        let close = CsilInstr::ReadBuiltin {
            source: pinewasm_hir::BuiltinSeries::Close,
            offset: 0,
        };
        let code = vec![
            close,                                          // 0
            CsilInstr::StoreUserSlotCurrent { slot: 0 },    // 1 a
            CsilInstr::ReadUserSlot { slot: 0, offset: 0 }, // 2 copy a -> b
            CsilInstr::StoreUserSlotCurrent { slot: 1 },    // 3
            CsilInstr::ReadUserSlot { slot: 1, offset: 0 }, // 4 copy b -> c
            CsilInstr::StoreUserSlotCurrent { slot: 2 },    // 5
            CsilInstr::ReadUserSlot { slot: 2, offset: 0 }, // 6 read c
            CsilInstr::PlotF64 { plot_slot: 0 },            // 7
            CsilInstr::ReadUserSlot { slot: 0, offset: 1 }, // 8 a[1]
            CsilInstr::JumpIfFalse { target: 11 },          // 9
            CsilInstr::Jump { target: 2 },                  // 10 back-edge onto a read
            CsilInstr::PushF64(F64Bits::new(-0.0)),         // 11
            CsilInstr::PlotF64 { plot_slot: 1 },            // 12
            CsilInstr::Return,                              // 13
        ];
        let slots = plain(3);
        let spans = spans_for(code.len());
        let run = alias_stage(&code, &spans, &slots);
        assert_eq!(
            run.pairs,
            vec![
                AliasPair {
                    store: 3,
                    src: 0,
                    dst: 1
                },
                AliasPair {
                    store: 5,
                    src: 1,
                    dst: 2
                },
            ]
        );
        assert_eq!(run.table, vec![0, 0, 0]);
        assert_eq!(run.after[2], CsilInstr::ReadUserSlot { slot: 0, offset: 0 });
        assert_eq!(run.check(&slots), Ok(()));
        (run, slots)
    }

    fn alias_rejected(run: &AliasRun, slots: &[crate::SlotDef]) -> PairCheckError {
        run.check(slots).expect_err("mutation must be rejected")
    }

    #[test]
    fn alias_checker_rejects_mutated_outputs() {
        let (run, slots) = alias_fixture();
        let n = run.after.len();
        for i in 0..n - 1 {
            if !instr_exact_eq(&run.after[i], &run.after[i + 1]) {
                let mut bad = run.clone_run();
                bad.after.swap(i, i + 1);
                assert_eq!(
                    alias_rejected(&bad, &slots),
                    PairCheckError::OutputMismatch,
                    "swap {i}"
                );
            }
        }
        for i in 0..n {
            let mut bad = run.clone_run();
            bad.after.remove(i);
            bad.after_spans.remove(i);
            assert_eq!(alias_rejected(&bad, &slots), PairCheckError::OutputLength);
        }
        // An un-renamed read, a read renamed to the wrong slot, a renamed
        // history read.
        for (index, instr) in [
            (2, CsilInstr::ReadUserSlot { slot: 2, offset: 0 }),
            (2, CsilInstr::ReadUserSlot { slot: 1, offset: 0 }),
            (4, CsilInstr::ReadUserSlot { slot: 1, offset: 1 }),
        ] {
            let mut bad = run.clone_run();
            bad.after[index] = instr;
            assert_eq!(alias_rejected(&bad, &slots), PairCheckError::OutputMismatch);
        }
        // A back-edge that is not relocated.
        let mut bad = run.clone_run();
        bad.after[6] = CsilInstr::Jump { target: 3 };
        assert_eq!(alias_rejected(&bad, &slots), PairCheckError::OutputMismatch);
        // A zero's sign flip.
        let mut bad = run.clone_run();
        bad.after[7] = CsilInstr::PushF64(F64Bits::new(0.0));
        assert_eq!(alias_rejected(&bad, &slots), PairCheckError::OutputMismatch);
        // A misplaced span.
        let mut bad = run.clone_run();
        bad.after_spans.swap(0, 1);
        assert_eq!(alias_rejected(&bad, &slots), PairCheckError::SpanMismatch);
    }

    #[test]
    fn alias_checker_rejects_bad_certificates() {
        let (run, slots) = alias_fixture();
        let pair = |store, src, dst| AliasPair { store, src, dst };
        let with = |pairs: Vec<AliasPair>, table: Vec<u32>| AliasRun {
            pairs,
            table,
            ..run.clone_run()
        };
        let cases = [
            (
                vec![pair(0, 0, 1)],
                vec![0, 0, 0],
                PairCheckError::CertificateRange,
            ),
            (
                vec![pair(14, 0, 1)],
                vec![0, 0, 0],
                PairCheckError::CertificateRange,
            ),
            (
                vec![pair(4, 0, 1)],
                vec![0, 0, 0],
                PairCheckError::NotAliasPair,
            ),
            (
                vec![pair(3, 1, 1)],
                vec![0, 0, 0],
                PairCheckError::NotAliasPair,
            ),
            (
                vec![pair(3, 0, 1), pair(3, 0, 1)],
                vec![0, 0, 0],
                PairCheckError::CertificateOrder,
            ),
            // The table must map each destination like its source...
            (
                vec![pair(3, 0, 1), pair(5, 1, 2)],
                vec![0, 0, 1],
                PairCheckError::TableMismatch,
            ),
            // ... and fix every other slot.
            (
                vec![pair(3, 0, 1)],
                vec![0, 0, 0],
                PairCheckError::TableMismatch,
            ),
            (vec![pair(3, 0, 1)], vec![0, 0], PairCheckError::TableLength),
            // Certifying only the first copy leaves the output unexplained.
            (
                vec![pair(3, 0, 1)],
                vec![0, 0, 2],
                PairCheckError::OutputLength,
            ),
        ];
        for (pairs, table, error) in cases {
            let bad = with(pairs.clone(), table.clone());
            assert_eq!(alias_rejected(&bad, &slots), error, "{pairs:?} {table:?}");
        }
        // Slot conditions, each on an input mutation that keeps the copy's shape.
        let mutate = |index: usize, instr: CsilInstr| {
            let mut bad = run.clone_run();
            bad.before[index] = instr;
            bad
        };
        let cases = [
            // A branch before the copy ends the prefix.
            (
                0,
                CsilInstr::Jump { target: 1 },
                PairCheckError::PairOutsidePrefix,
            ),
            // A branch into the store.
            (
                10,
                CsilInstr::Jump { target: 3 },
                PairCheckError::TargetIntoPair,
            ),
            // `a` defined twice, or `b` defined elsewhere, read early, observed.
            (
                12,
                CsilInstr::StoreUserSlotCurrent { slot: 0 },
                PairCheckError::SourceNotDefined,
            ),
            (
                12,
                CsilInstr::StoreUserSlotCurrent { slot: 1 },
                PairCheckError::DestinationNotFresh,
            ),
            (
                12,
                CsilInstr::ReadUserSlot { slot: 1, offset: 3 },
                PairCheckError::SlotObserved,
            ),
            (
                12,
                CsilInstr::ReadUserSlotDynamic { slot: 2 },
                PairCheckError::SlotObserved,
            ),
            (
                12,
                CsilInstr::WindowMean { slot: 1, length: 2 },
                PairCheckError::SlotObserved,
            ),
            (
                9,
                CsilInstr::JumpIfSlotWritten {
                    slot: 2,
                    target: 11,
                },
                PairCheckError::SlotObserved,
            ),
            (
                12,
                CsilInstr::ReadUserSlot { slot: 3, offset: 0 },
                PairCheckError::OperandOutOfRange,
            ),
            (
                9,
                CsilInstr::JumpIfFalse { target: 15 },
                PairCheckError::TargetOutOfRange,
            ),
        ];
        for (index, instr, error) in cases {
            let bad = mutate(index, instr.clone());
            assert_eq!(alias_rejected(&bad, &slots), error, "{index}: {instr:?}");
        }
        // The source's single store must come strictly before the copy.
        let mut bad = run.clone_run();
        bad.before[0] = CsilInstr::PushNa;
        bad.before[1] = CsilInstr::Pop;
        bad.before[12] = CsilInstr::StoreUserSlotCurrent { slot: 0 };
        assert_eq!(
            alias_rejected(&bad, &slots),
            PairCheckError::SourceNotDefined
        );
        // A destination read before its copy.
        let mut bad = run.clone_run();
        bad.before[0] = CsilInstr::ReadUserSlot { slot: 2, offset: 0 };
        assert_eq!(
            alias_rejected(&bad, &slots),
            PairCheckError::DestinationNotFresh
        );
        // var / varip destinations.
        for flags in [(true, false), (false, true)] {
            let persistent = slot_defs(&[(false, false), flags, (false, false)]);
            assert_eq!(
                alias_rejected(&run, &persistent),
                PairCheckError::PersistentSlot
            );
        }
        assert_eq!(alias_rejected(&run, &plain(2)), PairCheckError::TableLength);
        let mut bad = run.clone_run();
        bad.before_spans.pop();
        assert_eq!(alias_rejected(&bad, &slots), PairCheckError::SpanLength);
        let mut bad = run.clone_run();
        bad.after_spans.pop();
        assert_eq!(
            alias_rejected(&bad, &slots),
            PairCheckError::OutputSpanLength
        );
    }

    /// Random single-point mutations of real alias outputs: whenever the
    /// mutated output differs from the true one, the checker rejects it.
    #[test]
    fn alias_random_output_mutations_are_rejected() {
        let mut rng = Rng(0x94D0_49BB_1331_11EB);
        let mut rejected_count = 0;
        for (run, slots) in alias_sweep(0xBF58_476D_1CE4_E5B9, 8_000) {
            if run.after.is_empty() {
                continue;
            }
            let mut bad = run.clone_run();
            let i = rng.below(bad.after.len() as u64) as usize;
            match rng.below(4) {
                0 => {
                    let j = rng.below(bad.after.len() as u64) as usize;
                    bad.after.swap(i, j);
                }
                1 => {
                    bad.after.remove(i);
                    bad.after_spans.remove(i);
                }
                2 => {
                    bad.after[i] =
                        instruction(&mut rng, slots.len() as u64, run.after.len() as u64);
                }
                _ => bad.after_spans[i].end += 1,
            }
            let changed = (0..bad.after.len().max(run.after.len())).any(|k| {
                match (bad.after.get(k), run.after.get(k)) {
                    (Some(a), Some(b)) => !instr_exact_eq(a, b),
                    _ => true,
                }
            }) || bad.after_spans != run.after_spans;
            if changed {
                assert!(bad.check(&slots).is_err(), "accepted {:?}", bad.after);
                rejected_count += 1;
            } else {
                assert_eq!(bad.check(&slots), Ok(()));
            }
        }
        assert!(rejected_count > 5_000, "{rejected_count}");
    }

    #[test]
    fn alias_pairs_reads_copies_off_the_keep_mask() {
        let (run, _) = alias_fixture();
        let keep: Vec<bool> = (0..run.before.len())
            .map(|i| !(2..6).contains(&i))
            .collect();
        assert_eq!(
            crate::translation_validation::alias_pairs(&run.before, &keep),
            run.pairs
        );
        // A mask that erases non-copies yields a certificate the checker rejects.
        let keep: Vec<bool> = (0..run.before.len()).map(|i| i > 1).collect();
        let pairs = crate::translation_validation::alias_pairs(&run.before, &keep);
        assert_eq!(
            pairs,
            vec![AliasPair {
                store: 1,
                src: u32::MAX,
                dst: 0
            }]
        );
        // A validated rejection of the alias stage.
        let mut bad = run.clone_run();
        bad.pairs = pairs;
        assert_eq!(
            alias_rejected(&bad, &plain(3)),
            PairCheckError::NotAliasPair
        );
    }
}

#[cfg(test)]
mod plot_table_tests {
    use super::*;

    /// Copy of the plot-list construction in `assemble_program`, used as the expected value.
    fn reference(mir: &MirBody) -> Vec<CsilPlot> {
        mir.metadata
            .plots
            .iter()
            .map(|plot| CsilPlot {
                title: plot.title.clone(),
                style: plot.style.clone(),
                color_roles: plot.color_roles.clone(),
            })
            .collect()
    }

    #[test]
    fn assembled_plots_match_reference() {
        let sources = [
            "indicator(\"t\")\nx = 1\n",
            "indicator(\"t\")\nplot(close)\n",
            "indicator(\"t\")\nplot(close, \"c\", color = close > open ? color.green : color.red)\nplot(open, \"o\", style = plot.style_histogram)\nplotshape(close > open)\nhline(1)\n",
            "indicator(\"t\")\nfor i = 0 to 2\n    x = i\nplot(ta.sma(close, 3), \"s\")\nplot(high - low)\nplot(volume, display = display.none)\n",
        ];
        let mut saw_plots = false;
        let mut saw_roles = false;
        for source in sources {
            let parsed = pinewasm_parse::parse(source);
            assert!(!pinewasm_span::has_errors(&parsed.diagnostics), "{source}");
            let analysis = pinewasm_semantics::analyze(&parsed.program);
            assert!(
                !pinewasm_span::has_errors(&analysis.diagnostics),
                "{source}"
            );
            let mir = pinewasm_mir::lower(&analysis.hir);
            let program = lower_mir(&mir).expect("lowers");
            assert_eq!(program.plots, reference(&mir), "{source}");
            assert_eq!(program.plots.len(), analysis.hir.plots.len(), "{source}");
            saw_plots |= program.plots.len() >= 3;
            saw_roles |= program
                .plots
                .iter()
                .any(|plot| !plot.color_roles.is_empty());
        }
        assert!(saw_plots, "no source declared several plots");
        assert!(saw_roles, "no source carried a color role");
    }
}
