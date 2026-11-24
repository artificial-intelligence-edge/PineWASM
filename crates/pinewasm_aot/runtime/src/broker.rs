//! The `pinewasm_rt_broker_*` export family: the strategy broker
//! ([`pinewasm_broker`]) driven from emitted strategy artifacts.
//!
//! The broker is the same crate the interpreter runs, which is the point of
//! linking it (see `pinewasm_broker`'s crate docs).
//! This module is only the thin linear-memory binding, the same shape as
//! [`crate::ta`]: a `static` context, raw lane reads keyed off the CSAW
//! header, and one `extern "C"` entry per operation the emitted code needs.
//!
//! # Per-bar protocol (follows the interpreter's sequencing)
//!
//! ```text
//! run() prologue:   pinewasm_rt_broker_init(cfg, cfg_len)      // fresh per run
//! per bar:          pinewasm_rt_broker_begin_bar(bar)          // currency sync,
//!                                                     // session, pre-script
//!                                                     // fill pass over OHLC
//!                   /* script body — pinewasm_rt_broker_entry / _order / _exit /
//!                      _close / _close_all / _cancel / _cancel_all /
//!                      _risk_* place orders; pinewasm_rt_broker_snap_f64 / _i64
//!                      back the strategy.* read surface */
//!                   pinewasm_rt_broker_finish_bar(bar)         // close-tick pass +
//!                                                     // equity + posture
//! serialize:        standalone_wasm_serialize_posture(slice_end)   // flat-aware events
//! ```
//!
//! # Output documents
//!
//! [`standalone_wasm_serialize_posture`] writes one of four documents, chosen
//! by the request: the signals-only posture events (the default, and the
//! only one a host that asks for none of the others reads), full results (`include_results`, with
//! `binary_results` as an alternative transport), the compact
//! `retained-objectives-v1` projection (`objective_results`), or the compact
//! `broker-trades-v1` trade ledger (`trade_results`). The two compact
//! projections read the completed broker in place: neither constructs the
//! outcome, the plot or equity lanes, or the report aggregates, and neither
//! carries the posture events. Those grow with the bars evaluated; the trade
//! ledger's rows stay inside the broker's retention window.
//!
//! # Order placement mirrors `CsilInstr::Order`
//!
//! Every placement export takes `(args, site)`: `args` points at an operand
//! region of 8-byte cells, one per operand slot in the kind's fixed layout
//! (`pinewasm_hir::OrderKind::param_names`), written by the emitted body
//! in slot order. Cell encodings by slot type:
//!
//! | slot type | cell | na |
//! |---|---|---|
//! | number (qty, prices, `value`, `contracts`) | f64 | NaN |
//! | int (`count`) | i64 | `i64::MIN` |
//! | string (ids, comments, tokens) | i64 const-pool handle | < 0 |
//! | bool (`immediately`, `disable_alert`) | i64 bool3 (0/1/2) | 1 (= false) |
//!
//! `site` is the CSIL pc, used by the `risk.*` directives for per-call-site
//! rule identity (the same value the interpreter passes).
//!
//! The operand decode below mirrors `pinewasm_vm`'s `place_order`
//! decoders case for case: a missing/na operand is `None`, empty strings are
//! filtered exactly where the VM filters them, and nonsensical orders are
//! dropped rather than failing the run. The only error paths are the ones
//! the interpreter turns into runtime errors (invalid risk tokens and
//! thresholds) and the [`pinewasm_broker::BrokerLimits`] caps — both
//! surface through the standalone WebAssembly error slot as the `"Indicator failure: "` class.
//!
//! # Memory
//!
//! The broker allocates through the crate's global allocator, which the
//! `broker` build backs with the standalone WebAssembly arena band (see `broker_alloc` in
//! `lib.rs`). Order-fill alerts are drained every bar so nothing accumulates
//! across the run; the posture lane is `O(bars)`; the trades deque is pruned
//! by the broker's own retention window of 9,000 successful orders. Across
//! runs, [`pinewasm_rt_broker_init`] forgets the previous evaluation's broker and
//! posture lane and rewinds the whole band to its floor — the band holds
//! nothing live at that point (the glue dropped the input JSON before
//! `run()`), and this wholesale reclaim is what keeps a resident strategy
//! instance's memory flat over thousands of evaluations.
//!
//! # Config block layout (defined here; emitted by `pinewasm_aot::emit`
//! as part of the per-call `memory.init` tables segment)
//!
//! All little-endian. Offsets from the block start:
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0  | 4 | version (this layout: 1) |
//! | 4  | 4 | flags — bit 0 `process_orders_on_close`, bit 1 `calc_on_every_tick`; upper 24 bits required quote currency |
//! | 8  | 8 | `initial_capital` f64 |
//! | 16 | 8 | `default_qty_value` f64 |
//! | 24 | 8 | `commission_value` f64 |
//! | 32 | 8 | `margin_long` f64 |
//! | 40 | 8 | `margin_short` f64 |
//! | 48 | 4 | `slippage` u32 |
//! | 52 | 4 | `pyramiding` u32 |
//! | 56 | 4 | `backtest_fill_limits_assumption` u32 |
//! | 60 | 1 | `default_qty_type` — 0 fixed / 1 cash / 2 percent_of_equity |
//! | 61 | 1 | `commission_type` — 0 percent / 1 cash_per_contract / 2 cash_per_order |
//! | 62 | 1 | `close_entries_rule` — 0 FIFO / 1 ANY |
//! | 63 | 1 | padding |
//! | 64 | 4 | `n_strings` — the program's const pool size |
//! | 68 | 8·n | per string: u32 offset (from block start), u32 byte length |
//! | …  |   | the UTF-8 string bytes |
//!
//! `calc_on_order_fills` is a structured refusal. Explicit account currency requires
//! a matching request quote currency (missing/mismatched quotes fail closed); `calc_on_every_tick`
//! is carried but inert (standalone WebAssembly evaluation has no realtime ticks).
//!
//! # Symbol economics and the session key
//!
//! Native strategy requests may supply positive finite `mintick`, `pointvalue`,
//! and `mincontract`. Omitted values keep the default economics: point value
//! and quantity increment 1.0, with the mintick written by `prepare`. The request may supply its quote `currency`; the broker uses identity conversion
//! only after the explicit account currency guard passes. Symbol
//! currency stays empty internally, so the compact trade ledger reports the
//! request's quote currency and the declaration's account currency itself.
//! Timeframe is `"1D"`, and timezone is `"UTC"`. The
//! risk-session key remains UTC calendar midnight of the bar's open time.
//! Differential tests pass identical symbol economics to both runtimes.

use crate::standalone_wasm;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::ptr;
use pinewasm_broker::{
    Broker, BrokerError, BrokerLimits, ExitMetadata, OcaType, OrderMetadata, RiskUnit,
    StrategyProperties,
};

/// CSAW header mirror (see the note on [`standalone_wasm`]'s copies).
const OFF_MINTICK: u32 = 0x38;

/// Builtin lane order (`pinewasm_aot::BUILTIN_LANES`).
const LANE_OPEN: u32 = 0;
const LANE_HIGH: u32 = 1;
const LANE_LOW: u32 = 2;

/// Generous caps on the broker's growable state, so a pathological script
/// surfaces a named `"Indicator failure: "` instead of exhausting the arena.
/// Normal strategies use far fewer pending orders; these limits are safety
/// caps intended to be checked, not approached.
const MAX_PENDING_ORDERS: usize = 10_000;
const MAX_OPEN_LOTS: usize = 10_000;
const MAX_EXIT_RECIPES: usize = 10_000;

/// Heap bytes a bar's strategy snapshot can own beyond its lane cell (its
/// account-currency and entry-name strings) plus its posture cell.
const BAR_HEAP_SLACK: u32 = 64;

/// The parsed config block: a plain-data [`StrategyProperties`] over the
/// emitter's region, so [`Broker::new_with_symbol`] reads the same surface
/// the interpreter reads from `pinewasm_hir::StrategyInfo`.
struct CfgView {
    initial_capital: f64,
    default_qty_value: f64,
    commission_value: f64,
    margin_long: f64,
    margin_short: f64,
    slippage: u32,
    pyramiding: u32,
    backtest_fill_limits_assumption: u32,
    process_orders_on_close: bool,
    calc_on_every_tick: bool,
    qty_type: u8,
    commission_type: u8,
    close_entries_rule: u8,
}

impl StrategyProperties for CfgView {
    fn initial_capital(&self) -> f64 {
        self.initial_capital
    }
    fn default_qty_type(&self) -> &str {
        match self.qty_type {
            1 => "cash",
            2 => "percent_of_equity",
            _ => "fixed",
        }
    }
    fn default_qty_value(&self) -> f64 {
        self.default_qty_value
    }
    fn commission_type(&self) -> &str {
        match self.commission_type {
            1 => "cash_per_contract",
            2 => "cash_per_order",
            _ => "percent",
        }
    }
    fn commission_value(&self) -> f64 {
        self.commission_value
    }
    fn slippage(&self) -> u32 {
        self.slippage
    }
    fn pyramiding(&self) -> u32 {
        self.pyramiding
    }
    fn process_orders_on_close(&self) -> bool {
        self.process_orders_on_close
    }
    fn calc_on_every_tick(&self) -> bool {
        self.calc_on_every_tick
    }
    fn calc_on_order_fills(&self) -> bool {
        // A structured AOT refusal; never reaches an emitted artifact.
        false
    }
    fn calc_on_every_history_tick(&self) -> bool {
        // A host run override; the standalone WebAssembly tier has no host override surface.
        false
    }
    fn backtest_fill_limits_assumption(&self) -> u32 {
        self.backtest_fill_limits_assumption
    }
    fn close_entries_rule(&self) -> &str {
        if self.close_entries_rule == 1 {
            "ANY"
        } else {
            "FIFO"
        }
    }
    fn margin_long(&self) -> f64 {
        self.margin_long
    }
    fn margin_short(&self) -> f64 {
        self.margin_short
    }
    fn currency(&self) -> &str {
        // The explicit-account-currency guard runs before broker construction.
        // Matching quotes need no FX lane; the internal empty currency keeps
        // both sides on identity conversion, rate 1.0.
        "NONE"
    }
}

/// The active run's broker context. Single-threaded by construction, like
/// [`standalone_wasm`]'s `STATE` and [`crate::ta`]'s `CTX`.
struct Ctx {
    broker: Option<Broker>,
    /// Bar-close posture lane: `sign(position_size)` per processed bar, in
    /// bar order. [`standalone_wasm_serialize_posture`] edge-detects it.
    postures: Vec<i8>,
    /// Const-pool: `n` `(offset, len)` pairs at `pool_table`, offsets
    /// relative to `pool_base` (the config block start).
    pool_base: u32,
    pool_table: u32,
    pool_n: u32,
    pool_end: u32,
    /// The bar [`pinewasm_rt_broker_begin_bar`] last opened — the execution bar for
    /// `immediately` closes.
    current_bar: u32,
}

static mut CTX: Ctx = Ctx {
    broker: None,
    postures: Vec::new(),
    pool_base: 0,
    pool_table: 0,
    pool_n: 0,
    pool_end: 0,
    current_bar: 0,
};

/// Borrow the broker runtime's singleton context.
///
/// # Safety
///
/// The caller must ensure that no other reference derived from [`CTX`] is
/// live for the duration of the returned borrow. The borrow must remain local
/// to one synchronous runtime operation and must not cross a call that can
/// access [`CTX`]. The runtime blob's no-threads, no-callback execution model
/// lets its internal call sites uphold those requirements.
#[allow(clippy::mut_from_ref)]
unsafe fn ctx() -> &'static mut Ctx {
    // SAFETY: the caller owns the uniqueness obligation documented above;
    // `addr_of_mut!` avoids first creating a reference to the mutable static.
    unsafe { &mut *ptr::addr_of_mut!(CTX) }
}

fn load_u32(addr: u32) -> u32 {
    standalone_wasm::load_u32(addr)
}

fn load_f64(addr: u32) -> f64 {
    standalone_wasm::load_f64(addr)
}

fn load_i64(addr: u32) -> i64 {
    // SAFETY: callers pass an address of an initialized eight-byte operand
    // cell or header field inside the compiler-sized linear-memory layout.
    unsafe { ptr::read_unaligned(addr as *const i64) }
}

fn load_u8(addr: u32) -> u8 {
    // SAFETY: callers pass an address inside the initialized compiler-emitted
    // broker configuration block, after its enclosing bounds were checked.
    unsafe { ptr::read_unaligned(addr as *const u8) }
}

/// One builtin-lane cell, bounds-degraded to NaN like [`crate::ta`]'s
/// `ArtifactLanes`.
fn lane(index: u32, bar: u32) -> f64 {
    let bars = load_u32(standalone_wasm::OFF_BARS);
    if bar >= bars {
        return f64::NAN;
    }
    load_f64(load_u32(standalone_wasm::OFF_BUILTIN_BASE) + (index * bars + bar) * 8)
}

/// The interpreter's no-lane risk-session key under default market data:
/// UTC calendar midnight of the bar's open time, in epoch ms
/// (`MarketData::fallback_trading_day_for_time` with the "UTC" offset 0).
fn session_key(time_ms: i64) -> i64 {
    let local_seconds = time_ms.div_euclid(1000);
    let day_start = local_seconds.div_euclid(86_400) * 86_400;
    day_start.saturating_mul(1000)
}

/// The bar's open time from the time lane (an i64 stored as an exact f64).
fn bar_time_ms(bar: u32) -> i64 {
    let t = lane(standalone_wasm::LANE_TIME, bar);
    if t.is_nan() {
        0
    } else {
        t as i64
    }
}

// ---------------------------------------------------------------------------
// Errors. Broker conditions are the expected-failure class: the script (or
// its declaration) did something the broker refuses, exactly the class the
// interpreter's runtime errors map to.

fn set_broker_err(detail: &[&[u8]]) {
    let mut parts: Vec<&[u8]> = Vec::with_capacity(detail.len() + 1);
    parts.push(b"Indicator failure: ");
    parts.extend_from_slice(detail);
    standalone_wasm::set_err(&parts);
}

fn set_cap_err(error: BrokerError) {
    let BrokerError::CapExceeded { cap, limit, actual } = error;
    let mut limit_buf = itoa::Buffer::new();
    let mut actual_buf = itoa::Buffer::new();
    set_broker_err(&[
        b"broker cap exceeded: ",
        cap.as_bytes(),
        b" (",
        actual_buf.format(actual).as_bytes(),
        b" > ",
        limit_buf.format(limit).as_bytes(),
        b")",
    ]);
}

// ---------------------------------------------------------------------------
// Operand cells (see the module docs for the encoding table).

fn cell_addr(args: u32, slot: u32) -> u32 {
    args + slot * 8
}

fn cell_f64(args: u32, slot: u32) -> Option<f64> {
    // Mirrors the VM's `as_float`: only finite numbers count.
    let v = load_f64(cell_addr(args, slot));
    v.is_finite().then_some(v)
}

fn cell_i64(args: u32, slot: u32) -> Option<i64> {
    let v = load_i64(cell_addr(args, slot));
    (v != i64::MIN).then_some(v)
}

fn cell_bool(args: u32, slot: u32) -> bool {
    // bool3: 0 false, 1 na, 2 true. The VM's `as_bool` is true-only.
    load_i64(cell_addr(args, slot)) == 2
}

/// Resolve a string cell against the const pool. `None` for na (< 0), an
/// out-of-range id, or non-UTF-8 bytes (the emitter writes only valid ids;
/// the guards keep a corrupted artifact at "order dropped", never UB).
fn cell_str(args: u32, slot: u32) -> Option<&'static str> {
    let handle = load_i64(cell_addr(args, slot));
    if handle < 0 {
        return None;
    }
    pool_str(handle as u32)
}

fn pool_str(id: u32) -> Option<&'static str> {
    // SAFETY: string lookup is synchronous and retains no other `CTX`
    // reference; the context borrow ends before the returned pool view forms.
    let c = unsafe { ctx() };
    if id >= c.pool_n {
        return None;
    }
    let entry = c.pool_table + id * 8;
    let offset = load_u32(entry);
    let len = load_u32(entry + 4);
    let start = c.pool_base.checked_add(offset)?;
    let end = start.checked_add(len)?;
    if end > c.pool_end {
        return None;
    }
    // SAFETY: `start + len` was checked for overflow and against `pool_end`;
    // the compiler-emitted configuration block remains immutable and live for
    // the synchronous artifact evaluation.
    let bytes = unsafe { core::slice::from_raw_parts(start as *const u8, len as usize) };
    core::str::from_utf8(bytes).ok()
}

/// The VM's `as_string`: na → `None`, a string stays (empty included).
fn as_string(args: u32, slot: u32) -> Option<String> {
    cell_str(args, slot).map(ToString::to_string)
}

/// The VM's `optional_string`: additionally filters the empty string.
fn optional_string(args: u32, slot: u32) -> Option<String> {
    as_string(args, slot).filter(|value| !value.is_empty())
}

/// The VM's `strategy_direction`, string half. (The numeric fallback
/// is unreachable here: the operand is typed `string` by the AOT analysis.)
fn direction_sign(args: u32, slot: u32) -> Option<f64> {
    match cell_str(args, slot) {
        Some("strategy.long") => Some(1.0),
        Some("strategy.short") => Some(-1.0),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Lifecycle.

/// Construct a fresh broker over the config block (fresh per `run()`, which
/// is what keeps a resident standalone WebAssembly instance stateless). Returns 0, or 1
/// with the error slot set for a malformed block (a compiler defect, not a
/// script error, hence unprefixed / infrastructure class).
#[no_mangle]
pub extern "C" fn pinewasm_rt_broker_init(cfg: u32, cfg_len: u32) -> i32 {
    // SAFETY: initialization is synchronous and holds the sole `CTX` borrow;
    // none of the calls below can re-enter this runtime module.
    let c = unsafe { ctx() };
    // Reclaim the previous evaluation's arena state wholesale, FIRST. The
    // stale broker and posture lane (if any) live in the dead band below
    // `arena_ptr`; the emitted sequence has already consumed and dropped the
    // input JSON before `run()` (emit.rs step 7), so at this point nothing in
    // the band is live and the rewind below hands every byte back. Without
    // it, each evaluation stacks its broker on top of the last run's dead
    // state and a resident instance leaks its way into the declared memory
    // maximum.
    //
    // The stale structures are forgotten, never dropped: their backing memory
    // is the arena band itself, which this rewind — or a host-side `reset`
    // between evaluations — reclaims out from under them, and a drop would
    // walk Vec/String headers in memory the band no longer guarantees intact
    // and feed whatever it finds to `standalone_wasm_dealloc` as (ptr, len) pairs.
    core::mem::forget(c.broker.take());
    core::mem::forget(core::mem::take(&mut c.postures));
    {
        // SAFETY: no `STATE` reference is live, and the reset performs no heap
        // allocation while holding the returned borrow.
        standalone_wasm::arena_rewind(unsafe { standalone_wasm::arena_init() });
    }
    if cfg_len < 68 {
        standalone_wasm::set_err(&[b"malformed broker config block"]);
        return 1;
    }
    let version = load_u32(cfg);
    if version != 1 {
        standalone_wasm::set_err(&[b"unsupported broker config version"]);
        return 1;
    }
    let flags = load_u32(cfg + 4);
    let required_currency = flags >> 8;
    // SAFETY: read-only copy, before any broker allocation.
    let symbol_currency = unsafe { standalone_wasm::state() }.symbol_currency;
    if required_currency != 0 && required_currency != symbol_currency {
        standalone_wasm::set_err(&[b"explicit account currency requires matching dataset quote currency; FX conversion is unsupported"]);
        return 1;
    }
    let n_strings = load_u32(cfg + 64);
    let table = cfg + 68;
    // The table must fit inside the block; string bytes are bounds-checked
    // per read against the block end.
    let Some(table_end) = n_strings.checked_mul(8).and_then(|n| table.checked_add(n)) else {
        standalone_wasm::set_err(&[b"malformed broker config block"]);
        return 1;
    };
    let Some(cfg_end) = cfg.checked_add(cfg_len) else {
        standalone_wasm::set_err(&[b"malformed broker config block"]);
        return 1;
    };
    if table_end > cfg_end {
        standalone_wasm::set_err(&[b"malformed broker config block"]);
        return 1;
    }

    let view = CfgView {
        initial_capital: load_f64(cfg + 8),
        default_qty_value: load_f64(cfg + 16),
        commission_value: load_f64(cfg + 24),
        margin_long: load_f64(cfg + 32),
        margin_short: load_f64(cfg + 40),
        slippage: load_u32(cfg + 48),
        pyramiding: load_u32(cfg + 52),
        backtest_fill_limits_assumption: load_u32(cfg + 56),
        process_orders_on_close: flags & 1 != 0,
        calc_on_every_tick: flags & 2 != 0,
        qty_type: load_u8(cfg + 60),
        commission_type: load_u8(cfg + 61),
        close_entries_rule: load_u8(cfg + 62),
    };
    let mintick = load_f64(OFF_MINTICK);
    // Symbol economics are reset and validated by each standalone WebAssembly request.
    // SAFETY: read-only copies; no standalone WebAssembly state reference spans broker allocation.
    let (pointvalue, mincontract) = unsafe {
        let s = standalone_wasm::state();
        (s.pointvalue, s.mincontract)
    };
    let mut broker = Broker::new_with_symbol(&view, mintick, pointvalue, mincontract, "");
    broker.set_limits(BrokerLimits {
        max_pending_orders: MAX_PENDING_ORDERS,
        max_open_lots: MAX_OPEN_LOTS,
        max_exit_recipes: MAX_EXIT_RECIPES,
    });

    c.pool_base = cfg;
    c.pool_table = table;
    c.pool_n = n_strings;
    c.pool_end = cfg_end;
    c.current_bar = 0;
    let bars = load_u32(standalone_wasm::OFF_BARS);
    // The per-bar lanes are reserved for the whole run up front, so a feed
    // too long for the memory maximum fails here with the documented error
    // instead of trapping when a lane outgrows the band part-way through.
    let per_bar = u32::try_from(Broker::BAR_LANE_BYTES)
        .unwrap_or(u32::MAX)
        .saturating_add(BAR_HEAP_SLACK);
    if !standalone_wasm::arena_has_headroom(
        bars.saturating_mul(per_bar)
            .saturating_add(standalone_wasm::BROKER_BAR_HEADROOM),
    ) {
        standalone_wasm::set_err(&[standalone_wasm::BROKER_MEMORY_EXHAUSTED]);
        return 1;
    }
    broker.reserve_bar_lanes(bars as usize);
    c.postures = Vec::with_capacity(bars as usize);
    c.broker = Some(broker);
    0
}

/// Whether the band has room for one bar's short-lived allocations and for
/// the broker's trade ledger to double. False sets the documented memory
/// error.
fn has_ledger_headroom(broker: &Broker) -> bool {
    let ledger = u32::try_from(broker.trade_ledger_bytes()).unwrap_or(u32::MAX);
    let room = standalone_wasm::arena_has_headroom(
        ledger
            .saturating_mul(2)
            .saturating_add(standalone_wasm::BROKER_BAR_HEADROOM),
    );
    if !room {
        standalone_wasm::set_err(&[standalone_wasm::BROKER_MEMORY_EXHAUSTED]);
    }
    room
}

/// Everything the interpreter does for one bar *before* the
/// script body runs: currency sync, risk-session bookkeeping, and the
/// pre-script fill pass over the bar's open, high, low and close (fills
/// before the body: orders placed on bar N fill on bar N+1's pass, or on
/// bar N's close pass under `process_orders_on_close`). Drains the
/// fill-alert accumulator. Returns 0, or 1 with the error slot set (a broker
/// cap breach, or less than
/// `BROKER_BAR_HEADROOM` of memory left for the bar).
#[no_mangle]
pub extern "C" fn pinewasm_rt_broker_begin_bar(bar: u32) -> i32 {
    // SAFETY: the exported call is synchronous and holds the sole `CTX`
    // borrow; broker operations cannot call back into this runtime module.
    let c = unsafe { ctx() };
    let Some(broker) = c.broker.as_mut() else {
        standalone_wasm::set_err(&[b"broker used before pinewasm_rt_broker_init"]);
        return 1;
    };
    // A bar that could not finish is refused before it starts.
    if !has_ledger_headroom(broker) {
        return 1;
    }
    let open = lane(LANE_OPEN, bar);
    let high = lane(LANE_HIGH, bar);
    let low = lane(LANE_LOW, bar);
    let close = lane(standalone_wasm::LANE_CLOSE, bar);
    // Identity FX every bar (equal/unset currencies), same call order as the
    // interpreter's bar loop.
    broker.sync_currency_rate(bar as usize, None);
    broker.begin_session(session_key(bar_time_ms(bar)), bar as usize, open);
    if let Err(error) = broker.process_bar_ticks(bar as usize, open, high, low, close) {
        set_cap_err(error);
        return 1;
    }
    // One script calculation per bar in the AOT tier.
    broker.begin_script_execution();
    // Bound the accumulators: the standalone WebAssembly tier has no alert subscriber.
    drop(broker.take_order_fill_alerts());
    let _ = broker.take_immediate_fill_count();
    c.current_bar = bar;
    0
}

/// Everything the interpreter does after the close-tick script execution:
/// the `process_orders_on_close` close pass (when declared) and the bar's
/// equity/margin sample, then this bar's posture cell. Returns 0, or 1 with
/// the error slot set.
#[no_mangle]
pub extern "C" fn pinewasm_rt_broker_finish_bar(bar: u32) -> i32 {
    // SAFETY: the exported call is synchronous and holds the sole `CTX`
    // borrow; broker operations cannot call back into this runtime module.
    let c = unsafe { ctx() };
    let Some(broker) = c.broker.as_mut() else {
        standalone_wasm::set_err(&[b"broker used before pinewasm_rt_broker_init"]);
        return 1;
    };
    let close = lane(standalone_wasm::LANE_CLOSE, bar);
    if let Err(error) = broker.finish_bar(bar as usize, close) {
        set_cap_err(error);
        return 1;
    }
    drop(broker.take_order_fill_alerts());
    let _ = broker.take_immediate_fill_count();
    let posture = broker.posture() as i8;
    c.postures.push(posture);
    0
}

// ---------------------------------------------------------------------------
// Placement. One export per `OrderKind`, uniform `(args, site) -> status`
// signature (`site` is meaningful for the risk directives only; the emitted
// call passes the CSIL pc everywhere for uniformity).

/// `strategy.entry` — slots `[id, direction, qty, limit, stop, oca_name,
/// oca_type, comment, alert_message, disable_alert]`.
#[no_mangle]
pub extern "C" fn pinewasm_rt_broker_entry(args: u32, _site: u32) -> i32 {
    place_opening(args, true)
}

/// `strategy.order` — same slots as `entry`, no pyramiding cap.
#[no_mangle]
pub extern "C" fn pinewasm_rt_broker_order(args: u32, _site: u32) -> i32 {
    place_opening(args, false)
}

fn place_opening(args: u32, is_entry: bool) -> i32 {
    let id = as_string(args, 0).unwrap_or_default();
    // A missing or invalid direction drops the order.
    let Some(sign) = direction_sign(args, 1) else {
        return 0;
    };
    let qty = cell_f64(args, 2);
    let limit = cell_f64(args, 3);
    let stop = cell_f64(args, 4);
    let metadata = OrderMetadata {
        oca_name: optional_string(args, 5),
        oca_type: OcaType::from_token(cell_str(args, 6)),
        comment: optional_string(args, 7),
        alert_message: optional_string(args, 8),
        disable_alert: cell_bool(args, 9),
    };
    // SAFETY: all pool-backed operands have been copied or decoded, so this
    // synchronous call now creates the sole `CTX` borrow.
    let Some(broker) = (unsafe { ctx() }).broker.as_mut() else {
        return 0;
    };
    if is_entry {
        broker.place_entry(id, sign, qty, limit, stop, metadata);
    } else {
        broker.place_order(id, sign, qty, limit, stop, metadata);
    }
    0
}

/// `strategy.exit` — slots `[id, from_entry, qty, qty_percent, profit,
/// limit, loss, stop, trail_price, trail_points, trail_offset, oca_name,
/// comment, comment_profit, comment_loss, comment_trailing, alert_message,
/// alert_profit, alert_loss, alert_trailing, disable_alert]`.
#[no_mangle]
pub extern "C" fn pinewasm_rt_broker_exit(args: u32, _site: u32) -> i32 {
    let id = as_string(args, 0).unwrap_or_default();
    let from_entry = as_string(args, 1);
    let qty = cell_f64(args, 2);
    let qty_percent = cell_f64(args, 3);
    let profit = cell_f64(args, 4);
    let limit = cell_f64(args, 5);
    let loss = cell_f64(args, 6);
    let stop = cell_f64(args, 7);
    let trail_price = cell_f64(args, 8);
    let trail_points = cell_f64(args, 9);
    let trail_offset = cell_f64(args, 10);
    let metadata = ExitMetadata {
        oca_name: optional_string(args, 11),
        comment: optional_string(args, 12),
        comment_profit: optional_string(args, 13),
        comment_loss: optional_string(args, 14),
        comment_trailing: optional_string(args, 15),
        alert_message: optional_string(args, 16),
        alert_profit: optional_string(args, 17),
        alert_loss: optional_string(args, 18),
        alert_trailing: optional_string(args, 19),
        disable_alert: cell_bool(args, 20),
    };
    // SAFETY: all pool-backed operands have been copied before this
    // synchronous call creates the sole `CTX` borrow.
    let Some(broker) = (unsafe { ctx() }).broker.as_mut() else {
        return 0;
    };
    broker.place_exit(
        id,
        from_entry,
        qty,
        qty_percent,
        profit,
        limit,
        loss,
        stop,
        trail_price,
        trail_points,
        trail_offset,
        metadata,
    );
    0
}

/// `strategy.close` — slots `[id, comment, qty, qty_percent, alert_message,
/// immediately, disable_alert]`.
#[no_mangle]
pub extern "C" fn pinewasm_rt_broker_close(args: u32, _site: u32) -> i32 {
    let id = as_string(args, 0);
    let qty = cell_f64(args, 2);
    let qty_percent = cell_f64(args, 3);
    let metadata = OrderMetadata {
        comment: optional_string(args, 1),
        alert_message: optional_string(args, 4),
        disable_alert: cell_bool(args, 6),
        ..OrderMetadata::default()
    };
    let fill_immediately = cell_bool(args, 5);
    // SAFETY: all pool-backed operands have been copied before this
    // synchronous call creates the sole `CTX` borrow.
    let c = unsafe { ctx() };
    let bar = c.current_bar;
    let Some(broker) = c.broker.as_mut() else {
        return 0;
    };
    let immediate =
        fill_immediately.then_some((bar as usize, lane(standalone_wasm::LANE_CLOSE, bar)));
    broker.place_close(id, qty, qty_percent, metadata, immediate);
    0
}

/// `strategy.close_all` — slots `[comment, alert_message, immediately,
/// disable_alert]`.
#[no_mangle]
pub extern "C" fn pinewasm_rt_broker_close_all(args: u32, _site: u32) -> i32 {
    let metadata = OrderMetadata {
        comment: optional_string(args, 0),
        alert_message: optional_string(args, 1),
        disable_alert: cell_bool(args, 3),
        ..OrderMetadata::default()
    };
    let fill_immediately = cell_bool(args, 2);
    // SAFETY: all pool-backed operands have been copied before this
    // synchronous call creates the sole `CTX` borrow.
    let c = unsafe { ctx() };
    let bar = c.current_bar;
    let Some(broker) = c.broker.as_mut() else {
        return 0;
    };
    let immediate =
        fill_immediately.then_some((bar as usize, lane(standalone_wasm::LANE_CLOSE, bar)));
    broker.place_close(None, None, None, metadata, immediate);
    0
}

/// `strategy.cancel` — slot `[id]`.
#[no_mangle]
pub extern "C" fn pinewasm_rt_broker_cancel(args: u32, _site: u32) -> i32 {
    let id = cell_str(args, 0);
    // SAFETY: pool lookup has completed, so this synchronous call creates the
    // sole `CTX` borrow and retains it only for the cancellation.
    let Some(broker) = (unsafe { ctx() }).broker.as_mut() else {
        return 0;
    };
    if let Some(id) = id {
        broker.cancel(id);
    }
    0
}

/// `strategy.cancel_all` — no slots.
#[no_mangle]
pub extern "C" fn pinewasm_rt_broker_cancel_all(_args: u32, _site: u32) -> i32 {
    // SAFETY: the exported call is synchronous and retains the sole `CTX`
    // borrow only for the cancellation.
    let Some(broker) = (unsafe { ctx() }).broker.as_mut() else {
        return 0;
    };
    broker.cancel_all();
    0
}

/// `strategy.risk.max_position_size` — slot `[contracts]`.
#[no_mangle]
pub extern "C" fn pinewasm_rt_broker_risk_max_position_size(args: u32, site: u32) -> i32 {
    let contracts = cell_f64(args, 0).unwrap_or(f64::NAN);
    // SAFETY: operand decoding does not retain a `CTX` reference, so this
    // synchronous call creates the sole context borrow.
    let Some(broker) = (unsafe { ctx() }).broker.as_mut() else {
        return 0;
    };
    broker.set_max_position_size_at_site(site as usize, contracts);
    0
}

/// `strategy.risk.allow_entry_in` — slot `[value]` (a direction token). An
/// invalid token is the interpreter's runtime-error path.
#[no_mangle]
pub extern "C" fn pinewasm_rt_broker_risk_allow_entry_in(args: u32, site: u32) -> i32 {
    let dir = match cell_str(args, 0) {
        Some("strategy.direction.long") => 1i8,
        Some("strategy.direction.short") => -1,
        Some("strategy.direction.all") => 0,
        _ => {
            set_broker_err(&[
                b"strategy.risk.allow_entry_in value must be strategy.direction.all, ",
                b"strategy.direction.long, or strategy.direction.short",
            ]);
            return 1;
        }
    };
    // SAFETY: pool lookup has completed, so this synchronous call creates the
    // sole `CTX` borrow.
    let Some(broker) = (unsafe { ctx() }).broker.as_mut() else {
        return 0;
    };
    broker.set_allow_entry_in_at_site(site as usize, dir);
    0
}

/// Shared decode for the two loss-threshold directives — slots
/// `[value, type, alert_message]`, mirroring the interpreter's validation
/// (finite non-negative value, valid unit token, percent ≤ 100).
fn risk_threshold(args: u32, member: &'static [u8]) -> Result<(f64, RiskUnit), ()> {
    let unit = match cell_str(args, 1) {
        Some("strategy.cash") => RiskUnit::Cash,
        Some("strategy.percent_of_equity") => RiskUnit::PercentOfEquity,
        _ => {
            set_broker_err(&[
                member,
                b" type must be strategy.cash or strategy.percent_of_equity",
            ]);
            return Err(());
        }
    };
    let Some(value) = cell_f64(args, 0) else {
        set_broker_err(&[member, b" value must be a finite non-negative number"]);
        return Err(());
    };
    if value < 0.0 {
        set_broker_err(&[member, b" value must be a finite non-negative number"]);
        return Err(());
    }
    if unit == RiskUnit::PercentOfEquity && value > 100.0 {
        set_broker_err(&[
            member,
            b" percent-of-equity value must be between 0 and 100 inclusive",
        ]);
        return Err(());
    }
    Ok((value, unit))
}

/// `strategy.risk.max_drawdown` — slots `[value, type, alert_message]`.
#[no_mangle]
pub extern "C" fn pinewasm_rt_broker_risk_max_drawdown(args: u32, site: u32) -> i32 {
    let Ok((value, unit)) = risk_threshold(args, b"strategy.risk.max_drawdown") else {
        return 1;
    };
    let alert = optional_string(args, 2);
    // SAFETY: all pool-backed operands have been copied before this
    // synchronous call creates the sole `CTX` borrow.
    let Some(broker) = (unsafe { ctx() }).broker.as_mut() else {
        return 0;
    };
    broker.set_max_drawdown_at_site(site as usize, value, unit, alert);
    0
}

/// `strategy.risk.max_intraday_loss` — slots `[value, type, alert_message]`.
#[no_mangle]
pub extern "C" fn pinewasm_rt_broker_risk_max_intraday_loss(args: u32, site: u32) -> i32 {
    let Ok((value, unit)) = risk_threshold(args, b"strategy.risk.max_intraday_loss") else {
        return 1;
    };
    let alert = optional_string(args, 2);
    // SAFETY: all pool-backed operands have been copied before this
    // synchronous call creates the sole `CTX` borrow.
    let Some(broker) = (unsafe { ctx() }).broker.as_mut() else {
        return 0;
    };
    broker.set_max_intraday_loss_at_site(site as usize, value, unit, alert);
    0
}

/// `strategy.risk.max_intraday_filled_orders` — slots `[count,
/// alert_message]`.
#[no_mangle]
pub extern "C" fn pinewasm_rt_broker_risk_max_intraday_filled_orders(args: u32, site: u32) -> i32 {
    let count = cell_i64(args, 0).map_or(f64::NAN, |count| count as f64);
    let alert = optional_string(args, 1);
    // SAFETY: all pool-backed operands have been copied before this
    // synchronous call creates the sole `CTX` borrow.
    let Some(broker) = (unsafe { ctx() }).broker.as_mut() else {
        return 0;
    };
    broker.set_max_intraday_filled_orders_at_site(site as usize, count, alert);
    0
}

/// `strategy.risk.max_cons_loss_days` — slots `[count, alert_message]`.
#[no_mangle]
pub extern "C" fn pinewasm_rt_broker_risk_max_cons_loss_days(args: u32, site: u32) -> i32 {
    let count = cell_i64(args, 0).map_or(f64::NAN, |count| count as f64);
    let alert = optional_string(args, 1);
    // SAFETY: all pool-backed operands have been copied before this
    // synchronous call creates the sole `CTX` borrow.
    let Some(broker) = (unsafe { ctx() }).broker.as_mut() else {
        return 0;
    };
    broker.set_max_cons_loss_days_at_site(site as usize, count, alert);
    0
}

// ---------------------------------------------------------------------------
// The strategy.* snapshot read surface. Field ids are the contract between
// `pinewasm_aot::body`'s lowering and this decode; the differential suite
// pins their agreement behaviorally.

/// f64 snapshot fields, by id (the `body.rs` table mirrors this order).
#[no_mangle]
pub extern "C" fn pinewasm_rt_broker_snap_f64(field: u32, bar: u32) -> f64 {
    // SAFETY: the exported snapshot read is synchronous and retains the sole
    // `CTX` borrow only through the field copy below.
    let Some(snap) = (unsafe { ctx() })
        .broker
        .as_ref()
        .and_then(|broker| broker.snapshot_at(bar as usize))
    else {
        return f64::NAN;
    };
    match field {
        0 => snap.position_size,
        1 => snap.position_avg_price,
        2 => snap.open_profit,
        3 => snap.net_profit,
        4 => snap.gross_profit,
        5 => snap.gross_loss,
        6 => snap.equity,
        7 => snap.initial_capital,
        8 => snap.max_drawdown,
        9 => snap.max_runup,
        10 => snap.avg_trade,
        11 => snap.avg_winning_trade,
        12 => snap.avg_losing_trade,
        13 => snap.max_contracts_held,
        14 => snap.max_contracts_held_long,
        15 => snap.max_contracts_held_short,
        16 => snap.avg_losing_trade_percent,
        17 => snap.avg_trade_percent,
        18 => snap.avg_winning_trade_percent,
        19 => snap.gross_loss_percent,
        20 => snap.gross_profit_percent,
        21 => snap.margin_liquidation_price,
        22 => snap.max_drawdown_percent,
        23 => snap.max_runup_percent,
        24 => snap.net_profit_percent,
        25 => snap.open_profit_percent,
        26 => snap.open_trades_capital_held,
        _ => f64::NAN,
    }
}

/// i64 snapshot fields, by id. `i64::MIN` is the na sentinel.
#[no_mangle]
pub extern "C" fn pinewasm_rt_broker_snap_i64(field: u32, bar: u32) -> i64 {
    // SAFETY: the exported snapshot read is synchronous and retains the sole
    // `CTX` borrow only through the field copy below.
    let Some(snap) = (unsafe { ctx() })
        .broker
        .as_ref()
        .and_then(|broker| broker.snapshot_at(bar as usize))
    else {
        return i64::MIN;
    };
    match field {
        0 => snap.win_trades,
        1 => snap.loss_trades,
        2 => snap.even_trades,
        3 => snap.closed_trades,
        4 => snap.open_trades,
        5 => snap.closed_trades_first_index,
        _ => i64::MIN,
    }
}

// ---------------------------------------------------------------------------
// The per-trade accessor read surface (`strategy.opentrades.*(n)` /
// `strategy.closedtrades.*(n)`). Like the interpreter's `read_trade`, these
// read the live broker's `trade_view` at call time, not a per-bar snapshot.
// `field` carries the accessor id in its low byte and [`TRADE_OPEN`] for the
// open-lot list; `index` is `trade_num` (`i64::MIN` = na). A negative, na, or
// out-of-range index reads na.

/// `field` flag selecting the open-lot list instead of the closed ledger.
const TRADE_OPEN: u32 = 0x100;

/// The trade `field` addresses at `index`, or `None` (reads na).
fn trade_at(field: u32, index: i64) -> Option<pinewasm_broker::TradeView<'static>> {
    let index = usize::try_from(index).ok()?;
    // SAFETY: the exported trade read is synchronous and retains the sole
    // `CTX` borrow only through the caller's field copy.
    (unsafe { ctx() })
        .broker
        .as_ref()?
        .trade_view(index, field & TRADE_OPEN != 0)
}

/// f64 trade fields, by id (`pinewasm_aot::analysis::strategy_trade_field`
/// mirrors this order). Non-finite results read na, as `Value::from_f64`.
#[no_mangle]
pub extern "C" fn pinewasm_rt_broker_trade_f64(field: u32, index: i64) -> f64 {
    let Some(trade) = trade_at(field, index) else {
        return f64::NAN;
    };
    let value = match field & 0xff {
        0 => trade.entry_price,
        1 => trade.exit_price,
        2 => trade.size,
        3 => trade.profit,
        4 => trade.profit / trade.entry_notional * 100.0,
        5 => trade.commission,
        6 => trade.max_runup,
        7 => trade.max_runup / trade.entry_notional * 100.0,
        8 => trade.max_drawdown,
        9 => trade.max_drawdown / trade.entry_notional * 100.0,
        _ => f64::NAN,
    };
    if value.is_finite() {
        value
    } else {
        f64::NAN
    }
}

/// i64 trade fields, by id. `i64::MIN` is the na sentinel.
#[no_mangle]
pub extern "C" fn pinewasm_rt_broker_trade_i64(field: u32, index: i64) -> i64 {
    let Some(trade) = trade_at(field, index) else {
        return i64::MIN;
    };
    match field & 0xff {
        0 => i64::from(trade.entry_bar),
        1 => i64::from(trade.exit_bar),
        2 => bar_time_ms(trade.entry_bar),
        3 => bar_time_ms(trade.exit_bar),
        _ => i64::MIN,
    }
}

// ---------------------------------------------------------------------------
// The posture serializer.

/// Serialize the broker's posture lane into the OUT document under the
/// `posture-events` semantics: an event on **every**
/// posture change — into +1/−1 *and into 0* (a close-to-flat event). The
/// well-formedness rules hold by construction: `prev` starts flat,
/// so a leading flat stretch emits nothing and the first event is nonzero,
/// and a `0` event can only follow a nonzero one (a 0→0 "transition" is not
/// a change). `price` is the event bar's close (finite check as
/// [`standalone_wasm::standalone_wasm_serialize`]), `time` the bar's time; same ryu shortest-roundtrip
/// serialization. Packed `(ptr << 32) | len`, or 0 with the error slot set.
#[no_mangle]
pub extern "C" fn standalone_wasm_serialize_posture(slice_end: u32, plot_count: u32) -> u64 {
    // Read-only projection after the complete run. Keep every live broker and
    // series dependency during execution; only final report construction and
    // output serialization are reduced.
    // SAFETY: only the scalar mode flag is copied; no state borrow survives.
    if unsafe { standalone_wasm::state() }.objective_results {
        return serialize_objectives(slice_end);
    }
    // SAFETY: only the scalar mode flag is copied; no state borrow survives.
    if unsafe { standalone_wasm::state() }.trade_results {
        return serialize_trade_results(slice_end);
    }
    // Finish broker allocations before reserving the output arena: outcome
    // construction collects the trade ledger and report aggregates.
    // SAFETY: both borrows end before any serialization or allocator call.
    let full = unsafe { standalone_wasm::state() }.include_results;
    // The outcome moves the per-bar lanes and copies the trade rows, so it
    // fits in the room a bar keeps. Refuse it up front rather than trap
    // part-way through.
    // SAFETY: the broker borrow ends with the check, before any allocation.
    let room = !full
        || unsafe { ctx() }
            .broker
            .as_ref()
            .is_none_or(has_ledger_headroom);
    if !room {
        return 0;
    }
    let broker = if full {
        // SAFETY: move the owned broker out before outcome allocation begins.
        unsafe { ctx() }.broker.take()
    } else {
        None
    };
    let result = broker.map(|broker| {
        broker.into_outcome(lane(
            standalone_wasm::LANE_CLOSE,
            slice_end.saturating_sub(1),
        ))
    });

    // SAFETY: this exported call is synchronous; the borrow ends after
    // `start` is computed, before posture serialization begins.
    let st = unsafe { standalone_wasm::arena_init() };
    let start = (st.arena_ptr + 7) & !7;
    let mut out = standalone_wasm::Out { start, pos: start };
    // SAFETY: copy the mode flag before the serializer uses the arena.
    let binary = unsafe { standalone_wasm::state() }.binary_results;
    let serialized = if binary {
        serialize_binary_results(slice_end, plot_count, result.as_ref(), &mut out)
    } else {
        serialize_postures(slice_end, plot_count, result.as_ref(), &mut out, false)
    };
    match serialized {
        Ok(()) => {
            // SAFETY: the initial arena borrow ended before serialization;
            // serialization retains no `STATE` reference.
            let st = unsafe { standalone_wasm::state() };
            st.arena_ptr = out.pos;
            ((out.start as u64) << 32) | u64::from(out.pos - out.start)
        }
        Err(()) => 0,
    }
}

fn serialize_objectives(bars: u32) -> u64 {
    // SAFETY: the broker borrow ends before error handling or output allocation.
    let result = unsafe { ctx() }
        .broker
        .as_ref()
        .and_then(Broker::retained_objectives);
    let Some(result) = result else {
        standalone_wasm::set_err(&[b"objective results require finite completed broker data"]);
        return 0;
    };
    // SAFETY: only scalar arena coordinates survive into serialization.
    let start = (unsafe { standalone_wasm::arena_init() }.arena_ptr + 7) & !7;
    let mut out = standalone_wasm::Out { start, pos: start };
    let serialized = (|| {
        out.push(b"{\"version\":1,\"projection\":\"retained-objectives-v1\",\"bars\":")?;
        out.number(f64::from(bars))?;
        out.push(b",\"firstRetainedTradeIndex\":")?;
        out.number(result.first_retained_trade_index as f64)?;
        out.push(b",\"metrics\":{")?;
        for (index, (name, value)) in [
            (b"initialCapital".as_slice(), result.initial_capital),
            (b"netProfit", result.net_profit),
            (b"grossProfit", result.gross_profit),
            (b"grossLoss", result.gross_loss),
            (b"closedTrades", result.closed_trades as f64),
            (b"totalCommission", result.total_commission),
            (b"finalEquity", result.final_equity),
            (b"positionSize", result.position_size),
            (b"maxDrawdown", result.max_drawdown),
        ]
        .into_iter()
        .enumerate()
        {
            if index != 0 {
                out.push(b",")?;
            }
            out.push(b"\"")?;
            out.push(name)?;
            out.push(b"\":")?;
            out.number(value)?;
        }
        out.push(b"}}")
    })();
    if serialized.is_err() {
        return 0;
    }
    // SAFETY: serialization has ended and retains no state reference.
    unsafe { standalone_wasm::state() }.arena_ptr = out.pos;
    ((out.start as u64) << 32) | u64::from(out.pos - out.start)
}

/// The compact trade ledger (`trade_results`, projection `broker-trades-v1`):
/// the completed broker's kept closed-trade rows and full-run counters,
/// with no posture events, plots, equity or margin lanes, currency-rate lane,
/// or report aggregates. Of the report it carries only the three commission
/// totals, which the broker sums in place from its open lots and running
/// scope totals. The posture events are left out because the document
/// must stay bounded: one can be written for every bar, while the rows are
/// held to the broker's filled-order window however long the run is. The
/// broker is read in place and nothing here allocates, so the output can
/// claim the top of the arena before the ledger is read.
/// Packed `(ptr << 32) | len`, or 0 with the error slot set.
fn serialize_trade_results(slice_end: u32) -> u64 {
    // SAFETY: only scalar arena coordinates survive into serialization.
    let start = (unsafe { standalone_wasm::arena_init() }.arena_ptr + 7) & !7;
    let mut out = standalone_wasm::Out { start, pos: start };
    let serialized = (|| {
        out.push(b"{\"version\":1,\"projection\":\"broker-trades-v1\",\"bars\":")?;
        ledger_integer(&mut out, slice_end)?;
        out.push(b",\"strategy\":{")?;
        serialize_trade_ledger(slice_end, &mut out)?;
        out.push(b"}}")
    })();
    if serialized.is_err() {
        return 0;
    }
    // SAFETY: serialization has ended and retains no state reference.
    unsafe { standalone_wasm::state() }.arena_ptr = out.pos;
    ((out.start as u64) << 32) | u64::from(out.pos - out.start)
}

/// A field the broker holds as an integer.
fn ledger_integer(out: &mut standalone_wasm::Out, value: u32) -> Result<(), ()> {
    out.push(itoa::Buffer::new().format(value).as_bytes())
}

/// One finite ledger number, shortest round-trip, with negative zero written
/// as `0.0`: a flat position's size and open profit are the empty sum `-0.0`,
/// and a JSON reader is not required to keep a zero's sign.
fn ledger_number(out: &mut standalone_wasm::Out, value: f64) -> Result<(), ()> {
    out.number(if value == 0.0 { 0.0 } else { value })
}

/// A count or direction the broker holds as an `f64`, written as the integer
/// it is so integer and float readers both accept it.
fn ledger_whole(out: &mut standalone_wasm::Out, value: f64) -> Result<(), ()> {
    let whole = value as i32;
    if f64::from(whole) == value {
        out.push(itoa::Buffer::new().format(whole).as_bytes())
    } else {
        ledger_number(out, value)
    }
}

/// Why a completed run has no ledger. The run's own arithmetic overflowed (an
/// order size or price the script chose), so this is the expected-failure
/// class, like a broker cap: one evaluation's outcome, not a host fault. Every
/// row of a live broker is its own, so a missing row can only be this.
const LEDGER_NOT_FINITE: &[u8] = b"trade ledger contains a value that is not finite";

/// The `strategy` object of [`serialize_trade_results`], without its braces.
fn serialize_trade_ledger(slice_end: u32, out: &mut standalone_wasm::Out) -> Result<(), ()> {
    // The same mark as full results: the last evaluated bar's close.
    let final_close = lane(standalone_wasm::LANE_CLOSE, slice_end.saturating_sub(1));
    // SAFETY: only the scalar currency text is copied; no state borrow survives.
    let (currency_text, currency_len) = unsafe {
        let st = standalone_wasm::state();
        (st.symbol_currency_text, st.symbol_currency_len)
    };
    // SAFETY: serialization is synchronous and retains the sole `CTX` borrow;
    // its callees access only linear memory and the separate standalone WebAssembly state.
    let c = unsafe { ctx() };
    let Some(broker) = c.broker.as_ref() else {
        standalone_wasm::set_err(&[b"trade results require a completed broker"]);
        return Err(());
    };
    let Some(ledger) = broker.trade_ledger(final_close) else {
        set_broker_err(&[LEDGER_NOT_FINITE]);
        return Err(());
    };
    // The broker runs on identity conversion with empty internal currencies
    // (see the module docs), so the document names the currencies itself:
    // the request's quote currency, and the declared account currency, which
    // the init guard held equal to it, or that quote currency when unset.
    let symbol_currency = currency_text
        .get(..usize::from(currency_len))
        .and_then(|text| core::str::from_utf8(text).ok())
        .unwrap_or("");
    let declared = (load_u32(c.pool_base + 4) >> 8).to_le_bytes();
    let account_currency = if declared.first().is_some_and(|byte| *byte != 0) {
        declared
            .get(..3)
            .and_then(|text| core::str::from_utf8(text).ok())
            .unwrap_or("")
    } else {
        symbol_currency
    };

    for (name, value) in [
        (b"\"initialCapital\":".as_slice(), ledger.initial_capital),
        (b",\"mintick\":", ledger.mintick),
        (b",\"pointValue\":", ledger.point_value),
    ] {
        out.push(name)?;
        ledger_number(out, value)?;
    }
    out.push(b",\"slippageTicks\":")?;
    ledger_whole(out, ledger.slippage_ticks)?;
    out.push(b",\"accountCurrency\":")?;
    out.string(account_currency)?;
    out.push(b",\"symbolCurrency\":")?;
    out.string(symbol_currency)?;
    out.push(if ledger.currency_conversion_complete {
        b",\"currencyConversionComplete\":true".as_slice()
    } else {
        b",\"currencyConversionComplete\":false"
    })?;
    out.push(b",\"firstRetainedTradeIndex\":")?;
    ledger_integer(out, ledger.first_retained_trade_index)?;
    out.push(b",\"closedTrades\":")?;
    ledger_integer(out, ledger.closed_trades)?;
    // The first-entry fields exist only once an opening leg has filled.
    if let Some(bar) = ledger.first_entry_bar {
        out.push(b",\"firstEntryBar\":")?;
        ledger_integer(out, bar)?;
    }
    if let Some(price) = ledger.first_entry_price {
        out.push(b",\"firstEntryPrice\":")?;
        ledger_number(out, price)?;
    }
    if let Some(notional) = ledger.first_entry_account_notional_per_unit {
        out.push(b",\"firstEntryAccountNotionalPerUnit\":")?;
        ledger_number(out, notional)?;
    }
    for (name, value) in [
        (b",\"netProfit\":".as_slice(), ledger.net_profit),
        (b",\"grossProfit\":", ledger.gross_profit),
        (b",\"grossLoss\":", ledger.gross_loss),
        (b",\"openProfit\":", ledger.open_profit),
        (b",\"maxDrawdown\":", ledger.max_drawdown),
        (b",\"maxDrawdownPercent\":", ledger.max_drawdown_percent),
        (b",\"openEntryCommission\":", ledger.open_entry_commission),
        (b",\"commissionPaid\":", ledger.commission_paid),
        (b",\"commissionPaidLong\":", ledger.commission_paid_long),
        (b",\"commissionPaidShort\":", ledger.commission_paid_short),
        (b",\"position\":{\"size\":", ledger.position.size),
        (b",\"avgPrice\":", ledger.position.avg_price),
    ] {
        out.push(name)?;
        ledger_number(out, value)?;
    }
    out.push(b"},\"trades\":[")?;
    for (index, trade) in ledger.trades().enumerate() {
        let Some(trade) = trade else {
            set_broker_err(&[LEDGER_NOT_FINITE]);
            return Err(());
        };
        out.push(if index == 0 {
            b"{\"index\":".as_slice()
        } else {
            b",{\"index\":"
        })?;
        ledger_integer(out, trade.index)?;
        out.push(b",\"entryId\":")?;
        out.string(trade.entry_id)?;
        out.push(b",\"exitId\":")?;
        out.string(trade.exit_id)?;
        if let Some(comment) = trade.comment {
            out.push(b",\"comment\":")?;
            out.string(comment)?;
        }
        if let Some(name) = trade.exit_order_name {
            out.push(b",\"exitOrderName\":")?;
            out.string(name)?;
        }
        out.push(b",\"sign\":")?;
        ledger_whole(out, trade.sign)?;
        out.push(b",\"qty\":")?;
        ledger_number(out, trade.qty)?;
        out.push(b",\"entryBar\":")?;
        ledger_integer(out, trade.entry_bar)?;
        out.push(b",\"exitBar\":")?;
        ledger_integer(out, trade.exit_bar)?;
        for (name, value) in [
            (b",\"entryPrice\":".as_slice(), trade.entry_price),
            (b",\"exitPrice\":", trade.exit_price),
            (b",\"pnl\":", trade.pnl),
            (b",\"entryNotional\":", trade.entry_notional),
            (b",\"entryCommission\":", trade.entry_commission),
            (b",\"exitCommission\":", trade.exit_commission),
            (b",\"commission\":", trade.commission),
            (b",\"cumulativePnl\":", trade.cumulative_pnl),
            (b",\"maxRunup\":", trade.max_runup),
            (b",\"maxDrawdown\":", trade.max_drawdown),
        ] {
            out.push(name)?;
            ledger_number(out, value)?;
        }
        out.push(b"}")?;
    }
    out.push(b"]")
}

fn serialize_postures(
    slice_end: u32,
    plot_count: u32,
    result: Option<&pinewasm_broker::BrokerOutcome>,
    out: &mut standalone_wasm::Out,
    binary: bool,
) -> Result<(), ()> {
    let bars = load_u32(standalone_wasm::OFF_BARS);
    let builtin_base = load_u32(standalone_wasm::OFF_BUILTIN_BASE);
    let close_base = builtin_base + standalone_wasm::LANE_CLOSE * 8 * bars;
    let time_base = builtin_base + standalone_wasm::LANE_TIME * 8 * bars;
    // SAFETY: serialization is synchronous and retains the sole `CTX` borrow;
    // its callees access only linear memory and the separate standalone WebAssembly state.
    let c = unsafe { ctx() };

    out.push(b"{\"version\":1,\"signals\":[")?;
    let mut prev: i32 = 0;
    let mut first = true;
    let end = (slice_end.min(bars) as usize).min(c.postures.len());
    for bar in 0..end {
        let cur = i32::from(*c.postures.get(bar).unwrap_or(&0));
        if cur != prev {
            let price = load_f64(close_base + bar as u32 * 8);
            if !price.is_finite() {
                let mut buf = itoa::Buffer::new();
                set_broker_err_unprefixed(&[
                    b"non-finite close price at bar ",
                    buf.format(bar).as_bytes(),
                ]);
                return Err(());
            }
            // The time lane holds an i64 stored as an exact f64.
            let time = load_f64(time_base + bar as u32 * 8) as i64;
            if !first {
                out.push(b",")?;
            }
            first = false;
            let mut ibuf = itoa::Buffer::new();
            let mut rbuf = ryu::Buffer::new();
            out.push(b"{\"time\":")?;
            out.push(ibuf.format(time).as_bytes())?;
            out.push(b",\"direction\":")?;
            out.push(match cur {
                1 => b"1".as_slice(),
                -1 => b"-1".as_slice(),
                _ => b"0".as_slice(),
            })?;
            out.push(b",\"price\":")?;
            out.push(rbuf.format_finite(price).as_bytes())?;
            out.push(b"}")?;
        }
        prev = cur;
    }
    // The signals-only document remains the default. Full results are an
    // opt-in ABI extension, serialized from this module's own native broker.
    // SAFETY: copy the flag without retaining the standalone WebAssembly state borrow.
    if unsafe { standalone_wasm::state() }.include_results {
        out.push(b"],\"plots\":[")?;
        let plots_base = load_u32(standalone_wasm::OFF_PLOTS_BASE);
        for plot in 0..if binary { 0 } else { plot_count } {
            if plot > 0 {
                out.push(b",")?;
            }
            out.push(b"[")?;
            for bar in 0..end {
                if bar > 0 {
                    out.push(b",")?;
                }
                out.number(load_f64(plots_base + (plot * bars + bar as u32) * 8))?;
            }
            out.push(b"]")?;
        }
        out.push(b"],\"strategy\":{")?;
        if let Some(result) = result {
            out.push(b"\"trades\":[")?;
            for (index, trade) in result.trades.iter().enumerate() {
                if index > 0 {
                    out.push(b",")?;
                }
                out.push(b"{\"entryId\":")?;
                out.string(&trade.entry_id)?;
                out.push(b",\"exitId\":")?;
                out.string(trade.exit_id.as_deref().unwrap_or(""))?;
                // Optional, as in the interpreter's results: the closing
                // order's comment, and the exit name of a
                // `strategy.close` or `strategy.close_all` order.
                if let Some(comment) = trade.comment.as_deref() {
                    out.push(b",\"comment\":")?;
                    out.string(comment)?;
                }
                if let Some(name) = trade.exit_order_name.as_deref() {
                    out.push(b",\"exitOrderName\":")?;
                    out.string(name)?;
                }
                out.push(b",\"sign\":")?;
                out.number(trade.sign)?;
                out.push(b",\"qty\":")?;
                out.number(trade.qty)?;
                out.push(b",\"entryBar\":")?;
                out.number(trade.entry_bar as f64)?;
                out.push(b",\"exitBar\":")?;
                out.number(trade.exit_bar as f64)?;
                out.push(b",\"entryPrice\":")?;
                out.number(trade.entry_price)?;
                out.push(b",\"exitPrice\":")?;
                out.number(trade.exit_price)?;
                out.push(b",\"pnl\":")?;
                out.number(trade.pnl)?;
                out.push(b",\"commission\":")?;
                out.number(trade.commission)?;
                out.push(b"}")?;
            }
            out.push(b"],\"equity\":[")?;
            for (index, equity) in result
                .equity
                .iter()
                .take(if binary { 0 } else { result.equity.len() })
                .enumerate()
            {
                if index > 0 {
                    out.push(b",")?;
                }
                out.number(*equity)?;
            }
            out.push(b"],\"position\":{\"size\":")?;
            out.number(result.position.size)?;
            out.push(b",\"avgPrice\":")?;
            out.number(result.position.avg_price)?;
            out.push(b"},\"netProfit\":")?;
            out.number(result.metrics.net_profit)?;
            out.push(b",\"grossProfit\":")?;
            out.number(result.metrics.gross_profit)?;
            out.push(b",\"grossLoss\":")?;
            out.number(result.metrics.gross_loss)?;
            out.push(b",\"closedTrades\":")?;
            out.number(result.metrics.closed_trades as f64)?;
            out.push(b",\"maxDrawdown\":")?;
            out.number(result.metrics.max_drawdown)?;
            out.push(b",\"maxDrawdownPercent\":")?;
            out.number(result.max_drawdown_percent)?;
            out.push(b",\"openEntryCommission\":")?;
            out.number(result.open_entry_commission)?;
            out.push(b",\"openProfit\":")?;
            out.number(result.metrics.open_profit)?;
            out.push(b",\"firstRetainedTradeIndex\":")?;
            out.number(result.first_retained_trade_index as f64)?;
        }
        out.push(b"}}")
    } else {
        out.push(b"]}")
    }
}

/// Owned binary result packet: 24-byte header, JSON metadata padded to eight
/// bytes, numeric plot lanes, then the equity lane. The metadata is identical
/// to the JSON result with plots/equity replaced by empty arrays. Every byte is
/// in the output allocation, so result destruction cannot invalidate a lane.
fn serialize_binary_results(
    bars: u32,
    plot_count: u32,
    result: Option<&pinewasm_broker::BrokerOutcome>,
    out: &mut standalone_wasm::Out,
) -> Result<(), ()> {
    let Some(result) = result else {
        standalone_wasm::set_err(&[b"binary results require a completed broker"]);
        return Err(());
    };
    if result.equity.len() != bars as usize {
        standalone_wasm::set_err(&[b"binary equity length mismatch"]);
        return Err(());
    }
    out.push(b"PWSWOUT1")?;
    out.push(&plot_count.to_le_bytes())?;
    out.push(&bars.to_le_bytes())?;
    out.push(&[0u8; 8])?; // metadata length, reserved (zero)
    let metadata_start = out.pos;
    serialize_postures(bars, plot_count, Some(result), out, true)?;
    let metadata_len = out.pos - metadata_start;
    // Header bytes were successfully reserved and initialized above.
    // SAFETY: the preceding successful header writes reserved this u32 cell.
    unsafe { ptr::write_unaligned((out.start + 16) as *mut u32, metadata_len) };
    while out.pos & 7 != 0 {
        out.push(&[0])?;
    }
    let plots_base = load_u32(standalone_wasm::OFF_PLOTS_BASE);
    let lane_bytes = (bars as usize)
        .checked_mul(8)
        .ok_or_else(|| standalone_wasm::set_err(&[b"binary lane overflow"]))?;
    if lane_bytes > isize::MAX as usize {
        standalone_wasm::set_err(&[b"binary lane exceeds addressable range"]);
        return Err(());
    }
    for plot in 0..plot_count {
        let offset = plot
            .checked_mul(bars)
            .and_then(|n| n.checked_mul(8))
            .and_then(|n| plots_base.checked_add(n))
            .ok_or_else(|| standalone_wasm::set_err(&[b"binary plot offset overflow"]))?;
        // The compiler's prepared plot region owns this initialized range;
        // verify bounds before forming a slice for a single bulk copy.
        if standalone_wasm::linear_memory_range(offset as *const u8, lane_bytes).is_none()
            || offset
                .checked_add(lane_bytes as u32)
                .is_none_or(|end| end > out.start)
        {
            standalone_wasm::set_err(&[b"binary plot range outside memory"]);
            return Err(());
        }
        // SAFETY: nonzero region address and range/length checks above; this
        // run owns the plot lanes and serialization cannot call back into it.
        let bytes = unsafe { core::slice::from_raw_parts(offset as *const u8, lane_bytes) };
        out.push(bytes)?;
    }
    // Wasm is little-endian. The result owns initialized f64 equity cells;
    // copy their bytes into the same output allocation before result drop.
    // SAFETY: the slice is a live Vec<f64>, with exactly bars cells as checked
    // above. Its byte length fits isize and its lifetime covers this copy.
    let equity =
        unsafe { core::slice::from_raw_parts(result.equity.as_ptr().cast::<u8>(), lane_bytes) };
    out.push(equity)
}

/// Infrastructure-class (unprefixed) error, matching [`standalone_wasm::standalone_wasm_serialize`]'s
/// non-finite-close class exactly.
fn set_broker_err_unprefixed(detail: &[&[u8]]) {
    standalone_wasm::set_err(detail);
}
