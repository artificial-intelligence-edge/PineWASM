//! Operand provenance over a program's main function.
//!
//! A small abstract interpretation shared by the passes that rewrite CSIL:
//! for every instruction, which instructions produced each of its operands,
//! and for every series slot, which instructions produced the values stored
//! to it. `Dup`, `Pop` and slot stores are plumbing and forward provenance;
//! every other instruction is a node that produces its own results.

use crate::{CsilInstr, CsilProgram, StackEffectRule, TaStateDecl};
use std::collections::BTreeSet;

/// The instructions (by pc) that may have produced one value.
pub type Producers = BTreeSet<usize>;

/// True pops and pushes, with `CallTa`'s arity read from its state table.
/// `None` when the state table does not describe the instruction.
pub fn effect(program: &CsilProgram, instr: &CsilInstr) -> Option<(usize, usize)> {
    effect_in(&program.ta_states, instr)
}

/// [`effect`] for code that is not yet part of a program.
pub fn effect_in(ta_states: &[TaStateDecl], instr: &CsilInstr) -> Option<(usize, usize)> {
    match instr.stack_effect_rule() {
        StackEffectRule::Fixed { pops, pushes } => Some((pops as usize, pushes as usize)),
        StackEffectRule::Payload { pops, pushes } => Some((pops, pushes)),
        StackEffectRule::TaState { state } => {
            let decl = ta_states.get(state as usize)?;
            Some((
                decl.func.engine_operands().len(),
                decl.func.engine_result_arity() as usize,
            ))
        }
    }
}

/// Which nodes produced each operand of each instruction, and which nodes
/// produced every value ever stored to each slot.
pub struct Flow {
    pub operands: Vec<Vec<Producers>>,
    pub slots: Vec<Producers>,
    /// Whether control can reach each instruction. An unreachable one has no
    /// recorded operands, so a pass must skip it rather than read them.
    pub reached: Vec<bool>,
}

impl Flow {
    /// The flow of `program`'s main function. `None` when the analysis
    /// cannot describe it (see [`Flow::build_code`]).
    pub fn build(program: &CsilProgram) -> Option<Self> {
        Self::build_code(
            &program.main().code,
            &program.ta_states,
            program.series_slots.len(),
        )
    }

    /// The flow of one function body that is not yet part of a program.
    /// `None` when the analysis cannot describe it: empty code, a branch out
    /// of range, a stack underflow, predecessors that disagree about the
    /// stack height, a slot or TA state outside its table, or no fixed point
    /// within the iteration cap.
    pub fn build_code(
        code: &[CsilInstr],
        ta_states: &[TaStateDecl],
        slot_count: usize,
    ) -> Option<Self> {
        let n = code.len();
        if n == 0 {
            return None;
        }

        // Basic blocks: a leader is the entry, every branch target, and the
        // instruction after a branch or return.
        let mut leader = vec![false; n];
        leader[0] = true;
        for (pc, instr) in code.iter().enumerate() {
            let targets: &[u32] = match instr {
                CsilInstr::Jump { target }
                | CsilInstr::JumpIfFalse { target }
                | CsilInstr::JumpIfExact { target, .. }
                | CsilInstr::JumpIfSlotWritten { target, .. } => std::slice::from_ref(target),
                CsilInstr::Return => &[],
                _ => continue,
            };
            for &target in targets {
                *leader.get_mut(target as usize)? = true;
            }
            if pc + 1 < n {
                leader[pc + 1] = true;
            }
        }
        let starts: Vec<usize> = (0..n).filter(|&pc| leader[pc]).collect();
        let block_of = |pc: usize| starts.partition_point(|&start| start <= pc) - 1;

        let mut operands: Vec<Vec<Producers>> = vec![Vec::new(); n];
        let mut slots: Vec<Producers> = vec![Producers::new(); slot_count];
        let mut entry: Vec<Option<Vec<Producers>>> = vec![None; starts.len()];
        entry[0] = Some(Vec::new());

        let cap = starts.len() + n + slots.len() + 4;
        for _ in 0..cap {
            let mut changed = false;
            for (block, &start) in starts.iter().enumerate() {
                let Some(mut stack) = entry[block].clone() else {
                    continue;
                };
                let end = starts.get(block + 1).copied().unwrap_or(n);
                for pc in start..end {
                    let instr = &code[pc];
                    match instr {
                        CsilInstr::Dup => {
                            let value = stack.pop()?;
                            stack.push(value.clone());
                            stack.push(value);
                        }
                        CsilInstr::Pop => {
                            stack.pop()?;
                        }
                        CsilInstr::StoreUserSlotCurrent { slot } => {
                            let value = stack.pop()?;
                            changed |= merge(slots.get_mut(*slot as usize)?, &value);
                        }
                        _ => {
                            let (pops, pushes) = effect_in(ta_states, instr)?;
                            if stack.len() < pops {
                                return None;
                            }
                            let args = stack.split_off(stack.len() - pops);
                            let seen = &mut operands[pc];
                            if seen.len() < pops {
                                seen.resize(pops, Producers::new());
                                changed = true;
                            }
                            for (slot, arg) in seen.iter_mut().zip(&args) {
                                changed |= merge(slot, arg);
                            }
                            for _ in 0..pushes {
                                stack.push(Producers::from([pc]));
                            }
                        }
                    }
                }
                let last = &code[end - 1];
                let mut successors = Vec::with_capacity(2);
                match last {
                    CsilInstr::Return => {}
                    CsilInstr::Jump { target } => successors.push(*target as usize),
                    CsilInstr::JumpIfFalse { target }
                    | CsilInstr::JumpIfExact { target, .. }
                    | CsilInstr::JumpIfSlotWritten { target, .. } => {
                        successors.push(*target as usize);
                        successors.push(end);
                    }
                    _ => successors.push(end),
                }
                for successor in successors {
                    if successor >= n {
                        return None;
                    }
                    let target = &mut entry[block_of(successor)];
                    match target {
                        None => {
                            *target = Some(stack.clone());
                            changed = true;
                        }
                        Some(current) => {
                            if current.len() != stack.len() {
                                return None;
                            }
                            for (left, right) in current.iter_mut().zip(&stack) {
                                changed |= merge(left, right);
                            }
                        }
                    }
                }
            }
            if !changed {
                let reached = (0..n).map(|pc| entry[block_of(pc)].is_some()).collect();
                return Some(Self {
                    operands,
                    slots,
                    reached,
                });
            }
        }
        None
    }
}

fn merge(into: &mut Producers, from: &Producers) -> bool {
    let before = into.len();
    into.extend(from.iter().copied());
    into.len() != before
}
