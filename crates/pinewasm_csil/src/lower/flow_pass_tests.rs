//! Tests of the flow-alias and jump-threading passes: unit fixtures, checker
//! rejections, and a randomized semantic differential that runs structured
//! programs (loops, branches, copies, history reads, `var` slots) through a
//! small interpreter of the test's own instruction subset, run before and
//! after the whole optimization pipeline.

use super::flow_alias::{
    check_flow_alias, flow_alias_plan, flow_alias_rounds, FlowAliasCheckError,
};
use super::jump_thread::{
    apply_jump_thread, check_jump_thread, jump_thread_plan, thread_jumps, JumpThreadCheckError,
};
use super::{optimize, optimize_validated, rename_entry_aliases, retain_instructions};
use crate::CsilInstr;
use crate::CsilInstr::{
    CmpLt, IntAdd, Jump, JumpIfFalse, Pop, PushI64, ReadUserSlot, Return, StoreUserSlotCurrent,
};
use pinewasm_mir::SlotDef;
use pinewasm_span::{SourceId, Span};

fn slots(count: usize) -> Vec<SlotDef> {
    (0..count)
        .map(|index| SlotDef {
            name: format!("s{index}"),
            persistent: false,
            varip: false,
            lane: None,
        })
        .collect()
}

fn spans(len: usize) -> Vec<Span> {
    (0..len as u32)
        .map(|index| Span::new(SourceId(3), index, index + 1))
        .collect()
}

fn read(slot: u32) -> CsilInstr {
    ReadUserSlot { slot, offset: 0 }
}

fn store(slot: u32) -> CsilInstr {
    StoreUserSlotCurrent { slot }
}

/// `for i = 0 to 2: p = i (the inlined parameter copy); output(p)`.
fn loop_copy() -> Vec<CsilInstr> {
    vec![
        PushI64(0),                 // 0
        store(0),                   // 1  i = 0
        read(0),                    // 2  loop head
        PushI64(3),                 // 3
        CmpLt,                      // 4
        JumpIfFalse { target: 17 }, // 5
        read(0),                    // 6  copy
        store(1),                   // 7
        read(1),                    // 8  use
        Pop,                        // 9
        read(1),                    // 10 second use
        Pop,                        // 11
        read(0),                    // 12 increment
        PushI64(1),                 // 13
        IntAdd,                     // 14
        store(0),                   // 15
        Jump { target: 2 },         // 16
        Return,                     // 17
    ]
}

/// One full flow-alias stage on `code`.
fn flow(code: &[CsilInstr], slots: &[SlotDef]) -> Vec<CsilInstr> {
    let (mut code, mut spans) = (code.to_vec(), spans(code.len()));
    flow_alias_rounds(&mut code, &mut spans, slots);
    code
}

#[test]
fn loop_parameter_copy_reads_the_argument_slot() {
    let code = loop_copy();
    let out = flow(&code, &slots(2));
    assert_eq!(out.len(), code.len() - 2);
    assert!(!out.contains(&store(1)), "{out:?}");
    assert!(!out.contains(&read(1)), "{out:?}");
    assert_eq!(out[6], read(0));
}

#[test]
fn history_observed_parameter_keeps_its_storage() {
    let mut code = loop_copy();
    code[10] = ReadUserSlot { slot: 1, offset: 1 };
    assert_eq!(flow(&code, &slots(2)), code);
}

#[test]
fn persistent_destination_keeps_its_storage() {
    let code = loop_copy();
    let mut defs = slots(2);
    defs[1].persistent = true;
    assert_eq!(flow(&code, &defs), code);
}

#[test]
fn store_to_the_source_between_copy_and_read_keeps_the_copy() {
    let mut code = loop_copy();
    // Replace the first use with `i = 7` (a store to the source).
    code[8] = PushI64(7);
    code[9] = store(0);
    assert_eq!(flow(&code, &slots(2)), code);
}

#[test]
fn read_before_the_copy_keeps_the_copy() {
    let mut code = loop_copy();
    // The loop head reads `p` before the copy has run on the first iteration.
    code[2] = read(1);
    code[3] = Pop;
    code[4] = read(0);
    code.insert(5, PushI64(3));
    code.insert(6, CmpLt);
    // Retarget after the two inserted instructions.
    for instr in &mut code {
        match instr {
            JumpIfFalse { target } | Jump { target } if *target > 4 => *target += 2,
            _ => {}
        }
    }
    let (keep, aliases) = flow_alias_plan(&code, &slots(2));
    assert!(keep.iter().all(|kept| *kept), "{code:?}");
    assert_eq!(aliases, vec![0, 1]);
}

#[test]
fn branch_into_the_store_keeps_the_copy() {
    let mut code = loop_copy();
    code.push(Jump { target: 7 });
    assert_eq!(flow(&code, &slots(2)), code);
}

#[test]
fn copy_chains_resolve_in_successive_rounds() {
    // s0 -> s1 -> s2 inside the loop body.
    let code = vec![
        PushI64(0),
        store(0),
        read(0),
        PushI64(3),
        CmpLt,
        JumpIfFalse { target: 19 },
        read(0),
        store(1),
        read(1),
        store(2),
        read(2),
        Pop,
        read(2),
        Pop,
        read(0),
        PushI64(1),
        IntAdd,
        store(0),
        Jump { target: 2 },
        Return,
    ];
    let (mut out, mut out_spans) = (code.clone(), spans(code.len()));
    let rounds = flow_alias_rounds(&mut out, &mut out_spans, &slots(3));
    assert_eq!(rounds.len(), 2, "{rounds:?}");
    assert_eq!(out.len(), code.len() - 4);
    assert_eq!(out.iter().filter(|i| **i == read(0)).count(), 4, "{out:?}");
}

#[test]
fn checker_accepts_the_pass_and_rejects_broken_certificates() {
    let code = loop_copy();
    let defs = slots(2);
    let before_spans = spans(code.len());
    let (keep, table) = flow_alias_plan(&code, &defs);
    let (mut after, mut after_spans) = (code.clone(), before_spans.clone());
    rename_entry_aliases(&mut after, &table);
    retain_instructions(&mut after, &mut after_spans, &keep);
    let check = |code: &[CsilInstr], keep: &[bool], table: &[u32]| {
        let (mut after, mut after_spans) = (code.to_vec(), spans(code.len()));
        rename_entry_aliases(&mut after, &table.to_vec());
        retain_instructions(&mut after, &mut after_spans, keep);
        check_flow_alias(
            code,
            &spans(code.len()),
            &after,
            &after_spans,
            keep,
            table,
            &defs,
        )
    };
    assert_eq!(
        check_flow_alias(
            &code,
            &before_spans,
            &after,
            &after_spans,
            &keep,
            &table,
            &defs
        ),
        Ok(())
    );

    // A store to the source between the copy and a read.
    let mut killed = code.clone();
    killed[8] = PushI64(7);
    killed[9] = store(0);
    assert_eq!(
        check(&killed, &keep, &table),
        Err(FlowAliasCheckError::NotAvailable)
    );
    // A read before the copy.
    let mut early = code.clone();
    early[0] = read(1);
    early[1] = Pop;
    assert_eq!(
        check(&early, &keep, &table),
        Err(FlowAliasCheckError::NotAvailable)
    );
    // Erasing an instruction that is not part of the copy.
    let mut stray = keep.clone();
    stray[3] = false;
    assert_eq!(
        check(&code, &stray, &table),
        Err(FlowAliasCheckError::StrayErasure)
    );
    // Renaming without erasing the copy.
    assert_eq!(
        check(&code, &vec![true; code.len()], &table),
        Err(FlowAliasCheckError::NotCopy)
    );
    // A renamed source that is itself renamed.
    assert_eq!(
        check(&code, &keep, &[1, 0]),
        Err(FlowAliasCheckError::Chain)
    );
    // An observed destination.
    let mut observed = code.clone();
    observed[10] = ReadUserSlot { slot: 1, offset: 1 };
    assert_eq!(
        check(&observed, &keep, &table),
        Err(FlowAliasCheckError::Observed)
    );
    // The output must be the renamed, erased input.
    let mut wrong = after.clone();
    wrong[6] = read(1);
    assert!(matches!(
        check_flow_alias(
            &code,
            &before_spans,
            &wrong,
            &after_spans,
            &keep,
            &table,
            &defs
        ),
        Err(FlowAliasCheckError::Output(_))
    ));
}

#[test]
fn jumps_to_the_next_instruction_are_erased_and_chains_threaded() {
    let code = vec![
        PushI64(1),                // 0
        CmpLt,                     // 1 (operands irrelevant here)
        JumpIfFalse { target: 6 }, // 2 -> JUMP 7 -> 8: threaded to 8
        Jump { target: 4 },        // 3 next instruction: erased
        PushI64(2),                // 4
        Jump { target: 8 },        // 5 over an erased jump only: erased
        Jump { target: 7 },        // 6 erased: forward over nothing
        Jump { target: 8 },        // 7 erased
        Return,                    // 8
    ];
    let (keep, targets) = jump_thread_plan(&code);
    assert_eq!(targets[2], 8);
    assert_eq!(
        keep,
        vec![true, true, true, false, true, false, false, false, true]
    );
    let (mut out, mut out_spans) = (code.clone(), spans(code.len()));
    thread_jumps(&mut out, &mut out_spans);
    assert_eq!(
        out,
        vec![
            PushI64(1),
            CmpLt,
            JumpIfFalse { target: 4 },
            PushI64(2),
            Return
        ]
    );
}

#[test]
fn cyclic_jump_chains_are_left_alone() {
    let code = vec![Jump { target: 1 }, Jump { target: 0 }, Return];
    let (mut out, mut out_spans) = (code.clone(), spans(code.len()));
    let cert = thread_jumps(&mut out, &mut out_spans);
    // The forward jump into the cycle is erased; the cycle survives.
    assert_eq!(out, vec![Jump { target: 0 }, Return]);
    assert_eq!(
        check_jump_thread(&code, &spans(3), &out, &out_spans, &cert),
        Ok(())
    );
}

#[test]
fn branch_past_the_end_leaves_code_alone() {
    let code = vec![Jump { target: 1 }, JumpIfFalse { target: 2 }];
    let (mut out, mut out_spans) = (code.clone(), spans(code.len()));
    thread_jumps(&mut out, &mut out_spans);
    assert_eq!(out, code);
}

#[test]
fn jump_thread_checker_rejects_broken_certificates() {
    let code = vec![
        PushI64(1),
        JumpIfFalse { target: 4 },
        Jump { target: 3 },
        PushI64(2),
        Jump { target: 6 },
        PushI64(3),
        Return,
    ];
    let before_spans = spans(code.len());
    let check = |keep: Vec<bool>, targets: Vec<u32>| {
        let cert = (keep, targets);
        let (mut after, mut after_spans) = (code.clone(), before_spans.clone());
        apply_jump_thread(&mut after, &mut after_spans, &cert);
        check_jump_thread(&code, &before_spans, &after, &after_spans, &cert)
    };
    let (keep, targets) = jump_thread_plan(&code);
    assert_eq!(check(keep.clone(), targets.clone()), Ok(()));

    // Retargeting off the chain.
    let mut off_chain = targets.clone();
    off_chain[1] = 5;
    assert_eq!(
        check(keep.clone(), off_chain),
        Err(JumpThreadCheckError::NotAChain)
    );
    // Erasing a non-jump.
    let mut non_jump = keep.clone();
    non_jump[5] = false;
    assert_eq!(
        check(non_jump, targets.clone()),
        Err(JumpThreadCheckError::NotErasable)
    );
    // Erasing a forward jump over a kept instruction.
    let mut over_kept = vec![true; code.len()];
    over_kept[4] = false;
    assert_eq!(
        check(over_kept, targets.clone()),
        Err(JumpThreadCheckError::NotErasable)
    );
    // A target for a non-branch.
    let mut stray = targets.clone();
    stray[0] = 3;
    assert_eq!(
        check(keep.clone(), stray),
        Err(JumpThreadCheckError::TargetTable)
    );
    // The output must be the rewritten input.
    let cert = (keep, targets);
    let (mut after, mut after_spans) = (code.clone(), before_spans.clone());
    apply_jump_thread(&mut after, &mut after_spans, &cert);
    after[0] = PushI64(9);
    assert_eq!(
        check_jump_thread(&code, &before_spans, &after, &after_spans, &cert),
        Err(JumpThreadCheckError::Output)
    );
}

// ---------------------------------------------------------------------------
// Randomized semantic differential.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum V {
    Na,
    I(i64),
    B(bool),
}

/// Outputs (popped values, then a bar marker per completed bar) and the
/// instructions executed per bar. `None` when a bar ran out of steps.
type Trace = Option<(Vec<V>, Vec<u64>)>;

/// An interpreter for the instruction subset the generator emits.
/// Non-persistent slots start every bar as `na`; persistent slots carry the
/// previous bar's final value; `READ s 1` reads the previous bar's final
/// value.
fn interpret(code: &[CsilInstr], defs: &[SlotDef], bars: usize) -> Trace {
    let mut history: Vec<Vec<V>> = vec![Vec::new(); defs.len()];
    let mut out = Vec::new();
    let mut steps_per_bar = Vec::new();
    for bar in 0..bars {
        let mut current: Vec<V> = (0..defs.len())
            .map(|slot| {
                if defs[slot].persistent && bar > 0 {
                    history[slot][bar - 1]
                } else {
                    V::Na
                }
            })
            .collect();
        let mut stack: Vec<V> = Vec::new();
        let mut pc = 0;
        let mut steps = 0u64;
        while pc < code.len() {
            steps += 1;
            if steps > 100_000 {
                return None;
            }
            let mut next = pc + 1;
            match code[pc] {
                PushI64(value) => stack.push(V::I(value)),
                ReadUserSlot { slot, offset } => stack.push(if offset == 0 {
                    current[slot as usize]
                } else if bar >= offset as usize {
                    history[slot as usize][bar - offset as usize]
                } else {
                    V::Na
                }),
                StoreUserSlotCurrent { slot } => current[slot as usize] = stack.pop()?,
                IntAdd => {
                    let (b, a) = (stack.pop()?, stack.pop()?);
                    stack.push(match (a, b) {
                        (V::I(a), V::I(b)) => V::I(a.wrapping_add(b)),
                        _ => V::Na,
                    });
                }
                CmpLt => {
                    let (b, a) = (stack.pop()?, stack.pop()?);
                    stack.push(match (a, b) {
                        (V::I(a), V::I(b)) => V::B(a < b),
                        _ => V::B(false),
                    });
                }
                JumpIfFalse { target } => {
                    if stack.pop()? != V::B(true) {
                        next = target as usize;
                    }
                }
                Jump { target } => next = target as usize,
                Pop => out.push(stack.pop()?),
                Return => break,
                _ => return None,
            }
            pc = next;
        }
        for (cells, value) in history.iter_mut().zip(&current) {
            cells.push(*value);
        }
        out.push(V::B(true));
        steps_per_bar.push(steps);
    }
    Some((out, steps_per_bar))
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

/// Slot layout of generated programs: general slots, loop counters (one per
/// nesting depth, never assigned by statements), parameter slots (written
/// only by copies).
const GENERAL: u32 = 4;
const COUNTERS: u32 = 3;
const PARAMS: u32 = 6;

struct Gen<'a> {
    rng: &'a mut Rng,
    code: Vec<CsilInstr>,
}

impl Gen<'_> {
    fn emit(&mut self, instr: CsilInstr) -> usize {
        self.code.push(instr);
        self.code.len() - 1
    }

    fn patch(&mut self, at: usize, to: usize) {
        match &mut self.code[at] {
            Jump { target } | JumpIfFalse { target } => *target = to as u32,
            _ => unreachable!(),
        }
    }

    fn readable(&mut self, depth: u32) -> u32 {
        match self.rng.below(3) {
            0 if depth > 0 => GENERAL + self.rng.below(u64::from(depth)) as u32,
            1 => GENERAL + COUNTERS + self.rng.below(u64::from(PARAMS)) as u32,
            _ => self.rng.below(u64::from(GENERAL)) as u32,
        }
    }

    fn expr(&mut self, depth: u32, size: u32) {
        match self.rng.below(if size == 0 { 2 } else { 3 }) {
            0 => {
                let value = self.rng.below(5) as i64;
                self.emit(PushI64(value));
            }
            1 => {
                let slot = self.readable(depth);
                let offset = u32::from(self.rng.below(5) == 0);
                self.emit(ReadUserSlot { slot, offset });
            }
            _ => {
                self.expr(depth, size - 1);
                self.expr(depth, size - 1);
                self.emit(IntAdd);
            }
        }
    }

    /// Lower like the stackifier: `cond; JUMP_IF_FALSE else; JUMP then`,
    /// each arm ending in `JUMP join`.
    fn block(&mut self, depth: u32, budget: u32) {
        let count = 1 + self.rng.below(4);
        for _ in 0..count {
            self.stmt(depth, budget);
        }
    }

    fn stmt(&mut self, depth: u32, budget: u32) {
        match self.rng.below(if budget == 0 { 3 } else { 5 }) {
            0 => {
                // general = expr
                self.expr(depth, 2);
                let slot = self.rng.below(u64::from(GENERAL)) as u32;
                self.emit(store(slot));
            }
            1 => {
                // param = slot (an inlined argument copy)
                let src = self.readable(depth);
                let dst = GENERAL + COUNTERS + self.rng.below(u64::from(PARAMS)) as u32;
                if src != dst {
                    self.emit(read(src));
                    self.emit(store(dst));
                }
            }
            2 => {
                self.expr(depth, 2);
                self.emit(Pop);
            }
            3 => {
                self.expr(depth, 1);
                self.expr(depth, 1);
                self.emit(CmpLt);
                let to_else = self.emit(JumpIfFalse { target: 0 });
                let to_then = self.emit(Jump { target: 0 });
                let then = self.code.len();
                self.patch(to_then, then);
                self.block(depth, budget - 1);
                let then_join = self.emit(Jump { target: 0 });
                let other = self.code.len();
                self.patch(to_else, other);
                if self.rng.below(2) == 0 {
                    self.block(depth, budget - 1);
                }
                let else_join = self.emit(Jump { target: 0 });
                let join = self.code.len();
                self.patch(then_join, join);
                self.patch(else_join, join);
            }
            _ if depth < COUNTERS => {
                let counter = GENERAL + depth;
                self.emit(PushI64(0));
                self.emit(store(counter));
                let head = self.emit(read(counter));
                let bound = 1 + self.rng.below(3) as i64;
                self.emit(PushI64(bound));
                self.emit(CmpLt);
                let to_exit = self.emit(JumpIfFalse { target: 0 });
                let to_body = self.emit(Jump { target: 0 });
                let body = self.code.len();
                self.patch(to_body, body);
                self.block(depth + 1, budget - 1);
                let to_inc = self.emit(Jump { target: 0 });
                let inc = self.code.len();
                self.patch(to_inc, inc);
                self.emit(read(counter));
                self.emit(PushI64(1));
                self.emit(IntAdd);
                self.emit(store(counter));
                self.emit(Jump {
                    target: head as u32,
                });
                let exit = self.code.len();
                self.patch(to_exit, exit);
            }
            _ => {
                self.expr(depth, 1);
                self.emit(Pop);
            }
        }
    }
}

#[test]
fn optimized_programs_compute_what_the_originals_compute() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let (mut aliased, mut threaded, mut programs) = (0, 0, 0);
    for _ in 0..4_000 {
        let mut defs = slots((GENERAL + COUNTERS + PARAMS) as usize);
        for def in defs.iter_mut().take(GENERAL as usize) {
            def.persistent = rng.below(4) == 0;
        }
        let mut generator = Gen {
            rng: &mut rng,
            code: Vec::new(),
        };
        generator.block(0, 3);
        generator.emit(Return);
        let code = generator.code;

        let Some(expected) = interpret(&code, &defs, 3) else {
            continue;
        };
        let (mut out, mut out_spans) = (code.clone(), spans(code.len()));
        optimize(&mut out, &mut out_spans, &defs);
        let actual = interpret(&out, &defs, 3).expect("optimized program runs");
        assert_eq!(actual.0, expected.0, "code={code:?}\nout={out:?}");
        for (after, before) in actual.1.iter().zip(&expected.1) {
            assert!(after <= before, "optimized ran more instructions");
        }
        // The validated pipeline accepts every stage.
        let (mut checked, mut checked_spans) = (code.clone(), spans(code.len()));
        assert_eq!(
            optimize_validated(&mut checked, &mut checked_spans, &defs),
            Ok(())
        );
        assert_eq!(checked, out);

        let (keep, _) = flow_alias_plan(&code, &defs);
        aliased += usize::from(keep.iter().any(|kept| !kept));
        let (keep, _) = jump_thread_plan(&code);
        threaded += usize::from(keep.iter().any(|kept| !kept));
        programs += 1;
    }
    assert!(programs > 3_000, "only {programs} programs ran");
    assert!(aliased > 500, "flow alias fired on only {aliased} programs");
    assert!(
        threaded > 1_000,
        "threading fired on only {threaded} programs"
    );
}

// ---------------------------------------------------------------------------
// Bounded work on adversarial input. Validation replays these passes on a
// serialized budget input, which is untrusted.

/// A long chain of `JUMP pc+1` with a branch into it from every link costs
/// linear time: the chain is resolved once, not walked per branch.
#[test]
fn a_long_jump_chain_is_threaded_in_linear_time() {
    const LINKS: usize = 200_000;
    let mut code: Vec<CsilInstr> = (1..=LINKS as u32)
        .map(|next| Jump { target: next })
        .collect();
    code.push(Return);
    let (mut out, mut out_spans) = (code.clone(), spans(code.len()));
    let cert = thread_jumps(&mut out, &mut out_spans);
    assert_eq!(out, vec![Return]);
    assert!(cert.1[..LINKS].iter().all(|target| *target == LINKS as u32));

    // The same chain closed into a cycle is left as a cycle.
    let mut cyclic = code;
    cyclic[LINKS - 1] = Jump { target: 0 };
    let (mut out, mut out_spans) = (cyclic.clone(), spans(cyclic.len()));
    thread_jumps(&mut out, &mut out_spans);
    assert_eq!(out.last(), Some(&Return));
    assert!(out.len() > 1, "the cycle survives");
}

/// `count` copies, each into its own slot and its own block, each read
/// twice afterwards.
fn many_block_copies(count: u32) -> (Vec<CsilInstr>, Vec<SlotDef>) {
    let mut code = vec![PushI64(1), store(0)];
    for dst in 1..=count {
        code.push(read(0));
        code.push(store(dst));
        let next = code.len() as u32 + 1;
        code.push(JumpIfFalse { target: next });
        code.push(PushI64(0));
    }
    for dst in 1..=count {
        for _ in 0..2 {
            code.push(read(dst));
            code.push(Pop);
        }
    }
    code.push(Return);
    (code, slots(count as usize + 1))
}

#[test]
fn many_blocks_times_many_copies_skips_the_analysis() {
    // Small: the copies are removed.
    let (code, defs) = many_block_copies(100);
    let (keep, _) = flow_alias_plan(&code, &defs);
    assert_eq!(keep.iter().filter(|kept| !**kept).count(), 200);

    // Past the matrix bound: the round changes nothing, decided before any
    // matrix is allocated.
    let (code, defs) = many_block_copies(12_000);
    let blocks = code
        .iter()
        .filter(|instr| matches!(instr, JumpIfFalse { .. }))
        .count();
    assert!(blocks * 12_000usize.div_ceil(64) > super::flow_alias::MAX_MATRIX_WORDS);
    let (keep, aliases) = flow_alias_plan(&code, &defs);
    assert!(keep.iter().all(|kept| *kept));
    assert!(aliases
        .iter()
        .enumerate()
        .all(|(slot, alias)| *alias as usize == slot));
    // The whole pipeline is still a deterministic function of its input, so
    // a serialized budget input replays to the same code.
    let (mut once, mut once_spans) = (code.clone(), spans(code.len()));
    optimize(&mut once, &mut once_spans, &defs);
    let (mut twice, mut twice_spans) = (code.clone(), spans(code.len()));
    optimize(&mut twice, &mut twice_spans, &defs);
    assert_eq!((once, once_spans), (twice, twice_spans));
}

#[test]
fn many_stores_times_many_copies_of_one_slot_skips_the_analysis() {
    // 5,000 copies of slot 0 and 5,000 stores to it: 25 million
    // invalidations, past the bound.
    let mut code = Vec::new();
    for dst in 1..=5_000u32 {
        code.push(PushI64(1));
        code.push(store(0));
        code.push(read(0));
        code.push(store(dst));
        code.push(read(dst));
        code.push(Pop);
        code.push(read(dst));
        code.push(Pop);
    }
    code.push(Return);
    let defs = slots(5_001);
    let (keep, _) = flow_alias_plan(&code, &defs);
    assert!(keep.iter().all(|kept| *kept));

    // Under the bound the same shape is optimized.
    let small: Vec<CsilInstr> = code[..8 * 100].iter().cloned().chain([Return]).collect();
    let (keep, _) = flow_alias_plan(&small, &slots(101));
    assert!(keep.iter().any(|kept| !*kept));
}
