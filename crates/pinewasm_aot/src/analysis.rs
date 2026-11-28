//! Pre-emission analysis: CFG recovery, dominators, loop/merge
//! classification (for the structured-control-flow algorithm in `body.rs`),
//! and demand-driven type inference assigning one concrete wasm type to
//! every stack cell and user slot.
//!
//! The CSIL validator already proved stack-depth consistency and operand
//! typing at the `AbstractTy` level; this pass re-derives CONCRETE types
//! (the validator's lattice is permissive — `PushNa` is `Any` — while wasm
//! locals need exactly one value type). `na` producers start `Unknown` and
//! are pinned by the first typed consumer; a residual `Unknown` defaults to
//! f64 (na-as-float, the interpreter's serialization default). Any true
//! conflict (a cell or slot demanded as two different concrete types) is an
//! [`AotError`] and the script falls back to the interpreter.

mod color_precision;
mod float_integer_ranges;
mod integer_fields;

use crate::AotError;
use pinewasm_csil::{AbstractTy, CsilInstr, CsilProgram, TaStateDecl};
use pinewasm_hir::{
    BuiltinSeries, DrawKind, InputKind, InputValue, OrderKind, PureFunc, TradeAccessor,
};
use std::collections::{BTreeMap, BTreeSet};

/// Concrete wasm-level value type of one CSIL stack cell / user slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VTy {
    F64,
    /// i64 with `i64::MIN` as the na sentinel.
    I64,
    /// Kleene bool in i32: 0 = false, 1 = na, 2 = true.
    Bool3,
    /// Packed rgbt in i32 (`r<<24 | g<<16 | b<<8 | t`); `0xFFFF_FFFF` = na.
    Color,
    /// Run string-table handle in i32 (`-1` = na): constant-pool ids first,
    /// host-produced dynamic strings after. String CONTENT operations
    /// (concat, equality, `str.*`) execute on the host, which owns the table.
    Str,
    /// VM heap handle in i32 (`-1` = na): collections, UDT objects, drawings,
    /// footprints. The heap stays host-side (docs/aot.md §5); native
    /// code moves handles, and `CmpEq` on refs is identity — exactly the
    /// interpreter's `HandleId` equality.
    Ref,
}

/// Whether a user slot has no value or write-state observers in the program.
///
/// This is intentionally conservative: persistent slots, history/window
/// reads, TA state, and initialize-once branches all make a store observable.
/// Native lowering may then choose an intermediate representation for a
/// value written only to such a slot without changing any reachable result.
pub(crate) fn slot_is_unobserved(program: &CsilProgram, slot: u32) -> bool {
    let Some(definition) = program.series_slots.get(slot as usize) else {
        return false;
    };
    if definition.persistent || definition.varip {
        return false;
    }
    for function in &program.functions {
        for instr in &function.code {
            match instr {
                CsilInstr::ReadUserSlot { slot: read, .. }
                | CsilInstr::ReadUserSlotDynamic { slot: read }
                | CsilInstr::WindowMean { slot: read, .. }
                | CsilInstr::WindowMeanDynamic { slot: read }
                | CsilInstr::WindowExtremumDynamic { slot: read, .. }
                | CsilInstr::WindowStatisticDynamic { slot: read, .. }
                | CsilInstr::EmaSimpleDynamic { slot: read }
                | CsilInstr::JumpIfSlotWritten { slot: read, .. }
                    if *read == slot =>
                {
                    return false;
                }
                _ => {}
            }
        }
    }
    if program.ta_input_bindings.iter().any(|binding| {
        matches!(
            binding.target,
            pinewasm_hir::TaInputTarget::Sma { slot: target }
                | pinewasm_hir::TaInputTarget::Change { slot: target }
                if target == slot
        )
    }) {
        return false;
    }
    true
}

/// Which operand of an ordering or equality comparison is a kept Pine integer
/// quotient (see [`quotient_compare_operand`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuotientOperand {
    Lhs,
    Rhs,
}

fn is_compare(instr: Option<&CsilInstr>) -> bool {
    matches!(
        instr,
        Some(
            CsilInstr::CmpGt
                | CsilInstr::CmpGe
                | CsilInstr::CmpLt
                | CsilInstr::CmpLe
                | CsilInstr::CmpEq
                | CsilInstr::CmpNe
        )
    )
}

/// An instruction that pops nothing, pushes exactly one value and transfers
/// no control: the only code admitted between a kept quotient and the
/// comparison that reads it as its left operand.
fn is_plain_push(instr: Option<&CsilInstr>) -> bool {
    matches!(
        instr,
        Some(
            CsilInstr::PushI64(_)
                | CsilInstr::PushF64(_)
                | CsilInstr::ReadInput { .. }
                | CsilInstr::ReadBuiltin { .. }
                | CsilInstr::ReadUserSlot { .. }
        )
    )
}

/// Whether `pc` starts a basic block (`blocks` is in pc order).
fn is_block_start(blocks: &[Block], pc: usize) -> bool {
    blocks
        .binary_search_by(|block| block.start.cmp(&pc))
        .is_ok()
}

/// The comparison that consumes the `PineIntDiv` result at `pc`, when the two
/// are fused: the quotient is the right operand of the very next instruction
/// (`x > a / b`), or the left operand with one plain push between
/// (`a / b > x`). Every instruction after the division is inside its basic
/// block, so no other path reaches the comparison with a different cell in
/// the quotient's position.
pub(crate) fn quotient_compare(
    code: &[CsilInstr],
    blocks: &[Block],
    pc: usize,
) -> Option<(usize, QuotientOperand)> {
    if !matches!(code.get(pc), Some(CsilInstr::PineIntDiv)) || is_block_start(blocks, pc + 1) {
        return None;
    }
    if is_compare(code.get(pc + 1)) {
        return Some((pc + 1, QuotientOperand::Rhs));
    }
    (is_plain_push(code.get(pc + 1))
        && is_compare(code.get(pc + 2))
        && !is_block_start(blocks, pc + 2))
    .then_some((pc + 2, QuotientOperand::Lhs))
}

/// The inverse of [`quotient_compare`]: which operand of the comparison at
/// `pc` is a fused Pine integer quotient, if either.
pub(crate) fn quotient_compare_operand(
    code: &[CsilInstr],
    blocks: &[Block],
    pc: usize,
) -> Option<QuotientOperand> {
    [1usize, 2].into_iter().find_map(|back| {
        let division = pc.checked_sub(back)?;
        match quotient_compare(code, blocks, division) {
            Some((compare, operand)) if compare == pc => Some(operand),
            _ => None,
        }
    })
}

/// How the instruction after a `PineIntDiv` turns its quotient into a genuine
/// integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuotientRounding {
    /// `math.round`: half away from zero.
    Nearest,
    /// `math.floor`.
    Floor,
    /// `math.ceil`.
    Ceil,
    /// `int()`: toward zero.
    Truncate,
}

/// The one-argument rounding or integer cast that reads the `PineIntDiv`
/// result at `pc` as the very next instruction. The two are fused: the
/// division emits the integer the call would answer (an exact quotient
/// unchanged, an uneven one divided in f64 and rounded, like the
/// interpreter), and the call that follows is the identity on that integer.
///
/// Another path that jumps to the call brings its own integer cell, on which
/// the call is the identity too, so the call need not be inside the
/// division's basic block.
pub(crate) fn quotient_rounding(code: &[CsilInstr], pc: usize) -> Option<QuotientRounding> {
    if !matches!(code.get(pc), Some(CsilInstr::PineIntDiv)) {
        return None;
    }
    match code.get(pc + 1) {
        Some(CsilInstr::CallPure { func, argc: 1 }) => match func {
            PureFunc::MathRound => Some(QuotientRounding::Nearest),
            PureFunc::MathFloor => Some(QuotientRounding::Floor),
            PureFunc::MathCeil => Some(QuotientRounding::Ceil),
            PureFunc::CastInt => Some(QuotientRounding::Truncate),
            _ => None,
        },
        _ => None,
    }
}

/// User slots that hold a Pine integer quotient as its numeric value in the
/// f64 lane.
///
/// `PINE_INT_DIV` leaves an exact quotient as an int and an uneven one as a
/// float, so the slot it is stored to holds either. `IntToF64` maps both to
/// the same float, and the f64 image of an int cell is what that conversion
/// would produce anyway. A slot therefore qualifies when it receives a
/// quotient (directly, or copied from a qualifying slot) and every read of
/// it, at any history offset, is converted at once or copied at once to
/// another qualifying slot: the slot then keeps the converted value, an
/// integer store converts on the way in, and the conversion after a read is
/// the identity.
///
/// Anything else that can see the cell disqualifies the slot: a dynamic
/// history read, a window scan, linked TA state, a read from another
/// function, a read whose conversion another path can jump to, and `varip`
/// storage.
fn quotient_slots(program: &CsilProgram, blocks: &[Block]) -> BTreeSet<u32> {
    let code = &program.main().code;
    let storable = |slot: u32| {
        program
            .series_slots
            .get(slot as usize)
            .is_some_and(|definition| !definition.varip)
    };
    let mut slots: BTreeSet<u32> = code
        .windows(2)
        .filter_map(|pair| match pair {
            [CsilInstr::PineIntDiv, CsilInstr::StoreUserSlotCurrent { slot }] => Some(*slot),
            _ => None,
        })
        .filter(|slot| storable(*slot))
        .collect();
    if slots.is_empty() {
        return slots;
    }
    // Every slot a quotient can be copied into, slot to slot.
    loop {
        let before = slots.len();
        for pair in code.windows(2) {
            if let [CsilInstr::ReadUserSlot { slot: from, .. }, CsilInstr::StoreUserSlotCurrent { slot: to }] =
                pair
            {
                if slots.contains(from) && storable(*to) {
                    slots.insert(*to);
                }
            }
        }
        if slots.len() == before {
            break;
        }
    }
    for binding in &program.ta_input_bindings {
        if let pinewasm_hir::TaInputTarget::Sma { slot }
        | pinewasm_hir::TaInputTarget::Change { slot } = binding.target
        {
            slots.remove(&slot);
        }
    }
    // Drop every slot with a reader that could tell an int from a float,
    // until the rest only feed conversions and each other.
    loop {
        let before = slots.len();
        for (index, function) in program.functions.iter().enumerate() {
            for (pc, instr) in function.code.iter().enumerate() {
                match instr {
                    CsilInstr::ReadUserSlot { slot, .. } if slots.contains(slot) => {
                        let next = if index == 0 { code.get(pc + 1) } else { None };
                        let unobservable = match next {
                            Some(CsilInstr::IntToF64) => !is_block_start(blocks, pc + 1),
                            Some(CsilInstr::StoreUserSlotCurrent { slot: to }) => {
                                slots.contains(to)
                            }
                            _ => false,
                        };
                        if !unobservable {
                            slots.remove(slot);
                        }
                    }
                    CsilInstr::ReadUserSlotDynamic { slot }
                    | CsilInstr::WindowMean { slot, .. }
                    | CsilInstr::WindowMeanDynamic { slot }
                    | CsilInstr::WindowExtremumDynamic { slot, .. }
                    | CsilInstr::WindowStatisticDynamic { slot, .. }
                    | CsilInstr::EmaSimpleDynamic { slot } => {
                        slots.remove(slot);
                    }
                    _ => {}
                }
            }
        }
        if slots.len() == before {
            break;
        }
    }
    slots
}

/// Map a validator abstract type onto a concrete lane type. `Any` has no
/// single concrete representation (`None` = demand-driven inference).
fn abstract_vty(ty: AbstractTy) -> Option<VTy> {
    match ty {
        AbstractTy::F64 => Some(VTy::F64),
        AbstractTy::Int => Some(VTy::I64),
        AbstractTy::Bool => Some(VTy::Bool3),
        AbstractTy::Str => Some(VTy::Str),
        AbstractTy::Color => Some(VTy::Color),
        AbstractTy::Ref => Some(VTy::Ref),
        AbstractTy::Any => None,
    }
}

fn vty_abstract(ty: VTy) -> AbstractTy {
    match ty {
        VTy::F64 => AbstractTy::F64,
        VTy::I64 => AbstractTy::Int,
        VTy::Bool3 => AbstractTy::Bool,
        VTy::Str => AbstractTy::Str,
        VTy::Color => AbstractTy::Color,
        VTy::Ref => AbstractTy::Ref,
    }
}

/// A basic block: instructions `[start, end)` where `code[end - 1]` is the
/// terminator (any instruction; non-branching ones fall through).
#[derive(Debug, Clone)]
pub struct Block {
    pub start: usize,
    pub end: usize,
}

/// Block terminator, in emission terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Term {
    /// Fall through to the next block in layout order.
    Fall(usize),
    Jump(usize),
    /// Two-way branch: `taken` when the runtime test says "jump", else
    /// `fall`. The test itself is emitted from the terminator instruction.
    Branch {
        taken: usize,
        fall: usize,
    },
    /// End of the per-bar program.
    Return,
}

pub struct Analysis {
    pub blocks: Vec<Block>,
    pub term: Vec<Term>,
    /// Reverse-postorder index per block (emission order).
    pub rpo: Vec<usize>,
    /// Immediate dominator per block (block 0 → itself).
    pub idom: Vec<usize>,
    /// Blocks that are targets of ≥1 back edge.
    pub loop_header: Vec<bool>,
    /// Blocks with ≥2 forward in-edges (need a wasm `block` label).
    pub merge: Vec<bool>,
    /// Stack depth at each instruction (before it executes).
    pub depth_at: Vec<u32>,
    /// Concrete type of each stack cell at each block ENTRY, by depth.
    pub entry_stack: Vec<Vec<VTy>>,
    /// Type of the value(s) each instruction pops/pushes, resolved.
    /// Indexed by pc: the operand types (deepest first) and result types.
    pub operand_tys: Vec<Vec<VTy>>,
    pub result_tys: Vec<Vec<VTy>>,
    /// Concrete storage type per user slot.
    pub slot_ty: Vec<VTy>,
    /// Instructions that execute on the interpreter host through the
    /// `host.vm` import (docs/aot.md §5): heap, string-content, effect,
    /// and request opcodes. `AlertCondition` is listed separately — it
    /// joins the host plan only when the program needs `host.vm` anyway,
    /// otherwise it keeps the lighter dedicated `host.alert` import.
    pub host_site: Vec<bool>,
    pub alertcondition_pcs: Vec<usize>,
    /// Native-heap plan (docs/aot.md §13); `Some` = collection/UDT
    /// opcodes lower to linear memory and are absent from `host_site`.
    pub heap: Option<HeapPlan>,
    /// Whether the program contains collection/UDT opcodes at all (drives
    /// the `vm_heap_residual` layout fact when `heap` is `None`).
    pub heap_ops_present: bool,
    /// Native-heap sites inside natural loops (the run-path policy's
    /// compute-heavy signal; 0 when `heap` is `None`).
    pub heap_loop_sites: u32,
    /// Host sites inside natural loops: each may run once per loop
    /// iteration, where every other host site runs at most once a bar.
    pub host_loop_sites: u32,
    /// Sites whose `I64` result the static facts could not clear of a genuine
    /// `i64::MIN`, with the runtime guard the emitter places on each. Empty
    /// for every program the static gate admits on its own.
    pub int_min_guards: BTreeMap<usize, IntMinGuard>,
    /// Where a `strategy()` program's broker runs; `None` for an indicator.
    pub(crate) strategy_broker: Option<StrategyBroker>,
    /// The module's host is the interpreter linked into it, which writes
    /// each string and color input's cell into the input table.
    pub(crate) linked_host: bool,
    /// Two string handles of the module are equal exactly when their
    /// strings are (see `InferenceMode::canonical_strings`).
    pub(crate) canonical_strings: bool,
}

/// Instruction cost in interpreter budget units (1 per dispatch; window
/// scanners cost their window; `CallTa` costs `engine_cost`).
pub fn instr_cost(instr: &CsilInstr, ta_states: &[TaStateDecl]) -> u64 {
    match instr {
        CsilInstr::WindowMean { length, .. } => u64::from(*length).max(1),
        CsilInstr::CallTa { state } => ta_states
            .get(*state as usize)
            .map_or(1, |d| d.func.engine_cost(&d.params).max(1)),
        _ => 1,
    }
}

fn unsupported(instr: &CsilInstr) -> AotError {
    let dbg = format!("{instr:?}");
    AotError::UnsupportedOpcode(dbg.split([' ', '(', '{']).next().unwrap_or("?").to_string())
}

/// Control-transfer class of an instruction (the CSIL-facing half of block
/// splitting; the rest is [`cfg::cfg_blocks`]).
fn ctrl_of(instr: &CsilInstr) -> cfg::Ctrl {
    match instr {
        CsilInstr::Jump { target } => cfg::Ctrl::Jump(*target as usize),
        CsilInstr::JumpIfFalse { target }
        | CsilInstr::JumpIfExact { target, .. }
        | CsilInstr::JumpIfSlotWritten { target, .. } => cfg::Ctrl::Branch(*target as usize),
        CsilInstr::Return => cfg::Ctrl::Return,
        _ => cfg::Ctrl::Fall,
    }
}

/// Whether `pc` belongs to a collection operation owned by the native heap.
/// Keep the lookup branch explicit for the Aeneas translator.
/// Clippy's `matches!` rewrite is rejected by that translator.
#[allow(clippy::match_like_matches_macro)] // Aeneas needs this enum match explicit.
fn native_heap_int_collection_site(heap: Option<&HeapPlan>, pc: usize) -> bool {
    let Some(plan) = heap else {
        return false;
    };
    let Some((op, _)) = plan.coll.get(&pc) else {
        return false;
    };
    match op {
        NativeCollOp::Get
        | NativeCollOp::Shift
        | NativeCollOp::PopLast
        | NativeCollOp::First
        | NativeCollOp::Last
        | NativeCollOp::Push
        | NativeCollOp::Set
        | NativeCollOp::Clear => true,
        _ => false,
    }
}

/// Return true when this instruction can create an integer value that the
/// current single-cell AOT lane cannot distinguish from `na`.
///
/// Until the emitter carries a validity side lane, native analysis admits only
/// sources with a proof that they cannot produce a valid `i64::MIN`. Copies
/// and checked operations that cannot create MIN are safe when their inputs
/// came from the admitted subset. Unknown, host-backed, and stateful producers
/// fail closed. `heap` is the native-heap plan the typing used, if any; only its
/// native array sites are admitted.
fn can_produce_valid_i64_min(
    program: &CsilProgram,
    pc: usize,
    instr: &CsilInstr,
    results: &[VTy],
    inputs: IntInputBoundary,
    heap: Option<&HeapPlan>,
) -> bool {
    can_produce_valid_i64_min_from_slices(
        &program.main().code,
        &program.inputs,
        &program.ta_states,
        pc,
        instr,
        results,
        inputs,
        heap,
    )
}

// Aeneas's symbolic translation failed when these
// explicit enum matches were expressed with `matches!`; keep them visible to
// the extraction. The separate slices avoid the rejected CsilProgram borrow.
// Clippy's `matches!` rewrite would reintroduce that extraction failure.
#[allow(clippy::match_like_matches_macro)] // Preserve the Aeneas-compatible explicit matches.
#[allow(clippy::too_many_arguments)] // Separate slices avoid Aeneas context aliasing.
fn can_produce_valid_i64_min_from_slices(
    code: &[CsilInstr],
    input_decls: &[pinewasm_hir::InputDecl],
    ta_states: &[pinewasm_hir::TaStateDecl],
    pc: usize,
    instr: &CsilInstr,
    results: &[VTy],
    inputs: IntInputBoundary,
    heap: Option<&HeapPlan>,
) -> bool {
    let mut has_i64_result = false;
    let mut result_index = 0;
    while result_index < results.len() && !has_i64_result {
        has_i64_result = results[result_index] == VTy::I64;
        result_index += 1;
    }
    if !has_i64_result {
        return false;
    }
    match instr {
        CsilInstr::PushNa
        | CsilInstr::ReadUserSlot { .. }
        | CsilInstr::ReadUserSlotDynamic { .. }
        | CsilInstr::Dup
        | CsilInstr::Select
        | CsilInstr::IntNeg
        | CsilInstr::IntDiv
        | CsilInstr::IntRem => false,
        CsilInstr::CallPure {
            func: PureFunc::MathRound,
            argc: 1,
        } if pc > 0
            && match code[pc - 1] {
                CsilInstr::PineIntDiv => true,
                _ => false,
            } =>
        {
            false
        }
        CsilInstr::CallPure {
            func:
                // Selection cannot create a new integer; operand producers
                // remain subject to this same MIN-sentinel safety gate.
                PureFunc::Nz
                | PureFunc::MathAbs
                | PureFunc::MathSign
                | PureFunc::MathMin
                | PureFunc::MathMax
                | PureFunc::StrLength
                | PureFunc::StrPos
                | PureFunc::ColorR
                | PureFunc::ColorG
                | PureFunc::ColorB
                | PureFunc::ColorT,
            ..
        } => false,
        // Trade bar indices are u32 bar positions widened to i64; the blob
        // returns MIN only as its na. Trade times stay conservative, like the
        // `time` builtin.
        CsilInstr::CallPure {
            func:
                PureFunc::StrategyOpenTrade(field) | PureFunc::StrategyClosedTrade(field),
            ..
        } => match field {
            TradeAccessor::EntryBarIndex | TradeAccessor::ExitBarIndex => false,
            _ => true,
        },
        CsilInstr::CallTa { state } => match ta_states.get(*state as usize) {
            None => true,
            Some(decl) => match decl.func {
                pinewasm_ta::TaFunc::HighestBars
                | pinewasm_ta::TaFunc::LowestBars
                | pinewasm_ta::TaFunc::BarsSince
                | pinewasm_ta::TaFunc::ValueWhenInt => false,
                _ => true,
            },
        },
        CsilInstr::ReadInput { input } => {
            // Preserve `iter().find()`'s first-match behavior for duplicate
            // ids without a slice iterator closure in the AOT extraction path.
            let mut input_index = 0;
            while input_index < input_decls.len() && input_decls[input_index].id != *input
            {
                input_index += 1;
            }
            if input_index == input_decls.len() {
                return true;
            }
            let kind = input_decls[input_index].kind;
            match kind {
                InputKind::Enum => false,
                // The standalone runtime rejects a MIN int/time parameter
                // before `run()`, so only the compiled-in default can reach
                // the read.
                InputKind::Int | InputKind::Time
                    if inputs == IntInputBoundary::RejectsSentinel =>
                {
                    match &input_decls[input_index].defval {
                        InputValue::Int(value) => *value == i64::MIN,
                        _ => false,
                    }
                }
                InputKind::Int | InputKind::Time => {
                    let min_excludes_sentinel = match &input_decls[input_index].constraints.min {
                        Some(InputValue::Int(min)) => *min > i64::MIN,
                        _ => false,
                    };
                    let options_len = input_decls[input_index].constraints.options.len();
                    let mut options_exclude_sentinel = options_len != 0;
                    let mut option_index = 0;
                    while option_index < options_len && options_exclude_sentinel {
                        options_exclude_sentinel = match &input_decls[input_index].constraints.options[option_index] {
                            InputValue::Int(value) => *value != i64::MIN,
                            _ => true,
                        };
                        option_index += 1;
                    }
                    !(min_excludes_sentinel || options_exclude_sentinel)
                }
                _ => false,
            }
        }
        CsilInstr::CallCollection {
            func:
                pinewasm_hir::CollectionFunc::ArraySize
                | pinewasm_hir::CollectionFunc::MapSize
                | pinewasm_hir::CollectionFunc::MatrixRows
                | pinewasm_hir::CollectionFunc::MatrixColumns,
            ..
        } => false,
        // Array sites that lower to the native heap. Native arrays
        // are written only from CSIL operands: the fill of `NewArray`, the
        // arguments of `FromArgs`, and the values of `Push` and `Set`. The
        // host never writes native heap memory, so every stored integer came
        // from a producer this gate already checked, and an element read
        // (`Get`, `Shift`, `PopLast`, `First`, `Last`) returns one of those
        // values or an `na` fill. The void mutators (`Push`, `Set`, `Clear`)
        // push only the lane's na constant as their placeholder, which the
        // element lane types I64 for an `array<int>`. `host.vm` collection
        // sites stay refused: VM builtins (lower-timeframe requests, matrix
        // results) fill elements with no CSIL producer to gate.
        CsilInstr::CallCollection { .. }
            if native_heap_int_collection_site(heap, pc) =>
        {
            false
        }
        // A field read, like a slot read, only returns a value stored earlier.
        // Every integer that reaches an object field is a CSIL operand of
        // `NewObject` (explicit arguments, and UDT/drawing defaults, which
        // semantics materializes as ordinary const pushes) or `SetField`, so
        // its producer already passed this gate; `CopyObject` copies such
        // cells verbatim. Omitted and `na` fields are `PushNa`, the sentinel
        // itself, so they read back as `na`. The VM's own objects
        // (footprints and their rows) carry only float and bool fields, and
        // the `host.vm` boundary refuses an `Int(i64::MIN)` it would encode.
        CsilInstr::GetField { .. } => false,
        CsilInstr::PushI64(value) => *value == i64::MIN,
        CsilInstr::ReadBuiltin { source, .. } | CsilInstr::ReadBuiltinDynamic { source } => {
            match source {
                BuiltinSeries::BarIndex
                | BuiltinSeries::LastBarIndex
                | BuiltinSeries::DayOfWeek
                | BuiltinSeries::DayOfMonth
                | BuiltinSeries::Month
                | BuiltinSeries::Year
                | BuiltinSeries::Hour
                | BuiltinSeries::Minute
                | BuiltinSeries::Second
                | BuiltinSeries::WeekOfYear
                // Broker trade counts and the first retained index are
                // non-negative; MIN is only the snapshot read's na.
                | BuiltinSeries::StrategyWinTrades
                | BuiltinSeries::StrategyLossTrades
                | BuiltinSeries::StrategyEvenTrades
                | BuiltinSeries::StrategyClosedTrades
                | BuiltinSeries::StrategyOpenTrades
                | BuiltinSeries::StrategyClosedTradesFirstIndex => false,
                _ => true,
            }
        }
        // The range pass below admits these only when it can prove their
        // reachable operands cannot produce MIN.
        CsilInstr::IntAdd | CsilInstr::IntSub | CsilInstr::IntMul => false,
        // If the inputs cannot be MIN, the exact integer quotient cannot be
        // MIN either (MIN / 1 is the only possible such quotient).
        CsilInstr::PineIntDiv => false,
        // All other integer-producing instructions are not proven to exclude
        // MIN: user-controlled inputs, float-to-int conversions, host-backed
        // values, collection elements, and TA outputs land here.
        _ => true,
    }
}

/// Abstract fact about one stack cell for the integer-minimum range pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IntFact {
    NonInt,
    NaOnly,
    Range(i64, i64),
    Unknown,
}

impl IntFact {
    fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::NaOnly, fact) | (fact, Self::NaOnly) => fact,
            (Self::Range(a, b), Self::Range(c, d)) => {
                Self::Range(if a <= c { a } else { c }, if b >= d { b } else { d })
            }
            (Self::NonInt, Self::NonInt) => Self::NonInt,
            _ => Self::Unknown,
        }
    }
}

/// The three checked integer operations the range pass transfers exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IntArith {
    Add,
    Sub,
    Mul,
}

/// Straight-line view of one CSIL instruction for [`int_fact_step`]: only the
/// distinctions the integer-minimum transfer makes. [`int_op`] computes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IntOp {
    /// `PushNa`: an integer-typed `na` is `NaOnly`, anything else `NonInt`.
    PushNa,
    /// Exactly one pushed value with a fixed closed range: an integer literal,
    /// a bounded builtin series, a string length/position or a size query.
    PushRange(i64, i64),
    /// Exactly one pushed value whose fact comes from its result type (an
    /// unbounded builtin series).
    PushTyped,
    Dup,
    Select,
    Arith(IntArith),
    Neg,
    DivRem,
    /// Every other instruction: one type-derived fact per pushed value.
    Other,
}

/// Classify one instruction for the range pass (the CSIL side of the kernel).
fn int_op(instr: &CsilInstr) -> IntOp {
    match instr {
        CsilInstr::PushNa => IntOp::PushNa,
        CsilInstr::PushI64(value) => IntOp::PushRange(*value, *value),
        CsilInstr::ReadBuiltin { source, .. } | CsilInstr::ReadBuiltinDynamic { source } => {
            match source {
                BuiltinSeries::BarIndex => IntOp::PushRange(0, i32::MAX as i64),
                // Empty runs execute no script instructions; a present last
                // index is bars - 1 with a nonzero unsigned native bar count.
                BuiltinSeries::LastBarIndex => IntOp::PushRange(0, u32::MAX as i64),
                BuiltinSeries::DayOfWeek => IntOp::PushRange(1, 7),
                BuiltinSeries::DayOfMonth => IntOp::PushRange(1, 31),
                BuiltinSeries::Month => IntOp::PushRange(1, 12),
                // Every accepted timestamp is an i64 epoch-millisecond
                // value; its civil year is bounded well inside this
                // deliberately wide interval.
                BuiltinSeries::Year => IntOp::PushRange(-1_000_000, 1_000_000),
                BuiltinSeries::Hour => IntOp::PushRange(0, 23),
                BuiltinSeries::Minute | BuiltinSeries::Second => IntOp::PushRange(0, 59),
                BuiltinSeries::WeekOfYear => IntOp::PushRange(1, 53),
                _ => IntOp::PushTyped,
            }
        }
        CsilInstr::Dup => IntOp::Dup,
        CsilInstr::Select => IntOp::Select,
        CsilInstr::CallPure {
            func: PureFunc::StrLength | PureFunc::StrPos,
            ..
        } => IntOp::PushRange(-1, i64::MAX),
        CsilInstr::CallCollection {
            func:
                pinewasm_hir::CollectionFunc::ArrayPush
                | pinewasm_hir::CollectionFunc::ArraySet
                | pinewasm_hir::CollectionFunc::ArrayClear,
            ..
        } => IntOp::PushNa,
        CsilInstr::CallCollection {
            func:
                pinewasm_hir::CollectionFunc::ArraySize
                | pinewasm_hir::CollectionFunc::MapSize
                | pinewasm_hir::CollectionFunc::MatrixRows
                | pinewasm_hir::CollectionFunc::MatrixColumns,
            ..
        } => IntOp::PushRange(0, i64::MAX),
        CsilInstr::IntAdd => IntOp::Arith(IntArith::Add),
        CsilInstr::IntSub => IntOp::Arith(IntArith::Sub),
        CsilInstr::IntMul => IntOp::Arith(IntArith::Mul),
        CsilInstr::IntNeg => IntOp::Neg,
        CsilInstr::IntDiv | CsilInstr::IntRem | CsilInstr::PineIntDiv => IntOp::DivRem,
        _ => IntOp::Other,
    }
}

/// Whether result type `index` of the instruction is the i64 lane.
fn ty_is_i64(tys: &[VTy], index: usize) -> bool {
    index < tys.len() && matches!(tys[index], VTy::I64)
}

/// The fact of an otherwise unknown pushed value: `Unknown` on the i64 lane.
fn fact_from_type(tys: &[VTy], index: usize) -> IntFact {
    if ty_is_i64(tys, index) {
        IntFact::Unknown
    } else {
        IntFact::NonInt
    }
}

/// `popped[index]`, or `Unknown` when the instruction popped fewer values.
fn popped_at(popped: &[IntFact], index: usize) -> IntFact {
    if index < popped.len() {
        popped[index]
    } else {
        IntFact::Unknown
    }
}

fn min_i128(a: i128, b: i128) -> i128 {
    if a <= b {
        a
    } else {
        b
    }
}

fn max_i128(a: i128, b: i128) -> i128 {
    if a >= b {
        a
    } else {
        b
    }
}

/// Guard an exact result interval `[lo, hi]` (in i128, before narrowing).
/// `None` means a valid `i64::MIN` is possible (the ambiguity); an interval
/// wholly outside i64 overflows to `na` on every input; otherwise the
/// representable, non-sentinel slice.
fn guard_bounds(lo: i128, hi: i128) -> Option<IntFact> {
    let min = i64::MIN as i128;
    let max = i64::MAX as i128;
    if lo <= min && hi >= min {
        return None;
    }
    if hi < min || lo > max {
        Some(IntFact::NaOnly)
    } else if lo < min {
        // The interval straddles the minimum, so a valid minimum is possible
        // even if other combinations overflow.
        None
    } else {
        Some(IntFact::Range(
            max_i128(lo, min + 1) as i64,
            min_i128(hi, max) as i64,
        ))
    }
}

/// Transfer for `IntAdd`/`IntSub`/`IntMul`; `None` is the ambiguity.
fn arith_fact(op: IntArith, lhs: IntFact, rhs: IntFact) -> Option<IntFact> {
    match (lhs, rhs) {
        (IntFact::NaOnly, _) | (_, IntFact::NaOnly) => Some(IntFact::NaOnly),
        (IntFact::Range(a, b), IntFact::Range(c, d)) => {
            let (a, b, c, d) = (a as i128, b as i128, c as i128, d as i128);
            match op {
                IntArith::Add => guard_bounds(a + c, b + d),
                IntArith::Sub => guard_bounds(a - d, b - c),
                IntArith::Mul => {
                    let (ac, ad, bc, bd) = (a * c, a * d, b * c, b * d);
                    guard_bounds(
                        min_i128(min_i128(ac, ad), min_i128(bc, bd)),
                        max_i128(max_i128(ac, ad), max_i128(bc, bd)),
                    )
                }
            }
        }
        _ => None,
    }
}

/// Transfer for `IntNeg`; `None` is the ambiguity.
fn neg_fact(operand: IntFact) -> Option<IntFact> {
    match operand {
        IntFact::NaOnly => Some(IntFact::NaOnly),
        IntFact::Range(min, max) => {
            let output_min = -(max as i128);
            let output_max = -(min as i128);
            if output_min <= i64::MIN as i128 {
                None
            } else {
                Some(IntFact::Range(output_min as i64, output_max as i64))
            }
        }
        _ => Some(IntFact::Unknown),
    }
}

/// One step of the straight-line integer-minimum range pass over the fact
/// stack: pop `pops` facts, push the transferred facts. Returns `true` when
/// the step may create a valid `i64::MIN` (or the stack underflows), which
/// sends the program to the VM tier. A closure-free kernel over a plain stack
/// `Vec` (extracted by Charon/Aeneas).
pub(crate) fn int_fact_step(
    stack: &mut Vec<IntFact>,
    op: IntOp,
    pops: usize,
    pushes: usize,
    tys: &[VTy],
) -> bool {
    if stack.len() < pops {
        return true;
    }
    let mut popped: Vec<IntFact> = Vec::new();
    let mut i = 0;
    while i < pops {
        match stack.pop() {
            Some(fact) => popped.push(fact),
            None => return true,
        }
        i += 1;
    }
    match op {
        IntOp::PushNa => stack.push(if ty_is_i64(tys, 0) {
            IntFact::NaOnly
        } else {
            IntFact::NonInt
        }),
        IntOp::PushRange(lo, hi) => stack.push(IntFact::Range(lo, hi)),
        IntOp::PushTyped => stack.push(fact_from_type(tys, 0)),
        IntOp::Dup => {
            let top = popped_at(&popped, 0);
            stack.push(top);
            stack.push(top);
        }
        IntOp::Select => {
            let rhs = popped_at(&popped, 1);
            let lhs = popped_at(&popped, 2);
            stack.push(lhs.combine(rhs));
        }
        IntOp::Arith(arith) => {
            let lhs = popped_at(&popped, 1);
            let rhs = popped_at(&popped, 0);
            match arith_fact(arith, lhs, rhs) {
                Some(fact) => stack.push(fact),
                None => return true,
            }
        }
        IntOp::Neg => match neg_fact(popped_at(&popped, 0)) {
            Some(fact) => stack.push(fact),
            None => return true,
        },
        IntOp::DivRem => {
            let lhs = popped_at(&popped, 1);
            let rhs = popped_at(&popped, 0);
            // Checked division cannot create MIN unless the input is already
            // MIN (which the producer gate rejects); the exact quotient or
            // remainder range is not needed unless a later add/subtract/
            // multiply tries to use it.
            stack.push(
                if matches!(lhs, IntFact::NaOnly) || matches!(rhs, IntFact::NaOnly) {
                    IntFact::NaOnly
                } else {
                    IntFact::Unknown
                },
            );
        }
        IntOp::Other => {
            let mut index = 0;
            while index < pushes {
                stack.push(fact_from_type(tys, index));
                index += 1;
            }
        }
    }
    false
}

/// Prove that reachable integer producers cannot create the sentinel value.
///
/// Non-arithmetic producers are admitted only where their domain is fixed and
/// bounded. Arithmetic may keep native lowering when a block-local stack
/// interpretation obtains finite operand ranges; values crossing control
/// flow and unknown operands conservatively select the VM tier.
fn integer_min_ambiguity(
    program: &CsilProgram,
    results: &[Vec<VTy>],
    inputs: IntInputBoundary,
    heap: Option<&HeapPlan>,
) -> Option<usize> {
    integer_min_ambiguity_with_fields(
        program,
        results,
        inputs,
        heap,
        &BTreeMap::new(),
        &BTreeSet::new(),
    )
}

fn integer_min_ambiguity_with_fields(
    program: &CsilProgram,
    results: &[Vec<VTy>],
    inputs: IntInputBoundary,
    heap: Option<&HeapPlan>,
    fields: &BTreeMap<usize, IntFact>,
    identities: &BTreeSet<usize>,
) -> Option<usize> {
    let code = &program.main().code;
    let input_decls = &program.inputs;
    let ta_states = &program.ta_states;
    if let Some(pc) = first_min_producer(
        code,
        input_decls,
        ta_states,
        results,
        inputs,
        heap,
        fields,
        identities,
    ) {
        return Some(pc);
    }
    block_local_min_ambiguity(code, ta_states, results, fields, identities)
}

/// The producer gate intentionally runs before the block-local stack pass.
/// Keep this scan in its own function: an early return in this loop must not
/// precede the range-pass loop in the same Aeneas extraction root.
// Separate code/input/state slices avoid Aeneas's context-matching failure.
#[allow(clippy::too_many_arguments)] // Keep the borrowed slices disjoint for extraction.
fn first_min_producer(
    code: &[CsilInstr],
    input_decls: &[pinewasm_hir::InputDecl],
    ta_states: &[pinewasm_hir::TaStateDecl],
    results: &[Vec<VTy>],
    inputs: IntInputBoundary,
    heap: Option<&HeapPlan>,
    fields: &BTreeMap<usize, IntFact>,
    identities: &BTreeSet<usize>,
) -> Option<usize> {
    // This is `code.iter().zip(results).enumerate()` with an explicit bound.
    // Keep the truncation when `results` is shorter than the instruction list.
    let limit = code.len().min(results.len());
    let mut pc = 0;
    while pc < limit {
        let instr = &code[pc];
        let tys = &results[pc];
        if !identities.contains(&pc)
            && !fields.contains_key(&pc)
            && can_produce_valid_i64_min_from_slices(
                code,
                input_decls,
                ta_states,
                pc,
                instr,
                tys,
                inputs,
                heap,
            )
        {
            return Some(pc);
        }
        pc += 1;
    }
    None
}

/// Mark one in-range block leader while returning the vector as an explicit
/// state value for Aeneas's loop translation. Out-of-range jump targets are
/// ignored, matching the original `Vec::get_mut` branch.
fn mark_block_leader(mut leader: Vec<bool>, target: usize) -> Vec<bool> {
    if target < leader.len() {
        leader[target] = true;
    }
    leader
}

fn block_leader_target(instr: &CsilInstr) -> Option<usize> {
    match instr {
        CsilInstr::Jump { target }
        | CsilInstr::JumpIfFalse { target }
        | CsilInstr::JumpIfExact { target, .. }
        | CsilInstr::JumpIfSlotWritten { target, .. } => Some(*target as usize),
        _ => None,
    }
}

/// Run the block-local abstract stack pass after the producer pre-pass has
/// found no unconditional ambiguity. The loop's early failure returns are
/// isolated from the producer loop for the Aeneas translation.
fn block_local_min_ambiguity(
    code: &[CsilInstr],
    ta_states: &[pinewasm_hir::TaStateDecl],
    results: &[Vec<VTy>],
    fields: &BTreeMap<usize, IntFact>,
    identities: &BTreeSet<usize>,
) -> Option<usize> {
    // The range pass is block-local: a basic block starts at every jump
    // target and after every jump, and any value it inherits from another
    // block (a join, a loop back edge) is `Unknown`. Arithmetic over operands
    // produced inside the same block (`bar_index + 1`) keeps its exact
    // interval; anything flowing across control flow stays conservative.
    let mut leader = vec![false; code.len()];
    let mut pc = 0;
    while pc < code.len() {
        if let Some(target) = block_leader_target(&code[pc]) {
            leader = mark_block_leader(leader, target);
            leader = mark_block_leader(leader, pc + 1);
        }
        pc += 1;
    }

    let mut stack = Vec::<IntFact>::new();
    // Straight-line code has no inherited operands, so an underflow there is
    // still a malformed stack (and fails closed inside `int_fact_step`).
    let mut after_leader = false;
    let mut pc = 0;
    while pc < code.len() {
        let instr = &code[pc];
        if leader[pc] {
            stack.clear();
            after_leader = true;
        }
        // Resolve through CSIL's shared stack contract. TA calls take their
        // arity from the declaration table; an unresolvable effect (such as a
        // malformed TA state index) fails closed to the VM tier.
        let Ok(effect) = instr.resolve_stack_effect(ta_states) else {
            return Some(pc);
        };
        let tys: &[VTy] = if pc < results.len() {
            &results[pc]
        } else {
            &[]
        };
        // Operands inherited from a predecessor block are unknown.
        let pops = effect.pops as usize;
        if after_leader && stack.len() < pops {
            let missing = pops - stack.len();
            stack.splice(0..0, std::iter::repeat_n(IntFact::Unknown, missing));
        }
        if identities.contains(&pc) {
            // Inferred native integer argument/result, one-argument form:
            // floor/ceil/round/int neither converts nor creates a value.
            // Keep the operand's range (or missing state) through the call.
            if effect.pops != 1 || effect.pushes != 1 {
                return Some(pc);
            }
            let Some(fact) = stack.pop() else {
                return Some(pc);
            };
            stack.push(fact);
            pc += 1;
            continue;
        }
        if int_fact_step(
            &mut stack,
            match fields.get(&pc) {
                Some(IntFact::Range(lo, hi)) => IntOp::PushRange(*lo, *hi),
                Some(IntFact::NaOnly) => IntOp::PushNa,
                _ => int_op(instr),
            },
            effect.pops as usize,
            effect.pushes as usize,
            tys,
        ) {
            return Some(pc);
        }
        pc += 1;
    }
    None
}

/// The runtime guard the emitter places on one site that the static facts
/// could not clear of a genuine `i64::MIN`. In every case the guard separates
/// the operation's own `na` result from a present result equal to the sentinel
/// and stops the run with [`crate::status::ABI_MISMATCH`] on the latter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntMinGuard {
    /// `IntAdd`/`IntSub`/`IntMul`: `na` is an `na` operand or a checked
    /// overflow; a present result of `i64::MIN` is genuine.
    Arith,
    /// `math.floor`/`math.ceil`/`math.round`/`int` of a float: `na` is a
    /// rounded operand outside `[-2^63, 2^63)` or NaN; a rounded operand of
    /// exactly `-2^63` is the genuine minimum.
    FloatToInt,
    /// A host-written `input.int`/`input.time` cell: an input is never `na`,
    /// so a cell holding the sentinel is the genuine minimum.
    InputRead,
    /// `PushI64(i64::MIN)`: the literal is always the genuine minimum.
    Literal,
    /// `time`/`last_bar_time`: the lane holds each timestamp as an f64 and is
    /// never `na` inside the run, so the read is exact only for a magnitude
    /// below `2^53`. Anything else, the minimum included, stops the run.
    TimeLane,
    /// A linked `ta.*` call with an integer result: the engine answers in an
    /// f64 cell and NaN is `na`. Any other value outside `(-2^63, 2^63)`
    /// stops the run: at or below `-2^63` it converts to the genuine minimum,
    /// and at or above `2^63` the interpreter keeps a float. An integer
    /// `ta.valuewhen`/`fixnan` returns its source, so it also stops on a
    /// fraction, which the interpreter keeps as a float; every such call
    /// carries this guard, whether or not the static pass flagged it.
    TaResult,
    /// A `host.vm` site with an `I64` result cell. The interpreter host
    /// already answers a present `i64::MIN` with the same status instead of
    /// encoding it as `na`, so the emitter adds nothing.
    HostCell,
}

/// The guard for a producer [`can_produce_valid_i64_min`] flagged, or `None`
/// when that producer kind has no guard and keeps the whole-program refusal.
fn producer_guard(
    program: &CsilProgram,
    instr: &CsilInstr,
    operands: &[VTy],
    results: &[VTy],
    host_site: bool,
) -> Option<IntMinGuard> {
    if host_site {
        return (results == [VTy::I64]).then_some(IntMinGuard::HostCell);
    }
    match instr {
        CsilInstr::ReadBuiltin {
            source: BuiltinSeries::Time | BuiltinSeries::LastBarTime,
            ..
        }
        | CsilInstr::ReadBuiltinDynamic {
            source: BuiltinSeries::Time | BuiltinSeries::LastBarTime,
        } if results == [VTy::I64] => Some(IntMinGuard::TimeLane),
        CsilInstr::CallPure {
            func: PureFunc::MathFloor | PureFunc::MathCeil | PureFunc::MathRound | PureFunc::CastInt,
            argc: 1,
        } if operands == [VTy::F64] && results == [VTy::I64] => Some(IntMinGuard::FloatToInt),
        CsilInstr::ReadInput { input }
            if results == [VTy::I64]
                && program.inputs.iter().any(|decl| {
                    decl.id == *input && matches!(decl.kind, InputKind::Int | InputKind::Time)
                }) =>
        {
            Some(IntMinGuard::InputRead)
        }
        CsilInstr::PushI64(i64::MIN) if results == [VTy::I64] => Some(IntMinGuard::Literal),
        CsilInstr::CallTa { state }
            if results == [VTy::I64]
                && program.ta_states.get(*state as usize).is_some_and(|decl| {
                    decl.func.engine_result() == pinewasm_hir::TaResult::Int
                }) =>
        {
            Some(IntMinGuard::TaResult)
        }
        _ => None,
    }
}

/// Every site [`integer_min_ambiguity_with_fields`] could report, with its
/// runtime guard: the same producer gate and the same block-local range pass,
/// continued past each guardable site instead of stopping at the first.
///
/// A guarded result is `na` or a present integer other than the sentinel,
/// which is what `IntFact::Unknown` already denotes on the `I64` lane, so the
/// pass continues from that fact. `Err(pc)` keeps the refusal: a producer
/// kind with no guard, or a malformed stack or stack effect.
#[allow(clippy::too_many_arguments)]
fn integer_min_guard_sites(
    program: &CsilProgram,
    operands: &[Vec<VTy>],
    results: &[Vec<VTy>],
    host_site: &[bool],
    inputs: IntInputBoundary,
    heap: Option<&HeapPlan>,
    fields: &BTreeMap<usize, IntFact>,
    identities: &BTreeSet<usize>,
) -> Result<BTreeMap<usize, IntMinGuard>, usize> {
    let code = &program.main().code;
    let mut guards = BTreeMap::new();
    for (pc, (instr, tys)) in code.iter().zip(results).enumerate() {
        if !identities.contains(&pc)
            && !fields.contains_key(&pc)
            && can_produce_valid_i64_min(program, pc, instr, tys, inputs, heap)
        {
            let operand_tys = operands.get(pc).map(Vec::as_slice).unwrap_or_default();
            let hosted = host_site.get(pc).copied().unwrap_or(false);
            let guard = producer_guard(program, instr, operand_tys, tys, hosted).ok_or(pc)?;
            guards.insert(pc, guard);
        }
    }

    // The same block leaders as `integer_min_ambiguity_with_fields`.
    let mut leader = vec![false; code.len()];
    for (pc, instr) in code.iter().enumerate() {
        let target = match instr {
            CsilInstr::Jump { target }
            | CsilInstr::JumpIfFalse { target }
            | CsilInstr::JumpIfExact { target, .. }
            | CsilInstr::JumpIfSlotWritten { target, .. } => *target as usize,
            _ => continue,
        };
        if let Some(slot) = leader.get_mut(target) {
            *slot = true;
        }
        if let Some(slot) = leader.get_mut(pc + 1) {
            *slot = true;
        }
    }

    let mut stack = Vec::<IntFact>::new();
    let mut after_leader = false;
    for (pc, instr) in code.iter().enumerate() {
        if leader[pc] {
            stack.clear();
            after_leader = true;
        }
        let Ok(effect) = instr.resolve_stack_effect(&program.ta_states) else {
            return Err(pc);
        };
        let tys = results.get(pc).map(Vec::as_slice).unwrap_or_default();
        let pops = effect.pops as usize;
        let pushes = effect.pushes as usize;
        if after_leader && stack.len() < pops {
            let missing = pops - stack.len();
            stack.splice(0..0, std::iter::repeat_n(IntFact::Unknown, missing));
        }
        if identities.contains(&pc) {
            if effect.pops != 1 || effect.pushes != 1 {
                return Err(pc);
            }
            let Some(fact) = stack.pop() else {
                return Err(pc);
            };
            stack.push(fact);
            continue;
        }
        // A malformed stack stays a refusal; past this check the kernel
        // reports only a failed arithmetic or negation transfer.
        if stack.len() < pops {
            return Err(pc);
        }
        let base = stack.len() - pops;
        if int_fact_step(
            &mut stack,
            match fields.get(&pc) {
                Some(IntFact::Range(lo, hi)) => IntOp::PushRange(*lo, *hi),
                Some(IntFact::NaOnly) => IntOp::PushNa,
                _ => int_op(instr),
            },
            pops,
            pushes,
            tys,
        ) {
            if !matches!(
                instr,
                CsilInstr::IntAdd | CsilInstr::IntSub | CsilInstr::IntMul
            ) || tys != [VTy::I64]
            {
                return Err(pc);
            }
            guards.insert(pc, IntMinGuard::Arith);
            stack.truncate(base);
            stack.push(IntFact::Unknown);
        }
    }
    Ok(guards)
}

/// The control-flow facts of one function body.
struct CfgParts {
    blocks: Vec<Block>,
    term: Vec<Term>,
    block_of_pc: Vec<usize>,
    rpo_order: Vec<usize>,
    rpo: Vec<usize>,
    idom: Vec<usize>,
    loop_header: Vec<bool>,
    merge: Vec<bool>,
    in_loop: Vec<bool>,
}

/// Basic blocks, reverse postorder, dominators, loop headers, merges and
/// natural-loop membership of a non-empty body (the [`cfg`] kernel over the
/// [`ctrl_of`] classification).
fn recover_cfg(code: &[CsilInstr]) -> Result<CfgParts, AotError> {
    let ctrl: Vec<cfg::Ctrl> = code.iter().map(ctrl_of).collect();
    let cfg::BlockGraph {
        blocks,
        term,
        succ,
        block_of_pc,
    } = cfg::cfg_blocks(&ctrl).map_err(|e| match e {
        cfg::CfgError::TargetOutOfRange(s) => {
            AotError::InvalidProgram(format!("jump target {s} out of range"))
        }
        // A fall-through off the end of the function is invalid (validator
        // rejects it; defense in depth).
        cfg::CfgError::FallsOffEnd => {
            AotError::InvalidProgram("control flow runs past the end of the function".to_string())
        }
    })?;
    // Unreachable blocks (rpo NONE) are legal CSIL; the kernel skips them.
    // A retreating edge is reducible iff its target dominates its source (a
    // natural-loop back edge). Natural-loop membership feeds the run-path
    // policy: it wants to know whether the native heap is LOOP-bound — the
    // compute-heavy shape where native execution amortizes its per-run setup
    // and residual cold host hops (docs/aot.md §13).
    let shape = cfg::cfg_shape(&succ);
    if shape.irreducible {
        return Err(AotError::IrreducibleControlFlow);
    }
    let cfg::CfgShape {
        rpo_order,
        rpo,
        idom,
        loop_header,
        forward_in,
        in_loop,
        irreducible: _,
    } = shape;
    let merge: Vec<bool> = forward_in.iter().map(|&c| c >= 2).collect();

    Ok(CfgParts {
        blocks,
        term,
        block_of_pc,
        rpo_order,
        rpo,
        idom,
        loop_header,
        merge,
        in_loop,
    })
}

/// Who writes integer input cells, which decides whether an unbounded
/// `input.int`/`input.time` read can carry a valid `i64::MIN`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IntInputBoundary {
    /// A PineWASM-aware host writes the input table and is trusted only to
    /// honor the serialized `minval`/`options` constraints.
    HostConstraints,
    /// The standalone runtime's `params` overlay, which refuses a MIN
    /// int/time value with a request error before evaluation.
    RejectsSentinel,
}

/// Where a `strategy()` program's broker emulator runs, which decides how its
/// orders, `strategy.*` getters, trade accessors and conversions compile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StrategyBroker {
    /// Spliced into the module (the standalone WebAssembly tier): orders,
    /// numeric getters and numeric trade accessors lower onto the linked
    /// broker, and the rest refuse by name.
    Linked,
    /// The interpreter host's (the chart tier): every order, getter, trade
    /// accessor and conversion is a `host.vm` site, executed by the
    /// interpreter's own broker, which the host advances with the module's
    /// bars (`pinewasm_vm::aot_host`).
    Host,
}

/// What a module's host does for it, which decides the instructions that
/// stay host sites.
#[derive(Clone, Copy)]
struct InferenceMode {
    broker: StrategyBroker,
    /// The host is the interpreter linked into the module
    /// ([`analyze_chart`]), which writes each string and color input's
    /// cell into the input table before the run.
    linked_host: bool,
    /// The linked host gives every run string one handle and the constant
    /// pool holds no text twice, so two handles are equal exactly when
    /// their strings are.
    canonical_strings: bool,
    /// Full colors live in the linked host's color table, because a packed
    /// RGBT cell cannot hold this program's transparencies or keep which
    /// color input a value came from.
    host_colors: bool,
}

impl InferenceMode {
    /// A module whose host is an import (or absent): every string and
    /// color instruction that needs one is a host site.
    fn imported_host(broker: StrategyBroker) -> InferenceMode {
        InferenceMode {
            broker,
            linked_host: false,
            canonical_strings: false,
            host_colors: false,
        }
    }
}

/// Whether no two entries of the program's string constant pool hold the
/// same text. The compiler's own pool never does; an imported one may.
fn constants_are_distinct(program: &CsilProgram) -> bool {
    let mut seen = std::collections::BTreeSet::new();
    program
        .constants
        .iter()
        .all(|text| seen.insert(text.as_str()))
}

/// The `ta.*` engines whose int form returns its source operand: the
/// interpreter decodes their result as an int when it is whole and keeps a
/// fraction otherwise (`pinewasm_vm::ta::decode`), while native code returns
/// every [`pinewasm_hir::TaResult::Int`] through an `i64` lane. These two
/// carry the run-time result guard. The refusal of a possibly fractional
/// source covers more engines: see [`int_result_follows_source`].
pub(crate) fn returns_int_source(func: pinewasm_ta::TaFunc) -> bool {
    matches!(
        func,
        pinewasm_ta::TaFunc::ValueWhenInt | pinewasm_ta::TaFunc::FixnanInt
    )
}

/// The int-result `ta.*` engines whose result can be fractional when their
/// source is: native code returns every [`pinewasm_hir::TaResult::Int`]
/// through an `i64` lane, which cannot hold it.
///
/// * `ta.valuewhen` and `fixnan` hand the source back
///   ([`returns_int_source`]).
/// * `ta.mode` returns one of the source's values, and `ta.range` the
///   difference of two of them, so either can be fractional.
///
/// `ta.median` over an int source is left out on purpose. Its result is
/// decoded as an int by the interpreter and by native code alike, so the two
/// agree; an interpreter test pins that integer result. The remaining
/// int-result engines (`ta.barssince`, `ta.highestbars`, `ta.lowestbars`)
/// return counts.
pub(crate) fn int_result_follows_source(func: pinewasm_ta::TaFunc) -> bool {
    returns_int_source(func)
        || matches!(
            func,
            pinewasm_ta::TaFunc::ModeInt | pinewasm_ta::TaFunc::RangeInt
        )
}

/// Whether a value that may hold a fraction (a Pine `int / int` quotient, or
/// anything it reaches while staying statically int) is the source of an
/// engine of [`int_result_follows_source`].
///
/// The front end widens the int source with `IntToF64` before the call, so
/// `PineIntDiv; IntToF64; CallTa` is the very shape the typed pass lowers as
/// a fused widening, and a quotient slot's read is widened the same way.
/// Both are exact for a float consumer. This consumer returns an int-lane
/// result made from the number, and the `i64` result lane would truncate a
/// fractional source value, which the interpreter keeps as a float. Such
/// a program is refused by the name of the opcode that makes the fraction.
///
/// When the provenance analysis cannot describe the program, any quotient
/// beside such an engine refuses it.
fn quotient_reaches_int_result_engine(program: &CsilProgram) -> bool {
    use pinewasm_csil::{flow::Flow, fraction};
    let code = &program.main().code;
    let engine = |instr: &CsilInstr| match instr {
        CsilInstr::CallTa { state } => program
            .ta_states
            .get(*state as usize)
            .is_some_and(|decl| int_result_follows_source(decl.func)),
        _ => false,
    };
    if !code.iter().any(engine) || fraction::leaves_unchanged(code) {
        return false;
    }
    let Some(flow) = Flow::build(program) else {
        return true;
    };
    let Some(fractional) = fraction::fractional(program, &flow, |_| true) else {
        return true;
    };
    let may_hold_a_fraction = |producer: usize| {
        fractional.nodes[producer]
            || (matches!(code[producer], CsilInstr::IntToF64)
                && flow.operands[producer]
                    .iter()
                    .flatten()
                    .any(|&value| fractional.nodes[value]))
    };
    code.iter().enumerate().any(|(pc, instr)| {
        engine(instr)
            && flow.reached[pc]
            && flow.operands[pc]
                .iter()
                .flatten()
                .any(|&producer| may_hold_a_fraction(producer))
    })
}

/// The chart tier's analysis: a PineWASM-aware host writes the inputs, and a
/// `strategy()` program's broker is the host's.
pub fn analyze(program: &CsilProgram) -> Result<Analysis, AotError> {
    analyze_with(
        program,
        IntInputBoundary::HostConstraints,
        false,
        StrategyBroker::Host,
    )
}

/// [`analyze`] for a named input boundary and broker. `strategy_heap` offers
/// the native heap to a `strategy()` program whose broker is linked; only the
/// standalone emitter can honor that, by making the heap allocate from the
/// arena band the linked broker allocates from, so the two never cross (see
/// `heap::arena_alloc_fn`). A host broker leaves the module's memory to the
/// heap alone.
pub(crate) fn analyze_with(
    program: &CsilProgram,
    inputs: IntInputBoundary,
    strategy_heap: bool,
    broker: StrategyBroker,
) -> Result<Analysis, AotError> {
    analyze_mode(program, inputs, strategy_heap, broker, false)
}

/// [`analyze`] for a chart module whose host is the interpreter linked
/// into it (`compile_chart_wasm_program`). That host keeps full colors in a
/// run-local table, gives every run string one handle, and resolves string
/// and color inputs before the run, so a program with color inputs or
/// fractional transparencies lowers natively and its input reads and string
/// comparisons are not host sites.
pub(crate) fn analyze_chart(program: &CsilProgram) -> Result<Analysis, AotError> {
    analyze_mode(
        program,
        IntInputBoundary::HostConstraints,
        false,
        StrategyBroker::Host,
        true,
    )
}

fn analyze_mode(
    program: &CsilProgram,
    inputs: IntInputBoundary,
    strategy_heap: bool,
    broker: StrategyBroker,
    linked_chart: bool,
) -> Result<Analysis, AotError> {
    let code = &program.main().code;
    if code.is_empty() {
        return Err(AotError::NoFunctions);
    }

    // A packed RGBT cell holds every color of a program that passes both
    // checks. A linked host keeps the colors of one that does not.
    let packed_colors = color_precision::require_representable_transparency(program)
        .and_then(|()| color_precision::require_representable_identity(program));
    let host_colors = match packed_colors {
        Ok(()) => false,
        Err(_) if linked_chart => true,
        Err(error) => return Err(error),
    };
    let mode = if linked_chart {
        InferenceMode {
            broker,
            linked_host: true,
            canonical_strings: constants_are_distinct(program),
            host_colors,
        }
    } else {
        InferenceMode::imported_host(broker)
    };

    // ---- basic blocks, dominators, loops (the `cfg` kernel) ----------------
    let CfgParts {
        blocks,
        term,
        block_of_pc,
        rpo_order,
        rpo,
        idom,
        loop_header,
        merge,
        in_loop,
    } = recover_cfg(code)?;

    // An int-result engine whose result follows its source cannot take a value
    // that may hold a fraction: its result lane is `i64`.
    if quotient_reaches_int_result_engine(program) {
        return Err(AotError::UnsupportedOpcode("PineIntDiv".to_string()));
    }

    // ---- type inference -----------------------------------------------------
    let mut elem_facts =
        infer_collection_elems(program, &blocks, &term, &rpo_order, &BTreeMap::new());
    let heap_ops_present = elem_facts.gate.any_heap;
    let infer = TypeInference::new(program, &blocks, &term, &rpo_order, &elem_facts, None, mode)?;
    let typed_phase_c = infer.run()?;
    // Residual field reads need the same storage-side lane evidence as a
    // native heap. A field observed only by na() has no consumer lane hint.
    elem_facts.field_vty = stored_field_lanes(program, &elem_facts, &typed_phase_c);
    let typed_phase_c =
        TypeInference::new(program, &blocks, &term, &rpo_order, &elem_facts, None, mode)?.run()?;

    // Native heap (docs/aot.md §13): when the whole-program heap gate passes,
    // retype with the native heap. A type conflict under native typing (a
    // dynamically-mixed field the widening join cannot express) keeps the
    // proven host-site typing, never fails the compile.
    // A strategy program's linked broker allocates in the relocatable arena
    // band during `run()`, and the native heap's own `memory.grow` bump region would
    // cross that band. A strategy with a linked broker therefore gets the
    // native heap only from a caller that moves the heap's allocations into
    // the band too (`strategy_heap`); without it, heap opcodes in a strategy
    // stay host sites, which the standalone WebAssembly gate refuses by name.
    // A host broker allocates nothing in the module.
    let heap_allowed =
        program.metadata.strategy.is_none() || strategy_heap || broker == StrategyBroker::Host;
    let plan = if heap_allowed {
        plan_native_heap(program, &elem_facts, &typed_phase_c)
    } else {
        None
    };
    if std::env::var_os("PINEWASM_AOT_DEBUG_HEAP").is_some() {
        eprintln!(
            "[heap] any={} flow_ok={} plan={}",
            elem_facts.gate.any_heap,
            elem_facts.gate.flow_ok,
            plan.is_some()
        );
    }
    // A gate the first element pass failed may pass once `array.from` arrays
    // carry their lanes. Stable lane facts also refine residual host sites:
    // a color/string element must not fall back to an f64 result cell merely
    // because another heap operation cannot be lowered natively.
    let (elem_facts, typed_phase_c, plan) = match plan {
        None if heap_allowed && elem_facts.gate.any_heap => {
            match refine_from_lanes(program, &blocks, &term, &rpo_order, &typed_phase_c, mode) {
                Some((facts, typed, plan)) => (facts, typed, plan),
                None => (elem_facts, typed_phase_c, None),
            }
        }
        plan => (elem_facts, typed_phase_c, plan),
    };
    let (typed, heap) = if let Some(plan) = plan {
        match TypeInference::new(
            program,
            &blocks,
            &term,
            &rpo_order,
            &elem_facts,
            Some(&plan),
            mode,
        )
        .and_then(TypeInference::run)
        {
            Ok(native_typed) => {
                // The collector roots the slots the native typing gives a
                // reference type: the types the module's cells have.
                let mut plan = plan;
                plan.gc = gc_plan(program, &elem_facts, &native_typed, &plan.layouts);
                if std::env::var_os("PINEWASM_AOT_DEBUG_HEAP").is_some() {
                    match &plan.gc {
                        Some(gc) => eprintln!(
                            "[heap] collector: {} roots, {} traced types, hosted={}",
                            gc.roots.len(),
                            gc.traced.len(),
                            gc.hosted
                        ),
                        None => eprintln!("[heap] no collector: the module never collects"),
                    }
                }
                (native_typed, Some(plan))
            }
            Err(e) => {
                if std::env::var_os("PINEWASM_AOT_DEBUG_HEAP").is_some() {
                    eprintln!("[heap] native typing failed: {e}");
                }
                (typed_phase_c, None)
            }
        }
    } else {
        (typed_phase_c, None)
    };

    let mut field_ranges =
        integer_fields::proven_field_ranges(program, &blocks, &typed, heap.as_ref(), inputs);
    field_ranges.extend(float_integer_ranges::proven_ranges(
        program, &blocks, &typed,
    ));
    let identities: BTreeSet<_> = code
        .iter()
        .enumerate()
        .filter_map(|(pc, instr)| {
            (matches!(
                instr,
                CsilInstr::CallPure {
                    func: PureFunc::MathFloor
                        | PureFunc::MathCeil
                        | PureFunc::MathRound
                        | PureFunc::CastInt,
                    argc: 1,
                }
            ) && typed.operand_tys[pc].as_slice() == [VTy::I64]
                && typed.result_tys[pc].as_slice() == [VTy::I64])
            .then_some(pc)
        })
        .collect();
    let ambiguity = if field_ranges.is_empty() && identities.is_empty() {
        integer_min_ambiguity(program, &typed.result_tys, inputs, heap.as_ref())
    } else {
        integer_min_ambiguity_with_fields(
            program,
            &typed.result_tys,
            inputs,
            heap.as_ref(),
            &field_ranges,
            &identities,
        )
    };
    // A program the static facts clear needs no guard and lowers exactly as
    // before. Otherwise every ambiguous site gets a runtime guard, and only a
    // site with no guard keeps the refusal.
    let int_min_guards = if let Some(reported) = ambiguity {
        let guards = integer_min_guard_sites(
            program,
            &typed.operand_tys,
            &typed.result_tys,
            &typed.host_site,
            inputs,
            heap.as_ref(),
            &field_ranges,
            &identities,
        )
        .map_err(|pc| AotError::UnsupportedIntegerNaEncoding { pc })?;
        // The collector repeats the refusing pass, so it must guard the site
        // that pass reported. Keep the refusal if the two ever disagree.
        if !guards.contains_key(&reported) {
            return Err(AotError::UnsupportedIntegerNaEncoding { pc: reported });
        }
        guards
    } else {
        BTreeMap::new()
    };
    // An integer `ta.valuewhen`/`fixnan` is always guarded, whatever the
    // static pass decided: the engine holds the captured int in an f64 cell,
    // so a source within 512 of either end of the `i64` range comes back as
    // `-2^63` or `2^63`, and the interpreter keeps a fraction as a float. The
    // `I64` result lane holds none of those.
    let mut int_min_guards = int_min_guards;
    for (pc, instr) in code.iter().enumerate() {
        let CsilInstr::CallTa { state } = instr else {
            continue;
        };
        if program
            .ta_states
            .get(*state as usize)
            .is_some_and(|decl| returns_int_source(decl.func))
            && typed.result_tys[pc].as_slice() == [VTy::I64]
            && !typed.host_site[pc]
        {
            int_min_guards.entry(pc).or_insert(IntMinGuard::TaResult);
        }
    }

    let block_of = |pc: usize| block_of_pc[pc];

    // The alert output lane is sized `bars × alertcondition sites`, which is
    // exact only while each site fires at most once per bar. Pine keeps
    // `alertcondition` at global scope, so this is a guard against CSIL that
    // did not come from the Pine front end rather than a real restriction —
    // and it degrades to the interpreter-hosted tier, not to a wrong answer.
    if typed
        .alertcondition_pcs
        .iter()
        .any(|&pc| in_loop[block_of(pc)])
    {
        return Err(AotError::UnsupportedOpcode(
            "alertcondition inside a loop".into(),
        ));
    }

    // Loop-bound native-heap sites: the run-path policy's compute-heavy
    // signal (each executes once per loop ITERATION, not once per bar).
    let heap_loop_sites = heap.as_ref().map_or(0, |plan| {
        let native_new = |pc: usize| match code.get(pc) {
            Some(CsilInstr::NewObject { type_id, .. }) => !plan.host_types.contains(type_id),
            _ => false,
        };
        (0..code.len())
            .filter(|&pc| {
                plan.coll.contains_key(&pc) || plan.site_obj.contains_key(&pc) || native_new(pc)
            })
            .filter(|&pc| in_loop[block_of(pc)])
            .count() as u32
    });

    let host_loop_sites = (0..code.len())
        .filter(|&pc| typed.host_site[pc] && in_loop[block_of(pc)])
        .count() as u32;

    Ok(Analysis {
        blocks,
        term,
        rpo,
        idom,
        loop_header,
        merge,
        depth_at: typed.depth_at,
        entry_stack: typed.entry_stack,
        operand_tys: typed.operand_tys,
        result_tys: typed.result_tys,
        slot_ty: typed.slot_ty,
        host_site: typed.host_site,
        alertcondition_pcs: typed.alertcondition_pcs,
        heap,
        heap_ops_present,
        heap_loop_sites,
        host_loop_sites,
        int_min_guards,
        strategy_broker: program.metadata.strategy.as_ref().map(|_| broker),
        linked_host: mode.linked_host,
        canonical_strings: mode.canonical_strings,
    })
}

// ---------------------------------------------------------------------------
// Collection element provenance
// ---------------------------------------------------------------------------

/// Provenance lattice cell: `Unset` (bottom, no producer seen yet),
/// `Known(t)` (a collection handle whose element lane type is `t`),
/// `Obj(type_id)` (a UDT handle of a known declared type), `Opaque`
/// (definitely NOT a native-heap handle: scalars, strings, colors, and
/// host-produced refs like drawings — safe to cross a residual host site),
/// or `Mixed` (top — a native handle merged with anything else, or an
/// untrackable producer; unsafe on either side of the native-heap gate).
/// `Map(key, value)` is a `map<K, V>` handle whose key lane and value type
/// come from its `map.new` type tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Elem {
    Unset,
    Known(VTy),
    /// A native array handle whose lane is statically unknowable in THIS
    /// pass (`array.from` results — the lane comes from the typed pass).
    /// Only lane-independent receivers (`size`, `clear`) accept it.
    AnyLane,
    Obj(u32),
    /// A hosted array whose elements have a declared object layout.
    ObjectArray(u32),
    /// A hosted `map<K, V>`: the key lane and the value type. Maps never
    /// lower to the native heap (every `map.*` site fails the native-heap gate),
    /// so this only carries element types to `map.keys`/`map.values`
    /// results and to the map's own element operands and results.
    Map(VTy, MapValue),
    Opaque,
    Mixed,
}

/// The value type of a [`Elem::Map`]: a scalar lane, or a declared object
/// type (`map<string, Point>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MapValue {
    Lane(VTy),
    Obj(u32),
}

impl MapValue {
    /// The value's cell lane: objects travel as handles.
    fn lane(self) -> VTy {
        match self {
            MapValue::Lane(lane) => lane,
            MapValue::Obj(_) => VTy::Ref,
        }
    }
}

impl Elem {
    fn join(self, other: Elem) -> Elem {
        match (self, other) {
            (Elem::Unset, x) | (x, Elem::Unset) => x,
            (Elem::Known(a), Elem::Known(b)) if a == b => Elem::Known(a),
            (Elem::AnyLane, Elem::AnyLane) => Elem::AnyLane,
            (Elem::Obj(a), Elem::Obj(b)) if a == b => Elem::Obj(a),
            (Elem::ObjectArray(a), Elem::ObjectArray(b)) if a == b => Elem::ObjectArray(a),
            (Elem::Map(ka, va), Elem::Map(kb, vb)) if ka == kb && va == vb => Elem::Map(ka, va),
            (Elem::Opaque, Elem::Opaque) => Elem::Opaque,
            _ => Elem::Mixed,
        }
    }

    /// Could this cell carry a native-heap handle? (`Mixed` counts: it means
    /// a heap handle merged with something else.) A map is a heap handle:
    /// it counts, exactly as the untracked `Mixed` it replaces did.
    fn maybe_native(self) -> bool {
        matches!(
            self,
            Elem::Known(_)
                | Elem::AnyLane
                | Elem::Obj(_)
                | Elem::ObjectArray(_)
                | Elem::Map(..)
                | Elem::Mixed
        )
    }
}

/// The scalar lane a `map.new` type tag names (`pinewasm_mir::ElemType`
/// tags 0 to 4), `None` for an object tag or an unknown one.
fn map_tag_lane(tag: i64) -> Option<VTy> {
    match tag {
        0 => Some(VTy::F64),
        1 => Some(VTy::I64),
        2 => Some(VTy::Bool3),
        3 => Some(VTy::Str),
        4 => Some(VTy::Color),
        _ => None,
    }
}

/// The handle a `map.new` at `pc` builds: `Map(key, value)` when both type
/// tags are literal operands this program can name, `Mixed` otherwise.
/// Value tags at 1000 and above name declared object types.
fn map_new_elem(program: &CsilProgram, pc: usize) -> Elem {
    let code = &program.main().code;
    let tag = |back: usize| match pc.checked_sub(back).and_then(|at| code.get(at)) {
        Some(CsilInstr::PushI64(tag)) => Some(*tag),
        _ => None,
    };
    let (Some(key_tag), Some(value_tag)) = (tag(2), tag(1)) else {
        return Elem::Mixed;
    };
    let Some(key) = map_tag_lane(key_tag) else {
        return Elem::Mixed;
    };
    let value = match map_tag_lane(value_tag) {
        Some(lane) => MapValue::Lane(lane),
        None if value_tag >= 1000 => match u32::try_from(value_tag - 1000) {
            Ok(tid) if (tid as usize) < program.object_types.len() => MapValue::Obj(tid),
            _ => return Elem::Mixed,
        },
        None => return Elem::Mixed,
    };
    Elem::Map(key, value)
}

/// The result cell of a `map.*` call whose receiver cell is `receiver`, or
/// `None` for a function that is not one of the map projections and element
/// readers below. `map.keys` and `map.values` build arrays of the map's key
/// lane and value type; `map.copy` is another map of the same types; the
/// element results (`get`, `put`, `remove`) are values of the value type.
/// A receiver that is not a proven map keeps the untracked results.
fn map_result_elem(func: pinewasm_csil::CollectionFunc, receiver: Elem) -> Option<Elem> {
    use pinewasm_csil::CollectionFunc as F;
    let (key, value) = match receiver {
        Elem::Map(key, value) => (key, value),
        // `na` receivers: nothing flows (the polymorphic bottom).
        Elem::Unset => {
            return matches!(
                func,
                F::MapKeys | F::MapValues | F::MapCopy | F::MapGet | F::MapPut | F::MapRemove
            )
            .then_some(Elem::Unset)
        }
        _ => return matches!(func, F::MapKeys | F::MapValues | F::MapCopy).then_some(Elem::Mixed),
    };
    Some(match func {
        F::MapKeys => Elem::Known(key),
        F::MapValues => match value {
            MapValue::Lane(lane) => Elem::Known(lane),
            MapValue::Obj(tid) => Elem::ObjectArray(tid),
        },
        F::MapCopy => receiver,
        F::MapGet | F::MapPut | F::MapRemove => match value {
            MapValue::Lane(_) => Elem::Opaque,
            MapValue::Obj(tid) => Elem::Obj(tid),
        },
        _ => return None,
    })
}

#[cfg(test)]
mod aeneas_cfg_tests;
#[cfg(test)]
mod aeneas_guard_tests;
#[cfg(test)]
mod aeneas_refactor_tests;
/// Free-function extraction roots for Charon/Aeneas (`AotAnalysis` extraction).
#[cfg(feature = "aeneas")]
mod aeneas_roots_aot_analysis;
pub(crate) mod cfg;
pub(crate) mod fixpoint;
#[cfg(test)]
mod gc_tests;
#[cfg(test)]
mod restart_tests;
#[cfg(test)]
mod rewind_tests;
#[cfg(test)]
mod table_reuse_tests;

/// Element type a typed constructor produces, when statically known.
fn constructor_elem(func: pinewasm_csil::CollectionFunc) -> Elem {
    use pinewasm_csil::CollectionFunc as F;
    match func {
        F::ArrayNewFloat => Elem::Known(VTy::F64),
        F::ArrayNewInt => Elem::Known(VTy::I64),
        F::ArrayNewBool => Elem::Known(VTy::Bool3),
        F::ArrayNewString | F::StrSplit => Elem::Known(VTy::Str),
        F::ArrayNewColor => Elem::Known(VTy::Color),
        F::ArrayNewObject => Elem::Known(VTy::Ref),
        _ => Elem::Mixed,
    }
}

/// Per-`CALL_COLLECTION`-site receiver element types, where provable: a
/// small abstract interpretation over the CFG tracking element types from
/// the typed constructors (`array.new_float`, …) through stack cells,
/// `Dup`/`Select`, and user slots. This is what lets `array.get` results
/// and `array.push`/`set` element operands take their true lane type
/// instead of the f64 default — the difference between `array<int>` /
/// `array<string>` programs running native and falling back at run time.
/// Maps carry their key lane and value type (`Elem::Map`), so `map.keys` /
/// `map.values` arrays and the map's own element sites are typed too.
///
/// The same pass evaluates the whole-program heap gate
/// (docs/aot.md §13): whether EVERY heap site is native-lowerable and no
/// native handle can cross a residual host site.
struct ElemFacts {
    /// Scalar storage lanes proven from every constructor and field store.
    field_vty: BTreeMap<(u32, u32), VTy>,
    /// Provable receiver element type per `CALL_COLLECTION` pc.
    site_elem: BTreeMap<usize, VTy>,
    /// Provable `(key lane, value lane)` per `map.*` receiver pc; an object
    /// value travels as a `Ref` cell.
    site_map: BTreeMap<usize, (VTy, VTy)>,
    /// `GetField` pcs whose pushed value is provably a heap handle (a
    /// collection or object stored in the field) — typed `Ref` instead of
    /// the demand-driven default.
    ref_getfields: BTreeSet<usize>,
    /// `GetField`/`SetField`/`CopyObject` pc → provable receiver `type_id`.
    site_obj: BTreeMap<usize, u32>,
    /// Receiver-op pcs whose receiver is a lane-unknown native array
    /// (`array.from` results) — legal for `size`/`clear` only.
    site_any: BTreeSet<usize>,
    /// Type ids whose objects flow into `RegisterDrawing`: drawing-backing
    /// objects stay on the interpreter heap (the registry, GC, and
    /// serializers read them there), so their sites keep `host.vm`.
    host_types: BTreeSet<u32>,
    /// The whole-program heap gate.
    gate: HeapGate,
    /// The final provenance of every slot and object field, which the
    /// collector plan reads ([`gc_plan`]).
    gc_cells: GcCells,
}

/// The cells [`gc_plan`] reads off the converged element pass.
#[derive(Debug, Clone, Default)]
struct GcCells {
    /// Per user slot: what its cells can hold.
    slot_elems: Vec<Elem>,
    /// Per `(type_id, field index)`: what the field can hold.
    field_elems: BTreeMap<(u32, u32), Elem>,
    /// A possible native handle reaches a host object: a drawing
    /// constructor's operand, or a field store on a host receiver. The host
    /// heap could then reach native objects, which the module's collector
    /// does not trace through it.
    native_into_host: bool,
}

/// Verdict of the native-heap gate's flow half (field typing is judged
/// later, against the host-site typed pass).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HeapGate {
    /// The program contains collection/UDT opcodes at all.
    pub any_heap: bool,
    /// Every heap site is native-lowerable and no native handle reaches a
    /// residual host site (drawings, strings, effects, requests).
    pub flow_ok: bool,
}

/// The native collection operations the native heap lowers (docs/aot.md §13),
/// with their canonical operand shapes. Anything else fails the gate and keeps
/// host-site (`host.vm`) behavior for the whole program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeCollOp {
    /// `(size, initial)` — `array.new_float/new_int/new_bool/new_color`.
    NewArray,
    /// `(arg, …)` — `array.from` over one uniform scalar lane.
    FromArgs,
    /// `(recv, value)`.
    Push,
    /// `(recv, idx)` → elem.
    Get,
    /// `(recv, idx, value)`.
    Set,
    /// `(recv)` → int.
    Size,
    /// `(recv)` → elem, removes the first element.
    Shift,
    /// `(recv)` → elem, removes the last element.
    PopLast,
    /// `(recv)` → elem.
    First,
    /// `(recv)` → elem.
    Last,
    /// `(recv)`.
    Clear,
    /// `(recv)` → float aggregate (na-skipping, element order).
    Sum,
    Avg,
    Min,
    Max,
    /// `(recv, biased)` → float aggregate (na-skipping, element order).
    Stdev,
    Variance,
}

impl NativeCollOp {
    /// Whether the op is metered as linear cost (`CollectionFunc::
    /// is_linear_cost` parity for the native subset).
    pub fn linear_cost(self) -> bool {
        matches!(
            self,
            NativeCollOp::NewArray
                | NativeCollOp::FromArgs
                | NativeCollOp::Shift
                | NativeCollOp::Clear
                | NativeCollOp::Sum
                | NativeCollOp::Avg
                | NativeCollOp::Min
                | NativeCollOp::Max
                | NativeCollOp::Stdev
                | NativeCollOp::Variance
        )
    }

    /// The float aggregates, which read their elements as numbers and so
    /// need an `int` or `float` lane.
    fn numeric_aggregate(self) -> bool {
        matches!(
            self,
            NativeCollOp::Sum
                | NativeCollOp::Avg
                | NativeCollOp::Min
                | NativeCollOp::Max
                | NativeCollOp::Stdev
                | NativeCollOp::Variance
        )
    }
}

/// Whether the operand pushed immediately before `pc` is the literal int
/// `want`. Used to admit the defaulted form of a collection call whose extra
/// argument the native op cannot honor (`array.min(id, nth)` with `nth = 0` is
/// exactly `array.min(id)`); any other value stays a `host.vm` site.
pub(crate) fn pushes_literal_int(code: &[CsilInstr], pc: usize, want: i64) -> bool {
    pc > 0 && matches!(code[pc - 1], CsilInstr::PushI64(v) if v == want)
}

/// The native lowering for a collection call, when the (func, argc) shape
/// is in the native-heap subset. `default_tail` says the call's trailing argument
/// is the compiler-materialized default, so the native op (which does not take
/// it) is still an exact lowering.
pub(crate) fn native_coll_op(
    func: pinewasm_csil::CollectionFunc,
    argc: u8,
    default_tail: bool,
) -> Option<NativeCollOp> {
    use pinewasm_csil::CollectionFunc as F;
    // One flat match (no `CollectionFunc` equality) keeps this Aeneas-extractable.
    Some(match (func, argc) {
        (F::ArrayMin, 2) if default_tail => NativeCollOp::Min,
        (F::ArrayMax, 2) if default_tail => NativeCollOp::Max,
        (F::ArrayNewFloat | F::ArrayNewInt | F::ArrayNewBool | F::ArrayNewColor, 2) => {
            NativeCollOp::NewArray
        }
        (F::ArrayFrom, n) if n >= 1 => NativeCollOp::FromArgs,
        (F::ArrayPush, 2) => NativeCollOp::Push,
        (F::ArrayGet, 2) => NativeCollOp::Get,
        (F::ArraySet, 3) => NativeCollOp::Set,
        (F::ArraySize, 1) => NativeCollOp::Size,
        (F::ArrayShift, 1) => NativeCollOp::Shift,
        (F::ArrayPop, 1) => NativeCollOp::PopLast,
        (F::ArrayFirst, 1) => NativeCollOp::First,
        (F::ArrayLast, 1) => NativeCollOp::Last,
        (F::ArrayClear, 1) => NativeCollOp::Clear,
        (F::ArraySum, 1) => NativeCollOp::Sum,
        (F::ArrayAvg, 1) => NativeCollOp::Avg,
        (F::ArrayMin, 1) => NativeCollOp::Min,
        (F::ArrayMax, 1) => NativeCollOp::Max,
        // `biased` is always materialized (default `true`).
        (F::ArrayStdev, 2) => NativeCollOp::Stdev,
        (F::ArrayVariance, 2) => NativeCollOp::Variance,
        // `(ArrayMin | ArrayMax, 2)` without `default_tail` falls through:
        // native only when `nth` is the materialized default.
        _ => return None,
    })
}

/// Element lane types the native heap stores. `Bool3` and `Color` share the
/// `i32` lane: an element is the same `i32` the value has on the stack.
fn native_lane(elem: VTy) -> bool {
    matches!(elem, VTy::F64 | VTy::I64 | VTy::Bool3 | VTy::Color)
}

/// Record one heap-gate violation (env-gated diagnostics) and return the
/// cleared flag.
fn gate_fail(pc: usize, why: &str) -> bool {
    if std::env::var_os("PINEWASM_AOT_DEBUG_HEAP").is_some() {
        eprintln!("[heap] gate violation at pc {pc}: {why}");
    }
    false
}

/// The state of [`infer_collection_elems`] across fixpoint rounds; one
/// [`fixpoint::FixpointRound::round`] is one pass over the blocks in rpo.
struct ElemRound<'a> {
    program: &'a CsilProgram,
    blocks: &'a [Block],
    term: &'a [Term],
    rpo_order: &'a [usize],
    entry: Vec<Option<Vec<Elem>>>,
    slot_elems: Vec<Elem>,
    field_elems: BTreeMap<(u32, u32), Elem>,
    site_elem: BTreeMap<usize, Elem>,
    ref_getfields: BTreeSet<usize>,
    site_obj: BTreeMap<usize, u32>,
    site_any: BTreeSet<usize>,
    host_types: BTreeSet<u32>,
    /// `array.from` pc → the lane its arguments have, when a refining pass
    /// knows it (see [`refine_from_lanes`]); other sites give `AnyLane`.
    from_lanes: &'a BTreeMap<usize, VTy>,
    /// Cleared by any gate violation.
    flow_ok: bool,
    /// Set on the checking round by a possible native handle stored into a
    /// host object ([`GcCells::native_into_host`]).
    native_into_host: bool,
}

impl fixpoint::FixpointRound for ElemRound<'_> {
    #[allow(clippy::too_many_lines)]
    fn round(&mut self, checking: bool) -> fixpoint::RoundEnd {
        use pinewasm_hir::CollectionAbstract;
        let ElemRound {
            program,
            blocks,
            term,
            rpo_order,
            entry,
            slot_elems,
            field_elems,
            site_elem,
            ref_getfields,
            site_obj,
            site_any,
            host_types,
            from_lanes,
            flow_ok,
            native_into_host,
        } = self;
        let program: &CsilProgram = program;
        let code = &program.main().code;
        // Resolve every opcode through CSIL's shared contract. An invalid
        // descriptor fails the native heap gate closed instead of indexing
        // the TA table independently here.
        let effect = |instr: &CsilInstr| {
            instr
                .resolve_stack_effect(&program.ta_states)
                .map(|effect| (effect.pops as usize, effect.pushes as usize))
        };
        let mut changed = false;
        for &b in *rpo_order {
            let Some(stack0) = entry[b].clone() else {
                continue;
            };
            let mut stack = stack0;
            // `pc` is the function-wide program counter, not a slice index —
            // the range loop is the semantic form.
            #[allow(clippy::needless_range_loop)]
            for pc in blocks[b].start..blocks[b].end {
                let instr = &code[pc];
                match instr {
                    CsilInstr::Dup => {
                        let top = stack.pop().unwrap_or(Elem::Mixed);
                        stack.push(top);
                        stack.push(top);
                    }
                    CsilInstr::Select => {
                        let rhs = stack.pop().unwrap_or(Elem::Mixed);
                        let lhs = stack.pop().unwrap_or(Elem::Mixed);
                        stack.pop();
                        stack.push(lhs.join(rhs));
                    }
                    // The polymorphic bottom: `na` takes any origin, so a
                    // `cond ? na : myArray` merge keeps the array's facts.
                    CsilInstr::PushNa => {
                        stack.push(Elem::Unset);
                    }
                    CsilInstr::ReadUserSlot { slot, .. } => {
                        stack.push(slot_elems[*slot as usize]);
                    }
                    CsilInstr::ReadUserSlotDynamic { slot } => {
                        stack.pop();
                        stack.push(slot_elems[*slot as usize]);
                    }
                    CsilInstr::StoreUserSlotCurrent { slot } => {
                        let value = stack.pop().unwrap_or(Elem::Mixed);
                        let joined = slot_elems[*slot as usize].join(value);
                        if joined != slot_elems[*slot as usize] {
                            slot_elems[*slot as usize] = joined;
                            changed = true;
                        }
                    }
                    CsilInstr::NewObject {
                        type_id,
                        field_count,
                    } => {
                        let fc = *field_count as usize;
                        for i in 0..fc {
                            let value = stack.pop().unwrap_or(Elem::Mixed);
                            if checking && value.maybe_native() && host_types.contains(type_id) {
                                *native_into_host = true;
                            }
                            let idx = (fc - 1 - i) as u32; // top = last field
                            let cell = field_elems.entry((*type_id, idx)).or_insert(Elem::Unset);
                            let joined = cell.join(value);
                            if joined != *cell {
                                *cell = joined;
                                changed = true;
                            }
                        }
                        stack.push(if host_types.contains(type_id) {
                            Elem::Opaque
                        } else {
                            Elem::Obj(*type_id)
                        });
                    }
                    CsilInstr::CopyObject { type_id } => {
                        let receiver = stack.pop().unwrap_or(Elem::Mixed);
                        if checking && !host_types.contains(type_id) {
                            match receiver {
                                Elem::Obj(tid) if tid == *type_id => {
                                    site_obj.insert(pc, tid);
                                }
                                _ => *flow_ok = gate_fail(pc, "CopyObject receiver unproven"),
                            }
                        }
                        stack.push(if host_types.contains(type_id) {
                            Elem::Opaque
                        } else {
                            Elem::Obj(*type_id)
                        });
                    }
                    CsilInstr::SetField { field_idx } => {
                        let value = stack.pop().unwrap_or(Elem::Mixed);
                        let receiver = stack.pop().unwrap_or(Elem::Mixed);
                        if let Elem::Obj(tid) = receiver {
                            let cell = field_elems.entry((tid, *field_idx)).or_insert(Elem::Unset);
                            let joined = cell.join(value);
                            if joined != *cell {
                                *cell = joined;
                                changed = true;
                            }
                        }
                        if checking {
                            match receiver {
                                Elem::Obj(tid) => {
                                    site_obj.insert(pc, tid);
                                }
                                // Registered drawing handles are host objects.
                                Elem::Opaque => {
                                    if value.maybe_native() {
                                        *native_into_host = true;
                                    }
                                }
                                _ => *flow_ok = gate_fail(pc, "SetField receiver unproven"),
                            }
                        }
                    }
                    CsilInstr::GetField { field_idx } => {
                        let receiver = stack.pop().unwrap_or(Elem::Mixed);
                        let field = match receiver {
                            Elem::Obj(tid) => field_elems
                                .get(&(tid, *field_idx))
                                .copied()
                                .unwrap_or(Elem::Mixed),
                            _ => Elem::Mixed,
                        };
                        // A field state of Known(elem)/Obj(t) means the field
                        // VALUE is itself a heap handle.
                        if matches!(
                            field,
                            Elem::Known(_) | Elem::Obj(_) | Elem::ObjectArray(_) | Elem::Map(..)
                        ) && ref_getfields.insert(pc)
                        {
                            changed = true;
                        }
                        if checking {
                            match receiver {
                                Elem::Obj(tid) => {
                                    site_obj.insert(pc, tid);
                                }
                                // Registered drawing handles are host objects.
                                Elem::Opaque => {}
                                _ => *flow_ok = gate_fail(pc, "GetField receiver unproven"),
                            }
                        }
                        stack.push(field);
                    }
                    CsilInstr::CallCollection { func, argc } => {
                        let pops = *argc as usize;
                        let mut popped = Vec::with_capacity(pops);
                        for _ in 0..pops {
                            popped.push(stack.pop().unwrap_or(Elem::Mixed));
                        }
                        // The receiver, when the function takes one, is the
                        // DEEPEST operand (= last popped).
                        let takes_receiver = func
                            .operand_reqs(*argc)
                            .last()
                            .is_some_and(|req| matches!(req, CollectionAbstract::Ref));
                        let receiver = if takes_receiver {
                            popped.last().copied().unwrap_or(Elem::Mixed)
                        } else {
                            Elem::Mixed
                        };
                        if takes_receiver {
                            let prev = site_elem.get(&pc).copied().unwrap_or(Elem::Unset);
                            let joined = prev.join(receiver);
                            if joined != prev {
                                site_elem.insert(pc, joined);
                                changed = true;
                            }
                        }
                        if checking {
                            // Gate: the (func, argc) shape must be native and
                            // the lane type provable (constructors carry it;
                            // receiver ops need a proven receiver; the float
                            // aggregates need a numeric lane).
                            match native_coll_op(*func, *argc, pushes_literal_int(code, pc, 0)) {
                                None => {
                                    *flow_ok =
                                        gate_fail(pc, &format!("non-native {func:?}/{argc}"));
                                }
                                Some(NativeCollOp::NewArray | NativeCollOp::FromArgs) => {}
                                Some(op) => {
                                    match site_elem.get(&pc).copied().unwrap_or(Elem::Unset) {
                                        Elem::Known(e) if native_lane(e) => {
                                            if op.numeric_aggregate()
                                                && !matches!(e, VTy::F64 | VTy::I64)
                                            {
                                                *flow_ok = gate_fail(
                                                    pc,
                                                    "aggregate over a non-numeric lane",
                                                );
                                            }
                                        }
                                        // Lane-unknown native arrays accept
                                        // the lane-independent receivers.
                                        Elem::AnyLane
                                            if matches!(
                                                op,
                                                NativeCollOp::Size | NativeCollOp::Clear
                                            ) =>
                                        {
                                            site_any.insert(pc);
                                        }
                                        _ => {
                                            *flow_ok = gate_fail(pc, "receiver lane unproven");
                                        }
                                    }
                                }
                            }
                        }
                        // Result: constructors know their element type;
                        // same-elem copies inherit the receiver's. An
                        // ELEMENT result from a scalar lane is a plain value
                        // (never a heap handle); from a Ref lane it is a
                        // handle of untracked class. Map projections take
                        // the map's key and value types.
                        let map_result = if matches!(func, pinewasm_csil::CollectionFunc::MapNew) {
                            Some(map_new_elem(program, pc))
                        } else {
                            map_result_elem(*func, receiver)
                        };
                        let result = if let Some(result) = map_result {
                            result
                        } else {
                            match func.result() {
                                CollectionAbstract::Elem => match receiver {
                                    Elem::ObjectArray(tid) => Elem::Obj(tid),
                                    Elem::Known(VTy::Ref) => Elem::Mixed,
                                    Elem::Known(_) => Elem::Opaque,
                                    Elem::Unset => Elem::Unset,
                                    other => other,
                                },
                                CollectionAbstract::Ref => {
                                    let fresh = if matches!(
                                        func,
                                        pinewasm_csil::CollectionFunc::ArrayNewObject
                                    ) {
                                        // Compiler-authored array<T> constructors carry
                                        // the declared object's 1000 + type-id tag.
                                        match code.get(pc.wrapping_sub(1)) {
                                            Some(CsilInstr::PushI64(tag)) if *tag >= 1000 => {
                                                u32::try_from(*tag - 1000)
                                                    .ok()
                                                    .filter(|tid| {
                                                        (*tid as usize) < program.object_types.len()
                                                    })
                                                    .map_or(
                                                        Elem::Known(VTy::Ref),
                                                        Elem::ObjectArray,
                                                    )
                                            }
                                            _ => Elem::Known(VTy::Ref),
                                        }
                                    } else {
                                        constructor_elem(*func)
                                    };
                                    if fresh != Elem::Mixed {
                                        fresh
                                    } else if matches!(
                                        func,
                                        pinewasm_csil::CollectionFunc::ArrayFrom
                                    ) {
                                        // Native array whose lane is its
                                        // arguments' type, which only the typed
                                        // pass knows: a refining pass supplies it
                                        // (docs/aot.md §13).
                                        from_lanes
                                            .get(&pc)
                                            .map_or(Elem::AnyLane, |&lane| Elem::Known(lane))
                                    } else if matches!(
                                        func,
                                        pinewasm_csil::CollectionFunc::ArrayCopy
                                            | pinewasm_csil::CollectionFunc::ArraySlice
                                    ) {
                                        receiver
                                    } else {
                                        Elem::Mixed
                                    }
                                }
                                // Scalar results (size, sum, includes, …) can
                                // never be heap handles.
                                _ => Elem::Opaque,
                            }
                        };
                        stack.push(result);
                    }
                    // The registration site pins the constructed object's
                    // type as host-backing: the drawing registry, GC, and
                    // serializers read it on the interpreter heap. The
                    // fixpoint re-runs NewObject with the pin, so the
                    // checking round sees these operands as Opaque.
                    CsilInstr::RegisterDrawing { .. } => {
                        let obj = stack.pop().unwrap_or(Elem::Mixed);
                        if let Elem::Obj(tid) = obj {
                            if host_types.insert(tid) {
                                changed = true;
                            }
                        } else if checking && obj.maybe_native() {
                            *flow_ok = gate_fail(pc, "RegisterDrawing operand unproven");
                        }
                        stack.push(Elem::Opaque);
                    }
                    other => {
                        let (pops, pushes) = match effect(other) {
                            Ok(effect) => effect,
                            Err(error) => {
                                *flow_ok = gate_fail(pc, &format!("invalid stack effect: {error}"));
                                break;
                            }
                        };
                        // Residual host sites (and every other non-heap
                        // opcode) must never consume a possible native-heap
                        // handle. Dead pops and ref identity are native and
                        // handle-safe.
                        let handle_safe =
                            matches!(other, CsilInstr::Pop | CsilInstr::CmpEq | CsilInstr::CmpNe);
                        for _ in 0..pops {
                            let cell = stack.pop().unwrap_or(Elem::Mixed);
                            if checking && !handle_safe && cell.maybe_native() {
                                *flow_ok =
                                    gate_fail(pc, &format!("native handle at non-heap {other:?}"));
                            }
                        }
                        for _ in 0..pushes {
                            // Non-heap opcodes never produce native handles
                            // (host refs — drawings, footprints — are opaque
                            // to the native heap).
                            stack.push(Elem::Opaque);
                        }
                    }
                }
            }
            let succs: Vec<usize> = match term[b] {
                Term::Fall(s) | Term::Jump(s) => vec![s],
                Term::Branch { taken, fall } => vec![taken, fall],
                Term::Return => vec![],
            };
            for s in succs {
                match &mut entry[s] {
                    slot @ None => {
                        *slot = Some(stack.clone());
                        changed = true;
                    }
                    Some(prev) => {
                        if prev.len() != stack.len() {
                            // Malformed input; the typed pass rejects it.
                            return fixpoint::RoundEnd::Malformed;
                        }
                        for (p, c) in prev.iter_mut().zip(&stack) {
                            let joined = p.join(*c);
                            if joined != *p {
                                *p = joined;
                                changed = true;
                            }
                        }
                    }
                }
            }
        }
        if changed {
            fixpoint::RoundEnd::Changed
        } else {
            fixpoint::RoundEnd::Stable
        }
    }
}

/// The `map.*` sites whose receiver is a proven map, with its key lane and
/// value lane.
fn site_map_facts(site_elem: &BTreeMap<usize, Elem>) -> BTreeMap<usize, (VTy, VTy)> {
    site_elem
        .iter()
        .filter_map(|(&pc, elem)| match *elem {
            Elem::Map(key, value) => Some((pc, (key, value.lane()))),
            _ => None,
        })
        .collect()
}

/// `from_lanes` gives the lane of each listed `array.from` site; an unlisted
/// site's array is `AnyLane`. The first pass lists none (see
/// [`refine_from_lanes`]).
fn infer_collection_elems(
    program: &CsilProgram,
    blocks: &[Block],
    term: &[Term],
    rpo_order: &[usize],
    from_lanes: &BTreeMap<usize, VTy>,
) -> ElemFacts {
    let code = &program.main().code;
    let slots = program.series_slots.len();
    let n = blocks.len();
    let mut entry: Vec<Option<Vec<Elem>>> = vec![None; n];
    entry[0] = Some(Vec::new());
    let any_heap = code.iter().any(|i| {
        matches!(
            i,
            CsilInstr::CallCollection { .. }
                | CsilInstr::NewObject { .. }
                | CsilInstr::CopyObject { .. }
                | CsilInstr::GetField { .. }
                | CsilInstr::SetField { .. }
        )
    });
    let mut state = ElemRound {
        program,
        blocks,
        term,
        rpo_order,
        entry,
        slot_elems: vec![Elem::Unset; slots],
        // Collection handles stored in UDT fields (`type Buffer` with an
        // `array<float> values` field) keep their element type through
        // GetField/SetField/NewObject. Object cells carry their declared type
        // (`Obj(type_id)`), so fields key by (type_id, field index) and
        // unrelated types never collide.
        field_elems: BTreeMap::new(),
        site_elem: BTreeMap::new(),
        ref_getfields: BTreeSet::new(),
        site_obj: BTreeMap::new(),
        site_any: BTreeSet::new(),
        host_types: BTreeSet::new(),
        from_lanes,
        // Evaluated on the post-convergence checking round.
        flow_ok: any_heap,
        native_into_host: false,
    };

    // Fixpoint over the reducible CFG (validated depths make every pop
    // safe), then ONE more `checking` round at the stable state that
    // evaluates the native-heap gate and records per-site facts. When the
    // rounds do not converge within `max_rounds` (should not happen on a
    // finite lattice) the gate fails: keep the host-site behavior rather than
    // judge the gate on a moving state.
    let max_rounds = n + code.len() + 4;
    let outcome = fixpoint::run_fixpoint(&mut state, max_rounds);
    if outcome.malformed {
        // Malformed input (entry stacks of different heights); the typed
        // pass rejects it.
        return ElemFacts {
            field_vty: BTreeMap::new(),
            site_elem: BTreeMap::new(),
            site_map: BTreeMap::new(),
            ref_getfields: BTreeSet::new(),
            site_obj: BTreeMap::new(),
            site_any: BTreeSet::new(),
            host_types: BTreeSet::new(),
            gate: HeapGate {
                any_heap,
                flow_ok: false,
            },
            gc_cells: GcCells::default(),
        };
    }
    let flow_ok = state.flow_ok && outcome.converged;

    ElemFacts {
        field_vty: BTreeMap::new(),
        site_map: site_map_facts(&state.site_elem),
        site_elem: state
            .site_elem
            .into_iter()
            .filter_map(|(pc, elem)| match elem {
                Elem::Known(t) => Some((pc, t)),
                Elem::ObjectArray(_) => Some((pc, VTy::Ref)),
                _ => None,
            })
            .collect(),
        ref_getfields: state.ref_getfields,
        site_obj: state.site_obj,
        site_any: state.site_any,
        host_types: state.host_types,
        gate: HeapGate { any_heap, flow_ok },
        gc_cells: GcCells {
            slot_elems: state.slot_elems,
            field_elems: state.field_elems,
            native_into_host: state.native_into_host,
        },
    }
}

// ---------------------------------------------------------------------------
// Native-heap plan (docs/aot.md §13)
// ---------------------------------------------------------------------------

/// The whole-program native-heap lowering plan. Present on [`Analysis::heap`]
/// only when the gate passed AND the typed pass re-converged under native
/// heap typing; its maps then drive emission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeapPlan {
    /// `CallCollection` pc → (native op, element lane type).
    pub coll: BTreeMap<usize, (NativeCollOp, VTy)>,
    /// `GetField`/`SetField`/`CopyObject` pc → receiver `type_id`.
    pub site_obj: BTreeMap<usize, u32>,
    /// Object layouts: `type_id` → per-field storage `VTy` (index order,
    /// `field_count` entries; untouched fields default to f64 na cells).
    pub layouts: BTreeMap<u32, Vec<VTy>>,
    /// Drawing-backing type ids: their `NewObject`/`CopyObject`/field sites
    /// stay `host.vm` (the registry owns those objects).
    pub host_types: BTreeSet<u32>,
    /// The module's collector, when it can count exactly what the
    /// interpreter's counts ([`GcPlan`]). Without one the module never
    /// collects and its counts only ever run ahead of the interpreter's.
    pub gc: Option<GcPlan>,
}

/// What a slot cell or an object field refers to, for the module's
/// collector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcRef {
    /// A native array. Its record holds its element count.
    Array,
    /// A native object of this `type_id`.
    Object(u32),
    /// An object on the `host.vm` heap: a registered drawing.
    Hosted,
}

/// One root of the module's collector: the cells of `slot` the
/// interpreter's collector roots at a bar boundary, the current one and
/// `depth` bars back (`pinewasm_vm`'s `user_slot_gc_depths`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcRoot {
    pub slot: u32,
    pub depth: u32,
    pub kind: GcRef,
}

/// The module's collector (docs/aot.md §13).
///
/// The interpreter collects its heap at a bar boundary, under pressure, and
/// returns the budget of every object nothing can reach any more. A module
/// with this plan collects at the same boundaries under the same pressure
/// rule and reaches the same objects: the same slot cells are its roots,
/// native objects are traced through their reference fields, and host
/// objects are collected by the host from the drawing registry plus every
/// host reference the module holds. Its element and object counts therefore
/// equal the interpreter's at every instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcPlan {
    /// The slots that can hold a reference at a bar boundary.
    pub roots: Vec<GcRoot>,
    /// Per native type with reference fields: each such field and what it
    /// refers to.
    pub traced: BTreeMap<u32, Vec<(u32, GcRef)>>,
    /// The program allocates on the host heap, so a collection collects it
    /// too.
    pub hosted: bool,
}

impl HeapPlan {
    pub fn field_ty(&self, type_id: u32, field_idx: u32) -> VTy {
        self.layouts
            .get(&type_id)
            .and_then(|l| l.get(field_idx as usize))
            .copied()
            .unwrap_or(VTy::F64)
    }
}

/// What a hosted instruction allocates on the interpreter heap, for a
/// program that also has a native heap. See `emit_hosted_heap_charge`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostedAlloc {
    /// Allocates nothing on the host heap.
    None,
    /// One object holding at most this many elements.
    Elements(u64),
    /// An allocation whose size depends on run data, with no bound known
    /// here. A program with one keeps its whole heap on the host.
    Unbounded,
}

/// Classify `instr` as a hosted allocation. This is only asked about
/// instructions that stay on `host.vm` under a native-heap plan, where every
/// `CallCollection` is native (the gate fails otherwise), so collections,
/// matrices and maps never reach it.
///
/// The interpreter's allocating instructions are the `Heap::alloc` callers in
/// `pinewasm_vm`: the object constructors, `*.all`, and the four request and
/// footprint builders below. A new allocating instruction must be added here.
pub(crate) fn hosted_alloc(program: &CsilProgram, instr: &CsilInstr) -> HostedAlloc {
    match instr {
        CsilInstr::NewObject {
            type_id,
            field_count,
        } => {
            // A polyline owns copies of the supplied points. Their count is
            // data-dependent, so a mixed native heap cannot charge this as
            // merely the fixed number of drawing fields.
            if program
                .object_types
                .get(*type_id as usize)
                .is_some_and(|ty| ty.name == "polyline")
            {
                HostedAlloc::Unbounded
            } else {
                HostedAlloc::Elements(u64::from(*field_count))
            }
        }
        CsilInstr::CopyObject { type_id } => HostedAlloc::Elements(
            program
                .object_types
                .get(*type_id as usize)
                .map_or(0, |decl| u64::from(decl.field_count)),
        ),
        // `label.all` and its siblings: an array of the kind's live drawings,
        // which the kind's live cap bounds.
        CsilInstr::ReadDrawingsAll { kind } => {
            let caps = program.metadata.drawing_caps;
            let cap = match kind {
                DrawKind::Line => caps.max_lines_count,
                DrawKind::Label => caps.max_labels_count,
                DrawKind::Box => caps.max_boxes_count,
                DrawKind::Polyline => caps.max_polylines_count,
                DrawKind::Table | DrawKind::Linefill | DrawKind::Point => None,
            };
            // A kind with no live cap (tables) has no bound either.
            match cap.map_or_else(|| u32::try_from(kind.default_max()).ok(), Some) {
                Some(bound) => HostedAlloc::Elements(u64::from(bound)),
                None => HostedAlloc::Unbounded,
            }
        }
        CsilInstr::CallPivotPointLevels { .. }
        | CsilInstr::BuildFootprint { .. }
        | CsilInstr::ReadSecurityLowerTf { .. }
        | CsilInstr::ReadPivotLevels { .. } => HostedAlloc::Unbounded,
        _ => HostedAlloc::None,
    }
}

/// Unify one more observed type into a field's storage cell. Numeric
/// widening is the one non-identity join (`{F64, I64} → F64`, mirroring the
/// interpreter's int-in-float-context promotion); everything else must
/// match exactly.
fn unify_field(cell: &mut Option<VTy>, seen: VTy) -> bool {
    match *cell {
        None => {
            *cell = Some(seen);
            true
        }
        Some(t) if t == seen => true,
        Some(VTy::F64) if seen == VTy::I64 => true,
        Some(VTy::I64) if seen == VTy::F64 => {
            *cell = Some(VTy::F64);
            true
        }
        Some(_) => false,
    }
}

/// The lane of each `array.from` site whose arguments all have the same
/// native lane type in `typed` (the plan's own `FromArgs` rule).
fn from_site_lanes(program: &CsilProgram, typed: &TypedOut) -> BTreeMap<usize, VTy> {
    let code = &program.main().code;
    code.iter()
        .enumerate()
        .filter_map(|(pc, instr)| {
            if !matches!(
                instr,
                CsilInstr::CallCollection {
                    func: pinewasm_csil::CollectionFunc::ArrayFrom,
                    ..
                }
            ) {
                return None;
            }
            let tys = typed.operand_tys.get(pc)?;
            let lane = *tys.first()?;
            (tys.iter().all(|&ty| ty == lane) && (native_lane(lane) || lane == VTy::Str))
                .then_some((pc, lane))
        })
        .collect()
}

/// Native heap for a program whose `array.from` arrays need their lanes.
///
/// The first element pass gives every `array.from` array `AnyLane`, which only
/// `size` and `clear` accept: the lane is the type of the arguments, and only
/// the typed pass knows it. When that first gate fails, this takes each site's
/// lane from the host-site typed pass and redoes the element pass and the host-site
/// typed pass with those lanes, until the redone typed pass gives every site
/// the lane it was given (a few rounds at most). The result counts only when
/// the lanes agree; the native heap plan is optional. Proven element lanes
/// remain necessary for residual host-site result cells.
fn refine_from_lanes(
    program: &CsilProgram,
    blocks: &[Block],
    term: &[Term],
    rpo_order: &[usize],
    typed_phase_c: &TypedOut,
    mode: InferenceMode,
) -> Option<(ElemFacts, TypedOut, Option<HeapPlan>)> {
    const MAX_ROUNDS: usize = 8;
    let mut from_lanes = from_site_lanes(program, typed_phase_c);
    let mut previous_typed: Option<TypedOut> = None;
    for _ in 0..MAX_ROUNDS {
        if from_lanes.is_empty() {
            return None;
        }
        let mut facts = infer_collection_elems(program, blocks, term, rpo_order, &from_lanes);
        facts.field_vty = stored_field_lanes(
            program,
            &facts,
            previous_typed.as_ref().unwrap_or(typed_phase_c),
        );
        let typed = TypeInference::new(program, blocks, term, rpo_order, &facts, None, mode)
            .and_then(TypeInference::run)
            .ok()?;
        let lanes = from_site_lanes(program, &typed);
        let fields = stored_field_lanes(program, &facts, &typed);
        if lanes == from_lanes && fields == facts.field_vty {
            let plan = plan_native_heap(program, &facts, &typed);
            if std::env::var_os("PINEWASM_AOT_DEBUG_HEAP").is_some() {
                eprintln!(
                    "[heap] array.from lanes {from_lanes:?}: flow_ok={} plan={}",
                    facts.gate.flow_ok,
                    plan.is_some()
                );
            }
            return Some((facts, typed, plan));
        }
        from_lanes = lanes;
        previous_typed = Some(typed);
    }
    None
}

/// Storage-side evidence applies to hosted objects too. Conflicting stores
/// discard the claim instead of guessing a representation from a read.
fn stored_field_lanes(
    program: &CsilProgram,
    facts: &ElemFacts,
    typed: &TypedOut,
) -> BTreeMap<(u32, u32), VTy> {
    let mut cells = BTreeMap::<(u32, u32), Option<VTy>>::new();
    let mut conflicts = BTreeSet::new();
    let mut store = |tid: u32, idx: u32, lane: VTy| {
        if !unify_field(cells.entry((tid, idx)).or_insert(None), lane) {
            conflicts.insert((tid, idx));
        }
    };
    for (pc, instruction) in program.main().code.iter().enumerate() {
        match instruction {
            CsilInstr::NewObject {
                type_id,
                field_count,
            } => {
                let count = *field_count as usize;
                let direct = pc
                    .checked_sub(count)
                    .and_then(|start| program.main().code.get(start..pc))
                    .filter(|instructions| {
                        instructions.iter().all(|instruction| {
                            instruction
                                .resolve_stack_effect(&program.ta_states)
                                .is_ok_and(|effect| effect.pops == 0 && effect.pushes == 1)
                        })
                    });
                for (idx, lane) in typed.operand_tys[pc].iter().copied().enumerate() {
                    if direct
                        .is_some_and(|instructions| matches!(instructions[idx], CsilInstr::PushNa))
                    {
                        continue; // Polymorphic missing defaults state no storage lane.
                    }
                    store(*type_id, idx as u32, lane);
                }
            }
            CsilInstr::SetField { field_idx } => {
                // A field-to-field copy is a read-side guess in this pass,
                // not independent storage evidence. Constructor/store facts
                // for the owning type already describe the source field.
                if matches!(
                    program.main().code.get(pc.wrapping_sub(1)),
                    Some(CsilInstr::GetField { .. })
                ) {
                    continue;
                }
                if let (Some(tid), Some(lane)) =
                    (facts.site_obj.get(&pc), typed.operand_tys[pc].get(1))
                {
                    store(*tid, *field_idx, *lane);
                }
            }
            _ => {}
        }
    }
    cells
        .into_iter()
        .filter_map(|(key, lane)| {
            (!conflicts.contains(&key))
                .then_some(lane)
                .flatten()
                .filter(|lane| *lane != VTy::F64)
                .map(|lane| (key, lane))
        })
        .collect()
}

/// Judge the typing half of the native-heap gate against the host-site typed
/// pass, producing the plan when every heap site lowers natively.
fn plan_native_heap(
    program: &CsilProgram,
    facts: &ElemFacts,
    typed: &TypedOut,
) -> Option<HeapPlan> {
    if !facts.gate.any_heap || !facts.gate.flow_ok {
        return None;
    }
    let code = &program.main().code;
    // A hosted allocation of unknown size cannot be charged to the module's
    // budgets, so the two heaps could together hold more than the
    // interpreter's one budget allows. Keep such a program on one heap.
    if code
        .iter()
        .any(|instr| hosted_alloc(program, instr) == HostedAlloc::Unbounded)
    {
        return None;
    }
    let mut coll = BTreeMap::new();
    let mut field_cells: BTreeMap<(u32, u32), Option<VTy>> = BTreeMap::new();
    let unify = |field_cells: &mut BTreeMap<(u32, u32), Option<VTy>>,
                 tid: u32,
                 idx: u32,
                 seen: VTy|
     -> bool {
        if idx
            >= program
                .object_types
                .get(tid as usize)
                .map_or(0, |d| d.field_count)
        {
            return false;
        }
        unify_field(field_cells.entry((tid, idx)).or_insert(None), seen)
    };
    for (pc, instr) in code.iter().enumerate() {
        match instr {
            CsilInstr::CallCollection { func, argc } => {
                let op = native_coll_op(*func, *argc, pushes_literal_int(code, pc, 0))?;
                let elem = match op {
                    NativeCollOp::NewArray => match constructor_elem(*func) {
                        Elem::Known(e) => e,
                        _ => return None,
                    },
                    // `array.from`: one uniform scalar lane, resolved from
                    // the host-site typed pass (na args default to f64 there).
                    NativeCollOp::FromArgs => {
                        let tys = typed.operand_tys.get(pc)?;
                        if tys.is_empty() || tys.windows(2).any(|w| w[0] != w[1]) {
                            return None;
                        }
                        tys[0]
                    }
                    // Lane-independent receivers over `array.from` results:
                    // the lane is unknown here and unused by emission; f64
                    // types the void placeholder.
                    _ if facts.site_any.contains(&pc) => VTy::F64,
                    _ => facts.site_elem.get(&pc).copied()?,
                };
                if !native_lane(elem) {
                    return None;
                }
                coll.insert(pc, (op, elem));
            }
            CsilInstr::NewObject {
                type_id,
                field_count,
            } => {
                // Drawing-backing constructors stay host sites.
                if facts.host_types.contains(type_id) {
                    continue;
                }
                let ops = typed.operand_tys.get(pc)?;
                if ops.len() != *field_count as usize
                    || program
                        .object_types
                        .get(*type_id as usize)
                        .is_none_or(|d| d.field_count != *field_count)
                {
                    return None;
                }
                for (idx, &ty) in ops.iter().enumerate() {
                    if !unify(&mut field_cells, *type_id, idx as u32, ty) {
                        return None;
                    }
                }
            }
            // For field/copy sites, a missing `site_obj` entry under a
            // passing gate means the receiver is a host object (registered
            // drawings): the site simply stays on `host.vm`.
            CsilInstr::CopyObject { .. } => {}
            // Field STORAGE types come from the store sides only (NewObject
            // operands, SetField values) — the checker inserted the numeric
            // conversions it wanted, so reads re-type from the layout under
            // the native pass. A host-site GetField result is a demand-driven
            // GUESS (an int field read only in numeric-polymorphic contexts
            // defaults to f64 there) and must not pollute the layout.
            CsilInstr::GetField { field_idx } => {
                let Some(&tid) = facts.site_obj.get(&pc) else {
                    continue;
                };
                if *field_idx
                    >= program
                        .object_types
                        .get(tid as usize)
                        .map_or(0, |d| d.field_count)
                {
                    return None;
                }
            }
            CsilInstr::SetField { field_idx } => {
                let Some(&tid) = facts.site_obj.get(&pc) else {
                    continue;
                };
                let ty = *typed.operand_tys.get(pc)?.get(1)?;
                if !unify(&mut field_cells, tid, *field_idx, ty) {
                    return None;
                }
            }
            _ => {}
        }
    }
    let mut layouts: BTreeMap<u32, Vec<VTy>> = BTreeMap::new();
    for (tid, decl) in program.object_types.iter().enumerate() {
        let tid = tid as u32;
        let fields = (0..decl.field_count)
            .map(|idx| {
                field_cells
                    .get(&(tid, idx))
                    .copied()
                    .flatten()
                    .unwrap_or(VTy::F64)
            })
            .collect();
        layouts.insert(tid, fields);
    }
    Some(HeapPlan {
        coll,
        site_obj: facts.site_obj.clone(),
        layouts,
        host_types: facts.host_types.clone(),
        // Planned once the native typing is known ([`gc_plan`]).
        gc: None,
    })
}

/// The collector plan for a native-heap program, or `None` when the module
/// cannot count exactly what the interpreter counts. Its counts must then
/// never fall behind the interpreter's, so it never collects.
///
/// Refused, each because a collection could reclaim a different set of
/// objects than the interpreter's would:
///
/// * a cell or field that can hold references of more than one kind
///   (`Mixed`), which the collector could not trace, or an array whose
///   elements are not scalars;
/// * a possible native reference stored in a host object, which the host's
///   collection would not see;
/// * `*.all`, which the module charges at the kind's live cap rather than
///   at the size of the array the host builds;
/// * a reference slot whose root depth depends on an input value.
fn gc_plan(
    program: &CsilProgram,
    facts: &ElemFacts,
    typed: &TypedOut,
    layouts: &BTreeMap<u32, Vec<VTy>>,
) -> Option<GcPlan> {
    let cells = &facts.gc_cells;
    if cells.native_into_host {
        return None;
    }
    let code = &program.main().code;
    if code
        .iter()
        .any(|instr| matches!(instr, CsilInstr::ReadDrawingsAll { .. }))
    {
        return None;
    }
    // A scalar cell holds no reference whatever its provenance (a `Mixed`
    // scalar is, say, a field read off a drawing); one that provably holds a
    // native handle disagrees with its own type.
    let native_handle = |elem: Elem| matches!(elem, Elem::Known(_) | Elem::AnyLane | Elem::Obj(_));
    let depths = slot_gc_depths(program);
    let mut roots = Vec::new();
    for (slot, &ty) in typed.slot_ty.iter().enumerate() {
        let elem = cells.slot_elems.get(slot).copied().unwrap_or(Elem::Unset);
        if ty != VTy::Ref {
            if native_handle(elem) {
                return None;
            }
            continue;
        }
        let kind = match gc_cell(elem, &facts.host_types) {
            GcCell::Ref(kind) => kind,
            GcCell::Na => continue,
            GcCell::Untraceable => return None,
        };
        let (depth, input_bound) = depths.get(slot).copied().unwrap_or((0, false));
        if input_bound {
            return None;
        }
        // A cell that starts every bar as `na` and is never read back is no
        // root at a bar boundary.
        let persistent = program
            .series_slots
            .get(slot)
            .is_some_and(|def| def.persistent);
        if !persistent && depth == 0 {
            continue;
        }
        roots.push(GcRoot {
            slot: slot as u32,
            depth,
            kind,
        });
    }
    let mut traced = BTreeMap::new();
    for (&tid, layout) in layouts {
        if facts.host_types.contains(&tid) {
            continue;
        }
        let mut refs = Vec::new();
        for (idx, &ty) in layout.iter().enumerate() {
            let elem = cells
                .field_elems
                .get(&(tid, idx as u32))
                .copied()
                .unwrap_or(Elem::Unset);
            if ty != VTy::Ref {
                if native_handle(elem) {
                    return None;
                }
                continue;
            }
            match gc_cell(elem, &facts.host_types) {
                GcCell::Ref(kind) => refs.push((idx as u32, kind)),
                GcCell::Na => {}
                GcCell::Untraceable => return None,
            }
        }
        if !refs.is_empty() {
            traced.insert(tid, refs);
        }
    }
    let hosted_alloc_site = code.iter().any(|instr| match instr {
        CsilInstr::NewObject { type_id, .. } | CsilInstr::CopyObject { type_id } => {
            facts.host_types.contains(type_id)
        }
        _ => false,
    });
    let hosted_ref = roots.iter().any(|root| root.kind == GcRef::Hosted)
        || traced
            .values()
            .flatten()
            .any(|&(_, kind)| kind == GcRef::Hosted);
    Some(GcPlan {
        roots,
        traced,
        hosted: hosted_alloc_site || hosted_ref,
    })
}

/// What a slot cell or object field can hold, for the collector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GcCell {
    /// Only ever `na`: nothing to trace.
    Na,
    /// References of one kind, which the collector traces.
    Ref(GcRef),
    /// References the collector cannot trace: no collector.
    Untraceable,
}

/// What a cell or field whose provenance is `elem` holds, for the collector.
///
/// The collector counts an array's elements without reading them, which is
/// exact only for a scalar lane: an array of references would keep what they
/// refer to alive, and nothing here would trace it. An `array.from` array
/// (`AnyLane`) is native only with a scalar lane (`plan_native_heap`'s
/// `FromArgs` rule). A type in `host_types` lives on the host and is reached
/// through the drawing references the module holds (`Opaque`), never as a
/// native object.
fn gc_cell(elem: Elem, host_types: &BTreeSet<u32>) -> GcCell {
    match elem {
        Elem::Unset => GcCell::Na,
        Elem::Known(lane) if native_lane(lane) => GcCell::Ref(GcRef::Array),
        Elem::AnyLane => GcCell::Ref(GcRef::Array),
        Elem::Obj(tid) if !host_types.contains(&tid) => GcCell::Ref(GcRef::Object(tid)),
        Elem::Opaque => GcCell::Ref(GcRef::Hosted),
        Elem::Known(_) | Elem::Obj(_) | Elem::ObjectArray(_) | Elem::Map(..) | Elem::Mixed => {
            GcCell::Untraceable
        }
    }
}

/// Per user slot: how many bars back the interpreter's collector roots its
/// cells (`user_slot_gc_depths` in `pinewasm_vm`), and whether that depth
/// depends on an input value (an input-backed `ta.change` or average reads
/// the slot).
fn slot_gc_depths(program: &CsilProgram) -> Vec<(u32, bool)> {
    let mut depths = vec![(0u32, false); program.series_slots.len()];
    for binding in &program.ta_input_bindings {
        let slot = match binding.target {
            pinewasm_hir::TaInputTarget::Sma { slot }
            | pinewasm_hir::TaInputTarget::Change { slot } => slot,
            pinewasm_hir::TaInputTarget::State { .. } => continue,
        };
        if let Some(entry) = depths.get_mut(slot as usize) {
            entry.1 = true;
        }
    }
    for function in &program.functions {
        for instr in &function.code {
            let (slot, depth) = match instr {
                CsilInstr::ReadUserSlot { slot, offset } => (*slot, *offset),
                CsilInstr::ReadUserSlotDynamic { slot } => (*slot, program.metadata.max_bars_back),
                CsilInstr::WindowMean { slot, length } => (*slot, length.saturating_sub(1)),
                CsilInstr::WindowMeanDynamic { slot }
                | CsilInstr::WindowExtremumDynamic { slot, .. }
                | CsilInstr::WindowStatisticDynamic { slot, .. } => {
                    (*slot, pinewasm_csil::MAX_DYNAMIC_WINDOW_DEPTH)
                }
                _ => continue,
            };
            if let Some(entry) = depths.get_mut(slot as usize) {
                entry.0 = entry.0.max(depth);
            }
        }
    }
    depths
}

// ---------------------------------------------------------------------------
// Type inference
// ---------------------------------------------------------------------------

/// Inference-time cell type: concrete, or an unresolved na producer set.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Cell {
    Ty(VTy),
    /// Produced by `PushNa`, a read of a not-yet-typed slot, or an untyped
    /// host result — directly, or carried through merges, `Select`, and the
    /// pure calls whose result takes its arguments' type. Carries the
    /// producing pcs so a typed consumer can pin them.
    Unknown(BTreeSet<usize>),
}

#[derive(Debug, PartialEq, Eq)]
struct TypedOut {
    depth_at: Vec<u32>,
    entry_stack: Vec<Vec<VTy>>,
    operand_tys: Vec<Vec<VTy>>,
    result_tys: Vec<Vec<VTy>>,
    slot_ty: Vec<VTy>,
    host_site: Vec<bool>,
    alertcondition_pcs: Vec<usize>,
}

#[cfg_attr(test, derive(Clone))]
struct TypeInference<'p> {
    program: &'p CsilProgram,
    blocks: &'p [Block],
    term: &'p [Term],
    rpo_order: &'p [usize],
    /// Pinned types for `PushNa` pcs, refined across restarts.
    pinned: BTreeMap<usize, VTy>,
    /// Pinned types per slot (from stores and read demands).
    slot_pin: Vec<Option<VTy>>,
    /// Per-pc "executes on the interpreter host" flags (rebuilt each pass).
    host_site: Vec<bool>,
    /// Producer sets a consumer read this pass without stating a type (a
    /// host site observing its operand, `na()`, an untyped store);
    /// still-unpinned ones default to f64 only at convergence.
    deferred: Vec<BTreeSet<usize>>,
    /// Producer sets a numeric-polymorphic consumer read this pass
    /// ([`Self::demand_numeric`]). They default to f64 at convergence, ahead
    /// of every other default: the value is a number, whatever else read it.
    deferred_numeric: Vec<BTreeSet<usize>>,
    /// Producer sets only `bool()` read this pass. They default to the bool
    /// lane at convergence, after the numeric defaults and ahead of the
    /// plain ones, so a bool that crosses a host site keeps its lane.
    deferred_bool: Vec<BTreeSet<usize>>,
    /// Element/handle provenance facts from [`infer_collection_elems`].
    elem_facts: &'p ElemFacts,
    /// Native-heap plan: when set, heap opcodes type natively (per
    /// plan lanes/layouts) instead of becoming `host.vm` sites.
    heap: Option<&'p HeapPlan>,
    /// Slots that keep a Pine integer quotient in the f64 lane
    /// ([`quotient_slots`]); pinned f64 before the first pass.
    quotient_slots: BTreeSet<u32>,
    /// Per slot: the block step of the current simulation in which an
    /// instruction first looked at the slot's pin (a read, a store or a
    /// window), or [`NEVER`]. A store or window that pins a slot nothing has
    /// looked at yet goes on without a restart (see
    /// [`Self::pin_fresh_slot`]); any other pin resumes the simulation from
    /// the first step that looked (see [`Self::resume_step`]).
    consulted: Vec<u32>,
    /// The slots [`Self::consult`] marked, oldest first: the undo log of
    /// `consulted`.
    consult_log: Vec<u32>,
    /// The block step being simulated: an index into [`Simulation::marks`].
    step_now: u32,
    /// The variables pinned since the simulation last resumed, fresh slots
    /// excepted: what [`Self::resume_step`] looks behind.
    repinned: Vec<PinVar>,
    /// The block containing each pc.
    block_of_pc: Vec<u32>,
    /// Where a `strategy()` program's broker runs (ignored for indicators).
    broker: StrategyBroker,
    /// See [`InferenceMode`].
    linked_host: bool,
    canonical_strings: bool,
    host_colors: bool,
}

/// A block step that did not happen in the current simulation.
const NEVER: u32 = u32::MAX;

/// What one instruction's step computed: its operand types, then its result
/// types.
type StepTypes = (Vec<VTy>, Vec<VTy>);

/// What a pin types: an untyped producer (a `PushNa`, an untyped host
/// result) or a slot.
#[derive(Debug, Clone, Copy)]
enum PinVar {
    Producer(usize),
    Slot(u32),
}

/// The state one simulation builds, and the journal that takes it back to
/// the start of any block step of that simulation.
///
/// A full replay would discard all of this and simulate again from the first
/// block. Everything simulated before the first instruction that looked at
/// the pinned variable is independent of the pin, so that replay recomputed,
/// step for step, the states already here. [`TypeInference::rewind`] returns
/// to that instruction's block instead, and [`TypeInference::simulate`]
/// carries on from there: the same states, pins, deferrals and errors as the
/// replay, in the same order, without the replay.
struct Simulation {
    /// Per block: the stack on entry, once a predecessor reached it.
    entry: Vec<Option<Vec<Cell>>>,
    depth_at: Vec<u32>,
    operand_tys: Vec<Vec<VTy>>,
    result_tys: Vec<Vec<VTy>>,
    /// The rpo pass in progress, from 1.
    pass: usize,
    /// The position in `rpo_order` of the pass's next block.
    index: usize,
    /// Whether the pass in progress changed a block entry so far.
    any_change: bool,
    /// One per block step so far, oldest first: the state before it.
    marks: Vec<Mark>,
    /// Per block: the step that first simulated it, or [`NEVER`].
    first_step: Vec<u32>,
    entry_log: Vec<EntryUndo>,
    record_log: Vec<RecordUndo>,
    /// The pcs whose `host_site` flag a step raised.
    host_log: Vec<usize>,
    /// The blocks `first_step` marked.
    first_log: Vec<usize>,
}

/// The simulation position and the journal lengths before one block step.
#[derive(Clone, Copy)]
struct Mark {
    pass: usize,
    index: usize,
    any_change: bool,
    entry_log: usize,
    record_log: usize,
    host_log: usize,
    first_log: usize,
    consult_log: usize,
    /// The lengths of `deferred_numeric`, `deferred_bool` and `deferred`.
    deferred: [usize; 3],
}

/// How to undo one change to a block entry.
enum EntryUndo {
    /// The block had no entry.
    Reached(usize),
    /// Cell `index` of the block's entry held `cell`.
    Joined {
        block: usize,
        index: usize,
        cell: Cell,
    },
}

/// What an instruction's records held before a step replaced them.
struct RecordUndo {
    pc: usize,
    depth: u32,
    operands: Vec<VTy>,
    results: Vec<VTy>,
}

impl Simulation {
    fn new(code_len: usize, block_count: usize) -> Self {
        let mut entry = vec![None; block_count];
        entry[0] = Some(Vec::new());
        Simulation {
            entry,
            depth_at: vec![0; code_len],
            operand_tys: vec![Vec::new(); code_len],
            result_tys: vec![Vec::new(); code_len],
            pass: 1,
            index: 0,
            any_change: false,
            marks: Vec::new(),
            first_step: vec![NEVER; block_count],
            entry_log: Vec::new(),
            record_log: Vec::new(),
            host_log: Vec::new(),
            first_log: Vec::new(),
        }
    }

    /// Record what a completed step of `pc` computed, keeping what it
    /// replaces. A later pass mostly computes the same records again; those
    /// leave nothing to undo.
    fn record(&mut self, pc: usize, depth: u32, operands: Vec<VTy>, results: Vec<VTy>) {
        if self.depth_at[pc] == depth
            && self.operand_tys[pc] == operands
            && self.result_tys[pc] == results
        {
            return;
        }
        self.record_log.push(RecordUndo {
            pc,
            depth: std::mem::replace(&mut self.depth_at[pc], depth),
            operands: std::mem::replace(&mut self.operand_tys[pc], operands),
            results: std::mem::replace(&mut self.result_tys[pc], results),
        });
    }
}

#[cfg(test)]
thread_local! {
    /// Simulations started on this thread, resumed or from scratch, for the
    /// tests that bound how many restarts inference needs.
    static SIMULATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Instructions [`TypeInference::simulate`] stepped on this thread, for
    /// the tests that bound how much of the program a restart simulates again.
    static STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Whether [`TypeInference::run`] also runs
    /// [`TypeInference::run_from_scratch`] and requires the same answer.
    static SHADOW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

enum Flow {
    /// A pin was added: the simulation has to be replayed from the first
    /// block step that looked at a newly pinned variable, and at the latest
    /// from `redo`, the step the pin interrupted ([`NEVER`] when no step was
    /// in progress).
    Restart {
        redo: u32,
    },
    Done(TypedOut),
}

impl<'p> TypeInference<'p> {
    fn new(
        program: &'p CsilProgram,
        blocks: &'p [Block],
        term: &'p [Term],
        rpo_order: &'p [usize],
        elem_facts: &'p ElemFacts,
        heap: Option<&'p HeapPlan>,
        mode: InferenceMode,
    ) -> Result<Self, AotError> {
        let quotient_slots = quotient_slots(program, blocks);
        let mut slot_pin = vec![None; program.series_slots.len()];
        for &slot in &quotient_slots {
            slot_pin[slot as usize] = Some(VTy::F64);
        }
        let mut block_of_pc = vec![0; program.main().code.len()];
        for (b, block) in blocks.iter().enumerate() {
            if let Some(pcs) = block_of_pc.get_mut(block.start..block.end) {
                pcs.fill(b as u32);
            }
        }
        Ok(TypeInference {
            program,
            blocks,
            term,
            rpo_order,
            pinned: BTreeMap::new(),
            slot_pin,
            host_site: Vec::new(),
            deferred: Vec::new(),
            deferred_numeric: Vec::new(),
            deferred_bool: Vec::new(),
            elem_facts,
            heap,
            quotient_slots,
            consulted: Vec::new(),
            consult_log: Vec::new(),
            step_now: 0,
            repinned: Vec::new(),
            block_of_pc,
            broker: mode.broker,
            linked_host: mode.linked_host,
            canonical_strings: mode.canonical_strings,
            host_colors: mode.host_colors,
        })
    }

    fn run(self) -> Result<TypedOut, AotError> {
        #[cfg(test)]
        if SHADOW.with(std::cell::Cell::get) {
            let replayed = self.clone().run_from_scratch();
            let resumed = self.run_resumed();
            assert_eq!(resumed, replayed, "resuming differs from replaying");
            return resumed;
        }
        self.run_resumed()
    }

    fn run_resumed(mut self) -> Result<TypedOut, AotError> {
        // Each restart pins at least one PushNa pc or slot, so the loop is
        // bounded by (#PushNa + #slots + 1).
        let code_len = self.program.main().code.len();
        let cap = code_len + self.program.series_slots.len() + 2;
        let mut sim = Simulation::new(code_len, self.blocks.len());
        self.host_site = vec![false; code_len];
        self.consulted = vec![NEVER; self.program.series_slots.len()];
        for _ in 0..cap {
            match self.simulate(&mut sim)? {
                Flow::Restart { redo } => {
                    let step = self.resume_step(&sim, redo);
                    self.rewind(&mut sim, step);
                }
                Flow::Done(out) => return Ok(out),
            }
        }
        Err(AotError::InvalidProgram(
            "type inference did not converge".to_string(),
        ))
    }

    /// The block step a restart resumes from: the earliest one that looked
    /// at a variable pinned since the simulation last resumed, and no later
    /// than `redo`, the step the pin interrupted ([`NEVER`] when the pin
    /// came after the last step).
    ///
    /// Nothing before that step depends on the new pins. A producer's pin is
    /// read by its own instruction alone, so the first step of its block
    /// stands for it; a slot's pin is read where [`Self::consult`] marks it.
    fn resume_step(&self, sim: &Simulation, redo: u32) -> u32 {
        let looked = self
            .repinned
            .iter()
            .map(|var| match *var {
                PinVar::Producer(pc) => sim.first_step[self.block_of_pc[pc] as usize],
                PinVar::Slot(slot) => self.consulted[slot as usize],
            })
            .min()
            .unwrap_or(NEVER);
        match looked.min(redo) {
            // Nothing names the step the simulations part at: replay it all.
            NEVER => 0,
            step => step,
        }
    }

    /// Take `sim`, and the state of `self` a simulation builds, back to the
    /// start of block step `step`. The pins stay.
    fn rewind(&mut self, sim: &mut Simulation, step: u32) {
        self.repinned.clear();
        // No step yet: the simulation is where it started.
        let Some(&mark) = sim.marks.get(step as usize) else {
            return;
        };
        sim.marks.truncate(step as usize);
        for undo in sim.entry_log.drain(mark.entry_log..).rev() {
            match undo {
                EntryUndo::Reached(block) => sim.entry[block] = None,
                EntryUndo::Joined { block, index, cell } => {
                    if let Some(cells) = &mut sim.entry[block] {
                        cells[index] = cell;
                    }
                }
            }
        }
        for undo in sim.record_log.drain(mark.record_log..).rev() {
            sim.depth_at[undo.pc] = undo.depth;
            sim.operand_tys[undo.pc] = undo.operands;
            sim.result_tys[undo.pc] = undo.results;
        }
        for pc in sim.host_log.drain(mark.host_log..) {
            self.host_site[pc] = false;
        }
        for block in sim.first_log.drain(mark.first_log..) {
            sim.first_step[block] = NEVER;
        }
        for slot in self.consult_log.drain(mark.consult_log..) {
            self.consulted[slot as usize] = NEVER;
        }
        self.deferred_numeric.truncate(mark.deferred[0]);
        self.deferred_bool.truncate(mark.deferred[1]);
        self.deferred.truncate(mark.deferred[2]);
        sim.pass = mark.pass;
        sim.index = mark.index;
        sim.any_change = mark.any_change;
    }

    /// Pin producer `pc`s to `ty`; error on conflicting pins. A producer is
    /// either a `PushNa` (pinned directly) or a read of a not-yet-typed slot
    /// (pinning routes to the slot, so read-before-store patterns type
    /// correctly).
    fn pin(&mut self, producers: &BTreeSet<usize>, ty: VTy) -> Result<bool, AotError> {
        let mut changed = false;
        for &pc in producers {
            match &self.program.main().code[pc] {
                CsilInstr::ReadUserSlot { slot, .. } | CsilInstr::ReadUserSlotDynamic { slot } => {
                    changed |= self.pin_slot(*slot, ty)?;
                }
                _ => match self.pinned.get(&pc) {
                    Some(&t) if t == ty => {}
                    Some(&t) => {
                        return Err(AotError::TypeConflict(format!(
                            "na at pc {pc} demanded as both {t:?} and {ty:?}"
                        )))
                    }
                    None => {
                        self.pinned.insert(pc, ty);
                        self.repinned.push(PinVar::Producer(pc));
                        changed = true;
                    }
                },
            }
        }
        Ok(changed)
    }

    /// Pin `slot` to `ty` from a store or a window, and say whether the
    /// simulation must restart.
    ///
    /// It need not when no instruction simulated so far in this
    /// [`Self::simulate`] call has looked at the slot's pin. Everything
    /// computed so far is then independent of it, so a restart would replay
    /// exactly the states already computed and reach this instruction with
    /// the slot pinned to the same type: going on is the same simulation, one
    /// replay shorter. Without this, a program pays one whole simulation per
    /// slot whose first store comes before its first read, which is nearly
    /// every slot (each inlined call's argument temps among them).
    fn pin_fresh_slot(&mut self, slot: u32, ty: VTy, fresh: bool) -> Result<bool, AotError> {
        Ok(if fresh {
            // No step looked at this pin before the one that sets it, so
            // there is none to resume from.
            self.set_slot_pin(slot, ty)?;
            false
        } else {
            self.pin_slot(slot, ty)?
        })
    }

    /// Note that the instruction being simulated looks at `slot`'s pin, and
    /// return whether nothing had before it in this simulation.
    fn consult(&mut self, slot: u32) -> bool {
        let looked = &mut self.consulted[slot as usize];
        if *looked != NEVER {
            return false;
        }
        *looked = self.step_now;
        self.consult_log.push(slot);
        true
    }

    fn pin_slot(&mut self, slot: u32, ty: VTy) -> Result<bool, AotError> {
        let pinned = self.set_slot_pin(slot, ty)?;
        if pinned {
            self.repinned.push(PinVar::Slot(slot));
        }
        Ok(pinned)
    }

    /// Set `slot`'s pin to `ty` and say whether it was unset; error on a
    /// different pin.
    fn set_slot_pin(&mut self, slot: u32, ty: VTy) -> Result<bool, AotError> {
        let cell = &mut self.slot_pin[slot as usize];
        match cell {
            Some(t) if *t == ty => Ok(false),
            Some(t) => Err(AotError::TypeConflict(format!(
                "slot {slot} stored as both {t:?} and {ty:?}"
            ))),
            None => {
                *cell = Some(ty);
                Ok(true)
            }
        }
    }

    /// Demand a concrete type from a cell: concrete → check match; Unknown →
    /// pin its producers and restart.
    fn demand(&mut self, cell: &Cell, ty: VTy, pc: usize) -> Result<Option<VTy>, AotError> {
        match cell {
            Cell::Ty(t) if *t == ty => Ok(Some(*t)),
            Cell::Ty(t) => Err(AotError::TypeConflict(format!(
                "pc {pc}: operand is {t:?}, needs {ty:?}"
            ))),
            Cell::Unknown(prods) => {
                self.pin(&prods.clone(), ty)?;
                Ok(None) // restart
            }
        }
    }

    /// State the type an untyped cell must have, leaving a typed cell as it
    /// is: Unknown → pin its producers and restart. For host operands whose
    /// instruction fixes the runtime type; a typed cell is passed through
    /// unchanged, exactly as [`Self::observe`] would.
    fn demand_untyped(&mut self, cell: Cell, ty: VTy) -> Result<Option<VTy>, AotError> {
        match cell {
            Cell::Ty(t) => Ok(Some(t)),
            Cell::Unknown(prods) => {
                self.pin(&prods, ty)?;
                Ok(None) // restart
            }
        }
    }

    /// Demand "any numeric". An untyped cell counts as f64 (the interpreter's
    /// na default in numeric contexts) only provisionally: its producers are
    /// deferred, never pinned here, so a consumer that states the real type
    /// still wins and f64 applies only at convergence. These consumers take
    /// an int or a float alike, so they have no type to state; pinning f64
    /// on sight turned an untyped int slot into a float for the conversion
    /// or int operator that read it next.
    fn demand_numeric(&mut self, cell: &Cell, pc: usize) -> Result<Option<VTy>, AotError> {
        match cell {
            Cell::Ty(VTy::F64) => Ok(Some(VTy::F64)),
            Cell::Ty(VTy::I64) => Ok(Some(VTy::I64)),
            Cell::Ty(t) => Err(AotError::TypeConflict(format!(
                "pc {pc}: operand is {t:?}, needs a numeric type"
            ))),
            Cell::Unknown(prods) => {
                self.deferred_numeric.push(prods.clone());
                Ok(Some(VTy::F64))
            }
        }
    }

    /// Simulate from where `sim` stands until every block entry is stable and
    /// every producer has a type ([`Flow::Done`]), or until a pin is added
    /// ([`Flow::Restart`]).
    #[allow(clippy::too_many_lines)]
    fn simulate(&mut self, sim: &mut Simulation) -> Result<Flow, AotError> {
        let code = &self.program.main().code;
        let n = self.blocks.len();
        #[cfg(test)]
        SIMULATIONS.with(|count| count.set(count.get() + 1));

        // Iterate blocks in rpo until entry states stabilize (loops need a
        // second pass when a back edge changes the header's entry; Unknown
        // producer sets grow monotonically, so passes are bounded by cells).
        loop {
            while sim.index < self.rpo_order.len() {
                let b = self.rpo_order[sim.index];
                self.step_now = sim.marks.len() as u32;
                sim.marks.push(Mark {
                    pass: sim.pass,
                    index: sim.index,
                    any_change: sim.any_change,
                    entry_log: sim.entry_log.len(),
                    record_log: sim.record_log.len(),
                    host_log: sim.host_log.len(),
                    first_log: sim.first_log.len(),
                    consult_log: self.consult_log.len(),
                    deferred: [
                        self.deferred_numeric.len(),
                        self.deferred_bool.len(),
                        self.deferred.len(),
                    ],
                });
                sim.index += 1;
                let Some(stack0) = sim.entry[b].clone() else {
                    continue;
                };
                if sim.first_step[b] == NEVER {
                    sim.first_step[b] = self.step_now;
                    sim.first_log.push(b);
                }
                let mut stack = stack0;
                for pc in self.blocks[b].start..self.blocks[b].end {
                    let depth = stack.len() as u32;
                    let hosted = self.host_site[pc];
                    #[cfg(test)]
                    STEPS.with(|count| count.set(count.get() + 1));
                    let stepped = self.step(code, pc, &mut stack);
                    if self.host_site[pc] && !hosted {
                        sim.host_log.push(pc);
                    }
                    let Some((operands, results)) = stepped? else {
                        return Ok(Flow::Restart {
                            redo: self.step_now,
                        });
                    };
                    sim.record(pc, depth, operands, results);
                }
                // Propagate exit state to successors, merging cells.
                let (succs, count) = match self.term[b] {
                    Term::Fall(s) | Term::Jump(s) => ([s, s], 1),
                    Term::Branch { taken, fall } => ([taken, fall], 2),
                    Term::Return => ([0, 0], 0),
                };
                for &s in &succs[..count] {
                    match &mut sim.entry[s] {
                        slot @ None => {
                            *slot = Some(stack.clone());
                            sim.entry_log.push(EntryUndo::Reached(s));
                            sim.any_change = true;
                        }
                        Some(prev) => {
                            if prev.len() != stack.len() {
                                return Err(AotError::InvalidProgram(format!(
                                    "inconsistent stack depth at merge block {s}"
                                )));
                            }
                            for (index, (p, c)) in prev.iter_mut().zip(&stack).enumerate() {
                                match Self::join(p, c) {
                                    JoinResult::Same => {}
                                    JoinResult::Changed(new) => {
                                        sim.entry_log.push(EntryUndo::Joined {
                                            block: s,
                                            index,
                                            cell: std::mem::replace(p, new),
                                        });
                                        sim.any_change = true;
                                    }
                                    JoinResult::PinNeeded(prods, ty) => {
                                        if self.pin(&prods, ty)? {
                                            return Ok(Flow::Restart {
                                                redo: self.step_now,
                                            });
                                        }
                                    }
                                    JoinResult::Conflict(a, bty) => {
                                        return Err(AotError::TypeConflict(format!(
                                            "merge block {s}: {a:?} vs {bty:?}"
                                        )))
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if !sim.any_change {
                break;
            }
            sim.pass += 1;
            if sim.pass > n + code.len() + 4 {
                return Err(AotError::InvalidProgram(
                    "stack-state analysis did not converge".to_string(),
                ));
            }
            sim.any_change = false;
            sim.index = 0;
        }

        // Deferred host-site operands: any producer a real demand did not
        // reach by convergence defaults to f64 now (then one more pass
        // records the settled types).
        if self.pin_deferred_defaults() {
            return Ok(Flow::Restart { redo: NEVER });
        }

        // Finalize: every reachable block entry stack must be concrete;
        // residual Unknowns default to f64.
        let mut entry_stack: Vec<Vec<VTy>> = Vec::with_capacity(n);
        let mut needs_restart = false;
        for e in &sim.entry {
            match e {
                None => entry_stack.push(Vec::new()), // unreachable block
                Some(cells) => {
                    let mut tys = Vec::with_capacity(cells.len());
                    for c in cells {
                        match c {
                            Cell::Ty(t) => tys.push(*t),
                            Cell::Unknown(prods) => {
                                if self.pin(prods, VTy::F64)? {
                                    needs_restart = true;
                                }
                                tys.push(VTy::F64);
                            }
                        }
                    }
                    entry_stack.push(tys);
                }
            }
        }
        if needs_restart {
            return Ok(Flow::Restart { redo: NEVER });
        }

        let slot_ty = self
            .slot_pin
            .iter()
            .map(|p| p.unwrap_or(VTy::F64))
            .collect();

        let alertcondition_pcs = code
            .iter()
            .enumerate()
            .filter(|(_, i)| matches!(i, CsilInstr::AlertCondition))
            .map(|(pc, _)| pc)
            .collect();
        Ok(Flow::Done(TypedOut {
            depth_at: std::mem::take(&mut sim.depth_at),
            entry_stack,
            operand_tys: std::mem::take(&mut sim.operand_tys),
            result_tys: std::mem::take(&mut sim.result_tys),
            slot_ty,
            host_site: std::mem::take(&mut self.host_site),
            alertcondition_pcs,
        }))
    }

    /// A whole-replay form of the inference: every pin discards the
    /// simulation and replays it from the first block. `cfg(test)` keeps it
    /// as a differential check of [`Self::run_resumed`].
    #[cfg(test)]
    fn run_from_scratch(mut self) -> Result<TypedOut, AotError> {
        let cap = self.program.main().code.len() + self.program.series_slots.len() + 2;
        for _ in 0..cap {
            match self.simulate_from_scratch()? {
                Flow::Restart { .. } => {}
                Flow::Done(out) => return Ok(out),
            }
        }
        Err(AotError::InvalidProgram(
            "type inference did not converge".to_string(),
        ))
    }

    /// One whole simulation of [`Self::run_from_scratch`], on fresh state.
    #[cfg(test)]
    #[allow(clippy::too_many_lines)]
    fn simulate_from_scratch(&mut self) -> Result<Flow, AotError> {
        let code = &self.program.main().code;
        let n = self.blocks.len();
        SIMULATIONS.with(|count| count.set(count.get() + 1));
        let mut entry: Vec<Option<Vec<Cell>>> = vec![None; n];
        entry[0] = Some(Vec::new());
        let mut depth_at = vec![0u32; code.len()];
        let mut operand_tys: Vec<Vec<VTy>> = vec![Vec::new(); code.len()];
        let mut result_tys: Vec<Vec<VTy>> = vec![Vec::new(); code.len()];
        self.host_site = vec![false; code.len()];
        self.consulted.clear();
        self.consulted
            .resize(self.program.series_slots.len(), NEVER);
        self.consult_log.clear();
        self.step_now = 0;
        self.repinned.clear();
        self.deferred.clear();
        self.deferred_numeric.clear();
        self.deferred_bool.clear();

        let mut pass = 0;
        loop {
            pass += 1;
            if pass > n + code.len() + 4 {
                return Err(AotError::InvalidProgram(
                    "stack-state analysis did not converge".to_string(),
                ));
            }
            let mut any_change = false;

            for &b in self.rpo_order {
                let Some(stack0) = entry[b].clone() else {
                    continue;
                };
                let mut stack = stack0;
                // `pc` is the function-wide program counter, not a slice
                // index — the range loop is the semantic form.
                #[allow(clippy::needless_range_loop)]
                for pc in self.blocks[b].start..self.blocks[b].end {
                    depth_at[pc] = stack.len() as u32;
                    let Some((operands, results)) = self.step(code, pc, &mut stack)? else {
                        return Ok(Flow::Restart { redo: NEVER });
                    };
                    operand_tys[pc] = operands;
                    result_tys[pc] = results;
                }
                let succs: Vec<usize> = match self.term[b] {
                    Term::Fall(s) | Term::Jump(s) => vec![s],
                    Term::Branch { taken, fall } => vec![taken, fall],
                    Term::Return => vec![],
                };
                for s in succs {
                    match &mut entry[s] {
                        slot @ None => {
                            *slot = Some(stack.clone());
                            any_change = true;
                        }
                        Some(prev) => {
                            if prev.len() != stack.len() {
                                return Err(AotError::InvalidProgram(format!(
                                    "inconsistent stack depth at merge block {s}"
                                )));
                            }
                            for (p, c) in prev.iter_mut().zip(&stack) {
                                match Self::join(p, c) {
                                    JoinResult::Same => {}
                                    JoinResult::Changed(new) => {
                                        *p = new;
                                        any_change = true;
                                    }
                                    JoinResult::PinNeeded(prods, ty) => {
                                        if self.pin(&prods, ty)? {
                                            return Ok(Flow::Restart { redo: NEVER });
                                        }
                                    }
                                    JoinResult::Conflict(a, bty) => {
                                        return Err(AotError::TypeConflict(format!(
                                            "merge block {s}: {a:?} vs {bty:?}"
                                        )))
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if !any_change {
                break;
            }
        }

        if self.pin_deferred_defaults() {
            return Ok(Flow::Restart { redo: NEVER });
        }

        let mut entry_stack: Vec<Vec<VTy>> = Vec::with_capacity(n);
        let mut needs_restart = false;
        for e in &entry {
            match e {
                None => entry_stack.push(Vec::new()),
                Some(cells) => {
                    let mut tys = Vec::with_capacity(cells.len());
                    for c in cells {
                        match c {
                            Cell::Ty(t) => tys.push(*t),
                            Cell::Unknown(prods) => {
                                if self.pin(prods, VTy::F64)? {
                                    needs_restart = true;
                                }
                                tys.push(VTy::F64);
                            }
                        }
                    }
                    entry_stack.push(tys);
                }
            }
        }
        if needs_restart {
            return Ok(Flow::Restart { redo: NEVER });
        }

        let slot_ty = self
            .slot_pin
            .iter()
            .map(|p| p.unwrap_or(VTy::F64))
            .collect();
        let alertcondition_pcs = code
            .iter()
            .enumerate()
            .filter(|(_, i)| matches!(i, CsilInstr::AlertCondition))
            .map(|(pc, _)| pc)
            .collect();
        Ok(Flow::Done(TypedOut {
            depth_at,
            entry_stack,
            operand_tys,
            result_tys,
            slot_ty,
            host_site: std::mem::take(&mut self.host_site),
            alertcondition_pcs,
        }))
    }

    fn join(a: &Cell, b: &Cell) -> JoinResult {
        match (a, b) {
            (Cell::Ty(x), Cell::Ty(y)) if x == y => JoinResult::Same,
            (Cell::Ty(x), Cell::Ty(y)) => JoinResult::Conflict(*x, *y),
            (Cell::Unknown(pa), Cell::Unknown(pb)) => {
                if pb.is_subset(pa) {
                    JoinResult::Same
                } else {
                    let mut u = pa.clone();
                    u.extend(pb.iter().copied());
                    JoinResult::Changed(Cell::Unknown(u))
                }
            }
            // Unknown meets concrete: pin the unknown side.
            (Cell::Unknown(p), Cell::Ty(t)) | (Cell::Ty(t), Cell::Unknown(p)) => {
                JoinResult::PinNeeded(p.clone(), *t)
            }
        }
    }

    /// Execute one instruction abstractly. `Ok(None)` = a pin was added,
    /// restart the simulation.
    #[allow(clippy::too_many_lines)]
    fn step(
        &mut self,
        code: &[CsilInstr],
        pc: usize,
        stack: &mut Vec<Cell>,
    ) -> Result<Option<StepTypes>, AotError> {
        use CsilInstr as I;
        let instr = &code[pc];

        macro_rules! pop {
            () => {
                stack.pop().ok_or_else(|| {
                    AotError::InvalidProgram(format!("stack underflow at pc {pc}"))
                })?
            };
        }
        macro_rules! demand_pop {
            ($ty:expr) => {{
                let cell = pop!();
                match self.demand(&cell, $ty, pc)? {
                    Some(t) => t,
                    None => return Ok(None),
                }
            }};
        }

        let mut ops: Vec<VTy> = Vec::new();
        let mut results: Vec<VTy> = Vec::new();

        match instr {
            I::PushNa => {
                let cell = Cell::Unknown(BTreeSet::from([pc])).pinned_or(&self.pinned, pc);
                // Once pinned (final pass), record the concrete type so the
                // emitter knows which na constant to materialize.
                if let Cell::Ty(t) = &cell {
                    results.push(*t);
                }
                stack.push(cell);
            }
            I::PushBool(_) => {
                results.push(VTy::Bool3);
                stack.push(Cell::Ty(VTy::Bool3));
            }
            I::PushI64(_) => {
                // The post-inference eligibility gate rejects a reachable
                // i64::MIN literal before this analysis can be emitted.
                results.push(VTy::I64);
                stack.push(Cell::Ty(VTy::I64));
            }
            I::PushF64(_) => {
                results.push(VTy::F64);
                stack.push(Cell::Ty(VTy::F64));
            }
            I::PushColor { t, .. } => {
                // The host's table keeps what a packed cell cannot: a
                // fractional transparency, and the sign of a zero one.
                let t = t.get();
                if self.host_colors && (t.fract() != 0.0 || (t == 0.0 && t.is_sign_negative())) {
                    self.host_site[pc] = true;
                }
                results.push(VTy::Color);
                stack.push(Cell::Ty(VTy::Color));
            }
            I::PushStr(_) => {
                results.push(VTy::Str);
                stack.push(Cell::Ty(VTy::Str));
            }
            I::AlertCondition => {
                // Pops message (top), title, condition.
                demand_pop!(VTy::Str);
                demand_pop!(VTy::Str);
                demand_pop!(VTy::Bool3);
                ops.extend([VTy::Bool3, VTy::Str, VTy::Str]);
            }
            I::Dup => {
                let top = pop!();
                stack.push(top.clone());
                stack.push(top);
            }
            I::Pop => {
                let cell = pop!();
                if let Cell::Ty(t) = cell {
                    ops.push(t);
                }
            }
            I::ReadBuiltin { source, offset: _ } | I::ReadBuiltinDynamic { source } => {
                if matches!(instr, I::ReadBuiltinDynamic { .. }) {
                    demand_pop!(VTy::I64);
                    ops.push(VTy::I64);
                }
                match builtin_vty(*source) {
                    Some(ty) => {
                        results.push(ty);
                        stack.push(Cell::Ty(ty));
                    }
                    None if source.is_strategy()
                        && self.program.metadata.strategy.is_some()
                        && self.broker == StrategyBroker::Linked =>
                    {
                        // A strategy() program reads its strategy.* getters
                        // off the linked broker's per-bar snapshots
                        // This is a broker-region read, not a host
                        // site. The two string-typed getters have no native
                        // string story and refuse with their names.
                        match strategy_snap_field(*source) {
                            Some((_, ty)) => {
                                results.push(ty);
                                stack.push(Cell::Ty(ty));
                            }
                            None => {
                                return Err(AotError::StrategyFeatureUnsupported {
                                    feature: source.name().to_string(),
                                })
                            }
                        }
                    }
                    None => {
                        // Everything else reads through the interpreter host
                        // (`Vm::read_builtin` — calendar, session, syminfo,
                        // timeframe, strategy snapshots). Statically-typed
                        // variants get a concrete lane; always-na interfaces
                        // stay demand-driven.
                        self.host_site[pc] = true;
                        match host_builtin_vty(*source) {
                            Some(ty) => {
                                results.push(ty);
                                stack.push(Cell::Ty(ty));
                            }
                            None => {
                                let cell =
                                    Cell::Unknown(BTreeSet::from([pc])).pinned_or(&self.pinned, pc);
                                if let Cell::Ty(t) = &cell {
                                    results.push(*t);
                                }
                                stack.push(cell);
                            }
                        }
                    }
                }
            }
            I::ReadUserSlot { slot, .. } | I::ReadUserSlotDynamic { slot } => {
                if matches!(instr, I::ReadUserSlotDynamic { .. }) {
                    demand_pop!(VTy::I64);
                    ops.push(VTy::I64);
                }
                let slot_pin = self.slot_pin.get(*slot as usize).copied().ok_or_else(|| {
                    AotError::InvalidProgram(format!("slot {slot} is out of range"))
                })?;
                self.consult(*slot);
                match slot_pin {
                    Some(t) => {
                        results.push(t);
                        stack.push(Cell::Ty(t));
                    }
                    // Not-yet-typed slot: push an Unknown carrying THIS pc as
                    // producer — `pin` routes a later demand to the slot, so
                    // read-before-store (`var` self-references) types right.
                    None => stack.push(Cell::Unknown(BTreeSet::from([pc]))),
                }
            }
            I::StoreUserSlotCurrent { slot } => {
                let cell = pop!();
                let slot_pin = self
                    .slot_pin
                    .get(*slot as usize)
                    .copied()
                    .ok_or_else(|| unsupported(instr))?;
                let fresh = self.consult(*slot);
                let ty = match (&cell, slot_pin) {
                    (Cell::Ty(t), None) => {
                        if self.pin_fresh_slot(*slot, *t, fresh)? {
                            return Ok(None);
                        }
                        *t
                    }
                    (Cell::Ty(t), Some(s)) if *t == s => s,
                    // An integer stored to a quotient slot is converted on
                    // the way in; the operand type records that for the
                    // emitter.
                    (Cell::Ty(VTy::I64), Some(VTy::F64)) if self.quotient_slots.contains(slot) => {
                        VTy::I64
                    }
                    (Cell::Ty(t), Some(s)) => {
                        return Err(AotError::TypeConflict(format!(
                            "slot {slot} stored as {t:?} but pinned {s:?}"
                        )))
                    }
                    (Cell::Unknown(prods), Some(s)) => {
                        if self.pin(&prods.clone(), s)? {
                            return Ok(None);
                        }
                        s
                    }
                    (Cell::Unknown(prods), None) => {
                        // na (or an untyped host result) stored into a
                        // never-typed slot: DEFER the f64 default so a later
                        // read's real demand (`line.delete(s)` needs Ref,
                        // `int(s)` needs I64…) can still pin the slot; the
                        // convergence pass defaults whatever remains.
                        self.deferred.push(prods.clone());
                        VTy::F64
                    }
                };
                ops.push(ty);
            }
            I::ReadInput { input } => {
                let decl = self
                    .program
                    .inputs
                    .get(*input as usize)
                    .ok_or_else(|| unsupported(instr))?;
                // String inputs resolve on the host. The shared type plan
                // retains color handling defensively, but the native identity
                // preflight sends color-input execution to the hosted tier.
                // A linked host has written both kinds' cells into the input
                // table, so the module reads them like any other input.
                if matches!(decl.kind, InputKind::String | InputKind::Color) && !self.linked_host {
                    self.host_site[pc] = true;
                }
                let ty = input_vty(self.program, *input).ok_or_else(|| unsupported(instr))?;
                results.push(ty);
                stack.push(Cell::Ty(ty));
            }
            I::WindowMean { slot, .. } => {
                let fresh = self.consult(*slot);
                if self.slot_pin[*slot as usize].is_none()
                    && self.pin_fresh_slot(*slot, VTy::F64, fresh)?
                {
                    return Ok(None);
                }
                if self.slot_pin[*slot as usize] != Some(VTy::F64) {
                    return Err(AotError::TypeConflict(format!(
                        "WINDOW_MEAN over non-f64 slot {slot}"
                    )));
                }
                results.push(VTy::F64);
                stack.push(Cell::Ty(VTy::F64));
            }
            I::WindowMeanDynamic { .. } => return Err(unsupported(instr)),
            I::WindowExtremumDynamic { .. } => return Err(unsupported(instr)),
            I::WindowStatisticDynamic { .. } => return Err(unsupported(instr)),
            I::EmaSimpleDynamic { .. } => {
                // Only the standalone WebAssembly tier lowers a run-time EMA
                // length (`body/ema.rs`): its status protocol can report a
                // length that is out of range once the script is running. A
                // chart module keeps the hosted fallback, where the
                // interpreter raises that error itself.
                if self.broker != StrategyBroker::Linked {
                    return Err(unsupported(instr));
                }
                // (source, length), the length on top. A fractional int
                // never reaches the length (`fraction::reads_whole_part`).
                demand_pop!(VTy::I64);
                demand_pop!(VTy::F64);
                ops.push(VTy::F64);
                ops.push(VTy::I64);
                results.push(VTy::F64);
                stack.push(Cell::Ty(VTy::F64));
            }
            I::CallTa { state } => {
                let decl = self
                    .program
                    .ta_states
                    .get(*state as usize)
                    .ok_or_else(|| AotError::InvalidProgram(format!("ta state {state}")))?;
                // Operands per the engine table, last on top.
                let operands = decl.func.engine_operands();
                for req in operands.iter().rev() {
                    let ty = match req {
                        pinewasm_hir::TaOperand::Num => VTy::F64,
                        pinewasm_hir::TaOperand::Cond => VTy::Bool3,
                        pinewasm_hir::TaOperand::Bool => VTy::Bool3,
                        pinewasm_hir::TaOperand::Color => VTy::Color,
                    };
                    demand_pop!(ty);
                }
                for req in operands {
                    ops.push(match req {
                        pinewasm_hir::TaOperand::Num => VTy::F64,
                        pinewasm_hir::TaOperand::Cond => VTy::Bool3,
                        pinewasm_hir::TaOperand::Bool => VTy::Bool3,
                        pinewasm_hir::TaOperand::Color => VTy::Color,
                    });
                }
                let result_ty = match decl.func.engine_result() {
                    pinewasm_hir::TaResult::Float => VTy::F64,
                    pinewasm_hir::TaResult::Int => VTy::I64,
                    pinewasm_hir::TaResult::Bool => VTy::Bool3,
                    pinewasm_hir::TaResult::Color => VTy::Color,
                };
                // Tuple results are pushed first-element-deepest; every tuple
                // element today is a float.
                for _ in 0..decl.func.engine_result_arity() {
                    results.push(result_ty);
                    stack.push(Cell::Ty(result_ty));
                }
            }
            I::CallPure { func, argc } => match self.pure_plan(*func, *argc) {
                Some(plan) => {
                    if self.host_colors
                        && matches!(
                            plan,
                            PurePlan::ColorChannel
                                | PurePlan::ColorNew
                                | PurePlan::ColorRgb
                                | PurePlan::ColorFromGradient
                        )
                    {
                        self.host_site[pc] = true;
                    }
                    // Pop args last-first; demand per plan (which may depend
                    // on the observed types for the polymorphic ops).
                    let mut popped: Vec<Cell> = Vec::with_capacity(*argc as usize);
                    for _ in 0..*argc {
                        popped.push(pop!());
                    }
                    popped.reverse(); // argument order
                    if let Some(prods) = result_follows_untyped_args(plan, *func, &popped) {
                        // The result takes its arguments' type and they have
                        // none yet (an object field read at a host site, a
                        // not-yet-typed slot, na). Push it untyped over the
                        // same producers, so the consumer's demand types the
                        // arguments and the call together: the conversion the
                        // checker emits after an int result (`INT_TO_F64`) is
                        // such a demand. A result nothing demands defaults to
                        // f64 at convergence, like every other deferred cell.
                        self.deferred.push(prods.clone());
                        ops.extend(popped.iter().map(|cell| match cell {
                            Cell::Ty(t) => *t,
                            Cell::Unknown(_) => VTy::F64,
                        }));
                        stack.push(Cell::Unknown(prods));
                    } else {
                        let (arg_tys, result) = match self.pure_types(plan, *func, &popped, pc)? {
                            Some(x) => x,
                            None => return Ok(None),
                        };
                        ops.extend(arg_tys);
                        results.push(result);
                        stack.push(Cell::Ty(result));
                    }
                }
                None => {
                    // The strategy trade accessors and conversions need the
                    // closed-trade list and currency-conversion support that
                    // a linked broker does not carry; in a strategy() program
                    // (the only place they are legal) they refuse with
                    // their names rather than silently demanding `host.vm`. A
                    // host broker carries both.
                    if self.program.metadata.strategy.is_some()
                        && self.broker == StrategyBroker::Linked
                    {
                        if let Some(feature) = strategy_pure_feature(*func) {
                            return Err(AotError::StrategyFeatureUnsupported {
                                feature: feature.to_string(),
                            });
                        }
                    }
                    // No inline/`host.pure` plan: evaluate on the interpreter
                    // host (`Vm::call_pure` — str.*, math.random, timestamps,
                    // calendar functions, everything else).
                    self.host_site[pc] = true;
                    let observed = match self.observe_args(*argc as usize, stack, pc)? {
                        Some(tys) => tys,
                        None => return Ok(None),
                    };
                    // `pure_result_ty` wants popped types top-of-stack first.
                    let popped: Vec<AbstractTy> =
                        observed.iter().rev().map(|t| vty_abstract(*t)).collect();
                    let result = abstract_vty(pinewasm_csil::pure_result_ty(*func, *argc, &popped));
                    ops.extend(observed);
                    self.push_result(result, pc, stack, &mut results);
                }
            },
            I::IntAdd | I::IntSub | I::IntMul | I::IntDiv | I::IntRem => {
                demand_pop!(VTy::I64);
                demand_pop!(VTy::I64);
                ops.extend([VTy::I64, VTy::I64]);
                results.push(VTy::I64);
                stack.push(Cell::Ty(VTy::I64));
            }
            // An immediate widening consumes the numeric value without
            // observing its integer/float runtime tag. Keep both CSIL steps
            // (and their budgets), but carry this intermediate in f64.
            I::PineIntDiv if matches!(code.get(pc + 1), Some(I::IntToF64)) => {
                demand_pop!(VTy::I64);
                demand_pop!(VTy::I64);
                ops.extend([VTy::I64, VTy::I64]);
                results.push(VTy::F64);
                stack.push(Cell::Ty(VTy::F64));
            }
            // A one-argument math.round, math.floor, math.ceil or int()
            // consumes either the exact integer quotient or the fractional
            // float result. Fuse that dynamic numeric distinction into the
            // native integer result rather than pretending every quotient is
            // representable as f64.
            I::PineIntDiv if quotient_rounding(code, pc).is_some() => {
                demand_pop!(VTy::I64);
                demand_pop!(VTy::I64);
                ops.extend([VTy::I64, VTy::I64]);
                results.push(VTy::I64);
                stack.push(Cell::Ty(VTy::I64));
            }
            // A fractional/exact integer union can be widened when the only
            // destination is a proven-unobserved scratch slot. Keep the
            // original store and its instruction charge; the slot's value
            // and written flag are unreachable to Pine code.
            I::PineIntDiv
                if matches!(
                    code.get(pc + 1),
                    Some(I::StoreUserSlotCurrent { slot })
                        if slot_is_unobserved(self.program, *slot)
                ) =>
            {
                demand_pop!(VTy::I64);
                demand_pop!(VTy::I64);
                ops.extend([VTy::I64, VTy::I64]);
                results.push(VTy::F64);
                stack.push(Cell::Ty(VTy::F64));
            }
            // A quotient slot keeps the numeric value in the f64 lane: every
            // read of it is converted at once, so no consumer can tell an
            // exact quotient from an uneven one.
            I::PineIntDiv
                if matches!(
                    code.get(pc + 1),
                    Some(I::StoreUserSlotCurrent { slot })
                        if self.quotient_slots.contains(slot)
                ) =>
            {
                demand_pop!(VTy::I64);
                demand_pop!(VTy::I64);
                ops.extend([VTy::I64, VTy::I64]);
                results.push(VTy::F64);
                stack.push(Cell::Ty(VTy::F64));
            }
            // A comparison fused with the division orders an exact quotient
            // as an integer and an uneven one as a float, like the
            // interpreter; a discarded quotient is never observed. Both carry
            // the numeric value in f64.
            I::PineIntDiv
                if matches!(code.get(pc + 1), Some(I::Pop))
                    || quotient_compare(code, self.blocks, pc).is_some() =>
            {
                demand_pop!(VTy::I64);
                demand_pop!(VTy::I64);
                ops.extend([VTy::I64, VTy::I64]);
                results.push(VTy::F64);
                stack.push(Cell::Ty(VTy::F64));
            }
            // Any other retained fractional integer cell still needs tagged
            // storage.
            I::PineIntDiv => return Err(unsupported(instr)),
            I::IntNeg => {
                demand_pop!(VTy::I64);
                ops.push(VTy::I64);
                results.push(VTy::I64);
                stack.push(Cell::Ty(VTy::I64));
            }
            I::F64Add | I::F64Sub | I::F64Mul | I::F64Div | I::F64Rem => {
                demand_pop!(VTy::F64);
                demand_pop!(VTy::F64);
                ops.extend([VTy::F64, VTy::F64]);
                results.push(VTy::F64);
                stack.push(Cell::Ty(VTy::F64));
            }
            I::F64Neg => {
                demand_pop!(VTy::F64);
                ops.push(VTy::F64);
                results.push(VTy::F64);
                stack.push(Cell::Ty(VTy::F64));
            }
            I::IntToF64 => {
                let value = pop!();
                let ty = if pc > 0
                    && (matches!(
                        code[pc - 1],
                        I::PineIntDiv
                            | I::CallPure {
                                func: PureFunc::MathFloor
                                    | PureFunc::MathCeil
                                    | PureFunc::MathRound
                                    | PureFunc::CastInt,
                                argc: 1
                            }
                    ) || matches!(
                        code[pc - 1],
                        I::ReadUserSlot { slot, .. } if self.quotient_slots.contains(&slot)
                    )) {
                    self.demand_numeric(&value, pc)?
                } else {
                    self.demand(&value, VTy::I64, pc)?
                };
                let Some(ty) = ty else {
                    return Ok(None);
                };
                ops.push(ty);
                results.push(VTy::F64);
                stack.push(Cell::Ty(VTy::F64));
            }
            I::Select => {
                let rhs = pop!();
                let lhs = pop!();
                let cond = pop!();
                if self.demand(&cond, VTy::Bool3, pc)?.is_none() {
                    return Ok(None);
                }
                let joined = match Self::join(&lhs, &rhs) {
                    JoinResult::Same | JoinResult::Changed(_) => match Self::join(&lhs, &rhs) {
                        JoinResult::Same => lhs.clone(),
                        JoinResult::Changed(c) => c,
                        _ => unreachable!(),
                    },
                    JoinResult::PinNeeded(prods, ty) => {
                        if self.pin(&prods, ty)? {
                            return Ok(None);
                        }
                        Cell::Ty(ty)
                    }
                    JoinResult::Conflict(a, b) => {
                        return Err(AotError::TypeConflict(format!(
                            "select arms {a:?} vs {b:?} at pc {pc}"
                        )))
                    }
                };
                if let Cell::Ty(t) = &joined {
                    ops.extend([VTy::Bool3, *t, *t]);
                    results.push(*t);
                }
                stack.push(joined);
            }
            I::CmpGt | I::CmpGe | I::CmpLt | I::CmpLe => {
                let rhs = pop!();
                let lhs = pop!();
                let (l, r) = match (
                    self.demand_numeric(&lhs, pc)?,
                    self.demand_numeric(&rhs, pc)?,
                ) {
                    (Some(l), Some(r)) => (l, r),
                    _ => return Ok(None),
                };
                ops.extend([l, r]);
                results.push(VTy::Bool3);
                stack.push(Cell::Ty(VTy::Bool3));
            }
            I::CmpEq | I::CmpNe => {
                let rhs = pop!();
                let lhs = pop!();
                // Equality is same-type (numeric-polymorphic across int/float).
                let (l, r) = match (&lhs, &rhs) {
                    (Cell::Ty(a), Cell::Ty(b)) => (*a, *b),
                    (Cell::Unknown(p), Cell::Ty(t)) | (Cell::Ty(t), Cell::Unknown(p)) => {
                        if self.pin(&p.clone(), *t)? {
                            return Ok(None);
                        }
                        (*t, *t)
                    }
                    (Cell::Unknown(pa), Cell::Unknown(pb)) => {
                        // `na == na`-shaped: defer so a later real demand on
                        // either side can still pin the true type.
                        self.deferred.push(pa.clone());
                        self.deferred.push(pb.clone());
                        (VTy::F64, VTy::F64)
                    }
                };
                let compatible =
                    l == r || matches!((l, r), (VTy::I64, VTy::F64) | (VTy::F64, VTy::I64));
                if !compatible {
                    return Err(AotError::TypeConflict(format!(
                        "equality between {l:?} and {r:?} at pc {pc}"
                    )));
                }
                // Handle equality is NOT string equality (two table entries
                // can hold the same text) — string comparison executes on the
                // host, which owns the content. Ref equality IS identity
                // (`HandleId` PartialEq), so it stays native, and so does
                // string equality beside a host that gives equal strings one
                // handle. Two color-table handles can name equal colors
                // (a transparency of `0.0` and of `-0.0`), so the host
                // compares those.
                if (l == VTy::Str && !self.canonical_strings)
                    || (self.host_colors && l == VTy::Color)
                {
                    self.host_site[pc] = true;
                }
                ops.extend([l, r]);
                results.push(VTy::Bool3);
                stack.push(Cell::Ty(VTy::Bool3));
            }
            I::BoolAnd | I::BoolOr => {
                demand_pop!(VTy::Bool3);
                demand_pop!(VTy::Bool3);
                ops.extend([VTy::Bool3, VTy::Bool3]);
                results.push(VTy::Bool3);
                stack.push(Cell::Ty(VTy::Bool3));
            }
            I::BoolNot => {
                demand_pop!(VTy::Bool3);
                ops.push(VTy::Bool3);
                results.push(VTy::Bool3);
                stack.push(Cell::Ty(VTy::Bool3));
            }
            I::Jump { .. } | I::JumpIfSlotWritten { .. } | I::Return => {}
            I::JumpIfFalse { .. } | I::JumpIfExact { .. } => {
                demand_pop!(VTy::Bool3);
                ops.push(VTy::Bool3);
            }
            I::PlotF64 { .. } => {
                let cell = pop!();
                match cell {
                    Cell::Ty(VTy::F64) => ops.push(VTy::F64),
                    Cell::Ty(t) => {
                        return Err(AotError::TypeConflict(format!(
                            "PLOT_F64 takes a float, found {t:?} at pc {pc}"
                        )))
                    }
                    Cell::Unknown(p) => {
                        if self.pin(&p.clone(), VTy::F64)? {
                            return Ok(None);
                        }
                        ops.push(VTy::F64);
                    }
                }
            }
            I::FillColor {
                fill_slot,
                color_count,
            } if self.program.fills[*fill_slot as usize]
                .color_roles
                .iter()
                .any(|role| role.value_domain() == "float-or-na") =>
            {
                if !self.linked_host {
                    return Err(unsupported(instr));
                }
                self.host_site[pc] = true;
                let roles = self.program.fills[*fill_slot as usize].color_roles.clone();
                for role in roles.iter().take(*color_count as usize).rev() {
                    let ty = if role.value_domain() == "float-or-na" {
                        VTy::F64
                    } else {
                        VTy::Color
                    };
                    demand_pop!(ty);
                    ops.push(ty);
                }
                ops.reverse();
            }
            I::PlotStyledF64 { color_count, .. }
            | I::VisualColor { color_count, .. }
            | I::FillColor { color_count, .. } => {
                for _ in 0..*color_count {
                    demand_pop!(VTy::Color);
                    ops.push(VTy::Color);
                }
                if matches!(instr, I::PlotStyledF64 { .. }) {
                    let cell = pop!();
                    match cell {
                        Cell::Ty(VTy::F64) => ops.push(VTy::F64),
                        Cell::Ty(t) => {
                            return Err(AotError::TypeConflict(format!(
                                "styled plot takes a float, found {t:?}"
                            )))
                        }
                        Cell::Unknown(p) => {
                            if self.pin(&p.clone(), VTy::F64)? {
                                return Ok(None);
                            }
                            ops.push(VTy::F64);
                        }
                    }
                }
            }
            // ---- interpreter host sites (docs/aot.md §5) --------
            // Operand cells are encoded per their inferred types and decoded
            // by the host into `Value`s; results come back the same way. The
            // semantics run in `Vm::exec_host_site` — the interpreter's own
            // handlers — so no typing rule here changes behavior, only the
            // cell encoding.
            I::CallCollection { func, argc } => {
                if let Some((op, elem)) = self.heap.and_then(|p| p.coll.get(&pc).copied()) {
                    // Native-heap lowering: typed lanes, no host site.
                    // Element stores into a float lane accept ints (widened
                    // at emission, matching the interpreter's `coerce_element`); every other
                    // lane demands its exact type. Void mutators push an
                    // elem-typed na placeholder like the host-site cells.
                    use NativeCollOp as N;
                    macro_rules! demand_elem_pop {
                        () => {{
                            let cell = pop!();
                            let got = if elem == VTy::F64 {
                                self.demand_numeric(&cell, pc)?
                            } else {
                                self.demand(&cell, elem, pc)?
                            };
                            match got {
                                Some(t) => t,
                                None => return Ok(None),
                            }
                        }};
                    }
                    match op {
                        N::NewArray => {
                            let initial = demand_elem_pop!();
                            demand_pop!(VTy::I64);
                            ops.extend([VTy::I64, initial]);
                            results.push(VTy::Ref);
                            stack.push(Cell::Ty(VTy::Ref));
                        }
                        N::FromArgs => {
                            let n = *argc as usize;
                            let mut arg_tys = vec![elem; n];
                            for i in (0..n).rev() {
                                arg_tys[i] = demand_elem_pop!();
                            }
                            ops.extend(arg_tys);
                            results.push(VTy::Ref);
                            stack.push(Cell::Ty(VTy::Ref));
                        }
                        N::Push => {
                            let value = demand_elem_pop!();
                            demand_pop!(VTy::Ref);
                            ops.extend([VTy::Ref, value]);
                            results.push(elem);
                            stack.push(Cell::Ty(elem));
                        }
                        N::Get => {
                            demand_pop!(VTy::I64);
                            demand_pop!(VTy::Ref);
                            ops.extend([VTy::Ref, VTy::I64]);
                            results.push(elem);
                            stack.push(Cell::Ty(elem));
                        }
                        N::Set => {
                            let value = demand_elem_pop!();
                            demand_pop!(VTy::I64);
                            demand_pop!(VTy::Ref);
                            ops.extend([VTy::Ref, VTy::I64, value]);
                            results.push(elem);
                            stack.push(Cell::Ty(elem));
                        }
                        N::Size => {
                            demand_pop!(VTy::Ref);
                            ops.push(VTy::Ref);
                            results.push(VTy::I64);
                            stack.push(Cell::Ty(VTy::I64));
                        }
                        N::Shift | N::PopLast | N::First | N::Last | N::Clear => {
                            demand_pop!(VTy::Ref);
                            ops.push(VTy::Ref);
                            results.push(elem);
                            stack.push(Cell::Ty(elem));
                        }
                        N::Sum | N::Avg | N::Min | N::Max => {
                            // `array.min`/`max` carry a materialized `nth = 0`
                            // the native op does not read (the plan only
                            // admits the literal-zero form); pop it so the
                            // receiver lines up, and record its lane so
                            // emission truncates the right stack depth.
                            if *argc == 2 {
                                demand_pop!(VTy::I64);
                            }
                            demand_pop!(VTy::Ref);
                            ops.push(VTy::Ref);
                            if *argc == 2 {
                                ops.push(VTy::I64);
                            }
                            results.push(VTy::F64);
                            stack.push(Cell::Ty(VTy::F64));
                        }
                        N::Stdev | N::Variance => {
                            // `biased` is a materialized bool: only `false`
                            // selects the sample form, as the interpreter's stdev handler does.
                            demand_pop!(VTy::Bool3);
                            demand_pop!(VTy::Ref);
                            ops.extend([VTy::Ref, VTy::Bool3]);
                            results.push(VTy::F64);
                            stack.push(Cell::Ty(VTy::F64));
                        }
                    }
                    return Ok(Some((ops, results)));
                }
                self.host_site[pc] = true;
                // Element-typed operands (array.push value, array.set value,
                // includes needle, …) demand the receiver's PROVEN element
                // type, and element results carry it — `array<int>` /
                // `array<string>` programs get true lanes instead of the f64
                // default. `operand_reqs` is top-of-stack first, matching
                // the pop order.
                let reqs = func.operand_reqs(*argc);
                let elem = self.elem_facts.site_elem.get(&pc).copied();
                // A proven map's element operands: `put` takes (key, value),
                // top value first; `get`/`contains`/`remove` take the key.
                // The host coerces a typed operand exactly as the
                // interpreter does, so only an untyped producer is pinned,
                // to the declared lane instead of the f64 default.
                let map = self.elem_facts.site_map.get(&pc).copied();
                let map_operand = |i: usize| {
                    let (key, value) = map?;
                    match (func, i) {
                        (pinewasm_csil::CollectionFunc::MapPut, 0) => Some(value),
                        (pinewasm_csil::CollectionFunc::MapPut, 1)
                        | (
                            pinewasm_csil::CollectionFunc::MapGet
                            | pinewasm_csil::CollectionFunc::MapContains
                            | pinewasm_csil::CollectionFunc::MapRemove,
                            0,
                        ) => Some(key),
                        _ => None,
                    }
                };
                let n = *argc as usize;
                let mut popped_rev = Vec::with_capacity(n);
                for i in 0..n {
                    let cell = pop!();
                    let want_elem =
                        matches!(reqs.get(i), Some(pinewasm_hir::CollectionAbstract::Elem));
                    // A collection operand is a heap handle or na, whatever
                    // produced it: an untyped producer (a host object's
                    // field, a drawing getter) takes the handle cell here
                    // instead of the f64 default, which no handle fits.
                    let want_ref =
                        matches!(reqs.get(i), Some(pinewasm_hir::CollectionAbstract::Ref));
                    let map_lane = if want_elem { map_operand(i) } else { None };
                    let ty = match (want_elem, elem, map_lane) {
                        (_, _, Some(lane)) => match self.demand_untyped(cell, lane)? {
                            Some(t) => t,
                            None => return Ok(None),
                        },
                        (true, Some(e), None) => match self.demand(&cell, e, pc)? {
                            Some(t) => t,
                            None => return Ok(None),
                        },
                        _ if want_ref => match self.demand_untyped(cell, VTy::Ref)? {
                            Some(t) => t,
                            None => return Ok(None),
                        },
                        _ => match self.observe(cell, pc)? {
                            Some(t) => t,
                            None => return Ok(None),
                        },
                    };
                    popped_rev.push(ty);
                }
                popped_rev.reverse();
                ops.extend(popped_rev);
                let result = match abstract_vty(pinewasm_csil::collection_result_ty(*func)) {
                    Some(t) => Some(t),
                    // A proven map's element results (`get`, `put`,
                    // `remove`) are values of its value type.
                    None if matches!(
                        func,
                        pinewasm_csil::CollectionFunc::MapGet
                            | pinewasm_csil::CollectionFunc::MapPut
                            | pinewasm_csil::CollectionFunc::MapRemove
                    ) =>
                    {
                        map.map(|(_, value)| value)
                    }
                    // Elem results (and dead void placeholders) take the
                    // proven element type when there is one.
                    None => elem,
                };
                self.push_result(result, pc, stack, &mut results);
            }
            I::NewObject {
                type_id,
                field_count,
            } => {
                if let Some(plan) = self.heap.filter(|p| !p.host_types.contains(type_id)) {
                    // Native heap: demand each field's unified storage type
                    // (float fields accept ints, widened at emission).
                    let n = *field_count as usize;
                    let mut arg_tys = vec![VTy::F64; n];
                    for i in (0..n).rev() {
                        let want = plan.field_ty(*type_id, i as u32);
                        let cell = pop!();
                        let got = if want == VTy::F64 {
                            self.demand_numeric(&cell, pc)?
                        } else {
                            self.demand(&cell, want, pc)?
                        };
                        arg_tys[i] = match got {
                            Some(t) => t,
                            None => return Ok(None),
                        };
                    }
                    ops.extend(arg_tys);
                    results.push(VTy::Ref);
                    stack.push(Cell::Ty(VTy::Ref));
                } else {
                    self.host_site[pc] = true;
                    let observed = match self.observe_args(*field_count as usize, stack, pc)? {
                        Some(tys) => tys,
                        None => return Ok(None),
                    };
                    ops.extend(observed);
                    self.push_result(Some(VTy::Ref), pc, stack, &mut results);
                }
            }
            I::CopyObject { .. } => {
                let native = self.heap.is_some_and(|p| p.site_obj.contains_key(&pc));
                if !native {
                    self.host_site[pc] = true;
                }
                demand_pop!(VTy::Ref);
                ops.push(VTy::Ref);
                if native {
                    results.push(VTy::Ref);
                    stack.push(Cell::Ty(VTy::Ref));
                } else {
                    self.push_result(Some(VTy::Ref), pc, stack, &mut results);
                }
            }
            I::GetField { field_idx } => {
                if let Some((plan, tid)) = self
                    .heap
                    .and_then(|p| p.site_obj.get(&pc).map(|&tid| (p, tid)))
                {
                    demand_pop!(VTy::Ref);
                    ops.push(VTy::Ref);
                    let ty = plan.field_ty(tid, *field_idx);
                    results.push(ty);
                    stack.push(Cell::Ty(ty));
                } else {
                    self.host_site[pc] = true;
                    demand_pop!(VTy::Ref);
                    ops.push(VTy::Ref);
                    // Fields that provably hold heap handles (nested
                    // collections and objects) type as Ref, and a built-in
                    // object's field takes its schema type when the receiver
                    // is proven; everything else is demand-driven.
                    let result = if self.elem_facts.ref_getfields.contains(&pc) {
                        Some(VTy::Ref)
                    } else {
                        self.elem_facts.site_obj.get(&pc).and_then(|&tid| {
                            builtin_field_vty(&self.program.object_types, tid, *field_idx).or_else(
                                || self.elem_facts.field_vty.get(&(tid, *field_idx)).copied(),
                            )
                        })
                    };
                    self.push_result(result, pc, stack, &mut results);
                }
            }
            I::SetField { field_idx } => {
                if let Some((plan, tid)) = self
                    .heap
                    .and_then(|p| p.site_obj.get(&pc).map(|&tid| (p, tid)))
                {
                    let want = plan.field_ty(tid, *field_idx);
                    let cell = pop!();
                    let got = if want == VTy::F64 {
                        self.demand_numeric(&cell, pc)?
                    } else {
                        self.demand(&cell, want, pc)?
                    };
                    let value = match got {
                        Some(t) => t,
                        None => return Ok(None),
                    };
                    demand_pop!(VTy::Ref);
                    ops.extend([VTy::Ref, value]);
                } else {
                    self.host_site[pc] = true;
                    let value = match self.observe(pop!(), pc)? {
                        Some(t) => t,
                        None => return Ok(None),
                    };
                    demand_pop!(VTy::Ref);
                    ops.extend([VTy::Ref, value]);
                }
            }
            I::RegisterDrawing { .. } => {
                self.host_site[pc] = true;
                demand_pop!(VTy::Ref);
                ops.push(VTy::Ref);
                self.push_result(Some(VTy::Ref), pc, stack, &mut results);
            }
            I::ReadDrawingsAll { .. } => {
                self.host_site[pc] = true;
                self.push_result(Some(VTy::Ref), pc, stack, &mut results);
            }
            I::DeleteDrawing { .. } => {
                self.host_site[pc] = true;
                demand_pop!(VTy::Ref);
                ops.push(VTy::Ref);
            }
            I::SetCell { attrs } => {
                self.host_site[pc] = true;
                let observed = match self.observe_args(attrs.len() + 2, stack, pc)? {
                    Some(tys) => tys,
                    None => return Ok(None),
                };
                demand_pop!(VTy::Ref);
                ops.push(VTy::Ref);
                ops.extend(observed);
            }
            I::ClearCells | I::MergeCells => {
                self.host_site[pc] = true;
                let observed = match self.observe_args(4, stack, pc)? {
                    Some(tys) => tys,
                    None => return Ok(None),
                };
                demand_pop!(VTy::Ref);
                ops.push(VTy::Ref);
                ops.extend(observed);
            }
            I::Log { argc, .. } => {
                self.host_site[pc] = true;
                let n = *argc as usize;
                let subs = match self.observe_args(n.saturating_sub(1), stack, pc)? {
                    Some(tys) => tys,
                    None => return Ok(None),
                };
                if n > 0 {
                    demand_pop!(VTy::Str);
                    ops.push(VTy::Str);
                }
                ops.extend(subs);
            }
            I::Alert => {
                self.host_site[pc] = true;
                demand_pop!(VTy::Str); // freq (top)
                demand_pop!(VTy::Str); // message
                ops.extend([VTy::Str, VTy::Str]);
            }
            I::StrConcat => {
                self.host_site[pc] = true;
                demand_pop!(VTy::Str);
                demand_pop!(VTy::Str);
                ops.extend([VTy::Str, VTy::Str]);
                self.push_result(Some(VTy::Str), pc, stack, &mut results);
            }
            I::RuntimeError => {
                self.host_site[pc] = true;
                demand_pop!(VTy::Str);
                ops.push(VTy::Str);
            }
            I::ReadSecurity { .. } | I::ReadSecurityExpr { .. } | I::ReadRequest { .. } => {
                self.host_site[pc] = true;
                self.push_result(Some(VTy::F64), pc, stack, &mut results);
            }
            I::ReadTimeTf { .. } => {
                self.host_site[pc] = true;
                self.push_result(Some(VTy::I64), pc, stack, &mut results);
            }
            I::BuildFootprint { .. }
            | I::ReadSecurityLowerTf { .. }
            | I::ReadPivotLevels { .. } => {
                self.host_site[pc] = true;
                self.push_result(Some(VTy::Ref), pc, stack, &mut results);
            }
            I::CallPivotPointLevels { .. } => {
                self.host_site[pc] = true;
                demand_pop!(VTy::Bool3); // developing
                demand_pop!(VTy::Bool3); // anchor
                demand_pop!(VTy::Str); // type
                                       // The host runs the instruction on these three; without
                                       // them its site has no arguments and the emitter leaves the
                                       // operands on its stack.
                ops.extend([VTy::Str, VTy::Bool3, VTy::Bool3]);
                self.push_result(Some(VTy::Ref), pc, stack, &mut results);
            }
            I::FootprintRowByPrice => {
                self.host_site[pc] = true;
                let price = match self.observe(pop!(), pc)? {
                    Some(t) => t,
                    None => return Ok(None),
                };
                demand_pop!(VTy::Ref);
                ops.extend([VTy::Ref, price]);
                self.push_result(Some(VTy::Ref), pc, stack, &mut results);
            }

            I::Order { kind } => {
                // Native strategy lowering: a linked broker rides inside the
                // module, so the opcode is typed here rather than refused; a
                // host broker takes the same typed cells through `host.vm`.
                // Slot types mirror the semantic layer's operand
                // construction exactly (`analyze_order_call`); Order can
                // only appear in a strategy() program.
                if self.host_broker() {
                    self.host_site[pc] = true;
                }
                let slots = order_slot_vtys(*kind);
                for ty in slots.iter().rev() {
                    demand_pop!(*ty);
                }
                ops.extend(slots);
            }
        }

        Ok(Some((ops, results)))
    }

    /// Whether this is a `strategy()` program whose broker is the host's.
    fn host_broker(&self) -> bool {
        self.program.metadata.strategy.is_some() && self.broker == StrategyBroker::Host
    }

    /// [`pure_plan`] in this analysis: a host broker's trade accessors read
    /// the host's trade ledger, so they are `host.vm` sites, not plans.
    fn pure_plan(&self, func: PureFunc, argc: u8) -> Option<PurePlan> {
        pure_plan(func, argc).filter(|plan| *plan != PurePlan::StrategyTrade || !self.host_broker())
    }

    /// Pop `n` operands (top-first), resolve each to a concrete type, and
    /// return them in ARGUMENT order (deepest first). An Unknown operand is
    /// recorded provisionally as f64 and DEFERRED: a real demand elsewhere
    /// may still pin its producers to their true type; only at convergence
    /// do still-unpinned deferrals default to f64 (na crosses any cell
    /// faithfully; a dynamic non-na value that does not fit aborts into the
    /// interpreter via the host's ABI-mismatch status). `Ok(None)` = restart.
    fn observe_args(
        &mut self,
        n: usize,
        stack: &mut Vec<Cell>,
        pc: usize,
    ) -> Result<Option<Vec<VTy>>, AotError> {
        let mut tys = Vec::with_capacity(n);
        for _ in 0..n {
            let cell = stack
                .pop()
                .ok_or_else(|| AotError::InvalidProgram(format!("stack underflow at pc {pc}")))?;
            match self.observe(cell, pc)? {
                Some(t) => tys.push(t),
                None => return Ok(None),
            }
        }
        tys.reverse();
        Ok(Some(tys))
    }

    /// Resolve one popped cell to a concrete type. Unknown → provisional
    /// f64, deferred for a convergence-time default (never an eager pin, so
    /// a later real demand wins).
    fn observe(&mut self, cell: Cell, _pc: usize) -> Result<Option<VTy>, AotError> {
        match cell {
            Cell::Ty(t) => Ok(Some(t)),
            Cell::Unknown(prods) => {
                self.deferred.push(prods);
                Ok(Some(VTy::F64))
            }
        }
    }

    /// Default the still-unpinned deferred producers: numeric readers first
    /// (f64), then `bool()`-only readers (bool), then the rest (f64). Returns whether
    /// any pin was added (→ restart so provisional records become real).
    fn pin_deferred_defaults(&mut self) -> bool {
        let code = &self.program.main().code;
        // The lists stay: the restart resumes behind a prefix of them.
        for (deferred, default) in [
            (&self.deferred_numeric, VTy::F64),
            (&self.deferred_bool, VTy::Bool3),
            (&self.deferred, VTy::F64),
        ] {
            let mut added = false;
            for &pc in deferred.iter().flatten() {
                match &code[pc] {
                    CsilInstr::ReadUserSlot { slot, .. }
                    | CsilInstr::ReadUserSlotDynamic { slot } => {
                        if self.slot_pin[*slot as usize].is_none() {
                            self.slot_pin[*slot as usize] = Some(default);
                            self.repinned.push(PinVar::Slot(*slot));
                            added = true;
                        }
                    }
                    _ => {
                        if let std::collections::btree_map::Entry::Vacant(slot) =
                            self.pinned.entry(pc)
                        {
                            slot.insert(default);
                            self.repinned.push(PinVar::Producer(pc));
                            added = true;
                        }
                    }
                }
            }
            // One class at a time: the restart carries these pins back
            // through stores and merges before a weaker default can claim a
            // producer the stronger one reaches indirectly.
            if added {
                return true;
            }
        }
        false
    }

    /// Push a host-site result cell: concrete when the opcode's contract
    /// fixes it, otherwise demand-driven (pinned by the first consumer).
    fn push_result(
        &mut self,
        ty: Option<VTy>,
        pc: usize,
        stack: &mut Vec<Cell>,
        results: &mut Vec<VTy>,
    ) {
        match ty {
            Some(t) => {
                results.push(t);
                stack.push(Cell::Ty(t));
            }
            None => {
                let cell = Cell::Unknown(BTreeSet::from([pc])).pinned_or(&self.pinned, pc);
                if let Cell::Ty(t) = &cell {
                    results.push(*t);
                }
                stack.push(cell);
            }
        }
    }

    /// Types for a supported pure call: argument types (in order) and the
    /// result type. `Ok(None)` = restart after a pin.
    fn pure_types(
        &mut self,
        plan: PurePlan,
        func: PureFunc,
        args: &[Cell],
        pc: usize,
    ) -> Result<Option<(Vec<VTy>, VTy)>, AotError> {
        let mut tys = Vec::with_capacity(args.len());
        // Only an immediate widening can elide the observable integer cell.
        let widened = matches!(
            self.program.main().code.get(pc + 1),
            Some(CsilInstr::IntToF64)
        );
        let integer_result = |ty| {
            if ty == VTy::F64 && widened {
                VTy::F64
            } else {
                VTy::I64
            }
        };
        match plan {
            PurePlan::HostF64 | PurePlan::InlineFloat => {
                for a in args {
                    match self.demand(a, VTy::F64, pc)? {
                        Some(t) => tys.push(t),
                        None => return Ok(None),
                    }
                }
                Ok(Some((tys, VTy::F64)))
            }
            PurePlan::FloorCeilRound => {
                // 1-arg forms: numeric in, INT out (identity on ints).
                match self.demand_numeric(&args[0], pc)? {
                    Some(t) => Ok(Some((vec![t], integer_result(t)))),
                    None => Ok(None),
                }
            }
            PurePlan::Round2 => {
                let a = self.demand(&args[0], VTy::F64, pc)?;
                let b = self.demand(&args[1], VTy::I64, pc)?;
                match (a, b) {
                    (Some(_), Some(_)) => Ok(Some((vec![VTy::F64, VTy::I64], VTy::F64))),
                    _ => Ok(None),
                }
            }
            PurePlan::StrategyTrade => {
                let (PureFunc::StrategyOpenTrade(field) | PureFunc::StrategyClosedTrade(field)) =
                    func
                else {
                    return Err(AotError::InvalidProgram(format!(
                        "{} is not a strategy trade accessor",
                        func.name()
                    )));
                };
                let (_, result) = strategy_trade_field(field).ok_or_else(|| {
                    AotError::StrategyFeatureUnsupported {
                        feature: func.name().to_string(),
                    }
                })?;
                match self.demand(&args[0], VTy::I64, pc)? {
                    Some(t) => Ok(Some((vec![t], result))),
                    None => Ok(None),
                }
            }
            // An untyped operand never reaches this arm
            // (`result_follows_untyped_args`), so the demand only checks that
            // the operand is numeric.
            PurePlan::AbsSign => match self.demand_numeric(&args[0], pc)? {
                Some(t) => Ok(Some((vec![t], t))),
                None => Ok(None),
            },
            PurePlan::MinMaxAvg => {
                // All-int stays int (min/max); otherwise everything is f64.
                // avg is always float. An untyped argument reaches this arm
                // only when the result is a float whatever its type turns out
                // to be (`math.avg`, or a float argument beside it);
                // `result_follows_untyped_args` keeps the int-or-float case
                // demand-driven.
                let mut seen = Vec::with_capacity(args.len());
                for a in args {
                    match self.demand_numeric(a, pc)? {
                        Some(t) => seen.push(t),
                        None => return Ok(None),
                    }
                }
                let all_int = seen.iter().all(|t| *t == VTy::I64);
                let result = if all_int && !matches!(func, PureFunc::MathAvg) {
                    VTy::I64
                } else {
                    VTy::F64
                };
                Ok(Some((seen, result)))
            }
            PurePlan::CastInt => match self.demand_numeric(&args[0], pc)? {
                Some(t) => Ok(Some((vec![t], integer_result(t)))),
                None => Ok(None),
            },
            PurePlan::CastFloat => match self.demand_numeric(&args[0], pc)? {
                Some(t) => Ok(Some((vec![t], VTy::F64))),
                None => Ok(None),
            },
            PurePlan::CastBool => match &args[0] {
                Cell::Ty(t @ (VTy::F64 | VTy::I64 | VTy::Bool3)) => {
                    Ok(Some((vec![*t], VTy::Bool3)))
                }
                Cell::Ty(t) => Err(AotError::TypeConflict(format!(
                    "pc {pc}: operand is {t:?}, needs a numeric or bool type"
                ))),
                // CSIL may leave a bare missing operand untyped. The cast
                // takes a number or a bool alike, so it states no type:
                // defer, so a consumer that does state one wins whichever
                // comes first. With no such consumer the operand takes the
                // bool lane, unless a numeric consumer also read it, in
                // which case it is a number; either cast lowers na to false.
                Cell::Unknown(prods) => {
                    self.deferred_bool.push(prods.clone());
                    Ok(Some((vec![VTy::Bool3], VTy::Bool3)))
                }
            },
            PurePlan::Na => {
                // any single value; result is a PLAIN bool (never na). An
                // untyped operand (`na(someRef)` before its slot is typed)
                // defers — a later real demand may pin the true type.
                let t = match &args[0] {
                    Cell::Ty(t) => *t,
                    Cell::Unknown(p) => {
                        self.deferred.push(p.clone());
                        VTy::F64
                    }
                };
                // String handles do not encode content. PineWASM treats the
                // empty string as na here; ask the string-owning host.
                if t == VTy::Str {
                    self.host_site[pc] = true;
                }
                Ok(Some((vec![t], VTy::Bool3)))
            }
            PurePlan::Nz => {
                // nz(x, replacement): both sides one type; result that type.
                let joined = match TypeInference::join(&args[0], &args[1]) {
                    JoinResult::Same | JoinResult::Changed(_) => match (&args[0], &args[1]) {
                        (Cell::Ty(t), _) | (_, Cell::Ty(t)) => Some(*t),
                        _ => None,
                    },
                    JoinResult::PinNeeded(prods, ty) => {
                        if self.pin(&prods, ty)? {
                            return Ok(None);
                        }
                        Some(ty)
                    }
                    JoinResult::Conflict(a, b) => {
                        return Err(AotError::TypeConflict(format!(
                            "nz arms {a:?} vs {b:?} at pc {pc}"
                        )))
                    }
                };
                let t = match joined {
                    Some(t) => t,
                    None => {
                        let mut prods = BTreeSet::new();
                        for a in args {
                            if let Cell::Unknown(p) = a {
                                prods.extend(p.iter().copied());
                            }
                        }
                        self.deferred.push(prods);
                        VTy::F64
                    }
                };
                Ok(Some((vec![t, t], t)))
            }
            PurePlan::ColorChannel => match self.demand(&args[0], VTy::Color, pc)? {
                Some(_) => Ok(Some((
                    vec![VTy::Color],
                    if func.color_channel_is_float() {
                        VTy::F64
                    } else {
                        VTy::I64
                    },
                ))),
                None => Ok(None),
            },
            PurePlan::ColorNew => {
                // Channel/transparency operands are numeric-polymorphic: the
                // compiler widens int literals to float (`pure_color_int`
                // rounds floats half-away and saturates).
                let a = self.demand(&args[0], VTy::Color, pc)?;
                let b = self.demand_numeric(&args[1], pc)?;
                match (a, b) {
                    (Some(_), Some(t)) => Ok(Some((vec![VTy::Color, t], VTy::Color))),
                    _ => Ok(None),
                }
            }
            PurePlan::ColorRgb => {
                for a in args {
                    match self.demand_numeric(a, pc)? {
                        Some(t) => tys.push(t),
                        None => return Ok(None),
                    }
                }
                Ok(Some((tys, VTy::Color)))
            }
            PurePlan::ColorFromGradient => {
                for a in &args[..3] {
                    match self.demand_numeric(a, pc)? {
                        Some(t) => tys.push(t),
                        None => return Ok(None),
                    }
                }
                for a in &args[3..5] {
                    match self.demand(a, VTy::Color, pc)? {
                        Some(t) => tys.push(t),
                        None => return Ok(None),
                    }
                }
                Ok(Some((tys, VTy::Color)))
            }
            PurePlan::RoundToMintick => match self.demand_numeric(&args[0], pc)? {
                Some(t) => Ok(Some((vec![t], VTy::F64))),
                None => Ok(None),
            },
        }
    }
}

/// The still-untyped producers a pure call's result type hangs on: `Some`
/// when the result takes its arguments' type and no argument has decided it
/// yet. `math.abs`/`math.sign` preserve their operand's type, and
/// `math.min`/`math.max` are int exactly when every argument is int, so while
/// their typed arguments are all ints the untyped ones decide. One float
/// argument already makes the result a float, and `math.avg` is always
/// float; those return `None`.
///
/// Typing such a call from an f64 guess for its untyped arguments would decide
/// the result before its consumer can state it. For an int the checker has
/// already emitted that statement (`INT_TO_F64`, an int operator, an int
/// slot), so the guess would surface as a type conflict against the program's
/// own conversion.
fn result_follows_untyped_args(
    plan: PurePlan,
    func: PureFunc,
    args: &[Cell],
) -> Option<BTreeSet<usize>> {
    match plan {
        PurePlan::AbsSign => match args.first() {
            Some(Cell::Unknown(prods)) => Some(prods.clone()),
            _ => None,
        },
        PurePlan::MinMaxAvg if !matches!(func, PureFunc::MathAvg) => {
            let mut prods = BTreeSet::new();
            for arg in args {
                match arg {
                    Cell::Unknown(p) => prods.extend(p.iter().copied()),
                    Cell::Ty(VTy::I64) => {}
                    // A float decides the result; any other type is
                    // `pure_types`' conflict to report.
                    Cell::Ty(_) => return None,
                }
            }
            (!prods.is_empty()).then_some(prods)
        }
        _ => None,
    }
}

trait PinnedOr {
    fn pinned_or(self, pinned: &BTreeMap<usize, VTy>, pc: usize) -> Cell;
}
impl PinnedOr for Cell {
    fn pinned_or(self, pinned: &BTreeMap<usize, VTy>, pc: usize) -> Cell {
        match pinned.get(&pc) {
            Some(&t) => Cell::Ty(t),
            None => self,
        }
    }
}

enum JoinResult {
    Same,
    Changed(Cell),
    PinNeeded(BTreeSet<usize>, VTy),
    Conflict(VTy, VTy),
}

/// Wasm-level type of a supported builtin series read.
pub fn builtin_vty(source: BuiltinSeries) -> Option<VTy> {
    use BuiltinSeries as B;
    Some(match source {
        B::SyminfoMintick
        | B::Open
        | B::High
        | B::Low
        | B::Close
        | B::Volume
        | B::Hl2
        | B::Hlc3
        | B::Ohlc4
        | B::Hlcc4 => VTy::F64,
        // TimeClose/Timenow need close-time synthesis (timeframe parsing);
        // they stay interpreter-only until the loader owns that rule.
        B::BarIndex | B::Time | B::LastBarIndex | B::LastBarTime => VTy::I64,
        B::BarstateIsfirst
        | B::BarstateIslast
        | B::BarstateIshistory
        | B::BarstateIsrealtime
        | B::BarstateIsconfirmed
        | B::BarstateIsnew
        | B::BarstateIslastconfirmedhistory => VTy::Bool3,
        _ => return None,
    })
}

/// Wasm-level type a `READ_INPUT` pushes. String and color inputs resolve
/// through the interpreter host site (`Vm::resolve_input` handles the
/// override map and string interning).
pub fn input_vty(program: &CsilProgram, input: u32) -> Option<VTy> {
    use pinewasm_hir::InputKind as K;
    let decl = program.inputs.get(input as usize)?;
    Some(match decl.kind {
        K::Int | K::Time => VTy::I64,
        K::Float | K::Price => VTy::F64,
        K::Bool => VTy::Bool3,
        // Source inputs read the loader-synthesized lane (always float).
        K::Source => VTy::F64,
        // Enum inputs resolve to the member's variant index.
        K::Enum => VTy::I64,
        K::String => VTy::Str,
        K::Color => VTy::Color,
    })
}

/// Lane type of a builtin series that reads through the `host.vm` import
/// (everything [`builtin_vty`] does not compile inline). Mirrors the value
/// types `Vm::read_builtin` returns. `None` = the read's static type is
/// demand-driven (the always-na host interfaces: chart.*, dividends.*,
/// earnings.*, the unsupplied syminfo fundamentals) — any cell encoding
/// carries na faithfully.
pub fn host_builtin_vty(source: BuiltinSeries) -> Option<VTy> {
    use BuiltinSeries as B;
    Some(match source {
        // Time axis and calendar decompositions: series int.
        B::TimeClose
        | B::Timenow
        | B::TimeTradingday
        | B::DayOfWeek
        | B::DayOfMonth
        | B::Month
        | B::Year
        | B::Hour
        | B::Minute
        | B::Second
        | B::WeekOfYear => VTy::I64,
        // timeframe.*
        B::TimeframeMultiplier => VTy::I64,
        B::TimeframePeriod | B::TimeframeMainPeriod => VTy::Str,
        B::TimeframeIsdwm
        | B::TimeframeIsdaily
        | B::TimeframeIsweekly
        | B::TimeframeIsmonthly
        | B::TimeframeIsintraday
        | B::TimeframeIsminutes
        | B::TimeframeIsseconds
        | B::TimeframeIsticks => VTy::Bool3,
        // session.*
        B::SessionIsfirstbar
        | B::SessionIsfirstbarRegular
        | B::SessionIslastbar
        | B::SessionIslastbarRegular
        | B::SessionIsmarket
        | B::SessionIspremarket
        | B::SessionIspostmarket => VTy::Bool3,
        // syminfo.*
        B::SyminfoMintick | B::SyminfoPointvalue | B::SyminfoMincontract => VTy::F64,
        B::SyminfoMinmove | B::SyminfoPricescale => VTy::I64,
        B::SyminfoBasecurrency
        | B::SyminfoCurrency
        | B::SyminfoDescription
        | B::SyminfoCountry
        | B::SyminfoIndustry
        | B::SyminfoSector
        | B::SyminfoType
        | B::SyminfoRoot
        | B::SyminfoPrefix
        | B::SyminfoSession
        | B::SyminfoTimezone
        | B::SyminfoVolumetype
        | B::SyminfoMainTickerid
        | B::SyminfoTicker
        | B::SyminfoTickerid
        | B::SyminfoIsin
        | B::SyminfoCurrentContract => VTy::Str,
        // Optional fundamentals still have fixed runtime types when supplied.
        // Inferring their type from a consumer is only safe while they are na.
        B::SyminfoEmployees
        | B::SyminfoShareholders
        | B::SyminfoSharesOutstandingTotal
        | B::SyminfoExpirationDate
        | B::SyminfoRecommendationsBuy
        | B::SyminfoRecommendationsBuyStrong
        | B::SyminfoRecommendationsDate
        | B::SyminfoRecommendationsHold
        | B::SyminfoRecommendationsTotal
        | B::SyminfoRecommendationsSell
        | B::SyminfoRecommendationsSellStrong
        | B::SyminfoTargetPriceDate => VTy::I64,
        B::SyminfoSharesOutstandingFloat
        | B::SyminfoTargetPriceAverage
        | B::SyminfoTargetPriceEstimates
        | B::SyminfoTargetPriceHigh
        | B::SyminfoTargetPriceLow
        | B::SyminfoTargetPriceMedian => VTy::F64,
        B::Bid | B::Ask => VTy::F64,
        // strategy.* snapshot reads, typed as the linked broker's snapshot
        // fields are, plus the two text getters.
        B::StrategyAccountCurrency | B::StrategyPositionEntryName => VTy::Str,
        source if source.is_strategy() => return strategy_snap_field(source).map(|(_, ty)| ty),
        _ => return None,
    })
}

/// The cell type of a built-in object's field whose stored value is never a
/// float, from the type's fixed schema (`chart.point`, the drawing kinds).
/// Left to its consumers, such a field read by a host operand that states no
/// type defaults to an f64 cell, which a string or a handle does not fit and
/// which hands an int back to the host as a float.
///
/// The built-in types are registered before any user type, in
/// [`pinewasm_hir::drawing::DrawKind::all`] order, so `type_id` indexes that
/// list; the declared name and field count are checked against it, and
/// anything else (a user-defined type) has no schema here. Float fields are
/// not listed: f64 is already their default, and a consumer may still state
/// it.
fn builtin_field_vty(
    object_types: &[pinewasm_hir::ObjectTypeDecl],
    type_id: u32,
    field_idx: u32,
) -> Option<VTy> {
    use pinewasm_hir::drawing::{DrawFieldTy, DrawKind};
    let kind = *DrawKind::all().get(type_id as usize)?;
    let decl = object_types.get(type_id as usize)?;
    if decl.name != kind.type_name() || decl.field_count as usize != kind.fields().len() {
        return None;
    }
    Some(match kind.fields().get(field_idx as usize)?.ty {
        DrawFieldTy::Float => return None,
        DrawFieldTy::Int => VTy::I64,
        DrawFieldTy::Bool => VTy::Bool3,
        DrawFieldTy::String | DrawFieldTy::TextFormat => VTy::Str,
        DrawFieldTy::Color => VTy::Color,
        DrawFieldTy::ArrayOfPoint | DrawFieldTy::LineObject => VTy::Ref,
    })
}

/// How a pure function is compiled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurePlan {
    /// f64-in/f64-out through the `host.pure` import (computed with libm).
    HostF64,
    /// Inline float math (single wasm ops / short exact sequences).
    InlineFloat,
    /// `math.floor/ceil/round` single-argument: numeric → int.
    FloorCeilRound,
    /// `math.round(value, precision)`: (f64, int) → f64 via `host.pure`.
    Round2,
    /// `math.abs` / `math.sign`: type-preserving numeric.
    AbsSign,
    /// `math.min/max/avg`: n-ary numeric.
    MinMaxAvg,
    /// `int(x)`: numeric input, truncating float to the sentinel-backed int.
    CastInt,
    /// `float(x)`: numeric input, widening a sentinel-backed int when needed.
    CastFloat,
    /// `bool(x)`: numeric-or-bool input, with every na representation false.
    CastBool,
    Na,
    Nz,
    ColorChannel,
    ColorNew,
    ColorRgb,
    /// `color.from_gradient(value, bottom_value, top_value, bottom, top)`.
    ColorFromGradient,
    RoundToMintick,
    /// A numeric `strategy.opentrades.*` / `strategy.closedtrades.*`
    /// accessor: an int `trade_num` in, one broker trade-view read out
    /// (`pinewasm_rt_broker_trade_f64` / `_i64`). Legal only in `strategy()`
    /// programs, whose artifacts always link the broker blob.
    StrategyTrade,
}

/// The supported pure-function set and how each compiles. `None` =
/// interpreter fallback for the whole script.
pub fn pure_plan(func: PureFunc, argc: u8) -> Option<PurePlan> {
    use PureFunc as P;
    Some(match func {
        // host.pure (transcendentals share the interpreter's exact libm —
        // the import lands in the same Rust-compiled module).
        P::MathExp
        | P::MathLog
        | P::MathLog10
        | P::MathSin
        | P::MathCos
        | P::MathTan
        | P::MathAsin
        | P::MathAcos
        | P::MathAtan
        | P::MathPow => PurePlan::HostF64,
        // math.round(value, precision) — 2-arg form routes to the host too,
        // but its precision operand is an INT (the validator enforces it).
        P::MathRound if argc == 2 => PurePlan::Round2,
        // Inline float ops with exact wasm equivalents.
        P::MathSqrt | P::MathToDegrees | P::MathToRadians => PurePlan::InlineFloat,
        P::MathFloor | P::MathCeil => PurePlan::FloorCeilRound,
        P::MathRound => PurePlan::FloorCeilRound,
        P::MathAbs | P::MathSign => PurePlan::AbsSign,
        P::MathMin | P::MathMax | P::MathAvg => PurePlan::MinMaxAvg,
        P::CastInt => PurePlan::CastInt,
        P::CastFloat => PurePlan::CastFloat,
        P::CastBool => PurePlan::CastBool,
        P::Na => PurePlan::Na,
        P::Nz => PurePlan::Nz,
        P::ColorR
        | P::ColorG
        | P::ColorB
        | P::ColorT
        | P::ColorRFloat
        | P::ColorGFloat
        | P::ColorBFloat
        | P::ColorTFloat => PurePlan::ColorChannel,
        P::ColorNew => PurePlan::ColorNew,
        P::ColorRgb => PurePlan::ColorRgb,
        P::ColorFromGradient => PurePlan::ColorFromGradient,
        P::MathRoundToMintick => PurePlan::RoundToMintick,
        P::StrategyOpenTrade(field) | P::StrategyClosedTrade(field)
            if strategy_trade_field(field).is_some() =>
        {
            PurePlan::StrategyTrade
        }
        _ => return None,
    })
}

/// The demanded operand type per `CsilInstr::Order` slot, in slot order —
/// exactly the types the semantic layer constructs (`analyze_order_call`):
/// ids/comments/tokens are strings, quantities and price legs are floats
/// (ints coerced at binding), the two risk `count`s are ints, and
/// `immediately`/`disable_alert` are bools. `body.rs` stages the cells and
/// the broker blob decodes them against this same table
/// (`runtime/src/broker.rs` module docs).
pub(crate) fn order_slot_vtys(kind: OrderKind) -> Vec<VTy> {
    kind.param_names()
        .iter()
        .map(|name| match *name {
            "id" | "from_entry" | "oca_name" | "oca_type" | "comment" | "comment_profit"
            | "comment_loss" | "comment_trailing" | "alert_message" | "alert_profit"
            | "alert_loss" | "alert_trailing" | "direction" => VTy::Str,
            "value" if kind == OrderKind::RiskAllowEntryIn => VTy::Str,
            "type"
                if matches!(
                    kind,
                    OrderKind::RiskMaxDrawdown | OrderKind::RiskMaxIntradayLoss
                ) =>
            {
                VTy::Str
            }
            "count"
                if matches!(
                    kind,
                    OrderKind::RiskMaxIntradayFilledOrders | OrderKind::RiskMaxConsLossDays
                ) =>
            {
                VTy::I64
            }
            "immediately" | "disable_alert" => VTy::Bool3,
            // Every remaining slot is a numeric price/quantity (`value` for
            // the loss thresholds, `contracts`, qty/limit/stop/trail_*).
            _ => VTy::F64,
        })
        .collect()
}

/// The broker-snapshot field id and lane type behind one `strategy.*`
/// getter, mirroring the blob's `pinewasm_rt_broker_snap_f64` / `pinewasm_rt_broker_snap_i64`
/// tables (`runtime/src/broker.rs`). `None` for the two string-typed
/// getters (`strategy.account_currency`, `strategy.position_entry_name`),
/// which have no native lowering and refuse by name.
pub(crate) fn strategy_snap_field(source: BuiltinSeries) -> Option<(u32, VTy)> {
    use BuiltinSeries as B;
    Some(match source {
        B::StrategyPositionSize => (0, VTy::F64),
        B::StrategyPositionAvgPrice => (1, VTy::F64),
        B::StrategyOpenProfit => (2, VTy::F64),
        B::StrategyNetProfit => (3, VTy::F64),
        B::StrategyGrossProfit => (4, VTy::F64),
        B::StrategyGrossLoss => (5, VTy::F64),
        B::StrategyEquity => (6, VTy::F64),
        B::StrategyInitialCapital => (7, VTy::F64),
        B::StrategyMaxDrawdown => (8, VTy::F64),
        B::StrategyMaxRunup => (9, VTy::F64),
        B::StrategyAvgTrade => (10, VTy::F64),
        B::StrategyAvgWinningTrade => (11, VTy::F64),
        B::StrategyAvgLosingTrade => (12, VTy::F64),
        B::StrategyMaxContractsHeldAll => (13, VTy::F64),
        B::StrategyMaxContractsHeldLong => (14, VTy::F64),
        B::StrategyMaxContractsHeldShort => (15, VTy::F64),
        B::StrategyAvgLosingTradePercent => (16, VTy::F64),
        B::StrategyAvgTradePercent => (17, VTy::F64),
        B::StrategyAvgWinningTradePercent => (18, VTy::F64),
        B::StrategyGrossLossPercent => (19, VTy::F64),
        B::StrategyGrossProfitPercent => (20, VTy::F64),
        B::StrategyMarginLiquidationPrice => (21, VTy::F64),
        B::StrategyMaxDrawdownPercent => (22, VTy::F64),
        B::StrategyMaxRunupPercent => (23, VTy::F64),
        B::StrategyNetProfitPercent => (24, VTy::F64),
        B::StrategyOpenProfitPercent => (25, VTy::F64),
        B::StrategyOpenTradesCapitalHeld => (26, VTy::F64),
        B::StrategyWinTrades => (0, VTy::I64),
        B::StrategyLossTrades => (1, VTy::I64),
        B::StrategyEvenTrades => (2, VTy::I64),
        B::StrategyClosedTrades => (3, VTy::I64),
        B::StrategyOpenTrades => (4, VTy::I64),
        B::StrategyClosedTradesFirstIndex => (5, VTy::I64),
        _ => return None,
    })
}

/// The trade-view field id and lane type behind one numeric per-trade
/// accessor, mirroring the blob's `pinewasm_rt_broker_trade_f64` /
/// `pinewasm_rt_broker_trade_i64` tables (`runtime/src/broker.rs`). The
/// open/closed selector is added at the call site. `None` for the
/// string-valued ids and comments, which have no native string story and
/// refuse by name.
pub(crate) fn strategy_trade_field(field: TradeAccessor) -> Option<(u32, VTy)> {
    use TradeAccessor as T;
    Some(match field {
        T::EntryPrice => (0, VTy::F64),
        T::ExitPrice => (1, VTy::F64),
        T::Size => (2, VTy::F64),
        T::Profit => (3, VTy::F64),
        T::ProfitPercent => (4, VTy::F64),
        T::Commission => (5, VTy::F64),
        T::MaxRunup => (6, VTy::F64),
        T::MaxRunupPercent => (7, VTy::F64),
        T::MaxDrawdown => (8, VTy::F64),
        T::MaxDrawdownPercent => (9, VTy::F64),
        T::EntryBarIndex => (0, VTy::I64),
        T::ExitBarIndex => (1, VTy::I64),
        T::EntryTime => (2, VTy::I64),
        T::ExitTime => (3, VTy::I64),
        T::EntryId | T::ExitId | T::EntryComment | T::ExitComment => return None,
    })
}

/// The `field` flag selecting the open-lot list (`TRADE_OPEN` in the blob).
pub(crate) const STRATEGY_TRADE_OPEN: u32 = 0x100;

/// The named feature behind a strategy pure function the native tier
/// refuses: the string-valued trade accessors (ids and comments) have no
/// native string lowering, the conversions need the FX surface, and
/// `default_entry_qty` needs a live mid-bar broker query — all deferred.
/// The numeric trade accessors compile natively (`PurePlan::StrategyTrade`).
fn strategy_pure_feature(func: PureFunc) -> Option<&'static str> {
    Some(match func {
        PureFunc::StrategyClosedTrade(field) | PureFunc::StrategyOpenTrade(field)
            if strategy_trade_field(field).is_none() =>
        {
            func.name()
        }
        PureFunc::StrategyConvertToAccount => "strategy.convert_to_account",
        PureFunc::StrategyConvertToSymbol => "strategy.convert_to_symbol",
        PureFunc::StrategyDefaultEntryQty => "strategy.default_entry_qty",
        _ => return None,
    })
}

#[cfg(test)]
#[test]
fn polyline_snapshot_allocation_is_not_a_fixed_field_charge() {
    let compiled = pinewasm_compiler::compile(
        "//@version=6\nindicator(\"Polyline ownership\")\np=array.from(chart.point.from_index(0,1),chart.point.from_index(1,2))\npolyline.new(p)\n",
        pinewasm_compiler::CompileOptions::default(),
    );
    let program = compiled.program.expect("valid polyline program");
    let instruction = program.main().code.iter().find(|instruction| {
        matches!(instruction, CsilInstr::NewObject { type_id, .. } if program.object_types[*type_id as usize].name == "polyline")
    }).expect("polyline constructor");
    assert_eq!(hosted_alloc(&program, instruction), HostedAlloc::Unbounded);
}
