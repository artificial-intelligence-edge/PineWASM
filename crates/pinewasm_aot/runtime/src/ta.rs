//! The stateful `ta.*` engine, as exports an emitted artifact can call.
//!
//! [`pinewasm_ta`] is the engine; this is the thin linear-memory binding
//! that lets a compiled module drive it. Together they are what retired the
//! `host.ta` import: a native artifact used to call out to
//! the VM for `ta.ema` and friends, which meant it could only run inside a
//! host that embedded the VM. Now the engine rides along inside the module.
//!
//! # The region
//!
//! `pinewasm_rt_ta_init` is handed one 8-aligned block that `prepare` reserved and
//! lays it out with [`pinewasm_ta::Layout`] — the *same* planner the
//! emitter used to size it, so the two cannot disagree:
//!
//! ```text
//! base ─┬─ decls   [Decl;  count]   validated copies of the table below the lane floor
//!       ├─ states  [State; count]   one per call site
//!       ├─ cells   [f64;   cells]   every ring buffer
//!       └─ scratch [f64;   scratch] shared sorting scratch (ta.median & co.)
//! ```
//!
//! Nothing is allocated: every ring capacity comes from a baked const param,
//! so the whole region is a compile-time size.
//!
//! # Market data
//!
//! The functions with no source operand (`ta.tr`, `ta.obv`, `ta.sar`, ...)
//! read the artifact's own builtin lanes, which the loader fills before `run`.
//! The six lanes are always allocated in the fixed
//! open/high/low/close/volume/time order at `builtin_base + lane * 8 * bars`,
//! whether or not the program reads them itself, so `ArtifactLanes` needs
//! only the base and the bar count.

use core::slice;
use pinewasm_ta::{Decl, Engine, Lanes, Layout, State};

/// Statuses `pinewasm_rt_ta_init` returns. Anything non-zero means the module and this
/// engine disagree about the declaration table, which a correctly emitted
/// artifact never does; `prepare` maps it to `INVALID_PROGRAM`.
pub mod status {
    pub const OK: i32 = 0;
    /// A declaration names no known function, or is wider than
    /// `pinewasm_ta::MAX_PARAMS`.
    pub const BAD_DECL: i32 = 1;
}

/// Where each region starts, and how long it is. Rebuilt per call from
/// [`CTX`] rather than stored as slices, because a `static mut` holding
/// references would be a self-referential borrow the compiler cannot check.
#[derive(Clone, Copy)]
struct Ctx {
    decls: u32,
    states: u32,
    cells: u32,
    scratch: u32,
    count: u32,
    cell_len: u32,
    scratch_len: u32,
    /// `builtin_base`: the artifact's lane block.
    lanes: u32,
    bars: u32,
    /// `scratch_base`: where a tuple call's extra results land, which is
    /// where the compiled body reads them from.
    extras: u32,
}

const EMPTY: Ctx = Ctx {
    decls: 0,
    states: 0,
    cells: 0,
    scratch: 0,
    count: 0,
    cell_len: 0,
    scratch_len: 0,
    lanes: 0,
    bars: 0,
    extras: 0,
};

/// The active run's region map. Single-threaded by construction: the module
/// declares no shared memory and the standalone WebAssembly contract forbids threads, so there is
/// exactly one caller.
static mut CTX: Ctx = EMPTY;

/// The artifact's builtin lanes as the engine's market-data reader.
///
/// Every read is bounds-checked against the declared bar count and returns na
/// outside it, which is also how a data hole reads — so an out-of-range bar
/// degrades to na instead of reading whatever else is in linear memory.
struct ArtifactLanes {
    base: u32,
    bars: u32,
}

impl ArtifactLanes {
    #[inline]
    fn lane(&self, index: u32, bar: u32) -> f64 {
        if bar >= self.bars {
            return f64::NAN;
        }
        let offset = self.base + (index * self.bars + bar) * 8;
        // SAFETY: `prepare` computed `base` as the start of six lanes of
        // `bars` f64s each and grew memory to cover the whole layout, so
        // every offset below `base + 6 * bars * 8` is inside the artifact's
        // own lane block. The bound above keeps `bar` inside a lane, and
        // `index` is a literal 0..=5 at every call site.
        unsafe { core::ptr::read_unaligned(offset as *const f64) }
    }
}

impl Lanes for ArtifactLanes {
    fn open(&self, bar: u32) -> f64 {
        self.lane(0, bar)
    }
    fn high(&self, bar: u32) -> f64 {
        self.lane(1, bar)
    }
    fn low(&self, bar: u32) -> f64 {
        self.lane(2, bar)
    }
    fn close(&self, bar: u32) -> f64 {
        self.lane(3, bar)
    }
    fn volume(&self, bar: u32) -> f64 {
        self.lane(4, bar)
    }
    fn time_ms(&self, bar: u32) -> i64 {
        // The time lane is f64, exactly as `ReadBuiltin(Time)` reads it:
        // epoch milliseconds are far inside the range f64 represents exactly.
        let t = self.lane(5, bar);
        if t.is_nan() {
            0
        } else {
            t as i64
        }
    }
}

/// Lay out the engine region and initialize every site.
///
/// `decl_table` points at the artifact's declaration table (a data segment of
/// `count` × [`Decl::SIZE`] little-endian entries); `base` at the 8-aligned
/// region `prepare` reserved. Calling this again is a reset — it is what a
/// second `run()` on a resident instance does, and what the standalone WebAssembly contract's
/// no-state-between-calls clause needs.
///
/// # Safety
///
/// `base` must be 8-aligned and name a writable, nonoverlapping region large
/// enough for [`Layout::plan`] over `count` declarations. `decl_table` must
/// name `count * Decl::SIZE` initialized bytes containing valid encoded
/// declarations. The builtin-lane region at `lanes` must contain six lanes of
/// `bars` f64 cells; `extras` must name the tuple scratch reserved by the same
/// layout. Calls must not be concurrent or reentrant, and none of these regions
/// may be mutated through another alias while the runtime is active.
#[no_mangle]
pub unsafe extern "C" fn pinewasm_rt_ta_init(
    base: u32,
    decl_table: u32,
    count: u32,
    lanes: u32,
    bars: u32,
    extras: u32,
) -> i32 {
    if count == 0 {
        // A `CallTa` site is what sets `links_ta`, so an emitted artifact
        // always has at least one declaration and never calls this. Guarding
        // anyway: `slice::from_raw_parts` requires a non-null pointer even
        // for an empty slice, and `base` would be whatever the caller passed.
        // SAFETY: the module has no shared memory and exported calls are
        // single-threaded, so this is the only access to the active context.
        unsafe { CTX = EMPTY };
        return status::OK;
    }
    let decls_ptr = base as *mut Decl;
    // Materialize validated declarations first: `Decl::from_le_bytes` is the
    // only way a tag becomes a `TaFunc`, so a corrupted table is refused here
    // rather than mis-decoded later.
    let mut i = 0u32;
    while i < count {
        let src = (decl_table + i * Decl::SIZE as u32) as *const u8;
        // SAFETY: the caller's contract places the complete declaration
        // table in initialized linear memory.
        let bytes = unsafe { slice::from_raw_parts(src, Decl::SIZE) };
        let Some(decl) = Decl::from_le_bytes(bytes) else {
            return status::BAD_DECL;
        };
        // SAFETY: `base` names a region sized by the same layout planner and
        // `i < count`, so this writes one initialized declaration in bounds.
        unsafe { core::ptr::write(decls_ptr.add(i as usize), decl) };
        i += 1;
    }
    // SAFETY: the loop above initialized exactly `count` declarations in the
    // caller-provided region.
    let decls = unsafe { slice::from_raw_parts(decls_ptr as *const Decl, count as usize) };
    let layout = Layout::plan(decls);

    let (states_off, cells_off, scratch_off) = crate::ta_offsets::region_addresses(base, layout);
    let states_ptr = states_off as *mut State;
    // `State` is `Copy` with no padding invariants, and `Invalid` is a valid
    // value for every slot; writing it first is what makes the slice below
    // reference initialized memory.
    let mut i = 0u32;
    while i < count {
        // SAFETY: the planned state region has `count` slots and `i < count`.
        unsafe { core::ptr::write(states_ptr.add(i as usize), State::Invalid) };
        i += 1;
    }

    // SAFETY: the module has no shared memory and exported calls are
    // single-threaded, so initialization has exclusive access to the active
    // context.
    unsafe {
        CTX = Ctx {
            decls: base,
            states: states_off,
            cells: cells_off,
            scratch: scratch_off,
            count,
            cell_len: layout.cells,
            scratch_len: layout.scratch,
            lanes,
            bars,
            extras,
        };
    }
    // SAFETY: the assignment above installed a context whose regions were
    // planned and initialized in this function.
    let ctx = unsafe { CTX };
    // SAFETY: `ctx` is the successfully initialized region map just stored.
    unsafe { engine(&ctx) }.reset();
    status::OK
}

/// Rebuild the engine over the region `ctx` describes.
///
/// # Safety
///
/// `ctx` must be the map a successful [`pinewasm_rt_ta_init`] stored.
unsafe fn engine(ctx: &Ctx) -> Engine<'static> {
    Engine::bind(
        // SAFETY: the function contract requires the initialized, nonoverlapping
        // region map produced by `pinewasm_rt_ta_init`.
        unsafe { slice::from_raw_parts(ctx.decls as *const Decl, ctx.count as usize) },
        // SAFETY: see above; the state region is initialized before binding.
        unsafe { slice::from_raw_parts_mut(ctx.states as *mut State, ctx.count as usize) },
        // SAFETY: see above; the cell region is disjoint from declarations,
        // state, and scratch.
        unsafe { slice::from_raw_parts_mut(ctx.cells as *mut f64, ctx.cell_len as usize) },
        // SAFETY: see above; the scratch region is disjoint and fully sized.
        unsafe { slice::from_raw_parts_mut(ctx.scratch as *mut f64, ctx.scratch_len as usize) },
    )
}

/// Advance ta site `state` on `bar` with up to three operands, returning the
/// first (or only) result. Tuple functions leave elements 1.. at the
/// artifact's scratch base, which is where the compiled body reads them.
///
/// The signature is the one `host.ta` had, so the emitter swapped an import
/// call for a direct call and nothing else about the call site changed.
///
/// # Safety
///
/// A successful [`pinewasm_rt_ta_init`] must have installed the active region
/// map. `state` and `bar` must be valid for that declaration table and lane
/// allocation. Calls must not overlap or reenter another TA runtime call, and
/// the initialized regions must remain valid and unaliased for the duration.
#[no_mangle]
pub unsafe extern "C" fn pinewasm_rt_ta_advance(
    state: u32,
    bar: u32,
    a: f64,
    b: f64,
    c: f64,
) -> f64 {
    // SAFETY: the module has no shared memory and the function contract
    // requires a successful initialization before this read.
    let ctx = unsafe { CTX };
    if ctx.count == 0 {
        // No successful `pinewasm_rt_ta_init`, so there is no region to read. Emitted
        // code always initializes at the top of `run`; this is the answer for
        // a caller that did not.
        return f64::NAN;
    }
    let lanes = ArtifactLanes {
        base: ctx.lanes,
        bars: ctx.bars,
    };
    // SAFETY: `ctx` came from the successful initialization required by this
    // function's contract.
    let Some(out) = unsafe { engine(&ctx) }.advance(state, [a, b, c], &lanes, bar) else {
        // Not advanceable: a declaration the CSIL validator would have
        // rejected. na is the same answer the interpreter's error path
        // produces once the host has surfaced it.
        return f64::NAN;
    };
    // SAFETY: `prepare` reserves `layout.scratch_cells * 8` bytes at
    // `extras`, never fewer than the 16 two tuple extras need.
    unsafe {
        core::ptr::write_unaligned(ctx.extras as *mut f64, out.extra[0]);
        core::ptr::write_unaligned((ctx.extras + 8) as *mut f64, out.extra[1]);
    }
    out.value
}
