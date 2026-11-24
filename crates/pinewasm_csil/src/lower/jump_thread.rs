//! Jump threading and removal of jumps to the next instruction.
//!
//! Stackification lowers every MIR `goto` to a `JUMP`, and every conditional
//! to `JUMP_IF_FALSE else; JUMP then`, even when `then` is the next block or
//! the join is itself a block holding only a `JUMP`. Each such jump costs one
//! instruction of the per-bar budget every time it runs. This pass
//!
//! * retargets every branch whose target is an unconditional `JUMP` to the end
//!   of that chain of `JUMP`s (a cyclic chain is left alone), and
//! * erases an unconditional forward `JUMP t` at `pc` when every instruction
//!   strictly between `pc` and `t` is erased too (in particular `t == pc + 1`).
//!   [`super::retain_instructions`] relocates a branch to an erased
//!   instruction to the next kept one, which is where the erased jump went.
//!
//! An unconditional `JUMP` has no effect other than transferring control and
//! being charged, so neither rewrite changes what a program computes; both
//! only lower the charged instruction count. The CFG keeps its shape up to
//! edge contraction through blocks that hold only a `JUMP`, so a reducible
//! CFG stays reducible.
//!
//! [`check_jump_thread`] re-checks a run from its certificate (the keep mask
//! and the new target of every branch).

use super::retain_instructions;
use crate::CsilInstr;
use pinewasm_span::Span;

/// The certificate of the pass: its keep mask, and for every instruction the
/// new branch target (`u32::MAX` for an instruction that is not a branch).
pub(super) type ThreadCert = (Vec<bool>, Vec<u32>);

/// The branch target of `instr`, if it is a branch.
fn target_of(instr: &CsilInstr) -> Option<u32> {
    match instr {
        CsilInstr::Jump { target }
        | CsilInstr::JumpIfFalse { target }
        | CsilInstr::JumpIfExact { target, .. }
        | CsilInstr::JumpIfSlotWritten { target, .. } => Some(*target),
        _ => None,
    }
}

/// `instr` with its branch target replaced by `target` (unchanged when it is
/// not a branch).
fn retarget(instr: &mut CsilInstr, new_target: u32) {
    match instr {
        CsilInstr::Jump { target }
        | CsilInstr::JumpIfFalse { target }
        | CsilInstr::JumpIfExact { target, .. }
        | CsilInstr::JumpIfSlotWritten { target, .. } => *target = new_target,
        _ => {}
    }
}

/// For every instruction, the end of the chain of unconditional jumps
/// starting there (the instruction itself when it is not a `JUMP`), or `None`
/// when that chain runs into a cycle. Every jump target must lie inside
/// `code`.
///
/// Linear time: each walk stops at an instruction already resolved and
/// resolves the whole path it took, so every instruction is walked once.
/// Replay runs this on untrusted budget input, so a long chain of
/// `JUMP pc+1` must not cost a walk per branch.
fn chain_ends(code: &[CsilInstr]) -> Vec<Option<u32>> {
    const UNSEEN: u8 = 0;
    const ON_PATH: u8 = 1;
    const DONE: u8 = 2;
    let mut state = vec![UNSEEN; code.len()];
    let mut ends: Vec<Option<u32>> = vec![None; code.len()];
    let mut path = Vec::new();
    for start in 0..code.len() {
        if state[start] == DONE {
            continue;
        }
        let mut at = start;
        let end = loop {
            if state[at] == DONE {
                break ends[at];
            }
            if state[at] == ON_PATH {
                break None;
            }
            match code[at] {
                CsilInstr::Jump { target } => {
                    state[at] = ON_PATH;
                    path.push(at);
                    at = target as usize;
                }
                _ => {
                    state[at] = DONE;
                    ends[at] = Some(at as u32);
                    break ends[at];
                }
            }
        };
        for pc in path.drain(..) {
            state[pc] = DONE;
            ends[pc] = end;
        }
    }
    ends
}

/// The analysis of the pass: its certificate. Code with a branch target past
/// its last instruction is left alone.
#[allow(clippy::ptr_arg)]
pub(super) fn jump_thread_plan(code: &Vec<CsilInstr>) -> ThreadCert {
    let n = code.len();
    let mut keep = vec![true; n];
    let mut targets = vec![u32::MAX; n];
    for (pc, instr) in code.iter().enumerate() {
        if let Some(target) = target_of(instr) {
            if target as usize >= n {
                let unchanged = code.iter().map(|i| target_of(i).unwrap_or(u32::MAX));
                return (keep, unchanged.collect());
            }
            targets[pc] = target;
        }
    }
    let ends = chain_ends(code);
    for target in &mut targets {
        if *target != u32::MAX {
            *target = ends[*target as usize].unwrap_or(*target);
        }
    }
    // Backwards, so the erasure of every later instruction is known.
    // `next_kept[i]` is the first kept index at or after `i`.
    let mut next_kept = vec![n; n + 1];
    for pc in (0..n).rev() {
        if matches!(code[pc], CsilInstr::Jump { .. }) {
            let target = targets[pc] as usize;
            if target > pc && next_kept[pc + 1] >= target {
                keep[pc] = false;
            }
        }
        next_kept[pc] = if keep[pc] { pc } else { next_kept[pc + 1] };
    }
    (keep, targets)
}

/// Apply a certificate: retarget every branch, then erase and relocate.
pub(super) fn apply_jump_thread(
    code: &mut Vec<CsilInstr>,
    spans: &mut Vec<Span>,
    (keep, targets): &ThreadCert,
) {
    for (instr, target) in code.iter_mut().zip(targets) {
        if *target != u32::MAX {
            retarget(instr, *target);
        }
    }
    retain_instructions(code, spans, keep);
}

/// Run the pass in place and return its certificate.
pub(super) fn thread_jumps(code: &mut Vec<CsilInstr>, spans: &mut Vec<Span>) -> ThreadCert {
    let cert = jump_thread_plan(code);
    apply_jump_thread(code, spans, &cert);
    cert
}

/// Why [`check_jump_thread`] rejected a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpThreadCheckError {
    /// Span vectors, the keep mask or the target table do not align with the
    /// code, or the input is too long.
    Shape,
    /// The target table names a target for a non-branch, none for a branch,
    /// or a target past the end.
    TargetTable,
    /// A new target is not the end of a chain of unconditional jumps from the
    /// original target.
    NotAChain,
    /// An erased instruction is not an unconditional forward jump over erased
    /// instructions only.
    NotErasable,
    /// The output is not the input retargeted, erased and relocated.
    Output,
}

/// Check one run of the pass against its certificate.
pub(super) fn check_jump_thread(
    before: &[CsilInstr],
    before_spans: &[Span],
    after: &[CsilInstr],
    after_spans: &[Span],
    (keep, targets): &ThreadCert,
) -> Result<(), JumpThreadCheckError> {
    let n = before.len();
    if before_spans.len() != n
        || after_spans.len() != after.len()
        || keep.len() != n
        || targets.len() != n
        || n >= u32::MAX as usize
    {
        return Err(JumpThreadCheckError::Shape);
    }
    for pc in 0..n {
        match target_of(&before[pc]) {
            None if targets[pc] != u32::MAX => return Err(JumpThreadCheckError::TargetTable),
            None => {}
            Some(original) => {
                if original == targets[pc] {
                    continue;
                }
                if targets[pc] as usize >= n || original as usize >= n {
                    return Err(JumpThreadCheckError::TargetTable);
                }
                // Walk the chain of unconditional jumps from the original
                // target; the new target must be on it.
                let mut at = original;
                let mut steps = 0;
                while at != targets[pc] {
                    let CsilInstr::Jump { target: next } = before[at as usize] else {
                        return Err(JumpThreadCheckError::NotAChain);
                    };
                    if steps >= n || next as usize >= n {
                        return Err(JumpThreadCheckError::NotAChain);
                    }
                    at = next;
                    steps += 1;
                }
            }
        }
    }
    for pc in 0..n {
        if keep[pc] {
            continue;
        }
        let erasable = matches!(before[pc], CsilInstr::Jump { .. })
            && targets[pc] as usize > pc
            && targets[pc] as usize <= n
            && (pc + 1..targets[pc] as usize).all(|between| !keep[between]);
        if !erasable {
            return Err(JumpThreadCheckError::NotErasable);
        }
    }
    let mut expected = before.to_vec();
    let mut expected_spans = before_spans.to_vec();
    apply_jump_thread(
        &mut expected,
        &mut expected_spans,
        &(keep.clone(), targets.clone()),
    );
    if expected.len() != after.len()
        || expected_spans != after_spans
        || expected
            .iter()
            .zip(after)
            .any(|(left, right)| !crate::instr_exact_eq(left, right))
    {
        return Err(JumpThreadCheckError::Output);
    }
    Ok(())
}
