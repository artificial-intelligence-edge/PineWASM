//! The standalone WebAssembly export family: the blob-side machinery behind the spec'd
//! `evaluate_indicator_cabi` surface (standalone WebAssembly ABI v1).
//!
//! The emitted glue in `pinewasm_aot::emit` sequences prepare → fill →
//! run → serialize; everything that would be miserable to hand-emit as
//! `wasm_encoder` instruction sequences — the JSON parser, the
//! shortest-roundtrip f64 printer, the arena allocator, the error slot —
//! lives here instead, for exactly the reason the numeric runtime exists
//! (`pinewasm_aot::runtime`, module doc). Blob code cannot call
//! artifact functions, so these helpers key off shared memory: the CSAW
//! header at address 0 carries every region base.
//!
//! # Discipline
//!
//! Everything here must stay **panic-free**: a reachable panic path drags
//! `core::fmt` vtables in, vtables need an indirect-call table, and the
//! splice rejects any runtime module with a table or element section
//! (`pinewasm_aot::runtime::Runtime::parse`). Hence no slice indexing
//! (only `get`), no `unwrap`, checked arithmetic where overflow is
//! representable, and raw-pointer memory access. `ryu` and `itoa` are the
//! two dependencies precisely because both are `no_std`, zero-dependency,
//! and documented never to panic.
//!
//! # Error classes
//!
//! Messages prefixed exactly `"Indicator failure: "` are the
//! expected-failure class (native exit-1); everything else — malformed
//! JSON, bad fraction/version, allocation failure — is infrastructure
//! (exit-2). Tokenizer failures are additionally prefixed
//! `"Invalid JSON: "` as required by the host contract.

use crate::arena_bounds::{allocation_bounds, relocation_delta};
use core::ptr;

// ---------------------------------------------------------------------------
// CSAW header mirror.
//
// Byte offsets of the fields this module reads, mirrored from
// `pinewasm_aot::header` (crates/pinewasm_aot/src/lib.rs). That module
// is the authority; these constants exist because the blob builds outside
// the workspace. `tests/standalone_wasm.rs` on the workspace side asserts the two agree.
pub(crate) const OFF_BARS: u32 = 0x08;
pub(crate) const OFF_BUILTIN_BASE: u32 = 0x10;
pub(crate) const OFF_PLOTS_BASE: u32 = 0x18;
/// Base of the scratch region; a `request.security` lane's status cell and
/// the chart timeframe cells are among its 8-byte cells.
const OFF_SCRATCH_BASE: u32 = 0x40;
/// `header::SIZE`: the input table starts here, 8 bytes per input.
const INPUT_TABLE_BASE: u32 = 0x60;

/// Builtin lane order (`pinewasm_aot::BUILTIN_LANES`):
/// open, high, low, close, volume, time. `input.source` lanes follow at 6+.
const LANE_VOLUME: u32 = 4;
pub(crate) const LANE_CLOSE: u32 = 3;
pub(crate) const LANE_TIME: u32 = 5;
const BUILTIN_LANE_COUNT: u32 = 6;

/// Mirrors `pinewasm_aot::runtime::STACK_SIZE`: the spliced shadow stack
/// grows down from `align16(__heap_base) + STACK_SIZE`, so the arena floor
/// is that same boundary — the first byte no other occupant can touch.
const STACK_SIZE: u32 = 16 * 1024;

/// Mirrors `pinewasm_aot::status`: the glue maps non-OK prepare/run
/// statuses through [`standalone_wasm_set_status_error`].
mod status {
    pub const BUDGET_EXCEEDED: i32 = 1;
    pub const ALLOC_FAILED: i32 = 2;
    pub const TOO_MANY_BARS: i32 = 3;
    pub const HISTORY_LIMIT: i32 = 4;
    pub const HEAP_REROUTE: i32 = 7;
    pub const RUN_BUDGET_EXCEEDED: i32 = 9;
    pub const INVALID_TA_LENGTH: i32 = 12;
    pub const SECURITY_TIMEFRAME_UNAVAILABLE: i32 = 13;
    pub const SECURITY_LENGTH_OUT_OF_RANGE: i32 = 14;
    pub const INT_FRACTION_RANGE: i32 = 15;
    pub const SECURITY_SESSION_UNSETTLED: i32 = 16;
    pub const SECURITY_TIMEZONE_UNSUPPORTED: i32 = 17;
}

// ---------------------------------------------------------------------------
// Mutable state. Lives in this module's .bss, inside the reserved block the
// splice already protects, so no artifact global is needed (blob code could
// not reference one anyway).

const ERR_CAP: usize = 256;

pub(crate) struct State {
    /// Arena band start. 0 = not yet initialized (the floor depends on
    /// `__heap_base`, which is a link-time symbol, not a Rust const).
    pub(crate) arena_lo: u32,
    /// Bump pointer: next free byte.
    pub(crate) arena_ptr: u32,
    /// One resident immutable primary-lane set. Full reset invalidates it.
    resident_id: u64,
    resident_bars: u32,
    resident_base: u32,
    resident_valid: bool,
    /// The scanner consumed a valid lease for the current evaluation.
    resident_reuse: bool,
    /// Per-evaluation symbol quantity increment.
    #[cfg(feature = "broker")]
    pub(crate) mincontract: f64,
    #[cfg(feature = "broker")]
    pub(crate) symbol_currency: u32,
    #[cfg(feature = "broker")]
    pub(crate) pointvalue: f64,
    #[cfg(feature = "broker")]
    pub(crate) include_results: bool,
    #[cfg(feature = "broker")]
    pub(crate) binary_results: bool,
    #[cfg(feature = "broker")]
    pub(crate) objective_results: bool,
    #[cfg(feature = "broker")]
    pub(crate) trade_results: bool,
    /// The request's quote currency as given: at most eight uppercase
    /// letters, `symbol_currency_len` of them live.
    #[cfg(feature = "broker")]
    pub(crate) symbol_currency_text: [u8; 8],
    #[cfg(feature = "broker")]
    pub(crate) symbol_currency_len: u8,
    /// Heads of the broker allocator's free lists, one per size class; each
    /// free block's first four bytes link to the next (0 ends a list).
    #[cfg(feature = "broker")]
    free_heads: [u32; FREE_CLASSES],
    /// Error slot: length of the live message in `err_buf`, 0 = none.
    err_len: u32,
    err_buf: [u8; ERR_CAP],
}

static mut STATE: State = State {
    arena_lo: 0,
    arena_ptr: 0,
    resident_id: 0,
    resident_bars: 0,
    resident_base: 0,
    resident_valid: false,
    resident_reuse: false,
    #[cfg(feature = "broker")]
    mincontract: 1.0,
    #[cfg(feature = "broker")]
    symbol_currency: 0,
    #[cfg(feature = "broker")]
    pointvalue: 1.0,
    #[cfg(feature = "broker")]
    include_results: false,
    #[cfg(feature = "broker")]
    binary_results: false,
    #[cfg(feature = "broker")]
    objective_results: false,
    #[cfg(feature = "broker")]
    trade_results: false,
    #[cfg(feature = "broker")]
    symbol_currency_text: [0; 8],
    #[cfg(feature = "broker")]
    symbol_currency_len: 0,
    #[cfg(feature = "broker")]
    free_heads: [0; FREE_CLASSES],
    err_len: 0,
    err_buf: [0; ERR_CAP],
};

/// Borrow the runtime's singleton state.
///
/// # Safety
///
/// The caller must ensure that no other reference derived from [`STATE`] is
/// live for the duration of the returned borrow. The borrow must remain local
/// to one synchronous runtime operation and must not cross a call that can
/// access [`STATE`]. The runtime blob's no-threads, no-callback execution model
/// lets its internal call sites uphold those requirements.
#[allow(clippy::mut_from_ref)]
pub(crate) unsafe fn state() -> &'static mut State {
    // SAFETY: the caller owns the uniqueness obligation documented above;
    // `addr_of_mut!` avoids first creating a reference to the mutable static.
    unsafe { &mut *ptr::addr_of_mut!(STATE) }
}

#[cfg(target_arch = "wasm32")]
extern "C" {
    /// Link-time symbol: first byte above this module's data + bss. The
    /// splice computes the same boundary from the exported global
    /// (`runtime.rs`); referencing the symbol here resolves to an
    /// `i32.const` at link time — no import.
    static __heap_base: u8;
}

/// First byte of the arena band: above rodata, bss, and the shadow stack.
#[cfg(target_arch = "wasm32")]
fn arena_floor() -> u32 {
    let heap_base = ptr::addr_of!(__heap_base) as u32;
    ((heap_base + 15) & !15) + STACK_SIZE
}

// Native builds exist only for the sanitizer harness. They never call the
// exported arena entry points: the sentinel keeps their code linkable while
// the tests exercise the target-independent scanner, serializer, and checked
// allocation arithmetic below.
#[cfg(not(target_arch = "wasm32"))]
fn arena_floor() -> u32 {
    STACK_SIZE
}

/// Initialize the arena, then borrow the runtime's singleton state.
///
/// # Safety
///
/// The caller must uphold [`state`]'s exclusivity and lifetime requirements.
/// In particular, the returned borrow must not span another state access or a
/// heap allocation that can enter [`arena_alloc_aligned`].
pub(crate) unsafe fn arena_init() -> &'static mut State {
    // SAFETY: `arena_init` forwards the caller's exclusive access to the same
    // singleton and does not create or retain another reference to it.
    let st = unsafe { state() };
    if st.arena_lo == 0 {
        st.arena_lo = arena_floor();
        st.arena_ptr = st.arena_lo;
    }
    st
}

/// Grow linear memory so `[0, end)` is addressable. False = grow refused
/// (the caller reports ALLOC-class failure; nothing traps).
fn growth_pages(current_pages: usize, end: u32) -> Option<usize> {
    const PAGE_SIZE: u64 = 1 << 16;
    let have = u64::try_from(current_pages).ok()?.checked_mul(PAGE_SIZE)?;
    let end = u64::from(end);
    if end <= have {
        return Some(0);
    }
    usize::try_from((end - have).div_ceil(PAGE_SIZE)).ok()
}

#[cfg(target_arch = "wasm32")]
fn ensure(end: u32) -> bool {
    let Some(pages) = growth_pages(core::arch::wasm32::memory_size::<0>(), end) else {
        return false;
    };
    if pages == 0 {
        return true;
    }
    core::arch::wasm32::memory_grow::<0>(pages) != usize::MAX
}

#[cfg(not(target_arch = "wasm32"))]
fn ensure(_end: u32) -> bool {
    false
}

/// Validate a byte range before constructing a Rust slice over linear memory.
///
/// Wasm loads trap outside linear memory, but `slice::from_raw_parts` also
/// requires a non-null pointer and a length no greater than `isize::MAX`.
/// Check those Rust invariants explicitly at the host-facing boundary.
fn checked_linear_range(start: usize, len: usize, memory_end: u64) -> Option<(u32, u32)> {
    if start == 0 || len > isize::MAX as usize {
        return None;
    }
    let end = start.checked_add(len)?;
    if (end as u64) > memory_end {
        return None;
    }
    Some((u32::try_from(start).ok()?, u32::try_from(end).ok()?))
}

pub(crate) fn linear_memory_range(ptr: *const u8, len: usize) -> Option<(u32, u32)> {
    #[cfg(target_arch = "wasm32")]
    let memory_end = (core::arch::wasm32::memory_size::<0>() as u64) << 16;
    #[cfg(not(target_arch = "wasm32"))]
    let memory_end = 0;
    checked_linear_range(ptr as usize, len, memory_end)
}

// ---------------------------------------------------------------------------
// Error slot.

/// Compose the error message from `parts`, truncating at [`ERR_CAP`].
pub(crate) fn set_err(parts: &[&[u8]]) {
    // SAFETY: error recording is synchronous and non-reentrant, and this
    // function retains no other reference to `STATE`.
    let st = unsafe { state() };
    st.resident_valid = false;
    st.resident_reuse = false;
    let mut n = 0usize;
    'outer: for part in parts {
        for &byte in *part {
            match st.err_buf.get_mut(n) {
                Some(slot) => {
                    *slot = byte;
                    n += 1;
                }
                None => break 'outer,
            }
        }
    }
    st.err_len = n as u32;
}

/// Tokenizer-failure class: prefixed exactly `"Invalid JSON: "`.
fn set_json_err(detail: &[u8]) {
    set_err(&[b"Invalid JSON: ", detail]);
}

fn set_alloc_err() {
    set_err(&[b"memory allocation failed: memory.grow refused"]);
}

// ---------------------------------------------------------------------------
// Raw linear-memory access. Addresses come from the CSAW header `prepare`
// wrote (all 8-aligned by layout, but `read_unaligned` costs nothing on
// wasm and keeps the soundness argument local).

/// Read one copyable value from a caller-validated, possibly unaligned byte
/// address. Kept target-independent so the native safety harness executes the
/// same raw-pointer primitive as the Wasm wrappers below.
///
/// # Safety
///
/// `source` must be valid and initialized for `size_of::<T>()` bytes, and its
/// bytes must represent a valid `T`.
unsafe fn read_unaligned<T: Copy>(source: *const u8) -> T {
    // SAFETY: the caller supplies the validity, initialization, and value
    // representation guarantees documented above; unaligned reads impose no
    // additional alignment requirement.
    unsafe { ptr::read_unaligned(source.cast::<T>()) }
}

/// Write one copyable value to a caller-validated, possibly unaligned byte
/// address.
///
/// # Safety
///
/// `destination` must be valid and writable for `size_of::<T>()` bytes.
unsafe fn write_unaligned<T: Copy>(destination: *mut u8, value: T) {
    // SAFETY: the caller supplies the writable-range guarantee documented
    // above; unaligned writes impose no additional alignment requirement.
    unsafe { ptr::write_unaligned(destination.cast::<T>(), value) }
}

/// Copy bytes between caller-validated disjoint regions.
///
/// # Safety
///
/// `source` and `destination` must each be valid for `len` bytes and must not
/// overlap.
unsafe fn copy_bytes(source: *const u8, destination: *mut u8, len: usize) {
    // SAFETY: the caller supplies the validity and disjointness guarantees
    // documented above.
    unsafe { ptr::copy_nonoverlapping(source, destination, len) }
}

/// Move bytes between caller-validated regions that may overlap.
///
/// # Safety
///
/// `source` and `destination` must each be valid for `len` bytes.
unsafe fn move_bytes(source: *const u8, destination: *mut u8, len: usize) {
    // SAFETY: the caller supplies the validity guarantee documented above;
    // `ptr::copy` deliberately permits overlap.
    unsafe { ptr::copy(source, destination, len) }
}

pub(crate) fn load_u32(addr: u32) -> u32 {
    // SAFETY: callers use fixed CSAW-header fields or compiler-emitted table
    // entries inside the module's grown linear-memory layout. The access is
    // read-only and the helper imposes no alignment precondition.
    unsafe { read_unaligned(addr as *const u8) }
}

pub(crate) fn load_f64(addr: u32) -> f64 {
    // SAFETY: callers derive `addr` from a lane or cell region that `prepare`
    // sized inside linear memory. The access is read-only and the helper is
    // unaligned-safe.
    unsafe { read_unaligned(addr as *const u8) }
}

fn load_i64(addr: u32) -> i64 {
    // SAFETY: callers pass an input-table cell address inside the header
    // block `prepare` sized. The access is read-only and the helper is
    // unaligned-safe.
    unsafe { read_unaligned(addr as *const u8) }
}

#[cfg(feature = "broker")]
fn store_u32(addr: u32, v: u32) {
    // SAFETY: callers pass the first bytes of a freed arena block of at
    // least eight bytes; the helper imposes no alignment precondition.
    unsafe { write_unaligned(addr as *mut u8, v) }
}

fn store_i64(addr: u32, v: i64) {
    // SAFETY: callers pass a scratch cell address inside the region
    // `prepare` reserved; the helper imposes no alignment precondition.
    unsafe { write_unaligned(addr as *mut u8, v) }
}

fn store_f64(addr: u32, v: f64) {
    // SAFETY: callers derive `addr` from writable lane/cell regions reserved
    // by `prepare`; the helper imposes no alignment precondition.
    unsafe { write_unaligned(addr as *mut u8, v) }
}

fn store_cell(addr: u32, cell: &[u8; 8]) {
    // SAFETY: `addr` points at a writable eight-byte input-table cell and
    // `cell` is a distinct Rust array, so both regions are valid and disjoint.
    unsafe { copy_bytes(cell.as_ptr(), addr as *mut u8, cell.len()) }
}

// ---------------------------------------------------------------------------
// The allocator family. Exported by the artifact under the public names
// (`alloc`/`dealloc`/`last_error`/`reset`); the `standalone_wasm_` prefix is the blob's
// internal namespace, renamed at the artifact's export section.

/// Bump-allocate on the topmost arena band. `standalone_wasm_alloc(0)` returns a
/// dangling well-aligned pointer; failure returns null.
#[no_mangle]
pub extern "C" fn standalone_wasm_alloc(len: usize) -> *mut u8 {
    // SAFETY: this exported call is synchronous in the no-threads runtime and
    // keeps the sole `STATE` borrow within this function.
    let st = unsafe { arena_init() };
    let Some((base, end)) = allocation_bounds(st.arena_ptr, len, 8) else {
        return ptr::null_mut();
    };
    if len == 0 {
        return base as *mut u8;
    }
    if !ensure(end) {
        return ptr::null_mut();
    }
    st.arena_ptr = end;
    base as *mut u8
}

/// The broker allocator's size classes: class `k` holds freed blocks of
/// `8 << k` bytes, up to 64 MiB.
#[cfg(any(feature = "broker", test))]
const FREE_CLASSES: usize = 24;

/// The free-list class and block size of a broker allocation, or `None` for
/// one that bypasses the free lists (aligned past 8, or past the largest
/// class), whose block is exactly `size` bytes.
#[cfg(any(feature = "broker", test))]
fn size_class(size: usize, align: usize) -> Option<(usize, usize)> {
    if align > 8 {
        return None;
    }
    let block = size.max(8).checked_next_power_of_two()?;
    let class = (block.trailing_zeros() - 3) as usize;
    (class < FREE_CLASSES).then_some((class, block))
}

/// Bytes the arena holds for a broker allocation of `size` at `align`.
#[cfg(any(feature = "broker", test))]
fn block_size(size: usize, align: usize) -> usize {
    size_class(size, align).map_or(size, |(_, block)| block)
}

/// Drop every free-listed block. Called wherever the arena band is rewound
/// or moved under the blocks, so a stale entry is never handed out.
fn forget_free_blocks(st: &mut State) {
    #[cfg(feature = "broker")]
    {
        st.free_heads = [0; FREE_CLASSES];
    }
    #[cfg(not(feature = "broker"))]
    let _ = st;
}

/// Rewind the whole arena band to its floor, dropping every block in it.
#[cfg(feature = "broker")]
pub(crate) fn arena_rewind(st: &mut State) {
    st.arena_ptr = st.arena_lo;
    forget_free_blocks(st);
}

/// The diagnostic an exhausted broker allocation leaves before it traps.
#[cfg(feature = "broker")]
pub(crate) const BROKER_MEMORY_EXHAUSTED: &[u8] =
    b"memory allocation failed: the strategy run exceeded the module's memory maximum";

/// Band headroom the broker keeps for one bar's short-lived allocations
/// (4 MiB), on top of the room it keeps for its trade record to double. A
/// bar that cannot have it is refused with [`BROKER_MEMORY_EXHAUSTED`] as
/// an ordinary error instead of trapping part-way through.
#[cfg(feature = "broker")]
pub(crate) const BROKER_BAR_HEADROOM: u32 = 4 << 20;

/// Record [`BROKER_MEMORY_EXHAUSTED`], then trap.
#[cfg(feature = "broker")]
fn broker_memory_exhausted() -> ! {
    set_err(&[BROKER_MEMORY_EXHAUSTED]);
    core::arch::wasm32::unreachable()
}

/// Allocate `size` bytes at `align` (≥ 8 effective) on the arena band, for
/// the `broker` build's global allocator. A block freed earlier in the run
/// is reused first, split when only a larger one is free: the broker frees
/// short-lived vectors every bar and outgrown ones as its per-bar lanes
/// double, and a plain bump allocator held all of them until the run ended.
///
/// Never returns null: an exhausted memory **traps** (`unreachable`) after
/// leaving [`BROKER_MEMORY_EXHAUSTED`] in the error slot, the
/// infrastructure-class abort the standalone WebAssembly host maps. The
/// broker avoids reaching it: it reserves its per-bar lanes for the whole
/// run when it starts and refuses a bar without [`BROKER_BAR_HEADROOM`]
/// plus room for its trade record to double, both with that error as an
/// ordinary failure, so a trap needs one bar's other allocations to exceed
/// the headroom. Returning null instead would route
/// through `handle_alloc_error` → `panic!` → `core::fmt`, whose vtables
/// need the element section the runtime-blob splice rejects — and because
/// this can be proven never-null, LTO eliminates that path entirely.
#[cfg(feature = "broker")]
#[inline(never)]
pub(crate) fn arena_alloc_aligned(size: usize, align: usize) -> *mut u8 {
    // SAFETY: allocation is synchronous and non-reentrant; callers do not
    // allocate while retaining a reference derived from `STATE`.
    let st = unsafe { arena_init() };
    let class = size_class(size, align);
    if let Some((class, _)) = class {
        // The smallest free block that fits: one of this class, or a larger
        // one split in halves, whose unused halves go back on the lower
        // lists. Splitting reuses the blocks a doubling vector leaves behind.
        let mut from = class;
        while from < FREE_CLASSES && st.free_heads[from] == 0 {
            from += 1;
        }
        if from < FREE_CLASSES {
            let head = st.free_heads[from];
            st.free_heads[from] = load_u32(head);
            while from > class {
                from -= 1;
                let half = head + (8u32 << from);
                store_u32(half, st.free_heads[from]);
                st.free_heads[from] = half;
            }
            return head as *mut u8;
        }
    }
    let block = class.map_or(size, |(_, block)| block);
    let Some((base, end)) = allocation_bounds(st.arena_ptr, block, align.max(8)) else {
        broker_memory_exhausted()
    };
    if block == 0 {
        return base as *mut u8;
    }
    if !ensure(end) {
        broker_memory_exhausted()
    }
    st.arena_ptr = end;
    base as *mut u8
}

/// Free a broker allocation: rewind the band when it is the top block,
/// otherwise put it on its class's free list (a block outside every class
/// stays unused until the next evaluation rewinds the band).
#[cfg(feature = "broker")]
#[inline(never)]
pub(crate) fn arena_free(ptr_: *mut u8, size: usize, align: usize) {
    // SAFETY: allocation is synchronous and non-reentrant; callers do not
    // allocate while retaining a reference derived from `STATE`.
    let st = unsafe { arena_init() };
    let lo = ptr_ as u32;
    if lo < st.arena_lo || lo >= st.arena_ptr {
        return;
    }
    let Ok(block) = u32::try_from(block_size(size, align)) else {
        return;
    };
    if lo.checked_add(block) == Some(st.arena_ptr) {
        st.arena_ptr = lo;
        return;
    }
    if let Some((class, _)) = size_class(size, align) {
        store_u32(lo, st.free_heads[class]);
        st.free_heads[class] = lo;
    }
}

/// Resize the broker allocation at `ptr` from `old` to `new` bytes in place:
/// always when its block already holds `new` bytes, and by growing the band
/// when it is the top block. False leaves everything as it was, and the
/// caller moves the allocation instead.
#[cfg(feature = "broker")]
#[inline(never)]
pub(crate) fn arena_resize(ptr_: *mut u8, old: usize, new: usize, align: usize) -> bool {
    // SAFETY: allocation is synchronous and non-reentrant; callers do not
    // allocate while retaining a reference derived from `STATE`.
    let st = unsafe { arena_init() };
    let (Ok(old), Ok(new)) = (
        u32::try_from(block_size(old, align)),
        u32::try_from(block_size(new, align)),
    ) else {
        return false;
    };
    let lo = ptr_ as u32;
    let top = lo >= st.arena_lo && lo.checked_add(old) == Some(st.arena_ptr);
    if !top {
        return new <= old;
    }
    let Some(end) = lo.checked_add(new) else {
        return false;
    };
    if !ensure(end) {
        return false;
    }
    st.arena_ptr = end;
    true
}

/// Whether the band can still grow `headroom` bytes past its top.
#[cfg(feature = "broker")]
pub(crate) fn arena_has_headroom(headroom: u32) -> bool {
    // SAFETY: synchronous and non-reentrant; the borrow ends in this call.
    let st = unsafe { arena_init() };
    st.arena_ptr.checked_add(headroom).is_some_and(ensure)
}

/// LIFO rewind iff `[ptr, ptr+len)` is the top allocation; otherwise a
/// no-op, which is what makes the host's dealloc(input) after
/// dealloc(output) sequence land back at the band start.
#[no_mangle]
pub extern "C" fn standalone_wasm_dealloc(ptr_: *mut u8, len: usize) {
    // SAFETY: this exported call is synchronous in the no-threads runtime and
    // keeps the sole `STATE` borrow within this function.
    let st = unsafe { arena_init() };
    if len == 0 {
        return;
    }
    let Ok(len) = u32::try_from(len) else {
        return;
    };
    let lo = ptr_ as u32;
    if let Some(end) = lo.checked_add(len) {
        if end == st.arena_ptr && lo >= st.arena_lo {
            st.arena_ptr = lo;
        }
    }
}

/// Packed `(ptr << 32) | len` of the UTF-8 diagnostic, 0 if none. The
/// buffer is module-owned: it is static and never allocated.
#[no_mangle]
pub extern "C" fn standalone_wasm_last_error() -> u64 {
    // SAFETY: this exported call is synchronous in the no-threads runtime and
    // keeps the sole `STATE` borrow within this function.
    let st = unsafe { state() };
    if st.err_len == 0 {
        return 0;
    }
    ((st.err_buf.as_ptr() as u64) << 32) | st.err_len as u64
}

/// Full reset: rewind the arena to its floor and clear the error
/// slot. Post-reset `standalone_wasm_last_error()` is 0.
#[no_mangle]
pub extern "C" fn standalone_wasm_reset() {
    // SAFETY: this exported call is synchronous in the no-threads runtime and
    // keeps the sole `STATE` borrow within this function.
    let st = unsafe { state() };
    st.arena_lo = arena_floor();
    st.arena_ptr = st.arena_lo;
    forget_free_blocks(st);
    st.err_len = 0;
    st.resident_valid = false;
    st.resident_reuse = false;
}

/// Move the arena band above `min_base` (glue: `heap_end + pad`, read from
/// the header after `prepare`). The band holds only the live input JSON at
/// this point, so one memmove relocates everything; the return value is the
/// non-negative byte delta the glue adds to its JSON pointer (0 = no move),
/// or −1 when growth was refused (error slot set, infrastructure class).
#[no_mangle]
pub extern "C" fn standalone_wasm_arena_reloc(min_base: u32) -> i32 {
    // SAFETY: this exported call is synchronous in the no-threads runtime. On
    // either error branch below, `st` is dead before `set_alloc_err` takes a
    // fresh `STATE` borrow; the success path keeps this sole borrow local.
    let st = unsafe { arena_init() };
    let Some((target, _)) = allocation_bounds(min_base.max(arena_floor()), 0, 8) else {
        set_alloc_err();
        return -1;
    };
    if st.arena_lo >= target {
        return 0;
    }
    let Some(delta) = relocation_delta(st.arena_lo, target) else {
        set_alloc_err();
        return -1;
    };
    let live = st.arena_ptr - st.arena_lo;
    let Some(end) = target.checked_add(live) else {
        set_alloc_err();
        return -1;
    };
    if !ensure(end) {
        set_alloc_err();
        return -1;
    }
    // Overlap is possible (target may sit inside the old band's tail), so
    // this must be a memmove, not a memcpy.
    // SAFETY: `[arena_lo, arena_lo + live)` is the initialized live arena;
    // `ensure(end)` made `[target, target + live)` writable. `ptr::copy`
    // deliberately permits the possible overlap described above.
    unsafe { move_bytes(st.arena_lo as *const u8, target as *mut u8, live as usize) };
    st.arena_lo = target;
    st.arena_ptr = target + live;
    forget_free_blocks(st);
    delta
}

/// Map a non-OK `prepare`/`run` status to the error slot:
/// module-internal indicator conditions get the exact
/// `"Indicator failure: "` prefix; resource/layout refusals stay
/// infrastructure-class (unprefixed).
#[no_mangle]
pub extern "C" fn standalone_wasm_set_status_error(code: i32) {
    match code {
        status::BUDGET_EXCEEDED => {
            set_err(&[b"Indicator failure: per-bar instruction budget exceeded"])
        }
        status::RUN_BUDGET_EXCEEDED => {
            set_err(&[b"Indicator failure: cumulative instruction budget exceeded"])
        }
        status::HISTORY_LIMIT => {
            set_err(&[b"Indicator failure: history buffer limit exceeded (max_bars_back)"])
        }
        status::INVALID_TA_LENGTH => set_err(&[b"Indicator failure: TA input length out of range"]),
        status::SECURITY_TIMEFRAME_UNAVAILABLE => set_err(&[
            b"Indicator failure: this request.security timeframe is not available in standalone WebAssembly",
        ]),
        status::SECURITY_LENGTH_OUT_OF_RANGE => {
            set_err(&[b"Indicator failure: request.security length is out of range"])
        }
        status::INT_FRACTION_RANGE => set_err(&[
            b"Indicator failure: a whole number too large to hold exactly met the result of dividing two whole numbers",
        ]),
        status::SECURITY_SESSION_UNSETTLED => set_err(&[
            b"Indicator failure: where this market's session starts each requested bucket is not settled in PineWASM yet",
        ]),
        status::SECURITY_TIMEZONE_UNSUPPORTED => set_err(&[
            b"Indicator failure: standalone WebAssembly groups bars only on a fixed-offset exchange clock, and this request's timezone is not one",
        ]),
        status::HEAP_REROUTE => set_err(&[b"Indicator failure: native heap error"]),
        status::ALLOC_FAILED => set_alloc_err(),
        status::TOO_MANY_BARS => set_err(&[b"too many bars for layout arithmetic"]),
        other => {
            let mut buf = itoa::Buffer::new();
            set_err(&[b"unexpected status ", buf.format(other).as_bytes()]);
        }
    }
}

/// Refuse a request that would execute `bars` bars when the artifact admits
/// at most `limit`: the per-artifact bound under which its erased drawing
/// constructors provably stay inside the interpreter's heap caps (the
/// compiler bakes `limit` into the entry point and publishes it as
/// `max_bars` in the `pinewasm_capabilities` section). A request error, not
/// an indicator failure: the same request on fewer bars is accepted.
#[no_mangle]
pub extern "C" fn standalone_wasm_set_bar_bound_error(bars: u32, limit: u32) {
    let mut have = itoa::Buffer::new();
    let mut max = itoa::Buffer::new();
    set_err(&[
        b"request executes ",
        have.format(bars).as_bytes(),
        b" bars, but this artifact's drawing allocation bound admits at most ",
        max.format(limit).as_bytes(),
        b" (pinewasm_capabilities max_bars)",
    ]);
}

// ---------------------------------------------------------------------------
// JSON scanner. One implementation, two passes: `standalone_wasm_scan` validates the
// whole document and extracts version/fraction/bar-count; `standalone_wasm_fill_lanes`
// re-walks the (now known-valid) text and writes lanes and the input table.
// Errors set the slot at the failure site and unwind as `Err(())`.

/// Nesting bound: the wire format needs 3 levels; the shadow stack under the
/// standalone blob build is one page, so recursion is bounded well below it.
const MAX_DEPTH: u32 = 32;

struct Scan<'a> {
    b: &'a [u8],
    pos: usize,
}

/// Uniform tokenizer failure: sets the slot, returns the `Err` the caller
/// propagates.
fn bad<T>(detail: &[u8]) -> Result<T, ()> {
    set_json_err(detail);
    Err(())
}

impl<'a> Scan<'a> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, want: u8) -> bool {
        if self.peek() == Some(want) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, want: u8, detail: &'static [u8]) -> Result<(), ()> {
        if self.eat(want) {
            Ok(())
        } else {
            bad(detail)
        }
    }

    /// `true`/`false`/`null`, first byte already peeked.
    fn literal(&mut self, word: &'static [u8]) -> Result<(), ()> {
        match self.b.get(self.pos..self.pos + word.len()) {
            Some(got) if got == word => {
                self.pos += word.len();
                Ok(())
            }
            _ => bad(b"bad literal"),
        }
    }

    /// A string token; returns the content span (quotes excluded, escapes
    /// left raw — keys are matched as raw bytes, so an escaped key simply
    /// fails to match and is ignored like any unknown key).
    fn string(&mut self) -> Result<(usize, usize), ()> {
        self.expect(b'"', b"expected string")?;
        let start = self.pos;
        loop {
            match self.peek() {
                None => return bad(b"unterminated string"),
                Some(b'"') => {
                    let end = self.pos;
                    self.pos += 1;
                    return Ok((start, end));
                }
                Some(b'\\') => {
                    self.pos += 1;
                    match self.peek() {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {
                            self.pos += 1
                        }
                        Some(b'u') => {
                            self.pos += 1;
                            for _ in 0..4 {
                                if !matches!(self.peek(), Some(c) if c.is_ascii_hexdigit()) {
                                    return bad(b"bad unicode escape");
                                }
                                self.pos += 1;
                            }
                        }
                        _ => return bad(b"bad escape"),
                    }
                }
                Some(c) if c < 0x20 => return bad(b"control character in string"),
                Some(_) => self.pos += 1,
            }
        }
    }

    /// A number token per the JSON grammar; returns its span.
    fn number(&mut self) -> Result<(usize, usize), ()> {
        let start = self.pos;
        self.eat(b'-');
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                    self.pos += 1;
                }
            }
            _ => return bad(b"bad number"),
        }
        if self.eat(b'.') {
            if !matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                return bad(b"bad number");
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                return bad(b"bad number");
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        Ok((start, self.pos))
    }

    /// Validate and skip any value. Leading whitespace already skipped.
    fn value(&mut self, depth: u32) -> Result<(), ()> {
        if depth > MAX_DEPTH {
            return bad(b"nesting too deep");
        }
        match self.peek() {
            Some(b'{') => {
                self.pos += 1;
                self.skip_ws();
                if self.eat(b'}') {
                    return Ok(());
                }
                loop {
                    self.string()?;
                    self.skip_ws();
                    self.expect(b':', b"expected ':'")?;
                    self.skip_ws();
                    self.value(depth + 1)?;
                    self.skip_ws();
                    if self.eat(b',') {
                        self.skip_ws();
                        continue;
                    }
                    return self.expect(b'}', b"expected ',' or '}'");
                }
            }
            Some(b'[') => {
                self.pos += 1;
                self.skip_ws();
                if self.eat(b']') {
                    return Ok(());
                }
                loop {
                    self.value(depth + 1)?;
                    self.skip_ws();
                    if self.eat(b',') {
                        self.skip_ws();
                        continue;
                    }
                    return self.expect(b']', b"expected ',' or ']'");
                }
            }
            Some(b'"') => self.string().map(drop),
            Some(b't') => self.literal(b"true"),
            Some(b'f') => self.literal(b"false"),
            Some(b'n') => self.literal(b"null"),
            Some(b'-' | b'0'..=b'9') => self.number().map(drop),
            Some(_) => bad(b"unexpected byte"),
            None => bad(b"truncated input"),
        }
    }

    fn span(&self, (start, end): (usize, usize)) -> &'a [u8] {
        self.b.get(start..end).unwrap_or(&[])
    }

    /// Exact f64 of a number token (core's correctly-rounded `dec2flt`;
    /// the token already matched the JSON grammar, a strict subset of
    /// Rust's float grammar, so parse failure is unreachable in practice).
    fn f64_of(&self, span: (usize, usize)) -> Result<f64, ()> {
        let bytes = self.span(span);
        // SAFETY: the tokenizer admits only ASCII bytes into number tokens.
        let text = unsafe { core::str::from_utf8_unchecked(bytes) };
        match text.parse::<f64>() {
            Ok(v) => Ok(v),
            Err(_) => bad(b"bad number"),
        }
    }

    /// Exact i64 of a number token. Integer-form tokens accumulate
    /// digit-by-digit (checked, so i64::MIN..=MAX round-trips exactly);
    /// float-form tokens (`14.0`, `1e3`) are accepted iff integral.
    fn i64_of(&self, span: (usize, usize)) -> Result<i64, ()> {
        let bytes = self.span(span);
        let (neg, digits) = match bytes.split_first() {
            Some((b'-', rest)) => (true, rest),
            _ => (false, bytes),
        };
        if !digits.is_empty() && digits.iter().all(u8::is_ascii_digit) {
            let mut acc: i64 = 0;
            for &d in digits {
                acc = acc
                    .checked_mul(10)
                    .and_then(|a| a.checked_sub((d - b'0') as i64))
                    .ok_or(())
                    .or_else(|_| bad(b"integer out of range"))?;
            }
            // Accumulated negative to reach i64::MIN without overflow.
            return if neg {
                Ok(acc)
            } else {
                acc.checked_neg()
                    .ok_or(())
                    .or_else(|_| bad(b"integer out of range"))
            };
        }
        let v = self.f64_of(span)?;
        // Exact-integral gate: representable, in range, no fraction.
        if v.is_finite() && (-9.007_199_254_740_992e15..=9.007_199_254_740_992e15).contains(&v) {
            let i = v as i64;
            if i as f64 == v {
                return Ok(i);
            }
        }
        set_err(&[b"number is not an integer"]);
        Err(())
    }
}

// ---------------------------------------------------------------------------
// Pass one: validate + count.

/// The four recognized top-level IN keys, as a presence bitmask. `version`,
/// `params`, and `bars` are required; `fraction` is optional and tracked only
/// so a repeat is caught. Required-field omissions remain input errors.
mod in_key {
    pub const VERSION: u32 = 1 << 0;
    pub const FRACTION: u32 = 1 << 1;
    pub const BARS: u32 = 1 << 2;
    pub const PARAMS: u32 = 1 << 3;
}

/// Claim a recognized top-level key, refusing a repeat (infrastructure
/// class, key named).
///
/// Reject rather than last-wins for two reasons. The module parses strictly
/// everywhere else, and neither this ABI nor RFC 8259 pins which occurrence
/// of a duplicate name wins — so "last wins" would be a rule [`scan_pass`]
/// and [`fill_pass`] would each have to implement, independently, forever.
/// They cannot disagree about a document that never reaches `fill_pass`: a
/// second `"bars"` used to add its elements to this pass's count while
/// `fill_pass` restarted its bar cursor at 0, so `slice_end` ran past the
/// bars actually written and stale lane bytes serialized as phantom signals.
///
/// Duplicated UNKNOWN keys stay tolerated: an unknown key is accepted and
/// ignored, so a second occurrence of one changes nothing in
/// either pass.
fn claim(seen: &mut u32, flag: u32, name: &'static [u8]) -> Result<(), ()> {
    if *seen & flag != 0 {
        set_err(&[b"duplicate top-level key: ", name]);
        return Err(());
    }
    *seen |= flag;
    Ok(())
}

/// Validate the IN document at `[ptr, ptr+len)`, count bars, and apply the
/// version/fraction rules. Returns `slice_end = max(1, floor(n × fraction))`
/// or −1 with the error slot set.
///
/// Also the per-evaluate prologue: clears the previous call's
/// error slot and drops any arena allocation above the live input buffer —
/// the previous output, if the host leaked it — so a resident instance
/// cannot grow monotonically.
///
/// # Safety
///
/// `ptr_` must point to `len` initialized bytes in this module's linear
/// memory, obtained from [`standalone_wasm_alloc`], for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn standalone_wasm_scan(ptr_: *const u8, len: usize) -> i64 {
    // SAFETY: this exported call is synchronous in the no-threads runtime. On
    // either invalid-range branch, `st` is dead before `set_err` takes a fresh
    // `STATE` borrow; the accepted range keeps this sole borrow local.
    let st = unsafe { arena_init() };
    st.err_len = 0;
    st.resident_reuse = st.resident_valid;
    st.resident_valid = false;
    let Some((lo, end)) = linear_memory_range(ptr_, len) else {
        set_err(&[b"input buffer is outside linear memory"]);
        return -1;
    };
    if lo < st.arena_lo || end > st.arena_ptr {
        set_err(&[b"input buffer is outside the active arena"]);
        return -1;
    }
    st.arena_ptr = end;

    // SAFETY: `linear_memory_range` checked the slice invariants and the arena
    // bounds above prove the host-owned range is an active allocation.
    let text = unsafe { core::slice::from_raw_parts(ptr_, len) };
    match scan_input(text) {
        Ok(slice_end) => slice_end as i64,
        Err(()) => -1,
    }
}

/// Versioned bulk input: magic[8], bar count u32, metadata length u32,
/// metadata JSON padded to 8 bytes, then six little-endian f64 lanes.
/// Metadata retains the JSON parameter/economics contract, without bars or
/// fraction. Shape and allocation ranges are checked before any lane access.
const BINARY_MAGIC: &[u8; 8] = b"PWSWBIN1";
/// Optional resident-data protocol through the existing ABI-v1 entry point.
/// BIN2 uploads lanes plus an opaque nonzero u64 dataset id; REF1 contains
/// only metadata and refers to the most recent successful upload on this
/// instance. Both add the id at byte 16, before metadata at byte 24.
const RESIDENT_UPLOAD_MAGIC: &[u8; 8] = b"PWSWBIN2";
const RESIDENT_REFERENCE_MAGIC: &[u8; 8] = b"PWSWREF1";

fn is_binary_input(bytes: &[u8]) -> bool {
    bytes.starts_with(BINARY_MAGIC)
        || bytes.starts_with(RESIDENT_UPLOAD_MAGIC)
        || bytes.starts_with(RESIDENT_REFERENCE_MAGIC)
}

struct BinaryInput<'a> {
    bars: u32,
    metadata: &'a [u8],
    lanes: &'a [u8],
    dataset_id: Option<u64>,
    reference: bool,
}

fn binary_input(bytes: &[u8]) -> Result<BinaryInput<'_>, ()> {
    let invalid = || set_err(&[b"invalid binary input layout"]);
    let read = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(
            bytes.get(at..at.checked_add(4)?)?.try_into().ok()?,
        ))
    };
    let bars = read(8).ok_or_else(invalid)?;
    let metadata_len = read(12).ok_or_else(invalid)? as usize;
    let reference = bytes.starts_with(RESIDENT_REFERENCE_MAGIC);
    let resident = reference || bytes.starts_with(RESIDENT_UPLOAD_MAGIC);
    let metadata_start = if resident { 24usize } else { 16usize };
    let dataset_id = if resident {
        let value = u64::from_le_bytes(
            bytes
                .get(16..24)
                .ok_or_else(invalid)?
                .try_into()
                .map_err(|_| invalid())?,
        );
        if value == 0 {
            return bad(b"resident dataset id must be nonzero");
        }
        Some(value)
    } else {
        None
    };
    let metadata_end = metadata_start
        .checked_add(metadata_len)
        .ok_or_else(invalid)?;
    let lanes_start = metadata_end.checked_add(7).ok_or_else(invalid)? & !7;
    let lanes_len = if reference {
        0
    } else {
        (bars as usize).checked_mul(48).ok_or_else(invalid)?
    };
    let end = lanes_start.checked_add(lanes_len).ok_or_else(invalid)?;
    if !is_binary_input(bytes) || bars == 0 || end != bytes.len() {
        invalid();
        return Err(());
    }
    Ok(BinaryInput {
        bars,
        metadata: bytes
            .get(metadata_start..metadata_end)
            .ok_or_else(invalid)?,
        lanes: bytes.get(lanes_start..end).ok_or_else(invalid)?,
        dataset_id,
        reference,
    })
}

fn scan_input(text: &[u8]) -> Result<u32, ()> {
    if is_binary_input(text) {
        let input = binary_input(text)?;
        if input.reference {
            // SAFETY: short synchronous read, ending before parser/error calls.
            let st = unsafe { state() };
            if !st.resident_reuse
                || Some(st.resident_id) != input.dataset_id
                || st.resident_bars != input.bars
            {
                return bad(b"resident dataset reference is stale or missing");
            }
        }
        scan_pass(input.metadata, Some(input.bars))
    } else {
        scan_pass(text, None)
    }
}

fn scan_pass(text: &[u8], binary_bars: Option<u32>) -> Result<u32, ()> {
    let s = &mut Scan { b: text, pos: 0 };
    let mut n_bars: u32 = binary_bars.unwrap_or(0);
    let mut seen: u32 = if binary_bars.is_some() {
        in_key::BARS
    } else {
        0
    };
    let mut fraction: f64 = 1.0;

    s.skip_ws();
    s.expect(b'{', b"expected object")?;
    s.skip_ws();
    if !s.eat(b'}') {
        loop {
            let key = s.string()?;
            s.skip_ws();
            s.expect(b':', b"expected ':'")?;
            s.skip_ws();
            match s.span(key) {
                b"bars" | b"fraction" if binary_bars.is_some() => {
                    return bad(b"binary metadata must omit bars and fraction");
                }
                b"version" => {
                    claim(&mut seen, in_key::VERSION, b"version")?;
                    let span = s.number()?;
                    let v = s.f64_of(span)?;
                    if v != 1.0 {
                        set_err(&[b"unsupported input version (expected 1)"]);
                        return Err(());
                    }
                }
                b"fraction" => {
                    claim(&mut seen, in_key::FRACTION, b"fraction")?;
                    let span = s.number()?;
                    fraction = s.f64_of(span)?;
                    if !fraction.is_finite() || fraction <= 0.0 || fraction > 1.0 {
                        set_err(&[b"fraction out of range (expected 0 < fraction <= 1)"]);
                        return Err(());
                    }
                }
                b"bars" => {
                    claim(&mut seen, in_key::BARS, b"bars")?;
                    s.expect(b'[', b"bars must be an array")?;
                    s.skip_ws();
                    if !s.eat(b']') {
                        loop {
                            if s.peek() != Some(b'{') {
                                // A well-formed non-object element is a shape
                                // error; anything else is a tokenizer error
                                // and keeps the `Invalid JSON: ` class.
                                s.value(1)?;
                                set_err(&[b"bars elements must be objects"]);
                                return Err(());
                            }
                            s.value(1)?;
                            n_bars = n_bars.saturating_add(1);
                            s.skip_ws();
                            if s.eat(b',') {
                                s.skip_ws();
                                continue;
                            }
                            s.expect(b']', b"expected ',' or ']'")?;
                            break;
                        }
                    }
                }
                b"params" => {
                    claim(&mut seen, in_key::PARAMS, b"params")?;
                    if s.peek() != Some(b'{') {
                        set_err(&[b"params must be an object"]);
                        return Err(());
                    }
                    s.value(0)?;
                }
                // Accept-and-ignore every unknown or inapplicable optional
                // field, including `override_kernel: null`
                // and `emit_debug`.
                _ => s.value(0)?,
            }
            s.skip_ws();
            if s.eat(b',') {
                s.skip_ws();
                continue;
            }
            s.expect(b'}', b"expected ',' or '}'")?;
            break;
        }
    }
    s.skip_ws();
    if s.pos != text.len() {
        return bad(b"trailing data");
    }

    // The three REQUIRED IN fields, in schema order. `fraction`,
    // `override_kernel` and `emit_debug` are optional; everything named as
    // required is checked here, and its omission is
    // an input error rather than a default.
    if seen & in_key::VERSION == 0 {
        set_err(&[b"missing required field: version"]);
        return Err(());
    }
    if seen & in_key::PARAMS == 0 {
        set_err(&[b"missing required field: params"]);
        return Err(());
    }
    if seen & in_key::BARS == 0 {
        set_err(&[b"missing required field: bars"]);
        return Err(());
    }
    if n_bars == 0 {
        set_err(&[b"bars array is empty"]);
        return Err(());
    }
    // Prefix rule: the cast truncates toward zero, which for a
    // non-negative product is exactly `floor`.
    let slice_end = ((n_bars as f64) * fraction) as u32;
    Ok(slice_end.max(1))
}

// ---------------------------------------------------------------------------
// Pass two: the key table and lane filling.

/// Cell-encoding kind codes carried by the key table (see the byte layout on
/// [`standalone_wasm_fill_lanes`]). 0–2 mirror the input-table encodings the artifact's
/// reads expect (`body.rs` `emit_read_input`): float/price cells are f64,
/// int/time/enum cells are exact little-endian i64, bool cells are f64 0/1.
/// 3 is not an input-table entry at all: it directs a builtin-lane copy into
/// an `input.source` lane.
mod kind {
    pub const F64: u8 = 0;
    pub const I64: u8 = 1;
    pub const BOOL: u8 = 2;
    pub const SRC: u8 = 3;
    /// A same-symbol `request.security` lane (see [`fill_security_lane`]).
    pub const SEC: u8 = 4;

    /// Kinds below this carry a scalar input-table cell and a wire key.
    pub const FIRST_LANE_KIND: u8 = SRC;
}

struct KeyEntry<'a> {
    kind: u8,
    /// `kind` 0–2: input index (table cell at `INPUT_TABLE_BASE + index*8`).
    /// `kind` 3–4: lane ordinal `j` (the lane written is `6 + j`).
    index: u32,
    /// `kind` 0–2: the declaration's default, already cell-encoded.
    /// `kind` 3: bytes 0–3 are the LE u32 builtin lane to copy (0–5).
    /// `kind` 4: byte 0 is the source code (9 = a compound expression),
    /// byte 1 the flags, bytes 2–5 the timeframe (see [`security_seconds`]),
    /// bytes 6–7 the index of the lane's status cell in the scratch region
    /// (LE u16).
    cell: [u8; 8],
    /// `kind` 0–3: the derived wire key (`derive_keys` slug), matched
    /// byte-for-byte against `params` keys. `kind` 4: a payload counted in
    /// 8-byte words: a header word (option count and ta slot count, both
    /// LE u16), the per-option timeframe table, then the expression
    /// program (either may be empty).
    key: &'a [u8],
}

/// Walk one entry; `Ok(None)` at end of table. A short or misaligned table
/// is a compiler-side defect, reported unprefixed (infrastructure class).
fn key_entry<'a>(tab: &'a [u8], pos: &mut usize) -> Result<Option<KeyEntry<'a>>, ()> {
    if *pos == tab.len() {
        return Ok(None);
    }
    let head = tab.get(*pos..*pos + 12).ok_or(()).map_err(|_| {
        set_err(&[b"malformed key table"]);
    })?;
    let mut it = head.iter().copied();
    let kind = it.next().unwrap_or(0);
    let key_len = it.next().unwrap_or(0) as usize;
    let lo = it.next().unwrap_or(0);
    let hi = it.next().unwrap_or(0);
    let index = u32::from(lo) | (u32::from(hi) << 8);
    let mut cell = [0u8; 8];
    for slot in cell.iter_mut() {
        *slot = it.next().unwrap_or(0);
    }
    // A `request.security` entry's payload is counted in 8-byte words, so
    // an option table and an expression program fit the one length byte.
    let key_len = if kind == kind::SEC {
        key_len * 8
    } else {
        key_len
    };
    let key = tab
        .get(*pos + 12..*pos + 12 + key_len)
        .ok_or(())
        .map_err(|_| {
            set_err(&[b"malformed key table"]);
        })?;
    *pos += 12 + key_len;
    Ok(Some(KeyEntry {
        kind,
        index,
        cell,
        key,
    }))
}

/// Second parse pass: write the input table and the builtin/source lanes
/// directly, internalizing the host-side write sequence of `run_aot`
/// (`tests/differential.rs`) and host loaders.
///
/// # Key-table byte layout (defined here; emitted by `emit.rs` as a data
/// segment)
///
/// A packed sequence of entries, no padding, little-endian, `keytab_len`
/// bytes total; entry count is implicit. Each entry:
///
/// | offset | size | field |
/// |---|---|---|
/// | 0 | 1 | `kind` — compiler-emitted cell-encoding code |
/// | 1 | 1 | `key_len` — derived-key byte length |
/// | 2 | 2 | `index` (u16 LE) — input index, or source-lane ordinal for kind 3 |
/// | 4 | 8 | default cell — 8 input-table bytes (kinds 0–2), or LE u32 builtin lane in bytes 0–3 (kind 3) |
/// | 12 | `key_len` | the derived key (UTF-8 slug, matched as raw bytes) |
///
/// Fill order: every entry's default first, then the wire `params` overlay
/// (unknown wire keys ignored), then bars into the builtin lanes (only
/// `0..slice_end`; missing `volume` = NaN; `time`/`timestamp` both
/// accepted, i64 ms stored as exact f64), then kind-3 builtin-lane copies
/// into the `input.source` lanes. Returns 0, or −1 with the error slot set.
///
/// # Safety
///
/// `json_ptr` must point to `json_len` initialized bytes in this module's
/// linear memory. `keytab_ptr` must point to an initialized, compiler-emitted
/// key table of `keytab_len` bytes. Both regions must remain valid for the
/// duration of the call and must be disjoint from the input-table and
/// builtin/source-lane cells that this function writes. The emitted caller
/// establishes that separation by relocating the JSON into the active arena
/// above the complete run layout and materializing the key table below the
/// lane floor before calling this function.
#[no_mangle]
pub unsafe extern "C" fn standalone_wasm_fill_lanes(
    json_ptr: *const u8,
    json_len: usize,
    slice_end: u32,
    keytab_ptr: *const u8,
    keytab_len: usize,
) -> i32 {
    if linear_memory_range(json_ptr, json_len).is_none()
        || linear_memory_range(keytab_ptr, keytab_len).is_none()
    {
        set_err(&[b"lane input range is outside linear memory"]);
        return -1;
    }
    // SAFETY: `linear_memory_range` checked the JSON slice invariants; the
    // function contract additionally keeps the relocated input live.
    let text = unsafe { core::slice::from_raw_parts(json_ptr, json_len) };
    // SAFETY: `linear_memory_range` checked the table slice invariants; the
    // compiler-emitted table remains initialized and immutable during the call.
    let tab = unsafe { core::slice::from_raw_parts(keytab_ptr, keytab_len) };
    let result = if is_binary_input(text) {
        binary_input(text).and_then(|input| {
            if input.bars != slice_end || input.bars != load_u32(OFF_BARS) {
                return bad(b"binary bar count does not match prepared layout");
            }
            if input.reference {
                // prepare may reset execution state, but must retain the same
                // primary-lane layout. No host-supplied pointer is accepted.
                // SAFETY: this borrow ends before fill_pass or any error call.
                let st = unsafe { state() };
                if !st.resident_reuse
                    || Some(st.resident_id) != input.dataset_id
                    || st.resident_bars != slice_end
                    || st.resident_base != load_u32(OFF_BUILTIN_BASE)
                {
                    return bad(b"resident dataset layout changed");
                }
            }
            fill_pass(
                input.metadata,
                slice_end,
                tab,
                if input.reference {
                    None
                } else {
                    Some(input.lanes)
                },
            )?;
            if let Some(id) = input.dataset_id {
                // SAFETY: fill_pass completed all validation and writes. Only
                // scalar fields are retained, never a Rust slice or reference.
                let st = unsafe { state() };
                st.resident_id = id;
                st.resident_bars = slice_end;
                st.resident_base = load_u32(OFF_BUILTIN_BASE);
                st.resident_valid = true;
                st.resident_reuse = false;
            }
            Ok(())
        })
    } else {
        fill_pass(text, slice_end, tab, None)
    };
    match result {
        Ok(()) => 0,
        Err(()) => -1,
    }
}

fn fill_pass(
    text: &[u8],
    slice_end: u32,
    tab: &[u8],
    binary_lanes: Option<&[u8]>,
) -> Result<(), ()> {
    #[cfg(feature = "broker")]
    {
        // SAFETY: this synchronous fill pass owns the short state borrow.
        let st = unsafe { state() };
        st.mincontract = 1.0;
        st.symbol_currency = 0;
        st.pointvalue = 1.0;
        st.include_results = false;
        st.binary_results = false;
        st.objective_results = false;
        st.trade_results = false;
        st.symbol_currency_len = 0;
    }
    let mut economics_seen = 0u32;
    let bars = load_u32(OFF_BARS);
    let builtin_base = load_u32(OFF_BUILTIN_BASE);
    let lane_addr = |lane: u32, bar: u32| builtin_base + lane * 8 * bars + bar * 8;
    let fill = slice_end.min(bars);

    // Defaults for every scalar input.
    let mut pos = 0usize;
    while let Some(entry) = key_entry(tab, &mut pos)? {
        if entry.kind < kind::FIRST_LANE_KIND {
            store_cell(INPUT_TABLE_BASE + entry.index * 8, &entry.cell);
        }
    }

    // One top-level walk: `params` overlays the table, `bars` fills lanes.
    // Structure was validated by `scan_pass`, but every shape check is
    // repeated (cheap) so a bug here degrades to an error, never UB.
    let s = &mut Scan { b: text, pos: 0 };
    s.skip_ws();
    s.expect(b'{', b"expected object")?;
    s.skip_ws();
    if !s.eat(b'}') {
        loop {
            let key = s.string()?;
            s.skip_ws();
            s.expect(b':', b"expected ':'")?;
            s.skip_ws();
            match s.span(key) {
                #[cfg(feature = "broker")]
                b"currency" => {
                    if economics_seen & 64 != 0 {
                        return bad(b"duplicate currency field");
                    }
                    economics_seen |= 64;
                    let span = s.string()?;
                    let value = s.span(span);
                    if value.is_empty()
                        || value.len() > 8
                        || !value.iter().all(u8::is_ascii_uppercase)
                    {
                        return bad(b"currency must be an uppercase quote currency code");
                    }
                    let code = if value.len() == 3 {
                        u32::from(value[0])
                            | (u32::from(value[1]) << 8)
                            | (u32::from(value[2]) << 16)
                    } else {
                        0
                    };
                    // SAFETY: the parser owns the short synchronous state borrow.
                    let st = unsafe { state() };
                    st.symbol_currency = code;
                    // At most eight bytes, checked above; the text itself is
                    // kept for the compact trade record to report.
                    for (slot, byte) in st.symbol_currency_text.iter_mut().zip(value) {
                        *slot = *byte;
                    }
                    st.symbol_currency_len = value.len() as u8;
                }
                b"mintick" | b"mincontract" | b"pointvalue" => {
                    let field = s.span(key);
                    let flag = if field == b"mintick" {
                        1
                    } else if field == b"mincontract" {
                        2
                    } else {
                        4
                    };
                    if economics_seen & flag != 0 {
                        return bad(b"duplicate symbol economics field");
                    }
                    economics_seen |= flag;
                    let span = s.number()?;
                    let value = s.f64_of(span)?;
                    if !value.is_finite() || value <= 0.0 {
                        return bad(b"symbol economics must be positive finite numbers");
                    }
                    if flag == 1 {
                        store_f64(0x38, value);
                    } else {
                        #[cfg(feature = "broker")]
                        {
                            // SAFETY: this borrow ends before continuing the parser.
                            let st = unsafe { state() };
                            if flag == 2 {
                                st.mincontract = value;
                            } else {
                                st.pointvalue = value;
                            }
                        }
                    }
                }
                #[cfg(feature = "broker")]
                b"include_results" | b"binary_results" | b"objective_results"
                | b"trade_results" => {
                    let binary = s.span(key) == b"binary_results";
                    let objective = s.span(key) == b"objective_results";
                    let trade = s.span(key) == b"trade_results";
                    let flag = if trade {
                        128
                    } else if objective {
                        32
                    } else if binary {
                        16
                    } else {
                        8
                    };
                    if economics_seen & flag != 0 {
                        return bad(b"duplicate result format field");
                    }
                    economics_seen |= flag;
                    let enabled = s.peek() == Some(b't');
                    s.literal(if enabled { b"true" } else { b"false" })?;
                    // SAFETY: this assignment retains no state borrow.
                    let st = unsafe { state() };
                    if trade {
                        st.trade_results = enabled;
                    } else if objective {
                        st.objective_results = enabled;
                    } else if binary {
                        st.binary_results = enabled;
                    } else {
                        st.include_results = enabled;
                    }
                }
                b"params" => fill_params(s, tab)?,
                b"bars" => fill_bars(s, fill, &lane_addr)?,
                _ => s.value(0)?,
            }
            s.skip_ws();
            if s.eat(b',') {
                s.skip_ws();
                continue;
            }
            s.expect(b'}', b"expected ',' or '}'")?;
            break;
        }
    }

    #[cfg(feature = "broker")]
    {
        // SAFETY: short state borrow, no calls while it is live.
        let st = unsafe { state() };
        if st.trade_results && (st.include_results || st.binary_results || st.objective_results) {
            return bad(b"trade_results cannot be combined with another result format");
        }
        if st.binary_results && !st.include_results {
            return bad(b"binary_results requires include_results");
        }
        if st.objective_results && (st.include_results || st.binary_results) {
            return bad(b"objective_results cannot be combined with full results");
        }
    }

    if let Some(lanes) = binary_lanes {
        // The same layout calculation used by prepare guarantees destination
        // capacity; check it again before writing host-provided numeric bytes.
        let count = (bars as usize)
            .checked_mul(6)
            .ok_or_else(|| set_err(&[b"binary lane count overflow"]))?;
        let byte_len = count
            .checked_mul(8)
            .ok_or_else(|| set_err(&[b"binary lane length overflow"]))?;
        if lanes.len() != byte_len
            || linear_memory_range(builtin_base as *const u8, byte_len).is_none()
        {
            return bad(b"binary lanes outside prepared memory");
        }
        for (i, bytes) in lanes.chunks_exact(8).enumerate() {
            let cell: [u8; 8] = bytes
                .try_into()
                .map_err(|_| set_err(&[b"invalid binary cell"]))?;
            let mut value = f64::from_le_bytes(cell);
            if i >= (bars as usize) * 5 {
                // Binary timestamps are exact JS-safe integer milliseconds.
                if !value.is_finite()
                    || value.abs() > 9_007_199_254_740_991.0
                    || value != (value as i64) as f64
                {
                    return bad(b"binary time must be a safe integer");
                }
            } else if !value.is_finite() {
                value = f64::NAN;
            }
            store_f64(builtin_base + (i as u32) * 8, value);
        }
    }

    // `input.source` lanes: copy the declaration's default builtin lane
    // (non-builtin defaults were refused at compile time).
    let mut pos = 0usize;
    while let Some(entry) = key_entry(tab, &mut pos)? {
        if entry.kind != kind::SRC {
            continue;
        }
        let src = u32::from_le_bytes([entry.cell[0], entry.cell[1], entry.cell[2], entry.cell[3]]);
        if src >= BUILTIN_LANE_COUNT {
            set_err(&[b"malformed key table"]);
            return Err(());
        }
        for bar in 0..fill {
            store_f64(
                lane_addr(BUILTIN_LANE_COUNT + entry.index, bar),
                load_f64(lane_addr(src, bar)),
            );
        }
    }

    // Same-symbol `request.security` lanes, resampled from the bars above.
    // The interpreter resolves a request when its instruction first runs:
    // an unservable timeframe or length is an error only if the request
    // executes, and the resample is charged to the budget at that point.
    // So each lane leaves a status cell for its sites to act on: the
    // one-time budget charge, or a negated status code.
    let scratch_base = load_u32(OFF_SCRATCH_BASE);
    let mut exchange = None;
    let mut pos = 0usize;
    while let Some(entry) = key_entry(tab, &mut pos)? {
        if entry.kind != kind::SEC {
            continue;
        }
        // A module with lanes requires the exchange's clock and session.
        let exchange = match exchange {
            Some(exchange) => exchange,
            None => *exchange.insert(request_exchange(text)?),
        };
        let cell = u32::from(u16::from_le_bytes([entry.cell[6], entry.cell[7]]));
        let outcome = match fill_security_lane(&entry, fill, &lane_addr, exchange) {
            Ok(charge) => charge,
            Err(SecurityError::Status(code)) => -i64::from(code),
            Err(SecurityError::Malformed) => {
                set_err(&[b"malformed key table"]);
                return Err(());
            }
        };
        store_i64(scratch_base + cell * 8, outcome);
    }
    Ok(())
}

/// Write the request's chart timeframe into the four scratch cells from
/// `cell`: its length in seconds, its multiplier, its `timeframe.is*` bits
/// and what reading `timeframe.period` adds to a call's budget charge
/// (`calendar::ChartTimeframe`). Returns 0, or −1 with the error slot set.
///
/// The emitted entry point calls it after [`standalone_wasm_fill_lanes`],
/// and only in a module whose program reads the chart timeframe. Such a
/// program requires the request's `timeframe` string: the interpreter's
/// `1D` default would silently mismeasure an intraday feed. Every other
/// module ignores the field like any unknown key, and carries none of this.
///
/// The interpreter refuses market data whose timeframe has no millisecond
/// length, or puts some bar's nominal close (`open + length`) outside `i64`,
/// before the script runs; the same request is refused here. So the emitted
/// `time_close` cannot overflow.
///
/// # Safety
///
/// `json_ptr` must point to `json_len` initialized bytes in this module's
/// linear memory for the duration of the call: the request that
/// [`standalone_wasm_fill_lanes`] has just filled the lanes from.
#[no_mangle]
pub unsafe extern "C" fn standalone_wasm_fill_timeframe(
    json_ptr: *const u8,
    json_len: usize,
    slice_end: u32,
    cell: u32,
) -> i32 {
    if linear_memory_range(json_ptr, json_len).is_none() {
        set_err(&[b"lane input range is outside linear memory"]);
        return -1;
    }
    // SAFETY: `linear_memory_range` checked the slice invariants; the
    // function contract keeps the request live.
    let text = unsafe { core::slice::from_raw_parts(json_ptr, json_len) };
    let metadata = if is_binary_input(text) {
        binary_input(text).map(|input| input.metadata)
    } else {
        Ok(text)
    };
    match metadata
        .and_then(request_timeframe)
        .and_then(|timeframe| fill_chart_timeframe(cell, timeframe, slice_end))
    {
        Ok(()) => 0,
        Err(()) => -1,
    }
}

/// The request's `timeframe`, from a walk of its top-level keys. Structure
/// was validated by [`scan_pass`], but every shape check is repeated, as in
/// [`fill_pass`].
fn request_timeframe(text: &[u8]) -> Result<crate::calendar::ChartTimeframe, ()> {
    let s = &mut Scan { b: text, pos: 0 };
    let mut timeframe = None;
    s.skip_ws();
    s.expect(b'{', b"expected object")?;
    s.skip_ws();
    if !s.eat(b'}') {
        loop {
            let key = s.string()?;
            s.skip_ws();
            s.expect(b':', b"expected ':'")?;
            s.skip_ws();
            if s.span(key) == b"timeframe" {
                if timeframe.is_some() {
                    set_err(&[b"duplicate top-level key: ", b"timeframe"]);
                    return Err(());
                }
                if s.peek() != Some(b'"') {
                    set_err(&[b"timeframe must be a string"]);
                    return Err(());
                }
                let span = s.string()?;
                let Some(parsed) = crate::calendar::chart_timeframe(s.span(span)) else {
                    set_err(&[b"timeframe must be ASCII text without escapes"]);
                    return Err(());
                };
                timeframe = Some(parsed);
            } else {
                s.value(0)?;
            }
            s.skip_ws();
            if s.eat(b',') {
                s.skip_ws();
                continue;
            }
            s.expect(b'}', b"expected ',' or '}'")?;
            break;
        }
    }
    timeframe.ok_or_else(|| set_err(&[b"missing required field: ", b"timeframe"]))
}

/// The exchange clock and session a module with lanes groups bars on.
#[derive(Clone, Copy)]
struct Exchange {
    /// The fixed offset east of UTC, in seconds, or `None` for a timezone
    /// that is not a fixed offset (an IANA zone, whose clocks may change).
    offset: Option<i32>,
    session: pinewasm_tz::SessionShape,
    /// The chart timeframe's nominal length, `timeframe.in_seconds()`.
    chart_seconds: i64,
}

impl Exchange {
    /// Whether bars can be grouped into buckets of `seconds`, mirroring
    /// `pinewasm_vm`'s `MarketData::bucket_refusal` for a module, which has
    /// no trading-day lane or host-supplied requested bars: `Ok` with the
    /// offset to group on, or the status a reading site stops with.
    fn grouping(self, seconds: i64) -> Result<i64, SecurityError> {
        use pinewasm_tz::SessionShape as S;
        // No longer than the chart's: each bar is its own bucket, wherever
        // buckets start or the clock reads.
        if seconds <= self.chart_seconds {
            return Ok(self.offset.map_or(0, i64::from));
        }
        let offset = self
            .offset
            .ok_or(SecurityError::Status(status::SECURITY_TIMEZONE_UNSUPPORTED))?;
        let continuous =
            self.session == S::Continuous || (self.session == S::Absent && offset == 0);
        let settled = continuous || (seconds == 86_400 && self.session == S::WithinDay);
        if !settled {
            return Err(SecurityError::Status(status::SECURITY_SESSION_UNSETTLED));
        }
        Ok(i64::from(offset))
    }
}

/// The request's `timezone`, `session_hours` and `timeframe`, all required,
/// from walks of its top-level keys: a fixed-offset timezone in the forms
/// `pinewasm_tz::parse_fixed` reads (anything else is kept as `None`, for
/// the sites that group on it to refuse), a session specification, and the
/// chart timeframe.
fn request_exchange(text: &[u8]) -> Result<Exchange, ()> {
    let chart_seconds = request_timeframe(text)?.seconds;
    let timezone = request_string_field(text, b"timezone")?
        .ok_or_else(|| set_err(&[b"missing required field: ", b"timezone"]))?;
    let session = request_string_field(text, b"session_hours")?
        .ok_or_else(|| set_err(&[b"missing required field: ", b"session_hours"]))?;
    let offset = pinewasm_tz::parse_fixed(utf8_field(timezone, b"timezone")?);
    let session =
        pinewasm_tz::session_shape(utf8_field(session, b"session_hours")?).ok_or_else(|| {
            set_err(&[
                b"session_hours must be a session specification such as 24x7 or 0930-1600:23456",
            ]);
        })?;
    Ok(Exchange {
        offset,
        session,
        chart_seconds,
    })
}

/// A request field's bytes as text, or the error that it is not UTF-8.
fn utf8_field<'a>(bytes: &'a [u8], what: &[u8]) -> Result<&'a str, ()> {
    core::str::from_utf8(bytes).map_err(|_| set_err(&[what, b" must be UTF-8 text"]))
}

/// The bytes of the top-level string field `key`, or `None` when the request
/// has none. Structure was validated by [`scan_pass`]; a duplicate key, a
/// value that is not a string, or a string with an escape is refused.
fn request_string_field<'a>(text: &'a [u8], key: &[u8]) -> Result<Option<&'a [u8]>, ()> {
    let s = &mut Scan { b: text, pos: 0 };
    let mut found = None;
    s.skip_ws();
    s.expect(b'{', b"expected object")?;
    s.skip_ws();
    if !s.eat(b'}') {
        loop {
            let name = s.string()?;
            s.skip_ws();
            s.expect(b':', b"expected ':'")?;
            s.skip_ws();
            if s.span(name) == key {
                if found.is_some() {
                    set_err(&[b"duplicate top-level key: ", key]);
                    return Err(());
                }
                if s.peek() != Some(b'"') {
                    set_err(&[key, b" must be a string"]);
                    return Err(());
                }
                let span = s.string()?;
                let bytes = s.span(span);
                if bytes.contains(&b'\\') {
                    set_err(&[key, b" must be text without escapes"]);
                    return Err(());
                }
                found = Some(bytes);
            } else {
                s.value(0)?;
            }
            s.skip_ws();
            if s.eat(b',') {
                s.skip_ws();
                continue;
            }
            s.expect(b'}', b"expected ',' or '}'")?;
            break;
        }
    }
    Ok(found)
}

/// Check `timeframe` against the bars the interpreter would run, then store
/// it from scratch cell `cell`.
fn fill_chart_timeframe(
    cell: u32,
    timeframe: crate::calendar::ChartTimeframe,
    slice_end: u32,
) -> Result<(), ()> {
    // The interpreter's two messages, one shared prefix.
    const OUT_OF_RANGE: &[u8] = b"timeframe close time exceeds the i64 timestamp range";
    let Some(millis) = timeframe.seconds.checked_mul(1000) else {
        set_err(&[
            OUT_OF_RANGE,
            b": the timeframe has no representable millisecond duration",
        ]);
        return Err(());
    };
    let bars = load_u32(OFF_BARS);
    let time_lane = load_u32(OFF_BUILTIN_BASE) + LANE_TIME * 8 * bars;
    for bar in 0..slice_end.min(bars) {
        // Integer milliseconds stored as f64 (`fill_bar`, binary lanes).
        let open = load_f64(time_lane + bar * 8) as i64;
        if open.checked_add(millis).is_none() {
            let mut buf = itoa::Buffer::new();
            set_err(&[OUT_OF_RANGE, b" at bar ", buf.format(bar).as_bytes()]);
            return Err(());
        }
    }
    let base = load_u32(OFF_SCRATCH_BASE) + cell * 8;
    store_i64(base, timeframe.seconds);
    store_i64(base + 8, timeframe.multiplier);
    store_i64(base + 16, timeframe.flags);
    store_i64(base + 24, timeframe.period_extra);
    Ok(())
}

/// Why a `request.security` lane was not filled.
enum SecurityError {
    /// A compiler-side defect in the key table: fatal.
    Malformed,
    /// The run status a site reading this lane must stop with.
    Status(i32),
}

/// `request.security` source codes (key-table kind 4, cell byte 0). Mirrors
/// the emitter's `SecuritySpec` and the source selection in `htf_source`.
mod security_source {
    pub const HL2: u8 = 5;
    pub const HLC3: u8 = 6;
    pub const OHLC4: u8 = 7;
    pub const HLCC4: u8 = 8;
    /// Not a series: the lane is a compound expression evaluated once per
    /// higher-timeframe bucket (see [`eval_security_expr`]).
    pub const EXPR: u8 = 9;
    /// A `time(timeframe)` call: each bar's bucket opening time, the open
    /// time of the bucket's first bar, which is what `time` returns in PineWASM.
    pub const TIME_OPEN: u8 = 10;
}

const SECURITY_GAPS_ON: u8 = 1;
const SECURITY_LOOKAHEAD_ON: u8 = 2;
/// The timeframe is the selected option of a finite string input.
const SECURITY_FROM_INPUT: u8 = 4;

/// A kind-4 payload's header word: the option count and the expression's
/// ta slot count.
fn security_header(entry: &KeyEntry<'_>) -> Result<(usize, u32), SecurityError> {
    let word = entry.key.get(..8).ok_or(SecurityError::Malformed)?;
    let mut it = word.iter().copied();
    let mut next = || u16::from(it.next().unwrap_or(0));
    let options = next() | (next() << 8);
    let ta_slots = next() | (next() << 8);
    Ok((usize::from(options), u32::from(ta_slots)))
}

/// The timeframe of one `request.security` lane for this evaluation, in
/// seconds: positive for fixed-width buckets, 0 for the chart's own period.
///
/// With a fixed timeframe, cell bytes 2–5 are its seconds (LE u32). With
/// `SECURITY_FROM_INPUT`, bytes 2–3 are the input index and bytes 4–5 the
/// default option (both LE u16), and the payload carries one LE i64 of
/// seconds per option after its header word (0 = the chart's own period,
/// negative = not servable).
fn security_seconds(entry: &KeyEntry<'_>, options: usize) -> Result<i64, SecurityError> {
    let cell = &entry.cell;
    if cell[1] & SECURITY_FROM_INPUT == 0 {
        return Ok(i64::from(u32::from_le_bytes([
            cell[2], cell[3], cell[4], cell[5],
        ])));
    }
    let input = u32::from(u16::from_le_bytes([cell[2], cell[3]]));
    let default = usize::from(u16::from_le_bytes([cell[4], cell[5]]));
    // The option index the params overlay left in the input table; an
    // index outside the declaration falls back to its default, as the
    // emitted input read does.
    let selected = load_i64(INPUT_TABLE_BASE + input * 8);
    let index = match usize::try_from(selected) {
        Ok(index) if index < options => index,
        _ => default,
    };
    let bytes = entry
        .key
        .get(8 + index * 8..16 + index * 8)
        .ok_or(SecurityError::Malformed)?;
    let mut seconds = [0u8; 8];
    seconds.copy_from_slice(bytes);
    let seconds = i64::from_le_bytes(seconds);
    if seconds < 0 {
        return Err(SecurityError::Status(
            status::SECURITY_TIMEFRAME_UNAVAILABLE,
        ));
    }
    Ok(seconds)
}

/// One aggregated higher-timeframe bar.
#[derive(Clone, Copy)]
struct HtfBar {
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
}

/// A plain source over one aggregated bar, `na` when not finite.
fn htf_source(bar: &HtfBar, source: u8) -> f64 {
    let raw = match source {
        0 => bar.open,
        1 => bar.high,
        2 => bar.low,
        3 => bar.close,
        4 => bar.volume,
        security_source::HL2 => f64::midpoint(bar.high, bar.low),
        security_source::HLC3 => (bar.high + bar.low + bar.close) / 3.0,
        security_source::OHLC4 => (bar.open + bar.high + bar.low + bar.close) / 4.0,
        security_source::HLCC4 => (bar.high + bar.low + bar.close + bar.close) / 4.0,
        _ => f64::NAN,
    };
    if raw.is_finite() {
        raw
    } else {
        f64::NAN
    }
}

/// Compound-expression opcodes (kind-4 payload after the option table). Each
/// instruction is one 8-byte word `[op, a, b: u16, c: u32]`, little-endian;
/// `CONST` is followed by one word holding the f64. The program is the
/// expression in postfix order, so every node is evaluated exactly once per
/// bucket, as the recursive `eval_security_expr` does.
mod expr_op {
    pub const CONST: u8 = 0;
    /// `a` = source code.
    pub const SOURCE: u8 = 1;
    pub const NEG: u8 = 2;
    /// `a` = operator.
    pub const BINARY: u8 = 3;
    /// `b` = history slot, `c` = offset in buckets.
    pub const HISTORY: u8 = 4;
    /// `a` = function, `b` = ta slot, `c` = length.
    pub const TA: u8 = 5;
    /// `a` = function, `b` = ta slot, `c` = index of the `input.int` length.
    pub const TA_INPUT: u8 = 6;
}

const EXPR_TA_EMA: u8 = 0;
const EXPR_TA_SMA: u8 = 1;
const EXPR_TA_CHANGE: u8 = 2;
/// Bounds the emitter enforces; the evaluator's fixed state is sized by them.
const EXPR_MAX_TA_SLOTS: usize = 16;
const EXPR_MAX_STACK: usize = 32;

/// The secondary context's EMA: seeded with the mean of the first `length`
/// values, then smoothed.
#[derive(Clone, Copy)]
struct ExprEma {
    sum: f64,
    value: f64,
    count: u32,
    seeded: bool,
}

struct ExprInstr {
    op: u8,
    a: u8,
    b: u32,
    c: u32,
}

fn expr_instr(code: &[u8], pos: usize) -> Option<ExprInstr> {
    let word = code.get(pos..pos + 8)?;
    let mut it = word.iter().copied();
    let op = it.next()?;
    let a = it.next()?;
    let b = u32::from(it.next()?) | (u32::from(it.next()?) << 8);
    let mut c = 0u32;
    let mut shift = 0;
    for byte in it {
        c |= u32::from(byte) << shift;
        shift += 8;
    }
    Some(ExprInstr { op, a, b, c })
}

/// A `ta.*` length, from the instruction or from its `input.int` cell, or
/// `None` outside 1..=100000 (an invalid length).
fn expr_ta_length(instr: &ExprInstr) -> Option<u32> {
    let length = if instr.op == expr_op::TA_INPUT {
        load_i64(INPUT_TABLE_BASE + instr.c * 8)
    } else {
        i64::from(instr.c)
    };
    (1..=100_000).contains(&length).then_some(length as u32)
}

/// Validate an expression program's lengths and total its work:
/// `(nodes, per_bucket)`, where binding the
/// expression's inputs costs one unit per node once per request, and each
/// bucket costs one unit per node plus a `ta.*` node's length.
fn expr_work(code: &[u8]) -> Result<(u64, u64), SecurityError> {
    let mut nodes = 0u64;
    let mut work = 0u64;
    let mut pos = 0usize;
    while pos < code.len() {
        let instr = expr_instr(code, pos).ok_or(SecurityError::Malformed)?;
        pos += 8;
        nodes = nodes.saturating_add(1);
        work = work.saturating_add(1);
        match instr.op {
            expr_op::CONST => pos += 8,
            expr_op::TA | expr_op::TA_INPUT => {
                let length = expr_ta_length(&instr)
                    .ok_or(SecurityError::Status(status::SECURITY_LENGTH_OUT_OF_RANGE))?;
                work = work.saturating_add(u64::from(length));
            }
            _ => {}
        }
    }
    Ok((nodes, work))
}

/// Evaluate a compound expression over one higher-timeframe bucket.
///
/// `bucket` is the bucket's ordinal, which indexes the per-bucket histories:
/// ta slot `t` keeps its source history in scratch lane `scratch + t`, and
/// history slot `h` keeps its values in lane `scratch + ta_slots + h`, one
/// cell per bucket. `NaN` is the secondary context's `na`.
#[allow(clippy::too_many_arguments)]
fn eval_security_expr(
    code: &[u8],
    bar: &HtfBar,
    bucket: u32,
    scratch: u32,
    ta_slots: u32,
    emas: &mut [ExprEma; EXPR_MAX_TA_SLOTS],
    lane_addr: &impl Fn(u32, u32) -> u32,
) -> Result<f64, SecurityError> {
    let malformed = || SecurityError::Malformed;
    let mut stack = [f64::NAN; EXPR_MAX_STACK];
    let mut depth = 0usize;
    let mut pos = 0usize;
    while pos < code.len() {
        let instr = expr_instr(code, pos).ok_or_else(malformed)?;
        pos += 8;
        let value = match instr.op {
            expr_op::CONST => {
                let bytes = code.get(pos..pos + 8).ok_or_else(malformed)?;
                pos += 8;
                let mut raw = [0u8; 8];
                raw.copy_from_slice(bytes);
                f64::from_le_bytes(raw)
            }
            expr_op::SOURCE => htf_source(bar, instr.a),
            expr_op::NEG => {
                depth = depth.checked_sub(1).ok_or_else(malformed)?;
                -stack.get(depth).copied().unwrap_or(f64::NAN)
            }
            expr_op::BINARY => {
                depth = depth.checked_sub(2).ok_or_else(malformed)?;
                let l = stack.get(depth).copied().unwrap_or(f64::NAN);
                let r = stack.get(depth + 1).copied().unwrap_or(f64::NAN);
                expr_binary(instr.a, l, r)
            }
            expr_op::HISTORY => {
                depth = depth.checked_sub(1).ok_or_else(malformed)?;
                let current = stack.get(depth).copied().unwrap_or(f64::NAN);
                let lane = scratch + ta_slots + instr.b;
                store_f64(lane_addr(lane, bucket), current);
                match bucket.checked_sub(instr.c) {
                    Some(index) => load_f64(lane_addr(lane, index)),
                    None => f64::NAN,
                }
            }
            expr_op::TA | expr_op::TA_INPUT => {
                depth = depth.checked_sub(1).ok_or_else(malformed)?;
                let source = stack.get(depth).copied().unwrap_or(f64::NAN);
                // `expr_work` validated every length before the first bucket.
                let length = expr_ta_length(&instr).ok_or_else(malformed)?;
                if instr.a == EXPR_TA_EMA {
                    let ema = emas.get_mut(instr.b as usize).ok_or_else(malformed)?;
                    expr_ema(ema, source, length)
                } else {
                    let lane = scratch + instr.b;
                    store_f64(lane_addr(lane, bucket), source);
                    expr_window(instr.a, lane, bucket, length, lane_addr)
                }
            }
            _ => return Err(malformed()),
        };
        *stack.get_mut(depth).ok_or_else(malformed)? = value;
        depth += 1;
    }
    if depth != 1 {
        return Err(malformed());
    }
    // A non-finite result is read as `na`.
    Ok(if stack[0].is_finite() {
        stack[0]
    } else {
        f64::NAN
    })
}

/// The secondary context's binary operators over `na`-propagating floats:
/// comparisons yield 1 or 0, a division
/// keeps its fractional quotient whatever its static type, and a non-finite
/// arithmetic result is `na`.
fn expr_binary(op: u8, l: f64, r: f64) -> f64 {
    if l.is_nan() || r.is_nan() {
        return f64::NAN;
    }
    let flag = |truth: bool| if truth { 1.0 } else { 0.0 };
    let value = match op {
        0 => l + r,
        1 => l - r,
        2 => l * r,
        3 => l / r,
        4 => l % r,
        5 => return flag(l < r),
        6 => return flag(l <= r),
        7 => return flag(l > r),
        8 => return flag(l >= r),
        9 => return flag(l == r),
        10 => return flag(l != r),
        _ => return f64::NAN,
    };
    if value.is_finite() {
        value
    } else {
        f64::NAN
    }
}

fn expr_ema(ema: &mut ExprEma, source: f64, length: u32) -> f64 {
    if source.is_nan() {
        return f64::NAN;
    }
    let alpha = 2.0 / (f64::from(length) + 1.0);
    if ema.seeded {
        ema.value = alpha * source + (1.0 - alpha) * ema.value;
    } else {
        ema.sum += source;
        ema.count += 1;
        if ema.count >= length {
            ema.value = ema.sum / f64::from(length);
            ema.seeded = true;
        }
    }
    if ema.seeded && ema.value.is_finite() {
        ema.value
    } else {
        f64::NAN
    }
}

/// `ta.sma` and `ta.change` over a ta slot's per-bucket source history, whose
/// newest cell is `bucket`.
fn expr_window(
    func: u8,
    lane: u32,
    bucket: u32,
    length: u32,
    lane_addr: &impl Fn(u32, u32) -> u32,
) -> f64 {
    let seen = bucket + 1;
    let value = match func {
        EXPR_TA_SMA => {
            if seen < length {
                return f64::NAN;
            }
            let mut sum = 0.0f64;
            let mut index = seen - length;
            while index < seen {
                let cell = load_f64(lane_addr(lane, index));
                if cell.is_nan() {
                    return f64::NAN;
                }
                sum += cell;
                index += 1;
            }
            sum / f64::from(length)
        }
        EXPR_TA_CHANGE => {
            if seen <= length {
                return f64::NAN;
            }
            load_f64(lane_addr(lane, bucket)) - load_f64(lane_addr(lane, bucket - length))
        }
        _ => return f64::NAN,
    };
    if value.is_finite() {
        value
    } else {
        f64::NAN
    }
}

/// Fill one `request.security` lane: the chart bars aggregated into
/// fixed-width higher-timeframe buckets and mapped back per chart bar, the
/// same-feed model of `pinewasm_vm::resample`, without its intermediate
/// vectors.
///
/// Buckets follow the UTC clock ([`clock_bucket_id`]), not the feed's first
/// bar; a bucket is a run of consecutive bars with the same bucket id. Its value is the requested
/// source over the aggregated bar (open first, high max, low min, close
/// last, volume summed), or the compound expression evaluated once for the
/// bucket. With lookahead off a chart bar sees its own bucket only on the
/// bucket's last bar and the previous bucket before that; with lookahead on
/// it sees its own bucket's final value throughout. With gaps on a value
/// appears only on the bar where the visible bucket changes: a bucket's last
/// bar with lookahead off, its first with lookahead on.
///
/// Returns the budget units the lane's first read charges.
fn fill_security_lane(
    entry: &KeyEntry<'_>,
    bars: u32,
    lane_addr: &impl Fn(u32, u32) -> u32,
    exchange: Exchange,
) -> Result<i64, SecurityError> {
    let source = entry.cell[0];
    let flags = entry.cell[1];
    if source > security_source::TIME_OPEN {
        return Err(SecurityError::Malformed);
    }
    let (options, ta_slots) = security_header(entry)?;
    if flags & SECURITY_FROM_INPUT == 0 && options != 0 {
        return Err(SecurityError::Malformed);
    }
    let htf_seconds = security_seconds(entry, options)?;
    // A compound expression's program follows the option table; its scratch
    // lanes follow the output lane.
    let code = entry.key.get(8 + options * 8..).unwrap_or(&[]);
    // What the first read of this lane charges: one unit per bar for a plain
    // resample; for a compound one, the expression's work plus two per bar,
    // and one unit per node for binding its inputs.
    let (once, per_bar) = if source == security_source::EXPR {
        let (nodes, work) = expr_work(code)?;
        (nodes, work.saturating_add(2))
    } else {
        (0, 1)
    };
    let charge = i64::try_from(u64::from(bars).saturating_mul(per_bar).saturating_add(once))
        .unwrap_or(i64::MAX);
    // The chart's own period: every bar is its own bucket. The module is
    // not told the period, so it cannot walk period-wide buckets from the
    // first bar. It also has no lower-than-chart check: such a request
    // resamples into its own, narrower buckets.
    let own_period = htf_seconds == 0;
    let step_ms = htf_seconds.saturating_mul(1000);
    // Buckets follow the exchange's clock: bar opens shifted by its fixed
    // offset, for a session whose bucket starts are settled.
    let shift_ms = if own_period {
        0
    } else {
        exchange.grouping(htf_seconds)?.saturating_mul(1000)
    };
    let out = BUILTIN_LANE_COUNT + entry.index;
    let time_at = |bar: u32| load_f64(lane_addr(LANE_TIME, bar)) as i64;

    let gaps_on = flags & SECURITY_GAPS_ON != 0;
    let lookahead_on = flags & SECURITY_LOOKAHEAD_ON != 0;
    let first = if bars > 0 { time_at(0) } else { 0 };
    let bucket_of = |bar: u32| {
        if own_period {
            i64::from(bar)
        } else {
            clock_bucket_id(
                time_at(bar).saturating_add(shift_ms),
                first.saturating_add(shift_ms),
                step_ms,
            )
        }
    };

    let mut emas = [ExprEma {
        sum: 0.0,
        value: 0.0,
        count: 0,
        seeded: false,
    }; EXPR_MAX_TA_SLOTS];
    let mut previous = f64::NAN;
    let mut bucket = 0u32;
    let mut start = 0u32;
    while start < bars {
        let id = bucket_of(start);
        let mut htf = HtfBar {
            open: load_f64(lane_addr(0, start)),
            high: load_f64(lane_addr(1, start)),
            low: load_f64(lane_addr(2, start)),
            close: load_f64(lane_addr(LANE_CLOSE, start)),
            volume: load_f64(lane_addr(LANE_VOLUME, start)),
        };
        let mut end = start;
        while end + 1 < bars && bucket_of(end + 1) == id {
            end += 1;
            htf.high = htf.high.max(load_f64(lane_addr(1, end)));
            htf.low = htf.low.min(load_f64(lane_addr(2, end)));
            htf.close = load_f64(lane_addr(LANE_CLOSE, end));
            htf.volume += load_f64(lane_addr(LANE_VOLUME, end));
        }
        let value = if source == security_source::EXPR {
            eval_security_expr(code, &htf, bucket, out + 1, ta_slots, &mut emas, lane_addr)?
        } else if source == security_source::TIME_OPEN {
            // Integer milliseconds stored as f64, exact below 2^53.
            load_f64(lane_addr(LANE_TIME, start))
        } else {
            htf_source(&htf, source)
        };
        let mut bar = start;
        while bar <= end {
            let closes = bar == end;
            let visible = if lookahead_on || closes {
                value
            } else {
                previous
            };
            // The bar on which this bucket becomes the visible one: its first
            // bar with lookahead on, its last with lookahead off.
            let appears = if lookahead_on { bar == start } else { closes };
            store_f64(
                lane_addr(out, bar),
                if gaps_on && !appears {
                    f64::NAN
                } else {
                    visible
                },
            );
            bar += 1;
        }
        previous = value;
        bucket += 1;
        start = end + 1;
    }
    Ok(charge)
}

/// One day in milliseconds.
const DAY_MS: i64 = 86_400_000;

/// The bucket of a bar opening at `time` (`clock_bucket_id`), with times
/// already on the exchange's clock. A width
/// under one day walks from local midnight and restarts every day, so a
/// 60-minute bucket opens on the hour whatever bar the feed starts on. A
/// width of a day or more walks from local midnight of `first`, the feed's
/// first open time.
fn clock_bucket_id(time: i64, first: i64, step_ms: i64) -> i64 {
    if step_ms < DAY_MS {
        let day = time.div_euclid(DAY_MS);
        let per_day = DAY_MS.div_euclid(step_ms) + i64::from(DAY_MS.rem_euclid(step_ms) != 0);
        day.saturating_mul(per_day)
            .saturating_add(time.rem_euclid(DAY_MS).div_euclid(step_ms))
    } else {
        let anchor = first.div_euclid(DAY_MS).saturating_mul(DAY_MS);
        time.saturating_sub(anchor).div_euclid(step_ms)
    }
}

/// A number token in `params` value position, with a param-shaped
/// diagnostic replacing the raw tokenizer one.
fn param_number(s: &mut Scan) -> Result<(usize, usize), ()> {
    s.number().map_err(|_| {
        set_err(&[b"param value must be a number"]);
    })
}

/// The `params` object: overlay wire values onto the input table, matched
/// by derived key. Unknown keys are ignored (the schema, not the wire, is
/// authoritative for what exists).
fn fill_params(s: &mut Scan, tab: &[u8]) -> Result<(), ()> {
    if s.peek() != Some(b'{') {
        set_err(&[b"params must be an object"]);
        return Err(());
    }
    s.pos += 1;
    s.skip_ws();
    if s.eat(b'}') {
        return Ok(());
    }
    loop {
        let key = s.string()?;
        s.skip_ws();
        s.expect(b':', b"expected ':'")?;
        s.skip_ws();

        let mut matched = None;
        let mut pos = 0usize;
        while let Some(entry) = key_entry(tab, &mut pos)? {
            if entry.kind < kind::FIRST_LANE_KIND && entry.key == s.span(key) {
                matched = Some(entry);
                break;
            }
        }
        match matched {
            Some(entry) => {
                let addr = INPUT_TABLE_BASE + entry.index * 8;
                match entry.kind {
                    kind::F64 => {
                        let span = param_number(s)?;
                        let v = s.f64_of(span)?;
                        store_f64(addr, v);
                    }
                    kind::I64 => {
                        let span = param_number(s)?;
                        let v = s.i64_of(span)?;
                        // MIN is the artifact's integer na cell. Refusing it
                        // is what lets the compiler lower unbounded int
                        // inputs natively without aliasing a valid value.
                        if v == i64::MIN {
                            return bad(b"integer param collides with na");
                        }
                        store_cell(addr, &v.to_le_bytes());
                    }
                    kind::BOOL => {
                        // true/false, with 0/1 numbers tolerated the way
                        // the artifact's own read is (`v != 0.0`).
                        let v = match s.peek() {
                            Some(b't') => {
                                s.literal(b"true")?;
                                1.0
                            }
                            Some(b'f') => {
                                s.literal(b"false")?;
                                0.0
                            }
                            Some(b'-' | b'0'..=b'9') => {
                                let span = s.number()?;
                                if s.f64_of(span)? != 0.0 {
                                    1.0
                                } else {
                                    0.0
                                }
                            }
                            _ => {
                                set_err(&[b"param value must be a boolean"]);
                                return Err(());
                            }
                        };
                        store_f64(addr, v);
                    }
                    _ => {
                        set_err(&[b"malformed key table"]);
                        return Err(());
                    }
                }
            }
            None => s.value(0)?,
        }
        s.skip_ws();
        if s.eat(b',') {
            s.skip_ws();
            continue;
        }
        return s.expect(b'}', b"expected ',' or '}'");
    }
}

/// The `bars` array: fill the six builtin lanes for bars `0..fill`, skip
/// (but still tokenize past) the rest.
fn fill_bars(s: &mut Scan, fill: u32, lane_addr: &dyn Fn(u32, u32) -> u32) -> Result<(), ()> {
    s.expect(b'[', b"bars must be an array")?;
    s.skip_ws();
    if s.eat(b']') {
        return Ok(());
    }
    let mut bar: u32 = 0;
    loop {
        if bar < fill {
            fill_bar(s, bar, lane_addr)?;
        } else {
            s.value(1)?;
        }
        bar = bar.saturating_add(1);
        s.skip_ws();
        if s.eat(b',') {
            s.skip_ws();
            continue;
        }
        return s.expect(b']', b"expected ',' or ']'");
    }
}

/// One bar object into lane cells at `bar`. Non-finite values normalize to
/// NaN (the binary loader does the same); `time` accepts the `timestamp` alias; unknown bar
/// keys are ignored.
fn fill_bar(s: &mut Scan, bar: u32, lane_addr: &dyn Fn(u32, u32) -> u32) -> Result<(), ()> {
    const F_OPEN: u32 = 1 << 0;
    const F_HIGH: u32 = 1 << 1;
    const F_LOW: u32 = 1 << 2;
    const F_CLOSE: u32 = 1 << 3;
    const F_TIME: u32 = 1 << 4;
    const REQUIRED: u32 = F_OPEN | F_HIGH | F_LOW | F_CLOSE | F_TIME;

    // Volume is optional; default the cell before the overlay.
    store_f64(lane_addr(LANE_VOLUME, bar), f64::NAN);

    if s.peek() != Some(b'{') {
        set_err(&[b"bars elements must be objects"]);
        return Err(());
    }
    s.pos += 1;
    s.skip_ws();
    let mut seen: u32 = 0;
    if !s.eat(b'}') {
        loop {
            let key = s.string()?;
            s.skip_ws();
            s.expect(b':', b"expected ':'")?;
            s.skip_ws();
            let field = match s.span(key) {
                b"open" => Some((0u32, F_OPEN)),
                b"high" => Some((1, F_HIGH)),
                b"low" => Some((2, F_LOW)),
                b"close" => Some((LANE_CLOSE, F_CLOSE)),
                b"volume" => Some((LANE_VOLUME, 0)),
                // The host contract accepts either timestamp spelling.
                b"time" | b"timestamp" => Some((LANE_TIME, F_TIME)),
                _ => None,
            };
            match field {
                Some((LANE_TIME, flag)) if flag == F_TIME => {
                    let span = s.number().map_err(|_| {
                        set_err(&[b"bar time must be a number"]);
                    })?;
                    let t = s.i64_of(span)?;
                    // The lane holds an f64, exact for safe integers only:
                    // the bound binary times already have.
                    if t.unsigned_abs() > 9_007_199_254_740_991 {
                        set_err(&[b"bar time must be a safe integer"]);
                        return Err(());
                    }
                    store_f64(lane_addr(LANE_TIME, bar), t as f64);
                    seen |= F_TIME;
                }
                Some((lane, flag)) => {
                    let span = s.number().map_err(|_| {
                        set_err(&[b"bar field must be a number"]);
                    })?;
                    let v = s.f64_of(span)?;
                    let v = if v.is_finite() { v } else { f64::NAN };
                    store_f64(lane_addr(lane, bar), v);
                    seen |= flag;
                }
                None => s.value(1)?,
            }
            s.skip_ws();
            if s.eat(b',') {
                s.skip_ws();
                continue;
            }
            s.expect(b'}', b"expected ',' or '}'")?;
            break;
        }
    }
    if seen & REQUIRED != REQUIRED {
        let mut buf = itoa::Buffer::new();
        set_err(&[
            b"bar ",
            buf.format(bar).as_bytes(),
            b" is missing a required field (open/high/low/close/time)",
        ]);
        return Err(());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Output serialization.

/// Append-only writer over the top of the arena. The output is always the
/// topmost allocation, so it can grow byte-by-byte and claim the band
/// (`arena_ptr = pos`) only once it is complete.
pub(crate) struct Out {
    pub(crate) start: u32,
    pub(crate) pos: u32,
}

trait ByteSink {
    fn push(&mut self, bytes: &[u8]) -> Result<(), ()>;
}

impl Out {
    #[cfg(feature = "broker")]
    pub(crate) fn number(&mut self, value: f64) -> Result<(), ()> {
        if value.is_finite() {
            self.push(ryu::Buffer::new().format_finite(value).as_bytes())
        } else {
            self.push(b"null")
        }
    }
    #[cfg(feature = "broker")]
    pub(crate) fn string(&mut self, value: &str) -> Result<(), ()> {
        serialize_string(value, self)
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<(), ()> {
        let Ok(len) = u32::try_from(bytes.len()) else {
            set_alloc_err();
            return Err(());
        };
        let Some(end) = self.pos.checked_add(len) else {
            set_alloc_err();
            return Err(());
        };
        if !ensure(end) {
            set_alloc_err();
            return Err(());
        }
        // SAFETY: `ensure(end)` made `[pos, end)` writable. All callers pass
        // static/stack data, the disjoint const pool, or initialized numeric
        // regions below `start` (checked by the binary serializer). None of
        // those source slices can overlap this append-only output allocation.
        unsafe { copy_bytes(bytes.as_ptr(), self.pos as *mut u8, bytes.len()) };
        self.pos = end;
        Ok(())
    }
}

impl ByteSink for Out {
    fn push(&mut self, bytes: &[u8]) -> Result<(), ()> {
        Out::push(self, bytes)
    }
}

/// Copy ordinary UTF-8 spans together; only JSON escape bytes need separate
/// appends. Every append still passes through the sink's checked allocation.
#[cfg(any(feature = "broker", test))]
fn serialize_string(value: &str, out: &mut impl ByteSink) -> Result<(), ()> {
    out.push(b"\"")?;
    let bytes = value.as_bytes();
    let mut start = 0;
    for (index, &byte) in bytes.iter().enumerate() {
        if byte > 31 && byte != b'"' && byte != b'\\' {
            continue;
        }
        if start < index {
            out.push(&bytes[start..index])?;
        }
        match byte {
            b'"' => out.push(b"\\\"")?,
            b'\\' => out.push(b"\\\\")?,
            _ => {
                let hex = b"0123456789abcdef";
                out.push(&[
                    b'\\',
                    b'u',
                    b'0',
                    b'0',
                    hex[(byte >> 4) as usize],
                    hex[(byte & 15) as usize],
                ])?;
            }
        }
        start = index + 1;
    }
    if start < bytes.len() {
        out.push(&bytes[start..])?;
    }
    out.push(b"\"")
}

/// Edge-detect the designated plot lane into the OUT document and return it
/// packed `(ptr << 32) | len`, or 0 with the
/// error slot set.
///
/// The lane is a per-bar posture in {1, −1, 0, NaN} (NaN = na = flat).
/// `posture_events == 0` preserves the original sparse position-event
/// contract: only transitions into +1 or −1 are emitted. A non-zero flag
/// emits every posture transition, including `0` exits (but never a leading
/// or repeated flat). Any other lane value is the expected-failure class,
/// prefixed exactly `"Indicator failure: "`.
/// `price` is the bar's close, shortest-roundtrip (ryu); `time` the bar's
/// time, printed as an integer. The empty series serializes as
/// `{"version":1,"signals":[]}` — a valid non-zero return.
#[no_mangle]
pub extern "C" fn standalone_wasm_serialize(
    plot_idx: u32,
    slice_end: u32,
    posture_events: u32,
) -> u64 {
    // SAFETY: this exported call is synchronous in the no-threads runtime;
    // the borrow ends after `start` is computed, before serialization begins.
    let st = unsafe { arena_init() };
    let Some((start, _)) = allocation_bounds(st.arena_ptr, 0, 8) else {
        set_alloc_err();
        return 0;
    };
    let mut out = Out { start, pos: start };
    match serialize_signals(plot_idx, slice_end, posture_events != 0, &mut out) {
        Ok(()) => {
            // SAFETY: the initial arena borrow ended before serialization;
            // serialization retains no `STATE` reference.
            let st = unsafe { state() };
            st.arena_ptr = out.pos;
            ((out.start as u64) << 32) | u64::from(out.pos - out.start)
        }
        Err(()) => 0,
    }
}

fn serialize_signals(
    plot_idx: u32,
    slice_end: u32,
    posture_events: bool,
    out: &mut Out,
) -> Result<(), ()> {
    let bars = load_u32(OFF_BARS);
    let builtin_base = load_u32(OFF_BUILTIN_BASE);
    let plots_base = load_u32(OFF_PLOTS_BASE);
    let plot_base = plots_base + plot_idx * 8 * bars;
    let close_base = builtin_base + LANE_CLOSE * 8 * bars;
    let time_base = builtin_base + LANE_TIME * 8 * bars;

    serialize_samples(
        slice_end.min(bars),
        posture_events,
        out,
        |bar| load_f64(plot_base + bar * 8),
        |bar| load_f64(close_base + bar * 8),
        |bar| load_f64(time_base + bar * 8),
    )
}

/// Target-independent serialization state machine. The Wasm export supplies
/// raw linear-memory loads; native sanitizer tests supply ordinary slices, so
/// both paths execute this exact transition/formatting implementation.
fn serialize_samples(
    bars: u32,
    posture_events: bool,
    out: &mut impl ByteSink,
    mut posture: impl FnMut(u32) -> f64,
    mut price: impl FnMut(u32) -> f64,
    mut time: impl FnMut(u32) -> f64,
) -> Result<(), ()> {
    out.push(b"{\"version\":1,\"signals\":[")?;
    let mut prev: i32 = 0;
    let mut first = true;
    for bar in 0..bars {
        let v = posture(bar);
        let cur = if v.is_nan() {
            0
        } else if v == 1.0 {
            1
        } else if v == -1.0 {
            -1
        } else if v == 0.0 {
            0
        } else {
            let mut buf = itoa::Buffer::new();
            set_err(&[
                b"Indicator failure: designated signal lane produced non-posture value at bar ",
                buf.format(bar).as_bytes(),
            ]);
            return Err(());
        };
        let emit = if posture_events {
            cur != prev && (cur != 0 || prev != 0)
        } else {
            cur != 0 && cur != prev
        };
        if emit {
            let price = price(bar);
            if !price.is_finite() {
                let mut buf = itoa::Buffer::new();
                set_err(&[
                    b"non-finite close price at bar ",
                    buf.format(bar).as_bytes(),
                ]);
                return Err(());
            }
            // The time lane holds an i64 stored as an exact f64, so the
            // cast back is exact.
            let time = time(bar) as i64;
            if !first {
                out.push(b",")?;
            }
            first = false;
            let mut ibuf = itoa::Buffer::new();
            let mut rbuf = ryu::Buffer::new();
            out.push(b"{\"time\":")?;
            out.push(ibuf.format(time).as_bytes())?;
            out.push(b",\"direction\":")?;
            out.push(ibuf.format(cur).as_bytes())?;
            out.push(b",\"price\":")?;
            out.push(rbuf.format_finite(price).as_bytes())?;
            out.push(b"}")?;
        }
        prev = cur;
    }
    out.push(b"]}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::String;
    use std::vec::Vec;

    #[derive(Default)]
    struct VecSink(Vec<u8>);

    impl ByteSink for VecSink {
        fn push(&mut self, bytes: &[u8]) -> Result<(), ()> {
            self.0.extend_from_slice(bytes);
            Ok(())
        }
    }

    #[test]
    fn string_spans_preserve_escaping_and_propagate_sink_failure() {
        let controls: String = (0u8..32).map(char::from).collect();
        let long = "plain α🙂 text".repeat(1000);
        for value in [
            "",
            "ordinary trade id",
            "\"\\\n\r\t",
            "α🙂\"世界",
            &controls,
            &long,
        ] {
            let mut expected = Vec::from(b"\"");
            for byte in value.bytes() {
                match byte {
                    b'"' => expected.extend_from_slice(b"\\\""),
                    b'\\' => expected.extend_from_slice(b"\\\\"),
                    0..=31 => {
                        let hex = b"0123456789abcdef";
                        expected.extend_from_slice(&[
                            b'\\',
                            b'u',
                            b'0',
                            b'0',
                            hex[(byte >> 4) as usize],
                            hex[(byte & 15) as usize],
                        ]);
                    }
                    _ => expected.push(byte),
                }
            }
            expected.push(b'"');
            let mut sink = VecSink::default();
            serialize_string(value, &mut sink).expect("valid string");
            assert_eq!(sink.0, expected);
        }
        struct LimitedSink {
            remaining: usize,
            calls: usize,
        }
        impl ByteSink for LimitedSink {
            fn push(&mut self, _: &[u8]) -> Result<(), ()> {
                self.calls += 1;
                if self.remaining == 0 {
                    return Err(());
                }
                self.remaining -= 1;
                Ok(())
            }
        }
        // Opening quote, ordinary span, escape, ordinary span, closing quote.
        for limit in 0..5 {
            let mut sink = LimitedSink {
                remaining: limit,
                calls: 0,
            };
            assert_eq!(serialize_string("left\"right", &mut sink), Err(()));
            assert_eq!(sink.calls, limit + 1, "stop immediately on sink failure");
        }
        let mut sink = LimitedSink {
            remaining: 3,
            calls: 0,
        };
        assert_eq!(serialize_string(&long, &mut sink), Ok(()));
        assert_eq!(sink.calls, 3, "ordinary UTF-8 uses one span append");
    }

    fn error_text() -> String {
        // SAFETY: this is the only test using the singleton, the test runner is
        // forced to one thread by the safety command, and the borrow ends
        // before any helper that can access the singleton runs.
        let st = unsafe { state() };
        String::from_utf8(st.err_buf[..st.err_len as usize].to_vec()).expect("ASCII error")
    }

    /// The broker allocator's blocks: powers of two from 8 bytes through the
    /// largest class, so a freed block always fits a later request of its
    /// class; anything aligned past 8 or larger keeps its exact size.
    #[test]
    fn broker_size_classes_round_up_to_powers_of_two() {
        assert_eq!(size_class(0, 8), Some((0, 8)));
        assert_eq!(size_class(1, 1), Some((0, 8)));
        assert_eq!(size_class(8, 8), Some((0, 8)));
        assert_eq!(size_class(9, 4), Some((1, 16)));
        assert_eq!(size_class(330, 8), Some((6, 512)));
        let largest = 8 << (FREE_CLASSES - 1);
        assert_eq!(size_class(largest, 8), Some((FREE_CLASSES - 1, largest)));
        assert_eq!(size_class(largest + 1, 8), None);
        assert_eq!(size_class(usize::MAX, 8), None);
        assert_eq!(size_class(24, 16), None);
        for (size, align) in [(1, 1), (24, 8), (330, 8), (24, 16), (largest + 1, 8)] {
            let block = block_size(size, align);
            assert!(block >= size, "{size}/{align}");
            assert_eq!(
                size_class(size, align).is_some(),
                block.is_power_of_two() && block >= 8 && align <= 8,
                "{size}/{align}"
            );
        }
        assert_eq!(block_size(24, 16), 24);
    }

    #[test]
    fn native_safety_harness_covers_shared_scanner_serializer_and_allocation_logic() {
        assert_eq!(growth_pages(0, 1), Some(1));
        assert_eq!(growth_pages(1, 1 << 16), Some(0));
        assert_eq!(growth_pages(1, (1 << 16) + 1), Some(1));
        assert_eq!(growth_pages(65_536, u32::MAX), Some(0));
        if usize::BITS > u32::BITS {
            assert_eq!(growth_pages(usize::MAX, u32::MAX), None);
        }

        assert_eq!(allocation_bounds(1, 7, 8), Some((8, 15)));
        assert_eq!(allocation_bounds(8, 0, 8), Some((8, 8)));
        assert_eq!(allocation_bounds(u32::MAX - 6, 0, 8), None);
        assert_eq!(allocation_bounds(1, 1, 3), None);
        if usize::BITS > u32::BITS {
            assert_eq!(allocation_bounds(8, u32::MAX as usize + 1, 8), None);
        }
        assert_eq!(relocation_delta(8, 16), Some(8));
        assert_eq!(relocation_delta(16, 8), None);
        assert_eq!(relocation_delta(0, i32::MAX as u32), Some(i32::MAX));
        assert_eq!(relocation_delta(0, i32::MAX as u32 + 1), None);

        assert_eq!(checked_linear_range(8, 8, 16), Some((8, 16)));
        assert_eq!(checked_linear_range(0, 0, 16), None);
        assert_eq!(checked_linear_range(8, usize::MAX, u64::MAX), None);
        assert_eq!(
            checked_linear_range(u32::MAX as usize, 1, u64::from(u32::MAX) + 1),
            None,
            "an exclusive end at 2^32 cannot fit the runtime's u32 arena pointer"
        );

        let mut raw = [0u8; 32];
        // SAFETY: offsets 1..=8 and 9..=16 lie inside `raw`; the first range
        // receives a valid f64 bit pattern before it is read, and the two
        // ranges are disjoint for the byte copy.
        unsafe {
            let base = raw.as_mut_ptr();
            write_unaligned(base.add(1), -123.5f64);
            assert_eq!(read_unaligned::<f64>(base.add(1)), -123.5);
            copy_bytes(base.add(1), base.add(9), 8);
            assert_eq!(read_unaligned::<f64>(base.add(9)), -123.5);
        }
        raw[..8].copy_from_slice(b"abcdefgh");
        // SAFETY: both eight-byte ranges lie inside `raw`; overlap is the
        // behavior this production relocation primitive is designed for.
        unsafe {
            let base = raw.as_mut_ptr();
            move_bytes(base, base.add(2), 8);
        }
        assert_eq!(&raw[2..10], b"abcdefgh");

        let valid =
            br#"{"version":1,"params":{},"bars":[{"time":1,"open":1,"high":2,"low":0,"close":1}]}"#;
        assert_eq!(scan_pass(valid, None), Ok(1));
        for end in 0..valid.len() {
            assert!(
                scan_pass(&valid[..end], None).is_err(),
                "prefix {end} accepted"
            );
        }
        for byte in 0u8..=u8::MAX {
            assert!(
                scan_pass(&[byte], None).is_err(),
                "single byte {byte:#04x} accepted"
            );
        }

        let posture = [f64::NAN, 1.0, 1.0, 0.0, -1.0, -1.0, 1.0];
        let price = [10.0, 11.0, 12.0, 13.0, 14.0, 15.0, 16.0];
        let time = [100.0, 101.0, 102.0, 103.0, 104.0, 105.0, 106.0];
        let mut sparse = VecSink::default();
        serialize_samples(
            posture.len() as u32,
            false,
            &mut sparse,
            |bar| posture[bar as usize],
            |bar| price[bar as usize],
            |bar| time[bar as usize],
        )
        .expect("valid sparse posture serialization");
        assert_eq!(
            String::from_utf8(sparse.0).expect("serializer emits UTF-8"),
            "{\"version\":1,\"signals\":[{\"time\":101,\"direction\":1,\"price\":11.0},{\"time\":104,\"direction\":-1,\"price\":14.0},{\"time\":106,\"direction\":1,\"price\":16.0}]}"
        );

        let mut transitions = VecSink::default();
        serialize_samples(
            posture.len() as u32,
            true,
            &mut transitions,
            |bar| posture[bar as usize],
            |bar| price[bar as usize],
            |bar| time[bar as usize],
        )
        .expect("valid transition serialization");
        assert_eq!(
            String::from_utf8(transitions.0).expect("serializer emits UTF-8"),
            "{\"version\":1,\"signals\":[{\"time\":101,\"direction\":1,\"price\":11.0},{\"time\":103,\"direction\":0,\"price\":13.0},{\"time\":104,\"direction\":-1,\"price\":14.0},{\"time\":106,\"direction\":1,\"price\":16.0}]}"
        );

        let mut invalid = VecSink::default();
        assert!(serialize_samples(
            1,
            false,
            &mut invalid,
            |_| 2.0,
            |_| panic!("price must not be loaded after an invalid posture"),
            |_| panic!("time must not be loaded after an invalid posture"),
        )
        .is_err());
        assert!(error_text().contains("non-posture value"));
        let mut invalid = VecSink::default();
        assert!(serialize_samples(
            1,
            false,
            &mut invalid,
            |_| 1.0,
            |_| f64::INFINITY,
            |_| panic!("time must not be loaded after an invalid price"),
        )
        .is_err());
        assert!(error_text().contains("non-finite close"));

        let mut out = Out {
            start: u32::MAX - 3,
            pos: u32::MAX - 3,
        };
        assert!(out.push(&[0; 8]).is_err());
        assert_eq!(
            out.pos,
            u32::MAX - 3,
            "failed writes leave the cursor unchanged"
        );

        let mut out = Out { start: 8, pos: 8 };
        assert!(out.push(b"x").is_err());
        assert_eq!(
            out.pos, 8,
            "refused memory growth must not advance the cursor"
        );
    }
}
