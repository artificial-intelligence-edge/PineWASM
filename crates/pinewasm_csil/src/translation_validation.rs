//! In-process translation validation for the CSIL pair-erasure passes.
//!
//! `elide_dead_constant_stores` and `coalesce_single_use_slots` (in
//! `lower.rs`) both delete disjoint adjacent instruction pairs and relocate
//! every surviving branch target through `retain_instructions`. This module
//! checks one such rewrite after the fact: given the code before and after a
//! pass, and a certificate naming the first index of every erased pair, it
//! accepts only when
//!
//! * every certified pair is an erasable pair: a literal push followed by a
//!   store to slot `s` (dead literal store), or a store to slot `s` followed
//!   by its reload `ReadUserSlot { slot: s, offset: 0 }` (single-use
//!   transfer), with `s` a declared, non-`var`, non-`varip` slot;
//! * pairs are disjoint and in increasing order, and no branch in the input
//!   targets the second half of a pair (every target also lies in range);
//! * a dead store's slot is never observed (no read at any offset, dynamic
//!   read, window or `JumpIfSlotWritten`), and a single-use slot has no access
//!   other than its erased store and reload;
//! * the output is exactly the input with the pairs removed, every kept jump
//!   retargeted to the post-removal address, and the spans retained alike.
//!
//! [`check_alias`] checks one run of `coalesce_entry_aliases` the same way.
//! Its certificate lists every coalesced copy (the offset-0 read of `src` at
//! `store - 1` and the store to `dst` at `store`) plus the renaming table, and
//! it accepts only when every copy lies in the entry's straight-line prefix,
//! no branch targets the store, `src` has exactly one store (strictly before
//! the copy), `dst` has exactly one store (the copy's), is neither read up to
//! the copy nor observed anywhere and is not `var`/`varip`, the table maps
//! each `dst` like its `src` and fixes every other slot, and the output is the
//! input with the copies removed, offset-0 reads renamed through the table and
//! every branch relocated.
//!
//! The checking logic never inspects a float: instruction payloads are
//! compared by [`instr_exact_eq`], which compares `f64` payloads by bit pattern
//! and is the only function of this module that looks inside a float.
//!
//! Written in the subset Aeneas extracts: index `while` loops with at most
//! one early exit, one `match` arm per opcode, explicit bounds checks. Two
//! clippy style lints are allowed for that reason: early exits are spelled
//! out instead of `?` (no `Try`/`FromResidual` plumbing in the extracted
//! code), and scratch tables are passed as `&Vec`/`&mut Vec` rather than
//! re-borrowed slices (no `deref`/`deref_mut` back-functions to reason about).
#![allow(clippy::question_mark, clippy::ptr_arg)]

use crate::CsilInstr;
use pinewasm_hir::HtfExpr;
use pinewasm_mir::SlotDef;
use pinewasm_span::Span;

/// Why [`check_pair_erasure`] rejected a rewrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairCheckError {
    /// The input spans do not align with the input code.
    SpanLength,
    /// The output spans do not align with the output code.
    OutputSpanLength,
    /// The input has `u32::MAX` or more instructions (targets are `u32`).
    CodeTooLong,
    /// Certified pairs overlap or are not in increasing order.
    CertificateOrder,
    /// A certified pair does not lie inside the input.
    CertificateRange,
    /// A certified pair is not a literal store or a store/reload transfer.
    NotErasablePair,
    /// The pair's slot is not declared.
    SlotOutOfRange,
    /// The pair's slot is `var` or `varip`, so its value outlives the bar.
    PersistentSlot,
    /// A branch target lies past the end of the input.
    TargetOutOfRange,
    /// A branch targets the second instruction of an erased pair.
    TargetIntoPair,
    /// The slot of an erased literal store is observed somewhere.
    SlotObserved,
    /// The slot of an erased store/reload pair has another access.
    SlotNotSingleUse,
    /// The output does not have one instruction per kept input instruction.
    OutputLength,
    /// A kept instruction does not reappear (relocated) at its new address.
    OutputMismatch,
    /// A kept span does not reappear at its new address.
    SpanMismatch,
    /// `check_alias`: the slot table has more than `u32::MAX` entries.
    TooManySlots,
    /// `check_alias`: an instruction names an undeclared slot.
    OperandOutOfRange,
    /// `check_alias`: the renaming table does not have one entry per slot.
    TableLength,
    /// `check_alias`: a certified copy is not an offset-0 read of `src`
    /// followed by a store to `dst`, or `src == dst`.
    NotAliasPair,
    /// `check_alias`: a certified copy lies after the first branch or
    /// `Return`.
    PairOutsidePrefix,
    /// `check_alias`: the source has no single store strictly before the
    /// copy.
    SourceNotDefined,
    /// `check_alias`: the destination has another store, or is read before
    /// or at the copy.
    DestinationNotFresh,
    /// `check_alias`: the renaming table does not map each destination like
    /// its source, or moves a slot that is not a destination.
    TableMismatch,
    /// A round of the flow-sensitive alias pass (copy propagation through
    /// branches and loops) was rejected by its checker.
    FlowAliasRejected,
    /// The jump-threading pass was rejected by its checker.
    JumpThreadRejected,
}

/// Check that `after`/`after_spans` is `before`/`before_spans` with the
/// adjacent pairs starting at `starts` erased and every branch relocated, and
/// that erasing those pairs is sound (see the module documentation).
///
/// Total: every input either passes or yields a [`PairCheckError`]; the
/// checker never panics.
pub fn check_pair_erasure(
    before: &[CsilInstr],
    before_spans: &[Span],
    after: &[CsilInstr],
    after_spans: &[Span],
    starts: &[u32],
    slots: &[SlotDef],
) -> Result<(), PairCheckError> {
    let n = before.len();
    if before_spans.len() != n {
        return Err(PairCheckError::SpanLength);
    }
    if after_spans.len() != after.len() {
        return Err(PairCheckError::OutputSpanLength);
    }
    if n >= u32::MAX as usize {
        return Err(PairCheckError::CodeTooLong);
    }
    let mut keep = vec![true; n];
    let mut mid = vec![false; n];
    let mut single = vec![false; slots.len()];
    let mut dead = vec![false; slots.len()];
    if let Err(error) = mark_pairs(
        before,
        starts,
        slots,
        &mut keep,
        &mut mid,
        &mut single,
        &mut dead,
    ) {
        return Err(error);
    }
    let mut access = vec![0u8; slots.len()];
    let mut observe = vec![0u8; slots.len()];
    if let Err(error) = scan_uses(before, &mid, &mut access, &mut observe) {
        return Err(error);
    }
    if let Err(error) = check_slots(&single, &dead, &access, &observe) {
        return Err(error);
    }
    let addresses = kept_addresses(&keep);
    check_output(before, before_spans, after, after_spans, &keep, &addresses)
}

/// Validate the certificate and record, for every certified pair, its erased
/// positions (`keep`), its second half (`mid`, which no branch may target) and
/// its slot's kind (`single` for a store/reload transfer, `dead` for a literal
/// store). `keep` and `mid` have one entry per input instruction; `single` and
/// `dead` one per slot.
fn mark_pairs(
    before: &[CsilInstr],
    starts: &[u32],
    slots: &[SlotDef],
    keep: &mut Vec<bool>,
    mid: &mut Vec<bool>,
    single: &mut Vec<bool>,
    dead: &mut Vec<bool>,
) -> Result<(), PairCheckError> {
    let mut floor = 0usize;
    let mut k = 0;
    while k < starts.len() {
        let first = starts[k] as usize;
        if first < floor {
            return Err(PairCheckError::CertificateOrder);
        }
        if first >= before.len() || first + 1 >= before.len() {
            return Err(PairCheckError::CertificateRange);
        }
        let (is_single, slot) = match erased_pair(&before[first], &before[first + 1]) {
            Some(pair) => pair,
            None => return Err(PairCheckError::NotErasablePair),
        };
        let index = slot as usize;
        if index >= slots.len() {
            return Err(PairCheckError::SlotOutOfRange);
        }
        if slots[index].persistent || slots[index].varip {
            return Err(PairCheckError::PersistentSlot);
        }
        if is_single {
            single[index] = true;
        } else {
            dead[index] = true;
        }
        keep[first] = false;
        keep[first + 1] = false;
        mid[first + 1] = true;
        floor = first + 2;
        k += 1;
    }
    Ok(())
}

/// The slot of an erasable pair, and whether it is a store/reload transfer
/// (`true`) or a literal store (`false`).
fn erased_pair(first: &CsilInstr, second: &CsilInstr) -> Option<(bool, u32)> {
    match second {
        CsilInstr::StoreUserSlotCurrent { slot } => {
            if is_literal(first) {
                Some((false, *slot))
            } else {
                None
            }
        }
        CsilInstr::ReadUserSlot { slot, offset } => match first {
            CsilInstr::StoreUserSlotCurrent { slot: stored } => {
                if *stored == *slot && *offset == 0 {
                    Some((true, *slot))
                } else {
                    None
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// The six literal pushes.
fn is_literal(instr: &CsilInstr) -> bool {
    matches!(
        instr,
        CsilInstr::PushNa
            | CsilInstr::PushBool(_)
            | CsilInstr::PushI64(_)
            | CsilInstr::PushF64(_)
            | CsilInstr::PushStr(_)
            | CsilInstr::PushColor { .. }
    )
}

/// Count, per declared slot, every access (`access`) and every observation
/// (`observe`: reads at any offset, dynamic reads, windows and written-flag
/// branches), saturating at 3 since only 0 and 2 matter; and check every
/// branch target (in range, not into a pair).
fn scan_uses(
    before: &[CsilInstr],
    mid: &Vec<bool>,
    access: &mut Vec<u8>,
    observe: &mut Vec<u8>,
) -> Result<(), PairCheckError> {
    let mut pc = 0;
    while pc < before.len() {
        match &before[pc] {
            CsilInstr::ReadUserSlot { slot, .. } => {
                count_use(access, *slot);
                count_use(observe, *slot);
            }
            CsilInstr::ReadUserSlotDynamic { slot } => {
                count_use(access, *slot);
                count_use(observe, *slot);
            }
            CsilInstr::WindowMean { slot, .. }
            | CsilInstr::WindowMeanDynamic { slot }
            | CsilInstr::WindowExtremumDynamic { slot, .. }
            | CsilInstr::WindowStatisticDynamic { slot, .. } => {
                count_use(access, *slot);
                count_use(observe, *slot);
            }
            CsilInstr::StoreUserSlotCurrent { slot } => count_use(access, *slot),
            CsilInstr::JumpIfSlotWritten { slot, target } => {
                count_use(access, *slot);
                count_use(observe, *slot);
                if let Err(error) = check_target(*target, mid) {
                    return Err(error);
                }
            }
            CsilInstr::Jump { target } => {
                if let Err(error) = check_target(*target, mid) {
                    return Err(error);
                }
            }
            CsilInstr::JumpIfFalse { target } => {
                if let Err(error) = check_target(*target, mid) {
                    return Err(error);
                }
            }
            CsilInstr::JumpIfExact { target, .. } => {
                if let Err(error) = check_target(*target, mid) {
                    return Err(error);
                }
            }
            _ => {}
        }
        pc += 1;
    }
    Ok(())
}

/// Bump a saturating (at 3) per-slot counter; undeclared slots are not
/// counted (no certified pair can name them).
fn count_use(counts: &mut Vec<u8>, slot: u32) {
    let index = slot as usize;
    if index < counts.len() && counts[index] < 3 {
        counts[index] += 1;
    }
}

/// A branch target must lie in `0..=len` (the end of the code is a valid
/// target) and must not enter an erased pair between its two instructions.
fn check_target(target: u32, mid: &Vec<bool>) -> Result<(), PairCheckError> {
    let index = target as usize;
    if index > mid.len() {
        Err(PairCheckError::TargetOutOfRange)
    } else if index < mid.len() && mid[index] {
        Err(PairCheckError::TargetIntoPair)
    } else {
        Ok(())
    }
}

/// Per-slot conditions: a single-use slot is accessed exactly by its erased
/// store and reload; a dead store's slot is never observed.
fn check_slots(
    single: &Vec<bool>,
    dead: &Vec<bool>,
    access: &Vec<u8>,
    observe: &Vec<u8>,
) -> Result<(), PairCheckError> {
    let mut slot = 0;
    while slot < single.len() {
        if single[slot] && access[slot] != 2 {
            return Err(PairCheckError::SlotNotSingleUse);
        }
        if dead[slot] && observe[slot] != 0 {
            return Err(PairCheckError::SlotObserved);
        }
        slot += 1;
    }
    Ok(())
}

/// `addresses[i]`: the number of kept instructions before input index `i`,
/// for `i` in `0..=keep.len()` (the relocation map `retain_instructions`
/// applies to branch targets).
fn kept_addresses(keep: &Vec<bool>) -> Vec<u32> {
    let mut addresses = Vec::with_capacity(keep.len() + 1);
    let mut next = 0u32;
    let mut index = 0;
    while index < keep.len() {
        addresses.push(next);
        if keep[index] {
            next += 1;
        }
        index += 1;
    }
    addresses.push(next);
    addresses
}

/// Every kept input instruction and span reappears, relocated, at its new
/// address, and nothing else is in the output.
fn check_output(
    before: &[CsilInstr],
    before_spans: &[Span],
    after: &[CsilInstr],
    after_spans: &[Span],
    keep: &Vec<bool>,
    addresses: &Vec<u32>,
) -> Result<(), PairCheckError> {
    if after.len() != addresses[before.len()] as usize {
        return Err(PairCheckError::OutputLength);
    }
    let mut index = 0;
    while index < before.len() {
        if keep[index] {
            let at = addresses[index] as usize;
            if !instr_matches(&before[index], &after[at], addresses) {
                return Err(PairCheckError::OutputMismatch);
            }
            if !span_eq(&before_spans[index], &after_spans[at]) {
                return Err(PairCheckError::SpanMismatch);
            }
        }
        index += 1;
    }
    Ok(())
}

/// `relocated` is `original` with its branch target (if any) mapped through
/// `addresses`.
fn instr_matches(original: &CsilInstr, relocated: &CsilInstr, addresses: &Vec<u32>) -> bool {
    match original {
        CsilInstr::Jump { target } => match relocated {
            CsilInstr::Jump { target: moved } => addresses[*target as usize] == *moved,
            _ => false,
        },
        CsilInstr::JumpIfFalse { target } => match relocated {
            CsilInstr::JumpIfFalse { target: moved } => addresses[*target as usize] == *moved,
            _ => false,
        },
        CsilInstr::JumpIfExact { on, target } => match relocated {
            CsilInstr::JumpIfExact {
                on: moved_on,
                target: moved,
            } => *on == *moved_on && addresses[*target as usize] == *moved,
            _ => false,
        },
        CsilInstr::JumpIfSlotWritten { slot, target } => match relocated {
            CsilInstr::JumpIfSlotWritten {
                slot: moved_slot,
                target: moved,
            } => *slot == *moved_slot && addresses[*target as usize] == *moved,
            _ => false,
        },
        _ => instr_exact_eq(original, relocated),
    }
}

/// Field-wise span equality.
fn span_eq(a: &Span, b: &Span) -> bool {
    a.source.0 == b.source.0 && a.start == b.start && a.end == b.end
}

/// Exact structural equality of two instructions: like the derived
/// `PartialEq`, except that `f64` payloads (`PushF64`, the constants inside a
/// `ReadSecurityExpr` tree) are compared by bit pattern, so `-0.0` differs
/// from `0.0` and a NaN equals the same NaN. The derived `PartialEq` uses
/// float equality there and is therefore not a sound instruction identity
/// for translation validation.
pub fn instr_exact_eq(a: &CsilInstr, b: &CsilInstr) -> bool {
    match (a, b) {
        (CsilInstr::PushF64(x), CsilInstr::PushF64(y)) => x.to_bits() == y.to_bits(),
        (
            CsilInstr::ReadSecurityExpr {
                expr: expr_a,
                htf_seconds: seconds_a,
                timeframe_expr: timeframe_a,
                calendar_months: months_a,
                gaps_on: gaps_a,
                lookahead_on: lookahead_a,
            },
            CsilInstr::ReadSecurityExpr {
                expr: expr_b,
                htf_seconds: seconds_b,
                timeframe_expr: timeframe_b,
                calendar_months: months_b,
                gaps_on: gaps_b,
                lookahead_on: lookahead_b,
            },
        ) => {
            htf_exact_eq(expr_a, expr_b)
                && seconds_a == seconds_b
                && timeframe_a == timeframe_b
                && months_a == months_b
                && gaps_a == gaps_b
                && lookahead_a == lookahead_b
        }
        // Every other variant carries only integers, booleans, strings and
        // field-less enums, where the derived equality is exact.
        _ => a == b,
    }
}

/// Exact structural equality of two secondary-context expression trees,
/// comparing constants by bit pattern.
fn htf_exact_eq(a: &HtfExpr, b: &HtfExpr) -> bool {
    match (a, b) {
        (HtfExpr::BoolResult(x), HtfExpr::BoolResult(y)) => htf_exact_eq(x, y),
        (HtfExpr::Const(x), HtfExpr::Const(y)) => x.to_bits() == y.to_bits(),
        (HtfExpr::Source(x), HtfExpr::Source(y)) => x == y,
        (HtfExpr::InputSource { input: x }, HtfExpr::InputSource { input: y }) => x == y,
        (HtfExpr::BarstateIslast, HtfExpr::BarstateIslast) => true,
        (HtfExpr::BarstateIsRealtime, HtfExpr::BarstateIsRealtime) => true,
        (
            HtfExpr::History {
                history_slot: slot_a,
                offset: offset_a,
                source: source_a,
            },
            HtfExpr::History {
                history_slot: slot_b,
                offset: offset_b,
                source: source_b,
            },
        ) => slot_a == slot_b && offset_a == offset_b && htf_exact_eq(source_a, source_b),
        (
            HtfExpr::HistoryInput {
                history_slot: a,
                offset: ai,
                source: ax,
            },
            HtfExpr::HistoryInput {
                history_slot: b,
                offset: bi,
                source: bx,
            },
        ) => a == b && ai.exact_eq(bi) && htf_exact_eq(ax, bx),
        (
            HtfExpr::HistoryRealtime {
                history_slot: slot_a,
                history_offset: historical_a,
                realtime_offset: realtime_a,
                source: source_a,
            },
            HtfExpr::HistoryRealtime {
                history_slot: slot_b,
                history_offset: historical_b,
                realtime_offset: realtime_b,
                source: source_b,
            },
        ) => {
            slot_a == slot_b
                && historical_a == historical_b
                && realtime_a == realtime_b
                && htf_exact_eq(source_a, source_b)
        }
        (
            HtfExpr::EnumSelect {
                input: ai,
                index: ax,
                when_equal: a,
                otherwise: ar,
            },
            HtfExpr::EnumSelect {
                input: bi,
                index: bx,
                when_equal: b,
                otherwise: br,
            },
        ) => {
            ai == bi
                && ax == bx
                && htf_exact_eq(a, b)
                && match (ar, br) {
                    (Some(a), Some(b)) => htf_exact_eq(a, b),
                    (None, None) => true,
                    _ => false,
                }
        }
        (HtfExpr::Neg(x), HtfExpr::Neg(y)) => htf_exact_eq(x, y),
        (
            HtfExpr::Binary {
                op: op_a,
                int_result: int_a,
                lhs: lhs_a,
                rhs: rhs_a,
            },
            HtfExpr::Binary {
                op: op_b,
                int_result: int_b,
                lhs: lhs_b,
                rhs: rhs_b,
            },
        ) => {
            op_a == op_b
                && int_a == int_b
                && htf_exact_eq(lhs_a, lhs_b)
                && htf_exact_eq(rhs_a, rhs_b)
        }
        (
            HtfExpr::Ta {
                func: func_a,
                ta_slot: slot_a,
                length: length_a,
                source: source_a,
            },
            HtfExpr::Ta {
                func: func_b,
                ta_slot: slot_b,
                length: length_b,
                source: source_b,
            },
        ) => {
            func_a == func_b
                && slot_a == slot_b
                && length_a == length_b
                && htf_exact_eq(source_a, source_b)
        }
        (
            HtfExpr::TaInput {
                func: func_a,
                ta_slot: slot_a,
                input: input_a,
                binding_target: target_a,
                default_length: length_a,
                source: source_a,
            },
            HtfExpr::TaInput {
                func: func_b,
                ta_slot: slot_b,
                input: input_b,
                binding_target: target_b,
                default_length: length_b,
                source: source_b,
            },
        ) => {
            func_a == func_b
                && slot_a == slot_b
                && input_a == input_b
                && target_a == target_b
                && length_a == length_b
                && htf_exact_eq(source_a, source_b)
        }
        _ => false,
    }
}

/// The certificate of a pair-erasure keep mask: the first index of every
/// erased pair, grouping the erased indices into pairs left to right. The
/// passes emit their keep mask; this reads it back as [`check_pair_erasure`]'s
/// certificate. Untrusted: a wrong certificate can only make the checker
/// reject.
pub fn pair_starts(keep: &[bool]) -> Vec<u32> {
    let mut starts = Vec::new();
    let mut index = 0;
    while index < keep.len() {
        if keep[index] {
            index += 1;
        } else {
            starts.push(index as u32);
            index += 2;
        }
    }
    starts
}

/// One coalesced copy of the entry-alias pass: the offset-0 read of `src` at
/// `store - 1` and the store to `dst` at `store`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AliasPair {
    pub store: u32,
    pub src: u32,
    pub dst: u32,
}

/// The copies an entry-alias keep mask erases: every erased index opens a
/// (read, store) pair, whose slots are read off the input code (`u32::MAX`
/// when the instruction is not a read/store). Untrusted, like
/// [`pair_starts`].
pub fn alias_pairs(code: &[CsilInstr], keep: &[bool]) -> Vec<AliasPair> {
    let mut pairs = Vec::new();
    let mut index = 0;
    while index < keep.len() {
        if keep[index] {
            index += 1;
        } else {
            let mut src = u32::MAX;
            if index < code.len() {
                src = read_slot_or_max(&code[index]);
            }
            let mut dst = u32::MAX;
            if index + 1 < code.len() {
                dst = store_slot_or_max(&code[index + 1]);
            }
            pairs.push(AliasPair {
                store: (index + 1) as u32,
                src,
                dst,
            });
            index += 2;
        }
    }
    pairs
}

/// The slot a `ReadUserSlot` reads (at any offset), `u32::MAX` for any other
/// instruction.
fn read_slot_or_max(instr: &CsilInstr) -> u32 {
    match instr {
        CsilInstr::ReadUserSlot { slot, .. } => *slot,
        _ => u32::MAX,
    }
}

/// The slot a `StoreUserSlotCurrent` stores, `u32::MAX` for any other
/// instruction.
fn store_slot_or_max(instr: &CsilInstr) -> u32 {
    match instr {
        CsilInstr::StoreUserSlotCurrent { slot } => *slot,
        _ => u32::MAX,
    }
}

/// Check that `after`/`after_spans` is `before`/`before_spans` with the
/// certified entry-prefix copies `pairs` erased, every offset-0 read renamed
/// through `table` and every branch relocated, and that the rewrite is sound
/// (see the module documentation).
///
/// Total: every input either passes or yields a [`PairCheckError`].
pub fn check_alias(
    before: &[CsilInstr],
    before_spans: &[Span],
    after: &[CsilInstr],
    after_spans: &[Span],
    pairs: &[AliasPair],
    table: &[u32],
    slots: &[SlotDef],
) -> Result<(), PairCheckError> {
    let n = before.len();
    if before_spans.len() != n {
        return Err(PairCheckError::SpanLength);
    }
    if after_spans.len() != after.len() {
        return Err(PairCheckError::OutputSpanLength);
    }
    if n >= u32::MAX as usize {
        return Err(PairCheckError::CodeTooLong);
    }
    if slots.len() > u32::MAX as usize {
        return Err(PairCheckError::TooManySlots);
    }
    if table.len() != slots.len() {
        return Err(PairCheckError::TableLength);
    }
    let mut writes = vec![0u8; slots.len()];
    let mut first_store = vec![n; slots.len()];
    let mut first_read = vec![n; slots.len()];
    let mut observed = vec![false; slots.len()];
    let mut targeted = vec![false; n];
    let prefix_end = match alias_scan(
        before,
        &mut writes,
        &mut first_store,
        &mut first_read,
        &mut observed,
        &mut targeted,
    ) {
        Ok(end) => end,
        Err(error) => return Err(error),
    };
    let mut keep = vec![true; n];
    let mut is_dst = vec![false; slots.len()];
    let scan = AliasScan {
        writes,
        first_store,
        first_read,
        observed,
        targeted,
        prefix_end,
    };
    if let Err(error) = alias_mark_pairs(before, pairs, slots, table, &scan, &mut keep, &mut is_dst)
    {
        return Err(error);
    }
    if let Err(error) = alias_check_table(table, &is_dst) {
        return Err(error);
    }
    let addresses = kept_addresses(&keep);
    check_alias_output(
        before,
        before_spans,
        after,
        after_spans,
        &keep,
        &addresses,
        table,
    )
}

/// One pass over the input: every slot operand is declared and every branch
/// target lies in `0..=len`; per slot, count stores (`writes`, saturating at
/// 3) and record the first store and the first read at any offset (`len` when
/// there is none) and whether a history, dynamic, window or written-flag read
/// observes it; per instruction, whether a branch targets it. Returns the end
/// of the entry's straight-line prefix: the first branch or `Return`, or
/// `len`.
fn alias_scan(
    before: &[CsilInstr],
    writes: &mut Vec<u8>,
    first_store: &mut Vec<usize>,
    first_read: &mut Vec<usize>,
    observed: &mut Vec<bool>,
    targeted: &mut Vec<bool>,
) -> Result<usize, PairCheckError> {
    let n = before.len();
    let mut prefix_end = n;
    let mut pc = 0;
    while pc < n {
        let (slot, kind, target, ends) = alias_operands(&before[pc]);
        if let Err(error) =
            alias_note_slot(slot, kind, pc, n, writes, first_store, first_read, observed)
        {
            return Err(error);
        }
        if let Err(error) = alias_note_target(target, n, targeted) {
            return Err(error);
        }
        if ends && prefix_end == n {
            prefix_end = pc;
        }
        pc += 1;
    }
    Ok(prefix_end)
}

/// [`alias_scan`]'s bookkeeping for the slot operand of the instruction at
/// `pc` (see [`alias_operands`] for `kind`): reject an undeclared slot, count
/// a store and record the slot's first store, first read and observers.
#[allow(clippy::too_many_arguments)]
fn alias_note_slot(
    slot: Option<u32>,
    kind: u8,
    pc: usize,
    n: usize,
    writes: &mut Vec<u8>,
    first_store: &mut Vec<usize>,
    first_read: &mut Vec<usize>,
    observed: &mut Vec<bool>,
) -> Result<(), PairCheckError> {
    if let Some(slot) = slot {
        let index = slot as usize;
        if index >= writes.len() {
            return Err(PairCheckError::OperandOutOfRange);
        }
        if kind == 1 {
            count_use(writes, slot);
            if first_store[index] == n {
                first_store[index] = pc;
            }
        }
        if (kind == 2 || kind == 3) && first_read[index] == n {
            first_read[index] = pc;
        }
        if kind >= 3 {
            observed[index] = true;
        }
    }
    Ok(())
}

/// [`alias_scan`]'s bookkeeping for a branch target: reject a target past
/// the end and mark the targeted instruction.
fn alias_note_target(
    target: Option<u32>,
    n: usize,
    targeted: &mut Vec<bool>,
) -> Result<(), PairCheckError> {
    if let Some(target) = target {
        let index = target as usize;
        if index > n {
            return Err(PairCheckError::TargetOutOfRange);
        }
        if index < n {
            targeted[index] = true;
        }
    }
    Ok(())
}

/// How [`alias_scan`] sees an instruction: its slot operand, the kind of slot
/// access (0 none, 1 store, 2 offset-0 read, 3 history read, 4 dynamic read,
/// window or written-flag branch; 3 and 4 observe the slot), its branch target,
/// and whether it ends the entry's straight-line prefix (a branch or
/// `Return`).
fn alias_operands(instr: &CsilInstr) -> (Option<u32>, u8, Option<u32>, bool) {
    match instr {
        CsilInstr::StoreUserSlotCurrent { slot } => (Some(*slot), 1, None, false),
        CsilInstr::ReadUserSlot { slot, offset } => {
            if *offset == 0 {
                (Some(*slot), 2, None, false)
            } else {
                (Some(*slot), 3, None, false)
            }
        }
        CsilInstr::ReadUserSlotDynamic { slot } => (Some(*slot), 4, None, false),
        CsilInstr::WindowMean { slot, .. }
        | CsilInstr::WindowMeanDynamic { slot }
        | CsilInstr::WindowExtremumDynamic { slot, .. }
        | CsilInstr::WindowStatisticDynamic { slot, .. } => (Some(*slot), 4, None, false),
        CsilInstr::JumpIfSlotWritten { slot, target } => (Some(*slot), 4, Some(*target), true),
        CsilInstr::Jump { target } => (None, 0, Some(*target), true),
        CsilInstr::JumpIfFalse { target } => (None, 0, Some(*target), true),
        CsilInstr::JumpIfExact { target, .. } => (None, 0, Some(*target), true),
        CsilInstr::Return => (None, 0, None, true),
        _ => (None, 0, None, false),
    }
}

/// What [`alias_scan`] records about the input.
struct AliasScan {
    writes: Vec<u8>,
    first_store: Vec<usize>,
    first_read: Vec<usize>,
    observed: Vec<bool>,
    targeted: Vec<bool>,
    prefix_end: usize,
}

/// Check every certified copy and record, for each, its erased positions
/// (`keep`) and its destination (`is_dst`). A store index may be certified
/// once only (`seen`).
fn alias_mark_pairs(
    before: &[CsilInstr],
    pairs: &[AliasPair],
    slots: &[SlotDef],
    table: &[u32],
    scan: &AliasScan,
    keep: &mut Vec<bool>,
    is_dst: &mut Vec<bool>,
) -> Result<(), PairCheckError> {
    let mut seen = vec![false; before.len()];
    let mut k = 0;
    while k < pairs.len() {
        if let Err(error) = alias_pair_ok(before, &pairs[k], slots, table, scan, &seen) {
            return Err(error);
        }
        let p = pairs[k].store as usize;
        seen[p] = true;
        keep[p - 1] = false;
        keep[p] = false;
        is_dst[pairs[k].dst as usize] = true;
        k += 1;
    }
    Ok(())
}

/// The table fixes every slot that is not a certified destination.
fn alias_check_table(table: &[u32], is_dst: &Vec<bool>) -> Result<(), PairCheckError> {
    let mut slot = 0;
    while slot < table.len() {
        if !is_dst[slot] && table[slot] as usize != slot {
            return Err(PairCheckError::TableMismatch);
        }
        slot += 1;
    }
    Ok(())
}

/// The local conditions of one certified copy (see the module
/// documentation), and that its store index was not certified before.
fn alias_pair_ok(
    before: &[CsilInstr],
    pair: &AliasPair,
    slots: &[SlotDef],
    table: &[u32],
    scan: &AliasScan,
    seen: &Vec<bool>,
) -> Result<(), PairCheckError> {
    let p = pair.store as usize;
    if p == 0 || p >= before.len() {
        return Err(PairCheckError::CertificateRange);
    }
    if !is_copy(&before[p - 1], &before[p], pair.src, pair.dst) {
        return Err(PairCheckError::NotAliasPair);
    }
    if p >= scan.prefix_end {
        return Err(PairCheckError::PairOutsidePrefix);
    }
    if scan.targeted[p] {
        return Err(PairCheckError::TargetIntoPair);
    }
    let (src, dst) = (pair.src as usize, pair.dst as usize);
    if src >= slots.len() || dst >= slots.len() {
        return Err(PairCheckError::SlotOutOfRange);
    }
    if scan.writes[src] != 1 || scan.first_store[src] + 1 >= p {
        return Err(PairCheckError::SourceNotDefined);
    }
    if scan.writes[dst] != 1 || scan.first_read[dst] <= p {
        return Err(PairCheckError::DestinationNotFresh);
    }
    if slots[dst].persistent || slots[dst].varip {
        return Err(PairCheckError::PersistentSlot);
    }
    if scan.observed[dst] {
        return Err(PairCheckError::SlotObserved);
    }
    if table[dst] != table[src] {
        return Err(PairCheckError::TableMismatch);
    }
    if seen[p] {
        return Err(PairCheckError::CertificateOrder);
    }
    Ok(())
}

/// `(read, store)` is an offset-0 read of `src` followed by a store to a
/// different slot `dst`.
fn is_copy(read: &CsilInstr, store: &CsilInstr, src: u32, dst: u32) -> bool {
    match read {
        CsilInstr::ReadUserSlot { slot, offset } => match store {
            CsilInstr::StoreUserSlotCurrent { slot: stored } => {
                *slot == src && *offset == 0 && *stored == dst && src != dst
            }
            _ => false,
        },
        _ => false,
    }
}

/// Every kept input instruction reappears renamed and relocated at its new
/// address, every kept span reappears there, and nothing else is in the
/// output.
fn check_alias_output(
    before: &[CsilInstr],
    before_spans: &[Span],
    after: &[CsilInstr],
    after_spans: &[Span],
    keep: &Vec<bool>,
    addresses: &Vec<u32>,
    table: &[u32],
) -> Result<(), PairCheckError> {
    if after.len() != addresses[before.len()] as usize {
        return Err(PairCheckError::OutputLength);
    }
    let mut index = 0;
    while index < before.len() {
        if keep[index] {
            let at = addresses[index] as usize;
            if !renamed_matches(&before[index], &after[at], addresses, table) {
                return Err(PairCheckError::OutputMismatch);
            }
            if !span_eq(&before_spans[index], &after_spans[at]) {
                return Err(PairCheckError::SpanMismatch);
            }
        }
        index += 1;
    }
    Ok(())
}

/// `renamed` is `original` with an offset-0 read renamed through `table` and a
/// branch target relocated through `addresses`.
fn renamed_matches(
    original: &CsilInstr,
    renamed: &CsilInstr,
    addresses: &Vec<u32>,
    table: &[u32],
) -> bool {
    match original {
        CsilInstr::ReadUserSlot { slot, offset } => {
            if *offset == 0 {
                match renamed {
                    CsilInstr::ReadUserSlot {
                        slot: moved,
                        offset: moved_offset,
                    } => {
                        (*slot as usize) < table.len()
                            && *moved_offset == 0
                            && table[*slot as usize] == *moved
                    }
                    _ => false,
                }
            } else {
                instr_exact_eq(original, renamed)
            }
        }
        _ => instr_matches(original, renamed, addresses),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pinewasm_hir::{SeriesSlot, TaFunc};
    use pinewasm_mir::{BlockId, MirBlock, MirBody, MirInstr, Terminator, Value, ValueId};
    use pinewasm_span::SourceId;

    fn windows(slot: u32) -> Vec<CsilInstr> {
        let mut result = vec![
            CsilInstr::WindowMeanDynamic { slot },
            CsilInstr::WindowExtremumDynamic {
                slot,
                highest: false,
            },
            CsilInstr::WindowExtremumDynamic {
                slot,
                highest: true,
            },
        ];
        for func in [
            TaFunc::Median,
            TaFunc::MedianInt,
            TaFunc::Mode,
            TaFunc::ModeInt,
            TaFunc::Range,
            TaFunc::RangeInt,
        ] {
            result.push(CsilInstr::WindowStatisticDynamic { slot, func });
        }
        result
    }

    fn slots() -> Vec<SlotDef> {
        (0..3)
            .map(|index| SlotDef {
                name: format!("s{index}"),
                persistent: false,
                varip: false,
                lane: None,
            })
            .collect()
    }

    fn spans(n: usize) -> Vec<Span> {
        (0..n)
            .map(|index| Span::new(SourceId(7), index as u32, index as u32 + 1))
            .collect()
    }

    fn erase(code: &[CsilInstr], start: usize) -> (Vec<CsilInstr>, Vec<Span>) {
        let source_spans = spans(code.len());
        let kept = (0..code.len()).filter(|pc| *pc != start && *pc != start + 1);
        (
            kept.clone().map(|pc| code[pc].clone()).collect(),
            kept.map(|pc| source_spans[pc]).collect(),
        )
    }

    #[test]
    fn forged_dead_store_is_observed_even_before_store_or_after_return() {
        for window in windows(0) {
            for placement in 0..3 {
                let mut code = vec![
                    CsilInstr::PushI64(42),
                    CsilInstr::StoreUserSlotCurrent { slot: 0 },
                ];
                let observer = vec![CsilInstr::PushI64(1), window.clone(), CsilInstr::Pop];
                let start = if placement == 0 {
                    code.splice(0..0, observer.clone());
                    3
                } else {
                    0
                };
                if placement == 1 {
                    code.extend(observer.clone());
                }
                code.push(CsilInstr::Return);
                if placement == 2 {
                    code.extend(observer);
                }
                let (after, after_spans) = erase(&code, start);
                assert_eq!(
                    check_pair_erasure(
                        &code,
                        &spans(code.len()),
                        &after,
                        &after_spans,
                        &[start as u32],
                        &slots()
                    ),
                    Err(PairCheckError::SlotObserved),
                    "{window:?}, placement={placement}"
                );
            }
        }
    }

    #[test]
    fn forged_single_use_counts_extra_window_access() {
        for window in windows(0) {
            let code = vec![
                CsilInstr::PushI64(42),
                CsilInstr::StoreUserSlotCurrent { slot: 0 },
                CsilInstr::ReadUserSlot { slot: 0, offset: 0 },
                CsilInstr::Pop,
                CsilInstr::Return,
                CsilInstr::PushI64(1),
                window.clone(),
                CsilInstr::Pop,
            ];
            let (after, after_spans) = erase(&code, 1);
            assert_eq!(
                check_pair_erasure(
                    &code,
                    &spans(code.len()),
                    &after,
                    &after_spans,
                    &[1],
                    &slots()
                ),
                Err(PairCheckError::SlotNotSingleUse),
                "{window:?}"
            );
        }
    }

    #[test]
    fn forged_alias_cannot_erase_observed_destination() {
        for window in windows(1) {
            for placement in 0..3 {
                let mut code = vec![
                    CsilInstr::PushI64(42),
                    CsilInstr::StoreUserSlotCurrent { slot: 0 },
                    CsilInstr::ReadUserSlot { slot: 0, offset: 0 },
                    CsilInstr::StoreUserSlotCurrent { slot: 1 },
                ];
                let observer = vec![CsilInstr::PushI64(1), window.clone(), CsilInstr::Pop];
                let start = if placement == 0 {
                    code.splice(0..0, observer.clone());
                    5
                } else {
                    2
                };
                if placement == 1 {
                    code.extend(observer.clone());
                }
                code.push(CsilInstr::Return);
                if placement == 2 {
                    code.extend(observer);
                }
                let (after, after_spans) = erase(&code, start);
                assert_eq!(
                    check_alias(
                        &code,
                        &spans(code.len()),
                        &after,
                        &after_spans,
                        &[AliasPair {
                            store: start as u32 + 1,
                            src: 0,
                            dst: 1
                        }],
                        &[0, 0, 2],
                        &slots()
                    ),
                    Err(PairCheckError::SlotObserved),
                    "{window:?}, placement={placement}"
                );
            }
        }
    }

    #[test]
    fn unrelated_slot_and_identity_certificates_are_accepted() {
        for window in windows(0) {
            let code = vec![
                CsilInstr::PushI64(42),
                CsilInstr::StoreUserSlotCurrent { slot: 1 },
                CsilInstr::PushI64(1),
                window.clone(),
                CsilInstr::Pop,
                CsilInstr::Return,
            ];
            let (after, after_spans) = erase(&code, 0);
            assert_eq!(
                check_pair_erasure(
                    &code,
                    &spans(code.len()),
                    &after,
                    &after_spans,
                    &[0],
                    &slots()
                ),
                Ok(())
            );
            assert_eq!(
                check_alias(
                    &code,
                    &spans(code.len()),
                    &code,
                    &spans(code.len()),
                    &[],
                    &[0, 1, 2],
                    &slots()
                ),
                Ok(())
            );
            // A source observer before a legitimate copy must not end the entry prefix.
            let alias_code = vec![
                CsilInstr::PushI64(1),
                window.clone(),
                CsilInstr::Pop,
                CsilInstr::PushI64(42),
                CsilInstr::StoreUserSlotCurrent { slot: 0 },
                CsilInstr::ReadUserSlot { slot: 0, offset: 0 },
                CsilInstr::StoreUserSlotCurrent { slot: 1 },
                CsilInstr::Return,
            ];
            let (alias_after, alias_spans) = erase(&alias_code, 5);
            assert_eq!(
                check_alias(
                    &alias_code,
                    &spans(alias_code.len()),
                    &alias_after,
                    &alias_spans,
                    &[AliasPair {
                        store: 6,
                        src: 0,
                        dst: 1
                    }],
                    &[0, 0, 2],
                    &slots()
                ),
                Ok(())
            );
        }
    }

    #[test]
    fn classifier_range_saturation_and_exact_operand_roles() {
        for window in windows(1) {
            assert_eq!(alias_operands(&window), (Some(1), 4, None, false));
            for initial in 0..=3 {
                for repetitions in 0..=4 {
                    let code = vec![window.clone(); repetitions];
                    let mut access = vec![0, initial, 0];
                    let mut observe = vec![0, initial, 0];
                    assert_eq!(
                        scan_uses(&code, &vec![false; code.len()], &mut access, &mut observe),
                        Ok(())
                    );
                    let expected = (initial + repetitions as u8).min(3);
                    assert_eq!(access, vec![0, expected, 0]);
                    assert_eq!(observe, vec![0, expected, 0]);
                }
            }
        }
        for slot in [2, 3, u32::MAX] {
            for window in windows(slot) {
                let code = vec![window];
                let source_spans = spans(code.len());
                let expected = if slot == 2 {
                    Ok(())
                } else {
                    Err(PairCheckError::OperandOutOfRange)
                };
                assert_eq!(
                    check_alias(
                        &code,
                        &source_spans,
                        &code,
                        &source_spans,
                        &[],
                        &[0, 1, 2],
                        &slots()
                    ),
                    expected
                );
                // Pair scanning keeps its existing policy: uncertified undeclared operands are ignored.
                let mut access = vec![0; 3];
                let mut observe = vec![0; 3];
                assert_eq!(
                    scan_uses(&code, &vec![false], &mut access, &mut observe),
                    Ok(())
                );
                if slot >= 3 {
                    assert_eq!(access, vec![0; 3]);
                    assert_eq!(observe, vec![0; 3]);
                }
            }
        }
    }

    #[test]
    fn optimizer_output_is_accepted() {
        for window in windows(0) {
            let span = Span::dummy();
            let mut instrs = Vec::new();
            for (dst, slot) in [(0, 1), (1, 0), (2, 2)] {
                instrs.push(MirInstr::Const {
                    dst: ValueId(dst),
                    value: Value::Int(42 + i64::from(dst)),
                    span,
                });
                instrs.push(MirInstr::StoreUserSeriesCurrent {
                    slot: SeriesSlot(slot),
                    value: ValueId(dst),
                    span,
                });
            }
            instrs.push(MirInstr::ReadUserSeries {
                dst: ValueId(3),
                slot: SeriesSlot(2),
                offset: 0,
                span,
            });
            instrs.push(MirInstr::Discard {
                value: ValueId(3),
                span,
            });
            instrs.push(MirInstr::Const {
                dst: ValueId(4),
                value: Value::Int(1),
                span,
            });
            let operation = match &window {
                CsilInstr::WindowMeanDynamic { slot } => MirInstr::WindowMeanDynamic {
                    dst: ValueId(5),
                    slot: SeriesSlot(*slot),
                    length: ValueId(4),
                    span,
                },
                CsilInstr::WindowExtremumDynamic { slot, highest } => {
                    MirInstr::WindowExtremumDynamic {
                        dst: ValueId(5),
                        slot: SeriesSlot(*slot),
                        length: ValueId(4),
                        highest: *highest,
                        span,
                    }
                }
                CsilInstr::WindowStatisticDynamic { slot, func } => {
                    MirInstr::WindowStatisticDynamic {
                        dst: ValueId(5),
                        slot: SeriesSlot(*slot),
                        length: ValueId(4),
                        func: *func,
                        span,
                    }
                }
                _ => unreachable!(),
            };
            instrs.push(operation);
            instrs.push(MirInstr::Discard {
                value: ValueId(5),
                span,
            });
            let mut metadata: pinewasm_mir::MirMetadata = serde_json::from_value(serde_json::json!({
                "title": null, "series_slots": [], "plots": [], "ta_states": [], "inputs": [], "object_types": []
            })).expect("complete required metadata fields");
            metadata.series_slots = slots();
            let mir = MirBody {
                blocks: vec![MirBlock {
                    id: BlockId(0),
                    instrs,
                    terminator: Terminator::Return { span },
                }],
                entry: BlockId(0),
                value_count: 6,
                metadata,
            };
            let ordinary = crate::lower_mir(&mir).expect("ordinary lowering");
            assert_eq!(crate::lower_mir_validated(&mir), Ok(ordinary.clone()));
            let capture = crate::lower_mir_with_pass_record(&mir).expect("captured lowering");
            assert_eq!(capture.program, ordinary);
            let code = &ordinary.functions[0].code;
            assert!(code.contains(&window));
            assert!(code.contains(&CsilInstr::StoreUserSlotCurrent { slot: 0 }));
            assert!(!code.contains(&CsilInstr::StoreUserSlotCurrent { slot: 1 }));
            let source = &capture.coalescing.source;
            let target = &capture.coalescing.target;
            let starts = pair_starts(&capture.coalescing.keep_mask);
            assert!(
                !starts.is_empty(),
                "actual unrelated single-use rewrite required"
            );
            assert_eq!(
                check_pair_erasure(
                    &source.code,
                    &source.source_map,
                    &target.code,
                    &target.source_map,
                    &starts,
                    &slots()
                ),
                Ok(())
            );
        }
    }
}
