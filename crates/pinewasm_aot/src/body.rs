//! Structured emission of the per-bar body: reconstructs wasm block/loop/if
//! nesting from the reducible CSIL CFG (the dominator-tree algorithm from
//! "Beyond Relooper", specialised to our generator's shapes) and
//! lowers every supported opcode onto typed locals.
//!
//! Stack model: the CSIL operand stack is virtualised. Within a basic block
//! the top cells live on the real wasm stack; across block edges every cell
//! is flushed to a canonical local keyed by (depth, type class), so merge
//! points agree by construction (type inference proved per-depth types
//! consistent). `na` is a value in every lane: NaN (f64), `i64::MIN` (i64),
//! 1 (bool3 0/1/2), `-1` (packed color).

#[cfg(test)]
mod aeneas_refactor_tests;
mod chart_fuel;
mod chart_time;
mod plan;
mod pure;
mod sma;
pub(crate) mod split;

use crate::analysis::{
    builtin_vty, order_slot_vtys, strategy_snap_field, Analysis, HostedAlloc, IntMinGuard,
    NativeCollOp, Term, VTy,
};
use crate::emit::{Globals, HostFns, Locals, SlotLayout, StringFns};
use crate::heap::{self, agg_op, ARR_BUF, ARR_LEN};
use crate::projection::ChartSite;
use crate::{header, status, AotError, Layout};
use chart_fuel::ChartFacts;
use pinewasm_csil::{CsilInstr, CsilProgram};
use pinewasm_hir::{BuiltinSeries, InputKind, InputValue, OrderKind, TaOperand, TaResult};
use plan::{PlanError, Step};
use std::collections::BTreeMap;
use wasm_encoder::{BlockType, Function, Instruction as I, MemArg, ValType};

fn mem_f64() -> MemArg {
    MemArg {
        offset: 0,
        align: 3,
        memory_index: 0,
    }
}
fn mem_i64() -> MemArg {
    MemArg {
        offset: 0,
        align: 3,
        memory_index: 0,
    }
}
fn mem_i32() -> MemArg {
    MemArg {
        offset: 0,
        align: 2,
        memory_index: 0,
    }
}
fn mem_u32_at(off: u32) -> MemArg {
    MemArg {
        offset: u64::from(off),
        align: 2,
        memory_index: 0,
    }
}
fn mem_u8() -> MemArg {
    MemArg {
        offset: 0,
        align: 0,
        memory_index: 0,
    }
}

/// The frequency name the interpreter gives every `alertcondition` event
/// (`Vm::exec_alert_condition`), whose bytes it reserves with the message's.
const ALERT_CONDITION_FREQ: &str = "alert.freq_once_per_bar_close";

/// The bytes of one string reservation ([`BodyEmitter::emit_string_reserve`]).
#[derive(Clone, Copy)]
enum StringBytes {
    /// Known when the module is compiled.
    Known(usize),
    /// Computed by the instruction, in the `i64` local `ti2`.
    InTi2,
}

/// na constants per type (see module docs).
const NA_I64: i64 = i64::MIN;
const NA_BOOL3: i32 = 1;
const NA_COLOR: i32 = -1;
const NA_STR: i32 = -1;
const NA_REF: i32 = -1;

fn val_type(ty: VTy) -> ValType {
    match ty {
        VTy::F64 => ValType::F64,
        VTy::I64 => ValType::I64,
        VTy::Bool3 | VTy::Color | VTy::Str | VTy::Ref => ValType::I32,
    }
}

fn ty_class(ty: VTy) -> u32 {
    match ty {
        VTy::F64 => 0,
        VTy::I64 => 1,
        VTy::Bool3 | VTy::Color | VTy::Str | VTy::Ref => 2,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Loc {
    Phys,
    Local,
}

/// Dominator-tree children per block, sorted by rpo (the plan's input).
fn dom_children(a: &Analysis) -> Vec<Vec<usize>> {
    let n = a.blocks.len();
    let mut dom_children: Vec<Vec<usize>> = vec![Vec::new(); n];
    for b in 1..n {
        if a.rpo[b] != usize::MAX && a.idom[b] != usize::MAX && a.idom[b] != b {
            dom_children[a.idom[b]].push(b);
        }
    }
    for children in &mut dom_children {
        children.sort_by_key(|&c| a.rpo[c]);
    }
    dom_children
}

/// Collect the facts of the instructions from `start` up to the block end,
/// stopping after the first one that cannot extend a chart window, and return
/// the end of the window and the cost its prelude precharges, if
/// [`chart_fuel::chart_group_len`] accepts one.
fn chart_window_end(
    start: usize,
    block_end: usize,
    facts_at: impl Fn(usize) -> ChartFacts,
) -> Option<(usize, u64)> {
    let mut window = Vec::new();
    for pc in start..block_end {
        let facts = facts_at(pc);
        let extends = chart_fuel::chart_group_eligible(&facts) && !facts.terminator;
        window.push(facts);
        if !extends {
            break;
        }
    }
    chart_fuel::chart_group_len(&window).map(|(k, cost)| (start + k, cost))
}

/// A block terminator: the instructions that transfer control.
fn is_terminator(instr: &CsilInstr) -> bool {
    matches!(
        instr,
        CsilInstr::Jump { .. }
            | CsilInstr::JumpIfFalse { .. }
            | CsilInstr::JumpIfExact { .. }
            | CsilInstr::JumpIfSlotWritten { .. }
            | CsilInstr::Return
    )
}

pub(crate) struct BodyCtx<'a> {
    pub program: &'a CsilProgram,
    pub analysis: &'a Analysis,
    pub layout: &'a Layout,
    pub slots: &'a SlotLayout,
    pub globals: &'a Globals,
    pub locals: &'a Locals,
    pub hosts: &'a HostFns,
    /// pc → `host.vm` site id (index into `layout.host_sites`).
    pub site_of: &'a BTreeMap<usize, u32>,
    /// Present only for the importless standalone WebAssembly projection.
    pub projection: Option<&'a crate::projection::StandaloneWasmProjection>,
    /// Present only for a chart-tier module whose kept int quotients were
    /// rewritten into float code (`fraction.rs`). The standalone tier
    /// carries the same two sets inside its projection. `program` is then
    /// the rewritten one, and every pc that leaves the module names the
    /// instruction of the program the caller holds.
    pub carry: Option<&'a crate::fraction::Carried>,
}

pub(crate) struct BodyEmitter<'a, 'f> {
    ctx: &'a BodyCtx<'a>,
    f: &'f mut Function,
    /// Virtual operand stack; physical cells are always the topmost run.
    vstack: Vec<(VTy, Loc)>,
    /// Whether the instruction being lowered emitted its minimum guard.
    guard_emitted: bool,
    /// Whether code emitted since the last reset reads or writes a budget
    /// counter. A grouped instruction must not (`emit_fuel_group`).
    counter_touched: bool,
}

impl<'a, 'f> BodyEmitter<'a, 'f> {
    pub(crate) fn new(ctx: &'a BodyCtx<'a>, f: &'f mut Function) -> Self {
        BodyEmitter {
            ctx,
            f,
            vstack: Vec::new(),
            guard_emitted: false,
            counter_touched: false,
        }
    }

    /// Emit the whole body (block 0 and everything it dominates). The caller
    /// wrapped us in the `block $bar_done` that `Frame::BarDone` names.
    ///
    /// The label structure comes from the pure [`plan::plan_structure`]
    /// kernel; this replays its steps in order. A planning error is reported
    /// after the steps planned before it, i.e. at the point where the walk
    /// found it.
    pub(crate) fn emit(mut self) -> Result<(), AotError> {
        let a = self.ctx.analysis;
        let dom_children = dom_children(a);
        let cfg = plan::Cfg {
            term: &a.term,
            rpo: &a.rpo,
            loop_header: &a.loop_header,
            merge: &a.merge,
            dom_children: &dom_children,
        };
        let (steps, planned) = plan::plan_structure(&cfg, a.blocks.len());
        for step in steps {
            match step {
                Step::Loop(_) => {
                    self.f.instruction(&I::Loop(BlockType::Empty));
                }
                Step::Block(_) => {
                    self.f.instruction(&I::Block(BlockType::Empty));
                }
                Step::If => {
                    self.f.instruction(&I::If(BlockType::Empty));
                }
                Step::Else => {
                    self.f.instruction(&I::Else);
                    // The virtual stack was fully flushed before the test;
                    // restore the same state for the second arm.
                    self.vstack.iter_mut().for_each(|c| c.1 = Loc::Local);
                }
                Step::End => {
                    self.f.instruction(&I::End);
                }
                Step::Br(depth, _) => {
                    self.f.instruction(&I::Br(depth as u32));
                }
                Step::Code(x) => self.emit_block_code(x)?,
                Step::Term(x) => self.emit_terminator(x)?,
            }
        }
        planned.map_err(|error| match error {
            PlanError::ScheduledTwice(x) => {
                AotError::InvalidProgram(format!("block {x} scheduled twice (relooper invariant)"))
            }
            PlanError::NoFrame(want) => AotError::InvalidProgram(format!("no frame for {want:?}")),
        })
    }

    // ---- virtual stack ----------------------------------------------------

    fn stack_local(&self, depth: usize, ty: VTy) -> u32 {
        self.stack_index(depth, ty_class(ty))
    }

    fn integer_valid_local(&self, depth: usize) -> u32 {
        self.stack_index(depth, 3)
    }

    /// The local of stack depth `depth`, class `class`. A depth past the
    /// function's declared stack locals would name a current-cell local
    /// after them, so it names no local at all instead: the module then
    /// fails validation rather than read or overwrite a cell. A split body's
    /// piece declares only the depths its own code uses.
    fn stack_index(&self, depth: usize, class: u32) -> u32 {
        let locals = self.ctx.locals;
        debug_assert!(
            (depth as u32) < locals.stack_depths,
            "stack depth {depth} past the {} declared",
            locals.stack_depths
        );
        if depth as u32 >= locals.stack_depths {
            return u32::MAX;
        }
        locals.stack_base + (depth as u32) * 4 + class
    }

    fn phys_count(&self) -> usize {
        self.vstack
            .iter()
            .rev()
            .take_while(|(_, l)| *l == Loc::Phys)
            .count()
    }

    /// Spill every physical cell to its canonical local (top first).
    fn flush_all(&mut self) {
        while let Some(i) = self.vstack.iter().rposition(|(_, l)| *l == Loc::Phys) {
            // Physical cells are topmost, so `i` is the current stack top.
            let ty = self.vstack[i].0;
            self.f.instruction(&I::LocalSet(self.stack_local(i, ty)));
            self.vstack[i].1 = Loc::Local;
        }
    }

    /// Ensure the top `n` cells are physical (in order).
    fn want(&mut self, n: usize) {
        debug_assert!(self.vstack.len() >= n, "stack underflow in emitter");
        if self.phys_count() >= n {
            return;
        }
        self.flush_all();
        let len = self.vstack.len();
        for i in len - n..len {
            let ty = self.vstack[i].0;
            self.f.instruction(&I::LocalGet(self.stack_local(i, ty)));
            self.vstack[i].1 = Loc::Phys;
        }
    }

    /// Flush everything below the (physical) top cell; keeps the top on the
    /// real stack. Used before conditional terminators.
    fn flush_below_top(&mut self) {
        self.want(1);
        if self.phys_count() == 1 {
            return;
        }
        let top = self.vstack.len() - 1;
        let ty = self.vstack[top].0;
        self.f.instruction(&I::LocalSet(self.stack_local(top, ty)));
        self.vstack[top].1 = Loc::Local;
        self.flush_all();
        self.f.instruction(&I::LocalGet(self.stack_local(top, ty)));
        self.vstack[top].1 = Loc::Phys;
    }

    /// Record a value the emitted code just pushed.
    fn pushed(&mut self, ty: VTy) {
        if ty == VTy::I64 {
            // Producers outside the tagged path exclude a valid MIN by analysis.
            // Seed their independent tag without clobbering scratch locals.
            let depth = self.vstack.len();
            let value = self.stack_local(depth, ty);
            let valid = self.integer_valid_local(depth);
            self.f
                .instruction(&I::LocalTee(value))
                .instruction(&I::LocalGet(value))
                .instruction(&I::I64Const(NA_I64))
                .instruction(&I::I64Ne)
                .instruction(&I::LocalSet(valid));
        }
        self.vstack.push((ty, Loc::Phys));
    }

    /// Payload is already physical; copy its independent tag into the
    /// canonical depth local. Tags survive spills and block edges unchanged.
    fn pushed_integer_with_valid(&mut self, valid: u32) {
        let output_valid = self.integer_valid_local(self.vstack.len());
        self.f
            .instruction(&I::LocalGet(valid))
            .instruction(&I::LocalSet(output_valid));
        self.vstack.push((VTy::I64, Loc::Phys));
    }

    /// The emitted code consumed the (physical) top cell.
    fn consumed(&mut self) -> VTy {
        let (ty, loc) = self.vstack.pop().expect("emitter stack underflow");
        debug_assert_eq!(loc, Loc::Phys);
        ty
    }

    fn na_const(&mut self, ty: VTy) {
        match ty {
            VTy::F64 => self.f.instruction(&I::F64Const(f64::NAN.into())),
            VTy::I64 => self.f.instruction(&I::I64Const(NA_I64)),
            VTy::Bool3 => self.f.instruction(&I::I32Const(NA_BOOL3)),
            VTy::Color => self.f.instruction(&I::I32Const(NA_COLOR)),
            VTy::Str => self.f.instruction(&I::I32Const(NA_STR)),
            VTy::Ref => self.f.instruction(&I::I32Const(NA_REF)),
        };
    }

    fn emit_block_code(&mut self, x: usize) -> Result<(), AotError> {
        let a = self.ctx.analysis;
        let block = &a.blocks[x];

        // Reset the virtual stack from the block's typed entry state.
        self.vstack = a.entry_stack[x].iter().map(|&t| (t, Loc::Local)).collect();

        // All instructions except the terminator's branch effect. `pc` is
        // the function-wide program counter, not a slice index — the range
        // loop is the semantic form.
        let code = &self.ctx.program.main().code;
        let last = block.end - 1;
        let mut pc = block.start;
        while pc < block.end {
            if let Some((end, cost)) = self.fuel_group_end(pc, block.end) {
                self.emit_fuel_group(pc, end, cost, block.end)?;
                pc = end;
                continue;
            }
            let instr = &code[pc];
            // Charge the dispatch unit on the Wasm side so both the per-bar
            // and run-wide counters see every instruction. The interpreter
            // host charges only data-dependent extras and returns their cost
            // through scratch cell 0.
            if self.ctx.site_of.contains_key(&pc) {
                self.emit_budget_charge(1, pc as u32);
                if let Some((func, argc, plan)) = self.packed_color_call(pc) {
                    self.emit_packed_color_or_host(pc, func, argc, plan)?;
                } else {
                    self.emit_hosted_heap_charge(pc);
                    self.emit_host_call(pc)?;
                }
                pc += 1;
                continue;
            }
            if !self.budget_free(pc) {
                self.emit_instruction_budget(instr, pc as u32);
            }
            if pc == last && is_terminator(instr) {
                break; // handled by emit_terminator
            }
            self.emit_instr(pc)?;
            pc += 1;
        }
        Ok(())
    }

    /// The end of the fuel window starting at `start` and the cost its
    /// prelude precharges, if any ([`chart_fuel::chart_group_len`], for chart
    /// modules and standalone WebAssembly alike). A window never crosses an
    /// analyzed block boundary.
    fn fuel_group_end(&self, start: usize, block_end: usize) -> Option<(usize, u64)> {
        chart_window_end(start, block_end, |pc| self.chart_facts(pc))
    }

    /// The instruction carries no dispatch charge: a standalone projection's
    /// or a fraction carry's placeholder.
    fn budget_free(&self, pc: usize) -> bool {
        self.ctx
            .projection
            .is_some_and(|projection| projection.budget_free.contains(&pc))
            || self
                .ctx
                .carry
                .is_some_and(|carry| carry.budget_free.contains(&pc))
    }

    /// What the chart kernel needs to know about the instruction at `pc`.
    fn chart_facts(&self, pc: usize) -> ChartFacts {
        let program = self.ctx.program;
        let instr = &program.main().code[pc];
        ChartFacts {
            cost: if self.budget_free(pc) {
                0
            } else {
                crate::analysis::instr_cost(instr, &program.ta_states)
            },
            terminator: is_terminator(instr),
            meters: self.ctx.site_of.contains_key(&pc) || self.chart_meters(pc),
        }
    }

    /// Whether an instruction's own code reads or writes a budget counter: a
    /// dispatch cost computed at run time, a runtime linear extra
    /// ([`Self::emit_extra_or_reroute`]), an in-op extra (`NewObject`,
    /// `CopyObject`, a served chart call's string argument), or a standalone
    /// `request.security` read's one-time resample charge. Host sites are
    /// flagged by the caller.
    /// [`Self::emit_fuel_group`] refuses the module when a grouped
    /// instruction touches a counter anyway, so a miss here fails closed.
    fn chart_meters(&self, pc: usize) -> bool {
        let program = self.ctx.program;
        if self
            .ctx
            .projection
            .is_some_and(|projection| projection.security_site.contains_key(&pc))
        {
            return true;
        }
        match self.chart_site(pc) {
            Some(ChartSite::PeriodSeconds) => return true,
            Some(ChartSite::Calendar { extra, .. } | ChartSite::Timestamp { extra, .. })
                if extra > 0 =>
            {
                return true
            }
            _ => {}
        }
        match &program.main().code[pc] {
            CsilInstr::WindowMean { .. } | CsilInstr::CallTa { .. } => {
                !program.ta_input_bindings.is_empty()
            }
            CsilInstr::CallCollection { .. } => match self
                .ctx
                .analysis
                .heap
                .as_ref()
                .and_then(|plan| plan.coll.get(&pc))
            {
                Some((op, _)) => match op {
                    NativeCollOp::NewArray
                    | NativeCollOp::FromArgs
                    | NativeCollOp::Shift
                    | NativeCollOp::Clear
                    | NativeCollOp::Sum
                    | NativeCollOp::Avg
                    | NativeCollOp::Min
                    | NativeCollOp::Max
                    | NativeCollOp::Stdev
                    | NativeCollOp::Variance => true,
                    NativeCollOp::Push
                    | NativeCollOp::Get
                    | NativeCollOp::Set
                    | NativeCollOp::Size
                    | NativeCollOp::PopLast
                    | NativeCollOp::First
                    | NativeCollOp::Last => false,
                },
                None => true,
            },
            CsilInstr::NewObject { .. } | CsilInstr::CopyObject { .. } => true,
            _ => false,
        }
    }

    /// The low-fuel arm is the original emitter, so it still fails at the
    /// original PC after exactly the same earlier effects. The fast arm may
    /// precharge `cost` (the summed charges of `start..end`) because no
    /// grouped instruction can observe either counter: it either falls
    /// through or stops the run, and a stopped run never reads them. Both
    /// arms join through the same typed locals rather than exposing different
    /// physical Wasm stack shapes. A chart window may end with its block's
    /// terminator; the group charges its unit and the branch follows the
    /// group.
    fn emit_fuel_group(
        &mut self,
        start: usize,
        end: usize,
        cost: u64,
        block_end: usize,
    ) -> Result<(), AotError> {
        let ctx = self.ctx;
        let code = &ctx.program.main().code;
        let l = ctx.locals;
        let effects_end = match end.checked_sub(1).map(|last| &code[last]) {
            Some(last) if is_terminator(last) => end - 1,
            _ => end,
        };
        if (effects_end < end && end != block_end)
            || (start..effects_end).any(|pc| is_terminator(&code[pc]))
        {
            return Err(AotError::InvalidProgram(format!(
                "fuel group at pc {start} holds a terminator before its block ends"
            )));
        }
        let cost = cost as i64;
        self.flush_all();
        let entry_stack = self.vstack.clone();
        self.f
            .instruction(&I::LocalGet(l.budget))
            .instruction(&I::I64Const(cost))
            .instruction(&I::I64GeU)
            .instruction(&I::LocalGet(l.run_budget))
            .instruction(&I::I64Const(cost))
            .instruction(&I::I64GeU)
            .instruction(&I::I32And)
            .instruction(&I::If(BlockType::Empty));
        self.f
            .instruction(&I::LocalGet(l.budget))
            .instruction(&I::I64Const(cost))
            .instruction(&I::I64Sub)
            .instruction(&I::LocalSet(l.budget))
            .instruction(&I::LocalGet(l.run_budget))
            .instruction(&I::I64Const(cost))
            .instruction(&I::I64Sub)
            .instruction(&I::LocalSet(l.run_budget));
        for pc in start..effects_end {
            self.counter_touched = false;
            self.emit_instr(pc)?;
            if self.counter_touched {
                return Err(AotError::InvalidProgram(format!(
                    "fuel group at pc {start}: the instruction at pc {pc} reads a budget counter"
                )));
            }
        }
        self.flush_all();
        let exit_stack = self.vstack.clone();
        self.f.instruction(&I::Else);
        self.vstack = entry_stack;
        if let Some(stops) = ctx.hosts.stops {
            // The fallback arm is the same per-instruction path with the
            // counters in two globals, each charge one call to `charge`,
            // because inline checks would make up most of the module's code.
            // The arm is entered only when a counter is below the window's
            // summed charge, so some charge in it fails unless an instruction
            // stops the run first: entering it always ends the run, and it
            // runs at most once per run.
            self.f
                .instruction(&I::LocalGet(l.budget))
                .instruction(&I::GlobalSet(stops.budget_global))
                .instruction(&I::LocalGet(l.run_budget))
                .instruction(&I::GlobalSet(stops.run_budget_global));
            for pc in start..end {
                let charge = self.chart_facts(pc).cost;
                if charge > 0 {
                    self.f
                        .instruction(&I::I32Const(self.reported_pc(pc as u32) as i32))
                        .instruction(&I::I64Const(charge as i64))
                        .instruction(&I::LocalGet(l.bar))
                        .instruction(&I::Call(stops.charge))
                        .instruction(&I::If(BlockType::Empty))
                        .instruction(&I::I32Const(0))
                        .instruction(&I::I32Load(mem_u32_at(header::OFF_ERR_CODE)))
                        .instruction(&I::Return)
                        .instruction(&I::End);
                }
                if pc < effects_end {
                    self.emit_instr(pc)?;
                }
            }
            self.f
                .instruction(&I::GlobalGet(stops.budget_global))
                .instruction(&I::LocalSet(l.budget))
                .instruction(&I::GlobalGet(stops.run_budget_global))
                .instruction(&I::LocalSet(l.run_budget));
        } else {
            for (pc, instr) in code.iter().enumerate().take(end).skip(start) {
                if !self.budget_free(pc) {
                    self.emit_instruction_budget(instr, pc as u32);
                }
                if pc < effects_end {
                    self.emit_instr(pc)?;
                }
            }
        }
        self.flush_all();
        if self.vstack != exit_stack {
            return Err(AotError::InvalidProgram(format!(
                "fuel group at pc {start} has inconsistent exit stacks"
            )));
        }
        self.f.instruction(&I::End);
        Ok(())
    }

    /// Emit one `host.vm` call site (docs/aot.md §5): budget and operands
    /// go out through the scratch cells, the PineWASM interpreter executes the
    /// instruction at `pc` on its own state, and the (typed) result comes
    /// back in cell 1. A nonzero status carries a structured host error
    /// (5) or an ABI mismatch requiring the interpreter (6); either way
    /// the run stops with `{code, bar, csil_pc}` recorded.
    fn emit_host_call(&mut self, pc: usize) -> Result<(), AotError> {
        self.counter_touched = true;
        let l = self.ctx.locals;
        let site = *self.ctx.site_of.get(&pc).expect("checked by caller");
        let spec = &self.ctx.layout.host_sites[site as usize];
        let vm_idx = self.ctx.hosts.vm.ok_or_else(|| {
            AotError::InvalidProgram("host site without host.vm import".to_string())
        })?;
        let tys = self.ctx.analysis.operand_tys[pc].clone();
        debug_assert_eq!(tys.len(), spec.args.len(), "site spec drift at pc {pc}");
        let argc = tys.len();
        self.flush_all();
        debug_assert!(self.vstack.len() >= argc, "host-site stack underflow");
        let base = self.vstack.len() - argc;

        // A host opcode may have a data-dependent extra charge as large as
        // the remaining per-bar counter. If the run counter is smaller, the
        // CSAW ABI v6 scratch contract cannot pass both independent ceilings
        // into the host without ambiguity. Reroute before effects; the embedding host
        // can execute the request on the interpreter and produce the exact
        // structured result.
        self.f
            .instruction(&I::LocalGet(l.run_budget))
            .instruction(&I::LocalGet(l.budget))
            .instruction(&I::I64LtU)
            .instruction(&I::If(BlockType::Empty));
        self.err_exit(status::RUN_BUDGET_REROUTE, pc as u32);
        self.f.instruction(&I::End);

        let cell = |i: usize| MemArg {
            offset: (8 * (1 + i)) as u64,
            align: 3,
            memory_index: 0,
        };
        // Preserve the pre-host per-bar counter so the returned delta can be
        // mirrored into the run-wide counter.
        self.f
            .instruction(&I::LocalGet(l.budget))
            .instruction(&I::LocalSet(l.ti3));
        // cell 0: remaining per-bar budget out.
        self.f
            .instruction(&I::GlobalGet(self.ctx.globals.scratch_base))
            .instruction(&I::LocalGet(l.budget))
            .instruction(&I::I64Store(mem_i64()));
        // cells 1..: operands in argument order (deepest first).
        for (i, ty) in tys.iter().enumerate() {
            let arg_local = self.stack_local(base + i, *ty);
            self.f
                .instruction(&I::GlobalGet(self.ctx.globals.scratch_base))
                .instruction(&I::LocalGet(arg_local));
            match ty {
                VTy::F64 => {
                    self.f.instruction(&I::F64Store(cell(i)));
                }
                VTy::I64 => {
                    self.f.instruction(&I::I64Store(cell(i)));
                }
                VTy::Bool3 | VTy::Color | VTy::Str | VTy::Ref => {
                    self.f
                        .instruction(&I::I64ExtendI32S)
                        .instruction(&I::I64Store(cell(i)));
                }
            }
        }
        self.f
            .instruction(&I::I32Const(site as i32))
            .instruction(&I::LocalGet(l.bar));
        match self.strings() {
            // What is left of the string budget goes in with the call and
            // comes back as its reply. A negative reply is the call's
            // status, negated.
            Some(strings) => {
                self.f
                    .instruction(&I::GlobalGet(strings.room))
                    .instruction(&I::Call(strings.vm))
                    .instruction(&I::LocalTee(l.ti2))
                    .instruction(&I::I64Const(0))
                    .instruction(&I::I64LtS)
                    .instruction(&I::If(BlockType::Result(ValType::I32)))
                    .instruction(&I::I64Const(0))
                    .instruction(&I::LocalGet(l.ti2))
                    .instruction(&I::I64Sub)
                    .instruction(&I::I32WrapI64)
                    .instruction(&I::Else)
                    .instruction(&I::LocalGet(l.ti2))
                    .instruction(&I::GlobalSet(strings.room))
                    .instruction(&I::I32Const(status::OK))
                    .instruction(&I::End);
            }
            None => {
                self.f.instruction(&I::Call(vm_idx));
            }
        }
        // Nonzero status: record {code, bar, csil_pc} and stop the run.
        let reported_pc = self.reported_pc(pc as u32);
        self.f
            .instruction(&I::LocalTee(l.t32a))
            .instruction(&I::If(BlockType::Empty))
            .instruction(&I::I32Const(0))
            .instruction(&I::LocalGet(l.t32a))
            .instruction(&I::I32Store(mem_u32_at(header::OFF_ERR_CODE)))
            .instruction(&I::I32Const(0))
            .instruction(&I::LocalGet(l.bar))
            .instruction(&I::I32Store(mem_u32_at(header::OFF_ERR_BAR)))
            .instruction(&I::I32Const(0))
            .instruction(&I::I32Const(reported_pc as i32))
            .instruction(&I::I32Store(mem_u32_at(header::OFF_ERR_PC)))
            .instruction(&I::LocalGet(l.t32a))
            .instruction(&I::Return)
            .instruction(&I::End);
        // cell 0: remaining budget back (the host charged this site).
        self.f
            .instruction(&I::GlobalGet(self.ctx.globals.scratch_base))
            .instruction(&I::I64Load(mem_i64()))
            .instruction(&I::LocalSet(l.budget));
        // run_budget -= (pre_host_budget - returned_budget). The pre-call
        // guard proves the delta cannot exceed run_budget.
        self.f
            .instruction(&I::LocalGet(l.run_budget))
            .instruction(&I::LocalGet(l.ti3))
            .instruction(&I::LocalGet(l.budget))
            .instruction(&I::I64Sub)
            .instruction(&I::I64Sub)
            .instruction(&I::LocalSet(l.run_budget));

        self.vstack.truncate(base);
        if spec.ret.is_some() {
            let rty = self
                .ctx
                .analysis
                .result_tys
                .get(pc)
                .and_then(|r| r.first().copied())
                .unwrap_or(VTy::F64);
            self.f
                .instruction(&I::GlobalGet(self.ctx.globals.scratch_base));
            match rty {
                VTy::F64 => {
                    self.f.instruction(&I::F64Load(cell(0)));
                }
                VTy::I64 => {
                    self.f.instruction(&I::I64Load(cell(0)));
                }
                VTy::Bool3 | VTy::Color | VTy::Str | VTy::Ref => {
                    self.f
                        .instruction(&I::I64Load(cell(0)))
                        .instruction(&I::I32WrapI64);
                }
            }
            self.pushed(rty);
        }
        Ok(())
    }

    // ---- native heap (docs/aot.md §13) -------------------------------------

    /// Charge a hosted allocation against the module's own heap budgets.
    ///
    /// With a native heap the run has two heaps: this module's, and the
    /// interpreter host's for the objects that stay there (drawings). The
    /// interpreter has one, with one element budget and one object budget.
    /// Charging the host's allocations here as well makes the module's
    /// counters cover both, so the module never completes a run the
    /// interpreter would stop for a heap limit. A spent budget reroutes; the
    /// interpreter rerun owns the verdict.
    ///
    /// The charge is the interpreter's own (`Heap::alloc`: one object, one
    /// element per field) or an upper bound on it, and nothing is ever
    /// credited back, so the counters can only run ahead of the interpreter's
    /// live counts. [`crate::analysis::hosted_alloc`] names every hosted
    /// allocation a planned program may contain.
    fn emit_hosted_heap_charge(&mut self, pc: usize) {
        let (Some(_), Some(g)) = (self.ctx.analysis.heap.as_ref(), self.ctx.globals.heap) else {
            return;
        };
        let program = self.ctx.program;
        let elems = match crate::analysis::hosted_alloc(program, &program.main().code[pc]) {
            HostedAlloc::None => return,
            HostedAlloc::Elements(elems) => elems,
            // `plan_native_heap` refuses a program with one of these.
            HostedAlloc::Unbounded => {
                debug_assert!(false, "unbounded hosted allocation beside a native heap");
                self.err_exit(status::HEAP_REROUTE, pc as u32);
                return;
            }
        };
        let pcu = pc as u32;
        self.f
            .instruction(&I::GlobalGet(g.objs))
            .instruction(&I::I64Const(1))
            .instruction(&I::I64LtS);
        self.emit_reroute_if(pcu);
        self.f
            .instruction(&I::GlobalGet(g.elems))
            .instruction(&I::I64Const(elems as i64))
            .instruction(&I::I64LtS);
        self.emit_reroute_if(pcu);
        self.f
            .instruction(&I::GlobalGet(g.objs))
            .instruction(&I::I64Const(1))
            .instruction(&I::I64Sub)
            .instruction(&I::GlobalSet(g.objs));
        self.f
            .instruction(&I::GlobalGet(g.elems))
            .instruction(&I::I64Const(elems as i64))
            .instruction(&I::I64Sub)
            .instruction(&I::GlobalSet(g.elems));
        self.emit_arena_count(g);
    }

    /// One more object in the arena of a module with a collector
    /// (`Heap::arena_len`).
    fn emit_arena_count(&mut self, g: heap::HeapGlobals) {
        if let Some(gc) = g.gc {
            self.f
                .instruction(&I::GlobalGet(gc.arena))
                .instruction(&I::I64Const(1))
                .instruction(&I::I64Add)
                .instruction(&I::GlobalSet(gc.arena));
        }
    }

    /// Allocate a native object of `fields` fields into `t32a`, rerouting
    /// on failure. A module with a collector gives it the collector's
    /// trailer (no mark yet) and counts it in the arena.
    fn emit_object_alloc(&mut self, fields: usize, alloc: u32, g: heap::HeapGlobals, pc: u32) {
        let l = self.ctx.locals;
        let body = heap::object_fields_bytes(fields);
        let trailer = if g.gc.is_some() { 8 } else { 0 };
        self.f
            .instruction(&I::I32Const((body + trailer) as i32))
            .instruction(&I::Call(alloc))
            .instruction(&I::LocalTee(l.t32a))
            .instruction(&I::I32Eqz);
        self.emit_reroute_if(pc);
        if g.gc.is_some() {
            self.f
                .instruction(&I::LocalGet(l.t32a))
                .instruction(&I::I64Const(0))
                .instruction(&I::I64Store(MemArg {
                    offset: body,
                    align: 3,
                    memory_index: 0,
                }));
            self.emit_arena_count(g);
        }
    }

    /// Nonzero i32 on the wasm stack → record `{HEAP_REROUTE, bar, pc}` and
    /// return. The loader reruns the request on the PineWASM interpreter,
    /// which reproduces the exact structured error.
    fn emit_reroute_if(&mut self, pc: u32) {
        self.f.instruction(&I::If(BlockType::Empty));
        self.err_exit(status::HEAP_REROUTE, pc);
        self.f.instruction(&I::End);
    }

    /// Charge a RUNTIME linear-cost extra held in i64 local `extra` (already
    /// ≥ 0). Insufficient budget reroutes: the interpreter's message for a
    /// mid-collection budget abort differs from the dispatch one, so the
    /// rerun owns it.
    fn emit_extra_or_reroute(&mut self, extra: u32, pc: u32) {
        self.counter_touched = true;
        let budget = self.ctx.locals.budget;
        let run_budget = self.ctx.locals.run_budget;
        self.f
            .instruction(&I::LocalGet(budget))
            .instruction(&I::LocalGet(extra))
            .instruction(&I::I64LtU);
        self.emit_reroute_if(pc);
        self.f
            .instruction(&I::LocalGet(run_budget))
            .instruction(&I::LocalGet(extra))
            .instruction(&I::I64LtU)
            .instruction(&I::If(BlockType::Empty));
        self.err_exit(status::RUN_BUDGET_EXCEEDED, pc);
        self.f
            .instruction(&I::End)
            .instruction(&I::LocalGet(budget))
            .instruction(&I::LocalGet(extra))
            .instruction(&I::I64Sub)
            .instruction(&I::LocalSet(budget))
            .instruction(&I::LocalGet(run_budget))
            .instruction(&I::LocalGet(extra))
            .instruction(&I::I64Sub)
            .instruction(&I::LocalSet(run_budget));
    }

    /// `local <= 0` (na or invalid handle) → reroute.
    fn emit_recv_check(&mut self, recv: u32, pc: u32) {
        self.f
            .instruction(&I::LocalGet(recv))
            .instruction(&I::I32Const(0))
            .instruction(&I::I32LeS);
        self.emit_reroute_if(pc);
    }

    /// Load a value operand from its stack local, widening `i64 → f64`
    /// (sentinel → NaN) when the lane wants floats and the operand is int.
    fn emit_value_operand(&mut self, local: u32, got: VTy, want: VTy) {
        self.f.instruction(&I::LocalGet(local));
        if want == VTy::F64 && got == VTy::I64 {
            self.emit_i64_to_f64_na();
        }
    }

    fn heap_lane_log2(elem: VTy) -> i32 {
        match elem {
            VTy::F64 | VTy::I64 => 3,
            _ => 2,
        }
    }

    fn heap_elem_load(&mut self, elem: VTy) {
        match elem {
            VTy::F64 => self.f.instruction(&I::F64Load(mem_f64())),
            VTy::I64 => self.f.instruction(&I::I64Load(mem_i64())),
            _ => self.f.instruction(&I::I32Load(mem_i32())),
        };
    }

    fn heap_elem_store(&mut self, elem: VTy) {
        match elem {
            VTy::F64 => self.f.instruction(&I::F64Store(mem_f64())),
            VTy::I64 => self.f.instruction(&I::I64Store(mem_i64())),
            _ => self.f.instruction(&I::I32Store(mem_i32())),
        };
    }

    /// `recv.len` (u32) extended to i64 in local `into`.
    fn emit_len_i64(&mut self, recv: u32, into: u32) {
        self.f
            .instruction(&I::LocalGet(recv))
            .instruction(&I::I32Load(mem_u32_at(ARR_LEN as u32)))
            .instruction(&I::I64ExtendI32U)
            .instruction(&I::LocalSet(into));
    }

    /// Rewrite a negative `idx_i64` as `idx + len`, so `-1` names the last
    /// element as in the VM's `array.get`/`array.set`. An index still negative
    /// afterwards (below `-len`, or `na`) fails the caller's unsigned bounds
    /// compare. The operand local is consumed by the operation, so it is
    /// rewritten in place.
    fn emit_resolve_negative_index(&mut self, idx_i64: u32, len_i64: u32) {
        self.f
            .instruction(&I::LocalGet(idx_i64))
            .instruction(&I::I64Const(0))
            .instruction(&I::I64LtS)
            .instruction(&I::If(BlockType::Empty))
            .instruction(&I::LocalGet(idx_i64))
            .instruction(&I::LocalGet(len_i64))
            .instruction(&I::I64Add)
            .instruction(&I::LocalSet(idx_i64))
            .instruction(&I::End);
    }

    /// Push `recv.buf + wrap(idx_local) << log2` (element address) onto the
    /// wasm stack. The bounds check must have run already.
    fn emit_elem_addr(&mut self, recv: u32, idx_i64: u32, log2: i32) {
        self.f
            .instruction(&I::LocalGet(recv))
            .instruction(&I::I32Load(mem_u32_at(ARR_BUF as u32)))
            .instruction(&I::LocalGet(idx_i64))
            .instruction(&I::I32WrapI64)
            .instruction(&I::I32Const(log2))
            .instruction(&I::I32Shl)
            .instruction(&I::I32Add);
    }

    /// One native heap opcode. Only reached when [`Analysis::heap`] is set
    /// (heap opcodes without a plan are `host.vm` sites).
    #[allow(clippy::too_many_lines)]
    fn emit_native_heap(&mut self, pc: usize) -> Result<(), AotError> {
        use NativeCollOp as N;
        let plan = self.ctx.analysis.heap.as_ref().ok_or_else(|| {
            AotError::InvalidProgram(format!("native heap op without a plan at pc {pc}"))
        })?;
        let fns = self.ctx.hosts.heap.ok_or_else(|| {
            AotError::InvalidProgram("native heap op without helper functions".to_string())
        })?;
        let g = self.ctx.globals.heap.ok_or_else(|| {
            AotError::InvalidProgram("native heap op without heap globals".to_string())
        })?;
        let l = self.ctx.locals;
        let code = &self.ctx.program.main().code;
        let pcu = pc as u32;

        match &code[pc] {
            CsilInstr::CallCollection { .. } => {
                let (op, elem) = *plan.coll.get(&pc).ok_or_else(|| {
                    AotError::InvalidProgram(format!("no native plan for collection at pc {pc}"))
                })?;
                let tys = self.ctx.analysis.operand_tys[pc].clone();
                let argc = tys.len();
                self.flush_all();
                debug_assert!(self.vstack.len() >= argc, "heap-site stack underflow");
                let base = self.vstack.len() - argc;
                let log2 = Self::heap_lane_log2(elem);
                let (new_fn, push_fn, shift_fn, pop_fn) = match elem {
                    VTy::F64 => (fns.new_f64, fns.push_f64, fns.shift_f64, fns.pop_f64),
                    VTy::I64 => (fns.new_i64, fns.push_i64, fns.shift_i64, fns.pop_i64),
                    _ => (fns.new_i32, fns.push_i32, fns.shift_i32, fns.pop_i32),
                };

                match op {
                    N::NewArray => {
                        let size = self.stack_local(base, VTy::I64);
                        let fill = self.stack_local(base + 1, tys[1]);
                        // na size → 0 (`call_collection` parity).
                        self.f
                            .instruction(&I::LocalGet(size))
                            .instruction(&I::I64Const(NA_I64))
                            .instruction(&I::I64Eq)
                            .instruction(&I::If(BlockType::Result(ValType::I64)))
                            .instruction(&I::I64Const(0))
                            .instruction(&I::Else)
                            .instruction(&I::LocalGet(size))
                            .instruction(&I::End)
                            .instruction(&I::LocalSet(l.ti1));
                        // negative → structured error on the interpreter.
                        self.f
                            .instruction(&I::LocalGet(l.ti1))
                            .instruction(&I::I64Const(0))
                            .instruction(&I::I64LtS);
                        self.emit_reroute_if(pcu);
                        // absurd sizes would wrap the i32 layout math; the
                        // interpreter turns them into structured errors.
                        self.f
                            .instruction(&I::LocalGet(l.ti1))
                            .instruction(&I::I64Const(i64::from(i32::MAX)))
                            .instruction(&I::I64GtS);
                        self.emit_reroute_if(pcu);
                        // linear extra = max(size − 1, 0).
                        self.f
                            .instruction(&I::LocalGet(l.ti1))
                            .instruction(&I::I64Const(1))
                            .instruction(&I::I64Sub)
                            .instruction(&I::LocalTee(l.ti2))
                            .instruction(&I::I64Const(0))
                            .instruction(&I::I64LtS)
                            .instruction(&I::If(BlockType::Result(ValType::I64)))
                            .instruction(&I::I64Const(0))
                            .instruction(&I::Else)
                            .instruction(&I::LocalGet(l.ti2))
                            .instruction(&I::End)
                            .instruction(&I::LocalSet(l.ti2));
                        self.emit_extra_or_reroute(l.ti2, pcu);
                        // handle = new(size, fill); 0 → reroute.
                        self.f
                            .instruction(&I::LocalGet(l.ti1))
                            .instruction(&I::I32WrapI64);
                        self.emit_value_operand(fill, tys[1], elem);
                        self.f
                            .instruction(&I::Call(new_fn))
                            .instruction(&I::LocalTee(l.t32a))
                            .instruction(&I::I32Eqz);
                        self.emit_reroute_if(pcu);
                        self.vstack.truncate(base);
                        self.f.instruction(&I::LocalGet(l.t32a));
                        self.pushed(VTy::Ref);
                    }
                    N::FromArgs => {
                        // `array.from`: alloc `argc` elements (na-filled),
                        // then overwrite each from its operand. The linear
                        // extra is `argc − 1` (`collection_extra_cost`
                        // parity); insufficient budget reroutes like every
                        // collection-linear charge.
                        self.f
                            .instruction(&I::I64Const(argc.saturating_sub(1) as i64))
                            .instruction(&I::LocalSet(l.ti2));
                        self.emit_extra_or_reroute(l.ti2, pcu);
                        self.f.instruction(&I::I32Const(argc as i32));
                        self.na_const(elem);
                        self.f
                            .instruction(&I::Call(new_fn))
                            .instruction(&I::LocalTee(l.t32a))
                            .instruction(&I::I32Eqz);
                        self.emit_reroute_if(pcu);
                        for (i, &got) in tys.iter().enumerate() {
                            self.f
                                .instruction(&I::LocalGet(l.t32a))
                                .instruction(&I::I32Load(mem_u32_at(ARR_BUF as u32)));
                            self.emit_value_operand(self.stack_local(base + i, got), got, elem);
                            let off = (i as u64) << log2;
                            match elem {
                                VTy::F64 => self.f.instruction(&I::F64Store(MemArg {
                                    offset: off,
                                    align: 3,
                                    memory_index: 0,
                                })),
                                VTy::I64 => self.f.instruction(&I::I64Store(MemArg {
                                    offset: off,
                                    align: 3,
                                    memory_index: 0,
                                })),
                                _ => self.f.instruction(&I::I32Store(MemArg {
                                    offset: off,
                                    align: 2,
                                    memory_index: 0,
                                })),
                            };
                        }
                        self.vstack.truncate(base);
                        self.f.instruction(&I::LocalGet(l.t32a));
                        self.pushed(VTy::Ref);
                    }
                    N::Push => {
                        let recv = self.stack_local(base, VTy::Ref);
                        let value = self.stack_local(base + 1, tys[1]);
                        self.emit_recv_check(recv, pcu);
                        self.f.instruction(&I::LocalGet(recv));
                        self.emit_value_operand(value, tys[1], elem);
                        self.f.instruction(&I::Call(push_fn));
                        self.emit_reroute_if(pcu);
                        self.vstack.truncate(base);
                        self.na_const(elem);
                        self.pushed(elem);
                    }
                    N::Get => {
                        let recv = self.stack_local(base, VTy::Ref);
                        let idx = self.stack_local(base + 1, VTy::I64);
                        self.emit_recv_check(recv, pcu);
                        self.emit_len_i64(recv, l.ti1);
                        self.emit_resolve_negative_index(idx, l.ti1);
                        // one unsigned compare catches what is still negative,
                        // na, and oob
                        self.f
                            .instruction(&I::LocalGet(idx))
                            .instruction(&I::LocalGet(l.ti1))
                            .instruction(&I::I64GeU);
                        self.emit_reroute_if(pcu);
                        self.emit_elem_addr(recv, idx, log2);
                        self.heap_elem_load(elem);
                        self.vstack.truncate(base);
                        self.pushed(elem);
                    }
                    N::Set => {
                        let recv = self.stack_local(base, VTy::Ref);
                        let idx = self.stack_local(base + 1, VTy::I64);
                        let value = self.stack_local(base + 2, tys[2]);
                        self.emit_recv_check(recv, pcu);
                        self.emit_len_i64(recv, l.ti1);
                        self.emit_resolve_negative_index(idx, l.ti1);
                        self.f
                            .instruction(&I::LocalGet(idx))
                            .instruction(&I::LocalGet(l.ti1))
                            .instruction(&I::I64GeU);
                        self.emit_reroute_if(pcu);
                        self.emit_elem_addr(recv, idx, log2);
                        self.emit_value_operand(value, tys[2], elem);
                        self.heap_elem_store(elem);
                        self.vstack.truncate(base);
                        self.na_const(elem);
                        self.pushed(elem);
                    }
                    N::Size => {
                        let recv = self.stack_local(base, VTy::Ref);
                        self.emit_recv_check(recv, pcu);
                        self.f
                            .instruction(&I::LocalGet(recv))
                            .instruction(&I::I32Load(mem_u32_at(ARR_LEN as u32)))
                            .instruction(&I::I64ExtendI32U);
                        self.vstack.truncate(base);
                        self.pushed(VTy::I64);
                    }
                    N::Shift | N::PopLast => {
                        let recv = self.stack_local(base, VTy::Ref);
                        self.emit_recv_check(recv, pcu);
                        self.emit_len_i64(recv, l.ti1);
                        self.f
                            .instruction(&I::LocalGet(l.ti1))
                            .instruction(&I::I64Eqz);
                        self.emit_reroute_if(pcu);
                        if matches!(op, N::Shift) {
                            // linear extra = len − 1 (len ≥ 1 here).
                            self.f
                                .instruction(&I::LocalGet(l.ti1))
                                .instruction(&I::I64Const(1))
                                .instruction(&I::I64Sub)
                                .instruction(&I::LocalSet(l.ti2));
                            self.emit_extra_or_reroute(l.ti2, pcu);
                        }
                        self.f.instruction(&I::LocalGet(recv)).instruction(&I::Call(
                            if matches!(op, N::Shift) {
                                shift_fn
                            } else {
                                pop_fn
                            },
                        ));
                        self.vstack.truncate(base);
                        self.pushed(elem);
                    }
                    N::First | N::Last => {
                        let recv = self.stack_local(base, VTy::Ref);
                        self.emit_recv_check(recv, pcu);
                        self.emit_len_i64(recv, l.ti1);
                        self.f
                            .instruction(&I::LocalGet(l.ti1))
                            .instruction(&I::I64Eqz);
                        self.emit_reroute_if(pcu);
                        self.f
                            .instruction(&I::LocalGet(recv))
                            .instruction(&I::I32Load(mem_u32_at(ARR_BUF as u32)));
                        if matches!(op, N::Last) {
                            self.f
                                .instruction(&I::LocalGet(l.ti1))
                                .instruction(&I::I32WrapI64)
                                .instruction(&I::I32Const(1))
                                .instruction(&I::I32Sub)
                                .instruction(&I::I32Const(log2))
                                .instruction(&I::I32Shl)
                                .instruction(&I::I32Add);
                        }
                        self.heap_elem_load(elem);
                        self.vstack.truncate(base);
                        self.pushed(elem);
                    }
                    N::Clear => {
                        let recv = self.stack_local(base, VTy::Ref);
                        self.emit_recv_check(recv, pcu);
                        self.emit_len_i64(recv, l.ti1);
                        // linear extra = max(len − 1, 0).
                        self.f
                            .instruction(&I::LocalGet(l.ti1))
                            .instruction(&I::I64Const(1))
                            .instruction(&I::I64Sub)
                            .instruction(&I::LocalTee(l.ti2))
                            .instruction(&I::I64Const(0))
                            .instruction(&I::I64LtS)
                            .instruction(&I::If(BlockType::Result(ValType::I64)))
                            .instruction(&I::I64Const(0))
                            .instruction(&I::Else)
                            .instruction(&I::LocalGet(l.ti2))
                            .instruction(&I::End)
                            .instruction(&I::LocalSet(l.ti2));
                        self.emit_extra_or_reroute(l.ti2, pcu);
                        // elems_left += len; len = 0 (`Heap::release` parity).
                        self.f
                            .instruction(&I::GlobalGet(g.elems))
                            .instruction(&I::LocalGet(l.ti1))
                            .instruction(&I::I64Add)
                            .instruction(&I::GlobalSet(g.elems));
                        self.f
                            .instruction(&I::LocalGet(recv))
                            .instruction(&I::I32Const(0))
                            .instruction(&I::I32Store(mem_u32_at(ARR_LEN as u32)));
                        self.vstack.truncate(base);
                        self.na_const(elem);
                        self.pushed(elem);
                    }
                    N::Sum | N::Avg | N::Min | N::Max | N::Stdev | N::Variance => {
                        let recv = self.stack_local(base, VTy::Ref);
                        self.emit_recv_check(recv, pcu);
                        self.emit_len_i64(recv, l.ti1);
                        self.f
                            .instruction(&I::LocalGet(l.ti1))
                            .instruction(&I::I64Const(1))
                            .instruction(&I::I64Sub)
                            .instruction(&I::LocalTee(l.ti2))
                            .instruction(&I::I64Const(0))
                            .instruction(&I::I64LtS)
                            .instruction(&I::If(BlockType::Result(ValType::I64)))
                            .instruction(&I::I64Const(0))
                            .instruction(&I::Else)
                            .instruction(&I::LocalGet(l.ti2))
                            .instruction(&I::End)
                            .instruction(&I::LocalSet(l.ti2));
                        self.emit_extra_or_reroute(l.ti2, pcu);
                        if matches!(op, N::Stdev | N::Variance) {
                            let (var_f64, var_i64) = fns.var.ok_or_else(|| {
                                AotError::InvalidProgram(format!(
                                    "array.stdev/variance at pc {pc} without its helper"
                                ))
                            })?;
                            // `biased` is a bool3 cell: only `false` (0)
                            // selects the sample form.
                            let biased = self.stack_local(base + 1, VTy::Bool3);
                            self.f
                                .instruction(&I::LocalGet(recv))
                                .instruction(&I::LocalGet(biased))
                                .instruction(&I::I32Const(i32::from(matches!(op, N::Stdev))))
                                .instruction(&I::Call(match elem {
                                    VTy::I64 => var_i64,
                                    _ => var_f64,
                                }));
                        } else {
                            let agg = match elem {
                                VTy::I64 => fns.agg_i64,
                                _ => fns.agg_f64,
                            };
                            let sel = match op {
                                N::Sum => agg_op::SUM,
                                N::Avg => agg_op::AVG,
                                N::Min => agg_op::MIN,
                                _ => agg_op::MAX,
                            };
                            self.f
                                .instruction(&I::LocalGet(recv))
                                .instruction(&I::I32Const(sel))
                                .instruction(&I::Call(agg));
                        }
                        self.vstack.truncate(base);
                        self.pushed(VTy::F64);
                    }
                }
            }
            CsilInstr::NewObject {
                type_id,
                field_count,
            } => {
                let n = *field_count as usize;
                let layout: Vec<VTy> = (0..n).map(|i| plan.field_ty(*type_id, i as u32)).collect();
                let tys = self.ctx.analysis.operand_tys[pc].clone();
                // `exec_new_object` extra: constructing N fields costs N
                // (dispatch already charged 1; same message and pc).
                if n > 1 {
                    self.emit_budget_charge((n - 1) as u64, pcu);
                }
                self.flush_all();
                let base = self.vstack.len() - n;
                self.f
                    .instruction(&I::GlobalGet(g.objs))
                    .instruction(&I::I64Const(1))
                    .instruction(&I::I64LtS);
                self.emit_reroute_if(pcu);
                self.f
                    .instruction(&I::GlobalGet(g.elems))
                    .instruction(&I::I64Const(n as i64))
                    .instruction(&I::I64LtS);
                self.emit_reroute_if(pcu);
                self.emit_object_alloc(n, fns.alloc, g, pcu);
                for (i, &want) in layout.iter().enumerate() {
                    let value = self.stack_local(base + i, tys[i]);
                    self.f.instruction(&I::LocalGet(l.t32a));
                    self.emit_value_operand(value, tys[i], want);
                    let off = (8 * i) as u64;
                    match want {
                        VTy::F64 => self.f.instruction(&I::F64Store(MemArg {
                            offset: off,
                            align: 3,
                            memory_index: 0,
                        })),
                        VTy::I64 => self.f.instruction(&I::I64Store(MemArg {
                            offset: off,
                            align: 3,
                            memory_index: 0,
                        })),
                        _ => self.f.instruction(&I::I32Store(MemArg {
                            offset: off,
                            align: 2,
                            memory_index: 0,
                        })),
                    };
                }
                self.f
                    .instruction(&I::GlobalGet(g.objs))
                    .instruction(&I::I64Const(1))
                    .instruction(&I::I64Sub)
                    .instruction(&I::GlobalSet(g.objs));
                self.f
                    .instruction(&I::GlobalGet(g.elems))
                    .instruction(&I::I64Const(n as i64))
                    .instruction(&I::I64Sub)
                    .instruction(&I::GlobalSet(g.elems));
                self.vstack.truncate(base);
                self.f.instruction(&I::LocalGet(l.t32a));
                self.pushed(VTy::Ref);
            }
            CsilInstr::CopyObject { type_id } => {
                let n = plan.layouts.get(type_id).map(Vec::len).unwrap_or_default();
                if n > 1 {
                    self.emit_budget_charge((n - 1) as u64, pcu);
                }
                self.flush_all();
                let base = self.vstack.len() - 1;
                let recv = self.stack_local(base, VTy::Ref);
                self.emit_recv_check(recv, pcu);
                // The interpreter reserves the bytes of the string fields it
                // clones, in one reservation, before it allocates the copy
                // (`Vm::exec_copy_object`).
                let string_fields: Vec<usize> = plan
                    .layouts
                    .get(type_id)
                    .map(|layout| {
                        (0..layout.len())
                            .filter(|&field| layout[field] == VTy::Str)
                            .collect()
                    })
                    .unwrap_or_default();
                if let (Some(strings), false) = (self.strings(), string_fields.is_empty()) {
                    self.f
                        .instruction(&I::I64Const(0))
                        .instruction(&I::LocalSet(l.ti2));
                    for field in string_fields {
                        self.f
                            .instruction(&I::LocalGet(recv))
                            .instruction(&I::I32Load(MemArg {
                                offset: 8 * field as u64,
                                align: 2,
                                memory_index: 0,
                            }));
                        self.emit_string_len(strings, pcu);
                        self.f
                            .instruction(&I::LocalGet(l.ti2))
                            .instruction(&I::I64Add)
                            .instruction(&I::LocalSet(l.ti2));
                    }
                    self.emit_string_reserve(strings, StringBytes::InTi2, pcu)?;
                }
                self.f
                    .instruction(&I::GlobalGet(g.objs))
                    .instruction(&I::I64Const(1))
                    .instruction(&I::I64LtS);
                self.emit_reroute_if(pcu);
                self.f
                    .instruction(&I::GlobalGet(g.elems))
                    .instruction(&I::I64Const(n as i64))
                    .instruction(&I::I64LtS);
                self.emit_reroute_if(pcu);
                self.emit_object_alloc(n, fns.alloc, g, pcu);
                self.f
                    .instruction(&I::LocalGet(l.t32a))
                    .instruction(&I::LocalGet(recv))
                    .instruction(&I::I32Const(8 * n as i32))
                    .instruction(&I::MemoryCopy {
                        src_mem: 0,
                        dst_mem: 0,
                    });
                self.f
                    .instruction(&I::GlobalGet(g.objs))
                    .instruction(&I::I64Const(1))
                    .instruction(&I::I64Sub)
                    .instruction(&I::GlobalSet(g.objs));
                self.f
                    .instruction(&I::GlobalGet(g.elems))
                    .instruction(&I::I64Const(n as i64))
                    .instruction(&I::I64Sub)
                    .instruction(&I::GlobalSet(g.elems));
                self.vstack.truncate(base);
                self.f.instruction(&I::LocalGet(l.t32a));
                self.pushed(VTy::Ref);
            }
            CsilInstr::GetField { field_idx } => {
                let tid = *plan.site_obj.get(&pc).ok_or_else(|| {
                    AotError::InvalidProgram(format!("no object plan at pc {pc}"))
                })?;
                let ty = plan.field_ty(tid, *field_idx);
                self.flush_all();
                let base = self.vstack.len() - 1;
                let recv = self.stack_local(base, VTy::Ref);
                self.emit_recv_check(recv, pcu);
                let off = u64::from(*field_idx) * 8;
                self.f.instruction(&I::LocalGet(recv));
                match ty {
                    VTy::F64 => self.f.instruction(&I::F64Load(MemArg {
                        offset: off,
                        align: 3,
                        memory_index: 0,
                    })),
                    VTy::I64 => self.f.instruction(&I::I64Load(MemArg {
                        offset: off,
                        align: 3,
                        memory_index: 0,
                    })),
                    _ => self.f.instruction(&I::I32Load(MemArg {
                        offset: off,
                        align: 2,
                        memory_index: 0,
                    })),
                };
                self.vstack.truncate(base);
                self.pushed(ty);
            }
            CsilInstr::SetField { field_idx } => {
                let tid = *plan.site_obj.get(&pc).ok_or_else(|| {
                    AotError::InvalidProgram(format!("no object plan at pc {pc}"))
                })?;
                let want = plan.field_ty(tid, *field_idx);
                let tys = self.ctx.analysis.operand_tys[pc].clone();
                self.flush_all();
                let base = self.vstack.len() - 2;
                let recv = self.stack_local(base, VTy::Ref);
                let value = self.stack_local(base + 1, tys[1]);
                self.emit_recv_check(recv, pcu);
                let off = u64::from(*field_idx) * 8;
                self.f.instruction(&I::LocalGet(recv));
                self.emit_value_operand(value, tys[1], want);
                match want {
                    VTy::F64 => self.f.instruction(&I::F64Store(MemArg {
                        offset: off,
                        align: 3,
                        memory_index: 0,
                    })),
                    VTy::I64 => self.f.instruction(&I::I64Store(MemArg {
                        offset: off,
                        align: 3,
                        memory_index: 0,
                    })),
                    _ => self.f.instruction(&I::I32Store(MemArg {
                        offset: off,
                        align: 2,
                        memory_index: 0,
                    })),
                };
                self.vstack.truncate(base);
            }
            other => {
                return Err(AotError::InvalidProgram(format!(
                    "emit_native_heap on non-heap opcode {other:?}"
                )))
            }
        }
        Ok(())
    }

    /// Push a validated bound parameter, retaining constants for literal sites.
    fn emit_ta_parameter(&mut self, target: pinewasm_hir::TaInputTarget, default: i64) {
        if let Some(binding) = self
            .ctx
            .program
            .ta_input_bindings
            .iter()
            .find(|b| b.target == target)
        {
            crate::emit::emit_ta_input_value(
                self.f,
                binding,
                crate::emit::ta_input_rule(self.ctx.program, binding),
            );
        } else {
            self.f.instruction(&I::I64Const(default));
        }
    }

    /// Charge before effects, preserving the interpreter's failure location
    /// and committed output. Comparisons retain the ABI's full u64 budget.
    fn emit_instruction_budget(&mut self, instr: &CsilInstr, pc: u32) {
        use pinewasm_hir::TaInputTarget;
        use pinewasm_ta::TaCost;
        if self.ctx.program.ta_input_bindings.is_empty() {
            self.emit_budget_charge(
                crate::analysis::instr_cost(instr, &self.ctx.program.ta_states),
                pc,
            );
            return;
        }
        match instr {
            CsilInstr::WindowMean { slot, length } => {
                self.emit_ta_parameter(TaInputTarget::Sma { slot: *slot }, i64::from(*length));
            }
            CsilInstr::CallTa { state } => {
                let decl = &self.ctx.program.ta_states[*state as usize];
                let first = decl.params.first().copied().unwrap_or(1);
                let second = decl.params.get(1).copied().unwrap_or(1);
                match decl.func.engine_cost_kind() {
                    TaCost::Fixed(cost) => {
                        self.f.instruction(&I::I64Const(cost as i64));
                    }
                    TaCost::Length { extra } => {
                        self.emit_ta_cost_length(*state, 0, first);
                        self.f
                            .instruction(&I::I64Const(extra as i64))
                            .instruction(&I::I64Add);
                    }
                    TaCost::Pivots => {
                        self.emit_ta_cost_length(*state, 0, first);
                        self.emit_ta_cost_length(*state, 1, second);
                        self.f
                            .instruction(&I::I64Add)
                            .instruction(&I::I64Const(1))
                            .instruction(&I::I64Add);
                    }
                    TaCost::Hma => {
                        self.emit_ta_cost_length(*state, 0, first);
                        self.f
                            .instruction(&I::LocalTee(self.ctx.locals.ti0))
                            .instruction(&I::LocalGet(self.ctx.locals.ti0))
                            .instruction(&I::I64Const(2))
                            .instruction(&I::I64DivU)
                            .instruction(&I::I64Add)
                            .instruction(&I::LocalGet(self.ctx.locals.ti0))
                            .instruction(&I::F64ConvertI64U)
                            .instruction(&I::F64Sqrt)
                            .instruction(&I::F64Const(0.5.into()))
                            .instruction(&I::F64Add)
                            .instruction(&I::I64TruncF64U)
                            .instruction(&I::I64Add);
                    }
                }
            }
            _ => {
                self.f.instruction(&I::I64Const(1));
            }
        }
        self.f.instruction(&I::LocalSet(self.ctx.locals.ti3));
        self.emit_budget_charge_local(pc);
    }

    fn emit_ta_cost_length(&mut self, state: u32, param: u32, default: i64) {
        self.emit_ta_parameter(pinewasm_hir::TaInputTarget::State { state, param }, default);
        // The engine meters zero-width pivot sides as one unit.
        self.f
            .instruction(&I::LocalTee(self.ctx.locals.ti1))
            .instruction(&I::I64Const(1))
            .instruction(&I::LocalGet(self.ctx.locals.ti1))
            .instruction(&I::I64Const(1))
            .instruction(&I::I64GtS)
            .instruction(&I::Select);
    }

    fn emit_budget_charge(&mut self, cost: u64, pc: u32) {
        self.emit_budget_charge_operand(Some(cost), pc);
    }

    fn emit_budget_charge_local(&mut self, pc: u32) {
        self.emit_budget_charge_operand(None, pc);
    }

    fn emit_budget_charge_operand(&mut self, cost: Option<u64>, pc: u32) {
        self.counter_touched = true;
        let cost_local = self.ctx.locals.ti3;
        let operand = || cost.map_or(I::LocalGet(cost_local), |value| I::I64Const(value as i64));
        let budget = self.ctx.locals.budget;
        let run_budget = self.ctx.locals.run_budget;
        if let Some(stops) = self.ctx.hosts.stops {
            // One test for both counters, and one shared stop function that
            // picks the status: the per-bar one when that counter is short,
            // as the two separate tests below report it.
            let reported = self.reported_pc(pc);
            self.f
                .instruction(&I::LocalGet(budget))
                .instruction(&operand())
                .instruction(&I::I64LtU)
                .instruction(&I::LocalGet(run_budget))
                .instruction(&operand())
                .instruction(&I::I64LtU)
                .instruction(&I::I32Or)
                .instruction(&I::If(BlockType::Empty))
                .instruction(&I::I32Const(reported as i32))
                .instruction(&I::LocalGet(budget))
                .instruction(&operand())
                .instruction(&I::LocalGet(self.ctx.locals.bar))
                .instruction(&I::Call(stops.budget_stop))
                .instruction(&I::Return)
                .instruction(&I::End)
                .instruction(&I::LocalGet(budget))
                .instruction(&operand())
                .instruction(&I::I64Sub)
                .instruction(&I::LocalSet(budget))
                .instruction(&I::LocalGet(run_budget))
                .instruction(&operand())
                .instruction(&I::I64Sub)
                .instruction(&I::LocalSet(run_budget));
            return;
        }
        self.f
            .instruction(&I::LocalGet(budget))
            .instruction(&operand())
            .instruction(&I::I64LtU)
            .instruction(&I::If(BlockType::Empty));
        self.err_exit(status::BUDGET_EXCEEDED, pc);
        self.f
            .instruction(&I::End)
            .instruction(&I::LocalGet(run_budget))
            .instruction(&operand())
            .instruction(&I::I64LtU)
            .instruction(&I::If(BlockType::Empty));
        self.err_exit(status::RUN_BUDGET_EXCEEDED, pc);
        self.f
            .instruction(&I::End)
            .instruction(&I::LocalGet(budget))
            .instruction(&operand())
            .instruction(&I::I64Sub)
            .instruction(&I::LocalSet(budget))
            .instruction(&I::LocalGet(run_budget))
            .instruction(&operand())
            .instruction(&I::I64Sub)
            .instruction(&I::LocalSet(run_budget));
    }

    /// The terminator prelude of block `x` ([`Step::Term`]): flush the
    /// virtual stack and, for a two-way branch, leave the i32 test on the
    /// real stack. The branch steps that follow come from the plan.
    fn emit_terminator(&mut self, x: usize) -> Result<(), AotError> {
        let code = &self.ctx.program.main().code;
        let block = &self.ctx.analysis.blocks[x];
        let last = &code[block.end - 1];
        let term = self.ctx.analysis.term[x];

        match (last, term) {
            (CsilInstr::Return, Term::Return)
            | (CsilInstr::Jump { .. }, Term::Jump(_))
            | (_, Term::Fall(_)) => {
                self.flush_all();
                Ok(())
            }
            (CsilInstr::JumpIfFalse { .. }, Term::Branch { .. }) => {
                // Jumps when the bool3 cond is false OR na (anything != 2).
                self.flush_below_top();
                self.consumed();
                self.f.instruction(&I::I32Const(2)).instruction(&I::I32Ne);
                Ok(())
            }
            (CsilInstr::JumpIfExact { on, .. }, Term::Branch { .. }) => {
                self.flush_below_top();
                self.consumed();
                if *on {
                    self.f.instruction(&I::I32Const(2)).instruction(&I::I32Eq);
                } else {
                    self.f.instruction(&I::I32Eqz);
                }
                Ok(())
            }
            (CsilInstr::JumpIfSlotWritten { slot, .. }, Term::Branch { .. }) => {
                self.flush_all();
                self.f
                    .instruction(&I::GlobalGet(self.ctx.globals.written_base))
                    .instruction(&I::I32Const(*slot as i32))
                    .instruction(&I::I32Add)
                    .instruction(&I::I32Load8U(mem_u8()));
                Ok(())
            }
            (other, term) => Err(AotError::InvalidProgram(format!(
                "terminator mismatch: {other:?} vs {term:?}"
            ))),
        }
    }

    /// A chart-tier carry moved instruction offsets; report the one the
    /// caller's program has.
    fn reported_pc(&self, pc: u32) -> u32 {
        self.ctx
            .carry
            .and_then(|carry| carry.origin.get(pc as usize).copied())
            .and_then(|origin| u32::try_from(origin).ok())
            .unwrap_or(pc)
    }

    fn err_exit(&mut self, code: i32, pc: u32) {
        let pc = self.reported_pc(pc);
        if let Some(stops) = self.ctx.hosts.stops {
            self.f
                .instruction(&I::I32Const(code))
                .instruction(&I::I32Const(pc as i32))
                .instruction(&I::LocalGet(self.ctx.locals.bar))
                .instruction(&I::Call(stops.stop))
                .instruction(&I::Return);
            return;
        }
        self.f
            .instruction(&I::I32Const(0))
            .instruction(&I::I32Const(code))
            .instruction(&I::I32Store(mem_u32_at(header::OFF_ERR_CODE)))
            .instruction(&I::I32Const(0))
            .instruction(&I::LocalGet(self.ctx.locals.bar))
            .instruction(&I::I32Store(mem_u32_at(header::OFF_ERR_BAR)))
            .instruction(&I::I32Const(0))
            .instruction(&I::I32Const(pc as i32))
            .instruction(&I::I32Store(mem_u32_at(header::OFF_ERR_PC)))
            .instruction(&I::I32Const(code))
            .instruction(&I::Return);
    }

    /// [`Self::err_exit`] with the status on the stack.
    fn err_exit_with_status(&mut self, pc: u32) -> Result<(), AotError> {
        let pc = self.reported_pc(pc);
        let stops = self.ctx.hosts.stops.ok_or_else(|| {
            AotError::InvalidProgram("a computed stop needs the stop functions".into())
        })?;
        self.f
            .instruction(&I::I32Const(pc as i32))
            .instruction(&I::LocalGet(self.ctx.locals.bar))
            .instruction(&I::Call(stops.stop))
            .instruction(&I::Return);
        Ok(())
    }

    /// The string budget this module keeps, if it keeps one
    /// ([`StringFns`]).
    fn strings(&self) -> Option<StringFns> {
        self.ctx.hosts.linked.and_then(|linked| linked.strings)
    }

    /// The length of the string the instruction before `pc` pushed, when
    /// that is the value on top of the stack at `pc` whatever path led
    /// there: the instruction before is a string constant or `na` in the
    /// same block. `na` is the interpreter's empty message.
    fn pushed_string_len(&self, pc: usize) -> Option<usize> {
        let program = self.ctx.program;
        if self
            .ctx
            .analysis
            .blocks
            .iter()
            .any(|block| block.start == pc)
        {
            return None;
        }
        match program.main().code.get(pc.checked_sub(1)?)? {
            CsilInstr::PushStr(id) => program.constants.get(*id as usize).map(String::len),
            CsilInstr::PushNa => Some(0),
            _ => None,
        }
    }

    /// Replace the string cell on the stack with its string's length in
    /// bytes, as an `i64`: 0 for `na`. A cell that names no string stops the
    /// run for the interpreter. Uses `ti3`.
    fn emit_string_len(&mut self, strings: StringFns, pc: u32) {
        let l = self.ctx.locals;
        self.f
            .instruction(&I::Call(strings.len))
            .instruction(&I::LocalTee(l.ti3))
            .instruction(&I::I64Const(0))
            .instruction(&I::I64LtS)
            .instruction(&I::If(BlockType::Empty));
        self.err_exit(status::ABI_MISMATCH, pc);
        self.f.instruction(&I::End).instruction(&I::LocalGet(l.ti3));
    }

    /// Take `bytes` from what is left of the string budget, where the
    /// interpreter reserves them at the instruction at `pc`
    /// ([`StringFns`]). When they fit, that is the whole reservation: one
    /// subtraction and one test. When they do not, the linked host makes the
    /// interpreter's own reservation, which fails with the interpreter's
    /// error, and the run stops with the status the host returns. That arm
    /// never rejoins the instruction's code, so it costs the other nothing.
    /// Leaves the stack as it found it and uses `ti3`.
    fn emit_string_reserve(
        &mut self,
        strings: StringFns,
        bytes: StringBytes,
        pc: u32,
    ) -> Result<(), AotError> {
        let l = self.ctx.locals;
        let operand = match bytes {
            StringBytes::Known(bytes) => I::I64Const(i64::try_from(bytes).map_err(|_| {
                AotError::InvalidProgram(format!("a string of {bytes} bytes at pc {pc}"))
            })?),
            StringBytes::InTi2 => I::LocalGet(l.ti2),
        };
        let reported = self.reported_pc(pc);
        self.f
            .instruction(&I::GlobalGet(strings.room))
            .instruction(&operand)
            .instruction(&I::I64Sub)
            .instruction(&I::LocalTee(l.ti3))
            .instruction(&I::I64Const(0))
            .instruction(&I::I64LtS)
            .instruction(&I::If(BlockType::Empty))
            .instruction(&I::I32Const(reported as i32))
            .instruction(&I::LocalGet(l.bar))
            .instruction(&I::GlobalGet(strings.room))
            .instruction(&operand)
            .instruction(&I::Call(strings.reserve));
        self.err_exit_with_status(pc)?;
        self.f
            .instruction(&I::End)
            .instruction(&I::LocalGet(l.ti3))
            .instruction(&I::GlobalSet(strings.room));
        Ok(())
    }

    fn exact_guarded(&self, pc: usize) -> bool {
        self.ctx
            .projection
            .is_some_and(|projection| projection.exact_guard.contains(&pc))
            || self
                .ctx
                .carry
                .is_some_and(|carry| carry.exact_guard.contains(&pc))
    }

    /// Stop the run because a carried whole number would lose bits. The
    /// standalone module has nobody to hand the run to and reports it. A
    /// chart-tier module asks its loader to rerun the request on the
    /// interpreter, which holds the value exactly: `ABI_MISMATCH`, the
    /// status of a value that does not fit its static cell, which the
    /// integer-minimum guards already return on this tier.
    fn fraction_stop(&mut self, pc: usize) {
        let code = if self.ctx.carry.is_some() {
            status::ABI_MISMATCH
        } else {
            status::INT_FRACTION_RANGE
        };
        self.err_exit(code, pc as u32);
    }

    /// `x == trunc(x)` for the f64 local: a whole value (never `na`).
    fn emit_is_whole(&mut self, local: u32) {
        self.f
            .instruction(&I::LocalGet(local))
            .instruction(&I::LocalGet(local))
            .instruction(&I::F64Trunc)
            .instruction(&I::F64Eq);
    }

    /// `|x| < 2^63` for the f64 local: it converts to i64 without loss.
    fn emit_fits_i64(&mut self, local: u32) {
        self.f
            .instruction(&I::LocalGet(local))
            .instruction(&I::F64Abs)
            .instruction(&I::F64Const(9_223_372_036_854_775_808.0.into()))
            .instruction(&I::F64Lt);
    }

    /// Whether f64 cannot hold the i64 local exactly: it does not survive a
    /// round trip through f64, or it converts to 2^63. The second test is
    /// for `i64::MAX`: it rounds up to 2^63, and the saturating conversion
    /// brings 2^63 back to `i64::MAX`, so the round trip alone calls it
    /// exact. (Its neighbours down to `i64::MAX - 511` also convert to 2^63
    /// and already fail the round trip.)
    fn emit_i64_not_exact_in_f64(&mut self, local: u32) {
        self.f
            .instruction(&I::LocalGet(local))
            .instruction(&I::F64ConvertI64S)
            .instruction(&I::I64TruncSatF64S)
            .instruction(&I::LocalGet(local))
            .instruction(&I::I64Ne)
            .instruction(&I::LocalGet(local))
            .instruction(&I::F64ConvertI64S)
            .instruction(&I::F64Const(9_223_372_036_854_775_808.0.into()))
            .instruction(&I::F64Ge)
            .instruction(&I::I32Or);
    }

    /// A fraction-carry `+`, `-`, `*` or `/` (see `fraction.rs`).
    ///
    /// The carry holds a statically-int value in f64. A fractional operand
    /// means the interpreter is doing this same float operation, so nothing
    /// needs checking. Two whole operands may be ints in the interpreter,
    /// whose arithmetic is exact: the float result is the same number only
    /// when it is exact too, and this stops the run when it is not. A
    /// non-finite result is the canonical NaN in every case.
    fn emit_exact_carried_binop(&mut self, instr: &CsilInstr, pc: usize) {
        let l = self.ctx.locals;
        self.want(2);
        self.consumed();
        self.consumed();
        self.f
            .instruction(&I::LocalSet(l.tf1))
            .instruction(&I::LocalSet(l.tf0));
        if matches!(instr, CsilInstr::F64Sub) {
            // a - b is a + (-b), bit for bit.
            self.f
                .instruction(&I::LocalGet(l.tf1))
                .instruction(&I::F64Neg)
                .instruction(&I::LocalSet(l.tf1));
        }
        self.f
            .instruction(&I::LocalGet(l.tf0))
            .instruction(&I::LocalGet(l.tf1));
        match instr {
            CsilInstr::F64Add | CsilInstr::F64Sub => self.f.instruction(&I::F64Add),
            CsilInstr::F64Mul => self.f.instruction(&I::F64Mul),
            CsilInstr::F64Div => self.f.instruction(&I::F64Div),
            _ => unreachable!("exact-guarded carry opcode"),
        };
        self.f.instruction(&I::LocalSet(l.tf2));

        // A division needs no check. An uneven one is this same float
        // division in the interpreter. An even one of two whole operands is
        // exact: the quotient is an integer no larger in magnitude than the
        // dividend and with no more significant bits, so f64 holds it, and
        // the correctly rounded float quotient is that integer.
        if matches!(instr, CsilInstr::F64Div) {
            self.f.instruction(&I::LocalGet(l.tf2));
            self.canonicalize_carried_f64();
            self.pushed(VTy::F64);
            return;
        }

        self.emit_is_whole(l.tf0);
        self.emit_is_whole(l.tf1);
        self.f.instruction(&I::I32And);
        self.f.instruction(&I::If(BlockType::Empty));
        match instr {
            CsilInstr::F64Add | CsilInstr::F64Sub => {
                // TwoSum: the rounding error of tf0 + tf1, zero when exact.
                // err = (a - (r - (r - a))) + (b - (r - a))
                self.f
                    .instruction(&I::LocalGet(l.tf0))
                    .instruction(&I::LocalGet(l.tf2))
                    .instruction(&I::LocalGet(l.tf2))
                    .instruction(&I::LocalGet(l.tf0))
                    .instruction(&I::F64Sub)
                    .instruction(&I::F64Sub)
                    .instruction(&I::F64Sub)
                    .instruction(&I::LocalGet(l.tf1))
                    .instruction(&I::LocalGet(l.tf2))
                    .instruction(&I::LocalGet(l.tf0))
                    .instruction(&I::F64Sub)
                    .instruction(&I::F64Sub)
                    .instruction(&I::F64Add)
                    .instruction(&I::F64Const(0.0.into()))
                    .instruction(&I::F64Ne);
                // An int sum past i64 is na in the interpreter.
                self.emit_fits_i64(l.tf2);
                self.f
                    .instruction(&I::I32Eqz)
                    .instruction(&I::I32Or)
                    .instruction(&I::If(BlockType::Empty));
                self.fraction_stop(pc);
                self.f.instruction(&I::End);
            }
            CsilInstr::F64Mul => {
                // Below 2^53 a product of whole numbers is exact.
                self.f
                    .instruction(&I::LocalGet(l.tf2))
                    .instruction(&I::F64Abs)
                    .instruction(&I::F64Const(9_007_199_254_740_992.0.into()))
                    .instruction(&I::F64Ge)
                    .instruction(&I::If(BlockType::Empty));
                self.emit_fits_i64(l.tf0);
                self.emit_fits_i64(l.tf1);
                self.f
                    .instruction(&I::I32And)
                    .instruction(&I::If(BlockType::Empty))
                    // Multiply in i64 and require no overflow (the product
                    // divides back) and an exact f64 image. The divisor is
                    // not zero (the product is not) and the dividend is not
                    // i64::MIN over -1 (both operands are above i64::MIN).
                    .instruction(&I::LocalGet(l.tf0))
                    .instruction(&I::I64TruncSatF64S)
                    .instruction(&I::LocalSet(l.ti0))
                    .instruction(&I::LocalGet(l.tf1))
                    .instruction(&I::I64TruncSatF64S)
                    .instruction(&I::LocalSet(l.ti1))
                    .instruction(&I::LocalGet(l.ti0))
                    .instruction(&I::LocalGet(l.ti1))
                    .instruction(&I::I64Mul)
                    .instruction(&I::LocalTee(l.ti2))
                    .instruction(&I::LocalGet(l.ti1))
                    .instruction(&I::I64DivS)
                    .instruction(&I::LocalGet(l.ti0))
                    .instruction(&I::I64Ne);
                self.emit_i64_not_exact_in_f64(l.ti2);
                self.f.instruction(&I::I32Or);
                // A product of exactly -2^63 is `i64::MIN`, which the
                // interpreter's int negation, `/ -1` and `% -1` turn into
                // `na`; the float operations here would not.
                self.emit_fits_i64(l.tf2);
                self.f
                    .instruction(&I::I32Eqz)
                    .instruction(&I::I32Or)
                    .instruction(&I::If(BlockType::Empty));
                self.fraction_stop(pc);
                self.f.instruction(&I::End).instruction(&I::Else);
                self.fraction_stop(pc);
                self.f.instruction(&I::End).instruction(&I::End);
            }
            _ => unreachable!("exact-guarded carry opcode"),
        }
        self.f.instruction(&I::End);
        self.f.instruction(&I::LocalGet(l.tf2));
        self.canonicalize_carried_f64();
        self.pushed(VTy::F64);
    }

    /// The f64 on the stack is the result of a fraction-carry operation that
    /// replaced an int one: make every non-finite result the canonical NaN.
    /// The int operation's `na` was one bit pattern. A float operation's NaN
    /// is not: `a + (-b)` hands back `-b`'s flipped sign bit when `b` is
    /// `na`, a negation flips it, and an invalid operation's default NaN has
    /// whatever sign the engine gives it. [`Self::canonicalize_f64`] maps
    /// only the infinities and lets a NaN through as it is.
    fn canonicalize_carried_f64(&mut self) {
        let l = self.ctx.locals;
        // `x - x` is zero for a finite `x` and NaN otherwise.
        self.f
            .instruction(&I::LocalSet(l.tf2))
            .instruction(&I::F64Const(f64::NAN.into()))
            .instruction(&I::LocalGet(l.tf2))
            .instruction(&I::LocalGet(l.tf2))
            .instruction(&I::LocalGet(l.tf2))
            .instruction(&I::F64Sub)
            .instruction(&I::F64Const(0.0.into()))
            .instruction(&I::F64Ne)
            .instruction(&I::Select);
    }

    /// The i64 on the stack, whose presence tag is the local `valid`, is
    /// about to be widened into the fraction carry: stop the run unless it is
    /// `na` or f64 holds it exactly. Leaves the value in place.
    fn emit_exact_widen_guard(&mut self, valid: u32, pc: usize) {
        let l = self.ctx.locals;
        self.f
            .instruction(&I::LocalSet(l.ti0))
            .instruction(&I::LocalGet(valid))
            .instruction(&I::I32Eqz)
            .instruction(&I::I32Eqz);
        self.emit_i64_not_exact_in_f64(l.ti0);
        self.f
            .instruction(&I::I32And)
            .instruction(&I::If(BlockType::Empty));
        self.fraction_stop(pc);
        self.f.instruction(&I::End).instruction(&I::LocalGet(l.ti0));
    }

    /// The runtime guard analysis assigned to `pc`, if any.
    fn int_min_guard(&self, pc: usize) -> Option<IntMinGuard> {
        self.ctx.analysis.int_min_guards.get(&pc).copied()
    }

    /// i32 condition on the wasm stack: nonzero means the operation at `pc`
    /// produced a present `i64::MIN`, which the `I64` lane would read back as
    /// `na`. Stop with `{ABI_MISMATCH, bar, pc}` so the request reruns on the
    /// interpreter instead of continuing with a missing value.
    fn emit_int_min_exit(&mut self, pc: usize) {
        self.guard_emitted = true;
        self.f.instruction(&I::If(BlockType::Empty));
        self.err_exit(status::ABI_MISMATCH, pc as u32);
        self.f.instruction(&I::End);
    }

    /// Guard a float-to-integer conversion [`Self::emit_f64_to_int`] just
    /// emitted: its operand is still in `tf0`, and `-2^63` is the only float
    /// in range that truncates to the sentinel.
    fn emit_float_to_int_min_guard(&mut self, pc: usize) {
        if self.int_min_guard(pc) != Some(IntMinGuard::FloatToInt) {
            return;
        }
        let l = self.ctx.locals;
        self.f
            .instruction(&I::LocalGet(l.tf0))
            .instruction(&I::F64Const((i64::MIN as f64).into()))
            .instruction(&I::F64Eq);
        self.emit_int_min_exit(pc);
    }

    /// Guard a time-lane f64 on the wasm stack before it is truncated: an
    /// i64 timestamp survives the f64 lane exactly only below `2^53` in
    /// magnitude, which also excludes the sentinel.
    fn emit_time_lane_guard(&mut self, pc: usize) {
        if self.int_min_guard(pc) != Some(IntMinGuard::TimeLane) {
            return;
        }
        let l = self.ctx.locals;
        self.f
            .instruction(&I::LocalTee(l.tf0))
            .instruction(&I::F64Abs)
            .instruction(&I::F64Const(9_007_199_254_740_992.0.into()))
            .instruction(&I::F64Lt)
            .instruction(&I::I32Eqz);
        self.emit_int_min_exit(pc);
        self.f.instruction(&I::LocalGet(l.tf0));
    }

    // ---- opcode lowering ----------------------------------------------------

    /// Lower the instruction at `pc`. A guard the analysis assigned must be
    /// emitted by the lowering that runs: analysis admitted the program on
    /// the strength of it, so a lowering that drops one is refused here
    /// instead of producing a module that could read a minimum as `na`. A
    /// chart-only site the projection erased computes nothing and needs none.
    fn emit_instr(&mut self, pc: usize) -> Result<(), AotError> {
        self.guard_emitted = false;
        self.emit_instr_unchecked(pc)?;
        let erased = self
            .ctx
            .projection
            .is_some_and(|projection| projection.chart_only.contains(&pc));
        if self.int_min_guard(pc).is_some() && !erased && !self.guard_emitted {
            return Err(AotError::InvalidProgram(format!(
                "integer minimum guard at pc {pc} was not emitted"
            )));
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn emit_instr_unchecked(&mut self, pc: usize) -> Result<(), AotError> {
        use CsilInstr as Csil;
        let code = &self.ctx.program.main().code;
        let l = self.ctx.locals;
        let instr = &code[pc];

        if self
            .ctx
            .projection
            .is_some_and(|projection| projection.chart_only.contains(&pc))
        {
            return self.emit_projected_chart_site(pc);
        }
        if self
            .ctx
            .projection
            .is_some_and(|projection| projection.time_open_site.get(&pc) == Some(&0))
        {
            // `time` of the chart's own period: the bar's open time, as
            // PineWASM's interpreter answers it, with no lane and no charge.
            for _ in 0..5 {
                self.discard_top();
            }
            self.f
                .instruction(&I::GlobalGet(self.ctx.globals.builtin_global(5)))
                .instruction(&I::LocalGet(l.bar))
                .instruction(&I::I32Const(3))
                .instruction(&I::I32Shl)
                .instruction(&I::I32Add)
                .instruction(&I::F64Load(mem_f64()));
            self.emit_time_lane_guard(pc);
            self.f.instruction(&I::I64TruncSatF64S);
            self.pushed(VTy::I64);
            return Ok(());
        }
        if let Some((site, status_base, time_open)) = self.ctx.projection.and_then(|projection| {
            projection.security_site.get(&pc).map(|site| {
                (
                    *site,
                    projection.security_status_base,
                    projection.time_open_site.get(&pc).copied(),
                )
            })
        }) {
            // A same-symbol `request.security` read, or a `time(timeframe)`
            // call: the runtime resampled the bars into this lane before the
            // run and left its outcome in a scratch cell. As in PineWASM's
            // interpreter, the request is settled when it first executes: a negative
            // cell is the status to stop with, a positive one the resample's
            // one-time budget charge.
            if let Some(seconds) = time_open {
                // The timeframe literal, session, timezone and both
                // `bars_back` operands: the lane already answers for them.
                for _ in 0..5 {
                    self.discard_top();
                }
                // As the interpreter's `time` does, before any bucket: a
                // timeframe shorter than the chart's reads `na`, and the
                // chart's own reads the bar's open time, with no charge.
                self.timeframe_seconds_cell();
                self.f
                    .instruction(&I::LocalSet(l.ti0))
                    .instruction(&I::I64Const(seconds))
                    .instruction(&I::LocalGet(l.ti0))
                    .instruction(&I::I64LtS)
                    .instruction(&I::If(BlockType::Result(ValType::I64)))
                    .instruction(&I::I64Const(NA_I64))
                    .instruction(&I::Else)
                    .instruction(&I::I64Const(seconds))
                    .instruction(&I::LocalGet(l.ti0))
                    .instruction(&I::I64Eq)
                    .instruction(&I::If(BlockType::Result(ValType::I64)))
                    .instruction(&I::GlobalGet(self.ctx.globals.builtin_global(5)))
                    .instruction(&I::LocalGet(l.bar))
                    .instruction(&I::I32Const(3))
                    .instruction(&I::I32Shl)
                    .instruction(&I::I32Add)
                    .instruction(&I::F64Load(mem_f64()));
                self.emit_time_lane_guard(pc);
                self.f
                    .instruction(&I::I64TruncSatF64S)
                    .instruction(&I::Else);
            }
            let lane = self
                .ctx
                .globals
                .security_lane_global(site.lane)
                .ok_or_else(|| {
                    AotError::InvalidProgram(format!("no request.security lane {}", site.lane))
                })?;
            let status_cell = MemArg {
                offset: u64::from(status_base + site.spec) * 8,
                align: 3,
                memory_index: 0,
            };
            let scratch_base = self.ctx.globals.scratch_base;
            self.f
                .instruction(&I::GlobalGet(scratch_base))
                .instruction(&I::I64Load(status_cell))
                .instruction(&I::LocalTee(l.ti3))
                .instruction(&I::I64Const(0))
                .instruction(&I::I64Ne)
                .instruction(&I::If(BlockType::Empty));
            for code in [
                status::SECURITY_TIMEFRAME_UNAVAILABLE,
                status::SECURITY_LENGTH_OUT_OF_RANGE,
                status::SECURITY_SESSION_UNSETTLED,
                status::SECURITY_TIMEZONE_UNSUPPORTED,
            ] {
                self.f
                    .instruction(&I::LocalGet(l.ti3))
                    .instruction(&I::I64Const(-i64::from(code)))
                    .instruction(&I::I64Eq)
                    .instruction(&I::If(BlockType::Empty));
                self.err_exit(code, pc as u32);
                self.f.instruction(&I::End);
            }
            self.emit_budget_charge_local(pc as u32);
            self.f
                .instruction(&I::GlobalGet(scratch_base))
                .instruction(&I::I64Const(0))
                .instruction(&I::I64Store(status_cell))
                .instruction(&I::End);
            self.f
                .instruction(&I::GlobalGet(lane))
                .instruction(&I::LocalGet(l.bar))
                .instruction(&I::I32Const(3))
                .instruction(&I::I32Shl)
                .instruction(&I::I32Add)
                .instruction(&I::F64Load(mem_f64()));
            if time_open.is_some() {
                // An opening time is a bar's open time: exact below 2^53.
                self.emit_time_lane_guard(pc);
                self.f
                    .instruction(&I::I64TruncSatF64S)
                    .instruction(&I::End)
                    .instruction(&I::End);
                self.pushed(VTy::I64);
            } else {
                self.pushed(VTy::F64);
            }
            return Ok(());
        }

        if let Some(
            site @ (ChartSite::PeriodSeconds
            | ChartSite::Calendar { .. }
            | ChartSite::Timestamp { .. }),
        ) = self.chart_site(pc)
        {
            return self.emit_chart_call(pc, site);
        }

        match instr {
            // Native heap (docs/aot.md §13). These opcodes reach
            // emit_instr only when the analysis carries a HeapPlan; without
            // one they are host sites and never get here.
            Csil::CallCollection { .. }
            | Csil::NewObject { .. }
            | Csil::CopyObject { .. }
            | Csil::GetField { .. }
            | Csil::SetField { .. } => {
                self.emit_native_heap(pc)?;
            }
            Csil::PushNa => {
                let ty = self
                    .ctx
                    .analysis
                    .result_tys
                    .get(pc)
                    .and_then(|r| r.first().copied())
                    .unwrap_or(VTy::F64);
                self.na_const(ty);
                self.pushed(ty);
            }
            Csil::PushBool(b) => {
                self.f.instruction(&I::I32Const(if *b { 2 } else { 0 }));
                self.pushed(VTy::Bool3);
            }
            Csil::PushI64(v) => {
                if self.int_min_guard(pc) == Some(IntMinGuard::Literal) {
                    // The literal is the sentinel's bit pattern and is never
                    // `na`: reaching it is the mismatch.
                    self.f.instruction(&I::I32Const(1));
                    self.emit_int_min_exit(pc);
                }
                let valid = self.integer_valid_local(self.vstack.len());
                self.f
                    .instruction(&I::I64Const(*v))
                    .instruction(&I::I32Const(1))
                    .instruction(&I::LocalSet(valid));
                self.vstack.push((VTy::I64, Loc::Phys));
            }
            Csil::PushF64(v) => {
                let v = v.get();
                let v = if v.is_finite() { v } else { f64::NAN };
                self.f.instruction(&I::F64Const(v.into()));
                self.pushed(VTy::F64);
            }
            Csil::PushColor { r, g, b, t } => {
                self.f
                    .instruction(&I::I32Const(pack_color(*r, *g, *b, t.get())));
                self.pushed(VTy::Color);
            }
            Csil::PushStr(id) => {
                // The interpreter reserves the constant's bytes each time it
                // pushes it (`Vm::exec_instr`). An empty one reserves none.
                let bytes = self
                    .ctx
                    .program
                    .constants
                    .get(*id as usize)
                    .map_or(0, String::len);
                if let (Some(strings), true) = (self.strings(), bytes > 0) {
                    self.emit_string_reserve(strings, StringBytes::Known(bytes), pc as u32)?;
                }
                self.f.instruction(&I::I32Const(*id as i32));
                self.pushed(VTy::Str);
            }
            Csil::AlertCondition => {
                // Pops message (top), title, condition; appends to the alert
                // output lane only when the condition is exactly true. The
                // count lives in the header so a run that aborts mid-bar still
                // reports the alerts it committed.
                self.want(1);
                self.consumed();
                self.f.instruction(&I::LocalSet(l.t32a)); // message handle
                self.want(1);
                self.consumed();
                self.f.instruction(&I::Drop); // title (metadata only)
                self.want(1);
                self.consumed();
                self.f
                    .instruction(&I::I32Const(2))
                    .instruction(&I::I32Eq)
                    .instruction(&I::If(BlockType::Empty));
                // The interpreter reserves an alert's message and the name
                // of its frequency before it records one
                // (`Vm::exec_alert_condition`).
                if let Some(strings) = self.strings() {
                    let bytes = match self.pushed_string_len(pc) {
                        Some(message) => StringBytes::Known(message + ALERT_CONDITION_FREQ.len()),
                        None => {
                            self.f.instruction(&I::LocalGet(l.t32a));
                            self.emit_string_len(strings, pc as u32);
                            self.f
                                .instruction(&I::I64Const(ALERT_CONDITION_FREQ.len() as i64))
                                .instruction(&I::I64Add)
                                .instruction(&I::LocalSet(l.ti2));
                            StringBytes::InTi2
                        }
                    };
                    self.emit_string_reserve(strings, bytes, pc as u32)?;
                }
                // entry = alerts_base + count * 8
                self.f
                    .instruction(&I::I32Const(0))
                    .instruction(&I::I32Load(mem_u32_at(header::OFF_ALERT_COUNT)))
                    .instruction(&I::LocalTee(l.t32b))
                    .instruction(&I::I32Const(3))
                    .instruction(&I::I32Shl)
                    .instruction(&I::I32Const(0))
                    .instruction(&I::I32Load(mem_u32_at(header::OFF_ALERTS_BASE)))
                    .instruction(&I::I32Add)
                    .instruction(&I::LocalTee(l.t32c))
                    .instruction(&I::LocalGet(l.bar))
                    .instruction(&I::I32Store(mem_i32()))
                    .instruction(&I::LocalGet(l.t32c))
                    .instruction(&I::LocalGet(l.t32a))
                    .instruction(&I::I32Store(mem_u32_at(4)));
                self.f
                    .instruction(&I::I32Const(0))
                    .instruction(&I::LocalGet(l.t32b))
                    .instruction(&I::I32Const(1))
                    .instruction(&I::I32Add)
                    .instruction(&I::I32Store(mem_u32_at(header::OFF_ALERT_COUNT)))
                    .instruction(&I::End);
            }
            Csil::Dup => {
                self.want(1);
                let ty = self.vstack.last().expect("dup operand").0;
                let local = self.stack_local(self.vstack.len() - 1, ty);
                self.f
                    .instruction(&I::LocalTee(local))
                    .instruction(&I::LocalGet(local));
                if ty == VTy::I64 {
                    self.pushed_integer_with_valid(self.integer_valid_local(self.vstack.len() - 1));
                } else {
                    self.pushed(ty);
                }
            }
            Csil::Pop => {
                if self.vstack.last().map(|c| c.1) == Some(Loc::Phys) {
                    self.f.instruction(&I::Drop);
                    self.consumed();
                } else {
                    self.vstack.pop();
                }
            }

            Csil::ReadBuiltin { source, offset } => {
                let ty = builtin_vty(*source)
                    .or_else(|| strategy_snap_field(*source).map(|(_, ty)| ty))
                    .or_else(|| self.chart_builtin_vty(pc, *source))
                    .ok_or_else(|| self.unsup(instr))?;
                if *offset == 0 {
                    self.f.instruction(&I::LocalGet(l.bar));
                    self.f.instruction(&I::LocalSet(l.t32a));
                    self.emit_builtin_read_at(pc, *source)?;
                } else {
                    // bar < offset → na
                    self.f
                        .instruction(&I::LocalGet(l.bar))
                        .instruction(&I::I32Const(*offset as i32))
                        .instruction(&I::I32LtU)
                        .instruction(&I::If(BlockType::Result(val_type(ty))));
                    self.na_const(ty);
                    self.f.instruction(&I::Else);
                    self.f
                        .instruction(&I::LocalGet(l.bar))
                        .instruction(&I::I32Const(*offset as i32))
                        .instruction(&I::I32Sub)
                        .instruction(&I::LocalSet(l.t32a));
                    self.emit_builtin_read_at(pc, *source)?;
                    self.f.instruction(&I::End);
                }
                self.pushed(ty);
            }
            Csil::ReadBuiltinDynamic { source } => {
                let ty = builtin_vty(*source)
                    .or_else(|| strategy_snap_field(*source).map(|(_, ty)| ty))
                    .or_else(|| self.chart_builtin_vty(pc, *source))
                    .ok_or_else(|| self.unsup(instr))?;
                self.emit_dynamic_offset_guard(pc, ty, |this| {
                    // index = bar - off (off in ti0, already ≤ bar)
                    this.f
                        .instruction(&I::LocalGet(l.bar))
                        .instruction(&I::LocalGet(l.ti0))
                        .instruction(&I::I32WrapI64)
                        .instruction(&I::I32Sub)
                        .instruction(&I::LocalSet(l.t32a));
                    this.emit_builtin_read_at(pc, *source)
                })?;
                self.pushed(ty);
            }
            Csil::ReadUserSlot { slot, offset } => {
                let ty = self.ctx.analysis.slot_ty[*slot as usize];
                if *offset == 0 {
                    self.f.instruction(&l.cur_get(*slot));
                } else if let Some(binding) =
                    self.ctx.program.ta_input_bindings.iter().find(|binding| {
                        binding.target == pinewasm_hir::TaInputTarget::Change { slot: *slot }
                    })
                {
                    crate::emit::emit_ta_input_value(
                        self.f,
                        binding,
                        crate::emit::ta_input_rule(self.ctx.program, binding),
                    );
                    self.f
                        .instruction(&I::LocalSet(l.ti0))
                        .instruction(&I::LocalGet(l.bar))
                        .instruction(&I::LocalGet(l.ti0))
                        .instruction(&I::I32WrapI64)
                        .instruction(&I::I32LtU)
                        .instruction(&I::If(BlockType::Result(val_type(ty))));
                    self.na_const(ty);
                    self.f.instruction(&I::Else);
                    self.push_slot_addr(*slot, SlotIndex::DynamicTi0);
                    self.emit_typed_load(ty);
                    self.f.instruction(&I::End);
                } else {
                    self.f
                        .instruction(&I::LocalGet(l.bar))
                        .instruction(&I::I32Const(*offset as i32))
                        .instruction(&I::I32LtU)
                        .instruction(&I::If(BlockType::Result(val_type(ty))));
                    self.na_const(ty);
                    self.f.instruction(&I::Else);
                    self.push_slot_addr(*slot, SlotIndex::Literal(*offset));
                    self.emit_typed_load(ty);
                    self.f.instruction(&I::End);
                }
                self.pushed(ty);
            }
            Csil::ReadUserSlotDynamic { slot } => {
                let ty = self.ctx.analysis.slot_ty[*slot as usize];
                self.emit_dynamic_offset_guard(pc, ty, |this| {
                    this.push_slot_addr(*slot, SlotIndex::DynamicTi0);
                    this.emit_typed_load(ty);
                    Ok(())
                })?;
                self.pushed(ty);
            }
            Csil::StoreUserSlotCurrent { slot } => {
                let ty = self.ctx.analysis.slot_ty[*slot as usize];
                self.want(1);
                let valid = self.integer_valid_local(self.vstack.len() - 1);
                // An integer stored to a slot that keeps Pine integer
                // quotients in the f64 lane is converted here, exactly as the
                // `IntToF64` after each read of the slot would convert it.
                if self.consumed() == VTy::I64 && ty == VTy::F64 {
                    self.emit_i64_to_f64_tagged(valid);
                }
                self.f.instruction(&l.cur_set(*slot));
                if self.ctx.slots.retains_history(*slot) {
                    self.push_slot_addr(*slot, SlotIndex::Literal(0));
                    self.f.instruction(&l.cur_get(*slot));
                    self.emit_typed_store(ty);
                }
                // standalone WebAssembly omits this store only when no initialize-once branch
                // observes it. Universal modules preserve their public lanes.
                if self.ctx.slots.observes_written(*slot) {
                    self.f
                        .instruction(&I::GlobalGet(self.ctx.globals.written_base))
                        .instruction(&I::I32Const(*slot as i32))
                        .instruction(&I::I32Add)
                        .instruction(&I::I32Const(1))
                        .instruction(&I::I32Store8(mem_u8()));
                }
            }
            Csil::ReadInput { input } => {
                self.emit_read_input(*input)?;
                if self.int_min_guard(pc) == Some(IntMinGuard::InputRead) {
                    // An input is never `na`, so the tag `pushed` derived
                    // from the sentinel comparison is clear only for a
                    // present minimum.
                    let valid = self.integer_valid_local(self.vstack.len() - 1);
                    self.f
                        .instruction(&I::LocalGet(valid))
                        .instruction(&I::I32Eqz);
                    self.emit_int_min_exit(pc);
                }
            }
            Csil::WindowMean { slot, length } => {
                self.emit_window_mean(*slot, *length);
            }
            Csil::WindowMeanDynamic { .. } => return Err(self.unsup(instr)),
            Csil::WindowExtremumDynamic { .. } => return Err(self.unsup(instr)),
            Csil::WindowStatisticDynamic { .. } => return Err(self.unsup(instr)),
            Csil::EmaSimpleDynamic { .. } => return Err(self.unsup(instr)),
            Csil::CallTa { state } => {
                self.emit_call_ta(pc, *state)?;
            }
            Csil::CallPure { func, argc } => {
                self.emit_call_pure(pc, *func, *argc)?;
            }

            Csil::IntAdd | Csil::IntSub | Csil::IntMul | Csil::IntDiv | Csil::IntRem => {
                self.emit_int_binop(pc, instr);
            }
            // The analysis fixed the lane: i64 only for the quotient that a
            // one-argument `math.round`, `math.floor`, `math.ceil` or `int()`
            // reads next, f64 for every other admitted shape.
            Csil::PineIntDiv if self.ctx.analysis.result_tys[pc] == [VTy::I64] => {
                let Some(rounding) =
                    crate::analysis::quotient_rounding(&self.ctx.program.main().code, pc)
                else {
                    return Err(self.unsup(instr));
                };
                self.emit_rounded_pine_int_div(rounding);
            }
            Csil::PineIntDiv => {
                let fused = crate::analysis::quotient_compare(
                    &self.ctx.program.main().code,
                    &self.ctx.analysis.blocks,
                    pc,
                );
                match fused {
                    Some((compare, operand))
                        if self.quotient_compare_is_exact(compare, operand) =>
                    {
                        self.emit_compared_pine_int_div();
                    }
                    _ => self.emit_widened_pine_int_div(),
                }
            }
            Csil::IntNeg => {
                use crate::tagged_integer::{emit_checked_negate, UnaryLocals};
                self.want(1);
                let input_valid = self.integer_valid_local(self.vstack.len() - 1);
                self.consumed();
                self.f.instruction(&I::LocalSet(l.ti0));
                emit_checked_negate(
                    self.f,
                    UnaryLocals {
                        input: l.ti0,
                        input_valid,
                        value: l.ti1,
                        valid: l.t32c,
                    },
                );
                self.f
                    .instruction(&I::LocalSet(l.t32c))
                    .instruction(&I::I64Const(NA_I64))
                    .instruction(&I::LocalGet(l.t32c))
                    .instruction(&I::Select);
                self.pushed_integer_with_valid(l.t32c);
            }
            Csil::F64Add | Csil::F64Sub | Csil::F64Mul | Csil::F64Div if self.exact_guarded(pc) => {
                self.emit_exact_carried_binop(instr, pc);
            }
            Csil::F64Add | Csil::F64Sub | Csil::F64Mul | Csil::F64Div => {
                self.want(2);
                self.consumed();
                self.consumed();
                match instr {
                    Csil::F64Add => self.f.instruction(&I::F64Add),
                    Csil::F64Sub => self.f.instruction(&I::F64Sub),
                    Csil::F64Mul => self.f.instruction(&I::F64Mul),
                    Csil::F64Div => self.f.instruction(&I::F64Div),
                    _ => unreachable!(),
                };
                self.canonicalize_f64();
                self.pushed(VTy::F64);
            }
            Csil::F64Rem => {
                // WebAssembly has no `f64.rem`, and the usual
                // `a - trunc(a / b) * b` expansion loses low bits for large
                // quotients, so this calls the linked runtime's `fmod`.
                let rem = self.ctx.hosts.pure_fn(pinewasm_num::id::REM)?;
                self.want(2);
                self.consumed();
                self.f.instruction(&I::LocalSet(l.tf1));
                self.consumed();
                self.f
                    .instruction(&I::LocalSet(l.tf0))
                    .instruction(&I::LocalGet(l.tf0))
                    .instruction(&I::LocalGet(l.tf1))
                    .instruction(&I::Call(rem));
                if self.exact_guarded(pc) {
                    // A fraction-carry `%`: its `na` is the canonical NaN.
                    self.canonicalize_carried_f64();
                }
                self.pushed(VTy::F64);
            }
            // A fraction-carry negation (see `fraction.rs`): `f64.neg` flips
            // the sign bit of a NaN, and `na` is the canonical NaN only.
            Csil::F64Neg if self.exact_guarded(pc) => {
                self.want(1);
                self.consumed();
                self.f.instruction(&I::F64Neg);
                self.canonicalize_carried_f64();
                self.pushed(VTy::F64);
            }
            Csil::F64Neg => {
                self.want(1);
                self.consumed();
                self.f.instruction(&I::F64Neg);
                self.pushed(VTy::F64);
            }
            Csil::IntToF64 => {
                self.want(1);
                let valid = self.integer_valid_local(self.vstack.len() - 1);
                let ty = self.consumed();
                if ty == VTy::I64 {
                    if self.exact_guarded(pc) {
                        self.emit_exact_widen_guard(valid, pc);
                    }
                    self.emit_i64_to_f64_tagged(valid);
                }
                self.pushed(VTy::F64);
            }

            Csil::Select => {
                // [cond, lhs, rhs] with rhs on top; eager both-arms select.
                let ty = self.vstack[self.vstack.len() - 1].0;
                self.flush_all();
                let len = self.vstack.len();
                let (rhs_l, lhs_l, cond_l) = (
                    self.stack_local(len - 1, ty),
                    self.stack_local(len - 2, ty),
                    self.stack_local(len - 3, VTy::Bool3),
                );
                self.vstack.truncate(len - 3);
                self.f
                    .instruction(&I::LocalGet(lhs_l))
                    .instruction(&I::LocalGet(rhs_l))
                    .instruction(&I::LocalGet(cond_l))
                    .instruction(&I::I32Const(2))
                    .instruction(&I::I32Eq)
                    .instruction(&I::Select);
                if ty == VTy::I64 {
                    let lhs_valid = self.integer_valid_local(len - 2);
                    let rhs_valid = self.integer_valid_local(len - 1);
                    let output_valid = self.integer_valid_local(len - 3);
                    self.f
                        .instruction(&I::LocalGet(lhs_valid))
                        .instruction(&I::LocalGet(rhs_valid))
                        .instruction(&I::LocalGet(cond_l))
                        .instruction(&I::I32Const(2))
                        .instruction(&I::I32Eq)
                        .instruction(&I::Select)
                        .instruction(&I::LocalSet(output_valid));
                    self.vstack.push((ty, Loc::Phys));
                } else {
                    self.pushed(ty);
                }
            }

            Csil::CmpGt | Csil::CmpGe | Csil::CmpLt | Csil::CmpLe | Csil::CmpEq | Csil::CmpNe => {
                self.emit_cmp(pc, instr)?;
            }

            Csil::BoolAnd | Csil::BoolOr => {
                // Kleene over 0/1/2: and = min, or = max.
                self.want(2);
                self.consumed();
                self.consumed();
                self.f
                    .instruction(&I::LocalSet(l.t32b))
                    .instruction(&I::LocalSet(l.t32a))
                    .instruction(&I::LocalGet(l.t32a))
                    .instruction(&I::LocalGet(l.t32b))
                    .instruction(&I::LocalGet(l.t32a))
                    .instruction(&I::LocalGet(l.t32b));
                if matches!(instr, Csil::BoolAnd) {
                    self.f.instruction(&I::I32LtS);
                } else {
                    self.f.instruction(&I::I32GtS);
                }
                self.f.instruction(&I::Select);
                self.pushed(VTy::Bool3);
            }
            Csil::BoolNot => {
                self.want(1);
                self.consumed();
                self.f
                    .instruction(&I::LocalSet(l.t32a))
                    .instruction(&I::I32Const(2))
                    .instruction(&I::LocalGet(l.t32a))
                    .instruction(&I::I32Sub);
                self.pushed(VTy::Bool3);
            }

            Csil::PlotF64 { plot_slot } => {
                self.want(1);
                self.consumed();
                self.f
                    .instruction(&I::LocalSet(l.tf0))
                    .instruction(&I::GlobalGet(self.ctx.globals.plot_global(*plot_slot)))
                    .instruction(&I::LocalGet(l.bar))
                    .instruction(&I::I32Const(3))
                    .instruction(&I::I32Shl)
                    .instruction(&I::I32Add)
                    .instruction(&I::LocalGet(l.tf0))
                    .instruction(&I::F64Store(mem_f64()));
            }
            Csil::PlotStyledF64 {
                plot_slot,
                color_count,
            } => {
                // Colors pop last-declared-first, then the float.
                for role in (0..*color_count).rev() {
                    let lane = self
                        .ctx
                        .globals
                        .plot_color_global(*plot_slot, u32::from(role));
                    self.emit_color_store(lane);
                }
                self.want(1);
                self.consumed();
                self.f
                    .instruction(&I::LocalSet(l.tf0))
                    .instruction(&I::GlobalGet(self.ctx.globals.plot_global(*plot_slot)))
                    .instruction(&I::LocalGet(l.bar))
                    .instruction(&I::I32Const(3))
                    .instruction(&I::I32Shl)
                    .instruction(&I::I32Add)
                    .instruction(&I::LocalGet(l.tf0))
                    .instruction(&I::F64Store(mem_f64()));
            }
            Csil::VisualColor {
                visual_slot,
                color_count,
            } => {
                for role in (0..*color_count).rev() {
                    let lane = self
                        .ctx
                        .globals
                        .visual_color_global(*visual_slot, u32::from(role));
                    self.emit_color_store(lane);
                }
            }
            Csil::FillColor {
                fill_slot,
                color_count,
            } => {
                // Same shape as a visual: the band's geometry comes from the
                // two plots it spans, so the only per-bar output is colors.
                for role in (0..*color_count).rev() {
                    let lane = self
                        .ctx
                        .globals
                        .fill_color_global(*fill_slot, u32::from(role));
                    self.emit_color_store(lane);
                }
            }

            Csil::Order { kind } => {
                self.emit_order(*kind, pc)?;
            }

            other => return Err(self.unsup(other)),
        }
        Ok(())
    }

    /// Erase a standalone WebAssembly-proven drawing/table-only value or effect while preserving
    /// its validated stack contract.  `RegisterDrawing` is value-identical;
    /// constructors/formatters return typed na because every use was proven
    /// to terminate in another erased chart sink.
    fn emit_projected_chart_site(&mut self, pc: usize) -> Result<(), AotError> {
        let instr = &self.ctx.program.main().code[pc];
        if matches!(instr, CsilInstr::RegisterDrawing { .. }) {
            return Ok(());
        }

        // An erased drawing constructor still charges `exec_new_object`'s
        // fuel extra (constructing N fields costs N, dispatch already
        // charged 1), so fuel runs out at the same pc as in the VM. That
        // parity is part of `NewObject`'s no-failure certificate
        // (`projection::certify`).
        if let CsilInstr::NewObject { field_count, .. } = instr {
            if *field_count > 1 {
                self.emit_budget_charge(u64::from(*field_count) - 1, pc as u32);
            }
        }

        let argc = self.ctx.analysis.operand_tys[pc].len();
        self.flush_all();
        if self.vstack.len() < argc {
            return Err(AotError::InvalidProgram(format!(
                "projected chart site stack underflow at pc {pc}"
            )));
        }
        self.vstack.truncate(self.vstack.len() - argc);
        for &ty in &self.ctx.analysis.result_tys[pc] {
            self.na_const(ty);
            self.pushed(ty);
        }
        Ok(())
    }

    /// Lower one `CsilInstr::Order` onto its `pinewasm_rt_broker_*` placement entry
    /// Operands are staged into the scratch region's args cells
    /// (cell 4 up — above the budget cell and the ta tuple extras), one
    /// 8-byte cell per slot in the kind's fixed layout, encoded per the
    /// blob's decode table (`runtime/src/broker.rs`): f64 slots as f64, int
    /// slots as i64, string/bool slots as sign-extended i32. The call takes
    /// `(args, site)` — `site` is the CSIL pc, the risk directives'
    /// per-call-site rule identity. A nonzero status is a broker error whose
    /// message the blob already wrote; the run stops with `BROKER_FAILED`.
    fn emit_order(&mut self, kind: OrderKind, pc: usize) -> Result<(), AotError> {
        use crate::runtime::broker as broker_ids;
        let slots = order_slot_vtys(kind);
        let argc = slots.len();
        let target = self.ctx.hosts.broker_fn(match kind {
            OrderKind::Entry => broker_ids::ENTRY,
            OrderKind::Order => broker_ids::ORDER,
            OrderKind::Exit => broker_ids::EXIT,
            OrderKind::Close => broker_ids::CLOSE,
            OrderKind::CloseAll => broker_ids::CLOSE_ALL,
            OrderKind::Cancel => broker_ids::CANCEL,
            OrderKind::CancelAll => broker_ids::CANCEL_ALL,
            OrderKind::RiskMaxPositionSize => broker_ids::RISK_MAX_POSITION_SIZE,
            OrderKind::RiskAllowEntryIn => broker_ids::RISK_ALLOW_ENTRY_IN,
            OrderKind::RiskMaxDrawdown => broker_ids::RISK_MAX_DRAWDOWN,
            OrderKind::RiskMaxIntradayLoss => broker_ids::RISK_MAX_INTRADAY_LOSS,
            OrderKind::RiskMaxIntradayFilledOrders => broker_ids::RISK_MAX_INTRADAY_FILLED_ORDERS,
            OrderKind::RiskMaxConsLossDays => broker_ids::RISK_MAX_CONS_LOSS_DAYS,
        })?;

        // Same staging discipline as a host call: spill the operand run to
        // canonical locals, then store each into its args cell.
        self.flush_all();
        debug_assert!(self.vstack.len() >= argc, "order-site stack underflow");
        let base = self.vstack.len() - argc;
        let args_cell = |slot: usize| MemArg {
            // Cells 0..4 are reserved (budget I/O + ta tuple extras).
            offset: (8 * (4 + slot)) as u64,
            align: 3,
            memory_index: 0,
        };
        for (i, ty) in slots.iter().enumerate() {
            let arg_local = self.stack_local(base + i, *ty);
            self.f
                .instruction(&I::GlobalGet(self.ctx.globals.scratch_base))
                .instruction(&I::LocalGet(arg_local));
            match ty {
                VTy::F64 => {
                    self.f.instruction(&I::F64Store(args_cell(i)));
                }
                VTy::I64 => {
                    self.f.instruction(&I::I64Store(args_cell(i)));
                }
                VTy::Bool3 | VTy::Color | VTy::Str | VTy::Ref => {
                    self.f
                        .instruction(&I::I64ExtendI32S)
                        .instruction(&I::I64Store(args_cell(i)));
                }
            }
        }
        self.vstack.truncate(base);

        self.f
            .instruction(&I::GlobalGet(self.ctx.globals.scratch_base))
            .instruction(&I::I32Const(32))
            .instruction(&I::I32Add)
            .instruction(&I::I32Const(pc as i32))
            .instruction(&I::Call(target))
            .instruction(&I::If(BlockType::Empty))
            .instruction(&I::I32Const(status::BROKER_FAILED))
            .instruction(&I::Return)
            .instruction(&I::End);
        Ok(())
    }

    fn unsup(&self, instr: &CsilInstr) -> AotError {
        let dbg = format!("{instr:?}");
        AotError::UnsupportedOpcode(dbg.split([' ', '(', '{']).next().unwrap_or("?").to_string())
    }

    /// `±Inf → NaN` on the f64 stack top (`Value::from_f64` parity).
    fn canonicalize_f64(&mut self) {
        let l = self.ctx.locals;
        self.f
            .instruction(&I::LocalSet(l.tf2))
            .instruction(&I::F64Const(f64::NAN.into()))
            .instruction(&I::LocalGet(l.tf2))
            .instruction(&I::LocalGet(l.tf2))
            .instruction(&I::F64Abs)
            .instruction(&I::F64Const(f64::INFINITY.into()))
            .instruction(&I::F64Eq)
            .instruction(&I::Select);
    }

    /// i64 (sentinel na) on stack → f64 (NaN na).
    fn emit_i64_to_f64_tagged(&mut self, valid: u32) {
        let l = self.ctx.locals;
        self.f
            .instruction(&I::LocalSet(l.ti0))
            .instruction(&I::F64Const(f64::NAN.into()))
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::F64ConvertI64S)
            .instruction(&I::LocalGet(valid))
            .instruction(&I::I32Eqz)
            .instruction(&I::Select);
    }

    fn emit_i64_to_f64_na(&mut self) {
        let l = self.ctx.locals;
        self.f
            .instruction(&I::LocalSet(l.ti0))
            .instruction(&I::F64Const(f64::NAN.into()))
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::F64ConvertI64S)
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::I64Const(NA_I64))
            .instruction(&I::I64Eq)
            .instruction(&I::Select);
    }

    /// Before the `int(...)` a chart-tier carry put in front of a history
    /// offset (`Carried::offset_casts`), with the offset on the stack as an
    /// f64: hand the run back when the offset is one the interpreter reads
    /// and the cast's cell cannot hold. The interpreter takes a float offset
    /// below 2^64 as that many bars (`vm_f64_ops::history_offset`), and every
    /// one from 2^63 up is past `max_bars_back`, where it stops the run. The
    /// cast would make it `na`, which reads as `na` and lets the run go on.
    /// Any other value is left to the cast: the two agree on it. Uses `tf0`.
    ///
    /// Only a linked chart module carries the guard. Its stops are the
    /// interpreter's errors or hand-backs (docs/aot.md §14); a module that
    /// imports its host does not carry the guard.
    fn emit_carried_offset_guard(&mut self, pc: usize) {
        if self.ctx.hosts.linked.is_none()
            || !self
                .ctx
                .carry
                .is_some_and(|carry| carry.offset_casts.contains(&pc))
        {
            return;
        }
        let l = self.ctx.locals;
        self.f
            .instruction(&I::LocalTee(l.tf0))
            .instruction(&I::F64Const(9_223_372_036_854_775_808.0.into()))
            .instruction(&I::F64Ge)
            .instruction(&I::LocalGet(l.tf0))
            .instruction(&I::F64Const(18_446_744_073_709_551_616.0.into()))
            .instruction(&I::F64Lt)
            .instruction(&I::I32And)
            .instruction(&I::If(BlockType::Empty));
        self.err_exit(status::ABI_MISMATCH, pc as u32);
        self.f.instruction(&I::End).instruction(&I::LocalGet(l.tf0));
    }

    /// f64 on stack → i64 via `f64_to_int` semantics: in i64 range → trunc,
    /// else na (NaN fails the range check).
    fn emit_f64_to_int(&mut self) {
        let l = self.ctx.locals;
        self.f
            .instruction(&I::LocalSet(l.tf0))
            .instruction(&I::I64Const(NA_I64))
            .instruction(&I::LocalGet(l.tf0))
            .instruction(&I::I64TruncSatF64S)
            // in-range: f >= -2^63 && f < 2^63
            .instruction(&I::LocalGet(l.tf0))
            .instruction(&I::F64Const((i64::MIN as f64).into()))
            .instruction(&I::F64Ge)
            .instruction(&I::LocalGet(l.tf0))
            .instruction(&I::F64Const((i64::MAX as f64).into()))
            .instruction(&I::F64Lt)
            .instruction(&I::I32And)
            .instruction(&I::I32Eqz)
            .instruction(&I::Select);
    }

    /// A rounded/truncated float immediately widened from int: preserve valid
    /// MIN, reject values outside the signed range, and normalize integer zero.
    fn emit_widened_integer_result(&mut self) {
        let l = self.ctx.locals;
        self.f
            .instruction(&I::LocalSet(l.tf0))
            .instruction(&I::F64Const(f64::NAN.into()))
            .instruction(&I::F64Const(0.0.into()))
            .instruction(&I::LocalGet(l.tf0))
            .instruction(&I::LocalGet(l.tf0))
            .instruction(&I::F64Const(0.0.into()))
            .instruction(&I::F64Eq)
            .instruction(&I::Select)
            .instruction(&I::LocalGet(l.tf0))
            .instruction(&I::F64Const((i64::MIN as f64).into()))
            .instruction(&I::F64Ge)
            .instruction(&I::LocalGet(l.tf0))
            .instruction(&I::F64Const((i64::MAX as f64).into()))
            .instruction(&I::F64Lt)
            .instruction(&I::I32And)
            .instruction(&I::I32Eqz)
            .instruction(&I::Select);
    }

    /// Round-half-away-from-zero on the f64 stack top (`f64::round` parity):
    /// `t = trunc(x); t + (|x - t| >= 0.5 ? copysign(1, x) : 0)`.
    fn emit_round_half_away(&mut self) {
        let l = self.ctx.locals;
        self.f
            .instruction(&I::LocalTee(l.tf0))
            .instruction(&I::F64Trunc)
            .instruction(&I::LocalTee(l.tf1))
            // stack: [t]; compute the add-on
            .instruction(&I::F64Const(1.0.into()))
            .instruction(&I::LocalGet(l.tf0))
            .instruction(&I::F64Copysign)
            .instruction(&I::F64Const(0.0.into()))
            .instruction(&I::LocalGet(l.tf0))
            .instruction(&I::LocalGet(l.tf1))
            .instruction(&I::F64Sub)
            .instruction(&I::F64Abs)
            .instruction(&I::F64Const(0.5.into()))
            .instruction(&I::F64Ge)
            .instruction(&I::Select)
            .instruction(&I::F64Add);
    }

    // ---- series access -------------------------------------------------------

    fn emit_typed_load(&mut self, ty: VTy) {
        match ty {
            VTy::F64 => self.f.instruction(&I::F64Load(mem_f64())),
            VTy::I64 => self.f.instruction(&I::I64Load(mem_i64())),
            VTy::Bool3 | VTy::Color | VTy::Str | VTy::Ref => {
                self.f.instruction(&I::I32Load(mem_i32()))
            }
        };
    }
    fn emit_typed_store(&mut self, ty: VTy) {
        match ty {
            VTy::F64 => self.f.instruction(&I::F64Store(mem_f64())),
            VTy::I64 => self.f.instruction(&I::I64Store(mem_i64())),
            VTy::Bool3 | VTy::Color | VTy::Str | VTy::Ref => {
                self.f.instruction(&I::I32Store(mem_i32()))
            }
        };
    }

    /// Push the ring-cell address of `slot` at the given history index.
    /// Lanes use a uniform 8-byte stride regardless of cell type.
    fn push_slot_addr(&mut self, slot: u32, index: SlotIndex) {
        let l = self.ctx.locals;
        self.f
            .instruction(&I::GlobalGet(self.ctx.globals.slot_global(slot)))
            .instruction(&I::LocalGet(l.bar));
        match index {
            SlotIndex::Literal(0) => {}
            SlotIndex::Literal(k) => {
                self.f
                    .instruction(&I::I32Const(k as i32))
                    .instruction(&I::I32Sub);
            }
            SlotIndex::DynamicTi0 => {
                self.f
                    .instruction(&I::LocalGet(l.ti0))
                    .instruction(&I::I32WrapI64)
                    .instruction(&I::I32Sub);
            }
        }
        self.f
            .instruction(&I::I32Const(self.ctx.slots.mask(slot) as i32))
            .instruction(&I::I32And)
            .instruction(&I::I32Const(3))
            .instruction(&I::I32Shl)
            .instruction(&I::I32Add);
    }

    /// Shared guard for the dynamic-offset reads: pops the i64 offset, maps
    /// na/negative → na, offsets beyond `max_bars_back` → structured error,
    /// beyond available history → na; otherwise runs `read` with the offset
    /// in `ti0` (0 ≤ off ≤ bar).
    fn emit_dynamic_offset_guard(
        &mut self,
        pc: usize,
        ty: VTy,
        read: impl FnOnce(&mut Self) -> Result<(), AotError>,
    ) -> Result<(), AotError> {
        let l = self.ctx.locals;
        let mbb = i64::from(self.ctx.program.metadata.max_bars_back);
        self.want(1);
        self.consumed();
        self.f.instruction(&I::LocalSet(l.ti0));
        // na offset (sentinel) or negative → na
        self.f
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::I64Const(0))
            .instruction(&I::I64LtS)
            .instruction(&I::If(BlockType::Result(val_type(ty))));
        self.na_const(ty);
        self.f.instruction(&I::Else);
        {
            // beyond max_bars_back → structured runtime error
            self.f
                .instruction(&I::LocalGet(l.ti0))
                .instruction(&I::I64Const(mbb))
                .instruction(&I::I64GtS)
                .instruction(&I::If(BlockType::Empty));
            if self.ctx.hosts.linked.is_some() {
                // A linked host builds the interpreter's error for this
                // stop, which names the offset: leave it in scratch cell 1,
                // where `stop` has the host read it.
                self.f
                    .instruction(&I::GlobalGet(self.ctx.globals.scratch_base))
                    .instruction(&I::LocalGet(l.ti0))
                    .instruction(&I::I64Store(MemArg {
                        offset: 8,
                        align: 3,
                        memory_index: 0,
                    }));
            }
            self.err_exit(status::HISTORY_LIMIT, pc as u32);
            self.f.instruction(&I::End);
            // beyond available history → na
            self.f
                .instruction(&I::LocalGet(l.ti0))
                .instruction(&I::LocalGet(l.bar))
                .instruction(&I::I64ExtendI32U)
                .instruction(&I::I64GtS)
                .instruction(&I::If(BlockType::Result(val_type(ty))));
            self.na_const(ty);
            self.f.instruction(&I::Else);
            read(self)?;
            self.f.instruction(&I::End);
        }
        self.f.instruction(&I::End);
        Ok(())
    }

    /// Read builtin `source` at the bar index in `t32a` (0 ≤ idx ≤ bar).
    /// Pushes one value of `builtin_vty(source)` — or, for a `strategy.*`
    /// getter in a strategy module, one broker-snapshot read
    /// (`pinewasm_rt_broker_snap_f64` / `_i64`; a missing snapshot reads na),
    /// or, for a chart-timeframe read a standalone projection serves, what
    /// `chart_time.rs` reads for it.
    fn emit_builtin_read_at(&mut self, pc: usize, source: BuiltinSeries) -> Result<(), AotError> {
        use BuiltinSeries as B;
        let l = self.ctx.locals;
        let g = self.ctx.globals;
        if self.emit_chart_read_at(pc)? {
            return Ok(());
        }
        if source.is_strategy() {
            use crate::runtime::broker as broker_ids;
            let (field, ty) = strategy_snap_field(source).ok_or_else(|| {
                AotError::StrategyFeatureUnsupported {
                    feature: source.name().to_string(),
                }
            })?;
            let getter = self.ctx.hosts.broker_fn(match ty {
                VTy::I64 => broker_ids::SNAP_I64,
                _ => broker_ids::SNAP_F64,
            })?;
            self.f
                .instruction(&I::I32Const(field as i32))
                .instruction(&I::LocalGet(l.t32a))
                .instruction(&I::Call(getter));
            return Ok(());
        }
        let lane_load = |this: &mut Self, lane: usize| {
            this.f
                .instruction(&I::GlobalGet(g.builtin_global(lane)))
                .instruction(&I::LocalGet(l.t32a))
                .instruction(&I::I32Const(3))
                .instruction(&I::I32Shl)
                .instruction(&I::I32Add)
                .instruction(&I::F64Load(mem_f64()));
        };
        match source {
            B::SyminfoMintick => {
                self.f
                    .instruction(&I::I32Const(0))
                    .instruction(&I::F64Load(MemArg {
                        offset: u64::from(header::OFF_MINTICK),
                        align: 3,
                        memory_index: 0,
                    }));
            }
            B::Open => lane_load(self, 0),
            B::High => lane_load(self, 1),
            B::Low => lane_load(self, 2),
            B::Close => lane_load(self, 3),
            B::Volume => lane_load(self, 4),
            // Derived averages, computed in the interpreter's exact order;
            // overflow canonicalizes (from_f64 parity).
            B::Hl2 => {
                lane_load(self, 1);
                lane_load(self, 2);
                self.f
                    .instruction(&I::F64Add)
                    .instruction(&I::F64Const(2.0.into()))
                    .instruction(&I::F64Div);
                self.canonicalize_f64();
            }
            B::Hlc3 => {
                lane_load(self, 1);
                lane_load(self, 2);
                self.f.instruction(&I::F64Add);
                lane_load(self, 3);
                self.f
                    .instruction(&I::F64Add)
                    .instruction(&I::F64Const(3.0.into()))
                    .instruction(&I::F64Div);
                self.canonicalize_f64();
            }
            B::Ohlc4 => {
                lane_load(self, 0);
                lane_load(self, 1);
                self.f.instruction(&I::F64Add);
                lane_load(self, 2);
                self.f.instruction(&I::F64Add);
                lane_load(self, 3);
                self.f
                    .instruction(&I::F64Add)
                    .instruction(&I::F64Const(4.0.into()))
                    .instruction(&I::F64Div);
                self.canonicalize_f64();
            }
            B::Hlcc4 => {
                lane_load(self, 1);
                lane_load(self, 2);
                self.f.instruction(&I::F64Add);
                lane_load(self, 3);
                self.f.instruction(&I::F64Add);
                lane_load(self, 3);
                self.f
                    .instruction(&I::F64Add)
                    .instruction(&I::F64Const(4.0.into()))
                    .instruction(&I::F64Div);
                self.canonicalize_f64();
            }
            B::BarIndex => {
                self.f
                    .instruction(&I::LocalGet(l.t32a))
                    .instruction(&I::I64ExtendI32U);
            }
            B::Time => {
                lane_load(self, 5);
                self.emit_time_lane_guard(pc);
                self.f.instruction(&I::I64TruncSatF64S);
            }
            B::LastBarIndex => {
                self.f
                    .instruction(&I::GlobalGet(g.bars))
                    .instruction(&I::I32Const(1))
                    .instruction(&I::I32Sub)
                    .instruction(&I::I64ExtendI32U);
            }
            B::LastBarTime => {
                self.f
                    .instruction(&I::GlobalGet(g.builtin_global(5)))
                    .instruction(&I::GlobalGet(g.bars))
                    .instruction(&I::I32Const(1))
                    .instruction(&I::I32Sub)
                    .instruction(&I::I32Const(3))
                    .instruction(&I::I32Shl)
                    .instruction(&I::I32Add)
                    .instruction(&I::F64Load(mem_f64()));
                self.emit_time_lane_guard(pc);
                self.f.instruction(&I::I64TruncSatF64S);
            }
            // Historical-only run: constants / index comparisons (bool 0/2).
            B::BarstateIsfirst => {
                self.f
                    .instruction(&I::LocalGet(l.t32a))
                    .instruction(&I::I32Eqz)
                    .instruction(&I::I32Const(1))
                    .instruction(&I::I32Shl);
            }
            B::BarstateIslast | B::BarstateIslastconfirmedhistory => {
                self.f
                    .instruction(&I::LocalGet(l.t32a))
                    .instruction(&I::I32Const(1))
                    .instruction(&I::I32Add)
                    .instruction(&I::GlobalGet(g.bars))
                    .instruction(&I::I32Eq)
                    .instruction(&I::I32Const(1))
                    .instruction(&I::I32Shl);
            }
            B::BarstateIshistory | B::BarstateIsconfirmed | B::BarstateIsnew => {
                self.f.instruction(&I::I32Const(2));
            }
            B::BarstateIsrealtime => {
                self.f.instruction(&I::I32Const(0));
            }
            other => {
                return Err(AotError::UnsupportedBuiltin(other));
            }
        }
        Ok(())
    }

    // ---- inputs ---------------------------------------------------------------

    fn emit_read_input(&mut self, input: u32) -> Result<(), AotError> {
        let l = self.ctx.locals;
        let decl = self
            .ctx
            .program
            .inputs
            .get(input as usize)
            .ok_or_else(|| AotError::InvalidProgram(format!("input {input} out of range")))?;
        let table_off = header::SIZE + input * 8;

        if let Some(options) = self
            .ctx
            .projection
            .and_then(|projection| projection.string_inputs.get(&input))
        {
            // The standalone WebAssembly JSON runtime has already written an integer option index
            // into this input-table cell.  Map it to the constant-pool handle
            // used by PushStr; an invalid index falls back to the declaration
            // default, matching ordinary input override resolution.
            self.f
                .instruction(&I::I32Const(0))
                .instruction(&I::I64Load(MemArg {
                    offset: u64::from(table_off),
                    align: 3,
                    memory_index: 0,
                }))
                .instruction(&I::LocalSet(l.ti0))
                .instruction(&I::I32Const(
                    options.handles[options.default_index as usize] as i32,
                ))
                .instruction(&I::LocalSet(l.t32a));
            for (index, &handle) in options.handles.iter().enumerate() {
                self.f
                    .instruction(&I::I32Const(handle as i32))
                    .instruction(&I::LocalGet(l.t32a))
                    .instruction(&I::LocalGet(l.ti0))
                    .instruction(&I::I64Const(index as i64))
                    .instruction(&I::I64Eq)
                    .instruction(&I::Select)
                    .instruction(&I::LocalSet(l.t32a));
            }
            self.f.instruction(&I::LocalGet(l.t32a));
            self.pushed(VTy::Str);
            return Ok(());
        }

        if self
            .ctx
            .projection
            .is_some_and(|projection| projection.color_inputs.contains(&input))
        {
            let InputValue::Color { r, g, b, t } = &decl.defval else {
                return Err(AotError::InvalidProgram(format!(
                    "color input {input} has non-color default"
                )));
            };
            self.f.instruction(&I::I32Const(pack_color(*r, *g, *b, *t)));
            self.pushed(VTy::Color);
            return Ok(());
        }

        match decl.kind {
            InputKind::Float | InputKind::Price => {
                self.f
                    .instruction(&I::I32Const(0))
                    .instruction(&I::F64Load(MemArg {
                        offset: u64::from(table_off),
                        align: 3,
                        memory_index: 0,
                    }));
                self.canonicalize_f64();
                self.pushed(VTy::F64);
            }
            InputKind::Int | InputKind::Time | InputKind::Enum => {
                self.f
                    .instruction(&I::I32Const(0))
                    .instruction(&I::I64Load(MemArg {
                        offset: u64::from(table_off),
                        align: 3,
                        memory_index: 0,
                    }));
                self.pushed(VTy::I64);
            }
            InputKind::Bool => {
                self.f
                    .instruction(&I::I32Const(0))
                    .instruction(&I::F64Load(MemArg {
                        offset: u64::from(table_off),
                        align: 3,
                        memory_index: 0,
                    }))
                    .instruction(&I::F64Const(0.0.into()))
                    .instruction(&I::F64Ne)
                    .instruction(&I::I32Const(1))
                    .instruction(&I::I32Shl);
                self.pushed(VTy::Bool3);
            }
            // A linked host wrote this input's cell before the run: a
            // string's handle or a color's cell, in the low half of the
            // input's table cell.
            InputKind::String | InputKind::Color if self.ctx.analysis.linked_host => {
                self.f
                    .instruction(&I::I32Const(0))
                    .instruction(&I::I32Load(MemArg {
                        offset: u64::from(table_off),
                        align: 2,
                        memory_index: 0,
                    }));
                self.pushed(if decl.kind == InputKind::String {
                    VTy::Str
                } else {
                    VTy::Color
                });
            }
            InputKind::Source => {
                // Loader-synthesized lane holding the resolved series.
                let lane = self.ctx.globals.source_lane_global(input).ok_or_else(|| {
                    AotError::InvalidProgram(format!("no source lane for input {input}"))
                })?;
                self.f
                    .instruction(&I::GlobalGet(lane))
                    .instruction(&I::LocalGet(l.bar))
                    .instruction(&I::I32Const(3))
                    .instruction(&I::I32Shl)
                    .instruction(&I::I32Add)
                    .instruction(&I::F64Load(mem_f64()));
                self.pushed(VTy::F64);
            }
            _ => return Err(AotError::UnsupportedInputKind(format!("{:?}", decl.kind))),
        }
        Ok(())
    }

    // ---- windows / host calls ---------------------------------------------------

    fn emit_call_ta(&mut self, pc: usize, state: u32) -> Result<(), AotError> {
        let l = self.ctx.locals;
        let decl = self
            .ctx
            .program
            .ta_states
            .get(state as usize)
            .ok_or_else(|| AotError::InvalidProgram(format!("ta state {state}")))?;
        let ta_idx = self.ctx.hosts.ta.ok_or_else(|| {
            AotError::InvalidProgram("CallTa without the linked ta engine".into())
        })?;
        let operands = decl.func.engine_operands();

        // Pop operands (last is on top) into f64 staging locals, encoding
        // Cond bool3 → f64 {0, 1, NaN}.
        let stage = [l.tf0, l.tf1, l.tf2];
        for i in (0..operands.len()).rev() {
            self.want(1);
            let ty = self.consumed();
            match operands[i] {
                TaOperand::Num => {
                    debug_assert_eq!(ty, VTy::F64);
                }
                TaOperand::Cond => {
                    // select(NaN, select(1.0, 0.0, b == 2), b == 1)
                    self.f
                        .instruction(&I::LocalSet(l.t32a))
                        .instruction(&I::F64Const(f64::NAN.into()))
                        .instruction(&I::F64Const(1.0.into()))
                        .instruction(&I::F64Const(0.0.into()))
                        .instruction(&I::LocalGet(l.t32a))
                        .instruction(&I::I32Const(2))
                        .instruction(&I::I32Eq)
                        .instruction(&I::Select)
                        .instruction(&I::LocalGet(l.t32a))
                        .instruction(&I::I32Const(NA_BOOL3))
                        .instruction(&I::I32Eq)
                        .instruction(&I::Select);
                }
                TaOperand::Bool => {
                    // A Pine bool source is statically non-na: encode false/true
                    // as the same 0/1 values stored by the TA ring.
                    self.f
                        .instruction(&I::LocalSet(l.t32a))
                        .instruction(&I::F64Const(1.0.into()))
                        .instruction(&I::F64Const(0.0.into()))
                        .instruction(&I::LocalGet(l.t32a))
                        .instruction(&I::I32Const(2))
                        .instruction(&I::I32Eq)
                        .instruction(&I::Select);
                }
                TaOperand::Color => {
                    // Packed RGBT is an unsigned 32-bit integer; all colors
                    // are exactly representable in the engine's f64 ring.
                    self.f
                        .instruction(&I::LocalSet(l.t32a))
                        .instruction(&I::F64Const(f64::NAN.into()))
                        .instruction(&I::LocalGet(l.t32a))
                        .instruction(&I::F64ConvertI32U)
                        .instruction(&I::LocalGet(l.t32a))
                        .instruction(&I::I32Const(NA_COLOR))
                        .instruction(&I::I32Eq)
                        .instruction(&I::Select);
                }
            }
            self.f.instruction(&I::LocalSet(stage[i]));
        }

        // call pinewasm_rt_ta_advance(state, bar, a, b, c) in the linked engine.
        self.f
            .instruction(&I::I32Const(state as i32))
            .instruction(&I::LocalGet(l.bar));
        for (i, staged) in stage.iter().enumerate() {
            if i < operands.len() {
                self.f.instruction(&I::LocalGet(*staged));
            } else {
                self.f.instruction(&I::F64Const(f64::NAN.into()));
            }
        }
        self.f.instruction(&I::Call(ta_idx));

        // Decode the first (or only) result.
        let arity = decl.func.engine_result_arity();
        match decl.func.engine_result() {
            TaResult::Float => {
                self.pushed(VTy::F64);
            }
            TaResult::Int => {
                // select(MIN, trunc(r), isnan(r))
                self.f
                    .instruction(&I::LocalSet(l.tf0))
                    .instruction(&I::I64Const(NA_I64))
                    .instruction(&I::LocalGet(l.tf0))
                    .instruction(&I::I64TruncSatF64S)
                    .instruction(&I::LocalGet(l.tf0))
                    .instruction(&I::LocalGet(l.tf0))
                    .instruction(&I::F64Ne)
                    .instruction(&I::Select);
                if self.int_min_guard(pc) == Some(IntMinGuard::TaResult) {
                    // NaN (the engine's `na`) fails both comparisons. A value
                    // at or below -2^63 saturates to the sentinel, and one at
                    // or above 2^63 is a float in the interpreter.
                    self.f
                        .instruction(&I::LocalGet(l.tf0))
                        .instruction(&I::F64Const((i64::MIN as f64).into()))
                        .instruction(&I::F64Le)
                        .instruction(&I::LocalGet(l.tf0))
                        .instruction(&I::F64Const((-(i64::MIN as f64)).into()))
                        .instruction(&I::F64Ge)
                        .instruction(&I::I32Or);
                    if crate::analysis::returns_int_source(decl.func) {
                        // A captured fraction is a float in the interpreter.
                        // NaN fails the comparison.
                        self.f
                            .instruction(&I::LocalGet(l.tf0))
                            .instruction(&I::LocalGet(l.tf0))
                            .instruction(&I::F64Trunc)
                            .instruction(&I::F64Sub)
                            .instruction(&I::F64Abs)
                            .instruction(&I::F64Const(0.0.into()))
                            .instruction(&I::F64Gt)
                            .instruction(&I::I32Or);
                    }
                    self.emit_int_min_exit(pc);
                }
                self.pushed(VTy::I64);
            }
            TaResult::Bool => {
                // na → 1 else (r != 0) << 1
                self.f
                    .instruction(&I::LocalSet(l.tf0))
                    .instruction(&I::I32Const(NA_BOOL3))
                    .instruction(&I::LocalGet(l.tf0))
                    .instruction(&I::F64Const(0.0.into()))
                    .instruction(&I::F64Ne)
                    .instruction(&I::I32Const(1))
                    .instruction(&I::I32Shl)
                    .instruction(&I::LocalGet(l.tf0))
                    .instruction(&I::LocalGet(l.tf0))
                    .instruction(&I::F64Ne)
                    .instruction(&I::Select);
                self.pushed(VTy::Bool3);
            }
            TaResult::Color => {
                // NaN maps to the packed color na sentinel; finite ring
                // values are exact unsigned RGBT words.
                self.f
                    .instruction(&I::LocalSet(l.tf0))
                    .instruction(&I::I32Const(NA_COLOR))
                    .instruction(&I::LocalGet(l.tf0))
                    .instruction(&I::I32TruncSatF64U)
                    .instruction(&I::LocalGet(l.tf0))
                    .instruction(&I::LocalGet(l.tf0))
                    .instruction(&I::F64Ne)
                    .instruction(&I::Select);
                self.pushed(VTy::Color);
            }
        }
        // Tuple extras arrive via the scratch region (the engine wrote them
        // there); every tuple element is a float.
        for extra in 0..arity.saturating_sub(1) {
            self.f
                .instruction(&I::GlobalGet(self.ctx.globals.scratch_base))
                .instruction(&I::F64Load(MemArg {
                    offset: u64::from(extra * 8),
                    align: 3,
                    memory_index: 0,
                }));
            self.pushed(VTy::F64);
        }
        Ok(())
    }

    /// f64 color-channel operand on the stack → i64 with `pure_color_int`
    /// parity: round half away from zero, saturating cast, non-finite → na
    /// sentinel.
    fn emit_f64_channel_to_i64(&mut self) {
        let l = self.ctx.locals;
        self.f.instruction(&I::LocalSet(l.tf2));
        self.f
            .instruction(&I::I64Const(NA_I64))
            .instruction(&I::LocalGet(l.tf2));
        self.emit_round_half_away();
        self.f.instruction(&I::I64TruncSatF64S);
        // A large-negative FINITE channel saturates to `i64::MIN`, which is
        // also the na sentinel — left as-is the downstream na check would
        // turn the whole color na, but the interpreter clamps it to a valid
        // channel (`clamp_u8(i64::MIN, ..) == 0`). Bump `MIN → MIN + 1` so a
        // finite input is never mistaken for na; the caller clamps to
        // `[0, hi]`, where both map to 0, so the value is unchanged.
        self.f
            .instruction(&I::LocalSet(l.ti0))
            .instruction(&I::I64Const(NA_I64 + 1))
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::I64Const(NA_I64))
            .instruction(&I::I64Eq)
            .instruction(&I::Select);
        self.f
            .instruction(&I::LocalGet(l.tf2))
            .instruction(&I::F64Abs)
            .instruction(&I::F64Const(f64::INFINITY.into()))
            .instruction(&I::F64Eq)
            .instruction(&I::LocalGet(l.tf2))
            .instruction(&I::LocalGet(l.tf2))
            .instruction(&I::F64Ne)
            .instruction(&I::I32Or)
            .instruction(&I::Select);
    }

    /// Clamp the i64 in `src` to `[lo, hi]` and leave the i32 result in
    /// `t32b` (`clamp_u8` parity; the sentinel clamps like any value, the
    /// caller's na check wins).
    fn emit_clamp_i64(&mut self, src: u32, lo: i64, hi: i64) {
        let l = self.ctx.locals;
        // t = min(max(x, lo), hi)
        self.f
            .instruction(&I::LocalGet(src))
            .instruction(&I::I64Const(lo))
            .instruction(&I::LocalGet(src))
            .instruction(&I::I64Const(lo))
            .instruction(&I::I64GtS)
            .instruction(&I::Select)
            .instruction(&I::LocalSet(l.ti1))
            .instruction(&I::LocalGet(l.ti1))
            .instruction(&I::I64Const(hi))
            .instruction(&I::LocalGet(l.ti1))
            .instruction(&I::I64Const(hi))
            .instruction(&I::I64LtS)
            .instruction(&I::Select)
            .instruction(&I::I32WrapI64)
            .instruction(&I::LocalSet(l.t32b));
    }

    /// Push `is_na(top-of-stack-in-local-form)` — the operand must be ON the
    /// real stack; consumes it and pushes an i32 flag.
    fn emit_is_na(&mut self, ty: VTy) {
        let l = self.ctx.locals;
        match ty {
            VTy::F64 => {
                self.f
                    .instruction(&I::LocalTee(l.tf0))
                    .instruction(&I::LocalGet(l.tf0))
                    .instruction(&I::F64Ne);
            }
            VTy::I64 => {
                self.f
                    .instruction(&I::I64Const(NA_I64))
                    .instruction(&I::I64Eq);
            }
            VTy::Bool3 => {
                self.f
                    .instruction(&I::I32Const(NA_BOOL3))
                    .instruction(&I::I32Eq);
            }
            VTy::Color => {
                self.f
                    .instruction(&I::I32Const(NA_COLOR))
                    .instruction(&I::I32Eq);
            }
            VTy::Str => {
                self.f
                    .instruction(&I::I32Const(NA_STR))
                    .instruction(&I::I32Eq);
            }
            VTy::Ref => {
                self.f
                    .instruction(&I::I32Const(NA_REF))
                    .instruction(&I::I32Eq);
            }
        }
    }

    /// Pop an i32 color from the vstack into a color lane store at `bar`.
    fn emit_color_store(&mut self, lane_global: u32) {
        let l = self.ctx.locals;
        self.want(1);
        self.consumed();
        self.f
            .instruction(&I::LocalSet(l.t32a))
            .instruction(&I::GlobalGet(lane_global))
            .instruction(&I::LocalGet(l.bar))
            .instruction(&I::I32Const(2))
            .instruction(&I::I32Shl)
            .instruction(&I::I32Add)
            .instruction(&I::LocalGet(l.t32a))
            .instruction(&I::I32Store(mem_i32()));
    }

    // ---- comparisons / int arithmetic -----------------------------------------

    /// Whether the comparison at `compare` reads the fused Pine integer
    /// quotient `operand` against an integer. Only then does the order depend
    /// on whether the quotient is exact; against a float both the
    /// interpreter and the f64 lane compare floats.
    fn quotient_compare_is_exact(
        &self,
        compare: usize,
        operand: crate::analysis::QuotientOperand,
    ) -> bool {
        use crate::analysis::QuotientOperand;
        let tys = &self.ctx.analysis.operand_tys[compare];
        matches!(
            (operand, tys.as_slice()),
            (QuotientOperand::Lhs, [VTy::F64, VTy::I64])
                | (QuotientOperand::Rhs, [VTy::I64, VTy::F64])
        )
    }

    /// Compare a fused Pine integer quotient with an integer. The division
    /// left the f64 value on the stack, the exact quotient in the i64 local
    /// of the quotient's stack depth, and 1 (exact), 2 (uneven) or 0 (`na`)
    /// in that depth's tag local. An exact quotient compares as an integer;
    /// an uneven one compares as a float against the promoted integer; `na`
    /// on either side is false.
    fn emit_quotient_cmp(&mut self, instr: &CsilInstr, operand: crate::analysis::QuotientOperand) {
        use crate::analysis::QuotientOperand;
        use crate::tagged_integer::emit_comparison;
        let l = self.ctx.locals;
        self.want(2);
        let (lhs_depth, rhs_depth) = (self.vstack.len() - 2, self.vstack.len() - 1);
        self.consumed();
        self.consumed();
        let (quotient_depth, integer_depth) = match operand {
            QuotientOperand::Lhs => (lhs_depth, rhs_depth),
            QuotientOperand::Rhs => (rhs_depth, lhs_depth),
        };
        let exact = self.stack_local(quotient_depth, VTy::I64);
        let tag = self.integer_valid_local(quotient_depth);
        let integer_valid = self.integer_valid_local(integer_depth);
        // The integer operand goes to `ti1`, the quotient's f64 to `tf2`.
        match operand {
            QuotientOperand::Lhs => self
                .f
                .instruction(&I::LocalSet(l.ti1))
                .instruction(&I::LocalSet(l.tf2)),
            QuotientOperand::Rhs => self
                .f
                .instruction(&I::LocalSet(l.tf2))
                .instruction(&I::LocalSet(l.ti1)),
        };
        self.f
            .instruction(&I::LocalGet(tag))
            .instruction(&I::I32Const(1))
            .instruction(&I::I32Eq)
            .instruction(&I::If(BlockType::Result(ValType::I32)));
        let op = Self::integer_comparison(instr);
        match operand {
            QuotientOperand::Lhs => emit_comparison(self.f, op, exact, tag, l.ti1, integer_valid),
            QuotientOperand::Rhs => emit_comparison(self.f, op, l.ti1, integer_valid, exact, tag),
        }
        self.f.instruction(&I::Else);
        self.f.instruction(&I::LocalGet(l.ti1));
        self.emit_i64_to_f64_tagged(integer_valid);
        match operand {
            QuotientOperand::Lhs => self
                .f
                .instruction(&I::LocalSet(l.tf1))
                .instruction(&I::LocalGet(l.tf2))
                .instruction(&I::LocalSet(l.tf0)),
            QuotientOperand::Rhs => self
                .f
                .instruction(&I::LocalSet(l.tf0))
                .instruction(&I::LocalGet(l.tf2))
                .instruction(&I::LocalSet(l.tf1)),
        };
        self.emit_f64_cmp_locals(instr);
        self.f.instruction(&I::End);
        self.pushed(VTy::Bool3);
    }

    fn integer_comparison(instr: &CsilInstr) -> crate::tagged_integer::Comparison {
        use crate::tagged_integer::Comparison;
        use CsilInstr as Csil;
        match instr {
            Csil::CmpGt => Comparison::Greater,
            Csil::CmpGe => Comparison::GreaterEqual,
            Csil::CmpLt => Comparison::Less,
            Csil::CmpLe => Comparison::LessEqual,
            Csil::CmpEq => Comparison::Equal,
            _ => Comparison::NotEqual,
        }
    }

    /// PineWASM float equality: exact IEEE equality, or an absolute difference
    /// within the `pinewasm_num::comparison::EPSILON` tolerance.
    fn emit_f64_equal_locals(&mut self) {
        let l = self.ctx.locals;
        self.f
            .instruction(&I::LocalGet(l.tf0))
            .instruction(&I::LocalGet(l.tf1))
            .instruction(&I::F64Eq)
            .instruction(&I::LocalGet(l.tf0))
            .instruction(&I::LocalGet(l.tf1))
            .instruction(&I::F64Sub)
            .instruction(&I::F64Abs)
            .instruction(&I::F64Const(pinewasm_num::comparison::EPSILON.into()))
            .instruction(&I::F64Le)
            .instruction(&I::I32Or);
    }

    /// The script f64 comparison of `tf0` with `tf1`, leaving Bool3.
    fn emit_f64_cmp_locals(&mut self, instr: &CsilInstr) {
        use CsilInstr as Csil;
        let l = self.ctx.locals;
        // A missing (na) operand makes the comparison false.
        // select(0, script_cmp(l, r) << 1, isnan(l) | isnan(r))
        self.f.instruction(&I::I32Const(0));
        if matches!(instr, Csil::CmpEq | Csil::CmpNe) {
            self.emit_f64_equal_locals();
            if matches!(instr, Csil::CmpNe) {
                self.f.instruction(&I::I32Eqz);
            }
        } else {
            self.f
                .instruction(&I::LocalGet(l.tf0))
                .instruction(&I::LocalGet(l.tf1));
            self.f.instruction(match instr {
                Csil::CmpGt | Csil::CmpGe => &I::F64Gt,
                Csil::CmpLt | Csil::CmpLe => &I::F64Lt,
                _ => unreachable!(),
            });
            self.emit_f64_equal_locals();
            if matches!(instr, Csil::CmpGt | Csil::CmpLt) {
                self.f.instruction(&I::I32Eqz).instruction(&I::I32And);
            } else {
                self.f.instruction(&I::I32Or);
            }
        }
        self.f
            .instruction(&I::I32Const(1))
            .instruction(&I::I32Shl)
            .instruction(&I::LocalGet(l.tf0))
            .instruction(&I::LocalGet(l.tf0))
            .instruction(&I::F64Ne)
            .instruction(&I::LocalGet(l.tf1))
            .instruction(&I::LocalGet(l.tf1))
            .instruction(&I::F64Ne)
            .instruction(&I::I32Or)
            .instruction(&I::Select);
    }

    fn emit_cmp(&mut self, pc: usize, instr: &CsilInstr) -> Result<(), AotError> {
        let tys = &self.ctx.analysis.operand_tys[pc];
        let (lt, rt) = (tys[0], tys[1]);
        let l = self.ctx.locals;

        if let Some(operand) = crate::analysis::quotient_compare_operand(
            &self.ctx.program.main().code,
            &self.ctx.analysis.blocks,
            pc,
        ) {
            if self.quotient_compare_is_exact(pc, operand) {
                self.emit_quotient_cmp(instr, operand);
                return Ok(());
            }
        }

        // Mixed numeric: promote the int side to float (interpreter rule).
        if matches!((lt, rt), (VTy::I64, VTy::F64) | (VTy::F64, VTy::I64)) {
            self.want(2);
            let lhs_valid = self.integer_valid_local(self.vstack.len() - 2);
            let rhs_valid = self.integer_valid_local(self.vstack.len() - 1);
            self.consumed();
            self.consumed();
            if rt == VTy::I64 {
                self.emit_i64_to_f64_tagged(rhs_valid);
            } else {
                // Promote the DEEPER operand: stash rhs, convert, restore.
                self.f.instruction(&I::LocalSet(l.tf1));
                self.emit_i64_to_f64_tagged(lhs_valid);
                self.f.instruction(&I::LocalGet(l.tf1));
            }
            return self.emit_cmp_typed(pc, instr, VTy::F64);
        }
        if lt != rt {
            return Err(AotError::TypeConflict(format!(
                "cmp between {lt:?} and {rt:?} at pc {pc}"
            )));
        }
        self.want(2);
        self.consumed();
        self.consumed();
        self.emit_cmp_typed(pc, instr, lt)
    }

    fn emit_cmp_typed(&mut self, pc: usize, instr: &CsilInstr, ty: VTy) -> Result<(), AotError> {
        use CsilInstr as Csil;
        let l = self.ctx.locals;
        match ty {
            VTy::F64 => {
                self.f
                    .instruction(&I::LocalSet(l.tf1))
                    .instruction(&I::LocalSet(l.tf0));
                self.emit_f64_cmp_locals(instr);
            }
            VTy::I64 => {
                use crate::tagged_integer::{emit_comparison, Comparison};
                self.f
                    .instruction(&I::LocalSet(l.ti1))
                    .instruction(&I::LocalSet(l.ti0));
                let op = match instr {
                    Csil::CmpGt => Comparison::Greater,
                    Csil::CmpGe => Comparison::GreaterEqual,
                    Csil::CmpLt => Comparison::Less,
                    Csil::CmpLe => Comparison::LessEqual,
                    Csil::CmpEq => Comparison::Equal,
                    _ => Comparison::NotEqual,
                };
                let lhs_valid = self.integer_valid_local(self.vstack.len());
                let rhs_valid = self.integer_valid_local(self.vstack.len() + 1);
                emit_comparison(self.f, op, l.ti0, lhs_valid, l.ti1, rhs_valid);
            }
            VTy::Str | VTy::Bool3 | VTy::Color | VTy::Ref => {
                if ty == VTy::Str
                    && !self.ctx.analysis.canonical_strings
                    && !self
                        .ctx
                        .projection
                        .is_some_and(|projection| projection.canonical_string_cmp.contains(&pc))
                {
                    // Handle equality is string equality only beside a
                    // linked host, which gives equal strings one handle, and
                    // for the standalone WebAssembly projection's canonical
                    // constant/finite-option values.
                    return Err(AotError::TypeConflict(
                        "string comparison must lower to a host site".to_string(),
                    ));
                }
                if !matches!(instr, Csil::CmpEq | Csil::CmpNe) {
                    return Err(AotError::TypeConflict(
                        "ordering comparison on non-numeric operands".to_string(),
                    ));
                }
                let na = match ty {
                    VTy::Bool3 => NA_BOOL3,
                    VTy::Color => NA_COLOR,
                    VTy::Str => NA_STR,
                    VTy::Ref => NA_REF,
                    _ => unreachable!("i32 comparison lane"),
                };
                self.f
                    .instruction(&I::LocalSet(l.t32b))
                    .instruction(&I::LocalSet(l.t32a));
                self.f
                    .instruction(&I::I32Const(0))
                    .instruction(&I::LocalGet(l.t32a))
                    .instruction(&I::LocalGet(l.t32b));
                self.f.instruction(if matches!(instr, Csil::CmpEq) {
                    &I::I32Eq
                } else {
                    &I::I32Ne
                });
                self.f
                    .instruction(&I::I32Const(1))
                    .instruction(&I::I32Shl)
                    .instruction(&I::LocalGet(l.t32a))
                    .instruction(&I::I32Const(na))
                    .instruction(&I::I32Eq)
                    .instruction(&I::LocalGet(l.t32b))
                    .instruction(&I::I32Const(na))
                    .instruction(&I::I32Eq)
                    .instruction(&I::I32Or)
                    .instruction(&I::Select);
            }
        }
        self.pushed(VTy::Bool3);
        Ok(())
    }

    /// Checked i64 binops with sentinel-na semantics (see docs/aot.md §3).
    /// The division's next consumer widens to float. Whole quotients must
    /// still divide exactly in i64 before conversion (not round operands first).
    fn emit_widened_pine_int_div(&mut self) {
        let l = self.ctx.locals;
        self.want(2);
        self.consumed();
        self.consumed();
        self.f
            .instruction(&I::LocalSet(l.ti1))
            .instruction(&I::LocalSet(l.ti0));
        self.f
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::I64Const(NA_I64))
            .instruction(&I::I64Eq)
            .instruction(&I::LocalGet(l.ti1))
            .instruction(&I::I64Const(NA_I64))
            .instruction(&I::I64Eq)
            .instruction(&I::I32Or)
            .instruction(&I::LocalGet(l.ti1))
            .instruction(&I::I64Eqz)
            .instruction(&I::I32Or)
            .instruction(&I::If(BlockType::Result(ValType::F64)))
            .instruction(&I::F64Const(f64::NAN.into()))
            .instruction(&I::Else)
            // PineWASM's integer division returns na for MIN / -1 instead of
            // trapping on WebAssembly's signed-division overflow.
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::I64Const(i64::MIN))
            .instruction(&I::I64Eq)
            .instruction(&I::LocalGet(l.ti1))
            .instruction(&I::I64Const(-1))
            .instruction(&I::I64Eq)
            .instruction(&I::I32And)
            .instruction(&I::If(BlockType::Result(ValType::F64)))
            .instruction(&I::F64Const(f64::NAN.into()))
            .instruction(&I::Else)
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::LocalGet(l.ti1))
            .instruction(&I::I64RemS)
            .instruction(&I::I64Eqz)
            .instruction(&I::If(BlockType::Result(ValType::F64)))
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::LocalGet(l.ti1))
            .instruction(&I::I64DivS)
            .instruction(&I::F64ConvertI64S)
            .instruction(&I::Else)
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::F64ConvertI64S)
            .instruction(&I::LocalGet(l.ti1))
            .instruction(&I::F64ConvertI64S)
            .instruction(&I::F64Div)
            .instruction(&I::End)
            .instruction(&I::End);
        self.f.instruction(&I::End);
        self.pushed(VTy::F64);
    }

    /// The division's result is compared with an integer by the fused
    /// comparison ([`Self::emit_quotient_cmp`]). Besides the f64 value of
    /// [`Self::emit_widened_pine_int_div`], keep what the comparison needs to
    /// order an exact quotient as an integer: the i64 quotient in the i64
    /// local of the result's stack depth and, in that depth's tag local,
    /// 1 for an exact quotient, 2 for an uneven one and 0 for `na`. Nothing
    /// between the division and the comparison touches those two locals: at
    /// most one push runs there, one depth above.
    fn emit_compared_pine_int_div(&mut self) {
        let l = self.ctx.locals;
        self.want(2);
        let depth = self.vstack.len() - 2;
        let exact = self.stack_local(depth, VTy::I64);
        let tag = self.integer_valid_local(depth);
        self.consumed();
        self.consumed();
        self.f
            .instruction(&I::LocalSet(l.ti1))
            .instruction(&I::LocalSet(l.ti0))
            .instruction(&I::I32Const(0))
            .instruction(&I::LocalSet(tag));
        self.f
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::I64Const(NA_I64))
            .instruction(&I::I64Eq)
            .instruction(&I::LocalGet(l.ti1))
            .instruction(&I::I64Const(NA_I64))
            .instruction(&I::I64Eq)
            .instruction(&I::I32Or)
            .instruction(&I::LocalGet(l.ti1))
            .instruction(&I::I64Eqz)
            .instruction(&I::I32Or)
            .instruction(&I::If(BlockType::Result(ValType::F64)))
            .instruction(&I::F64Const(f64::NAN.into()))
            .instruction(&I::Else)
            // The first test already answered `na` for a dividend equal to
            // the sentinel, so the signed division below cannot overflow.
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::LocalGet(l.ti1))
            .instruction(&I::I64RemS)
            .instruction(&I::I64Eqz)
            .instruction(&I::If(BlockType::Result(ValType::F64)))
            .instruction(&I::I32Const(1))
            .instruction(&I::LocalSet(tag))
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::LocalGet(l.ti1))
            .instruction(&I::I64DivS)
            .instruction(&I::LocalTee(exact))
            .instruction(&I::F64ConvertI64S)
            .instruction(&I::Else)
            .instruction(&I::I32Const(2))
            .instruction(&I::LocalSet(tag))
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::F64ConvertI64S)
            .instruction(&I::LocalGet(l.ti1))
            .instruction(&I::F64ConvertI64S)
            .instruction(&I::F64Div)
            .instruction(&I::End)
            .instruction(&I::End);
        self.pushed(VTy::F64);
    }

    /// Preserve the exact integer quotient when it is immediately rounded
    /// or cast to an integer. Fractional quotients follow the VM's f64
    /// division and then its `math.round` (half away from zero),
    /// `math.floor`, `math.ceil` or `int()` (toward zero) path. The following
    /// `CallPure` remains in CSIL for instruction accounting and is an
    /// identity on the emitted i64.
    ///
    /// The result is never a present `i64::MIN`: an exact quotient of two
    /// present operands is the minimum only for `MIN / 1`, and an uneven one
    /// has a divisor of magnitude two or more, so it rounds to a magnitude of
    /// at most `2^62`. No integer-minimum guard belongs to this site.
    fn emit_rounded_pine_int_div(&mut self, rounding: crate::analysis::QuotientRounding) {
        use crate::analysis::QuotientRounding;
        let l = self.ctx.locals;
        self.want(2);
        self.consumed();
        self.consumed();
        self.f
            .instruction(&I::LocalSet(l.ti1))
            .instruction(&I::LocalSet(l.ti0))
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::I64Const(NA_I64))
            .instruction(&I::I64Eq)
            .instruction(&I::LocalGet(l.ti1))
            .instruction(&I::I64Const(NA_I64))
            .instruction(&I::I64Eq)
            .instruction(&I::I32Or)
            .instruction(&I::LocalGet(l.ti1))
            .instruction(&I::I64Eqz)
            .instruction(&I::I32Or)
            .instruction(&I::If(BlockType::Result(ValType::I64)))
            .instruction(&I::I64Const(NA_I64))
            .instruction(&I::Else)
            // i64::MIN / -1 overflows; PineWASM returns na.
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::I64Const(i64::MIN))
            .instruction(&I::I64Eq)
            .instruction(&I::LocalGet(l.ti1))
            .instruction(&I::I64Const(-1))
            .instruction(&I::I64Eq)
            .instruction(&I::I32And)
            .instruction(&I::If(BlockType::Result(ValType::I64)))
            .instruction(&I::I64Const(NA_I64))
            .instruction(&I::Else)
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::LocalGet(l.ti1))
            .instruction(&I::I64RemS)
            .instruction(&I::I64Eqz)
            .instruction(&I::If(BlockType::Result(ValType::I64)))
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::LocalGet(l.ti1))
            .instruction(&I::I64DivS)
            .instruction(&I::Else)
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::F64ConvertI64S)
            .instruction(&I::LocalGet(l.ti1))
            .instruction(&I::F64ConvertI64S)
            .instruction(&I::F64Div);
        match rounding {
            QuotientRounding::Nearest => self.emit_round_half_away(),
            QuotientRounding::Floor => {
                self.f.instruction(&I::F64Floor);
            }
            QuotientRounding::Ceil => {
                self.f.instruction(&I::F64Ceil);
            }
            QuotientRounding::Truncate => {
                self.f.instruction(&I::F64Trunc);
            }
        }
        self.emit_f64_to_int();
        self.f
            .instruction(&I::End)
            .instruction(&I::End)
            .instruction(&I::End);
        self.pushed(VTy::I64);
    }

    fn emit_int_binop(&mut self, pc: usize, instr: &CsilInstr) {
        use crate::tagged_integer::{emit_checked_binary, BinaryLocals, BinaryOp};
        use CsilInstr as Csil;
        let l = self.ctx.locals;
        let op = match instr {
            Csil::IntAdd => BinaryOp::Add,
            Csil::IntSub => BinaryOp::Subtract,
            Csil::IntMul => BinaryOp::Multiply,
            Csil::IntDiv => BinaryOp::Divide,
            Csil::IntRem => BinaryOp::Remainder,
            _ => unreachable!("emit_int_binop callers"),
        };
        self.want(2);
        let lhs_valid = self.integer_valid_local(self.vstack.len() - 2);
        let rhs_valid = self.integer_valid_local(self.vstack.len() - 1);
        self.consumed();
        self.consumed();
        self.f
            .instruction(&I::LocalSet(l.ti1))
            .instruction(&I::LocalSet(l.ti0));

        // Load independent operand tags before scratch arithmetic begins.
        self.f
            .instruction(&I::LocalGet(lhs_valid))
            .instruction(&I::LocalSet(l.t32a))
            .instruction(&I::LocalGet(rhs_valid))
            .instruction(&I::LocalSet(l.t32b));
        emit_checked_binary(
            self.f,
            op,
            BinaryLocals {
                lhs: l.ti0,
                lhs_valid: l.t32a,
                rhs: l.ti1,
                rhs_valid: l.t32b,
                value: l.ti2,
                valid: l.t32c,
            },
        );
        // The native stack carries a single sentinel cell.
        self.f
            .instruction(&I::LocalSet(l.t32c))
            .instruction(&I::I64Const(NA_I64))
            .instruction(&I::LocalGet(l.t32c))
            .instruction(&I::Select);
        if self.int_min_guard(pc) == Some(IntMinGuard::Arith) {
            // `valid` excludes an `na` operand and a checked overflow, so a
            // valid payload equal to the sentinel is a present minimum.
            self.f
                .instruction(&I::LocalGet(l.t32c))
                .instruction(&I::LocalGet(l.ti2))
                .instruction(&I::I64Const(NA_I64))
                .instruction(&I::I64Eq)
                .instruction(&I::I32And);
            self.emit_int_min_exit(pc);
        }
        self.pushed_integer_with_valid(l.t32c);
    }
}

#[derive(Clone, Copy)]
enum SlotIndex {
    /// History offset baked at emit time (0 = current bar).
    Literal(u32),
    /// Offset in the `ti0` local (already validated 0 ≤ off ≤ bar).
    DynamicTi0,
}

fn pack_color(r: u8, g: u8, b: u8, t: f64) -> i32 {
    ((u32::from(r) << 24) | (u32::from(g) << 16) | (u32::from(b) << 8) | t as u32) as i32
}
