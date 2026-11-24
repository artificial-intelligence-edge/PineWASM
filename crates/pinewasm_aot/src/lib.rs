//! `pinewasm_aot` — ahead-of-time CSIL → WebAssembly backend.
//!
//! Turns a validated [`CsilProgram`] into a `.wasm` module.
//! [`compile_program`] is the strict native-AOT entry point: its exported
//! `run` executes the whole bar loop over typed lanes in linear memory and it
//! reports any unsupported native opcode. [`compile_program_to_wasm`] is the
//! universal artifact entry point: it keeps that native module when possible
//! and otherwise emits an interpreter-hosted WebAssembly module whose `run`
//! import delegates to the interpreter. Consequently every valid CSIL
//! program has a `.wasm` artifact (native codegen where supported, otherwise
//! a hosted stub that runs on the interpreter), and the interpreter defines
//! the semantics of every surface not yet lowered to native Wasm.
//!
//! Native scope (docs/aot.md §8) includes structured control flow, the
//! float/int/bool/color value domains, bounded history, linked numeric and
//! `ta.*` runtimes, scalar/source inputs, styled plots and visuals,
//! `fill(...)` bands, and analyzer-selected collection/UDT operations on the
//! bounded native heap. Residual string, effect, request, drawing, or heap
//! sites may use the explicit `host.vm` plan; the embedding host applies the
//! run-path policy in docs/aot.md §10 before choosing such a module. Strategy
//! metadata selects the interpreter-hosted chart tier; eligible standalone WebAssembly strategy
//! artifacts instead link the shared broker. Realtime/profiled requests run on
//! the interpreter.
//!
//! # Module contract (docs/aot.md §4–6)
//!
//! Exports `memory`, `prepare(bars) -> status`, `run() -> status`. A native
//! artifact has at most one import: `host.vm(site_id, bar)`. The reproducible
//! coverage report is available through `cargo test -p pinewasm_aot
//! --test census -- --nocapture`. [`compile_standalone_program`]
//! refuses modules that still need the interpreter host.
//!
//! Three surfaces that used to be imports are compiled in instead:
//! transcendentals and `%` from the linked numeric [`runtime`], fired
//! alertconditions into an output lane, and the stateful `ta.*` engine
//! ([`pinewasm_ta`], spliced from the same blob as the numerics) with its
//! state in the module's own memory.
//! After `prepare`, the host writes the builtin lanes
//! (open/high/low/close/volume/time as f64, non-finite normalized to NaN,
//! time synthesized per the interpreter's rule when absent), one
//! resolved-source lane per `input.source` declaration, the input table, and
//! optionally the budget and mintick cells; after `run`, plot lanes hold one
//! f64 per bar (NaN = na), color lanes one packed rgbt u32 per bar (`-1` =
//! na) in flat order (every plot's roles, then every visual's, then every
//! fill's), and the alert lane one `(bar, message_id)` pair per firing.
//!
//! Both tiers also carry a `parameter_schema` custom section describing the
//! program's `input.*` declarations, so a host can learn what the module can
//! be asked without a hand-written registry entry. See [`schema`] and
//! docs/aot.md §11.
//!
//! # Emit and audit an artifact
//!
//! The direct Rust emitter is workspace-composition API rather than a stable
//! embedding facade. The emitted module's CSAW ABI is the supported non-Rust
//! contract. This example nevertheless compiles and exercises the real
//! artifact path so its documented handoff cannot drift:
//!
//! ```
//! use pinewasm_aot::{audit_standalone, compile_standalone_program};
//! use pinewasm_compiler::{compile, CompileOptions};
//!
//! let compiled = compile(
//!     "//@version=6\nindicator(\"Artifact\")\nplot(close * 2)",
//!     CompileOptions::default(),
//! );
//! let program = compiled.program.expect("example source compiles");
//! let artifact = compile_standalone_program(&program)
//!     .expect("the example is inside the standalone native subset");
//! assert_eq!(&artifact.wasm[..4], b"\0asm");
//! audit_standalone(&artifact.wasm).expect("emitted bytes satisfy the host gate");
//! ```

#![forbid(unsafe_code)]

#[cfg(feature = "aeneas")]
mod aeneas_roots_aot_prepare;
pub mod analysis;
mod body;
mod emit;
mod fraction;
mod hardened;
mod heap;
mod layout_footprint;
mod prepare_consts;
mod projection;
pub mod runtime;
pub mod schema;
mod slice;
mod standalone_scan;
/// Value/validity integer primitives; binary arithmetic is shared with native lowering.
pub mod tagged_integer;

use std::collections::BTreeSet;

use pinewasm_csil::{CsilInstr, CsilProgram, CSIL_VERSION, MAX_WINDOW_LENGTH};
use pinewasm_hir::{BuiltinSeries, DrawKind, InputKind, InputValue, InputWidget};
use thiserror::Error;

/// Header field byte offsets in linear memory (little-endian). The header
/// is the loader's window into the module: `prepare` fills it, `run`
/// reports through it. The input table (8 bytes per input, encoded according
/// to the declaration kind) starts at [`header::SIZE`].
pub mod header {
    /// `b"CSAW"` little-endian.
    pub const MAGIC: u32 = u32::from_le_bytes(*b"CSAW");
    /// Bump on any layout/contract change. Version 3 changed int/time/enum
    /// input cells from lossy f64 encoding to exact little-endian i64.
    /// Version 4 added the native heap-budget cells (`OFF_MAX_ELEMS` /
    /// `OFF_MAX_OBJS`) and the `HEAP_REROUTE` run status. Version 5 replaced
    /// the `host.alert` import with the alert output lane
    /// (`OFF_ALERTS_BASE` / `OFF_ALERT_COUNT`) and the `host.pure` import
    /// with the linked numeric runtime, which also moved the first lane above
    /// the runtime's reserved block. Version 6 appended the `fill(...)` color
    /// lanes to the color region: the flat lane order is now plots, visuals,
    /// then fills, and a v5 reader would stop at the visuals and drop every
    /// band's color silently.
    pub const ABI_VERSION: u32 = 6;

    pub const OFF_MAGIC: u32 = 0x00;
    pub const OFF_ABI_VERSION: u32 = 0x04;
    pub const OFF_BARS: u32 = 0x08;
    pub const OFF_RING_MASK: u32 = 0x0C;
    pub const OFF_BUILTIN_BASE: u32 = 0x10;
    pub const OFF_SLOTS_BASE: u32 = 0x14;
    pub const OFF_PLOTS_BASE: u32 = 0x18;
    pub const OFF_HEAP_END: u32 = 0x1C;
    pub const OFF_ERR_CODE: u32 = 0x20;
    pub const OFF_ERR_BAR: u32 = 0x24;
    pub const OFF_ERR_PC: u32 = 0x28;
    /// u32: base of the packed-color lane region.
    pub const OFF_COLORS_BASE: u32 = 0x2C;
    /// u64: per-bar instruction budget; `prepare` defaults it to the
    /// interpreter's default, the loader may overwrite before `run`.
    pub const OFF_BUDGET_PER_BAR: u32 = 0x30;
    /// f64: `syminfo.mintick` for `math.round_to_mintick`; loader-written.
    pub const OFF_MINTICK: u32 = 0x38;
    /// u32: base of the 32-byte scratch region (ta tuple extras land at
    /// scratch+0 and scratch+8, written by the linked engine).
    pub const OFF_SCRATCH_BASE: u32 = 0x40;
    /// u64: run-wide live collection-element budget (native heap);
    /// `prepare` defaults it to the VM's documented default, the loader may
    /// overwrite from `RunOptions.maxCollectionElements` before `run`.
    pub const OFF_MAX_ELEMS: u32 = 0x48;
    /// u64: run-wide live heap-object (arena slot) budget.
    pub const OFF_MAX_OBJS: u32 = 0x50;
    /// u32: base of the alert output lane — one `(bar: u32, message_id: i32)`
    /// pair per fired `alertcondition`, in execution order, `-1` = na = empty
    /// message. Replaces the `host.alert` import: a side channel that only
    /// ever appended is an output lane, and making it one removed a reason for
    /// artifacts to need a host at all.
    pub const OFF_ALERTS_BASE: u32 = 0x58;
    /// u32: how many pairs `run` wrote to the alert lane.
    pub const OFF_ALERT_COUNT: u32 = 0x5C;
    pub const SIZE: u32 = 0x60;
    /// Relative to `OFF_SCRATCH_BASE`: u64 cumulative instruction budget.
    /// `prepare` writes the default and a loader may overwrite it before
    /// `run`. The value is copied into a local before `host.vm` reuses this
    /// same cell for its per-call per-bar budget exchange, so ABI v6's header
    /// layout remains unchanged.
    pub const SCRATCH_OFF_BUDGET_PER_RUN: u32 = 0;
    /// Bytes per alert-lane entry.
    pub const ALERT_ENTRY: u32 = 8;
}

/// Status codes returned by `prepare`/`run` (and stored at
/// [`header::OFF_ERR_CODE`] by `run`).
pub mod status {
    pub const OK: i32 = 0;
    /// `run`: a bar exceeded the per-bar instruction budget
    /// (`RuntimeErrorKind::InstructionLimitExceeded`).
    pub const BUDGET_EXCEEDED: i32 = 1;
    /// `prepare`: `memory.grow` refused the requested size.
    pub const ALLOC_FAILED: i32 = 2;
    /// `prepare`: `bars` exceeds this module's exact wasm32 layout bound.
    pub const TOO_MANY_BARS: i32 = 3;
    /// `run`: a dynamic history offset exceeded `max_bars_back`
    /// (`RuntimeErrorKind::HistoryBufferLimitExceeded`).
    pub const HISTORY_LIMIT: i32 = 4;
    /// `run`: a value did not fit its static cell. The interpreter host
    /// returns it from a `host.vm` site, and native code raises it where an
    /// integer operation produced a present `i64::MIN`, the bit pattern the
    /// `I64` lane reserves for `na`. The loader reruns the request on the
    /// interpreter.
    pub const ABI_MISMATCH: i32 = 6;
    /// `run`: the linked `ta.*` engine refused the artifact's own declaration
    /// table. Unreachable for a module this compiler emitted — the table is
    /// written from the same declarations the engine validates — so it means
    /// the module's data segment was corrupted between emission and the call.
    pub const TA_INIT_FAILED: i32 = 8;
    /// `run`: a native heap operation detected an error condition (bounds,
    /// na index/receiver, empty receiver, element/object budget, or a
    /// linear-cost budget abort). The interpreter reproduces the exact
    /// structured error, so the loader reruns the request there — like
    /// `ABI_MISMATCH` (6), which the interpreter host owns. The host
    /// returns it too, from a `host.vm` site, when its own heap budget is
    /// spent: a module without a collector never collects, so its count can
    /// run ahead of the interpreter's.
    pub const HEAP_REROUTE: i32 = 7;
    /// `run`: the cumulative historical + realtime instruction budget was
    /// exhausted (`RuntimeErrorKind::RunInstructionLimitExceeded`).
    pub const RUN_BUDGET_EXCEEDED: i32 = 9;
    /// `run`: a `host.vm` site was reached when its data-dependent cost could
    /// not be bounded against both remaining counters. The loader reruns on
    /// the interpreter to preserve the exact structured error and effects.
    pub const RUN_BUDGET_REROUTE: i32 = 10;
    /// `run`: the linked broker emulator reported an error — a cap breach or
    /// an invalid risk directive. The blob wrote the precise message to the
    /// standalone WebAssembly error slot before returning; the glue's status mapping preserves it.
    pub const BROKER_FAILED: i32 = 11;
    /// `run`: an input-backed TA parameter is outside its legal range.
    pub const INVALID_TA_LENGTH: i32 = 12;
    /// Standalone WebAssembly: an executed `request.security` selected a
    /// timeframe the module cannot resample (a calendar week or month, or
    /// one out of range).
    pub const SECURITY_TIMEFRAME_UNAVAILABLE: i32 = 13;
    /// Standalone WebAssembly: an executed compound `request.security` has a
    /// `ta.*` length outside 1..=100000.
    pub const SECURITY_LENGTH_OUT_OF_RANGE: i32 = 14;
    /// Standalone WebAssembly: a whole number that f64 cannot hold exactly
    /// met a kept `int / int` quotient. The module carries those values in
    /// f64 (`fraction.rs`), so it stops rather than return a number the
    /// interpreter would not. Possible only from 2^53 up.
    pub const INT_FRACTION_RANGE: i32 = 15;
    /// Standalone WebAssembly: an executed `request.security` or
    /// `time(timeframe)` groups bars into buckets whose start the request's
    /// session leaves unsettled (the interpreter refuses them too).
    pub const SECURITY_SESSION_UNSETTLED: i32 = 16;
    /// Standalone WebAssembly: an executed `request.security` or
    /// `time(timeframe)` groups bars on an exchange clock that is not a
    /// fixed offset; the module carries no time zone rules.
    pub const SECURITY_TIMEZONE_UNSUPPORTED: i32 = 17;
}

/// Reserved `host.vm` site ids of a module's collection (docs/aot.md §13).
/// Real sites index the module's site table, which never comes near them.
/// Neither call reads or writes scratch cell 0, the per-bar budget cell.
pub mod host_gc_site {
    /// `cells[1]` = n, `cells[2..2 + n]` = host references the module
    /// holds. The host keeps them as roots for the next [`COLLECT`].
    pub const ROOTS: u32 = 0x7FFF_FFFE;
    /// Collect the host heap from the roots sent since the last collection
    /// plus the drawing registry; then `cells[1]` = its live elements and
    /// `cells[2]` = its live objects.
    pub const COLLECT: u32 = 0x7FFF_FFFF;
    /// A host-only module requests a sweep only under allocation pressure.
    pub const COLLECT_IF_DUE: u32 = 0x7FFF_FFFD;
}

/// Loop-bound native-heap sites required before the run path takes the
/// native tier for a module that still imports `host.vm` (docs/aot.md §13).
/// Below this, residual per-bar host hops and per-run setup outweigh the
/// dispatch win on trivial-compute scripts.
pub const HEAP_HOT_LOOP_SITES: u32 = 4;

/// Native heap-budget defaults, mirrored from `pinewasm_mir::heap` (the
/// loader overwrites the element cell from `RunOptions`).
pub mod heap_defaults {
    pub const MAX_ELEMENTS: u64 = 100_000;
    pub const MAX_OBJECTS: u64 = 1_000_000;
}

/// Mapping from the compiler's pure functions to [`pinewasm_num`]'s id
/// space, which is what the linked runtime implements.
///
/// The ids outlived the `host.pure` import they were invented for: the
/// interpreter-hosted tier still speaks them, and keeping one numbering means
/// the emitter, the runtime module, and the interpreter index the same table.
pub mod host_ids {
    use pinewasm_hir::PureFunc;

    /// The pure id for a function whose lowering is a call into the linked
    /// runtime. `MathRound` here is exclusively its two-argument form (the
    /// 1-arg form compiles inline to `f64.nearest`-style instructions).
    pub fn pure_host_id(func: PureFunc) -> Option<u32> {
        use pinewasm_num::id;
        Some(match func {
            PureFunc::MathExp => id::EXP,
            PureFunc::MathLog => id::LOG,
            PureFunc::MathLog10 => id::LOG10,
            PureFunc::MathSin => id::SIN,
            PureFunc::MathCos => id::COS,
            PureFunc::MathTan => id::TAN,
            PureFunc::MathAsin => id::ASIN,
            PureFunc::MathAcos => id::ACOS,
            PureFunc::MathAtan => id::ATAN,
            PureFunc::MathPow => id::POW,
            PureFunc::MathRound => id::ROUND2,
            _ => return None,
        })
    }

    /// Inverse of [`pure_host_id`], for the engine side.
    pub fn pure_from_host_id(id: u32) -> Option<PureFunc> {
        use pinewasm_num::id as n;
        Some(match id {
            n::EXP => PureFunc::MathExp,
            n::LOG => PureFunc::MathLog,
            n::LOG10 => PureFunc::MathLog10,
            n::SIN => PureFunc::MathSin,
            n::COS => PureFunc::MathCos,
            n::TAN => PureFunc::MathTan,
            n::ASIN => PureFunc::MathAsin,
            n::ACOS => PureFunc::MathAcos,
            n::ATAN => PureFunc::MathAtan,
            n::POW => PureFunc::MathPow,
            n::ROUND2 => PureFunc::MathRound,
            _ => return None,
        })
    }
}

/// Builtin lane order inside the builtin region (each `bars × 8` bytes).
/// Time is stored as f64 (epoch ms are ≪ 2^53, so the conversion is
/// exact); `input.source` lanes follow at indices 6+.
pub const BUILTIN_LANES: [BuiltinSeries; 6] = [
    BuiltinSeries::Open,
    BuiltinSeries::High,
    BuiltinSeries::Low,
    BuiltinSeries::Close,
    BuiltinSeries::Volume,
    BuiltinSeries::Time,
];

/// Scratch-cell encodings for `host.vm` operands/results. Must mirror
/// `pinewasm_vm::aot_host::tag` (asserted by the differential tests).
pub mod cell_tag {
    pub const F64: u8 = 0;
    pub const I64: u8 = 1;
    pub const BOOL3: u8 = 2;
    pub const COLOR: u8 = 3;
    pub const STR: u8 = 4;
    pub const REF: u8 = 5;
    /// Result-only: the value is immediately popped (a statement-position
    /// mutator like `array.shift(a)`), so the host encodes na regardless of
    /// the runtime type instead of aborting on a cell mismatch.
    pub const DISCARD: u8 = 6;
}

/// One `host.vm(site_id, bar)` call site: which instruction it executes and
/// the static cell encodings of its operands (argument order) and result.
/// `site_id` is the index into [`Layout::host_sites`]; the emitted module
/// and the host both derive this list from the same program analysis, so
/// they agree by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSiteSpec {
    pub pc: u32,
    pub args: Vec<u8>,
    pub ret: Option<u8>,
}

fn vty_tag(ty: analysis::VTy) -> u8 {
    match ty {
        analysis::VTy::F64 => cell_tag::F64,
        analysis::VTy::I64 => cell_tag::I64,
        analysis::VTy::Bool3 => cell_tag::BOOL3,
        analysis::VTy::Color => cell_tag::COLOR,
        analysis::VTy::Str => cell_tag::STR,
        analysis::VTy::Ref => cell_tag::REF,
    }
}

/// Emit-time layout facts the loader needs. Region bases depend on the
/// runtime bar count and are read from the header after `prepare`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Compiler-owned fixed style cells. Offsets are relative to
    /// `OFF_SLOTS_BASE`; each cell uses its own history mask and the same
    /// typed encoding as `host.vm`. Published together with the module so
    /// a loader never guesses slot storage or reevaluates Pine expressions.
    pub style_slots: Vec<NativeStyleSlot>,
    /// Whether `run` contains native CSIL lowering or delegates the complete
    /// run to the interpreter through `host.interpret`.
    pub execution_mode: ExecutionMode,
    /// The native-AOT blocker that selected hosted mode. Kept as build
    /// metadata so UIs and coverage tooling can report the remaining native
    /// frontier without making the script non-exportable.
    pub native_blocker: Option<String>,
    /// The deepest user series ring, in cells (power of two ≥ every literal
    /// history offset + 1, every `WINDOW_MEAN` length, and `max_bars_back + 1`),
    /// published as the header's `ring_mask + 1`. Each slot's own ring is
    /// planned separately and is often smaller (`emit::SlotLayout`).
    pub ring: u32,
    pub slot_count: u32,
    pub plot_count: u32,
    /// Peak operand-stack depth of `main` (the validator's `max_stack`). The
    /// loader compares it against `RunOptions.maxStackDepth`: the interpreter
    /// aborts with a stack-overflow error when the peak exceeds that limit
    /// (`Vm::push`), so a lower limit forces an interpreter fallback.
    pub max_stack: u32,
    /// Declared color-role count per plot slot (styled plots).
    pub plot_color_roles: Vec<u32>,
    /// Declared color-role count per visual slot.
    pub visual_color_roles: Vec<u32>,
    /// Declared color-role count per `fill(...)` band slot. Third and last
    /// family in the flat color-lane order (plots, then visuals, then fills);
    /// every producer and reader of the color region walks the three in that
    /// sequence.
    pub fill_color_roles: Vec<u32>,
    /// Input ids of `input.source` declarations, in lane order (lane 6+j).
    pub source_inputs: Vec<u32>,
    /// Same-symbol `request.security` lanes (standalone WebAssembly only),
    /// laid out after the source lanes: lane `6 + source_inputs.len() + k`.
    /// The runtime fills them from the bars before the run (see
    /// `projection::SecuritySpec`).
    pub security_lanes: u32,
    /// Whether the module links the stateful `ta.*` engine (`CallTa` sites).
    /// Not an import: the engine's functions are spliced into the module the
    /// same way the numeric runtime's are.
    pub links_ta: bool,
    /// The declaration table the artifact carries as a data segment, one
    /// entry per `ta_states` entry. Shorter than `ta_states` only when a
    /// declaration is too wide for [`pinewasm_ta::MAX_PARAMS`], which
    /// emission refuses rather than truncating.
    pub ta_decls: Vec<pinewasm_ta::Decl>,
    /// Engine region plan for [`Layout::ta_decls`]: states, ring cells, and
    /// the shared sorting scratch. `prepare` reserves
    /// `ta_layout.region_bytes()` and the linked engine lays the same
    /// regions out inside it.
    pub ta_layout: pinewasm_ta::Layout,
    /// Whether the module links the strategy broker emulator (a
    /// strategy()-sourced standalone WebAssembly module). Like [`Layout::links_ta`],
    /// not an import: the emulator is spliced from the broker blob.
    pub links_broker: bool,
    /// Whether a strategy()-sourced chart module's broker is the interpreter
    /// host's: its orders, `strategy.*` getters, trade accessors and
    /// conversions are `host.vm` sites, and the host returns the run's
    /// strategy results after the run (`VmHost::drain_strategy`).
    pub host_broker: bool,
    /// Whether the module links the numeric runtime ([`runtime`]) for
    /// transcendentals and `%`. Not an import: the runtime's functions are
    /// spliced into the module itself.
    pub links_runtime: bool,
    /// Which [`pinewasm_num`] ids the program actually calls. The linked
    /// runtime keeps only the bodies these reach; the rest are stubbed, which
    /// matters because its code section is 93% of its ~15.4 KiB.
    pub used_pure_ids: [bool; pinewasm_num::id::COUNT as usize],
    /// `alertcondition` sites writing to the alert output lane, which sizes
    /// the lane at `bars × alert_sites` entries. Zero when the program has
    /// `host.vm` sites, because alertconditions then join that stream to keep
    /// one execution-ordered record of every effect.
    pub alert_sites: u32,
    /// `host.vm(site_id, bar) -> status` — the interpreter host executing
    /// heap/string/effect/request opcodes per [`Layout::host_sites`].
    pub imports_vm: bool,
    /// Native heap (docs/aot.md §13): collections/UDTs live in the module's
    /// linear memory; no heap opcode crosses `host.vm`.
    pub native_heap: bool,
    /// The native heap is LOOP-bound (≥ [`HEAP_HOT_LOOP_SITES`] sites inside
    /// natural loops): the compute-heavy shape where native execution
    /// amortizes residual cold host hops. The run-path policy requires this
    /// before taking the native tier for a module that still imports
    /// `host.vm` (drawing/log-dense scripts with trivial compute can be
    /// slower natively, while compute-heavy collection loops benefit).
    pub native_heap_hot: bool,
    /// Ordered Rust search kernels in the native body. Their internal work
    /// is not represented by the static residual-host instruction share.
    pub distance_scan_kernels: u32,
    /// The program has collection/UDT opcodes that still execute on the
    /// interpreter host (the hot-hop class). The run-path policy
    /// keeps the interpreter for these; cold host sites (drawings, strings,
    /// effects, requests) alone no longer decline native runs.
    pub vm_heap_residual: bool,
    /// `host.interpret() -> status`, used only by universal hosted modules.
    pub imports_interpreter: bool,
    /// `host.vm` call sites in site-id order (ascending pc).
    pub host_sites: Vec<HostSiteSpec>,
    /// How many of [`Layout::host_sites`] are inside a loop. A site outside
    /// every loop runs at most once a bar, so a module with none in a loop
    /// calls its host at most `host_sites.len()` times a bar; a site inside
    /// one may run on every iteration. An embedding host uses this to bound
    /// what a module's host calls can cost before running it.
    pub host_sites_in_loops: u32,
    /// Scratch region size in 8-byte cells (cell 0 = budget I/O, cells 1..
    /// = operands/result; also covers the ta tuple-extra cells).
    pub scratch_cells: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeStyleSlot {
    pub slot: u32,
    pub tag: u8,
    pub offset: u32,
    pub mask: u32,
}

/// Execution tier of an emitted WebAssembly module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    /// CSIL was lowered instruction-by-instruction to WebAssembly.
    Native,
    /// The WebAssembly entry point calls the ABI host's interpreter.
    InterpreterHosted,
}

impl Layout {
    /// Ring index mask (`ring − 1`).
    pub fn mask(&self) -> u32 {
        self.ring - 1
    }
}

/// A compiled per-script module.
#[derive(Debug, Clone)]
pub struct AotModule {
    pub wasm: Vec<u8>,
    pub layout: Layout,
}

/// Why a program cannot be AOT-compiled (yet). The caller falls back to the
/// interpreter; the message names the first blocker for diagnostics.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AotError {
    #[error("unsupported opcode for AOT: {0}")]
    UnsupportedOpcode(String),
    #[error("unsupported builtin series for AOT: {0:?}")]
    UnsupportedBuiltin(BuiltinSeries),
    #[error("unsupported input kind for AOT: {0}")]
    UnsupportedInputKind(String),
    #[error("strategy metadata requires the interpreter-hosted Wasm tier")]
    StrategyRequiresInterpreter,
    #[error("strategy feature unsupported for standalone WebAssembly: {feature}")]
    StrategyFeatureUnsupported { feature: String },
    #[error("AOT supports single-function programs, found {0}")]
    MultipleFunctions(usize),
    #[error("program has no functions")]
    NoFunctions,
    #[error("CSIL version {0} does not match the AOT backend's {expected}", expected = CSIL_VERSION)]
    CsilVersionMismatch(u32),
    #[error("irreducible control flow (not producible by this compiler)")]
    IrreducibleControlFlow,
    #[error(
        "native AOT cannot preserve a valid i64::MIN at instruction {pc}; use the interpreter tier"
    )]
    UnsupportedIntegerNaEncoding { pc: usize },
    #[error("type conflict: {0}")]
    TypeConflict(String),
    #[error("program failed validation: {0}")]
    InvalidProgram(String),
    #[error("standalone emit requires a zero-import module: {0}")]
    NotStandalone(String),
    #[error("standalone WebAssembly export requires a standalone-eligible program: {0}")]
    StandaloneWasmNotStandalone(String),
    #[error("emitted standalone WebAssembly module fails its export-surface audit: {0}")]
    StandaloneWasmExportSurface(String),
    #[error("emitted standalone WebAssembly module fails its metadata audit: {0}")]
    StandaloneWasmMetadata(String),
    #[error("standalone WebAssembly export requires a designated signal plot: {0}")]
    StandaloneWasmNoDesignatedPlot(String),
    #[error("standalone WebAssembly export does not support this input.source declaration: {0}")]
    StandaloneWasmSourceInputUnsupported(String),
    #[error("invalid standalone WebAssembly build option: {0}")]
    StandaloneWasmInvalidOptions(String),
    #[error("this compiler build does not link the chart runtime")]
    ChartRuntimeUnavailable,
}

/// What a refusal says about the program and about the backend.
///
/// The backend may decline a valid program for a declared reason, and a host
/// then reports that reason. It must never decline one because its own
/// analysis contradicted itself. The two cases look the same to a caller that
/// only reads the message, so the class is part of the error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RefusalClass {
    /// A declared limit of the backend or of the requested build: an opcode,
    /// builtin, input kind or strategy feature it does not lower, a value
    /// representation it cannot preserve, a module that would need host
    /// imports, or a build option the caller must change.
    Capability,
    /// Not reachable for a program this compiler's front end produced. For
    /// such a program it is a compiler defect. For a hand-built, stale or
    /// tampered program it is an ordinary rejection.
    Internal,
}

impl AotError {
    /// The class of this refusal. The match is exhaustive on purpose: a new
    /// variant must be classified before the crate compiles.
    #[must_use]
    pub fn class(&self) -> RefusalClass {
        match self {
            AotError::UnsupportedOpcode(_)
            | AotError::UnsupportedBuiltin(_)
            | AotError::UnsupportedInputKind(_)
            | AotError::StrategyRequiresInterpreter
            | AotError::StrategyFeatureUnsupported { .. }
            | AotError::UnsupportedIntegerNaEncoding { .. }
            | AotError::NotStandalone(_)
            | AotError::StandaloneWasmNotStandalone(_)
            | AotError::StandaloneWasmNoDesignatedPlot(_)
            | AotError::StandaloneWasmSourceInputUnsupported(_)
            | AotError::StandaloneWasmInvalidOptions(_)
            | AotError::ChartRuntimeUnavailable => RefusalClass::Capability,
            AotError::MultipleFunctions(_)
            | AotError::NoFunctions
            | AotError::CsilVersionMismatch(_)
            | AotError::IrreducibleControlFlow
            | AotError::TypeConflict(_)
            | AotError::InvalidProgram(_)
            | AotError::StandaloneWasmExportSurface(_)
            | AotError::StandaloneWasmMetadata(_) => RefusalClass::Internal,
        }
    }

    /// The variant name, without its payload. Stable across programs, so
    /// refusals can be counted by kind.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            AotError::UnsupportedOpcode(_) => "UnsupportedOpcode",
            AotError::UnsupportedBuiltin(_) => "UnsupportedBuiltin",
            AotError::UnsupportedInputKind(_) => "UnsupportedInputKind",
            AotError::StrategyRequiresInterpreter => "StrategyRequiresInterpreter",
            AotError::StrategyFeatureUnsupported { .. } => "StrategyFeatureUnsupported",
            AotError::MultipleFunctions(_) => "MultipleFunctions",
            AotError::NoFunctions => "NoFunctions",
            AotError::CsilVersionMismatch(_) => "CsilVersionMismatch",
            AotError::IrreducibleControlFlow => "IrreducibleControlFlow",
            AotError::UnsupportedIntegerNaEncoding { .. } => "UnsupportedIntegerNaEncoding",
            AotError::TypeConflict(_) => "TypeConflict",
            AotError::InvalidProgram(_) => "InvalidProgram",
            AotError::NotStandalone(_) => "NotStandalone",
            AotError::StandaloneWasmNotStandalone(_) => "StandaloneWasmNotStandalone",
            AotError::StandaloneWasmExportSurface(_) => "StandaloneWasmExportSurface",
            AotError::StandaloneWasmMetadata(_) => "StandaloneWasmMetadata",
            AotError::StandaloneWasmNoDesignatedPlot(_) => "StandaloneWasmNoDesignatedPlot",
            AotError::StandaloneWasmSourceInputUnsupported(_) => {
                "StandaloneWasmSourceInputUnsupported"
            }
            AotError::StandaloneWasmInvalidOptions(_) => "StandaloneWasmInvalidOptions",
            AotError::ChartRuntimeUnavailable => "ChartRuntimeUnavailable",
        }
    }
}

/// Check whether the whole program is inside the supported subset without
/// emitting anything. `Ok` means [`compile_program`] will succeed.
///
/// Runs the full CSIL validator first: the emitter trusts slot/plot indices
/// and stack discipline the way the interpreter does *after* validation,
/// and — like `runProgram`'s re-validation — a tampered artifact must
/// degrade to a structured fallback, never a panic/trap.
pub fn supports(program: &CsilProgram) -> Result<(), AotError> {
    plan_native(program).map(drop)
}

fn preflight(program: &CsilProgram) -> Result<(), AotError> {
    preflight_shared(program)?;
    // A chart-tier strategy keeps the interpreter's own broker on the
    // interpreter host, which returns the strategy results the result ABI
    // does not carry. The module executes the script once per bar close and
    // the host replays the interpreter's per-bar broker work around it, so a
    // declaration that executes the script again after fills or on every
    // historical tick, or skips early bars, keeps the interpreter. The host
    // checks the same settings again after run-time overrides. (The
    // standalone WebAssembly tier links the broker instead and takes
    // [`preflight_standalone_wasm`].)
    if let Some(info) = &program.metadata.strategy {
        if info.calc_on_order_fills || info.calc_on_every_history_tick || info.calc_bars_count > 0 {
            return Err(AotError::StrategyRequiresInterpreter);
        }
    }
    single_function(program)
}

/// [`compile_standalone_wasm_program`]'s preflight: strategies are admitted — the broker
/// rides inside the module — with per-feature gates replacing the blanket
/// refusal. Every named refusal is a
/// [`AotError::StrategyFeatureUnsupported`] so a host can report exactly
/// which declaration argument keeps the script out.
fn preflight_standalone_wasm(program: &CsilProgram) -> Result<(), AotError> {
    preflight_shared(program)?;
    if let Some(info) = &program.metadata.strategy {
        let feature = |feature: &str| AotError::StrategyFeatureUnsupported {
            feature: feature.to_string(),
        };
        // Requires intra-bar script re-execution with state rollback, an
        // emitter structural change, deferred.
        if info.calc_on_order_fills {
            return Err(feature("calc_on_order_fills"));
        }
        // Same structural gap on the history path: the interpreter replays
        // the synthetic OHLC ticks of every historical bar, and the emitted
        // loop executes once per bar close. Accepting it would export a
        // module with silently different posture events.
        if info.calc_on_every_history_tick {
            return Err(feature("calc_on_every_history_tick"));
        }
        // The wire carries no FX lane. Explicit currencies are admitted only
        // with a runtime guard requiring an identical dataset quote currency. (`calc_on_every_tick` is accepted-and-inert:
        // it only affects realtime ticks, which standalone WebAssembly evaluation does not
        // have.)
        if !info.currency.is_empty()
            && info.currency != "NONE"
            && (info.currency.len() != 3 || !info.currency.bytes().all(|b| b.is_ascii_uppercase()))
        {
            return Err(feature("currency"));
        }
        // `calc_bars_count` skips script execution (but not equity
        // sampling) for a leading window — a bar-loop shape change the standalone WebAssembly
        // emitter does not model yet.
        if info.calc_bars_count > 0 {
            return Err(feature("calc_bars_count"));
        }
    }
    single_function(program)
}

fn preflight_shared(program: &CsilProgram) -> Result<(), AotError> {
    if program.header.csil_version != CSIL_VERSION {
        return Err(AotError::CsilVersionMismatch(program.header.csil_version));
    }
    if let Err(errors) = pinewasm_csil::validate(program) {
        let first = errors.first().map_or_else(
            || "unknown validation error".to_string(),
            std::string::ToString::to_string,
        );
        return Err(AotError::InvalidProgram(first));
    }
    if program.functions.iter().any(|function| {
        function.code.iter().any(
            |instr| matches!(instr, CsilInstr::DeleteDrawing { kind } if *kind == DrawKind::Table),
        )
    }) {
        // Native chart slots live in module memory, outside the interpreter
        // that executes drawing host sites. Until AOT can rewrite every
        // direct and collection alias in that memory, the whole program must
        // take the interpreter path for table.delete alias invalidation.
        return Err(AotError::UnsupportedOpcode(
            "table.delete alias invalidation requires interpreter".into(),
        ));
    }
    let mut table_registrations = Vec::new();
    for (function_index, function) in program.functions.iter().enumerate() {
        for (pc, instr) in function.code.iter().enumerate() {
            if matches!(instr, CsilInstr::RegisterDrawing { kind } if *kind == DrawKind::Table) {
                table_registrations.push((function_index, pc));
            }
        }
    }
    if table_registrations.len() > 1
        || table_registrations.first().is_some_and(|&(function, pc)| {
            !table_register_is_one_shot_initializer(program, function, pc)
        })
    {
        // The interpreter replaces a table at a repeated position and makes
        // aliases to the displaced id `na`. AOT module slots live outside that
        // VM. Keep the native path for a single persistent initializer, where
        // no replacement can occur, and rerun all other table-construction
        // patterns through the interpreter until the module can rewrite its
        // direct and collection aliases too. More than one constructor site
        // is conservatively refused even when its literal positions differ:
        // this preflight does not prove pairwise-disjoint positions.
        return Err(AotError::UnsupportedOpcode(
            "table position replacement requires interpreter".into(),
        ));
    }
    Ok(())
}

/// Whether a table constructor is protected by its persistent slot's
/// run-once initializer guard. A constructor without this shape may execute
/// on later bars, in a loop, or from a repeatedly called helper and therefore
/// may replace a table while AOT-owned aliases remain in module memory.
fn table_register_is_one_shot_initializer(
    program: &CsilProgram,
    function_index: usize,
    pc: usize,
) -> bool {
    let Some(function) = program.functions.get(function_index) else {
        return false;
    };
    let Some(CsilInstr::StoreUserSlotCurrent { slot }) = function.code.get(pc + 1) else {
        return false;
    };
    if !program
        .series_slots
        .get(*slot as usize)
        .is_some_and(|definition| definition.persistent)
    {
        return false;
    }
    function.code.iter().enumerate().any(|(guard_pc, instr)| {
        matches!(
            instr,
            CsilInstr::JumpIfSlotWritten {
                slot: guard_slot,
                target,
            } if *guard_slot == *slot && guard_pc < pc && *target as usize > pc + 1
        )
    })
}

fn single_function(program: &CsilProgram) -> Result<(), AotError> {
    match program.functions.len() {
        0 => Err(AotError::NoFunctions),
        1 => Ok(()),
        n => Err(AotError::MultipleFunctions(n)),
    }
}

/// Compute the [`Layout`] for an analyzable program.
fn plan_layout(program: &CsilProgram, analysis: &analysis::Analysis) -> Layout {
    let mut deepest: u32 = program.metadata.max_bars_back;
    let mut links_ta = false;
    let mut links_runtime = false;
    let mut used_pure_ids = [false; pinewasm_num::id::COUNT as usize];
    let mut alert_sites = 0u32;
    for instr in &program.main().code {
        match instr {
            CsilInstr::ReadUserSlot { offset, .. } => deepest = deepest.max(*offset),
            CsilInstr::WindowMean { length, .. } => deepest = deepest.max(length.saturating_sub(1)),
            CsilInstr::CallTa { .. } => links_ta = true,
            CsilInstr::AlertCondition => alert_sites += 1,
            CsilInstr::F64Rem => {
                links_runtime = true;
                used_pure_ids[pinewasm_num::id::REM as usize] = true;
            }
            CsilInstr::CallPure { func, argc } => {
                if matches!(
                    analysis::pure_plan(*func, *argc),
                    Some(analysis::PurePlan::HostF64 | analysis::PurePlan::Round2)
                ) {
                    links_runtime = true;
                    // An unmapped func would be rejected later by `pure_fn`;
                    // marking nothing here is safe because the body emitter
                    // errors rather than calling a stub.
                    if let Some(id) = crate::host_ids::pure_host_id(*func) {
                        used_pure_ids[id as usize] = true;
                    }
                }
            }
            _ => {}
        }
    }

    // The engine's declaration table, and the region it needs. A `CallTa`
    // program links the runtime for it even when it touches no
    // transcendental: the engine ships in the same blob.
    let ta_decls: Vec<pinewasm_ta::Decl> = program
        .ta_states
        .iter()
        .filter_map(|decl| pinewasm_ta::Decl::new(decl.func, &decl.params))
        .collect();
    // `prepare` runs before the host fills its input table. Reserve the legal
    // upper bounds; `run` installs actual parameters before engine init. Both
    // the runtime and emitter use the same monotone engine layout planner.
    let mut upper_decls = program.ta_states.clone();
    for binding in &program.ta_input_bindings {
        let Some((_, max)) = program.ta_input_bounds(binding) else {
            // Float multipliers/percentages change results but do not change
            // state allocation or history depth.
            continue;
        };
        match binding.target {
            pinewasm_hir::TaInputTarget::State { state, param } => {
                upper_decls[state as usize].params[param as usize] = max;
            }
            pinewasm_hir::TaInputTarget::Sma { .. } => deepest = deepest.max(max as u32 - 1),
            pinewasm_hir::TaInputTarget::Change { .. } => deepest = deepest.max(max as u32),
        }
    }
    let upper_decls: Vec<_> = upper_decls
        .iter()
        .filter_map(|d| pinewasm_ta::Decl::new(d.func, &d.params))
        .collect();
    let ta_layout = pinewasm_ta::Layout::plan(&upper_decls);
    links_runtime |= links_ta;

    // host.vm sites in ascending pc. When any exist, `alertcondition` joins
    // them (one host-recorded alert stream in execution order) and the alert
    // output lane is dropped; a program whose only effectful surface is
    // `alertcondition` keeps the lane and stays import-free.
    let code = &program.main().code;
    let mut site_pcs: Vec<usize> = (0..code.len())
        .filter(|&pc| analysis.host_site.get(pc).copied().unwrap_or(false))
        .collect();
    if !site_pcs.is_empty() {
        site_pcs.extend(analysis.alertcondition_pcs.iter().copied());
        site_pcs.sort_unstable();
        site_pcs.dedup();
        alert_sites = 0;
    }
    let host_sites: Vec<HostSiteSpec> = site_pcs
        .iter()
        .map(|&pc| {
            let args: Vec<u8> = analysis.operand_tys[pc]
                .iter()
                .map(|&t| vty_tag(t))
                .collect();
            // Whether the site returns a value is the opcode's stack effect,
            // not whether analysis typed it: an untyped result still needs a
            // return cell (DISCARD or the f64 default below).
            let pushes_result = matches!(
                code[pc].resolve_stack_effect(&program.ta_states),
                Ok(effect) if effect.pushes > 0
            );
            let ret = if pushes_result {
                // A result the very next instruction pops is dead: any
                // runtime type encodes as na (statement-position mutators).
                // Otherwise, a result no consumer ever typed defaults to the
                // f64 cell (na crosses faithfully; a dynamic non-na value
                // that does not fit aborts into the interpreter via
                // ABI_MISMATCH).
                if matches!(code.get(pc + 1), Some(CsilInstr::Pop)) {
                    Some(cell_tag::DISCARD)
                } else {
                    Some(
                        analysis.result_tys[pc]
                            .first()
                            .map_or(cell_tag::F64, |&t| vty_tag(t)),
                    )
                }
            } else {
                None
            };
            HostSiteSpec {
                pc: pc as u32,
                args,
                ret,
            }
        })
        .collect();
    let imports_vm = !host_sites.is_empty();
    // Strategy modules that link the broker stage `CsilInstr::Order` operands
    // in the scratch region above the 4 reserved cells (budget I/O + the ta
    // tuple extras): 4 + the widest slot layout (`Exit`, 21). A host broker's
    // orders are host sites, sized like every other.
    let links_broker = analysis.strategy_broker == Some(analysis::StrategyBroker::Linked);
    let host_broker = analysis.strategy_broker == Some(analysis::StrategyBroker::Host);
    let scratch_floor = if links_broker { 25 } else { 4 };
    let scratch_cells = host_sites
        .iter()
        .map(|s| 1 + s.args.len().max(1) as u32)
        .max()
        .unwrap_or(0)
        .max(scratch_floor);

    Layout {
        style_slots: Vec::new(),
        execution_mode: ExecutionMode::Native,
        native_blocker: None,
        native_heap: analysis.heap.is_some(),
        native_heap_hot: analysis.heap.is_some() && analysis.heap_loop_sites >= HEAP_HOT_LOOP_SITES,
        distance_scan_kernels: program
            .main()
            .code
            .iter()
            .filter(|i| {
                matches!(
                    i,
                    CsilInstr::CallCollection {
                        func: pinewasm_csil::CollectionFunc::InternalDistanceScan,
                        ..
                    }
                )
            })
            .count() as u32,
        vm_heap_residual: analysis.heap_ops_present && analysis.heap.is_none(),
        // Defense in depth: `validate()` bounds every contributor to
        // `deepest` (max_bars_back <= MAX_BARS_BACK_LIMIT, literal offsets
        // and window lengths <= MAX_WINDOW_LENGTH), so a validated program
        // never reaches the clamp. It exists because the unclamped form
        // panics in debug and wraps to a degenerate ring in release once
        // `deepest` passes 2^31 — a wrong artifact is worse than a refusal,
        // and `plan_layout` has no error channel to refuse through.
        ring: (deepest.min(MAX_WINDOW_LENGTH) + 1).next_power_of_two(),
        slot_count: program.series_slots.len() as u32,
        plot_count: program.plots.len() as u32,
        max_stack: program.main().max_stack,
        plot_color_roles: program
            .plots
            .iter()
            .map(|p| p.color_roles.len() as u32)
            .collect(),
        visual_color_roles: program
            .visuals
            .iter()
            .map(|v| v.color_roles.len() as u32)
            .collect(),
        fill_color_roles: program
            .fills
            .iter()
            .map(|f| f.color_roles.len() as u32)
            .collect(),
        source_inputs: program
            .inputs
            .iter()
            .filter(|d| d.kind == InputKind::Source)
            .map(|d| d.id)
            .collect(),
        security_lanes: 0,
        links_ta,
        ta_decls,
        ta_layout,
        links_broker,
        host_broker,
        links_runtime,
        used_pure_ids,
        alert_sites,
        imports_vm,
        imports_interpreter: false,
        host_sites,
        host_sites_in_loops: analysis.host_loop_sites,
        scratch_cells,
    }
}

/// The `host.vm` site plan (specs + scratch size in cells) for a natively
/// lowerable program; `None` when native lowering is unavailable or the
/// program needs no interpreter host. The host derives its decode tables
/// from this — the same analysis emission uses, so module and host always
/// agree.
pub fn host_site_plan(program: &CsilProgram) -> Option<(Vec<HostSiteSpec>, u32)> {
    let layout = native_host_layout(program)?;
    if layout.host_sites.is_empty() {
        return None;
    }
    Some((layout.host_sites, layout.scratch_cells))
}

/// Native host facts from the identical analysis and slot planner used to
/// emit the module, including fixed styles for import-free modules.
pub fn native_host_layout(program: &CsilProgram) -> Option<Layout> {
    let mut plan = plan_native(program).ok()?;
    plan.name_sites_as_written();
    Some(plan.layout)
}

fn complete_native_layout(
    program: &CsilProgram,
    analysis: &analysis::Analysis,
) -> Result<Layout, AotError> {
    let mut layout = plan_layout(program, analysis);
    layout.style_slots = emit::native_style_slots(program, analysis, &layout)?;
    Ok(layout)
}

/// What the chart tier compiles: the program as written or, when a kept
/// Pine int quotient is all that keeps it out, the program with those
/// quotients carried as float code (`fraction.rs`).
///
/// [`compile_program`], [`supports`] and [`host_site_plan`] all start here,
/// so the module and the interpreter host that serves it number the same
/// sites.
struct NativePlan {
    carried: Option<fraction::Carried>,
    analysis: analysis::Analysis,
    /// Host sites are numbered against the compiled program until
    /// [`NativePlan::name_sites_as_written`].
    layout: Layout,
}

impl NativePlan {
    /// Point every host site at the program as written. The host holds
    /// that program and runs the instruction at the site's pc.
    fn name_sites_as_written(&mut self) {
        if let Some(carried) = &self.carried {
            for site in &mut self.layout.host_sites {
                // `plan_carried` checked every site's pc against `origin`.
                if let Some(origin) = carried.origin.get(site.pc as usize) {
                    site.pc = *origin as u32;
                }
            }
        }
    }
}

/// The program as written always goes first: a script this tier already
/// compiled compiles to the same module, and the carry only adds scripts.
/// The carry is tried when the typed pass refused a kept quotient, as in the
/// standalone compile; a program it cannot take keeps the refusal it was
/// written with.
fn plan_native(program: &CsilProgram) -> Result<NativePlan, AotError> {
    plan_native_with(program, false)
}

/// [`plan_native`], or the same plan for a module that links its host
/// (`linked_host`: [`analysis::analyze_chart`] in place of
/// [`analysis::analyze`]).
fn plan_native_with(program: &CsilProgram, linked_host: bool) -> Result<NativePlan, AotError> {
    preflight(program)?;
    let analyzed = if linked_host {
        analysis::analyze_chart(program)
    } else {
        analysis::analyze(program)
    };
    let plan = match analyzed {
        Ok(analysis) => {
            let layout = complete_native_layout(program, &analysis)?;
            NativePlan {
                carried: None,
                analysis,
                layout,
            }
        }
        Err(written) => {
            let kept_quotient = matches!(
                &written,
                AotError::UnsupportedOpcode(opcode) if opcode == "PineIntDiv"
            );
            if !kept_quotient {
                return Err(written);
            }
            plan_carried(program, linked_host).ok_or(written)?
        }
    };
    // A host broker lives in the interpreter host, and a module that
    // imports its host but never calls it has none to return the strategy
    // results from. A linked host is in the module whatever it calls.
    if plan.layout.host_broker && plan.layout.host_sites.is_empty() && !linked_host {
        return Err(AotError::StrategyRequiresInterpreter);
    }
    Ok(plan)
}

fn plan_carried(program: &CsilProgram, linked_host: bool) -> Option<NativePlan> {
    let carried = fraction::carry(program, &BTreeSet::new())?;
    let analysis = if linked_host {
        analysis::analyze_chart(&carried.program)
    } else {
        analysis::analyze(&carried.program)
    }
    .ok()?;
    let layout = complete_native_layout(&carried.program, &analysis).ok()?;
    if !layout.host_sites.is_empty() {
        // A host site runs the instruction as written on the interpreter's
        // own values. An instruction the carry inserted is not in that
        // program. A carried operand would reach the host as a float where
        // the interpreter may hold a whole int, and text formatting tells
        // the two apart. Either way the program stays on the interpreter.
        let flow = pinewasm_csil::flow::Flow::build(&carried.program)?;
        for site in &layout.host_sites {
            let pc = site.pc as usize;
            let origin = *carried.origin.get(pc)?;
            u32::try_from(origin).ok()?;
            let carried_operand = flow.operands.get(pc)?.iter().any(|producers| {
                producers
                    .iter()
                    .any(|producer| carried.float.contains(producer))
            });
            if carried.budget_free.contains(&pc) || carried_operand {
                return None;
            }
        }
    }
    Some(NativePlan {
        carried: Some(carried),
        analysis,
        layout,
    })
}

/// Compile a validated program to a standalone wasm module, or report the
/// first unsupported construct.
pub fn compile_program(program: &CsilProgram) -> Result<AotModule, AotError> {
    let mut plan = plan_native(program)?;
    let compiled = plan
        .carried
        .as_ref()
        .map_or(program, |carried| &carried.program);
    let wasm = emit::emit(
        compiled,
        &plan.layout,
        &plan.analysis,
        plan.carried.as_ref(),
    )?;
    plan.name_sites_as_written();
    Ok(AotModule {
        wasm,
        layout: plan.layout,
    })
}

/// Whether this compiler was built with the chart runtime, which
/// [`compile_chart_wasm_program`] links into the modules it emits.
#[must_use]
pub fn chart_runtime_linked() -> bool {
    runtime::runtime_chart().is_some()
}

/// Compile a chart module whose host is linked into it, so the module
/// imports nothing.
///
/// [`compile_program`] leaves a program's text, drawing, alert, order and
/// request instructions as `host.vm` sites, which the embedding host runs on
/// the interpreter. This entry point links that same host into the module
/// (`runtime/src/chart.rs`) and calls it directly, so the same instructions
/// run on the same handlers without an import. The module stores the
/// program and its site plan for that host, and exports `chartAlloc`,
/// `chartInit` and `chartCommand` beside `prepare` and `run` for the loader
/// to pass the run's request in and read the host's results back.
///
/// A linked host also keeps what the native cells of [`compile_program`]
/// cannot: full colors (fractional transparency, and which color input a
/// value came from) and one handle per distinct string. A program that
/// [`compile_program`] refuses for those reasons compiles here.
///
/// The linked host is the chart runtime, which holds the interpreter and is
/// therefore built with the compiler rather than checked in
/// ([`chart_runtime_linked`]).
///
/// # Errors
///
/// Returns [`AotError::ChartRuntimeUnavailable`] from a compiler built
/// without the chart runtime, the native refusal of a program the analyzer
/// cannot lower, and [`AotError::NotStandalone`] if the emitted bytes fail
/// the import-free audit.
pub fn compile_chart_wasm_program(program: &CsilProgram) -> Result<AotModule, AotError> {
    if !chart_runtime_linked() {
        return Err(AotError::ChartRuntimeUnavailable);
    }
    let mut plan = plan_native_with(program, true)?;
    // The host executes the program as written, so a site names the
    // instruction it was written as.
    let sites: Vec<_> = plan
        .layout
        .host_sites
        .iter()
        .map(|site| {
            let pc = plan
                .carried
                .as_ref()
                .map_or(site.pc, |carried| carried.origin[site.pc as usize] as u32);
            serde_json::json!({ "pc": pc, "args": site.args, "ret": site.ret })
        })
        .collect();
    let styles: Vec<_> = plan
        .layout
        .style_slots
        .iter()
        .map(|slot| {
            serde_json::json!({
                "slot": slot.slot,
                "offset": slot.offset,
                "mask": slot.mask,
                "tag": slot.tag,
            })
        })
        .collect();
    let host_plan = serde_json::to_vec(&serde_json::json!({
        "program": program,
        "sites": sites,
        "styles": styles,
        "scratch_cells": plan.layout.scratch_cells,
        "canonical_strings": plan.analysis.canonical_strings,
    }))
    .map_err(|error| AotError::InvalidProgram(error.to_string()))?;
    let compiled = plan
        .carried
        .as_ref()
        .map_or(program, |carried| &carried.program);
    let wasm = emit::emit_chart(
        compiled,
        &plan.layout,
        &plan.analysis,
        &host_plan,
        plan.carried.as_ref(),
    )?;
    // Inspect and validate the bytes, rather than trusting emission flags.
    audit_standalone(&wasm).map_err(AotError::NotStandalone)?;
    plan.name_sites_as_written();
    plan.layout.imports_vm = false;
    plan.layout.links_runtime = true;
    Ok(AotModule {
        wasm,
        layout: plan.layout,
    })
}

/// Compile to an artifact a foreign host can instantiate: nothing to
/// resolve, and a memory it can bound. Refuse otherwise.
///
/// [`compile_program`] emits the best native module it can, which today may
/// still import `host.vm` (the interpreter executing the
/// heap/string/effect/request opcodes with no native lowering). Those
/// artifacts are correct, and they run fine on a host that supplies it.
/// They are useless to any other host, and until this gate existed nothing said
/// so: emission succeeded, export succeeded, and the failure surfaced at the
/// far host as an unresolved import naming PineWASM internals.
///
/// This is that failure, moved to the point where it is cheap. It is the
/// emit mode an external host asks for when it cannot provide PineWASM's
/// interpreter imports.
pub fn compile_standalone_program(program: &CsilProgram) -> Result<AotModule, AotError> {
    // A chart-tier strategy's broker is the interpreter host's.
    if program.metadata.strategy.is_some() {
        preflight_shared(program)?;
        return Err(AotError::StrategyRequiresInterpreter);
    }
    let module = compile_program(program)?;
    audit_standalone(&module.wasm).map_err(AotError::NotStandalone)?;
    Ok(module)
}

/// Confirm an emitted module imports nothing and bounds its memory.
///
/// Deliberately reads the bytes rather than [`Layout`]. The flags record what
/// the internal layout planner believed; these bytes are what the emitter produced. A gate
/// consulting the flags would agree with the planner's own mistakes, and the
/// entire point of this one is to notice when emission drifts from the plan.
/// Public because the artifact-export path wants the same answer about bytes
/// it did not emit itself, which is also why the module is validated and not
/// merely decoded: `Parser::parse_all` reads the binary format without
/// checking any of the semantic rules a host applies before instantiating.
/// A module with a well-formed section layout, a bounded memory, and an
/// export naming a function index that does not exist decodes cleanly and is
/// rejected by every engine. "A host can instantiate this" is the claim being
/// made here, so validity is part of it.
pub fn audit_standalone(wasm: &[u8]) -> Result<(), String> {
    use wasmparser::Payload;

    wasmparser::validate(wasm).map_err(|error| format!("module is not valid wasm: {error}"))?;

    let mut bounded_memory = false;
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        let payload = payload.map_err(|error| format!("module does not parse: {error}"))?;
        match payload {
            Payload::ImportSection(reader) => {
                let mut imports = Vec::new();
                for group in reader {
                    let group = group.map_err(|error| format!("import section: {error}"))?;
                    for entry in group {
                        let (_, import) =
                            entry.map_err(|error| format!("import section: {error}"))?;
                        imports.push(format!("{}.{}", import.module, import.name));
                    }
                }
                if !imports.is_empty() {
                    return Err(format!("module imports {}", imports.join(", ")));
                }
            }
            Payload::MemorySection(reader) => {
                for memory in reader {
                    let memory = memory.map_err(|error| format!("memory section: {error}"))?;
                    if memory.maximum.is_none() {
                        return Err("module memory declares no maximum".to_string());
                    }
                    bounded_memory = true;
                }
            }
            _ => {}
        }
    }
    if !bounded_memory {
        return Err("module declares no memory".to_string());
    }
    Ok(())
}

/// The standalone WebAssembly consumer-surface classification.
///
/// For an `indicator()` source, this classifies whether a designated plot is
/// exported as a position-event strategy signal. For a `strategy()` source,
/// the compiler forces the strategy classification and exports the linked
/// broker's posture-event lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StandaloneWasmModuleType {
    /// The OUT `signals` array is a scoreable position-event series; the
    /// module carries the `signal` metadata object and is optimizable.
    Strategy,
    /// No designated signal; catalog/render only. Successful evaluations
    /// carry an empty `signals` array.
    Indicator,
}

/// Event semantics for a designated indicator plot in a standalone WebAssembly strategy module.
///
/// The default preserves the original standalone WebAssembly contract: only transitions into a
/// non-zero direction are serialized. A caller that owns a posture plot can
/// opt into flat-aware events, where transitions back to `0` are serialized
/// as exits as well.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum StandaloneWasmSignalSemantics {
    #[default]
    PositionEvents,
    PostureEvents,
}

impl StandaloneWasmSignalSemantics {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PositionEvents => "position-events",
            Self::PostureEvents => "posture-events",
        }
    }
}

/// Largest native-heap element budget a standalone WebAssembly artifact may bake in.
///
/// standalone WebAssembly has no loader capable of overwriting the ordinary CSAW header after
/// `prepare`, so a build that needs more than the conservative default must
/// declare its bounded budget at compile time. Four million is the largest
/// element budget the browser host uses.
pub const STANDALONE_WASM_MAX_COLLECTION_ELEMENTS: u64 = 4_000_000;

/// Optional standalone WebAssembly artifact configuration. [`compile_standalone_wasm_program`] uses
/// [`Default`], preserving the original artifact contract for Rust callers;
/// hosts that need explicit posture events or a larger bounded native heap
/// use [`compile_standalone_wasm_program_with_config`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StandaloneWasmBuildConfig {
    pub signal_semantics: StandaloneWasmSignalSemantics,
    pub max_collection_elements: u64,
    /// Compile only the part of a plot-designated program that can reach
    /// the designated plot (see `slice.rs`). Off by default: the artifact
    /// then reproduces every failure of the interpreter run and writes every
    /// plot lane. On, display code that cannot reach the signal is erased,
    /// so it can no longer refuse the script, fail the run, or cost budget;
    /// the designated plot is unchanged wherever the interpreter run succeeds.
    pub signal_slice: bool,
}

impl Default for StandaloneWasmBuildConfig {
    fn default() -> Self {
        Self {
            signal_semantics: StandaloneWasmSignalSemantics::PositionEvents,
            max_collection_elements: heap_defaults::MAX_ELEMENTS,
            signal_slice: false,
        }
    }
}

/// Options for [`compile_standalone_wasm_program`].
#[derive(Debug, Clone)]
pub struct StandaloneWasmOptions {
    /// The plot whose per-bar posture becomes the OUT `signals` series.
    /// `None` defaults to plot 0 **iff** the program has exactly one plot
    ///; a multi-plot indicator strategy must designate explicitly. Must be
    /// `None` for an indicator module and for every `strategy()` source.
    pub signal_plot: Option<usize>,
    pub module_type: StandaloneWasmModuleType,
}

/// Where in the program or the build options a [`StandaloneRefusal`] points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StandaloneRefusalSite {
    /// The instruction at `pc` in the program's one function.
    Instruction { pc: usize },
    /// The first instruction, in ascending `pc`, that would still call
    /// `host.vm` after the standalone projection.
    HostSite { pc: usize },
    /// The input declaration with this id.
    Input { id: u32 },
    /// The plot index the caller passed as `signal_plot`.
    Plot { index: usize },
}

/// The structured part of a [`StandaloneRefusal`]: which refusal, of which
/// class, and where. It holds no rendered text, so it can be compared and
/// counted without reading a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StandaloneRefusalReason {
    /// The [`AotError`] variant name, as [`AotError::kind`] returns it.
    pub kind: &'static str,
    pub class: RefusalClass,
    /// `None` where the refusal has no single site, or where the step that
    /// raised it does not report one yet.
    pub site: Option<StandaloneRefusalSite>,
}

/// Why [`standalone_eligibility`] refused a program: a structured
/// [`StandaloneRefusalReason`], the [`AotError`] the standalone WebAssembly
/// compile returns for the same program and options, and, where that error
/// is that `host.vm` would be imported, the Pine calls that need it.
#[derive(Debug, PartialEq, Eq)]
pub struct StandaloneRefusal {
    reason: StandaloneRefusalReason,
    /// The compile's error, kept so its message is produced in one place.
    error: AotError,
    /// See [`StandaloneRefusal::host_features`].
    host_features: Vec<String>,
}

impl StandaloneRefusal {
    fn new(error: AotError, site: Option<StandaloneRefusalSite>) -> Self {
        Self {
            reason: StandaloneRefusalReason {
                kind: error.kind(),
                class: error.class(),
                site,
            },
            error,
            host_features: Vec::new(),
        }
    }

    fn with_host_features(mut self, host_features: Vec<String>) -> Self {
        self.host_features = host_features;
        self
    }

    /// The Pine-level names of what keeps the program out of the standalone
    /// WebAssembly tier, for the program this refusal reports on: the signal
    /// slice when the options designate a plot and the slice applies, else
    /// the whole program. Sorted, without repeats. Empty unless the refusal
    /// is [`AotError::StandaloneWasmNotStandalone`] raised before emission.
    ///
    /// That error says only that `host.vm` would be imported. That is the
    /// name of a mechanism, not of anything in the script; these are the
    /// calls a reader can find and change. When collections and objects stay
    /// on the host only because some other call does, the heap operations
    /// themselves are left out, so the list holds causes and not their
    /// consequences.
    ///
    /// The native heap takes a program whole or not at all, so this cannot
    /// tell a heap operation the heap would decline on its own account from
    /// one that only follows another call. While any other call is named,
    /// the operations that have a native lowering are not; they are named
    /// once nothing else is.
    #[must_use]
    pub fn host_features(&self) -> &[String] {
        &self.host_features
    }

    #[must_use]
    pub fn reason(&self) -> StandaloneRefusalReason {
        self.reason
    }

    #[must_use]
    pub fn kind(&self) -> &'static str {
        self.reason.kind
    }

    #[must_use]
    pub fn class(&self) -> RefusalClass {
        self.reason.class
    }

    #[must_use]
    pub fn site(&self) -> Option<StandaloneRefusalSite> {
        self.reason.site
    }

    /// The error the standalone WebAssembly compile returns for this refusal.
    #[must_use]
    pub fn as_aot_error(&self) -> &AotError {
        &self.error
    }

    /// This refusal of a rewritten program, with its instruction site (and
    /// the pc inside an integer refusal) moved to the instruction of the
    /// program as written that `origin` names.
    fn at_origin(mut self, origin: &[usize]) -> Self {
        let back = |pc: usize| origin.get(pc).copied().unwrap_or(pc);
        self.reason.site = self.reason.site.map(|site| match site {
            StandaloneRefusalSite::Instruction { pc } => {
                StandaloneRefusalSite::Instruction { pc: back(pc) }
            }
            StandaloneRefusalSite::HostSite { pc } => {
                StandaloneRefusalSite::HostSite { pc: back(pc) }
            }
            other => other,
        });
        if let AotError::UnsupportedIntegerNaEncoding { pc } = &mut self.error {
            *pc = back(*pc);
        }
        self
    }
}

impl std::fmt::Display for StandaloneRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for StandaloneRefusal {}

/// A refusal raised by a step that returns [`AotError`]. The only such error
/// that carries its own site is the integer one.
impl From<AotError> for StandaloneRefusal {
    fn from(error: AotError) -> Self {
        let site = match &error {
            AotError::UnsupportedIntegerNaEncoding { pc } => {
                Some(StandaloneRefusalSite::Instruction { pc: *pc })
            }
            _ => None,
        };
        Self::new(error, site)
    }
}

impl From<StandaloneRefusal> for AotError {
    fn from(refusal: StandaloneRefusal) -> Self {
        refusal.error
    }
}

/// What [`host_site_feature`] says when an opcode has no Pine-level name to
/// give. Deliberately a phrase, not an identifier: a caller reading it must
/// not mistake it for something to search their script for.
const UNNAMED_HOST_FEATURE: &str = "a charting-host feature";

/// [`host_site_feature`] for the host site at `pc`, naming a call the
/// compiler lowered by what the script wrote: `timeframe.change(tf)` lowers
/// onto `time(tf)`, whose result it stores in a slot only the lowering names
/// (`%tfcN_bucket`, `pinewasm_semantics`, `analyze_timeframe_change`).
fn host_site_feature_at(program: &CsilProgram, pc: usize) -> String {
    let code = &program.main().code;
    let Some(instr) = code.get(pc) else {
        return UNNAMED_HOST_FEATURE.to_string();
    };
    if let (
        CsilInstr::CallPure {
            func: pinewasm_hir::PureFunc::TimeOpen,
            ..
        },
        Some(CsilInstr::StoreUserSlotCurrent { slot }),
    ) = (instr, code.get(pc + 1))
    {
        let lowered = program
            .series_slots
            .get(*slot as usize)
            .is_some_and(|def| def.name.starts_with("%tfc") && def.name.ends_with("_bucket"));
        if lowered {
            return "timeframe.change".to_string();
        }
    }
    host_site_feature(program, instr)
}

/// The **Pine-level** name for one surviving `host.vm` site.
///
/// What this returns is published verbatim to a caller, as
/// [`AotError::StrategyFeatureUnsupported`]'s `feature` for a `strategy()`
/// source and as [`StandaloneRefusal::host_features`] for an indicator, and
/// consumers render it as an engine identifier the reader is expected to
/// find in their own script. So it must name what they wrote.
///
/// Never `format!("{instr:?}")`: a CSIL variant name like `CallCollection`,
/// `NewObject` or `StrConcat` is an internal compiler identifier that appears
/// nowhere in Pine, and telling someone to "remove or rework `CallCollection`"
/// is not an instruction anyone can follow. That is what this used to do, and
/// drawing-only sites can otherwise mask this refusal class.
fn host_site_feature(program: &CsilProgram, instr: &CsilInstr) -> String {
    use CsilInstr as I;
    // A UDT site names the user's own type (`MyState`); a drawing object's
    // synthesized type names the namespace (`label`) either way.
    let object_type = |type_id: u32| {
        program
            .object_types
            .get(type_id as usize)
            .map_or_else(|| UNNAMED_HOST_FEATURE.to_string(), |d| d.name.clone())
    };
    match instr {
        // Calendar / session / syminfo / timeframe reads: already the Pine
        // name (`month`, `syminfo.mintick`) — the case that always worked.
        I::ReadBuiltin { source, .. } | I::ReadBuiltinDynamic { source } => {
            source.name().to_string()
        }
        // `str.*`, `math.random`, timestamp and calendar functions, and the
        // array/matrix/map surface: both enums carry their own Pine spelling.
        I::CallPure { func, .. } => func.name().to_string(),
        I::CallCollection { func, .. } => func.name().to_string(),
        // Runtime string building — `"n=" + str.tostring(x)`. Named as the
        // operator, since that is what is in the source.
        I::StrConcat => "string concatenation (+)".to_string(),
        // String equality runs on the host because it compares content, not
        // handles.
        I::CmpEq | I::CmpNe => "string comparison (== / !=)".to_string(),
        // Only string and color inputs reach here; a finite `options=` list
        // is projected away first, so a survivor is a free-form text input,
        // named by the call that declared it.
        I::ReadInput { input } => input_feature(program, *input).to_string(),
        I::ReadSecurity { .. } | I::ReadSecurityExpr { .. } => "request.security".to_string(),
        I::ReadSecurityLowerTf { .. } => "request.security_lower_tf".to_string(),
        I::ReadRequest { func, .. } => format!("request.{}", func.member()),
        I::ReadTimeTf { .. } => "time with a session or timeframe".to_string(),
        I::ReadPivotLevels { .. } | I::CallPivotPointLevels { .. } => {
            "ta.pivot_point_levels".to_string()
        }
        I::BuildFootprint { .. } | I::FootprintRowByPrice => "footprint data".to_string(),
        // User-defined types. `GetField`/`SetField` carry only a field index,
        // so they name the surface rather than a specific type.
        I::NewObject { type_id, .. } | I::CopyObject { type_id } => object_type(*type_id),
        I::GetField { .. } | I::SetField { .. } => "user-defined type fields".to_string(),
        // Drawing objects: `namespace()` is exactly the Pine prefix.
        I::RegisterDrawing { kind } | I::ReadDrawingsAll { kind } | I::DeleteDrawing { kind } => {
            kind.namespace().to_string()
        }
        I::SetCell { .. } | I::ClearCells | I::MergeCells => "table cells".to_string(),
        I::Log { level, .. } => format!("log.{}", level.member()),
        I::Alert => "alert".to_string(),
        I::AlertCondition => "alertcondition".to_string(),
        I::RuntimeError => "runtime.error".to_string(),
        // No opcode outside the set above is marked a host site today
        // (`analysis::Typer`), and `strategy_host_features_are_named_in_pine`
        // pins the ones a script can actually reach. A future one lands here
        // and says something honest rather than leaking its variant name.
        _ => UNNAMED_HOST_FEATURE.to_string(),
    }
}

/// The Pine call that declared input `input`, as a host feature.
///
/// Several input widgets share the `input string` type and are distinct
/// calls in the source, so the declaration's widget picks the name: a
/// timeframe input reported as `input.string` names a call the script never
/// wrote. `input.string`, `input.timeframe` and `input.session` take an
/// `options=` list, which is what would make them standalone, so their name
/// says the list is missing; `input.symbol` and `input.text_area` have no
/// such argument.
fn input_feature(program: &CsilProgram, input: u32) -> &'static str {
    let Some(decl) = program.inputs.get(input as usize) else {
        return UNNAMED_HOST_FEATURE;
    };
    match (decl.kind, decl.widget) {
        (InputKind::Color, _) => "input.color",
        (InputKind::String, InputWidget::Timeframe) => "input.timeframe with no options list",
        (InputKind::String, InputWidget::Session) => "input.session with no options list",
        (InputKind::String, InputWidget::Symbol) => "input.symbol",
        (InputKind::String, InputWidget::TextArea) => "input.text_area",
        // `input.string`, and a plain `input()` with a text default.
        (InputKind::String, InputWidget::String | InputWidget::Standard) => {
            "input.string with no options list"
        }
        _ => UNNAMED_HOST_FEATURE,
    }
}

/// Whether the host site at `pc` is an argument the compiler supplied for
/// the call after it, which a refusal does not name: `timeframe.in_seconds()`
/// reads `timeframe.period` for its default, and `ticker.standard()` reads
/// `syminfo.tickerid`. The call is what the script says. An argument the
/// script wrote has its own span.
fn supplied_argument(program: &CsilProgram, analysis: &analysis::Analysis, pc: usize) -> bool {
    let code = &program.main().code;
    let source_map = &program.main().source_map;
    matches!(code.get(pc), Some(CsilInstr::ReadBuiltin { .. }))
        && analysis.host_site.get(pc + 1).copied().unwrap_or(false)
        && matches!(code.get(pc + 1), Some(CsilInstr::CallPure { .. }))
        && source_map.get(pc).is_some()
        && source_map.get(pc) == source_map.get(pc + 1)
}

/// What [`StandaloneRefusal::host_features`] reports for a projected,
/// analyzed program whose layout still imports `host.vm`.
fn standalone_host_features(
    program: &CsilProgram,
    analysis: &analysis::Analysis,
    layout: &Layout,
) -> Vec<String> {
    let code = &program.main().code;
    // With a native heap every remaining site is a cause. Without one,
    // every collection and object operation is a host site whether or not
    // it could run natively, so only the operations the native heap has no
    // lowering for are causes, beside everything that is not a heap
    // operation at all.
    let native_heap = analysis.heap.is_some();
    let mut causes = BTreeSet::new();
    let mut heap = BTreeSet::new();
    for site in &layout.host_sites {
        let pc = site.pc as usize;
        let Some(instr) = code.get(pc) else { continue };
        let name = host_site_feature_at(program, pc);
        match instr {
            // Joins the host plan only when something else already needs it.
            CsilInstr::AlertCondition => {}
            _ if supplied_argument(program, analysis, pc) => {}
            CsilInstr::CallCollection { func, argc } if !native_heap => {
                let lowers = analysis::native_coll_op(
                    *func,
                    *argc,
                    analysis::pushes_literal_int(code, pc, 0),
                )
                .is_some();
                if lowers {
                    heap.insert(name);
                } else {
                    causes.insert(name);
                }
            }
            CsilInstr::NewObject { .. }
            | CsilInstr::CopyObject { .. }
            | CsilInstr::GetField { .. }
            | CsilInstr::SetField { .. }
                if !native_heap =>
            {
                heap.insert(name);
            }
            _ => {
                causes.insert(name);
            }
        }
    }
    // Nothing but heap operations: the native heap declined the program for
    // how it uses them, and the operations are all there is to name.
    let named = if causes.is_empty() { heap } else { causes };
    named.into_iter().collect()
}

/// Compile a validated program to a standalone WebAssembly v1 consumer artifact. See
/// `docs/aot.md` for the public contract and eligibility model.
///
/// The emitted module exports exactly `evaluate_indicator_cabi`, `alloc`,
/// `dealloc`, `last_error`, `reset`, and `memory`; `prepare`/`run` remain
/// internal functions sequenced by the emitted entry point around the
/// linked runtime's standalone WebAssembly family (JSON parse, lane fill, signal
/// serialization). Only standalone-eligible programs are accepted, and
/// compiler-generated distance-scan guards select their literal native arm
/// first. Any remaining program needing `host.vm` is refused with the import
/// name, the same failure [`compile_standalone_program`] moved to where it is cheap.
pub fn compile_standalone_wasm_program(
    program: &CsilProgram,
    opts: &StandaloneWasmOptions,
) -> Result<Vec<u8>, AotError> {
    compile_standalone_wasm_program_with_config(
        program,
        opts,
        &StandaloneWasmBuildConfig::default(),
    )
}

/// Compile a standalone WebAssembly artifact with an explicit designated-plot event contract and
/// native-heap budget. See [`StandaloneWasmBuildConfig`].
pub fn compile_standalone_wasm_program_with_config(
    program: &CsilProgram,
    opts: &StandaloneWasmOptions,
    config: &StandaloneWasmBuildConfig,
) -> Result<Vec<u8>, AotError> {
    compile_standalone_wasm_program_explained(program, opts, config)
        .map_err(StandaloneCompileRefusal::into_error)
}

/// A standalone WebAssembly compile that produced no module: the error
/// [`compile_standalone_wasm_program_with_config`] returns, and what
/// [`standalone_eligibility`] says about it.
///
/// A caller that reports a refusal wants its site and the Pine calls behind
/// it, which the error alone does not carry. The compile runs the
/// eligibility steps itself, so it hands them over here and the caller has
/// no second analysis of the program to run.
#[derive(Debug)]
pub struct StandaloneCompileRefusal {
    refusal: StandaloneRefusal,
    /// Whether every attempt at a module was refused before emission.
    ineligible: bool,
}

impl StandaloneCompileRefusal {
    /// The error [`compile_standalone_wasm_program_with_config`] returns.
    #[must_use]
    pub fn error(&self) -> &AotError {
        self.refusal.as_aot_error()
    }

    /// [`Self::error`], owned.
    #[must_use]
    pub fn into_error(self) -> AotError {
        self.refusal.into()
    }

    /// What [`standalone_eligibility`] returns for the same program, options
    /// and config, when it refuses with this compile's error. `None` for a
    /// refusal first raised during emission or by the audit of the emitted
    /// bytes: eligibility accepts that program, so it has no site to give.
    #[must_use]
    pub fn eligibility(&self) -> Option<&StandaloneRefusal> {
        self.ineligible.then_some(&self.refusal)
    }
}

/// [`compile_standalone_wasm_program_with_config`], keeping the eligibility
/// refusal behind an error (see [`StandaloneCompileRefusal`]).
pub fn compile_standalone_wasm_program_explained(
    program: &CsilProgram,
    opts: &StandaloneWasmOptions,
    config: &StandaloneWasmBuildConfig,
) -> Result<Vec<u8>, StandaloneCompileRefusal> {
    let (wasm, linked) = emit_standalone_wasm_explained(program, opts, config)?;
    audit_emitted_standalone_wasm(&wasm, linked).map_err(|error| StandaloneCompileRefusal {
        refusal: error.into(),
        ineligible: false,
    })?;
    Ok(wasm)
}

/// [`emit_standalone_wasm_explained`] with the bare error, for the audit's
/// tests: they hold unaudited bytes next to a blob that did, or did not,
/// produce them.
#[cfg(test)]
fn emit_standalone_wasm_unaudited(
    program: &CsilProgram,
    opts: &StandaloneWasmOptions,
    config: &StandaloneWasmBuildConfig,
) -> Result<(Vec<u8>, &'static runtime::Runtime), AotError> {
    emit_standalone_wasm_explained(program, opts, config)
        .map_err(StandaloneCompileRefusal::into_error)
}

/// Everything [`compile_standalone_wasm_program_explained`] does before the
/// producer audit: eligibility, projection, analysis, layout, and emission.
/// Returns the bytes together with the runtime blob they splice, which is
/// what the audit compares copied bodies against. An error keeps the
/// eligibility refusal behind it.
///
/// [`standalone_eligibility`] makes the same attempts in the same order and
/// stops at the first one it accepts. While every attempt made here is
/// refused before emission, the two therefore make the same attempts with
/// the same refusals and report the same one. Once an attempt gets past its
/// eligibility steps, eligibility accepts the program, whatever emission
/// does next.
fn emit_standalone_wasm_explained(
    program: &CsilProgram,
    opts: &StandaloneWasmOptions,
    config: &StandaloneWasmBuildConfig,
) -> Result<(Vec<u8>, &'static runtime::Runtime), StandaloneCompileRefusal> {
    let eligible = std::cell::Cell::new(false);
    standalone_wasm_attempts(program, opts, config, |candidate| {
        let preflight = standalone_wasm_preflight(&candidate, opts, config)?;
        eligible.set(true);
        emit_standalone_wasm_preflighted(preflight, config).map_err(StandaloneRefusal::from)
    })
    .map_err(|refusal| StandaloneCompileRefusal {
        refusal,
        ineligible: !eligible.get(),
    })
}

/// Whether the standalone WebAssembly compile accepts `program` before it
/// emits a byte, and if not, the first refusal.
///
/// This is the prefix of [`compile_standalone_wasm_program_with_config`] that
/// decides eligibility: the build options, the preflight, the standalone
/// projection, the analysis, the `host.vm` check, the `input.source` defaults
/// and the plot designation, in that order. The compile runs this same code
/// and then emits, so a refusal reported here is the refusal the compile
/// reports, with the same message.
///
/// With [`StandaloneWasmBuildConfig::signal_slice`], the program is eligible
/// when its signal slice or the whole program is. When neither is, the
/// slice's refusal is the one reported, as in the compile.
///
/// `Ok` does not yet mean the compile succeeds: the layout and size limits
/// are still computed during emission, and the emitted bytes are audited
/// afterwards.
pub fn standalone_eligibility(
    program: &CsilProgram,
    opts: &StandaloneWasmOptions,
    config: &StandaloneWasmBuildConfig,
) -> Result<(), StandaloneRefusal> {
    standalone_wasm_attempts(program, opts, config, |candidate| {
        standalone_wasm_preflight(&candidate, opts, config).map(drop)
    })
}

/// The steps shared by the standalone WebAssembly compile and by
/// [`standalone_eligibility`] around one `attempt` on a program: the build
/// option check, the preflight of the whole program, and the choice between
/// the signal slice and the whole program.
fn standalone_wasm_attempts<T>(
    program: &CsilProgram,
    opts: &StandaloneWasmOptions,
    config: &StandaloneWasmBuildConfig,
    attempt: impl Fn(Candidate<'_>) -> Result<T, StandaloneRefusal>,
) -> Result<T, StandaloneRefusal> {
    if config.max_collection_elements == 0
        || config.max_collection_elements > STANDALONE_WASM_MAX_COLLECTION_ELEMENTS
    {
        return Err(AotError::StandaloneWasmInvalidOptions(format!(
            "max_collection_elements must be between 1 and {STANDALONE_WASM_MAX_COLLECTION_ELEMENTS}, got {}",
            config.max_collection_elements
        ))
        .into());
    }
    preflight_standalone_wasm(program)?;
    // The native-only consumer has no Rust heap service. Select the literal
    // arm of compiler-generated distance-scan guards before slicing and planning.
    // VM and chart AOT retain their indexed kernel; this target meters the
    // literal work it actually compiles.
    let literal = standalone_scan::literal_program(program);
    let program = literal
        .as_ref()
        .map_or(program, |selected| &selected.program);
    // A plot-designated module serializes one plot. Compile the slice of
    // the program that can reach it, so display code the importless tier
    // cannot run (or type) no longer refuses the script. The slice only
    // widens what is accepted: when the sliced program fails, the whole
    // program gets its chance, and if both fail the slice's error is the
    // one reported, because it names what stands between the script and
    // its signal.
    //
    // A strategy() source is anchored to its orders and its plot values. It
    // is tried whole first, and without the native heap a strategy never
    // had, so a script that needs neither keeps the bytes it always had.
    // Only when that is refused is it tried again with the heap, from its
    // slice when the slice is on and applies. That attempt's refusal names
    // what stands between the script and its orders, so it is the one
    // reported.
    let whole = |strategy_heap| {
        attempt(Candidate {
            program,
            budget_free: &BTreeSet::new(),
            strategy_heap,
            preflighted: literal.is_none(),
        })
        .map_err(|refusal| match &literal {
            Some(selected) => refusal.at_origin(&selected.origin),
            None => refusal,
        })
    };
    let part = |sliced: &slice::Sliced, strategy_heap| {
        attempt(Candidate {
            program: &sliced.program,
            budget_free: &sliced.budget_free,
            strategy_heap,
            preflighted: false,
        })
    };
    if program.metadata.strategy.is_some() {
        return whole(false).or_else(|_| {
            let sliced = if config.signal_slice && opts.signal_plot.is_none() {
                slice::project_to_signal(program, slice::Anchor::Strategy)
            } else {
                None
            };
            match sliced {
                Some(sliced) => part(&sliced, true),
                None => whole(true),
            }
        });
    }
    let designated = if config.signal_slice {
        designated_plot_for_slice(program, opts)
    } else {
        None
    };
    if let Some(plot) = designated {
        if let Some(sliced) = slice::project_to_signal(program, slice::Anchor::Plot(plot)) {
            return match part(&sliced, false) {
                Ok(accepted) => Ok(accepted),
                Err(sliced_error) => whole(false).map_err(|_| sliced_error),
            };
        }
    }
    whole(false)
}

/// The plot a strategy-type indicator module designates, when the options
/// name one unambiguously. Invalid combinations return `None` here and are
/// refused with their own error by [`standalone_wasm_preflight_as_written`].
fn designated_plot_for_slice(program: &CsilProgram, opts: &StandaloneWasmOptions) -> Option<u32> {
    if program.metadata.strategy.is_some() || opts.module_type != StandaloneWasmModuleType::Strategy
    {
        return None;
    }
    match opts.signal_plot {
        Some(plot) if plot < program.plots.len() => u32::try_from(plot).ok(),
        None if program.plots.len() == 1 => Some(0),
        _ => None,
    }
}

/// What [`standalone_wasm_preflight`] hands to emission: the projected
/// program and everything decided about it before a byte is written.
struct StandaloneWasmPreflight {
    program: CsilProgram,
    projection: projection::StandaloneWasmProjection,
    analysis: analysis::Analysis,
    layout: Layout,
    signal_plot: Option<u32>,
    module_type: StandaloneWasmModuleType,
    strategy_source: bool,
}

/// Emission of one program, sliced or whole, that
/// [`standalone_wasm_preflight`] accepted.
fn emit_standalone_wasm_preflighted(
    preflight: StandaloneWasmPreflight,
    config: &StandaloneWasmBuildConfig,
) -> Result<(Vec<u8>, &'static runtime::Runtime), AotError> {
    let StandaloneWasmPreflight {
        program,
        projection,
        analysis,
        mut layout,
        signal_plot,
        module_type,
        strategy_source,
    } = preflight;

    // The standalone WebAssembly entry point always needs the blob (parser, serializer,
    // allocator), whether or not the program calls any transcendental.
    layout.links_runtime = true;

    let keytab = schema::standalone_wasm_key_table(&program, &projection)?;
    let spec = emit::StandaloneWasmSpec {
        signal_plot,
        module_type,
        strategy_source,
        signal_semantics: config.signal_semantics,
        max_collection_elements: config.max_collection_elements,
        keytab: &keytab,
        projection: &projection,
    };
    let wasm = emit::emit_standalone_wasm(&program, &layout, &analysis, &spec)?;
    let linked = emit::linked_runtime(&layout, true)
        .expect("a standalone WebAssembly layout always links the runtime");
    Ok((wasm, linked))
}

/// [`standalone_wasm_preflight_as_written`], and when that refuses a kept
/// Pine int quotient, the same steps on the program with its kept quotients
/// carried as float code (see `fraction.rs`).
///
/// The carry only widens what is accepted. A program the steps accept as
/// written is never rewritten, so its module is the one it always was. A
/// program the carry cannot rewrite reports the refusal of the program as
/// written.
fn standalone_wasm_preflight(
    candidate: &Candidate<'_>,
    opts: &StandaloneWasmOptions,
    config: &StandaloneWasmBuildConfig,
) -> Result<StandaloneWasmPreflight, StandaloneRefusal> {
    let Candidate {
        program,
        budget_free,
        strategy_heap,
        preflighted,
    } = *candidate;
    let written = standalone_wasm_preflight_as_written(
        program,
        opts,
        config,
        &Attempt {
            budget_free,
            exact_guard: &BTreeSet::new(),
            strategy_heap,
            preflighted,
        },
    );
    let kept_quotient = matches!(
        &written,
        Err(refusal) if matches!(
            refusal.as_aot_error(),
            AotError::UnsupportedOpcode(opcode) if opcode == "PineIntDiv"
        )
    );
    if !kept_quotient {
        return written;
    }
    let Some(carried) = fraction::carry(program, budget_free) else {
        return written;
    };
    match standalone_wasm_preflight_as_written(
        &carried.program,
        opts,
        config,
        &Attempt {
            budget_free: &carried.budget_free,
            exact_guard: &carried.exact_guard,
            strategy_heap,
            preflighted: false,
        },
    ) {
        Ok(preflight) => Ok(preflight),
        // The rewritten program is the one that could have compiled, so its
        // refusal names what actually stands in the way (a host-only opcode,
        // say), at the instruction of the program as written. A refusal of
        // the internal class is this pass's own limit: report the quotient
        // the program was written with instead.
        Err(refusal) if refusal.class() == RefusalClass::Internal => written,
        Err(refusal) => Err(refusal.at_origin(&carried.origin)),
    }
}

/// One program [`standalone_wasm_attempts`] tries to build a module from:
/// the supplied one, or its signal slice.
#[derive(Clone, Copy)]
struct Candidate<'a> {
    program: &'a CsilProgram,
    /// Slice placeholders the instruction budget does not charge; empty for
    /// the supplied program.
    budget_free: &'a BTreeSet<usize>,
    /// Offer a `strategy()` program the native heap.
    strategy_heap: bool,
    /// Whether `program` is the supplied one, which
    /// [`standalone_wasm_attempts`] ran [`preflight_standalone_wasm`] on
    /// before any attempt.
    preflighted: bool,
}

/// How one attempt at a module is made, beyond the caller's options.
struct Attempt<'a> {
    /// Slice placeholders the instruction budget does not charge.
    budget_free: &'a BTreeSet<usize>,
    /// The fraction carry's exactness-checked instructions (empty for a
    /// program compiled as written).
    exact_guard: &'a BTreeSet<usize>,
    /// Offer a `strategy()` program the native heap.
    strategy_heap: bool,
    /// Whether [`preflight_standalone_wasm`] already accepted this very
    /// program. It reads nothing but the program, so it need not run again.
    preflighted: bool,
}

/// Eligibility, projection, analysis and layout of one program, sliced or
/// whole: every step of the standalone WebAssembly compile that can refuse
/// before emission, in the order the compile has always run them.
/// `exact_guard` names the fraction carry's exactness-checked instructions
/// (empty for a program compiled as written).
fn standalone_wasm_preflight_as_written(
    program: &CsilProgram,
    opts: &StandaloneWasmOptions,
    config: &StandaloneWasmBuildConfig,
    attempt: &Attempt<'_>,
) -> Result<StandaloneWasmPreflight, StandaloneRefusal> {
    if !attempt.preflighted {
        preflight_standalone_wasm(program)?;
    }
    // standalone WebAssembly has no PineWASM host. Build a standalone WebAssembly-only projection rather than
    // weakening the ordinary AOT tier: finite string options use integer
    // wire indexes, colors pin to their declared defaults, and only
    // backward-proven chart effects may disappear.  A strategy() source
    // takes the same projection: the broker rides inside the module, but
    // the script body around it is ordinary PineWASM and is projected
    // identically — including the const-pool canonicalization the broker's
    // own string-handle resolution then reads (`broker_config_bytes`).
    let (program, mut projection) = projection::StandaloneWasmProjection::prepare(program)?;
    // `prepare` rewrites string handles in place, so pcs still line up.
    projection.budget_free.clone_from(attempt.budget_free);
    projection.exact_guard.clone_from(attempt.exact_guard);
    // The standalone runtime rejects MIN int/time `params`, so unbounded
    // integer inputs are admitted here (never in the host-loaded tiers).
    let mut analysis = analysis::analyze_with(
        &program,
        analysis::IntInputBoundary::RejectsSentinel,
        attempt.strategy_heap,
        analysis::StrategyBroker::Linked,
    )?;
    // Erased drawing constructors are certified against the heap caps of
    // the VM run this artifact stands in for: its baked element budget and
    // the VM's fixed live object cap.
    projection.finish(
        &program,
        &mut analysis,
        projection::HeapBudget {
            max_elements: config.max_collection_elements,
            max_objects: heap_defaults::MAX_OBJECTS,
        },
    )?;
    let mut layout = plan_layout(&program, &analysis);
    layout.security_lanes = projection
        .security_specs
        .iter()
        .map(projection::SecuritySpec::lanes)
        .sum();
    // One scratch cell per request lane, after every cell the run itself
    // uses: the runtime leaves the lane's first-read charge or status there.
    projection.security_status_base = layout.scratch_cells;
    layout.scratch_cells += projection.security_specs.len() as u32;
    // Then the request's chart timeframe, when a served site reads it.
    if projection.reads_timeframe() {
        projection.timeframe_cells = layout.scratch_cells;
        layout.scratch_cells += 4;
    }

    let strategy_source = program.metadata.strategy.is_some();

    // Refuse imports up front, naming them, rather than letting emission
    // succeed and the byte audit refuse a module we built on purpose.
    if layout.imports_vm {
        // `host_sites` is in ascending pc order.
        let first_host_site =
            layout
                .host_sites
                .first()
                .map(|site| StandaloneRefusalSite::HostSite {
                    pc: site.pc as usize,
                });
        // A strategy() source gets the feature-named refusal instead of the
        // generic not-standalone one: the diagnostic must
        // say WHICH construct keeps the script out — e.g. the `month`
        // calendar builtin — not merely that `host.vm` would be imported.
        // As for an indicator, an argument the compiler supplied is left to
        // the call that takes it.
        if strategy_source {
            let mut names: Vec<String> = layout
                .host_sites
                .iter()
                .filter(|site| !supplied_argument(&program, &analysis, site.pc as usize))
                .map(|site| host_site_feature_at(&program, site.pc as usize))
                .collect();
            names.sort();
            names.dedup();
            return Err(StandaloneRefusal::new(
                AotError::StrategyFeatureUnsupported {
                    feature: names.join(", "),
                },
                first_host_site,
            ));
        }
        // `host.vm` names a mechanism. The refusal also carries the calls
        // that need it, the way a strategy refusal names them in its error.
        return Err(StandaloneRefusal::new(
            AotError::StandaloneWasmNotStandalone("program would import host.vm".to_string()),
            first_host_site,
        )
        .with_host_features(standalone_host_features(&program, &analysis, &layout)));
    }

    // standalone WebAssembly has no loader to resolve `input.source` against a chart, so the
    // wire protocol pins each source lane to the declaration's default —
    // which must therefore be one of the six raw builtin lanes the input
    // JSON carries (derived series like hl2 have no lane to copy).
    for decl in program
        .inputs
        .iter()
        .filter(|d| d.kind == InputKind::Source)
    {
        let supported =
            matches!(&decl.defval, InputValue::Source(series) if BUILTIN_LANES.contains(series));
        if !supported {
            return Err(StandaloneRefusal::new(
                AotError::StandaloneWasmSourceInputUnsupported(format!(
                    "input {} defaults to {:?}, not a raw builtin series",
                    decl.id, decl.defval
                )),
                Some(StandaloneRefusalSite::Input { id: decl.id }),
            ));
        }
    }

    // A strategy() source derives its OUT signals from the broker's posture
    // lane, not from any plot: designating one is a refusal, and the module
    // type is forced to `strategy` — the
    // catalog surface reflects the source construct, whatever the caller
    // passed.
    if let (true, Some(plot)) = (strategy_source, opts.signal_plot) {
        return Err(StandaloneRefusal::new(
            AotError::StandaloneWasmNoDesignatedPlot(
                "a strategy() source derives its signals from the broker posture; omit signal_plot"
                    .into(),
            ),
            Some(StandaloneRefusalSite::Plot { index: plot }),
        ));
    }
    let module_type = if strategy_source {
        StandaloneWasmModuleType::Strategy
    } else {
        opts.module_type
    };

    let signal_plot = if strategy_source {
        None
    } else {
        match (opts.module_type, opts.signal_plot) {
            (StandaloneWasmModuleType::Indicator, None) => None,
            (StandaloneWasmModuleType::Indicator, Some(plot)) => {
                return Err(StandaloneRefusal::new(
                    AotError::StandaloneWasmNoDesignatedPlot(
                        "an indicator-type module designates no signal plot; omit signal_plot"
                            .into(),
                    ),
                    Some(StandaloneRefusalSite::Plot { index: plot }),
                ))
            }
            (StandaloneWasmModuleType::Strategy, Some(plot)) => {
                if plot >= program.plots.len() {
                    return Err(StandaloneRefusal::new(
                        AotError::StandaloneWasmNoDesignatedPlot(format!(
                            "signal_plot {plot} out of range (program has {} plots)",
                            program.plots.len()
                        )),
                        Some(StandaloneRefusalSite::Plot { index: plot }),
                    ));
                }
                Some(plot as u32)
            }
            (StandaloneWasmModuleType::Strategy, None) if program.plots.len() == 1 => Some(0),
            (StandaloneWasmModuleType::Strategy, None) => {
                return Err(AotError::StandaloneWasmNoDesignatedPlot(format!(
                    "program has {} plots; set StandaloneWasmOptions::signal_plot",
                    program.plots.len()
                ))
                .into())
            }
        }
    };

    Ok(StandaloneWasmPreflight {
        program,
        projection,
        analysis,
        layout,
        signal_plot,
        module_type,
        strategy_source,
    })
}

/// Confirm an emitted module satisfies the standalone WebAssembly v1 host contract's static
/// half: valid under the hardened host's feature set, zero imports, bounded
/// memory, exactly the five spec'd function exports with exact signatures,
/// and a conformant first `module_metadata` section.
///
/// Reads the bytes rather than any plan state, for the same reason as
/// [`audit_standalone`] (whose checks this includes): the producer gate
/// must never pass bytes that a conforming host must reject.
pub fn audit_standalone_wasm(wasm: &[u8]) -> Result<(), AotError> {
    audit_standalone_wasm_with(wasm, None)
}

/// The producer's audit of bytes it just emitted by splicing `linked`: the
/// same gate as [`audit_standalone_wasm`] with the same verdict on every
/// input. The one difference is that a runtime body proven to be a verbatim
/// copy of `linked`'s canonical body is not validated a second time (see
/// [`validate_hardened`]). Bytes from anywhere else take the public entry
/// point, which knows nothing about their origin and validates everything.
fn audit_emitted_standalone_wasm(wasm: &[u8], linked: &runtime::Runtime) -> Result<(), AotError> {
    audit_standalone_wasm_with(wasm, Some(linked))
}

fn audit_standalone_wasm_with(
    wasm: &[u8],
    verified: Option<&runtime::Runtime>,
) -> Result<(), AotError> {
    use wasmparser::Payload;

    validate_hardened(wasm, verified)?;

    /// Required function exports and their signatures, as
    /// `(name, params, results)` over core value types.
    const REQUIRED: [(&str, &[wasmparser::ValType], &[wasmparser::ValType]); 5] = {
        use wasmparser::ValType::{I32, I64};
        [
            ("evaluate_indicator_cabi", &[I32, I32], &[I64]),
            ("alloc", &[I32], &[I32]),
            ("dealloc", &[I32, I32], &[]),
            ("last_error", &[], &[I64]),
            ("reset", &[], &[]),
        ]
    };

    let mut types: Vec<wasmparser::FuncType> = Vec::new();
    let mut func_types: Vec<u32> = Vec::new();
    let mut func_exports: Vec<(String, u32)> = Vec::new();
    let mut memory_exported = false;
    let mut bounded_memory = false;
    let mut metadata: Option<Vec<u8>> = None;
    let surface = |error: String| AotError::StandaloneWasmExportSurface(error);

    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        match payload.map_err(|error| surface(format!("module does not parse: {error}")))? {
            Payload::TypeSection(reader) => {
                for group in reader {
                    for ty in group
                        .map_err(|error| surface(format!("type section: {error}")))?
                        .into_types()
                    {
                        // Not every composite type is a function type: the
                        // validation above runs `WasmFeatures::default()`
                        // minus the hardened-host flags, and `gc` is not among those —
                        // it stays on, so a struct/array (or a typed
                        // continuation) reaches this walk and `unwrap_func`
                        // would abort the compiler. A producer gate returns
                        // an `Err` for every input, so it is refused by name
                        // instead. The feature set itself is left alone
                        // deliberately: the documented hardened host does not
                        // disable `gc`, and the audit must not diverge from
                        // it (this emitter emits function types only, so
                        // nothing legitimate is lost either way).
                        match &ty.composite_type.inner {
                            wasmparser::CompositeInnerType::Func(func) => types.push(func.clone()),
                            other => {
                                return Err(surface(format!(
                                    "type {} is not a function type ({other})",
                                    types.len()
                                )))
                            }
                        }
                    }
                }
            }
            Payload::FunctionSection(reader) => {
                for t in reader {
                    func_types
                        .push(t.map_err(|error| surface(format!("function section: {error}")))?);
                }
            }
            Payload::ImportSection(reader) => {
                let mut imports = Vec::new();
                for group in reader {
                    let group =
                        group.map_err(|error| surface(format!("import section: {error}")))?;
                    for entry in group {
                        let (_, import) =
                            entry.map_err(|error| surface(format!("import section: {error}")))?;
                        imports.push(format!("{}.{}", import.module, import.name));
                    }
                }
                if !imports.is_empty() {
                    return Err(AotError::StandaloneWasmNotStandalone(format!(
                        "module imports {}",
                        imports.join(", ")
                    )));
                }
            }
            Payload::MemorySection(reader) => {
                for memory in reader {
                    let memory =
                        memory.map_err(|error| surface(format!("memory section: {error}")))?;
                    if memory.maximum.is_none() {
                        return Err(surface("module memory declares no maximum".to_string()));
                    }
                    bounded_memory = true;
                }
            }
            Payload::ExportSection(reader) => {
                for export in reader {
                    let export =
                        export.map_err(|error| surface(format!("export section: {error}")))?;
                    match export.kind {
                        wasmparser::ExternalKind::Func => {
                            func_exports.push((export.name.to_string(), export.index));
                        }
                        wasmparser::ExternalKind::Memory if export.name == "memory" => {
                            memory_exported = true;
                        }
                        // Other non-function exports carry no contract
                        // meaning; this emitter produces none.
                        _ => {}
                    }
                }
            }
            // Only the first metadata occurrence is read.
            Payload::CustomSection(reader)
                if reader.name() == schema::MODULE_METADATA_SECTION && metadata.is_none() =>
            {
                metadata = Some(reader.data().to_vec());
            }
            _ => {}
        }
    }

    if !bounded_memory {
        return Err(surface("module declares no memory".to_string()));
    }
    if !memory_exported {
        return Err(surface("module does not export memory".to_string()));
    }

    // The set of function exports must equal exactly the documented five: a
    // missing name is a broken surface, and any extra (`prepare`, `run`, a
    // leaked helper) violates the exact export surface.
    let missing: Vec<&str> = REQUIRED
        .iter()
        .map(|&(name, _, _)| name)
        .filter(|name| !func_exports.iter().any(|(n, _)| n == name))
        .collect();
    if !missing.is_empty() {
        return Err(surface(format!(
            "missing function exports: {}",
            missing.join(", ")
        )));
    }
    let extra: Vec<&str> = func_exports
        .iter()
        .map(|(n, _)| n.as_str())
        .filter(|n| !REQUIRED.iter().any(|&(name, _, _)| name == *n))
        .collect();
    if !extra.is_empty() {
        return Err(surface(format!(
            "extra function exports: {}",
            extra.join(", ")
        )));
    }
    for &(name, params, results) in &REQUIRED {
        // Zero imports were enforced above, so an exported function index
        // addresses the function section directly.
        let &(_, index) = func_exports
            .iter()
            .find(|(n, _)| n == name)
            .expect("missing exports were rejected above");
        let ty = func_types
            .get(index as usize)
            .and_then(|&t| types.get(t as usize))
            .ok_or_else(|| surface(format!("{name} export has no resolvable type")))?;
        if ty.params() != params || ty.results() != results {
            return Err(surface(format!(
                "{name} has signature {:?} -> {:?}, expected {params:?} -> {results:?}",
                ty.params(),
                ty.results()
            )));
        }
    }

    audit_standalone_wasm_metadata(metadata.as_deref())
}

/// Validate `wasm` under the hardened host feature set with the same
/// accept/reject verdict, and the same error, as
/// `wasmparser::Validator::validate_all`, except that a function body proven
/// to be a verbatim copy of one of `verified`'s canonical bodies is not
/// validated again. Returns how many bodies that covered; the public audit
/// passes `None` and covers nothing.
///
/// # Why a verified copy needs no second validation
///
/// A body's validity depends on its own bytes and on the module entries its
/// instructions can name: its type, the types, functions, and globals it
/// indexes, the memory it addresses, the data segments `memory.init` and
/// `data.drop` count against, and the enabled feature set. Every frozen
/// runtime is validated under exactly this feature set when the compiler is
/// built (`build.rs`), which also refuses `ref.func`, the one operator whose
/// validity depends on export and element declarations an artifact does not
/// carry over, and the runtime parse refuses tables, tags, imports, and
/// explicit rec groups for the same reason. The splice puts the runtime's
/// types, functions, globals, and data segments at the low indices and copies
/// each reachable body byte for byte (`runtime::Runtime::splice`). So a body
/// byte-identical to canonical body `i`, in an artifact whose type,
/// function-type, and global-type prefixes equal the blob's, whose single
/// memory is 32-bit and unshared, and whose data count covers the blob's
/// segments, can name nothing the blob's own validation did not already
/// check. Every section, and every other body — the artifact's own functions
/// and the tree-shaken stubs — is validated here exactly as before. A prefix
/// that differs, or a byte that differs, means that body is validated, so the
/// verdict never depends on the emitter having been right about what it
/// copied.
///
/// This is where a standalone WebAssembly emission used to spend most of its
/// time: the frozen runtime is ~144 KiB of code and the program a few KiB.
fn validate_hardened(wasm: &[u8], verified: Option<&runtime::Runtime>) -> Result<usize, AotError> {
    let invalid = |error: wasmparser::BinaryReaderError| {
        AotError::StandaloneWasmExportSurface(format!(
            "module is not valid hardened-host wasm: {error}"
        ))
    };
    let features = hardened::hardened_host_features();
    let mut validator = wasmparser::Validator::new_with_features(features);
    let mut parser = wasmparser::Parser::new(0);
    parser.set_features(features);
    let mut prefix = verified.map(VerifiedPrefix::new);
    let mut pending = Vec::new();
    let mut covered = 0;
    let mut index = 0;
    for payload in parser.parse_all(wasm) {
        let payload = payload.map_err(invalid)?;
        if let wasmparser::ValidPayload::Func(func, body) =
            validator.payload(&payload).map_err(invalid)?
        {
            if prefix
                .as_ref()
                .is_some_and(|prefix| prefix.covers(index, body.as_bytes()))
            {
                covered += 1;
            } else {
                pending.push((func, body));
            }
            index += 1;
        }
        // Observed after the validator accepted the section, so an
        // out-of-order or repeated section is its error to report and never
        // a prefix this bookkeeping believes.
        if let Some(prefix) = prefix.as_mut() {
            prefix.observe(&payload);
        }
    }
    let mut allocations = wasmparser::FuncValidatorAllocations::default();
    for (func, body) in pending {
        let mut function = func.into_validator(allocations);
        function.validate(&body).map_err(invalid)?;
        allocations = function.into_allocations();
    }
    Ok(covered)
}

/// Section-by-section record of whether an artifact's low index spaces equal
/// a frozen runtime's, so [`validate_hardened`] may treat a byte-identical
/// body as already validated. Every flag starts false and stays false for a
/// section that is missing or fails to read: an unreadable section is the
/// validator's error to report, and until then nothing is covered.
// One flag per index space the proof in `validate_hardened` names; folding
// them into a counter would hide which space failed to match.
#[allow(clippy::struct_excessive_bools)]
struct VerifiedPrefix<'a> {
    runtime: &'a runtime::Runtime,
    types: bool,
    functions: bool,
    globals: bool,
    memory: bool,
    data_count: bool,
}

impl<'a> VerifiedPrefix<'a> {
    fn new(runtime: &'a runtime::Runtime) -> Self {
        VerifiedPrefix {
            runtime,
            types: false,
            functions: false,
            globals: false,
            memory: false,
            data_count: false,
        }
    }

    fn observe(&mut self, payload: &wasmparser::Payload<'_>) {
        use wasmparser::Payload;
        match payload {
            Payload::TypeSection(reader) => {
                let mut groups = reader.clone().into_iter();
                self.types = self.runtime.canonical_types().iter().all(|want| {
                    groups.next().and_then(Result::ok).is_some_and(|group| {
                        !group.is_explicit_rec_group()
                            && group.types().len() == 1
                            && group.types().next() == Some(want)
                    })
                });
            }
            Payload::FunctionSection(reader) => {
                let mut entries = reader.clone().into_iter();
                self.functions = self
                    .runtime
                    .canonical_function_types()
                    .iter()
                    .all(|&want| entries.next().and_then(Result::ok) == Some(want));
            }
            Payload::GlobalSection(reader) => {
                let mut entries = reader.clone().into_iter();
                self.globals = self.runtime.canonical_global_types().all(|want| {
                    entries
                        .next()
                        .and_then(Result::ok)
                        .is_some_and(|global| global.ty == *want)
                });
            }
            Payload::MemorySection(reader) => {
                let mut entries = reader.clone().into_iter();
                let first = entries.next().and_then(Result::ok);
                self.memory = first.is_some_and(|memory| !memory.memory64 && !memory.shared)
                    && entries.next().is_none();
            }
            Payload::DataCountSection { count, .. } => {
                self.data_count = *count as usize >= self.runtime.data_segment_count();
            }
            _ => {}
        }
    }

    /// Whether body `index` with these bytes is a verified copy.
    fn covers(&self, index: usize, body: &[u8]) -> bool {
        self.types
            && self.functions
            && self.globals
            && self.memory
            && self.data_count
            && self.runtime.canonical_body(index) == Some(body)
    }
}

/// The metadata half of [`audit_standalone_wasm`]: present, ≤ 1 KiB, valid JSON,
/// `abi_version` 1,
/// the pinned `type` enum, and the strategy⇔`signal` biconditional with the
/// full signal shape.
fn audit_standalone_wasm_metadata(metadata: Option<&[u8]>) -> Result<(), AotError> {
    let fail = |error: String| AotError::StandaloneWasmMetadata(error);
    let bytes = metadata.ok_or_else(|| fail("module_metadata section missing".to_string()))?;
    if bytes.len() > schema::MODULE_METADATA_MAX_BYTES {
        return Err(fail(format!(
            "module_metadata is {} bytes (cap {})",
            bytes.len(),
            schema::MODULE_METADATA_MAX_BYTES
        )));
    }
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| fail(format!("module_metadata is not valid JSON: {error}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| fail("module_metadata is not a JSON object".to_string()))?;
    if object
        .get("abi_version")
        .and_then(serde_json::Value::as_u64)
        != Some(1)
    {
        return Err(fail("abi_version missing or not 1".to_string()));
    }
    let kind = object
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| fail("type missing or not a string".to_string()))?;
    if kind != "strategy" && kind != "indicator" {
        return Err(fail(format!(
            "type {kind:?} not in {{strategy, indicator}}"
        )));
    }
    let signal = object.get("signal");
    if (kind == "strategy") != signal.is_some() {
        return Err(fail(if kind == "strategy" {
            "strategy module designates no signal series".to_string()
        } else {
            "indicator module carries a signal object".to_string()
        }));
    }
    let Some(signal) = signal else {
        return Ok(());
    };
    let signal = signal
        .as_object()
        .ok_or_else(|| fail("signal is not a JSON object".to_string()))?;
    for key in signal.keys() {
        if !matches!(key.as_str(), "source" | "semantics" | "price_source") {
            return Err(fail(format!("signal carries unknown key {key:?}")));
        }
    }
    let semantics = signal.get("semantics").and_then(serde_json::Value::as_str);
    if !matches!(semantics, Some("position-events" | "posture-events")) {
        return Err(fail(
            "signal.semantics missing or not in {position-events, posture-events}".to_string(),
        ));
    }
    // `posture-events` may come either from a strategy() broker lane or from
    // an explicitly designated indicator posture plot. The source string is
    // therefore intentionally opaque to the consumer audit.
    if signal
        .get("price_source")
        .and_then(serde_json::Value::as_str)
        != Some("close")
    {
        return Err(fail("signal.price_source missing or not close".to_string()));
    }
    if let Some(source) = signal.get("source") {
        let ok = source.as_str().is_some_and(|s| (1..=64).contains(&s.len()));
        if !ok {
            return Err(fail("signal.source is not a 1-64-byte string".to_string()));
        }
    }
    Ok(())
}

/// Compile any valid CSIL program to an executable WebAssembly module.
///
/// Native lowering remains the preferred tier. When its deliberately
/// narrower type/opcode analysis rejects a valid program, this emits a small
/// ABI module whose exported `run` calls `host.interpret`. An ABI-compatible
/// host must bind the wrapper to the exact validated program out of band and
/// execute it with the same VM used by direct CSIL runs. Validation errors are
/// never hidden behind hosted mode.
pub fn compile_program_to_wasm(program: &CsilProgram) -> Result<AotModule, AotError> {
    match compile_program(program) {
        Ok(module) => Ok(module),
        Err(reason) => {
            // Native preflight already validates most hosted candidates, but
            // it deliberately rejects multiple functions before analysis.
            // Re-run the universal contract here so invalid/tampered CSIL can
            // never be disguised as a hosted artifact. The native fast path
            // pays no duplicate validation.
            portable_preflight(program)?;
            let layout = hosted_layout(program, reason.to_string());
            let wasm = emit::emit_interpreter_hosted(program);
            Ok(AotModule { wasm, layout })
        }
    }
}

/// Validation shared by every executable Wasm tier. Unlike native
/// `preflight`, multiple CSIL functions are legal because the interpreter
/// supports them; a program without an entry function is not executable.
fn portable_preflight(program: &CsilProgram) -> Result<(), AotError> {
    if program.header.csil_version != CSIL_VERSION {
        return Err(AotError::CsilVersionMismatch(program.header.csil_version));
    }
    if let Err(errors) = pinewasm_csil::validate(program) {
        let first = errors.first().map_or_else(
            || "unknown validation error".to_string(),
            ToString::to_string,
        );
        return Err(AotError::InvalidProgram(first));
    }
    if program.functions.is_empty() {
        return Err(AotError::NoFunctions);
    }
    Ok(())
}

fn hosted_layout(program: &CsilProgram, native_blocker: String) -> Layout {
    Layout {
        style_slots: Vec::new(),
        execution_mode: ExecutionMode::InterpreterHosted,
        native_blocker: Some(native_blocker),
        native_heap: false,
        native_heap_hot: false,
        distance_scan_kernels: 0,
        vm_heap_residual: false,
        // Hosted execution does not expose native lanes, but retaining the
        // program's public counts keeps build metadata useful to tooling.
        ring: 1,
        slot_count: program.series_slots.len() as u32,
        plot_count: program.plots.len() as u32,
        max_stack: program.main().max_stack,
        plot_color_roles: program
            .plots
            .iter()
            .map(|plot| plot.color_roles.len() as u32)
            .collect(),
        visual_color_roles: program
            .visuals
            .iter()
            .map(|visual| visual.color_roles.len() as u32)
            .collect(),
        fill_color_roles: program
            .fills
            .iter()
            .map(|fill| fill.color_roles.len() as u32)
            .collect(),
        source_inputs: Vec::new(),
        security_lanes: 0,
        links_ta: false,
        ta_decls: Vec::new(),
        ta_layout: pinewasm_ta::Layout::default(),
        links_broker: false,
        host_broker: false,
        links_runtime: false,
        used_pure_ids: [false; pinewasm_num::id::COUNT as usize],
        alert_sites: 0,
        imports_vm: false,
        imports_interpreter: true,
        host_sites: Vec::new(),
        host_sites_in_loops: 0,
        scratch_cells: 4,
    }
}

#[cfg(test)]
mod verified_copy_audit {
    use super::*;
    use pinewasm_compiler::{compile, CompileOptions};
    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
    use std::sync::OnceLock;

    /// A strategy, so the widest blob (numerics, JSON, ta, broker) is spliced.
    const STRATEGY: &str = "//@version=6\nstrategy(\"Verified copies\")\nlength = input.int(20, \"Length\", minval=1)\nbasis = ta.sma(close, length)\natr = ta.atr(length)\nif close > basis + atr\n    strategy.entry(\"L\", strategy.long)\nif close < basis\n    strategy.close(\"L\")\nplot(basis)\nplot(atr)\n";
    /// An indicator, so the standalone blob without the broker is spliced.
    const INDICATOR: &str =
        "//@version=6\nindicator(\"Verified copies\")\nplot(ta.sma(close, 5) - close)\n";

    fn emitted(
        source: &str,
        module_type: StandaloneWasmModuleType,
    ) -> (Vec<u8>, &'static runtime::Runtime) {
        let compiled = compile(source, CompileOptions::all());
        assert!(
            !compiled
                .diagnostics
                .iter()
                .any(pinewasm_span::Diagnostic::is_error),
            "{:?}",
            compiled.diagnostics
        );
        let program = compiled.program.expect("program");
        emit_standalone_wasm_unaudited(
            &program,
            &StandaloneWasmOptions {
                module_type,
                signal_plot: None,
            },
            &StandaloneWasmBuildConfig::default(),
        )
        .expect("standalone WebAssembly emission")
    }

    fn strategy_artifact() -> &'static (Vec<u8>, &'static runtime::Runtime) {
        static ARTIFACT: OnceLock<(Vec<u8>, &'static runtime::Runtime)> = OnceLock::new();
        ARTIFACT.get_or_init(|| emitted(STRATEGY, StandaloneWasmModuleType::Strategy))
    }

    /// Bodies byte-identical to the canonical body at the same index, counted
    /// by a plain parse rather than by the audit's own bookkeeping.
    fn identical_copies(wasm: &[u8], linked: &runtime::Runtime) -> usize {
        let mut index = 0;
        let mut identical = 0;
        for payload in wasmparser::Parser::new(0).parse_all(wasm) {
            if let wasmparser::Payload::CodeSectionEntry(body) =
                payload.expect("emitted bytes parse")
            {
                if linked.canonical_body(index) == Some(body.as_bytes()) {
                    identical += 1;
                }
                index += 1;
            }
        }
        identical
    }

    fn verdict(result: Result<usize, AotError>) -> Result<(), String> {
        result.map(|_| ()).map_err(|error| error.to_string())
    }

    #[test]
    fn the_producer_audit_covers_exactly_the_verified_copies() {
        for (source, module_type) in [
            (STRATEGY, StandaloneWasmModuleType::Strategy),
            (INDICATOR, StandaloneWasmModuleType::Indicator),
        ] {
            let (wasm, linked) = emitted(source, module_type);
            let identical = identical_copies(&wasm, linked);
            assert!(identical > 0, "the splice copies runtime bodies verbatim");
            assert!(
                identical < linked.func_count() as usize,
                "tree shaking leaves stubs, which are not copies"
            );
            assert_eq!(
                validate_hardened(&wasm, Some(linked)).expect("emitted bytes validate"),
                identical
            );
            assert_eq!(
                validate_hardened(&wasm, None).expect("emitted bytes validate"),
                0
            );
            audit_emitted_standalone_wasm(&wasm, linked).expect("producer audit");
            audit_standalone_wasm(&wasm).expect("the complete public audit agrees");
        }
    }

    #[test]
    fn a_blob_that_was_not_spliced_proves_nothing() {
        let (wasm, linked) = strategy_artifact();
        for other in [
            runtime::runtime(),
            runtime::runtime_ta(),
            runtime::runtime_standalone_wasm(),
        ] {
            assert!(!std::ptr::eq(*linked, other));
            assert_eq!(
                validate_hardened(wasm, Some(other)).expect("still valid"),
                0,
                "a different prefix must fall back to validating every body"
            );
        }
    }

    #[test]
    fn a_changed_runtime_byte_is_validated_rather_than_trusted() {
        let (wasm, linked) = strategy_artifact();
        let identical = identical_copies(wasm, linked);
        // The midpoint of the first copied body.
        let mut index = 0;
        let mut target = None;
        for payload in wasmparser::Parser::new(0).parse_all(wasm) {
            if let wasmparser::Payload::CodeSectionEntry(body) = payload.expect("parse") {
                if target.is_none() && linked.canonical_body(index) == Some(body.as_bytes()) {
                    let range = body.range();
                    target = Some((range.start + (range.end - range.start) / 2) as usize);
                }
                index += 1;
            }
        }
        let mut mutated = wasm.clone();
        mutated[target.expect("some body is a copy")] ^= 0xff;
        match validate_hardened(&mutated, Some(*linked)) {
            Ok(covered) => {
                assert_eq!(
                    covered,
                    identical - 1,
                    "the changed body is no longer covered"
                );
                validate_hardened(&mutated, None)
                    .expect("the full audit accepts what the producer audit accepted");
            }
            Err(error) => {
                let full = validate_hardened(&mutated, None)
                    .expect_err("the full audit rejects what the producer audit rejected");
                assert_eq!(error.to_string(), full.to_string());
            }
        }
    }

    #[test]
    fn mutated_artifacts_receive_the_same_verdict_from_both_audits() {
        let (wasm, linked) = strategy_artifact();
        let mut runner = TestRunner::new_with_rng(
            Config {
                cases: 256,
                failure_persistence: None,
                max_shrink_iters: 1_024,
                ..Config::default()
            },
            TestRng::from_seed(RngAlgorithm::ChaCha, &[0x34; 32]),
        );
        let edits = proptest::collection::vec((0..wasm.len(), any::<u8>()), 1..=4);
        runner
            .run(&edits, |edits| {
                let mut mutated = wasm.clone();
                for (at, byte) in edits {
                    mutated[at] = byte;
                }
                prop_assert_eq!(
                    verdict(validate_hardened(&mutated, Some(*linked))),
                    verdict(validate_hardened(&mutated, None))
                );
                let producer =
                    audit_emitted_standalone_wasm(&mutated, linked).map_err(|e| e.to_string());
                let public = audit_standalone_wasm(&mutated).map_err(|e| e.to_string());
                prop_assert_eq!(producer, public);
                Ok(())
            })
            .expect("every mutation receives the same verdict from both audits");
    }
}
