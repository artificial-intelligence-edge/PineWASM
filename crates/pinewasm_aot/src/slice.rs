//! Standalone WebAssembly-only signal slice: erase what cannot reach the
//! designated plot.
//!
//! A plot-designated standalone WebAssembly module reports one thing, the designated plot's
//! per-bar value. Everything else a script does (other plots, bar colors,
//! labels, tables, alerts, and the arithmetic feeding them) is invisible in
//! its output, yet would otherwise be typed and lowered like the rest. One opcode
//! the importless tier cannot run, or one typing gap, anywhere in that
//! display code refused the whole script.
//!
//! A `strategy()` source reports what its broker did, and on request the
//! value of every plot. Its output is anchored to its orders and its plot
//! values instead of one plot (see [`Anchor`]). Order comments and their
//! variants can reach the trade ledger and remain live. Alert messages
//! appear in no output and are erased with the rest.
//!
//! This pass runs before typing. It proves which instructions can influence
//! the module's output and rewrites every other value-producing or effecting
//! instruction into its stack shape: one `Pop` per operand, one `PushNa` per
//! result. `na` is typed by demand, so the placeholders unify with whatever
//! the surviving plumbing expects.
//!
//! The proof is deliberately coarse and fails closed:
//!
//! * a conditional jump is kept unless nothing it decides can reach the
//!   output: no live instruction, and no store to a slot a live instruction
//!   reads, lies between the jump and the point where its paths rejoin.
//!   Such a jump becomes a plain jump to that point, so a branch or loop
//!   that only feeds chart output is skipped whole, and its condition is
//!   erased like any other display value. A jump whose paths never rejoin,
//!   or rejoin with the operand stack rearranged, stays as written (see
//!   [`Control`]);
//! * a `var` slot whose run-once guard decides anything live keeps every
//!   store to it, so a skipped store can never change which way a guard
//!   that matters goes;
//! * slot stores, `Dup`, `Pop` and every other jump are never rewritten, so
//!   `var` initializers keep their run-once guard;
//! * storage is flow-insensitive: a live read of a slot keeps every value
//!   ever stored to it;
//! * heap objects are grouped into alias classes (anything stored in a
//!   container aliases what is read back out of it), and a live use of any
//!   member keeps every mutation of the class;
//! * a value that provably cannot be a heap handle joins no class (see
//!   `handle_free_nodes`), so a number passed from one object to another
//!   does not tie the two together;
//! * instructions with an effect the output can observe indirectly
//!   (`runtime.error`, `math.random`, strategy orders) are always live, as
//!   is any opcode this pass does not know.
//!
//! Erasing display code changes two things a caller can notice: a runtime
//! error raised only by erased code no longer stops the run (a display loop
//! that never ends included), and an erased instruction never costs more
//! budget than it did (its first placeholder is charged one unit, the rest
//! are free; see [`Sliced::budget_free`]).

use pinewasm_csil::{CsilInstr, CsilProgram, StackEffectRule};
use pinewasm_hir::{CollectionFunc, DrawKind, OrderKind, PureFunc, TaInputTarget};
use std::collections::{BTreeMap, BTreeSet};

type Producers = BTreeSet<usize>;

/// What a module's output is made of: where liveness starts.
#[derive(Clone, Copy)]
pub(crate) enum Anchor {
    /// The value of one plot of an indicator source.
    Plot(u32),
    /// A `strategy()` source: every order, and the value of every plot (a
    /// request for full results returns them all).
    Strategy,
}

impl Anchor {
    fn keeps_plot(self, plot_slot: u32) -> bool {
        match self {
            Self::Plot(designated) => designated == plot_slot,
            Self::Strategy => true,
        }
    }
}

/// Alert operands no output carries. Comments remain live because full
/// trade results expose the selected order comment.
fn unobserved_order_operand(kind: OrderKind, operand: usize) -> bool {
    kind.param_names()
        .get(operand)
        .is_some_and(|name| name.starts_with("alert_"))
}

/// A program reduced to what can reach its output.
pub(crate) struct Sliced {
    pub program: CsilProgram,
    /// Placeholder instructions the emitter must not charge to the
    /// instruction budget. An erased instruction with several operands
    /// becomes several placeholders; charging each would make display code
    /// cost more erased than it did running.
    pub budget_free: BTreeSet<usize>,
}

/// The program with every instruction that cannot reach `anchor` erased, or
/// `None` when nothing is erasable or the pass cannot prove its result (the
/// caller then compiles the program unchanged).
pub(crate) fn project_to_signal(program: &CsilProgram, anchor: Anchor) -> Option<Sliced> {
    let strategy_source = program.metadata.strategy.is_some();
    if program.functions.len() != 1 || strategy_source != matches!(anchor, Anchor::Strategy) {
        return None;
    }
    let code = &program.main().code;
    if code.is_empty() || code.len() != program.main().source_map.len() {
        return None;
    }
    let flow = Flow::build(program)?;
    let control = Control::build(program, &flow);
    let live = liveness(program, &flow, &control, anchor)?;

    let mut erased_any = false;
    let mut budget_free = BTreeSet::new();
    let mut new_code = Vec::with_capacity(code.len());
    let mut new_map = Vec::with_capacity(code.len());
    let mut pc_map = Vec::with_capacity(code.len() + 1);
    for (pc, instr) in code.iter().enumerate() {
        pc_map.push(new_code.len() as u32);
        let span = program.main().source_map[pc];
        if let Some(&join) = control.join.get(&pc) {
            if !live[pc] {
                // Nothing this jump decides can reach the output: drop its
                // condition and go straight to where its paths rejoin. The
                // jump itself costs what the conditional one did.
                erased_any = true;
                new_code.push(CsilInstr::Pop);
                new_map.push(span);
                budget_free.insert(new_code.len());
                new_code.push(CsilInstr::Jump {
                    target: join as u32,
                });
                new_map.push(span);
                continue;
            }
        }
        if is_node(instr)
            && !live[pc]
            && !matches!(instr, CsilInstr::PushNa)
            && !carries_input_binding(program, instr)
        {
            erased_any = true;
            let (pops, pushes) = effect(program, instr)?;
            let first = new_code.len();
            for _ in 0..pops {
                new_code.push(CsilInstr::Pop);
                new_map.push(span);
            }
            for _ in 0..pushes {
                new_code.push(CsilInstr::PushNa);
                new_map.push(span);
            }
            budget_free.extend(first + 1..new_code.len());
        } else {
            new_code.push(instr.clone());
            new_map.push(span);
        }
    }
    if !erased_any {
        return None;
    }
    pc_map.push(new_code.len() as u32);
    for instr in &mut new_code {
        match instr {
            CsilInstr::Jump { target }
            | CsilInstr::JumpIfFalse { target }
            | CsilInstr::JumpIfExact { target, .. }
            | CsilInstr::JumpIfSlotWritten { target, .. } => {
                *target = *pc_map.get(*target as usize)?;
            }
            _ => {}
        }
    }

    let mut projected = program.clone();
    let main = projected.functions.first_mut()?;
    main.code = new_code;
    main.source_map = new_map;
    // The interpreter attributes its budget failures to the unsliced code
    // before instruction-deleting passes; the sliced code is no longer an
    // optimization of it, and the standalone tier never reads it.
    main.budget_reference = None;
    pinewasm_csil::validate(&projected).ok()?;
    Some(Sliced {
        program: projected,
        budget_free,
    })
}

/// An input-bound TA length lives in the instruction itself, and the
/// validator requires each binding's target to appear exactly once. Such an
/// instruction stays in place even when dead; it is cheap, native, and its
/// operands may still be placeholders.
fn carries_input_binding(program: &CsilProgram, instr: &CsilInstr) -> bool {
    let target = match instr {
        CsilInstr::WindowMean { slot, .. } => TaInputTarget::Sma { slot: *slot },
        CsilInstr::ReadUserSlot { slot, offset } if *offset != 0 => {
            TaInputTarget::Change { slot: *slot }
        }
        _ => return false,
    };
    program
        .ta_input_bindings
        .iter()
        .any(|binding| binding.target == target)
}

/// True pops and pushes, with `CallTa`'s arity read from its state table.
fn effect(program: &CsilProgram, instr: &CsilInstr) -> Option<(usize, usize)> {
    if let CsilInstr::CallTa { state } = instr {
        let decl = program.ta_states.get(*state as usize)?;
        return Some((
            decl.func.engine_operands().len(),
            decl.func.engine_result_arity() as usize,
        ));
    }
    match instr.stack_effect_rule() {
        StackEffectRule::Fixed { pops, pushes } => Some((pops as usize, pushes as usize)),
        StackEffectRule::Payload { pops, pushes } => Some((pops, pushes)),
        // `CallTa` is resolved above; any other state-dependent effect is
        // one this pass does not know, so it declines to slice.
        StackEffectRule::TaState { .. } => None,
    }
}

/// Instructions the slice reasons about as values or effects. The rest is
/// plumbing and control flow, which is never rewritten.
fn is_node(instr: &CsilInstr) -> bool {
    !matches!(
        instr,
        CsilInstr::Dup
            | CsilInstr::Pop
            | CsilInstr::StoreUserSlotCurrent { .. }
            | CsilInstr::Jump { .. }
            | CsilInstr::JumpIfFalse { .. }
            | CsilInstr::JumpIfExact { .. }
            | CsilInstr::JumpIfSlotWritten { .. }
            | CsilInstr::Return
    )
}

/// The series slot an instruction reads, if any.
fn slot_read(instr: &CsilInstr) -> Option<usize> {
    match instr {
        CsilInstr::ReadUserSlot { slot, .. }
        | CsilInstr::ReadUserSlotDynamic { slot }
        | CsilInstr::WindowMean { slot, .. }
        | CsilInstr::WindowMeanDynamic { slot }
        | CsilInstr::WindowExtremumDynamic { slot, .. }
        | CsilInstr::WindowStatisticDynamic { slot, .. }
        | CsilInstr::EmaSimpleDynamic { slot } => Some(*slot as usize),
        _ => None,
    }
}

/// Nodes that may be erased when nothing live depends on them. Any opcode
/// missing from this list is treated as always live.
fn erasable(instr: &CsilInstr) -> bool {
    match instr {
        CsilInstr::CallPure { func, .. } => !matches!(func, PureFunc::MathRandom),
        CsilInstr::PushNa
        | CsilInstr::PushBool(_)
        | CsilInstr::PushI64(_)
        | CsilInstr::PushF64(_)
        | CsilInstr::PushStr(_)
        | CsilInstr::PushColor { .. }
        | CsilInstr::ReadBuiltin { .. }
        | CsilInstr::ReadUserSlot { .. }
        | CsilInstr::ReadBuiltinDynamic { .. }
        | CsilInstr::ReadUserSlotDynamic { .. }
        | CsilInstr::ReadInput { .. }
        | CsilInstr::ReadSecurity { .. }
        | CsilInstr::ReadSecurityExpr { .. }
        | CsilInstr::ReadRequest { .. }
        | CsilInstr::BuildFootprint { .. }
        | CsilInstr::ReadSecurityLowerTf { .. }
        | CsilInstr::ReadPivotLevels { .. }
        | CsilInstr::ReadTimeTf { .. }
        | CsilInstr::FootprintRowByPrice
        | CsilInstr::WindowMean { .. }
        | CsilInstr::CallCollection { .. }
        | CsilInstr::CallTa { .. }
        | CsilInstr::IntAdd
        | CsilInstr::IntSub
        | CsilInstr::IntMul
        | CsilInstr::IntDiv
        | CsilInstr::PineIntDiv
        | CsilInstr::IntRem
        | CsilInstr::IntNeg
        | CsilInstr::F64Add
        | CsilInstr::F64Sub
        | CsilInstr::F64Mul
        | CsilInstr::F64Div
        | CsilInstr::F64Rem
        | CsilInstr::F64Neg
        | CsilInstr::IntToF64
        | CsilInstr::StrConcat
        | CsilInstr::Select
        | CsilInstr::CmpGt
        | CsilInstr::CmpGe
        | CsilInstr::CmpLt
        | CsilInstr::CmpLe
        | CsilInstr::CmpEq
        | CsilInstr::CmpNe
        | CsilInstr::BoolAnd
        | CsilInstr::BoolOr
        | CsilInstr::BoolNot
        | CsilInstr::PlotF64 { .. }
        | CsilInstr::PlotStyledF64 { .. }
        | CsilInstr::VisualColor { .. }
        | CsilInstr::FillColor { .. }
        | CsilInstr::NewObject { .. }
        | CsilInstr::CopyObject { .. }
        | CsilInstr::GetField { .. }
        | CsilInstr::SetField { .. }
        | CsilInstr::RegisterDrawing { .. }
        | CsilInstr::ReadDrawingsAll { .. }
        | CsilInstr::DeleteDrawing { .. }
        | CsilInstr::SetCell { .. }
        | CsilInstr::ClearCells
        | CsilInstr::MergeCells
        | CsilInstr::Log { .. }
        | CsilInstr::Alert
        | CsilInstr::AlertCondition => true,
        _ => false,
    }
}

/// Nodes whose result may be (or contain) one of their operands, so the
/// result joins the operands' alias class.
fn forwards_handles(instr: &CsilInstr) -> bool {
    if matches!(
        instr,
        CsilInstr::CallCollection {
            func: CollectionFunc::InternalDistanceScan
                | CollectionFunc::InternalDistanceScanEligible,
            ..
        }
    ) {
        // Both results are scalar. Inventing a result alias here would tie
        // unrelated query objects and input settings to the neighbor queues.
        return false;
    }
    matches!(
        instr,
        CsilInstr::Select
            | CsilInstr::CopyObject { .. }
            | CsilInstr::RegisterDrawing { .. }
            | CsilInstr::GetField { .. }
            | CsilInstr::NewObject { .. }
            | CsilInstr::CallCollection { .. }
            | CsilInstr::FootprintRowByPrice
            | CsilInstr::SetField { .. }
            | CsilInstr::SetCell { .. }
            | CsilInstr::ClearCells
            | CsilInstr::MergeCells
            | CsilInstr::DeleteDrawing { .. }
    )
}

/// Nodes that may change an object another instruction later observes. A
/// live use of anything in the node's alias class keeps the node.
fn mutates_heap(instr: &CsilInstr) -> bool {
    match instr {
        CsilInstr::CallCollection { func, .. } => !matches!(
            func,
            CollectionFunc::InternalDistanceScanEligible
                | CollectionFunc::ArrayGet
                | CollectionFunc::ArraySize
                | CollectionFunc::ArrayIncludes
                | CollectionFunc::ArrayIndexOf
                | CollectionFunc::ArrayLastIndexOf
                | CollectionFunc::ArrayFirst
                | CollectionFunc::ArrayLast
                | CollectionFunc::ArraySum
                | CollectionFunc::ArrayAvg
                | CollectionFunc::ArrayMin
                | CollectionFunc::ArrayMax
        ),
        CsilInstr::SetField { .. }
        | CsilInstr::RegisterDrawing { .. }
        | CsilInstr::DeleteDrawing { .. }
        | CsilInstr::SetCell { .. }
        | CsilInstr::ClearCells
        | CsilInstr::MergeCells => true,
        _ => false,
    }
}

fn draw_kind(instr: &CsilInstr) -> Option<DrawKind> {
    match instr {
        CsilInstr::RegisterDrawing { kind }
        | CsilInstr::ReadDrawingsAll { kind }
        | CsilInstr::DeleteDrawing { kind } => Some(*kind),
        _ => None,
    }
}

/// Which nodes produced each operand of each instruction, and which nodes
/// produced every value ever stored to each slot, over the program's basic
/// blocks.
struct Flow {
    operands: Vec<Vec<Producers>>,
    slots: Vec<Producers>,
    /// First pc of each basic block, ascending.
    starts: Vec<usize>,
    /// Operand stack depth on entry to each block; `None` when no path
    /// reaches it.
    entry_depth: Vec<Option<usize>>,
}

impl Flow {
    fn build(program: &CsilProgram) -> Option<Self> {
        let code = &program.main().code;
        let n = code.len();

        // Basic blocks, as `analysis::analyze` forms them.
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

        let mut operands: Vec<Vec<Producers>> = vec![Vec::new(); n];
        let mut slots: Vec<Producers> = vec![Producers::new(); program.series_slots.len()];
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
                            let (pops, pushes) = effect(program, instr)?;
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
                for successor in successor_pcs(code, end) {
                    if successor >= n {
                        return None;
                    }
                    let target = &mut entry[block_at(&starts, successor)];
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
                let entry_depth = entry
                    .iter()
                    .map(|stack| stack.as_ref().map(Vec::len))
                    .collect();
                return Some(Self {
                    operands,
                    slots,
                    starts,
                    entry_depth,
                });
            }
        }
        None
    }

    /// The block holding `pc`.
    fn block_of(&self, pc: usize) -> usize {
        block_at(&self.starts, pc)
    }

    /// One past the last pc of `block`, in a function of `code_len`
    /// instructions.
    fn block_end(&self, block: usize, code_len: usize) -> usize {
        self.starts.get(block + 1).copied().unwrap_or(code_len)
    }
}

/// The block holding `pc`, given ascending block starts beginning at 0.
fn block_at(starts: &[usize], pc: usize) -> usize {
    starts.partition_point(|&start| start <= pc) - 1
}

/// The pcs control may reach after the block ending just before `end`.
fn successor_pcs(code: &[CsilInstr], end: usize) -> Vec<usize> {
    match &code[end - 1] {
        CsilInstr::Return => Vec::new(),
        CsilInstr::Jump { target } => vec![*target as usize],
        CsilInstr::JumpIfFalse { target }
        | CsilInstr::JumpIfExact { target, .. }
        | CsilInstr::JumpIfSlotWritten { target, .. } => vec![*target as usize, end],
        _ => vec![end],
    }
}

fn merge(into: &mut Producers, from: &Producers) -> bool {
    let before = into.len();
    into.extend(from.iter().copied());
    into.len() != before
}

/// What each conditional jump decides.
///
/// A jump decides the blocks control can pass through between it and the
/// block every one of its paths reaches next (its immediate post-dominator,
/// the join). When nothing in those blocks can reach the output, running them
/// or not makes no difference to it, and the jump may go straight to the
/// join, provided the operand stack arrives there as a path through the
/// blocks would leave it (see [`stack_survives`]). Going to the join rather
/// than down either arm also cannot spin: every such jump moves strictly
/// closer to the end of the program.
///
/// A `var` slot's run-once guard (`JumpIfSlotWritten`) is never rewritten,
/// but which way it goes depends on the stores that ran before it, and a
/// skipped branch skips its stores. The blocks a guard decides are recorded
/// too, so that a guard deciding anything live keeps every store to its
/// slot running.
struct Control {
    /// Replaceable conditional jump pc → the pc its paths rejoin at.
    join: BTreeMap<usize, usize>,
    /// Per block: the replaceable conditional jumps that decide whether it
    /// runs. A live instruction or live store in the block keeps them all.
    deciders: Vec<Vec<usize>>,
    /// Per block: the slots whose run-once guards decide whether it runs.
    guards: Vec<Vec<usize>>,
    /// Slots whose guard's paths never rejoin; their stores always run.
    loose_guards: Vec<usize>,
}

impl Control {
    fn build(program: &CsilProgram, flow: &Flow) -> Self {
        let code = &program.main().code;
        let blocks = flow.starts.len();
        let mut control = Self {
            join: BTreeMap::new(),
            deciders: vec![Vec::new(); blocks],
            guards: vec![Vec::new(); blocks],
            loose_guards: Vec::new(),
        };
        // Block successors, with one extra node standing for the end of the
        // program so that every `Return` leads to the same place.
        let exit = blocks;
        let successors: Vec<Vec<usize>> = (0..blocks)
            .map(|block| {
                if flow.entry_depth[block].is_none() {
                    return Vec::new();
                }
                let end = flow.block_end(block, code.len());
                if matches!(code[end - 1], CsilInstr::Return) {
                    return vec![exit];
                }
                successor_pcs(code, end)
                    .into_iter()
                    .map(|pc| flow.block_of(pc))
                    .collect()
            })
            .collect();

        // The reverse graph, walked from the exit. A block the walk never
        // reaches cannot end the program (a loop with no way out); it has
        // no join and its jumps stay as written.
        let mut predecessors: Vec<Vec<usize>> = vec![Vec::new(); blocks + 1];
        for (block, targets) in successors.iter().enumerate() {
            for &target in targets {
                predecessors[target].push(block);
            }
        }
        let mut postorder = Vec::with_capacity(blocks + 1);
        let mut visited = vec![false; blocks + 1];
        visited[exit] = true;
        let mut walk = vec![(exit, 0usize)];
        while let Some((node, next)) = walk.last_mut() {
            if let Some(&from) = predecessors[*node].get(*next) {
                *next += 1;
                if !std::mem::replace(&mut visited[from], true) {
                    walk.push((from, 0));
                }
            } else {
                postorder.push(*node);
                walk.pop();
            }
        }
        let mut rank = vec![usize::MAX; blocks + 1];
        for (index, &node) in postorder.iter().enumerate() {
            rank[node] = index;
        }

        // Immediate post-dominators (Cooper, Harvey and Kennedy's iteration
        // on the reverse graph): the first node every path from a block to
        // the exit passes through.
        let mut next_on_every_path = vec![usize::MAX; blocks + 1];
        next_on_every_path[exit] = exit;
        loop {
            let mut changed = false;
            for &node in postorder.iter().rev() {
                if node == exit {
                    continue;
                }
                let mut found = usize::MAX;
                for &target in &successors[node] {
                    if rank[target] == usize::MAX || next_on_every_path[target] == usize::MAX {
                        continue;
                    }
                    found = if found == usize::MAX {
                        target
                    } else {
                        let (mut a, mut b) = (target, found);
                        while a != b {
                            while rank[a] < rank[b] {
                                a = next_on_every_path[a];
                            }
                            while rank[b] < rank[a] {
                                b = next_on_every_path[b];
                            }
                        }
                        a
                    };
                }
                if found != usize::MAX && next_on_every_path[node] != found {
                    next_on_every_path[node] = found;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        // `walked[node] == block` marks a node already visited on the walk
        // from `block`; each block ends in one jump, so each walks once.
        let mut walked = vec![usize::MAX; blocks];
        for block in 0..blocks {
            if flow.entry_depth[block].is_none() {
                continue;
            }
            let jump = flow.block_end(block, code.len()) - 1;
            let guard = match &code[jump] {
                CsilInstr::JumpIfFalse { .. } | CsilInstr::JumpIfExact { .. } => None,
                CsilInstr::JumpIfSlotWritten { slot, .. } => Some(*slot as usize),
                _ => continue,
            };
            let join = next_on_every_path[block];
            if join == usize::MAX || join == exit {
                control.loose_guards.extend(guard);
                continue;
            }
            if guard.is_none() {
                // The stack right after the jump pops its condition must be
                // as deep as the join expects, and hold the same values.
                let Some(depth) = flow.entry_depth[join] else {
                    continue;
                };
                if successors[block]
                    .iter()
                    .any(|&target| target == exit || flow.entry_depth[target] != Some(depth))
                    || !stack_survives(program, flow, &successors, block, join, depth)
                {
                    continue;
                }
                control.join.insert(jump, flow.starts[join]);
            }
            let mut pending: Vec<usize> = successors[block].clone();
            while let Some(node) = pending.pop() {
                if node == join
                    || node == exit
                    || std::mem::replace(&mut walked[node], block) == block
                {
                    continue;
                }
                match guard {
                    Some(slot) => control.guards[node].push(slot),
                    None => control.deciders[node].push(jump),
                }
                pending.extend(successors[node].iter().copied());
            }
        }
        control
    }
}

/// What one operand stack position holds somewhere between a jump and its
/// join, relative to the stack right after the jump.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Held {
    /// The value this position held right after the jump.
    Same(usize),
    /// A value an instruction on the way pushed.
    Made,
    /// One or the other, depending on the path taken.
    SameOrMade(usize),
}

impl Held {
    /// Both, where two paths meet; `None` when a position would hold the
    /// values of two different positions.
    fn meet(self, other: Self) -> Option<Self> {
        let position = |held: Self| match held {
            Self::Same(index) | Self::SameOrMade(index) => Some(index),
            Self::Made => None,
        };
        if self == other {
            return Some(self);
        }
        match (position(self), position(other)) {
            (Some(left), Some(right)) if left != right => None,
            (Some(index), _) | (_, Some(index)) => Some(Self::SameOrMade(index)),
            (None, None) => Some(Self::Made),
        }
    }
}

/// Whether going from `block`'s jump straight to `join` leaves the operand
/// stack as every path between them would.
///
/// Each position must reach the join holding what it held right after the
/// jump, or a value an instruction on the way pushed. A pushed value is
/// safe to lose: whatever reads it live keeps the instruction that pushed
/// it, and that keeps the jump. The right side of a lazy `and` is the case
/// in practice; its combine pops the left side and pushes the result.
/// A position left holding another position's value has no instruction to
/// keep (`Dup` and `Pop` alone moved it), so the jump stays as written.
fn stack_survives(
    program: &CsilProgram,
    flow: &Flow,
    successors: &[Vec<usize>],
    block: usize,
    join: usize,
    depth: usize,
) -> bool {
    let code = &program.main().code;
    // The stack each block between the jump and the join is entered with.
    let mut entry: BTreeMap<usize, Vec<Held>> = BTreeMap::new();
    let mut pending = Vec::new();
    let mut leaving: Vec<(usize, Vec<Held>)> = successors[block]
        .iter()
        .map(|&target| (target, (0..depth).map(Held::Same).collect()))
        .collect();
    loop {
        for (target, stack) in leaving.drain(..) {
            if target == join {
                let intact = stack.len() == depth
                    && stack.iter().enumerate().all(|(index, held)| match *held {
                        Held::Made => true,
                        Held::Same(from) | Held::SameOrMade(from) => from == index,
                    });
                if !intact {
                    return false;
                }
                continue;
            }
            if target >= flow.starts.len() {
                return false;
            }
            let Some(current) = entry.get_mut(&target) else {
                entry.insert(target, stack);
                pending.push(target);
                continue;
            };
            if current.len() != stack.len() {
                return false;
            }
            let mut changed = false;
            for (have, arriving) in current.iter_mut().zip(&stack) {
                let Some(both) = have.meet(*arriving) else {
                    return false;
                };
                changed |= both != *have;
                *have = both;
            }
            if changed {
                pending.push(target);
            }
        }
        let Some(node) = pending.pop() else {
            return true;
        };
        let mut stack = entry[&node].clone();
        for instr in &code[flow.starts[node]..flow.block_end(node, code.len())] {
            if matches!(instr, CsilInstr::Dup) {
                let Some(&top) = stack.last() else {
                    return false;
                };
                stack.push(top);
                continue;
            }
            let Some((pops, pushes)) = effect(program, instr) else {
                return false;
            };
            if stack.len() < pops {
                return false;
            }
            stack.truncate(stack.len() - pops);
            stack.resize(stack.len() + pushes, Held::Made);
        }
        leaving.extend(
            successors[node]
                .iter()
                .map(|&target| (target, stack.clone())),
        );
    }
}

struct Classes {
    parent: Vec<usize>,
}

impl Classes {
    fn find(&mut self, mut node: usize) -> usize {
        while self.parent[node] != node {
            self.parent[node] = self.parent[self.parent[node]];
            node = self.parent[node];
        }
        node
    }

    fn union(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.parent[a] = b;
        }
    }
}

/// Which nodes can never produce a heap handle (an array, map, matrix,
/// object or drawing): the greatest solution of the rules below, so a value
/// is handle-free only when every value it can be is.
///
/// * constants, builtin and input reads, requests, arithmetic, comparisons
///   and string concatenation are scalars (a string is immutable, so sharing
///   one is not aliasing);
/// * a pure or TA call, and `Select`, is handle-free when all its operands
///   are, which covers a function that returns its argument;
/// * a slot read is handle-free when every value stored to the slot is;
/// * a field read is handle-free when its receiver is provably one of a known
///   set of object types and every value ever written to that field of those
///   types is: `NewObject` writes operand `i` to field `i`, `SetField` writes
///   its second operand (to every type, when its receiver's type is not
///   provable), and `CopyObject` copies cells;
/// * everything else, including every collection call, constructor, drawing
///   opcode and any opcode this pass does not know, may produce a handle.
fn handle_free_nodes(program: &CsilProgram, flow: &Flow) -> Option<Vec<bool>> {
    let code = &program.main().code;
    let mut free: Vec<bool> = code
        .iter()
        .map(|instr| {
            matches!(
                instr,
                CsilInstr::CallCollection {func: CollectionFunc::InternalDistanceScan | CollectionFunc::InternalDistanceScanEligible, ..}
                    | CsilInstr::PushNa
                    | CsilInstr::PushBool(_)
                    | CsilInstr::PushI64(_)
                    | CsilInstr::PushF64(_)
                    | CsilInstr::PushStr(_)
                    | CsilInstr::PushColor { .. }
                    | CsilInstr::ReadBuiltin { .. }
                    | CsilInstr::ReadBuiltinDynamic { .. }
                    | CsilInstr::ReadInput { .. }
                    | CsilInstr::ReadSecurity { .. }
                    | CsilInstr::ReadSecurityExpr { .. }
                    | CsilInstr::WindowMean { .. }
                    | CsilInstr::IntAdd
                    | CsilInstr::IntSub
                    | CsilInstr::IntMul
                    | CsilInstr::IntDiv
                    | CsilInstr::PineIntDiv
                    | CsilInstr::IntRem
                    | CsilInstr::IntNeg
                    | CsilInstr::F64Add
                    | CsilInstr::F64Sub
                    | CsilInstr::F64Mul
                    | CsilInstr::F64Div
                    | CsilInstr::F64Rem
                    | CsilInstr::F64Neg
                    | CsilInstr::IntToF64
                    | CsilInstr::StrConcat
                    | CsilInstr::CmpGt
                    | CsilInstr::CmpGe
                    | CsilInstr::CmpLt
                    | CsilInstr::CmpLe
                    | CsilInstr::CmpEq
                    | CsilInstr::CmpNe
                    | CsilInstr::BoolAnd
                    | CsilInstr::BoolOr
                    | CsilInstr::BoolNot
                    // Conditional on their operands, slots or fields below.
                    | CsilInstr::CallPure { .. }
                    | CsilInstr::CallTa { .. }
                    | CsilInstr::Select
                    | CsilInstr::ReadUserSlot { .. }
                    | CsilInstr::ReadUserSlotDynamic { .. }
                    | CsilInstr::GetField { .. }
            )
        })
        .collect();
    let all_free = |free: &[bool], producers: &Producers| producers.iter().all(|&pc| free[pc]);
    // The object types each field access may act on, where provable.
    let receivers: Vec<Option<BTreeSet<u32>>> = code
        .iter()
        .enumerate()
        .map(|(pc, instr)| match instr {
            CsilInstr::GetField { .. } | CsilInstr::SetField { .. } => flow.operands[pc]
                .first()
                .and_then(|receiver| object_types(code, flow, receiver)),
            _ => None,
        })
        .collect();
    loop {
        // (type, field) -> whether every value written to it is handle-free;
        // `any_type` holds the writes whose receiver type is not provable.
        let mut field_free: BTreeMap<(u32, u32), bool> = BTreeMap::new();
        let mut any_type: BTreeMap<u32, bool> = BTreeMap::new();
        for (pc, instr) in code.iter().enumerate() {
            match instr {
                CsilInstr::NewObject { type_id, .. } => {
                    for (field, producers) in flow.operands[pc].iter().enumerate() {
                        *field_free.entry((*type_id, field as u32)).or_insert(true) &=
                            all_free(&free, producers);
                    }
                }
                CsilInstr::SetField { field_idx } => {
                    let ok = all_free(&free, flow.operands[pc].get(1)?);
                    match &receivers[pc] {
                        Some(types) => {
                            for type_id in types {
                                *field_free.entry((*type_id, *field_idx)).or_insert(true) &= ok;
                            }
                        }
                        None => *any_type.entry(*field_idx).or_insert(true) &= ok,
                    }
                }
                _ => {}
            }
        }
        let mut changed = false;
        for (pc, instr) in code.iter().enumerate() {
            if !free[pc] {
                continue;
            }
            let still = match instr {
                CsilInstr::CallPure { .. } | CsilInstr::CallTa { .. } | CsilInstr::Select => flow
                    .operands[pc]
                    .iter()
                    .all(|producers| all_free(&free, producers)),
                CsilInstr::ReadUserSlot { .. } | CsilInstr::ReadUserSlotDynamic { .. } => {
                    all_free(&free, flow.slots.get(slot_read(instr)?)?)
                }
                CsilInstr::GetField { field_idx } => {
                    any_type.get(field_idx).copied().unwrap_or(true)
                        && receivers[pc].as_ref().is_some_and(|types| {
                            types.iter().all(|type_id| {
                                // A field no instruction writes holds no
                                // value this pass can vouch for.
                                field_free
                                    .get(&(*type_id, *field_idx))
                                    .copied()
                                    .unwrap_or(false)
                            })
                        })
                }
                _ => true,
            };
            if !still {
                free[pc] = false;
                changed = true;
            }
        }
        if !changed {
            return Some(free);
        }
    }
}

/// The object types the values `producers` yield can have, or `None` when
/// one of them is not provably an object built by a constructor in this
/// program (`na` contributes no type).
fn object_types(code: &[CsilInstr], flow: &Flow, producers: &Producers) -> Option<BTreeSet<u32>> {
    let mut types = BTreeSet::new();
    let mut seen = BTreeSet::new();
    let mut work: Vec<usize> = producers.iter().copied().collect();
    while let Some(pc) = work.pop() {
        if !seen.insert(pc) {
            continue;
        }
        match &code[pc] {
            CsilInstr::PushNa => {}
            CsilInstr::NewObject { type_id, .. } | CsilInstr::CopyObject { type_id } => {
                types.insert(*type_id);
            }
            instr @ (CsilInstr::ReadUserSlot { .. } | CsilInstr::ReadUserSlotDynamic { .. }) => {
                work.extend(flow.slots.get(slot_read(instr)?)?.iter().copied());
            }
            _ => return None,
        }
    }
    Some(types)
}

/// Per-instruction liveness with respect to `anchor`. A conditional jump
/// listed in `control.join` is live when what it decides can reach the
/// output.
fn liveness(
    program: &CsilProgram,
    flow: &Flow,
    control: &Control,
    anchor: Anchor,
) -> Option<Vec<bool>> {
    let code = &program.main().code;
    let n = code.len();
    let mut stores: Vec<Vec<usize>> = vec![Vec::new(); program.series_slots.len()];
    for (pc, instr) in code.iter().enumerate() {
        if let CsilInstr::StoreUserSlotCurrent { slot } = instr {
            stores.get_mut(*slot as usize)?.push(pc);
        }
    }
    let mut slot_live = vec![false; program.series_slots.len()];
    // Slots whose stores must all run, not yet acted on.
    let mut slots_due: Vec<usize> = control.loose_guards.clone();
    // Blocks holding something the output depends on.
    let mut kept = vec![false; flow.starts.len()];

    // Alias classes over producing nodes.
    let mut classes = Classes {
        parent: (0..n).collect(),
    };
    let mut drawing_class: Vec<(DrawKind, usize)> = Vec::new();
    // A value that can never be a heap handle aliases nothing, so it joins
    // no class: a scalar read from a settings object and passed to a label
    // must not tie the two objects together.
    let handle_free = handle_free_nodes(program, flow)?;
    for (pc, instr) in code.iter().enumerate() {
        if forwards_handles(instr) && !handle_free[pc] {
            for producers in &flow.operands[pc] {
                for &producer in producers {
                    if !handle_free[producer] {
                        classes.union(pc, producer);
                    }
                }
            }
        }
        if matches!(
            instr,
            CsilInstr::ReadUserSlot { .. } | CsilInstr::ReadUserSlotDynamic { .. }
        ) {
            for &producer in flow.slots.get(slot_read(instr)?)? {
                if !handle_free[producer] {
                    classes.union(pc, producer);
                }
            }
        }
        if let Some(kind) = draw_kind(instr) {
            match drawing_class.iter().find(|(known, _)| *known == kind) {
                Some(&(_, representative)) => classes.union(pc, representative),
                None => drawing_class.push((kind, pc)),
            }
        }
    }
    let mut mutators: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (pc, instr) in code.iter().enumerate() {
        if matches!(
            instr,
            CsilInstr::CallCollection {
                func: CollectionFunc::InternalDistanceScan,
                ..
            }
        ) {
            // This scalar-returning operation mutates its two queue operands,
            // not the query, feature arrays, label array, or its result class.
            for operand in [6, 7] {
                for &producer in flow.operands[pc].get(operand)? {
                    let class = classes.find(producer);
                    mutators[class].push(pc);
                }
            }
        } else if mutates_heap(instr) {
            let class = classes.find(pc);
            mutators[class].push(pc);
        }
    }

    let mut live = vec![false; n];
    let mut observed = vec![false; n];
    let mut work: Vec<usize> = Vec::new();
    let mut designated = false;

    for (pc, instr) in code.iter().enumerate() {
        match instr {
            // A jump with no usable join is kept exactly as written.
            CsilInstr::JumpIfFalse { .. } | CsilInstr::JumpIfExact { .. } => {
                if !control.join.contains_key(&pc) {
                    work.push(pc);
                }
            }
            CsilInstr::PlotF64 { plot_slot } | CsilInstr::PlotStyledF64 { plot_slot, .. } => {
                if anchor.keeps_plot(*plot_slot) {
                    designated = true;
                    work.push(pc);
                }
            }
            CsilInstr::Order { .. } => {
                designated = true;
                work.push(pc);
            }
            _ if is_node(instr) && !erasable(instr) => work.push(pc),
            _ => {}
        }
    }
    // No instruction writes the designated plot (or, for a strategy, places
    // an order or writes a plot): nothing to anchor the slice to, so leave
    // the program alone.
    if !designated {
        return None;
    }

    loop {
        if let Some(slot) = slots_due.pop() {
            // A store is never rewritten, but a skipped branch would skip
            // it: whatever decides that a store to this slot runs is live.
            if !std::mem::replace(slot_live.get_mut(slot)?, true) {
                for &store in &stores[slot] {
                    keep_block(
                        control,
                        flow.block_of(store),
                        &mut kept,
                        &mut work,
                        &mut slots_due,
                    );
                }
            }
            continue;
        }
        let Some(pc) = work.pop() else {
            break;
        };
        if std::mem::replace(&mut live[pc], true) {
            continue;
        }
        let instr = &code[pc];
        // Whatever decides that this instruction runs is live with it.
        keep_block(
            control,
            flow.block_of(pc),
            &mut kept,
            &mut work,
            &mut slots_due,
        );
        // Only the value of a kept plot is serialized; its colors are chart
        // output like any other. Only an order's alert text is unobserved.
        let unobserved = |index: usize| match instr {
            CsilInstr::PlotStyledF64 { .. } => index > 0,
            CsilInstr::Order { kind } => unobserved_order_operand(*kind, index),
            _ => false,
        };
        let mut touched = vec![pc];
        for (index, producers) in flow.operands[pc].iter().enumerate() {
            if unobserved(index) {
                continue;
            }
            touched.extend(producers.iter().copied());
            work.extend(producers.iter().copied());
        }
        if let Some(slot) = slot_read(instr) {
            let stored = flow.slots.get(slot)?;
            touched.extend(stored.iter().copied());
            work.extend(stored.iter().copied());
            slots_due.push(slot);
        }
        for node in touched {
            let class = classes.find(node);
            if !std::mem::replace(&mut observed[class], true) {
                work.extend(mutators[class].iter().copied());
            }
        }
    }
    Some(live)
}

/// Records that the output depends on something in `block`: the jumps that
/// decide whether it runs are live, and the slots whose run-once guards
/// decide it keep their stores.
fn keep_block(
    control: &Control,
    block: usize,
    kept: &mut [bool],
    work: &mut Vec<usize>,
    slots_due: &mut Vec<usize>,
) {
    if std::mem::replace(&mut kept[block], true) {
        return;
    }
    work.extend(control.deciders[block].iter().copied());
    slots_due.extend(control.guards[block].iter().copied());
}

#[cfg(test)]
mod tests {
    use super::*;
    use pinewasm_compiler::{compile, CompileOptions};

    fn program(body: &str) -> CsilProgram {
        let source = format!("//@version=6\nindicator(\"slice\")\n{body}");
        let result = compile(&source, CompileOptions::all());
        assert!(
            !result
                .diagnostics
                .iter()
                .any(pinewasm_span::Diagnostic::is_error),
            "{:?}",
            result.diagnostics
        );
        result.program.expect("program")
    }

    fn conditional_jumps(program: &CsilProgram) -> usize {
        program
            .main()
            .code
            .iter()
            .filter(|instr| {
                matches!(
                    instr,
                    CsilInstr::JumpIfFalse { .. } | CsilInstr::JumpIfExact { .. }
                )
            })
            .count()
    }

    // Standalone signal output omits paint. With the plot color at its default,
    // a projection can now erase that color even when every numeric/control
    // instruction must stay. Assert the complete surviving program instead
    // of mistaking that legitimate paint erasure for a skipped branch.
    fn assert_only_default_paint_is_erased(program: &CsilProgram) {
        let sliced =
            project_to_signal(program, Anchor::Plot(0)).expect("default paint is erasable");
        let mut expected = program.clone();
        let mut colors = 0;
        for instruction in &mut expected.functions[0].code {
            if matches!(
                instruction,
                CsilInstr::PushColor {
                    r: 33,
                    g: 150,
                    b: 243,
                    t
                } if t.get() == 0.0
            ) {
                *instruction = CsilInstr::PushNa;
                colors += 1;
            }
        }
        assert_eq!(colors, 1, "fixture has one default-painted signal plot");
        expected.functions[0].budget_reference = None;
        assert_eq!(sliced.program, expected);
        assert!(sliced.budget_free.is_empty());
    }

    /// `plot(close - open)` with `between` run on up bars only, while both
    /// operands of the subtraction wait on the stack. The lowering never
    /// holds two values across a branch; this is the shape the stack check
    /// exists for.
    fn subtraction_with(between: &[CsilInstr]) -> CsilProgram {
        let mut program = program("plot(close - open, \"Signal\")\n");
        let main = program.functions.first_mut().expect("main");
        assert!(matches!(main.code[2], CsilInstr::F64Sub), "{:?}", main.code);
        let mut inserted = vec![
            main.code[0].clone(),
            main.code[1].clone(),
            CsilInstr::CmpGt,
            CsilInstr::JumpIfFalse {
                target: (2 + 4 + between.len()) as u32,
            },
        ];
        inserted.extend_from_slice(between);
        let span = main.source_map[2];
        main.source_map
            .splice(2..2, std::iter::repeat_n(span, inserted.len()));
        main.code.splice(2..2, inserted);
        // The condition's two operands sit above the two that wait.
        main.max_stack += 2;
        pinewasm_csil::validate(&program).expect("valid CSIL");
        program
    }

    #[test]
    fn a_jump_whose_branch_only_rearranges_the_stack_stays_as_written() {
        // On up bars the branch drops `open` and copies `close` over it, so
        // the plot is 0 there and `close - open` elsewhere. No instruction
        // in the branch makes a value, so nothing in it could ever be live:
        // skipping it would plot `close - open` on every bar.
        let moved = subtraction_with(&[CsilInstr::Pop, CsilInstr::Dup]);
        let flow = Flow::build(&moved).expect("flow");
        assert!(Control::build(&moved, &flow).join.is_empty());
        assert_only_default_paint_is_erased(&moved);

        // The same branch pushing a fresh `close` instead: the push is an
        // instruction liveness can see, so the jump may be considered, and
        // the live subtraction then keeps the push and the jump with it.
        let fresh = moved.main().code[0].clone();
        let made = subtraction_with(&[CsilInstr::Pop, fresh]);
        let flow = Flow::build(&made).expect("flow");
        assert_eq!(Control::build(&made, &flow).join.len(), 1);
        assert_only_default_paint_is_erased(&made);
    }

    #[test]
    fn a_var_whose_initializer_matters_keeps_every_store_to_it() {
        let signal = "plot(close > close[1] ? 1 : -1, \"Signal\")\n";
        // Nothing live reads `n`, so the branch that assigns it goes.
        let plain = program(&format!(
            "var int n = 0\nif close > open\n    n := 1\n{signal}"
        ));
        assert_eq!(conditional_jumps(&plain), 1);
        let sliced = project_to_signal(&plain, Anchor::Plot(0)).expect("the branch is erased");
        assert_eq!(conditional_jumps(&sliced.program), 0);

        // `math.random` is always live, and it runs only while `n` is
        // unwritten. Which stores to `n` have run now decides something
        // live, so the branch that stores to it stays.
        let random = program(&format!(
            "var int n = int(math.random() * 0)\nif close > open\n    n := 1\n{signal}"
        ));
        assert_eq!(conditional_jumps(&random), 1);
        let sliced = project_to_signal(&random, Anchor::Plot(0)).expect("the dead push is erased");
        assert_eq!(conditional_jumps(&sliced.program), 1);
    }

    #[test]
    fn a_skipped_jump_costs_what_the_conditional_one_did() {
        // The conditional jump cost one unit. The `Pop` that drops its
        // condition is charged that unit and the jump after it is free, so
        // skipping a branch never costs more than testing it did.
        let plain = program(
            "var int n = 0\nif close > open\n    n := 1\nplot(close > close[1] ? 1 : -1, \"Signal\")\n",
        );
        let sliced = project_to_signal(&plain, Anchor::Plot(0)).expect("the branch is erased");
        let code = &sliced.program.main().code;
        let free_jumps: Vec<usize> = sliced
            .budget_free
            .iter()
            .copied()
            .filter(|&pc| matches!(code[pc], CsilInstr::Jump { .. }))
            .collect();
        let [jump] = free_jumps[..] else {
            panic!("one free jump, the skipped branch's: {free_jumps:?}");
        };
        assert!(matches!(code[jump - 1], CsilInstr::Pop), "{code:?}");
        assert!(!sliced.budget_free.contains(&(jump - 1)));
    }

    /// `program` with `code` in place of its own, the operand stack
    /// `max_stack` deep. The front end does not lower to the shapes the
    /// tests below need, so they are written out.
    fn with_code(mut program: CsilProgram, code: Vec<CsilInstr>, max_stack: u32) -> CsilProgram {
        let main = program.functions.first_mut().expect("main");
        let span = main.source_map[0];
        main.source_map = vec![span; code.len()];
        main.code = code;
        main.max_stack = max_stack;
        main.budget_reference = None;
        pinewasm_csil::validate(&program).expect("valid CSIL");
        program
    }

    #[test]
    fn a_store_the_plot_reads_keeps_the_jump_that_decides_it() {
        // `level` is `open`, and on up bars a branch stores `close` over it,
        // so the plot is 0 there and `close - open` elsewhere. The value the
        // branch stores is made before the jump and waits on the stack, so
        // no instruction inside the branch is live: only the store says the
        // plot depends on which way the jump goes.
        let base = program("float level = open\nplot(close - level, \"Signal\")\n");
        let code = base.main().code.clone();
        assert!(
            matches!(code[1], CsilInstr::StoreUserSlotCurrent { slot: 0 })
                && matches!(code[3], CsilInstr::ReadUserSlot { slot: 0, .. }),
            "{code:?}"
        );
        let (open, close) = (code[0].clone(), code[2].clone());
        let mut written = code[..2].to_vec();
        written.extend([
            close.clone(),
            close,
            open,
            CsilInstr::CmpGt,
            CsilInstr::JumpIfFalse { target: 9 },
            CsilInstr::StoreUserSlotCurrent { slot: 0 },
            // The join expects one value, whichever path reached it.
            CsilInstr::PushNa,
            CsilInstr::Pop,
        ]);
        written.extend_from_slice(&code[2..]);
        let stored = with_code(base, written, 3);
        let flow = Flow::build(&stored).expect("flow");
        assert_eq!(Control::build(&stored, &flow).join.len(), 1);
        assert_only_default_paint_is_erased(&stored);
    }

    #[test]
    fn a_jump_whose_paths_never_rejoin_stays_as_written() {
        // On up bars the program returns before the plot. The two paths
        // meet nowhere, so there is no point to jump to.
        let base = program("plot(close - open, \"Signal\")\n");
        let code = base.main().code.clone();
        let mut written = vec![
            code[0].clone(),
            code[1].clone(),
            CsilInstr::CmpGt,
            CsilInstr::JumpIfFalse { target: 5 },
            CsilInstr::Return,
        ];
        written.extend_from_slice(&code);
        let split = with_code(base, written, 2);
        let flow = Flow::build(&split).expect("flow");
        assert!(Control::build(&split, &flow).join.is_empty());
        assert_only_default_paint_is_erased(&split);
    }

    #[test]
    fn a_var_guard_whose_paths_never_rejoin_keeps_every_store_to_its_slot() {
        // The plot runs only once `mark` has been written, and the one
        // store to it sits in a branch nothing live reads. The guard's
        // paths never rejoin, so it has no blocks to watch; its slot keeps
        // every store regardless.
        let base = program("var float mark = na\nplot(close - open, \"Signal\")\n");
        let code = base.main().code.clone();
        let sub = code
            .iter()
            .position(|instr| matches!(instr, CsilInstr::F64Sub))
            .expect("the plot subtracts");
        assert!(
            matches!(code[0], CsilInstr::JumpIfSlotWritten { slot: 0, .. }) && sub >= 2,
            "{code:?}"
        );
        let (close, open) = (code[sub - 2].clone(), code[sub - 1].clone());
        let mut written = vec![
            close,
            open,
            CsilInstr::CmpGt,
            CsilInstr::JumpIfFalse { target: 6 },
            CsilInstr::PushNa,
            CsilInstr::StoreUserSlotCurrent { slot: 0 },
            CsilInstr::JumpIfSlotWritten { slot: 0, target: 8 },
            CsilInstr::Return,
        ];
        written.extend_from_slice(&code[sub - 2..]);
        let guarded = with_code(base, written, 2);
        let flow = Flow::build(&guarded).expect("flow");
        let control = Control::build(&guarded, &flow);
        assert_eq!(control.join.len(), 1);
        assert_eq!(control.loose_guards, [0]);
        assert_only_default_paint_is_erased(&guarded);
    }
}

#[cfg(test)]
mod ordered_scan_tests {
    use super::*;
    use pinewasm_compiler::{compile, CompileOptions};
    use pinewasm_vm::{run, MarketData, RunOptions};

    #[test]
    fn scan_queue_effects_survive_but_do_not_alias_readonly_query_outputs() {
        let source = format!(
            "{}\nplot(featureSeries.f1, \"Query\")\n",
            include_str!("../../../tests/regression/indexed_distance_scan.pine")
        );
        let program = compile(&source, CompileOptions::default()).program.unwrap();
        let contains_scan = |p: &CsilProgram| {
            p.main().code.iter().any(|i| {
                matches!(
                    i,
                    CsilInstr::CallCollection {
                        func: CollectionFunc::InternalDistanceScan,
                        ..
                    }
                )
            })
        };
        assert!(contains_scan(&program));
        let close: Vec<f64> = (0..129)
            .map(|i| 100.0 + (f64::from(i) * 0.19).sin())
            .collect();
        let data = MarketData {
            open: close.clone(),
            high: close.clone(),
            low: close.clone(),
            volume: vec![0.0; 129],
            close,
            ..MarketData::default()
        };
        let reference = run(&program, &data, &RunOptions::default());
        assert!(reference.errors.is_empty());
        for plot in [0, 4] {
            let sliced = project_to_signal(&program, Anchor::Plot(plot)).expect("slices");
            assert_eq!(contains_scan(&sliced.program), plot == 0);
            let actual = run(&sliced.program, &data, &RunOptions::default());
            assert!(actual.errors.is_empty(), "{:?}", actual.errors);
            assert_eq!(
                actual.plots[plot as usize].values,
                reference.plots[plot as usize].values
            );
        }
    }
}
