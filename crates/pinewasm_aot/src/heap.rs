//! The native heap in linear memory (docs/aot.md §13).
//!
//! Helper functions emitted into a module whose analysis produced a
//! [`crate::analysis::HeapPlan`]. Array records live in a bump-allocated
//! region after scratch; handles are the record's byte offset (never 0, so
//! `-1` stays the na handle and 0 doubles as every helper's failure
//! sentinel). Buffers are separate allocations so growth never moves the
//! record — handle identity is `CmpEq` identity, exactly like the
//! interpreter's `HandleId`.
//!
//! Record layout (little-endian, 8-aligned):
//!
//! ```text
//! array  { len: u32, cap: u32, elem_log2: u32, buf: u32 }   16 bytes
//! object { fields: 8 bytes each }                            8 × field_count
//! ```
//!
//! Budget accounting: helpers maintain the run-wide element budget and the
//! arena-slot budget in mutable globals, decrementing on alloc/push and
//! crediting back on `pop`/`shift`/`clear` — `Heap::reserve`/`release`
//! byte-for-byte. Every fallible helper reports failure through its return
//! value (`0` handle or nonzero status); the call site reroutes the run to
//! the interpreter (`status::HEAP_REROUTE`), which reproduces the exact
//! structured error.
//!
//! A module whose plan has a collector ([`crate::analysis::GcPlan`]) also
//! credits back what the interpreter's bar-boundary collection returns. Its
//! records then end in an 8-byte trailer, `{ mark: u32, link: u32 }`: the
//! array record grows to [`ARR_RECORD_GC`] bytes, and an object's trailer
//! follows its fields. `mark` is the epoch of the last collection that
//! reached the record; `link` threads the collection's work list through
//! objects still to scan. Beyond the budget, the collector returns the
//! memory above the last live record to the bump allocator (the interpreter's
//! own collection is observable only by the budget; this keeps a module's
//! memory near what is live when temporaries are its latest allocations).

use crate::analysis::GcRef;
use std::collections::BTreeMap;
use wasm_encoder::{BlockType, Function, Instruction as I, MemArg, ValType};

/// Array record field offsets.
pub const ARR_LEN: u64 = 0;
pub const ARR_CAP: u64 = 4;
pub const ARR_LOG2: u64 = 8;
pub const ARR_BUF: u64 = 12;
/// Array record size (the initial buffer follows it in the same allocation).
pub const ARR_RECORD: i32 = 16;
/// The collector's trailer fields of an array record ([`ARR_RECORD_GC`]).
pub const ARR_MARK: u64 = 16;
/// Array record size in a module with a collector: the record, then the
/// collector's `{ mark, link }` trailer.
pub const ARR_RECORD_GC: i32 = 24;
/// Byte size of an object's fields, after which a module with a collector
/// keeps the object's `{ mark, link }` trailer. An object with no fields
/// still occupies one 8-byte cell, so its handle is a real allocation.
pub fn object_fields_bytes(field_count: usize) -> u64 {
    8 * field_count.max(1) as u64
}

/// Mutable-global indices for the heap runtime, allocated after the lane
/// globals by `Globals::plan`.
#[derive(Debug, Clone, Copy)]
pub struct HeapGlobals {
    /// i32: heap region base (set by `prepare`).
    pub base: u32,
    /// i32: bump pointer (reset to `base` at every `run`).
    pub ptr: u32,
    /// i64: remaining live-element budget.
    pub elems: u32,
    /// i64: remaining live-object (arena slot) budget.
    pub objs: u32,
    /// The collector's globals, for a plan with a collector.
    pub gc: Option<GcGlobals>,
}

/// Mutable globals of the module's collector, after [`HeapGlobals::objs`].
#[derive(Debug, Clone, Copy)]
pub struct GcGlobals {
    /// i64: objects allocated so far this run, on both heaps
    /// (`Heap::arena_len`).
    pub arena: u32,
    /// i64: the arena size at which the next bar boundary collects
    /// (`gc_threshold`).
    pub arena_due: u32,
    /// i64: the remaining element budget at or below which the next bar
    /// boundary collects (`max − gc_elem_threshold`).
    pub elems_due: u32,
    /// i32: the current collection's mark, one more per collection.
    pub epoch: u32,
}

/// The initial `arena_due`: the arena size below which the interpreter
/// never collects (`GC_MIN_ARENA`).
pub const GC_MIN_ARENA: i64 = 4_096;

/// Function indices of the emitted heap helpers.
#[derive(Debug, Clone, Copy)]
pub struct HeapFns {
    pub alloc: u32,
    pub new_f64: u32,
    pub new_i64: u32,
    pub new_i32: u32,
    pub push_f64: u32,
    pub push_i64: u32,
    pub push_i32: u32,
    pub shift_f64: u32,
    pub shift_i64: u32,
    pub shift_i32: u32,
    pub pop_f64: u32,
    pub pop_i64: u32,
    pub pop_i32: u32,
    pub agg_f64: u32,
    pub agg_i64: u32,
    /// `var_f64`/`var_i64`, emitted only when the plan has an
    /// `array.stdev`/`array.variance` site, so every other module keeps its
    /// bytes.
    pub var: Option<(u32, u32)>,
    /// `collect(bar) -> status`, the collector, for a plan with one.
    pub collect: Option<u32>,
}

/// Aggregate selector for `agg_f64`/`agg_i64` (`op` param).
pub mod agg_op {
    pub const SUM: i32 = 0;
    pub const AVG: i32 = 1;
    pub const MIN: i32 = 2;
    pub const MAX: i32 = 3;
}

fn m32(offset: u64) -> MemArg {
    MemArg {
        offset,
        align: 2,
        memory_index: 0,
    }
}
fn m64(offset: u64) -> MemArg {
    MemArg {
        offset,
        align: 3,
        memory_index: 0,
    }
}

/// Largest `alloc` request (unsigned) whose 8-byte round does not wrap u32.
pub const ALLOC_MAX_BYTES: u32 = 0xFFFF_FFF8;
/// Largest bump end (unsigned) whose page count `(end + 0xFFFF) >> 16` does
/// not wrap u32.
pub const ALLOC_MAX_END: u32 = 0xFFFF_0000;

/// [`alloc_fn`] for a `strategy()` module: the same contract (8-aligned,
/// never 0 on success, 0 on failure), served by the runtime's arena
/// allocator.
///
/// The linked broker allocates in the arena band while `run()` executes, so
/// a bump region of the heap's own, growing from below, would run into it.
/// One allocator for both keeps every allocation of a run in one band, which
/// the next run's broker reset rewinds whole.
pub fn arena_alloc_fn(arena_alloc: u32) -> Function {
    let bytes = 0u32;
    let mut f = Function::new([]);
    // bytes > ALLOC_MAX_BYTES -> fail (the round below would wrap to 0, and
    // the arena hands a zero-byte request a pointer with nothing reserved)
    f.instruction(&I::LocalGet(bytes))
        .instruction(&I::I32Const(ALLOC_MAX_BYTES as i32))
        .instruction(&I::I32GtU)
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::I32Const(0))
        .instruction(&I::Return)
        .instruction(&I::End);
    f.instruction(&I::LocalGet(bytes))
        .instruction(&I::I32Const(7))
        .instruction(&I::I32Add)
        .instruction(&I::I32Const(!7))
        .instruction(&I::I32And)
        .instruction(&I::Call(arena_alloc))
        .instruction(&I::End);
    f
}

/// `alloc(bytes: i32) -> i32` — 8-aligned bump allocation, growing memory on
/// demand; 0 on failure (u32 overflow or `memory.grow` refusal).
///
/// `bytes` is unsigned. Every step stays inside u32: a request above
/// [`ALLOC_MAX_BYTES`] fails before the round (which would wrap it to 0 and
/// hand out the bump pointer with nothing reserved), and an end above
/// [`ALLOC_MAX_END`] fails before the page count (which would wrap to 0,
/// skip growth, and publish a bump pointer past linear memory).
pub fn alloc_fn(g: HeapGlobals) -> Function {
    let bytes = 0u32;
    let ret = 1u32;
    let next = 2u32;
    let pages = 3u32;
    let mut f = Function::new([(3, ValType::I32)]);
    // bytes > ALLOC_MAX_BYTES -> fail (the round below would wrap)
    f.instruction(&I::LocalGet(bytes))
        .instruction(&I::I32Const(ALLOC_MAX_BYTES as i32))
        .instruction(&I::I32GtU)
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::I32Const(0))
        .instruction(&I::Return)
        .instruction(&I::End);
    // bytes = (bytes + 7) & ~7
    f.instruction(&I::LocalGet(bytes))
        .instruction(&I::I32Const(7))
        .instruction(&I::I32Add)
        .instruction(&I::I32Const(!7))
        .instruction(&I::I32And)
        .instruction(&I::LocalSet(bytes));
    f.instruction(&I::GlobalGet(g.ptr))
        .instruction(&I::LocalSet(ret));
    // next = ret + bytes; u32 overflow -> fail
    f.instruction(&I::LocalGet(ret))
        .instruction(&I::LocalGet(bytes))
        .instruction(&I::I32Add)
        .instruction(&I::LocalTee(next))
        .instruction(&I::LocalGet(ret))
        .instruction(&I::I32LtU)
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::I32Const(0))
        .instruction(&I::Return)
        .instruction(&I::End);
    // next > ALLOC_MAX_END -> fail (the page count below would wrap)
    f.instruction(&I::LocalGet(next))
        .instruction(&I::I32Const(ALLOC_MAX_END as i32))
        .instruction(&I::I32GtU)
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::I32Const(0))
        .instruction(&I::Return)
        .instruction(&I::End);
    // pages = (next + 0xFFFF) >> 16; grow if beyond current size
    f.instruction(&I::LocalGet(next))
        .instruction(&I::I32Const(0xFFFF))
        .instruction(&I::I32Add)
        .instruction(&I::I32Const(16))
        .instruction(&I::I32ShrU)
        .instruction(&I::LocalSet(pages));
    f.instruction(&I::LocalGet(pages))
        .instruction(&I::MemorySize(0))
        .instruction(&I::I32GtU)
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::LocalGet(pages))
        .instruction(&I::MemorySize(0))
        .instruction(&I::I32Sub)
        .instruction(&I::MemoryGrow(0))
        .instruction(&I::I32Const(-1))
        .instruction(&I::I32Eq)
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::I32Const(0))
        .instruction(&I::Return)
        .instruction(&I::End)
        .instruction(&I::End);
    f.instruction(&I::LocalGet(next))
        .instruction(&I::GlobalSet(g.ptr));
    f.instruction(&I::LocalGet(ret)).instruction(&I::End);
    f
}

/// One element-kind's parameters for the typed helper builders.
#[derive(Debug, Clone, Copy)]
pub struct Lane {
    pub val: ValType,
    pub log2: i32,
}
pub const LANE_F64: Lane = Lane {
    val: ValType::F64,
    log2: 3,
};
pub const LANE_I64: Lane = Lane {
    val: ValType::I64,
    log2: 3,
};
pub const LANE_I32: Lane = Lane {
    val: ValType::I32,
    log2: 2,
};

fn store_elem(f: &mut Function, lane: Lane) {
    match lane.val {
        ValType::F64 => f.instruction(&I::F64Store(m64(0))),
        ValType::I64 => f.instruction(&I::I64Store(m64(0))),
        _ => f.instruction(&I::I32Store(m32(0))),
    };
}
fn load_elem(f: &mut Function, lane: Lane) {
    match lane.val {
        ValType::F64 => f.instruction(&I::F64Load(m64(0))),
        ValType::I64 => f.instruction(&I::I64Load(m64(0))),
        _ => f.instruction(&I::I32Load(m32(0))),
    };
}

/// `new(size: i32, fill: T) -> i32` — allocate an array with `size` elements
/// of `fill`. Charges one arena slot and `size` elements; 0 on any failure
/// (caller reroutes). `size` is pre-validated non-negative by the call site.
/// With a collector the record carries its trailer and the allocation is
/// counted in the arena.
pub fn new_fn(g: HeapGlobals, alloc: u32, lane: Lane) -> Function {
    let record = if g.gc.is_some() {
        ARR_RECORD_GC
    } else {
        ARR_RECORD
    };
    let size = 0u32;
    // param 1 = fill
    let rec = 2u32;
    let cap = 3u32;
    let i = 4u32;
    let mut f = Function::new([(3, ValType::I32)]);
    // objs_left < 1 -> 0
    f.instruction(&I::GlobalGet(g.objs))
        .instruction(&I::I64Const(1))
        .instruction(&I::I64LtS)
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::I32Const(0))
        .instruction(&I::Return)
        .instruction(&I::End);
    // elems_left < size -> 0
    f.instruction(&I::GlobalGet(g.elems))
        .instruction(&I::LocalGet(size))
        .instruction(&I::I64ExtendI32U)
        .instruction(&I::I64LtS)
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::I32Const(0))
        .instruction(&I::Return)
        .instruction(&I::End);
    // cap = max(size, 4)
    f.instruction(&I::LocalGet(size))
        .instruction(&I::I32Const(4))
        .instruction(&I::LocalGet(size))
        .instruction(&I::I32Const(4))
        .instruction(&I::I32GtU)
        .instruction(&I::Select)
        .instruction(&I::LocalSet(cap));
    // cap > (ALLOC_MAX_BYTES - 16) >> log2 -> fail. The call site bounds
    // `size` only by i32::MAX and the host-written element budget, so with
    // a budget of 2^29 (2^30 for the i32 lane) or more, `16 + (cap << log2)`
    // would wrap u32 into a small allocation the fill loop then overruns.
    f.instruction(&I::LocalGet(cap))
        .instruction(&I::I32Const(
            ((ALLOC_MAX_BYTES - record as u32) >> lane.log2) as i32,
        ))
        .instruction(&I::I32GtU)
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::I32Const(0))
        .instruction(&I::Return)
        .instruction(&I::End);
    // rec = alloc(record + (cap << log2)); 0 -> fail
    f.instruction(&I::I32Const(record))
        .instruction(&I::LocalGet(cap))
        .instruction(&I::I32Const(lane.log2))
        .instruction(&I::I32Shl)
        .instruction(&I::I32Add)
        .instruction(&I::Call(alloc))
        .instruction(&I::LocalTee(rec))
        .instruction(&I::I32Eqz)
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::I32Const(0))
        .instruction(&I::Return)
        .instruction(&I::End);
    // record fields
    f.instruction(&I::LocalGet(rec))
        .instruction(&I::LocalGet(size))
        .instruction(&I::I32Store(m32(ARR_LEN)));
    f.instruction(&I::LocalGet(rec))
        .instruction(&I::LocalGet(cap))
        .instruction(&I::I32Store(m32(ARR_CAP)));
    f.instruction(&I::LocalGet(rec))
        .instruction(&I::I32Const(lane.log2))
        .instruction(&I::I32Store(m32(ARR_LOG2)));
    f.instruction(&I::LocalGet(rec))
        .instruction(&I::LocalGet(rec))
        .instruction(&I::I32Const(record))
        .instruction(&I::I32Add)
        .instruction(&I::I32Store(m32(ARR_BUF)));
    if let Some(gc) = g.gc {
        // trailer { mark: 0, link: 0 } (no epoch is 0), and one more
        // object in the arena
        f.instruction(&I::LocalGet(rec))
            .instruction(&I::I64Const(0))
            .instruction(&I::I64Store(m64(ARR_MARK)));
        f.instruction(&I::GlobalGet(gc.arena))
            .instruction(&I::I64Const(1))
            .instruction(&I::I64Add)
            .instruction(&I::GlobalSet(gc.arena));
    }
    // budgets
    f.instruction(&I::GlobalGet(g.objs))
        .instruction(&I::I64Const(1))
        .instruction(&I::I64Sub)
        .instruction(&I::GlobalSet(g.objs));
    f.instruction(&I::GlobalGet(g.elems))
        .instruction(&I::LocalGet(size))
        .instruction(&I::I64ExtendI32U)
        .instruction(&I::I64Sub)
        .instruction(&I::GlobalSet(g.elems));
    // fill loop: i = 0; while i < size { buf[i] = fill; i++ }
    f.instruction(&I::I32Const(0)).instruction(&I::LocalSet(i));
    f.instruction(&I::Block(BlockType::Empty))
        .instruction(&I::Loop(BlockType::Empty))
        .instruction(&I::LocalGet(i))
        .instruction(&I::LocalGet(size))
        .instruction(&I::I32GeU)
        .instruction(&I::BrIf(1));
    f.instruction(&I::LocalGet(rec))
        .instruction(&I::I32Const(record))
        .instruction(&I::I32Add)
        .instruction(&I::LocalGet(i))
        .instruction(&I::I32Const(lane.log2))
        .instruction(&I::I32Shl)
        .instruction(&I::I32Add)
        .instruction(&I::LocalGet(1));
    store_elem(&mut f, lane);
    f.instruction(&I::LocalGet(i))
        .instruction(&I::I32Const(1))
        .instruction(&I::I32Add)
        .instruction(&I::LocalSet(i))
        .instruction(&I::Br(0))
        .instruction(&I::End)
        .instruction(&I::End);
    f.instruction(&I::LocalGet(rec)).instruction(&I::End);
    f
}

/// `push(h: i32, v: T) -> i32` — append; doubles the buffer when full.
/// Returns 0 on success, 1 on element-budget or allocation failure.
pub fn push_fn(g: HeapGlobals, alloc: u32, lane: Lane) -> Function {
    let h = 0u32;
    // param 1 = value
    let len = 2u32;
    let cap = 3u32;
    let newbuf = 4u32;
    let mut f = Function::new([(3, ValType::I32)]);
    // elems_left < 1 -> 1
    f.instruction(&I::GlobalGet(g.elems))
        .instruction(&I::I64Const(1))
        .instruction(&I::I64LtS)
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::I32Const(1))
        .instruction(&I::Return)
        .instruction(&I::End);
    f.instruction(&I::LocalGet(h))
        .instruction(&I::I32Load(m32(ARR_LEN)))
        .instruction(&I::LocalSet(len));
    f.instruction(&I::LocalGet(h))
        .instruction(&I::I32Load(m32(ARR_CAP)))
        .instruction(&I::LocalSet(cap));
    // grow when full
    f.instruction(&I::LocalGet(len))
        .instruction(&I::LocalGet(cap))
        .instruction(&I::I32GeU)
        .instruction(&I::If(BlockType::Empty));
    {
        // newbuf = alloc((cap * 2) << log2); 0 -> 1. Cannot wrap: `cap`
        // elements are already backed by a successful allocation of
        // `cap << log2` bytes, which is at most the declared memory maximum
        // (4096 pages = 2^28 bytes), so the doubled size is at most 2^29.
        f.instruction(&I::LocalGet(cap))
            .instruction(&I::I32Const(1))
            .instruction(&I::I32Shl)
            .instruction(&I::I32Const(lane.log2))
            .instruction(&I::I32Shl)
            .instruction(&I::Call(alloc))
            .instruction(&I::LocalTee(newbuf))
            .instruction(&I::I32Eqz)
            .instruction(&I::If(BlockType::Empty))
            .instruction(&I::I32Const(1))
            .instruction(&I::Return)
            .instruction(&I::End);
        // copy len elements
        f.instruction(&I::LocalGet(newbuf))
            .instruction(&I::LocalGet(h))
            .instruction(&I::I32Load(m32(ARR_BUF)))
            .instruction(&I::LocalGet(len))
            .instruction(&I::I32Const(lane.log2))
            .instruction(&I::I32Shl)
            .instruction(&I::MemoryCopy {
                src_mem: 0,
                dst_mem: 0,
            });
        f.instruction(&I::LocalGet(h))
            .instruction(&I::LocalGet(newbuf))
            .instruction(&I::I32Store(m32(ARR_BUF)));
        f.instruction(&I::LocalGet(h))
            .instruction(&I::LocalGet(cap))
            .instruction(&I::I32Const(1))
            .instruction(&I::I32Shl)
            .instruction(&I::I32Store(m32(ARR_CAP)));
    }
    f.instruction(&I::End);
    // buf[len] = v
    f.instruction(&I::LocalGet(h))
        .instruction(&I::I32Load(m32(ARR_BUF)))
        .instruction(&I::LocalGet(len))
        .instruction(&I::I32Const(lane.log2))
        .instruction(&I::I32Shl)
        .instruction(&I::I32Add)
        .instruction(&I::LocalGet(1));
    store_elem(&mut f, lane);
    // len++, elems_left--
    f.instruction(&I::LocalGet(h))
        .instruction(&I::LocalGet(len))
        .instruction(&I::I32Const(1))
        .instruction(&I::I32Add)
        .instruction(&I::I32Store(m32(ARR_LEN)));
    f.instruction(&I::GlobalGet(g.elems))
        .instruction(&I::I64Const(1))
        .instruction(&I::I64Sub)
        .instruction(&I::GlobalSet(g.elems));
    f.instruction(&I::I32Const(0)).instruction(&I::End);
    f
}

/// `shift(h: i32) -> T` — remove and return element 0. The call site
/// pre-checks the handle and non-empty length and charges the linear cost.
pub fn shift_fn(g: HeapGlobals, lane: Lane) -> Function {
    let h = 0u32;
    let buf = 1u32;
    let len = 2u32;
    let out = 3u32;
    let mut f = Function::new([(2, ValType::I32), (1, lane.val)]);
    f.instruction(&I::LocalGet(h))
        .instruction(&I::I32Load(m32(ARR_BUF)))
        .instruction(&I::LocalSet(buf));
    f.instruction(&I::LocalGet(h))
        .instruction(&I::I32Load(m32(ARR_LEN)))
        .instruction(&I::LocalSet(len));
    f.instruction(&I::LocalGet(buf));
    load_elem(&mut f, lane);
    f.instruction(&I::LocalSet(out));
    // memmove down one element
    f.instruction(&I::LocalGet(buf))
        .instruction(&I::LocalGet(buf))
        .instruction(&I::I32Const(1 << lane.log2))
        .instruction(&I::I32Add)
        .instruction(&I::LocalGet(len))
        .instruction(&I::I32Const(1))
        .instruction(&I::I32Sub)
        .instruction(&I::I32Const(lane.log2))
        .instruction(&I::I32Shl)
        .instruction(&I::MemoryCopy {
            src_mem: 0,
            dst_mem: 0,
        });
    f.instruction(&I::LocalGet(h))
        .instruction(&I::LocalGet(len))
        .instruction(&I::I32Const(1))
        .instruction(&I::I32Sub)
        .instruction(&I::I32Store(m32(ARR_LEN)));
    f.instruction(&I::GlobalGet(g.elems))
        .instruction(&I::I64Const(1))
        .instruction(&I::I64Add)
        .instruction(&I::GlobalSet(g.elems));
    f.instruction(&I::LocalGet(out)).instruction(&I::End);
    f
}

/// `pop(h: i32) -> T` — remove and return the last element. Same call-site
/// contract as `shift`.
pub fn pop_fn(g: HeapGlobals, lane: Lane) -> Function {
    let h = 0u32;
    let len = 1u32;
    let mut f = Function::new([(1, ValType::I32)]);
    f.instruction(&I::LocalGet(h))
        .instruction(&I::I32Load(m32(ARR_LEN)))
        .instruction(&I::I32Const(1))
        .instruction(&I::I32Sub)
        .instruction(&I::LocalSet(len));
    f.instruction(&I::LocalGet(h))
        .instruction(&I::LocalGet(len))
        .instruction(&I::I32Store(m32(ARR_LEN)));
    f.instruction(&I::GlobalGet(g.elems))
        .instruction(&I::I64Const(1))
        .instruction(&I::I64Add)
        .instruction(&I::GlobalSet(g.elems));
    f.instruction(&I::LocalGet(h))
        .instruction(&I::I32Load(m32(ARR_BUF)))
        .instruction(&I::LocalGet(len))
        .instruction(&I::I32Const(lane.log2))
        .instruction(&I::I32Shl)
        .instruction(&I::I32Add);
    load_elem(&mut f, lane);
    f.instruction(&I::End);
    f
}

/// `agg_f64(h: i32, op: i32) -> f64` / `agg_i64` — the na-skipping
/// aggregates in element order (`ArraySum`/`Avg`/`Min`/`Max` rules: skip
/// na, empty/all-na → na, non-finite results canonicalize to na). The i64
/// variant widens each non-sentinel element exactly like `value_as_f64`.
pub fn agg_fn(int_lane: bool) -> Function {
    let h = 0u32;
    let op = 1u32;
    let buf = 2u32;
    let len = 3u32;
    let i = 4u32;
    let cnt = 5u32;
    let acc = 6u32;
    let v = 7u32;
    let iv = 8u32;
    let mut f = Function::new([
        (4, ValType::I32), // buf, len, i, cnt
        (2, ValType::F64), // acc, v
        (1, ValType::I64), // iv
    ]);
    f.instruction(&I::LocalGet(h))
        .instruction(&I::I32Load(m32(ARR_BUF)))
        .instruction(&I::LocalSet(buf));
    f.instruction(&I::LocalGet(h))
        .instruction(&I::I32Load(m32(ARR_LEN)))
        .instruction(&I::LocalSet(len));
    // acc = op == MIN ? +inf : op == MAX ? -inf : 0
    f.instruction(&I::LocalGet(op))
        .instruction(&I::I32Const(agg_op::MIN))
        .instruction(&I::I32Eq)
        .instruction(&I::If(BlockType::Result(ValType::F64)))
        .instruction(&I::F64Const(f64::INFINITY.into()))
        .instruction(&I::Else)
        .instruction(&I::LocalGet(op))
        .instruction(&I::I32Const(agg_op::MAX))
        .instruction(&I::I32Eq)
        .instruction(&I::If(BlockType::Result(ValType::F64)))
        .instruction(&I::F64Const(f64::NEG_INFINITY.into()))
        .instruction(&I::Else)
        .instruction(&I::F64Const(0f64.into()))
        .instruction(&I::End)
        .instruction(&I::End)
        .instruction(&I::LocalSet(acc));
    // loop
    f.instruction(&I::I32Const(0)).instruction(&I::LocalSet(i));
    f.instruction(&I::Block(BlockType::Empty))
        .instruction(&I::Loop(BlockType::Empty))
        .instruction(&I::LocalGet(i))
        .instruction(&I::LocalGet(len))
        .instruction(&I::I32GeU)
        .instruction(&I::BrIf(1));
    // v = elem[i] (widen / na-skip)
    f.instruction(&I::LocalGet(buf))
        .instruction(&I::LocalGet(i))
        .instruction(&I::I32Const(3))
        .instruction(&I::I32Shl)
        .instruction(&I::I32Add);
    if int_lane {
        f.instruction(&I::I64Load(m64(0)))
            .instruction(&I::LocalSet(iv));
        // sentinel -> skip
        f.instruction(&I::LocalGet(iv))
            .instruction(&I::I64Const(i64::MIN))
            .instruction(&I::I64Ne)
            .instruction(&I::If(BlockType::Empty));
        f.instruction(&I::LocalGet(iv))
            .instruction(&I::F64ConvertI64S)
            .instruction(&I::LocalSet(v));
    } else {
        f.instruction(&I::F64Load(m64(0)))
            .instruction(&I::LocalSet(v));
        // NaN -> skip
        f.instruction(&I::LocalGet(v))
            .instruction(&I::LocalGet(v))
            .instruction(&I::F64Eq)
            .instruction(&I::If(BlockType::Empty));
    }
    f.instruction(&I::LocalGet(cnt))
        .instruction(&I::I32Const(1))
        .instruction(&I::I32Add)
        .instruction(&I::LocalSet(cnt));
    // acc = op<=AVG ? acc+v : op==MIN ? min : max
    f.instruction(&I::LocalGet(op))
        .instruction(&I::I32Const(agg_op::AVG))
        .instruction(&I::I32LeS)
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::LocalGet(acc))
        .instruction(&I::LocalGet(v))
        .instruction(&I::F64Add)
        .instruction(&I::LocalSet(acc))
        .instruction(&I::Else)
        .instruction(&I::LocalGet(op))
        .instruction(&I::I32Const(agg_op::MIN))
        .instruction(&I::I32Eq)
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::LocalGet(acc))
        .instruction(&I::LocalGet(v))
        .instruction(&I::F64Min)
        .instruction(&I::LocalSet(acc))
        .instruction(&I::Else)
        .instruction(&I::LocalGet(acc))
        .instruction(&I::LocalGet(v))
        .instruction(&I::F64Max)
        .instruction(&I::LocalSet(acc))
        .instruction(&I::End)
        .instruction(&I::End);
    f.instruction(&I::End); // skip guard
    f.instruction(&I::LocalGet(i))
        .instruction(&I::I32Const(1))
        .instruction(&I::I32Add)
        .instruction(&I::LocalSet(i))
        .instruction(&I::Br(0))
        .instruction(&I::End)
        .instruction(&I::End);
    // cnt == 0 -> NaN
    f.instruction(&I::LocalGet(cnt))
        .instruction(&I::I32Eqz)
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::F64Const(f64::NAN.into()))
        .instruction(&I::Return)
        .instruction(&I::End);
    // avg divides by the numeric count
    f.instruction(&I::LocalGet(op))
        .instruction(&I::I32Const(agg_op::AVG))
        .instruction(&I::I32Eq)
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::LocalGet(acc))
        .instruction(&I::LocalGet(cnt))
        .instruction(&I::F64ConvertI32U)
        .instruction(&I::F64Div)
        .instruction(&I::LocalSet(acc))
        .instruction(&I::End);
    // canonicalize non-finite -> NaN (as `Value::from_f64` does)
    f.instruction(&I::LocalGet(acc))
        .instruction(&I::F64Abs)
        .instruction(&I::F64Const(f64::INFINITY.into()))
        .instruction(&I::F64Eq)
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::F64Const(f64::NAN.into()))
        .instruction(&I::Return)
        .instruction(&I::End);
    f.instruction(&I::LocalGet(acc)).instruction(&I::End);
    f
}

/// `var_f64(h: i32, biased: i32, stdev: i32) -> f64` / `var_i64` —
/// `array.variance` and `array.stdev` (the `variance_with` arithmetic). The numeric
/// elements are read in element order, skipping na (the i64 variant widens
/// each non-sentinel element like `value_as_f64`). The mean is their sum,
/// added left to right from `-0.0` as Rust's `f64` sum does, over their count;
/// the variance is the sum of squared deviations, added the same way, over the
/// count (`biased` is any bool but `false`) or the count less one. `stdev`
/// takes the square root. No numeric element, or a result that is not finite,
/// gives NaN (na), as `Value::from_f64` does.
pub fn var_fn(int_lane: bool) -> Function {
    let h = 0u32;
    let biased = 1u32;
    let stdev = 2u32;
    let buf = 3u32;
    let len = 4u32;
    let i = 5u32;
    let cnt = 6u32;
    let acc = 7u32;
    let v = 8u32;
    let mean = 9u32;
    let iv = 10u32;
    let mut f = Function::new([
        (4, ValType::I32), // buf, len, i, cnt
        (3, ValType::F64), // acc, v, mean
        (1, ValType::I64), // iv
    ]);
    f.instruction(&I::LocalGet(h))
        .instruction(&I::I32Load(m32(ARR_BUF)))
        .instruction(&I::LocalSet(buf));
    f.instruction(&I::LocalGet(h))
        .instruction(&I::I32Load(m32(ARR_LEN)))
        .instruction(&I::LocalSet(len));
    // One pass over the elements: `v` = element `i`, and `body` runs only
    // when it is numeric.
    let pass = |f: &mut Function, body: &dyn Fn(&mut Function)| {
        f.instruction(&I::F64Const((-0.0f64).into()))
            .instruction(&I::LocalSet(acc))
            .instruction(&I::I32Const(0))
            .instruction(&I::LocalSet(i));
        f.instruction(&I::Block(BlockType::Empty))
            .instruction(&I::Loop(BlockType::Empty))
            .instruction(&I::LocalGet(i))
            .instruction(&I::LocalGet(len))
            .instruction(&I::I32GeU)
            .instruction(&I::BrIf(1));
        f.instruction(&I::LocalGet(buf))
            .instruction(&I::LocalGet(i))
            .instruction(&I::I32Const(3))
            .instruction(&I::I32Shl)
            .instruction(&I::I32Add);
        if int_lane {
            f.instruction(&I::I64Load(m64(0)))
                .instruction(&I::LocalSet(iv));
            // sentinel -> skip
            f.instruction(&I::LocalGet(iv))
                .instruction(&I::I64Const(i64::MIN))
                .instruction(&I::I64Ne)
                .instruction(&I::If(BlockType::Empty));
            f.instruction(&I::LocalGet(iv))
                .instruction(&I::F64ConvertI64S)
                .instruction(&I::LocalSet(v));
        } else {
            f.instruction(&I::F64Load(m64(0)))
                .instruction(&I::LocalSet(v));
            // only finite values count (`value_as_f64`): |v| < inf
            f.instruction(&I::LocalGet(v))
                .instruction(&I::F64Abs)
                .instruction(&I::F64Const(f64::INFINITY.into()))
                .instruction(&I::F64Lt)
                .instruction(&I::If(BlockType::Empty));
        }
        body(f);
        f.instruction(&I::End); // skip guard
        f.instruction(&I::LocalGet(i))
            .instruction(&I::I32Const(1))
            .instruction(&I::I32Add)
            .instruction(&I::LocalSet(i))
            .instruction(&I::Br(0))
            .instruction(&I::End)
            .instruction(&I::End);
    };
    // mean: cnt += 1; acc += v
    pass(&mut f, &|f: &mut Function| {
        f.instruction(&I::LocalGet(cnt))
            .instruction(&I::I32Const(1))
            .instruction(&I::I32Add)
            .instruction(&I::LocalSet(cnt))
            .instruction(&I::LocalGet(acc))
            .instruction(&I::LocalGet(v))
            .instruction(&I::F64Add)
            .instruction(&I::LocalSet(acc));
    });
    // cnt == 0 -> NaN
    f.instruction(&I::LocalGet(cnt))
        .instruction(&I::I32Eqz)
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::F64Const(f64::NAN.into()))
        .instruction(&I::Return)
        .instruction(&I::End);
    f.instruction(&I::LocalGet(acc))
        .instruction(&I::LocalGet(cnt))
        .instruction(&I::F64ConvertI32U)
        .instruction(&I::F64Div)
        .instruction(&I::LocalSet(mean));
    // squared deviations: acc += (v - mean) * (v - mean)
    pass(&mut f, &|f: &mut Function| {
        f.instruction(&I::LocalGet(acc))
            .instruction(&I::LocalGet(v))
            .instruction(&I::LocalGet(mean))
            .instruction(&I::F64Sub)
            .instruction(&I::LocalTee(v))
            .instruction(&I::LocalGet(v))
            .instruction(&I::F64Mul)
            .instruction(&I::F64Add)
            .instruction(&I::LocalSet(acc));
    });
    // acc / (biased ? cnt : cnt - 1)
    f.instruction(&I::LocalGet(acc))
        .instruction(&I::LocalGet(cnt))
        .instruction(&I::F64ConvertI32U)
        .instruction(&I::LocalGet(biased))
        .instruction(&I::I32Eqz)
        .instruction(&I::If(BlockType::Result(ValType::F64)))
        .instruction(&I::F64Const(1f64.into()))
        .instruction(&I::Else)
        .instruction(&I::F64Const(0f64.into()))
        .instruction(&I::End)
        .instruction(&I::F64Sub)
        .instruction(&I::F64Div)
        .instruction(&I::LocalSet(acc));
    f.instruction(&I::LocalGet(stdev))
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::LocalGet(acc))
        .instruction(&I::F64Sqrt)
        .instruction(&I::LocalSet(acc))
        .instruction(&I::End);
    // canonicalize non-finite -> NaN (as `Value::from_f64` does)
    f.instruction(&I::LocalGet(acc))
        .instruction(&I::F64Abs)
        .instruction(&I::F64Const(f64::INFINITY.into()))
        .instruction(&I::F64Eq)
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::F64Const(f64::NAN.into()))
        .instruction(&I::Return)
        .instruction(&I::End);
    f.instruction(&I::LocalGet(acc)).instruction(&I::End);
    f
}

/// One root of the collector, as emitted: every cell of a slot's history
/// lane from the current bar back `depth` bars.
#[derive(Debug, Clone, Copy)]
pub struct CollectRoot {
    /// The global holding the lane's base address.
    pub lane: u32,
    /// The lane's ring mask.
    pub mask: u32,
    pub depth: u32,
    pub kind: GcRef,
}

/// The `host.vm` side of a collection, for a plan whose host heap can hold
/// objects.
#[derive(Debug, Clone, Copy)]
pub struct CollectHost {
    /// The `host.vm` import.
    pub vm: u32,
    /// The global holding the scratch region's address.
    pub scratch_base: u32,
    /// The scratch region's size in cells.
    pub cells: u32,
    /// `stop(code, pc, bar)`, which records a failed host call's status.
    pub stop: u32,
}

/// `collect(bar) -> status`: the interpreter's bar-boundary collection
/// (`Vm::maybe_collect_heap` after its pressure test, which the caller makes)
/// for this module's two heaps.
///
/// It marks every native record reachable from the roots, tracing objects
/// through their reference fields with one work list per type threaded
/// through the records' `link` words, and sums their elements and objects.
/// Every host reference it meets goes to the host in batches
/// ([`crate::host_gc_site::ROOTS`]); the host then collects its own heap from
/// those references and its drawing registry
/// ([`crate::host_gc_site::COLLECT`]) and reports what stays live. The
/// budgets become the limits minus both live counts, and the thresholds
/// re-arm exactly as the interpreter's do. A failed host call stops the run
/// with the host's status.
///
/// With `trim` (a module whose heap is the bump region [`alloc_fn`] serves)
/// the bump pointer then drops to the end of the highest record still
/// reached, with its current buffer: nothing above it can be reached again,
/// so the next allocations reuse that memory. A strategy's heap shares the
/// arena band with its broker ([`arena_alloc_fn`]) and is not trimmed.
pub fn collect_fn(
    g: HeapGlobals,
    roots: &[CollectRoot],
    field_counts: &BTreeMap<u32, u32>,
    traced: &BTreeMap<u32, Vec<(u32, GcRef)>>,
    host: Option<CollectHost>,
    trim: bool,
) -> Function {
    let gc = g.gc.expect("a collector has its globals");
    let bar = 0u32;
    let n_e = 1u32;
    let n_o = 2u32;
    let wide = 3u32;
    let h = 4u32;
    let v = 5u32;
    let k = 6u32;
    let k_max = 7u32;
    let count = 8u32;
    let status = 9u32;
    let changed = 10u32;
    let top = 11u32;
    let end = 12u32;
    let heads: BTreeMap<u32, u32> = traced
        .keys()
        .enumerate()
        .map(|(i, &tid)| (tid, 13 + i as u32))
        .collect();
    let mut c = Collect {
        f: Function::new([(3, ValType::I64), (9 + heads.len() as u32, ValType::I32)]),
        gc,
        field_counts,
        heads: &heads,
        host,
        bar,
        n_e,
        n_o,
        count,
        status,
        top: trim.then_some(top),
        end,
    };
    // A new mark: every record is unreached until this collection reaches it.
    c.f.instruction(&I::GlobalGet(gc.epoch))
        .instruction(&I::I32Const(1))
        .instruction(&I::I32Add)
        .instruction(&I::GlobalSet(gc.epoch));

    // Roots: the cells the interpreter roots (`Vm::gc_roots`), from the
    // current bar back `min(depth, bar)` bars.
    for root in roots {
        let cell = |f: &mut Function, offset: Option<u32>| {
            f.instruction(&I::GlobalGet(root.lane))
                .instruction(&I::LocalGet(bar));
            if let Some(offset) = offset {
                f.instruction(&I::LocalGet(offset)).instruction(&I::I32Sub);
            }
            f.instruction(&I::I32Const(root.mask as i32))
                .instruction(&I::I32And)
                .instruction(&I::I32Const(3))
                .instruction(&I::I32Shl)
                .instruction(&I::I32Add)
                .instruction(&I::I32Load(m32(0)))
                .instruction(&I::LocalTee(v))
                .instruction(&I::I32Const(-1))
                .instruction(&I::I32Ne)
                .instruction(&I::If(BlockType::Empty));
        };
        if root.depth == 0 {
            cell(&mut c.f, None);
            c.visit(v, root.kind);
            c.f.instruction(&I::End);
            continue;
        }
        // k_max = min(depth, bar)
        c.f.instruction(&I::LocalGet(bar))
            .instruction(&I::I32Const(root.depth as i32))
            .instruction(&I::LocalGet(bar))
            .instruction(&I::I32Const(root.depth as i32))
            .instruction(&I::I32LtU)
            .instruction(&I::Select)
            .instruction(&I::LocalSet(k_max))
            .instruction(&I::I32Const(0))
            .instruction(&I::LocalSet(k))
            .instruction(&I::Block(BlockType::Empty))
            .instruction(&I::Loop(BlockType::Empty))
            .instruction(&I::LocalGet(k))
            .instruction(&I::LocalGet(k_max))
            .instruction(&I::I32GtU)
            .instruction(&I::BrIf(1));
        cell(&mut c.f, Some(k));
        c.visit(v, root.kind);
        c.f.instruction(&I::End)
            .instruction(&I::LocalGet(k))
            .instruction(&I::I32Const(1))
            .instruction(&I::I32Add)
            .instruction(&I::LocalSet(k))
            .instruction(&I::Br(0))
            .instruction(&I::End)
            .instruction(&I::End);
    }

    // Scan the reached objects until no work list has any left: scanning one
    // type's objects can reach objects of a type already emptied this pass.
    if !traced.is_empty() {
        c.f.instruction(&I::Block(BlockType::Empty))
            .instruction(&I::Loop(BlockType::Empty))
            .instruction(&I::I32Const(0))
            .instruction(&I::LocalSet(changed));
        for (tid, refs) in traced {
            let head = heads[tid];
            let link = object_fields_bytes(field_counts[tid] as usize) + 4;
            c.f.instruction(&I::Block(BlockType::Empty))
                .instruction(&I::Loop(BlockType::Empty))
                .instruction(&I::LocalGet(head))
                .instruction(&I::I32Eqz)
                .instruction(&I::BrIf(1))
                .instruction(&I::LocalGet(head))
                .instruction(&I::LocalTee(h))
                .instruction(&I::I32Load(m32(link)))
                .instruction(&I::LocalSet(head))
                .instruction(&I::I32Const(1))
                .instruction(&I::LocalSet(changed));
            for &(field, kind) in refs {
                c.f.instruction(&I::LocalGet(h))
                    .instruction(&I::I32Load(m32(8 * u64::from(field))))
                    .instruction(&I::LocalTee(v))
                    .instruction(&I::I32Const(-1))
                    .instruction(&I::I32Ne)
                    .instruction(&I::If(BlockType::Empty));
                c.visit(v, kind);
                c.f.instruction(&I::End);
            }
            c.f.instruction(&I::Br(0))
                .instruction(&I::End)
                .instruction(&I::End);
        }
        c.f.instruction(&I::LocalGet(changed))
            .instruction(&I::BrIf(0))
            .instruction(&I::End)
            .instruction(&I::End);
    }

    // Nothing above the highest record reached (and its buffer) can be
    // reached again: hand that memory back to the bump allocator. `top` is 0
    // when nothing was reached. It rounds up to 8 bytes as `alloc` rounds
    // every request, so it lands on the end of that record's allocation and
    // the next one stays 8-aligned (wasm allows an unaligned access, but it
    // can be slower).
    if trim {
        c.f.instruction(&I::LocalGet(top))
            .instruction(&I::I32Const(7))
            .instruction(&I::I32Add)
            .instruction(&I::I32Const(!7))
            .instruction(&I::I32And)
            .instruction(&I::LocalTee(top))
            .instruction(&I::GlobalGet(g.base))
            .instruction(&I::LocalGet(top))
            .instruction(&I::GlobalGet(g.base))
            .instruction(&I::I32GtU)
            .instruction(&I::Select)
            .instruction(&I::GlobalSet(g.ptr));
    }

    // The host collects its own heap and reports what stays live there.
    if let Some(host) = host {
        c.f.instruction(&I::LocalGet(count))
            .instruction(&I::If(BlockType::Empty));
        c.flush(host);
        c.f.instruction(&I::End);
        c.host_call(host, crate::host_gc_site::COLLECT);
        for (cell, total) in [(1u64, n_e), (2u64, n_o)] {
            c.f.instruction(&I::LocalGet(total))
                .instruction(&I::GlobalGet(host.scratch_base))
                .instruction(&I::I64Load(m64(8 * cell)))
                .instruction(&I::I64Add)
                .instruction(&I::LocalSet(total));
        }
    }

    let f = &mut c.f;
    // The budgets are the limits minus what is live.
    for (limit, total, budget) in [
        (crate::header::OFF_MAX_ELEMS, n_e, g.elems),
        (crate::header::OFF_MAX_OBJS, n_o, g.objs),
    ] {
        f.instruction(&I::I32Const(0))
            .instruction(&I::I64Load(m64(u64::from(limit))))
            .instruction(&I::LocalGet(total))
            .instruction(&I::I64Sub)
            .instruction(&I::GlobalSet(budget));
    }
    // arena_due = max(GC_MIN_ARENA, 2 * arena)
    f.instruction(&I::GlobalGet(gc.arena))
        .instruction(&I::I64Const(1))
        .instruction(&I::I64Shl)
        .instruction(&I::LocalTee(wide))
        .instruction(&I::I64Const(GC_MIN_ARENA))
        .instruction(&I::LocalGet(wide))
        .instruction(&I::I64Const(GC_MIN_ARENA))
        .instruction(&I::I64GtS)
        .instruction(&I::Select)
        .instruction(&I::GlobalSet(gc.arena_due));
    // threshold = max(live + max(limit - live, 0) / 2, 1)
    // (`re_arm_elem_threshold`); elems_due = limit - threshold
    f.instruction(&I::I32Const(0))
        .instruction(&I::I64Load(m64(u64::from(crate::header::OFF_MAX_ELEMS))))
        .instruction(&I::LocalGet(n_e))
        .instruction(&I::I64Sub)
        .instruction(&I::LocalTee(wide))
        .instruction(&I::I64Const(0))
        .instruction(&I::LocalGet(wide))
        .instruction(&I::I64Const(0))
        .instruction(&I::I64GtS)
        .instruction(&I::Select)
        .instruction(&I::I64Const(1))
        .instruction(&I::I64ShrU)
        .instruction(&I::LocalGet(n_e))
        .instruction(&I::I64Add)
        .instruction(&I::LocalTee(wide))
        .instruction(&I::I64Const(1))
        .instruction(&I::LocalGet(wide))
        .instruction(&I::I64Const(1))
        .instruction(&I::I64GtS)
        .instruction(&I::Select)
        .instruction(&I::LocalSet(wide));
    f.instruction(&I::I32Const(0))
        .instruction(&I::I64Load(m64(u64::from(crate::header::OFF_MAX_ELEMS))))
        .instruction(&I::LocalGet(wide))
        .instruction(&I::I64Sub)
        .instruction(&I::GlobalSet(gc.elems_due));
    f.instruction(&I::I32Const(0)).instruction(&I::End);
    c.f
}

/// [`collect_fn`]'s emission state.
struct Collect<'a> {
    f: Function,
    gc: GcGlobals,
    field_counts: &'a BTreeMap<u32, u32>,
    heads: &'a BTreeMap<u32, u32>,
    host: Option<CollectHost>,
    bar: u32,
    n_e: u32,
    n_o: u32,
    count: u32,
    status: u32,
    /// The local holding the highest end of a reached record, for a
    /// trimming collector.
    top: Option<u32>,
    /// A scratch local for one record's end.
    end: u32,
}

impl Collect<'_> {
    /// Reach the non-`na` reference in local `v`, of `kind`.
    fn visit(&mut self, v: u32, kind: GcRef) {
        let epoch = self.gc.epoch;
        match kind {
            GcRef::Array => {
                self.mark(v, ARR_MARK, epoch);
                self.count_live(|f| {
                    f.instruction(&I::LocalGet(v))
                        .instruction(&I::I32Load(m32(ARR_LEN)))
                        .instruction(&I::I64ExtendI32U);
                });
                // The record, and its current buffer: the inline one it was
                // allocated with, or the one its last growth allocated.
                self.raise_top(|f| {
                    f.instruction(&I::LocalGet(v))
                        .instruction(&I::I32Const(ARR_RECORD_GC))
                        .instruction(&I::I32Add);
                });
                self.raise_top(|f| {
                    f.instruction(&I::LocalGet(v))
                        .instruction(&I::I32Load(m32(ARR_BUF)))
                        .instruction(&I::LocalGet(v))
                        .instruction(&I::I32Load(m32(ARR_CAP)))
                        .instruction(&I::LocalGet(v))
                        .instruction(&I::I32Load(m32(ARR_LOG2)))
                        .instruction(&I::I32Shl)
                        .instruction(&I::I32Add);
                });
                self.f.instruction(&I::End);
            }
            GcRef::Object(tid) => {
                let fields = self.field_counts.get(&tid).copied().unwrap_or(0);
                let mark = object_fields_bytes(fields as usize);
                self.mark(v, mark, epoch);
                self.count_live(|f| {
                    f.instruction(&I::I64Const(i64::from(fields)));
                });
                // The fields and the trailer.
                self.raise_top(|f| {
                    f.instruction(&I::LocalGet(v))
                        .instruction(&I::I32Const(mark as i32 + 8))
                        .instruction(&I::I32Add);
                });
                if let Some(&head) = self.heads.get(&tid) {
                    // link = head; head = v
                    self.f
                        .instruction(&I::LocalGet(v))
                        .instruction(&I::LocalGet(head))
                        .instruction(&I::I32Store(m32(mark + 4)))
                        .instruction(&I::LocalGet(v))
                        .instruction(&I::LocalSet(head));
                }
                self.f.instruction(&I::End);
            }
            GcRef::Hosted => {
                let host = self
                    .host
                    .expect("a host reference has a host to collect it");
                // cells[2 + count] = v; one batch per full scratch region
                self.f
                    .instruction(&I::GlobalGet(host.scratch_base))
                    .instruction(&I::LocalGet(self.count))
                    .instruction(&I::I32Const(3))
                    .instruction(&I::I32Shl)
                    .instruction(&I::I32Add)
                    .instruction(&I::LocalGet(v))
                    .instruction(&I::I64ExtendI32U)
                    .instruction(&I::I64Store(m64(16)))
                    .instruction(&I::LocalGet(self.count))
                    .instruction(&I::I32Const(1))
                    .instruction(&I::I32Add)
                    .instruction(&I::LocalTee(self.count))
                    .instruction(&I::I32Const(host.cells as i32 - 2))
                    .instruction(&I::I32GeU)
                    .instruction(&I::If(BlockType::Empty));
                self.flush(host);
                self.f.instruction(&I::End);
            }
        }
    }

    /// Open `if <record v unmarked> { mark it; ...`; the caller closes it.
    fn mark(&mut self, v: u32, offset: u64, epoch: u32) {
        self.f
            .instruction(&I::LocalGet(v))
            .instruction(&I::I32Load(m32(offset)))
            .instruction(&I::GlobalGet(epoch))
            .instruction(&I::I32Ne)
            .instruction(&I::If(BlockType::Empty))
            .instruction(&I::LocalGet(v))
            .instruction(&I::GlobalGet(epoch))
            .instruction(&I::I32Store(m32(offset)));
    }

    /// For a trimming collector, `top = max(top, end)` with the end address
    /// `push_end` pushes.
    fn raise_top(&mut self, push_end: impl FnOnce(&mut Function)) {
        let Some(top) = self.top else {
            return;
        };
        push_end(&mut self.f);
        // [end] -> [end, top, end > top] -> select
        self.f
            .instruction(&I::LocalTee(self.end))
            .instruction(&I::LocalGet(top))
            .instruction(&I::LocalGet(self.end))
            .instruction(&I::LocalGet(top))
            .instruction(&I::I32GtU)
            .instruction(&I::Select)
            .instruction(&I::LocalSet(top));
    }

    /// One more live object holding the elements `push_elements` pushes.
    fn count_live(&mut self, push_elements: impl FnOnce(&mut Function)) {
        self.f.instruction(&I::LocalGet(self.n_e));
        push_elements(&mut self.f);
        self.f
            .instruction(&I::I64Add)
            .instruction(&I::LocalSet(self.n_e))
            .instruction(&I::LocalGet(self.n_o))
            .instruction(&I::I64Const(1))
            .instruction(&I::I64Add)
            .instruction(&I::LocalSet(self.n_o));
    }

    /// Send the batched host references (`cells[1]` = their count).
    fn flush(&mut self, host: CollectHost) {
        self.f
            .instruction(&I::GlobalGet(host.scratch_base))
            .instruction(&I::LocalGet(self.count))
            .instruction(&I::I64ExtendI32U)
            .instruction(&I::I64Store(m64(8)));
        self.host_call(host, crate::host_gc_site::ROOTS);
        self.f
            .instruction(&I::I32Const(0))
            .instruction(&I::LocalSet(self.count));
    }

    /// Call the host's collection `site`; a nonzero status stops the run.
    fn host_call(&mut self, host: CollectHost, site: u32) {
        self.f
            .instruction(&I::I32Const(site as i32))
            .instruction(&I::LocalGet(self.bar))
            .instruction(&I::Call(host.vm))
            .instruction(&I::LocalTee(self.status))
            .instruction(&I::If(BlockType::Empty))
            .instruction(&I::LocalGet(self.status))
            .instruction(&I::I32Const(0))
            .instruction(&I::LocalGet(self.bar))
            .instruction(&I::Call(host.stop))
            .instruction(&I::Return)
            .instruction(&I::End);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_encoder::{
        CodeSection, ConstExpr, ExportKind, ExportSection, FunctionSection, GlobalSection,
        GlobalType, MemorySection, MemoryType, Module, TypeSection,
    };

    /// Bump pointer at instantiation: an 8-aligned offset inside page 0.
    const BASE: i32 = 1024;

    /// The emitted `alloc` plus the `new` helpers, over a memory with the
    /// emitted modules' declared maximum and unlimited element/object
    /// budgets, so only the size arithmetic stands between a request and
    /// linear memory.
    fn instance() -> (wasmi::Store<()>, wasmi::Instance) {
        let g = HeapGlobals {
            base: 0,
            ptr: 1,
            elems: 2,
            objs: 3,
            gc: None,
        };
        let mut types = TypeSection::new();
        types.ty().function([ValType::I32], [ValType::I32]);
        types
            .ty()
            .function([ValType::I32, ValType::F64], [ValType::I32]);
        types
            .ty()
            .function([ValType::I32, ValType::I32], [ValType::I32]);
        let mut funcs = FunctionSection::new();
        let mut code = CodeSection::new();
        let mut exports = ExportSection::new();
        for (index, (name, ty, body)) in [
            ("alloc", 0, alloc_fn(g)),
            ("new_f64", 1, new_fn(g, 0, LANE_F64)),
            ("new_i32", 2, new_fn(g, 0, LANE_I32)),
        ]
        .into_iter()
        .enumerate()
        {
            funcs.function(ty);
            exports.export(name, ExportKind::Func, index as u32);
            code.function(&body);
        }
        let mut memories = MemorySection::new();
        memories.memory(MemoryType {
            minimum: 1,
            maximum: Some(crate::layout_footprint::MEMORY_MAX_PAGES),
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
        let mut globals = GlobalSection::new();
        for (ty, init) in [
            (ValType::I32, ConstExpr::i32_const(BASE)),
            (ValType::I32, ConstExpr::i32_const(BASE)),
            (ValType::I64, ConstExpr::i64_const(i64::MAX)),
            (ValType::I64, ConstExpr::i64_const(i64::MAX)),
        ] {
            globals.global(
                GlobalType {
                    val_type: ty,
                    mutable: true,
                    shared: false,
                },
                &init,
            );
        }
        exports.export("ptr", ExportKind::Global, g.ptr);
        exports.export("elems", ExportKind::Global, g.elems);
        exports.export("memory", ExportKind::Memory, 0);
        let mut m = Module::new();
        m.section(&types)
            .section(&funcs)
            .section(&memories)
            .section(&globals)
            .section(&exports)
            .section(&code);
        let engine = wasmi::Engine::default();
        let module = wasmi::Module::new(&engine, &m.finish()[..]).expect("valid heap helpers");
        let mut store = wasmi::Store::new(&engine, ());
        let instance = wasmi::Linker::new(&engine)
            .instantiate_and_start(&mut store, &module)
            .expect("zero-import heap helpers");
        (store, instance)
    }

    fn alloc(store: &mut wasmi::Store<()>, instance: wasmi::Instance, bytes: u32) -> u32 {
        instance
            .get_typed_func::<i32, i32>(&*store, "alloc")
            .unwrap()
            .call(store, bytes as i32)
            .expect("alloc never traps") as u32
    }

    fn ptr(store: &wasmi::Store<()>, instance: wasmi::Instance) -> u32 {
        instance
            .get_global(store, "ptr")
            .unwrap()
            .get(store)
            .i32()
            .unwrap() as u32
    }

    fn elems(store: &wasmi::Store<()>, instance: wasmi::Instance) -> i64 {
        instance
            .get_global(store, "elems")
            .unwrap()
            .get(store)
            .i64()
            .unwrap()
    }

    #[test]
    fn alloc_bumps_aligned_and_grows_memory() {
        let (mut store, instance) = instance();
        assert_eq!(alloc(&mut store, instance, 1), BASE as u32);
        assert_eq!(ptr(&store, instance), BASE as u32 + 8);
        let big = alloc(&mut store, instance, 3 << 16);
        assert_eq!(big, BASE as u32 + 8);
        assert_eq!(ptr(&store, instance), BASE as u32 + 8 + (3 << 16));
        let pages = instance.get_memory(&store, "memory").unwrap().size(&store);
        assert_eq!(pages, 4, "grown to cover the new end");
    }

    #[test]
    fn alloc_rejects_requests_whose_alignment_round_wraps() {
        let (mut store, instance) = instance();
        for bytes in 0xFFFF_FFF9u32..=u32::MAX {
            let first = alloc(&mut store, instance, bytes);
            let second = alloc(&mut store, instance, bytes);
            assert_eq!(first, 0, "alloc({bytes:#x}) must fail, not reserve 0 bytes");
            assert_eq!(second, 0, "alloc({bytes:#x}) must fail, not alias");
            assert_eq!(ptr(&store, instance), BASE as u32, "nothing reserved");
        }
        // The largest request whose round does not wrap fails through the
        // `next < ret` overflow check.
        assert_eq!(alloc(&mut store, instance, 0xFFFF_FFF8), 0);
        assert_eq!(ptr(&store, instance), BASE as u32);
    }

    #[test]
    fn alloc_rejects_ends_whose_page_count_wraps() {
        let (mut store, instance) = instance();
        // next = BASE + 0xFFFF_0000 does not overflow u32, but
        // next + 0xFFFF does: the page count would wrap to 0 and skip growth.
        let got = alloc(&mut store, instance, 0xFFFF_0000);
        assert_eq!(got, 0, "an end past every page must fail");
        assert_eq!(ptr(&store, instance), BASE as u32, "bump pointer unchanged");
        // The next allocation is still inside memory.
        assert_eq!(alloc(&mut store, instance, 8), BASE as u32);
    }

    #[test]
    fn new_rejects_capacities_whose_record_size_wraps() {
        for (export, size) in [
            // f64/i64 lane: 0x2000_0000 << 3 wraps to 0, so the record would
            // be a 16-byte allocation claiming 2^29 elements.
            ("new_f64", 0x2000_0000u32),
            // i32 lane: 16 + (0x3FFF_FFFB << 2) = 0xFFFF_FFFC, which wraps
            // in `alloc`'s alignment round.
            ("new_i32", 0x3FFF_FFFB),
            ("new_i32", 0x4000_0000),
            // The largest capacity that does not wrap: a 0xFFFF_FFF8-byte
            // request, refused by `alloc`'s overflow check instead.
            ("new_f64", 0x1FFF_FFFD),
        ] {
            let (mut store, instance) = instance();
            let result = if export == "new_f64" {
                instance
                    .get_typed_func::<(i32, f64), i32>(&store, export)
                    .unwrap()
                    .call(&mut store, (size as i32, 0.0))
            } else {
                instance
                    .get_typed_func::<(i32, i32), i32>(&store, export)
                    .unwrap()
                    .call(&mut store, (size as i32, 0))
            };
            assert_eq!(
                result.expect("a wrapping size fails, it does not trap"),
                0,
                "{export}({size:#x})"
            );
            assert_eq!(ptr(&store, instance), BASE as u32, "nothing reserved");
            assert_eq!(elems(&store, instance), i64::MAX, "nothing charged");
        }
    }
}
