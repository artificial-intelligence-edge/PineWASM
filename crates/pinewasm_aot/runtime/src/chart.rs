//! The `chart` export family: the interpreter as the host of a chart
//! module, linked into the module itself.
//!
//! A chart module compiles its numeric work natively and leaves its other
//! instructions (text, drawings, alerts, orders, full colors) as host sites.
//! An ordinary chart module imports `host.vm` and calls the embedding host
//! for each one. A module built with this family calls
//! [`pinewasm_rt_chart_vm`] instead, a function of its own, so it imports
//! nothing: [`pinewasm_vm::aot_host::VmHost`] runs each site on the
//! interpreter's own handlers, exactly as the embedding host would.
//!
//! The loader calls `prepare`, writes the market data lanes and the inputs it
//! owns, and then calls `chartInit` with the run's request. `chartInit`
//! builds the host and writes the cells of the inputs it owns (strings and
//! colors) into the input table. After `run`, `chartCommand` returns the
//! host's side of the result. An instance runs once.
//!
//! This build links `std`: its allocator obtains memory with `memory.grow`,
//! above whatever `prepare` laid out, and `prepare` is also what points the
//! shadow stack at its full-size region. So the entry points a loader calls
//! (`chartAlloc`, `chartInit`, `chartCommand`) do nothing before `prepare`
//! has run: each tests that first and leaves the rest to a function of its
//! own, so the test itself needs no stack to speak of. The ones the module
//! calls from `run` need no test, because `run` itself refuses until
//! `chartInit` has succeeded.
//!
//! A linked module's errors are its host's. The module stops through
//! [`pinewasm_rt_chart_stop`], which turns a stop that is a final error
//! into the interpreter's own error for it, and it keeps the interpreter's
//! string budget with the host ([`pinewasm_rt_chart_string_room`]), so it
//! stops on that budget where the interpreter does.

use pinewasm_csil::CsilProgram;
use pinewasm_vm::{
    aot_host::{SiteSpec, VmHost},
    AlertEvent, MarketData, RunOptions,
};
use serde::{Deserialize, Serialize};

// Header fields of the module (`pinewasm_aot::header`). The compiler is the
// source of truth; these mirror it because the blob builds outside the workspace.
// `tests/chart_wasm.rs` on the workspace side asserts the two agree.
const OFF_ABI_VERSION: u32 = 0x04;
const OFF_BARS: u32 = 0x08;
const OFF_SLOTS_BASE: u32 = 0x14;
const OFF_ERR_CODE: u32 = 0x20;
const OFF_ERR_BAR: u32 = 0x24;
const OFF_COLORS_BASE: u32 = 0x2C;
const OFF_SCRATCH_BASE: u32 = 0x40;
const OFF_ALERTS_BASE: u32 = 0x58;
const OFF_ALERT_COUNT: u32 = 0x5C;
/// `header::ALERT_ENTRY`: bytes per alert-lane entry, `(bar: u32, message:
/// i32)`.
const ALERT_ENTRY: u32 = 8;
/// `header::SIZE`: the input table starts here, 8 bytes per input.
const INPUT_TABLE_BASE: u32 = 0x60;
/// `header::ABI_VERSION`, which `prepare` writes into the header.
const ABI_VERSION: u32 = 6;
/// `status::ABI_MISMATCH`: the request must run on the interpreter.
const STATUS_ABI_MISMATCH: i32 = 6;

/// One host site of the module's plan (`pinewasm_aot::HostSiteSpec`).
#[derive(Deserialize)]
struct Site {
    pc: u32,
    args: Vec<u8>,
    ret: Option<u8>,
}

/// One fixed presentation cell (`pinewasm_aot::NativeStyleSlot`).
#[derive(Deserialize)]
struct Style {
    slot: u32,
    offset: u32,
    mask: u32,
    tag: u8,
}

/// What the compiler stores in the module for its host: the program the
/// module was compiled from and the plan the two share.
#[derive(Deserialize)]
struct Plan {
    program: CsilProgram,
    sites: Vec<Site>,
    styles: Vec<Style>,
    scratch_cells: u32,
    /// The module compares string handles itself, so the program's constant
    /// pool must hold no text twice.
    canonical_strings: bool,
}

/// What the loader passes to `chartInit`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Request {
    data: MarketData,
    #[serde(default)]
    run_options: RunOptions,
    bars: usize,
}

struct State {
    /// `run` has started ([`pinewasm_rt_chart_begin`]).
    running: bool,
    host: VmHost,
    /// The module has no host site, so it recorded its own alerts
    /// ([`native_alerts`]).
    host_free: bool,
    styles: Vec<Style>,
    /// Color roles of each plot, each visual and each fill, in the order of
    /// the module's color lanes.
    color_roles: [Vec<usize>; 3],
    /// Where `prepare` put the scratch cells, read from the header once and
    /// checked against the module's memory ([`init`]).
    scratch: u32,
    scratch_cells: usize,
    bars: usize,
}

static mut STATE: Option<State> = None;
static mut REPLY: Vec<u8> = Vec::new();
/// Why `chartInit` declined the request, for `chartCommand(9)`.
static mut INIT_ERROR: Option<String> = None;

/// The run's state, once `chartInit` has built it.
fn state() -> Option<&'static mut State> {
    // SAFETY: a chart module has no imports, threads or shared memory, and
    // its exports are synchronous, so nothing can call back into this module
    // while a reference from here is live.
    unsafe { (*core::ptr::addr_of_mut!(STATE)).as_mut() }
}

/// A guard for the shadow stack. `prepare` puts the stack in the megabyte
/// above this runtime's data, growing down, with nothing below it to trap
/// on: an overflow would run into that data, this module's own state
/// included, and whatever the run computed next would be wrong without a
/// sign. So a word at the stack's low end is written when the host is built
/// and read back each time a call into the host returns. A stack that
/// reached it traps there, and the loader reruns the request on the
/// interpreter.
#[cfg(target_arch = "wasm32")]
mod stack_guard {
    extern "C" {
        /// The linker's end of this runtime's data. The stack region starts
        /// at the next multiple of 16 (`chart_reserved_hi` in the compiler).
        static __heap_base: u8;
    }

    const WORD: u64 = u64::from_le_bytes(*b"rt_stack");

    fn floor() -> *mut u64 {
        // Only the address of the linker's symbol is taken.
        let base = core::ptr::addr_of!(__heap_base) as usize;
        ((base + 15) & !15) as *mut u64
    }

    pub fn arm() {
        // SAFETY: the word is the lowest of the stack region `prepare`
        // reserved, 16-aligned, and holds a frame only once the megabyte
        // above it is full.
        unsafe { floor().write(WORD) }
    }

    pub fn check() {
        // SAFETY: as in `arm`.
        if unsafe { floor().read() } != WORD {
            core::arch::wasm32::unreachable()
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod stack_guard {
    pub fn arm() {}
    pub fn check() {}
}

/// Bytes of linear memory, in a `u64` so an end address cannot wrap.
fn memory_bytes() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        (core::arch::wasm32::memory_size::<0>() as u64) << 16
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        0
    }
}

fn in_memory(at: u32, len: u32) -> bool {
    at != 0 && u64::from(at) + u64::from(len) <= memory_bytes()
}

/// A byte range of linear memory the caller named.
fn bytes(at: u32, len: u32) -> Result<&'static [u8], String> {
    if !in_memory(at, len) {
        return Err("a chart request lies outside the module's memory".into());
    }
    // SAFETY: the range is inside linear memory, which no import can shrink
    // or detach, and is only read while this synchronous call runs.
    Ok(unsafe { core::slice::from_raw_parts(at as *const u8, len as usize) })
}

/// A header field. The header is in the module's first page, which every
/// module has; `prepare` fills it ([`prepared`]).
fn header(offset: u32) -> u32 {
    // SAFETY: `offset` is one of the header constants above: nonzero (so
    // the address is not null), and below `INPUT_TABLE_BASE`.
    unsafe { core::ptr::read_unaligned(offset as *const u32) }
}

/// Whether `prepare` has run: it writes the header's version, and the
/// module's memory starts zeroed. The magic at address 0 would say the same,
/// but Rust cannot read through a null pointer.
fn prepared() -> bool {
    header(OFF_ABI_VERSION) == ABI_VERSION
}

/// `value` as JSON, its fields in their declared order: the order the
/// engine's own `AotHost` methods give the same value's properties.
fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    serde_json::to_vec(value).map_err(|error| error.to_string())
}

/// How a failed call's reply reads.
#[derive(Serialize)]
struct Failure<'a> {
    #[serde(rename = "chartError")]
    chart_error: &'a str,
}

/// Publish `result` as the reply of the call in progress and return its
/// packed `(length << 32) | address`. A failure travels as
/// `{"chartError": message}`.
fn reply(result: Result<Vec<u8>, String>) -> u64 {
    let encoded = result.unwrap_or_else(|error| {
        encode(&Failure {
            chart_error: &error,
        })
        .unwrap_or_else(|_| b"{\"chartError\":\"reply\"}".to_vec())
    });
    let address = encoded.as_ptr() as u32;
    let length = encoded.len() as u32;
    // SAFETY: entry points are synchronous and single-threaded. The previous
    // reply is dropped here, so a caller copies a reply before its next call.
    unsafe {
        *core::ptr::addr_of_mut!(REPLY) = encoded;
    }
    (u64::from(length) << 32) | u64::from(address)
}

/// The slab [`pinewasm_rt_chart_alloc`] cuts its blocks from, and the size
/// of the next one.
struct Slab {
    next: u32,
    end: u32,
    grow: u32,
}

/// The first slab. Each later one is twice the last, up to [`SLAB_MAX`], so
/// a run asks the allocator (and through it `memory.grow`) for memory a
/// handful of times rather than once per page. An engine that tracks a
/// module's memory as external allocation collects its own heap on those
/// growths, which cost a chart run more than its host calls.
const SLAB_FIRST: u32 = 1 << 20;
const SLAB_MAX: u32 = 32 << 20;

static mut SLAB: Slab = Slab {
    next: 0,
    end: 0,
    grow: SLAB_FIRST,
};

/// A block of `size` bytes at alignment 8 from the global allocator, or 0.
fn system_block(size: u32) -> u32 {
    match std::alloc::Layout::from_size_align(size as usize, 8) {
        // SAFETY: the layout has a nonzero size (callers pass at least 8).
        Ok(layout) => unsafe { std::alloc::alloc(layout) as u32 },
        Err(_) => 0,
    }
}

/// `chartAlloc(len) -> address`: an 8-aligned block of at least `len` bytes,
/// or 0. The module's native heap takes its blocks here too, so every
/// allocation of a run comes from one allocator. Blocks are never freed,
/// which is what lets them be cut from slabs.
#[no_mangle]
pub extern "C" fn pinewasm_rt_chart_alloc(len: u32) -> u32 {
    if !prepared() {
        return 0;
    }
    alloc_prepared(len)
}

#[inline(never)]
fn alloc_prepared(len: u32) -> u32 {
    let Some(len) = len.max(1).checked_add(7).map(|len| len & !7) else {
        return 0;
    };
    // SAFETY: entry points are synchronous and single-threaded, and this
    // function holds the only reference to `SLAB` while it runs.
    let slab = unsafe { &mut *core::ptr::addr_of_mut!(SLAB) };
    if slab.end - slab.next < len {
        // The rest of the current slab is abandoned: at most one block's
        // size short of a slab that has already doubled past it.
        let size = len.max(slab.grow);
        let mut block = system_block(size);
        let mut end = block.wrapping_add(size);
        if block == 0 && size > len {
            // Near the module's memory limit a whole slab may not fit where
            // the block alone does.
            block = system_block(len);
            end = block.wrapping_add(len);
        }
        if block == 0 {
            return 0;
        }
        slab.next = block;
        slab.end = end;
        slab.grow = slab.grow.saturating_mul(2).min(SLAB_MAX);
    }
    let block = slab.next;
    slab.next += len;
    block
}

/// Build the run's host from the module's plan and the loader's request, and
/// write the cells of the inputs the host owns.
fn init(plan: &[u8], request: &[u8]) -> Result<State, String> {
    if state().is_some() {
        return Err("a chart instance runs once".into());
    }
    let plan: Plan = serde_json::from_slice(plan).map_err(|error| error.to_string())?;
    let request: Request = serde_json::from_slice(request).map_err(|error| error.to_string())?;
    // The scratch cells the host exchanges operands and results through:
    // where the header says, once, and inside the module's memory.
    let scratch = header(OFF_SCRATCH_BASE);
    let scratch_fits = plan
        .scratch_cells
        .checked_mul(8)
        .is_some_and(|len| scratch % 8 == 0 && in_memory(scratch, len));
    if request.bars != header(OFF_BARS) as usize
        || request.bars > request.data.close.len()
        || plan.scratch_cells < 4
        || !scratch_fits
    {
        return Err("the chart request differs from the prepared layout".into());
    }
    let host_free = plan.sites.is_empty();
    let roles = |counts: &mut dyn Iterator<Item = usize>| counts.collect::<Vec<_>>();
    let color_roles = [
        roles(&mut plan.program.plots.iter().map(|decl| decl.color_roles.len())),
        roles(
            &mut plan
                .program
                .visuals
                .iter()
                .map(|decl| decl.color_roles.len()),
        ),
        roles(&mut plan.program.fills.iter().map(|decl| decl.color_roles.len())),
    ];
    let inputs = plan.program.inputs.len() as u32;
    let sites = plan
        .sites
        .into_iter()
        .map(|site| SiteSpec {
            pc: site.pc,
            args: site.args,
            ret: site.ret,
        })
        .collect();
    let mut host = VmHost::new(
        plan.program,
        request.data,
        request.run_options,
        request.bars,
        sites,
    )
    .linked();
    if let Some(why) = host.strategy_refusal() {
        return Err(why.into());
    }
    if plan.canonical_strings && !host.strings_are_distinct() {
        return Err("the program's string constants are not distinct".into());
    }
    for input in 0..inputs {
        if let Some(cell) = host.input_cell(input)? {
            // SAFETY: the compiler bounds a module's inputs so its input
            // table ends below the runtime's data, inside the first page.
            unsafe {
                core::ptr::write_unaligned((INPUT_TABLE_BASE + 8 * input) as *mut i64, cell);
            }
        }
    }
    Ok(State {
        running: false,
        host,
        host_free,
        styles: plan.styles,
        color_roles,
        scratch,
        scratch_cells: plan.scratch_cells as usize,
        bars: request.bars,
    })
}

/// `chartInit`'s target: the module passes its own plan's address and
/// length, then the loader's request. 0 on success; otherwise the status
/// that sends the request to the interpreter, and `chartCommand(9)`
/// returns the reason.
#[no_mangle]
pub extern "C" fn pinewasm_rt_chart_init(
    plan_ptr: u32,
    plan_len: u32,
    request_ptr: u32,
    request_len: u32,
) -> i32 {
    if !prepared() {
        return STATUS_ABI_MISMATCH;
    }
    init_prepared(plan_ptr, plan_len, request_ptr, request_len)
}

#[inline(never)]
fn init_prepared(plan_ptr: u32, plan_len: u32, request_ptr: u32, request_len: u32) -> i32 {
    stack_guard::arm();
    std::panic::set_hook(Box::new(|info| {
        // SAFETY: a chart instance is synchronous and single-threaded. Preserve
        // the panic text for the loader after the Wasm trap, through command 9.
        unsafe {
            *core::ptr::addr_of_mut!(INIT_ERROR) = Some(info.to_string());
        }
    }));
    let built = bytes(plan_ptr, plan_len)
        .and_then(|plan| bytes(request_ptr, request_len).map(|request| (plan, request)))
        .and_then(|(plan, request)| init(plan, request));
    stack_guard::check();
    match built {
        Ok(built) => {
            // SAFETY: synchronous, and `init` refused a second call.
            unsafe {
                *core::ptr::addr_of_mut!(STATE) = Some(built);
            }
            0
        }
        Err(error) => {
            // SAFETY: synchronous and single-threaded, as above.
            unsafe {
                *core::ptr::addr_of_mut!(INIT_ERROR) = Some(error);
            }
            STATUS_ABI_MISMATCH
        }
    }
}

/// The module's `run` calls this first and runs only when it returns 1,
/// which it does once: after `chartInit` succeeded, for the first `run`.
/// Without that, a module that happens to have no host site would run on
/// input cells no host wrote, or a second time over the first run's state.
/// 0 says no host was built, so the header should say the run was refused;
/// 2 says the instance already ran, and its header is that run's.
#[no_mangle]
pub extern "C" fn pinewasm_rt_chart_begin() -> i32 {
    if !prepared() {
        return 0;
    }
    match state() {
        Some(state) if !state.running => {
            state.running = true;
            1
        }
        Some(_) => 2,
        None => 0,
    }
}

/// The scratch cells the module and its host exchange operands, results and
/// budgets through.
fn scratch_cells(state: &State) -> &'static mut [i64] {
    // SAFETY: `init` found `scratch_cells` aligned 8-byte cells at this
    // address inside the module's memory, which never shrinks. Each entry
    // point takes the cells once, for the one synchronous call it makes, and
    // the host does not reach them any other way while it runs.
    unsafe { core::slice::from_raw_parts_mut(state.scratch as *mut i64, state.scratch_cells) }
}

/// A module's host site: what `host.vm(site, bar)` is for an ordinary chart
/// module. Operands, result and budget travel through the scratch cells.
#[no_mangle]
pub extern "C" fn pinewasm_rt_chart_vm(site: u32, bar: u32) -> i32 {
    let Some(state) = state() else {
        return STATUS_ABI_MISMATCH;
    };
    let status = state.host.vm_call(site, bar, scratch_cells(state));
    stack_guard::check();
    status
}

/// A host site of a module that keeps the string budget
/// ([`pinewasm_rt_chart_string_room`]): [`pinewasm_rt_chart_vm`] with the
/// module's count of what is left going in. Returns what is left after the
/// call, or the call's nonzero status negated.
#[no_mangle]
pub extern "C" fn pinewasm_rt_chart_vm_strings(site: u32, bar: u32, room: i64) -> i64 {
    let Some(state) = state() else {
        return -i64::from(STATUS_ABI_MISMATCH);
    };
    let left = state
        .host
        .vm_call_metered(site, bar, scratch_cells(state), room);
    stack_guard::check();
    left
}

/// What is left of the run's string budget. The interpreter reserves string
/// bytes at instructions a module runs itself, so a module with such an
/// instruction starts its run from this number and takes its own
/// reservations from it.
#[no_mangle]
pub extern "C" fn pinewasm_rt_chart_string_room() -> i64 {
    state().map_or(0, |state| state.host.string_room())
}

/// The status to stop with when `bytes` of the string budget, which the
/// instruction at `pc` reserves on bar `bar`, do not fit the module's
/// `room`. The interpreter's own reservation then fails: status 5, and
/// `chartCommand(0)` returns its error. A count that is not the host's is
/// status 6. Never 0.
#[no_mangle]
pub extern "C" fn pinewasm_rt_chart_string_reserve(
    pc: u32,
    bar: u32,
    room: i64,
    bytes: i64,
) -> i32 {
    let Some(state) = state() else {
        return STATUS_ABI_MISMATCH;
    };
    let status = state.host.string_reserve(pc, bar, room, bytes);
    stack_guard::check();
    status
}

/// The length in bytes of the run string a cell names: 0 for `na`, -1 for a
/// cell that names none.
#[no_mangle]
pub extern "C" fn pinewasm_rt_chart_string_len(handle: i32) -> i64 {
    state().map_or(-1, |state| state.host.string_len(handle))
}

/// The status to stop with after the module's own code stopped the run with
/// `code` at instruction `pc` on bar `bar`: 5 with the interpreter's error
/// for a stop that is a final error, 6 for one whose error only the
/// interpreter can produce, and `code` itself otherwise.
#[no_mangle]
pub extern "C" fn pinewasm_rt_chart_stop(code: i32, pc: u32, bar: u32) -> i32 {
    let Some(state) = state() else {
        return code;
    };
    let status = state.host.native_stop(code, pc, bar, scratch_cells(state));
    stack_guard::check();
    status
}

/// The fixed presentation cells at the last of `visible` bars, resolved as
/// the interpreter resolves a plot's style.
fn display_styles(state: &State, visible: u32) -> Result<Vec<u8>, String> {
    let base = header(OFF_SLOTS_BASE);
    let mut slots = Vec::with_capacity(state.styles.len());
    let mut tags = Vec::with_capacity(state.styles.len());
    let mut cells = Vec::with_capacity(state.styles.len());
    for style in &state.styles {
        let at = u64::from(base)
            + u64::from(style.offset)
            + u64::from(visible.saturating_sub(1) & style.mask) * 8;
        let at = u32::try_from(at)
            .ok()
            .filter(|&at| in_memory(at, 8))
            .ok_or("a style cell lies outside the module's memory")?;
        // SAFETY: the cell is inside linear memory. A cell of the four
        // 32-bit kinds holds its value in the low half, sign-extended.
        let cell = unsafe {
            if style.tag >= 2 {
                i64::from(core::ptr::read_unaligned(at as *const i32))
            } else {
                core::ptr::read_unaligned(at as *const i64)
            }
        };
        slots.push(style.slot);
        tags.push(style.tag);
        cells.push(cell);
    }
    encode(
        &state
            .host
            .resolve_display_styles(&slots, &tags, &cells, visible as usize)?,
    )
}

/// One color lane of the run (`family`: 0 plots, 1 visuals, 2 fills), as
/// the interpreter writes a plot's color series.
///
/// A lane holds few distinct colors, and a bar's entry in the series
/// depends only on its own cell. The reply therefore carries the series of
/// the lane's distinct cells, in the order they first appear, and `index`,
/// which names one of them for each bar; the loader expands it.
fn color_series(state: &mut State, family: u32, slot: u32, role: u32) -> Result<Vec<u8>, String> {
    let roles = state
        .color_roles
        .get(family as usize)
        .ok_or("invalid color family")?;
    let count = *roles.get(slot as usize).ok_or("invalid color slot")?;
    if role as usize >= count {
        return Err("invalid color role".into());
    }
    // Lanes are laid out family by family, slot by slot, role by role.
    let preceding = state.color_roles[..family as usize]
        .iter()
        .flatten()
        .sum::<usize>()
        + roles[..slot as usize].iter().sum::<usize>()
        + role as usize;
    // A run that stopped early wrote its lanes up to the failed bar.
    let visible = if header(OFF_ERR_CODE) == 0 {
        state.bars
    } else {
        (header(OFF_ERR_BAR) as usize).min(state.bars)
    };
    if family == 2 {
        if let Some(series) = state.host.resolve_hosted_fill_series(slot, role, visible)? {
            return encode(&IndexedSeries {
                series,
                index: (0..visible as u32).collect(),
            });
        }
    }
    let at = u64::from(header(OFF_COLORS_BASE)) + (preceding as u64) * 4 * (state.bars as u64);
    let len = (visible as u64) * 4;
    let (Ok(at), Ok(len)) = (u32::try_from(at), u32::try_from(len)) else {
        return Err("a color lane lies outside the module's memory".into());
    };
    if visible > 0 && !in_memory(at, len) {
        return Err("a color lane lies outside the module's memory".into());
    }
    let cells: &[i32] = if visible == 0 {
        &[]
    } else {
        // SAFETY: the lane is inside linear memory and 4-aligned (the
        // region's base is 8-aligned and lanes are `4 * bars` apart);
        // nothing writes it during this call.
        unsafe { core::slice::from_raw_parts(at as *const i32, visible) }
    };
    let mut distinct = Vec::new();
    let mut seen = std::collections::HashMap::new();
    let index: Vec<u32> = cells
        .iter()
        .map(|&cell| {
            *seen.entry(cell).or_insert_with(|| {
                distinct.push(cell);
                (distinct.len() - 1) as u32
            })
        })
        .collect();
    let series = state
        .host
        .resolve_color_series(family, slot, role, &distinct)?;
    encode(&IndexedSeries { series, index })
}

/// A color lane as [`color_series`] replies with it.
#[derive(Serialize)]
struct IndexedSeries {
    series: pinewasm_vm::PlotColorSeries,
    index: Vec<u32>,
}

/// The alerts a module recorded itself. A module with no host site writes
/// each `alertcondition` that fires to its alert lane, `(bar, message)` pairs
/// in execution order, and runs none of them on the host. A module with host
/// sites sends every alert through the host instead, to keep one ordered
/// record of its effects, and leaves the lane empty.
fn native_alerts(state: &State) -> Result<Vec<AlertEvent>, String> {
    let count = header(OFF_ALERT_COUNT);
    if count == 0 {
        return Ok(Vec::new());
    }
    let base = header(OFF_ALERTS_BASE);
    let outside = "the alert lane lies outside the module's memory";
    let len = count.checked_mul(ALERT_ENTRY).ok_or(outside)?;
    let lane = bytes(base, len).map_err(|_| outside)?;
    Ok(lane
        .chunks_exact(ALERT_ENTRY as usize)
        .map(|entry| {
            let word = |at: usize| [entry[at], entry[at + 1], entry[at + 2], entry[at + 3]];
            // A negative id is `na`: the interpreter's empty message.
            let message = i32::from_le_bytes(word(4));
            AlertEvent {
                bar: u32::from_le_bytes(word(0)),
                message: state
                    .host
                    .string(i64::from(message))
                    .unwrap_or_default()
                    .to_string(),
                freq: "alert.freq_once_per_bar_close".to_string(),
            }
        })
        .collect())
}

/// Commands of [`pinewasm_rt_chart_command`].
mod command {
    pub const TAKE_ERROR: u32 = 0;
    pub const ABI_MISMATCH: u32 = 1;
    pub const DRAIN_LOGS: u32 = 2;
    pub const DRAIN_ALERTS: u32 = 3;
    pub const DRAIN_STRATEGY: u32 = 4;
    pub const DRAIN_DRAWINGS: u32 = 5;
    pub const DISPLAY_STYLES: u32 = 6;
    pub const COLOR_SERIES: u32 = 7;
    pub const STRATEGY_REFUSAL: u32 = 8;
    /// Why `chartInit` declined; the one command valid without a host.
    pub const INIT_ERROR: u32 = 9;
}

/// `chartCommand(op, a, b, c)`: the host's side of the run's result, as the
/// JSON the engine's own `AotHost` methods return. The reply is the packed
/// `(length << 32) | address` of that JSON, valid until the next call; 0
/// before `prepare`.
#[no_mangle]
pub extern "C" fn pinewasm_rt_chart_command(op: u32, a: u32, b: u32, c: u32) -> u64 {
    if !prepared() {
        return 0;
    }
    command_prepared(op, a, b, c)
}

#[inline(never)]
fn command_prepared(op: u32, a: u32, b: u32, c: u32) -> u64 {
    let run = || -> Result<Vec<u8>, String> {
        if op == command::INIT_ERROR {
            // SAFETY: synchronous and single-threaded; read-only here.
            return encode(&unsafe { (*core::ptr::addr_of!(INIT_ERROR)).as_deref() });
        }
        let state = state().ok_or("the chart runtime is not initialized")?;
        match op {
            command::TAKE_ERROR => encode(&state.host.take_error()),
            command::ABI_MISMATCH => encode(&state.host.abi_mismatch()),
            command::DRAIN_LOGS => encode(&state.host.drain_logs()),
            // A module without host sites recorded its own alerts; one with
            // host sites sent them all through the host.
            command::DRAIN_ALERTS if state.host_free => encode(&native_alerts(state)?),
            command::DRAIN_ALERTS => encode(&state.host.drain_alerts()),
            command::DRAIN_STRATEGY => encode(&state.host.drain_strategy(a as i32, b)?),
            command::DRAIN_DRAWINGS => encode(&state.host.drain_drawings()),
            command::DISPLAY_STYLES => display_styles(state, a),
            command::COLOR_SERIES => color_series(state, a, b, c),
            command::STRATEGY_REFUSAL => encode(&state.host.strategy_refusal()),
            _ => Err("unknown chart command".into()),
        }
    };
    let result = run();
    stack_guard::check();
    reply(result)
}
