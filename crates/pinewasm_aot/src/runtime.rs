//! The numeric runtime linked into emitted artifacts (docs/aot.md §5).
//!
//! [`MODULE_WASM`] is [`pinewasm_num`] compiled for
//! `wasm32-unknown-unknown` — the same crate the PineWASM interpreter
//! evaluates `math.exp` and friends with, so the compiled result is
//! bit-identical rather than merely close. The internal runtime splicer copies its
//! functions into an artifact under construction, which is how a program
//! using transcendentals compiles to a module with an empty import section
//! instead of one importing `host.pure`.
//!
//! # Why a linked module rather than emitted instruction sequences
//!
//! Wasm has exact instructions for the easy half of the pure set, and
//! `body.rs` already inlines those (`abs`, `min`, `max`, `floor`, `ceil`,
//! `sqrt`, `round`, `sign`). It has nothing for `exp`, `log`, `pow`, or trig.
//! Hand-emitting those kernels would mean porting ~2,000 lines of libm-style
//! double arithmetic — including `rem_pio2_large`'s 66-entry table — into
//! `wasm_encoder` calls, where a single transposed constant is a last-ulp bug
//! that shows up only on some inputs. Compiling the real crate and linking it
//! keeps one implementation and one place to be wrong.
//!
//! # Index discipline
//!
//! The runtime's functions, globals, and types are spliced in *first*, so its
//! type and global indices survive unchanged. Function indices do not: wasm
//! puts imported functions before defined ones, so an artifact that still
//! imports `host.vm` shifts every defined function up by its import count. The
//! runtime splicer re-encodes the runtime's bodies through that shift.
//!
//! # Memory
//!
//! The runtime's rodata is spliced at its original addresses — relocating it
//! would mean rewriting every address constant in its code — so those
//! addresses become part of the artifact's memory map. The whole reserved
//! block (the runtime's reserved address range, including rodata plus the
//! shadow stack) sits inside the module's first page, and `prepare` starts the
//! artifact's own lanes above it. The header and input table live below the
//! block, which is what bounds an artifact to [`MAX_INPUTS`] inputs.

use crate::AotError;
use std::sync::OnceLock;
use wasm_encoder::reencode::Reencode;
use wasm_encoder::{
    CodeSection, ConstExpr, DataSection, Function, FunctionSection, GlobalSection, Instruction,
    TypeSection,
};

/// [`pinewasm_num`] compiled to WebAssembly. Regenerate with
/// `cargo xtask aot-runtime-all` after any change to that crate; the differential sweep
/// in `tests/runtime_numerics.rs` fails when this is stale.
///
/// # Why separate blobs
///
/// This is the *base* blob, spliced into every chart-interface artifact. Its bytes are a
/// reproducibility contract for a fixed source tree, toolchain, and build
/// recipe: rebuilding that tree must reproduce them byte-for-byte. The
/// runtime splicer copies data segments verbatim — only function bodies are
/// tree-shaken to stubs — so
/// a single combined blob would push the standalone WebAssembly family's ~25 KiB of rodata
/// (float formatting and parsing tables, error strings), the TA engine, and the broker into
/// artifacts that do not use them. The optional families therefore live in
/// separate variants built from the same crate; each splice selects only the
/// smallest variant that satisfies the artifact.
pub const MODULE_WASM: &[u8] = include_bytes!("../runtime/pinewasm_aot_runtime.wasm");

/// The same runtime crate built with `--features standalone_wasm`: the twelve numeric
/// exports plus the `standalone_wasm_*` family (JSON scanner, serializer, and allocator)
/// and their rodata. Spliced only into standalone WebAssembly artifacts; see
/// [`MODULE_WASM`] for why it is a separate blob. Regenerate with
/// `cargo xtask aot-runtime-all`.
pub const MODULE_WASM_STANDALONE_WASM: &[u8] =
    include_bytes!("../runtime/pinewasm_aot_runtime_standalone_wasm.wasm");

/// The same runtime crate built with `--features ta`: the twelve numeric
/// exports plus the stateful `ta.*` engine (`pinewasm_rt_ta_init`/`pinewasm_rt_ta_advance`).
/// Spliced into every chart-interface artifact with a `CallTa` site; see
/// [`MODULE_WASM`] for why it is a separate blob. Regenerate with
/// `cargo xtask aot-runtime-all`.
pub const MODULE_WASM_TA: &[u8] = include_bytes!("../runtime/pinewasm_aot_runtime_ta.wasm");

/// The same runtime crate built with `--features broker`: the standalone WebAssembly blob's
/// families plus the `pinewasm_rt_broker_*` surface — the shared
/// `pinewasm_broker` emulator behind emitted strategy artifacts and
/// `standalone_wasm_serialize_posture`, the flat-aware posture serializer.
/// Spliced only into `strategy()` standalone WebAssembly artifacts; see [`MODULE_WASM`] for why
/// it is a separate blob. Regenerate with `cargo xtask aot-runtime-all`.
pub const MODULE_WASM_BROKER: &[u8] = include_bytes!("../runtime/pinewasm_aot_runtime_broker.wasm");

/// Shadow stack handed to the spliced runtime, above its rodata. The deepest
/// user is `rem_pio2_large` at a few hundred bytes; the rest is margin, and it
/// is free because the whole reserved block still fits in the first 64 KiB
/// page the module allocates anyway.
const STACK_SIZE: u32 = 16 * 1024;

/// Inputs an artifact may declare once the runtime is linked: the input table
/// grows upward from [`crate::header::SIZE`] and must stay below the runtime's
/// rodata. Pine scripts run to dozens of inputs, so this bound exists to be
/// checked rather than to be approached.
pub const MAX_INPUTS: u32 = 500;

/// Exported names, indexed by [`pinewasm_num::id`].
const EXPORT_NAMES: [&str; pinewasm_num::id::COUNT as usize] = [
    "pinewasm_rt_exp",
    "pinewasm_rt_log",
    "pinewasm_rt_log10",
    "pinewasm_rt_sin",
    "pinewasm_rt_cos",
    "pinewasm_rt_tan",
    "pinewasm_rt_asin",
    "pinewasm_rt_acos",
    "pinewasm_rt_atan",
    "pinewasm_rt_pow",
    "pinewasm_rt_round2",
    "pinewasm_rt_rem",
];

/// Ids of the blob's second export family: the standalone WebAssembly JSON/allocator machinery
/// in `runtime/src/standalone_wasm.rs`. A parallel registry rather than an
/// extension of [`pinewasm_num::id`], because that id space is
/// ABI-versioned append-only and these are not pure `(f64, f64) -> f64`
/// numerics; the splice is signature-agnostic, so nothing else changes.
pub(crate) mod standalone_wasm {
    pub const ALLOC: u32 = 0;
    pub const DEALLOC: u32 = 1;
    pub const LAST_ERROR: u32 = 2;
    pub const RESET: u32 = 3;
    pub const SCAN: u32 = 4;
    pub const ARENA_RELOC: u32 = 5;
    pub const FILL_LANES: u32 = 6;
    pub const SERIALIZE: u32 = 7;
    pub const SET_STATUS_ERROR: u32 = 8;
    pub const SET_BAR_BOUND_ERROR: u32 = 9;
    /// The request's chart timeframe into its scratch cells.
    pub const FILL_TIMEFRAME: u32 = 10;
    /// A civil part of a time in a fixed timezone (`runtime/src/calendar.rs`).
    pub const CALENDAR: u32 = 11;
    /// The time of a `timestamp` call's fields, in UTC or a fixed timezone
    /// (`runtime/src/calendar.rs`).
    pub const TIMESTAMP: u32 = 12;
    pub const COUNT: u32 = 13;
    /// Members a module keeps only when it calls them; the rest of the
    /// family is kept in every standalone WebAssembly module.
    pub const OPTIONAL: [u32; 3] = [FILL_TIMEFRAME, CALENDAR, TIMESTAMP];
}

/// Ids of the blob's `ta` export family: the stateful engine that replaced
/// the `host.ta` import.
pub(crate) mod ta {
    pub const INIT: u32 = 0;
    pub const ADVANCE: u32 = 1;
    pub const COUNT: u32 = 2;
}

/// Exported names of the `ta` family, indexed by [`ta`].
const TA_EXPORT_NAMES: [&str; ta::COUNT as usize] =
    ["pinewasm_rt_ta_init", "pinewasm_rt_ta_advance"];

/// Ids of the blob's `broker` export family: the strategy broker
/// emulator behind emitted strategy artifacts. The 13 placement entries
/// mirror `pinewasm_hir::OrderKind` one-to-one (see the blob's
/// `runtime/src/broker.rs` for the operand-cell encoding).
pub(crate) mod broker {
    pub const INIT: u32 = 0;
    pub const BEGIN_BAR: u32 = 1;
    pub const FINISH_BAR: u32 = 2;
    pub const ENTRY: u32 = 3;
    pub const ORDER: u32 = 4;
    pub const EXIT: u32 = 5;
    pub const CLOSE: u32 = 6;
    pub const CLOSE_ALL: u32 = 7;
    pub const CANCEL: u32 = 8;
    pub const CANCEL_ALL: u32 = 9;
    pub const RISK_MAX_POSITION_SIZE: u32 = 10;
    pub const RISK_ALLOW_ENTRY_IN: u32 = 11;
    pub const RISK_MAX_DRAWDOWN: u32 = 12;
    pub const RISK_MAX_INTRADAY_LOSS: u32 = 13;
    pub const RISK_MAX_INTRADAY_FILLED_ORDERS: u32 = 14;
    pub const RISK_MAX_CONS_LOSS_DAYS: u32 = 15;
    pub const SNAP_F64: u32 = 16;
    pub const SNAP_I64: u32 = 17;
    pub const TRADE_F64: u32 = 18;
    pub const TRADE_I64: u32 = 19;
    pub const SERIALIZE_POSTURE: u32 = 20;
    pub const COUNT: u32 = 21;
}

/// Exported names of the `broker` family, indexed by [`broker`].
const BROKER_EXPORT_NAMES: [&str; broker::COUNT as usize] = [
    "pinewasm_rt_broker_init",
    "pinewasm_rt_broker_begin_bar",
    "pinewasm_rt_broker_finish_bar",
    "pinewasm_rt_broker_entry",
    "pinewasm_rt_broker_order",
    "pinewasm_rt_broker_exit",
    "pinewasm_rt_broker_close",
    "pinewasm_rt_broker_close_all",
    "pinewasm_rt_broker_cancel",
    "pinewasm_rt_broker_cancel_all",
    "pinewasm_rt_broker_risk_max_position_size",
    "pinewasm_rt_broker_risk_allow_entry_in",
    "pinewasm_rt_broker_risk_max_drawdown",
    "pinewasm_rt_broker_risk_max_intraday_loss",
    "pinewasm_rt_broker_risk_max_intraday_filled_orders",
    "pinewasm_rt_broker_risk_max_cons_loss_days",
    "pinewasm_rt_broker_snap_f64",
    "pinewasm_rt_broker_snap_i64",
    "pinewasm_rt_broker_trade_f64",
    "pinewasm_rt_broker_trade_i64",
    "standalone_wasm_serialize_posture",
];

/// Ids of the chart runtime's export family (`runtime/src/chart.rs`): the
/// PineWASM interpreter VM as a chart module's own host.
pub(crate) mod chart {
    /// `chartAlloc`, and the module's native-heap allocator.
    pub const ALLOC: u32 = 0;
    /// The target of the module's `chartInit` wrapper.
    pub const INIT: u32 = 1;
    /// What `host.vm` is for a module that imports its host.
    pub const VM: u32 = 2;
    /// `chartCommand`.
    pub const COMMAND: u32 = 3;
    /// Whether the module's `run` may start, answered once.
    pub const BEGIN: u32 = 4;
    /// The status the module's own stop becomes.
    pub const STOP: u32 = 5;
    /// [`VM`] for a module that keeps the string budget.
    pub const VM_STRINGS: u32 = 6;
    /// What is left of the run's string budget.
    pub const STRING_ROOM: u32 = 7;
    /// The reservation that did not fit what is left.
    pub const STRING_RESERVE: u32 = 8;
    /// The length of a run string.
    pub const STRING_LEN: u32 = 9;
    pub const COUNT: u32 = 10;
}

/// Exported names of the chart family, indexed by [`chart`].
const CHART_EXPORT_NAMES: [&str; chart::COUNT as usize] = [
    "pinewasm_rt_chart_alloc",
    "pinewasm_rt_chart_init",
    "pinewasm_rt_chart_vm",
    "pinewasm_rt_chart_command",
    "pinewasm_rt_chart_begin",
    "pinewasm_rt_chart_stop",
    "pinewasm_rt_chart_vm_strings",
    "pinewasm_rt_chart_string_room",
    "pinewasm_rt_chart_string_reserve",
    "pinewasm_rt_chart_string_len",
];

/// The chart runtime's shadow stack, above its data. It runs the PineWASM
/// interpreter VM, whose deepest paths (formatting, regular expressions, the request
/// parser) need what a default Rust Wasm build gives them.
const CHART_STACK_SIZE: u32 = 1024 * 1024;

/// Exported names of the standalone WebAssembly family, indexed by [`standalone_wasm`].
const STANDALONE_WASM_EXPORT_NAMES: [&str; standalone_wasm::COUNT as usize] = [
    "standalone_wasm_alloc",
    "standalone_wasm_dealloc",
    "standalone_wasm_last_error",
    "standalone_wasm_reset",
    "standalone_wasm_scan",
    "standalone_wasm_arena_reloc",
    "standalone_wasm_fill_lanes",
    "standalone_wasm_serialize",
    "standalone_wasm_set_status_error",
    "standalone_wasm_set_bar_bound_error",
    "standalone_wasm_fill_timeframe",
    "standalone_wasm_calendar",
    "standalone_wasm_timestamp",
];

/// Which build of the runtime crate a blob is, and so which export
/// families [`Runtime::parse`] requires of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Variant {
    /// The numeric surface alone.
    Base,
    /// `--features ta`.
    Ta,
    /// `--features standalone_wasm`, which implies `ta`.
    StandaloneWasm,
    /// `--features broker`, which implies `standalone_wasm`.
    Broker,
    /// `--features chart`, which implies `ta`.
    Chart,
}

impl Variant {
    fn standalone_wasm(self) -> bool {
        matches!(self, Variant::StandaloneWasm | Variant::Broker)
    }

    fn ta(self) -> bool {
        self != Variant::Base
    }

    fn broker(self) -> bool {
        self == Variant::Broker
    }

    fn chart(self) -> bool {
        self == Variant::Chart
    }
}

/// The parsed runtime module, ready to splice.
pub(crate) struct Runtime {
    types: Vec<wasmparser::FuncType>,
    /// The same types as parsed, one implicit rec group each: what an
    /// artifact's type prefix must equal for the producer audit to treat a
    /// copied body as already validated.
    sub_types: Vec<wasmparser::SubType>,
    /// Type index per defined function.
    func_types: Vec<u32>,
    globals: Vec<(wasmparser::GlobalType, i64)>,
    /// Function bodies, in definition order.
    bodies: Vec<wasmparser::FunctionBody<'static>>,
    /// Immutable direct-call edges derived from this blob at compiler build time.
    direct_calls: &'static [&'static [u32]],
    /// Active data segments: `(address, bytes)`.
    data: Vec<(u32, &'static [u8])>,
    /// Defined-function index per [`chart`] id, for the chart runtime.
    chart_entry: Option<[u32; chart::COUNT as usize]>,
    /// The chart runtime's table and element sections, verbatim. It is the
    /// one runtime that calls through a table (`std`'s formatting and the
    /// VM's regular expressions do); its functions are spliced first and
    /// unshifted, so the sections' function indices hold in the artifact.
    table_section: Option<&'static [u8]>,
    element_section: Option<&'static [u8]>,
    /// Defined-function index (before any import shift) per pure id.
    entry: [u32; pinewasm_num::id::COUNT as usize],
    /// Defined-function index (before any import shift) per [`standalone_wasm`] id.
    /// `None` for the base blob, which has no `standalone_wasm_*` exports.
    standalone_wasm_entry: Option<[u32; standalone_wasm::COUNT as usize]>,
    /// Defined-function index (before any import shift) per [`ta`] id.
    /// `None` for the base blob, which has no engine.
    ta_entry: Option<[u32; ta::COUNT as usize]>,
    /// Defined-function index (before any import shift) per [`broker`] id.
    /// `None` for every blob but the broker one.
    broker_entry: Option<[u32; broker::COUNT as usize]>,
    /// Global index of `__stack_pointer`.
    stack_pointer: u32,
    reserved_hi: u32,
}

/// Parse [`MODULE_WASM`] (the base blob) once per process.
pub(crate) fn runtime() -> &'static Runtime {
    static PARSED: OnceLock<Runtime> = OnceLock::new();
    PARSED.get_or_init(|| {
        Runtime::parse(
            include_bytes!(concat!(env!("OUT_DIR"), "/pinewasm_aot_runtime.wasm")),
            Variant::Base,
            include!(concat!(
                env!("OUT_DIR"),
                "/pinewasm_aot_runtime.wasm.calls.rs"
            )),
        )
        .expect("the checked-in AOT runtime module is well-formed")
    })
}

/// Parse [`MODULE_WASM_TA`] (the ta blob) once per process.
pub(crate) fn runtime_ta() -> &'static Runtime {
    static PARSED: OnceLock<Runtime> = OnceLock::new();
    PARSED.get_or_init(|| {
        Runtime::parse(
            include_bytes!(concat!(env!("OUT_DIR"), "/pinewasm_aot_runtime_ta.wasm")),
            Variant::Ta,
            include!(concat!(
                env!("OUT_DIR"),
                "/pinewasm_aot_runtime_ta.wasm.calls.rs"
            )),
        )
        .expect("the checked-in AOT ta runtime module is well-formed")
    })
}

/// Parse [`MODULE_WASM_STANDALONE_WASM`] (the standalone WebAssembly blob) once per process. Only standalone WebAssembly
/// compiles pay for the parse; the three are separate [`OnceLock`]s.
pub(crate) fn runtime_standalone_wasm() -> &'static Runtime {
    static PARSED: OnceLock<Runtime> = OnceLock::new();
    PARSED.get_or_init(|| {
        Runtime::parse(
            include_bytes!(concat!(
                env!("OUT_DIR"),
                "/pinewasm_aot_runtime_standalone_wasm.wasm"
            )),
            Variant::StandaloneWasm,
            include!(concat!(
                env!("OUT_DIR"),
                "/pinewasm_aot_runtime_standalone_wasm.wasm.calls.rs"
            )),
        )
        .expect("the checked-in AOT standalone WebAssembly runtime module is well-formed")
    })
}

/// Parse [`MODULE_WASM_BROKER`] (the broker blob) once per process.
pub(crate) fn runtime_broker() -> &'static Runtime {
    static PARSED: OnceLock<Runtime> = OnceLock::new();
    PARSED.get_or_init(|| {
        Runtime::parse(
            include_bytes!(concat!(
                env!("OUT_DIR"),
                "/pinewasm_aot_runtime_broker.wasm"
            )),
            Variant::Broker,
            include!(concat!(
                env!("OUT_DIR"),
                "/pinewasm_aot_runtime_broker.wasm.calls.rs"
            )),
        )
        .expect("the checked-in AOT broker runtime module is well-formed")
    })
}

/// The runtime crate built with `--features chart`: the `ta` blob's
/// families plus the `pinewasm_rt_chart_*` surface, the PineWASM interpreter VM as a
/// chart module's linked host (`runtime/src/chart.rs`). `None` in a compiler
/// built without one.
///
/// Unlike the four blobs above this one is not checked in. It holds the
/// PineWASM interpreter VM, so nearly every compiler change changes its bytes, and a
/// checked-in copy would have to be rebuilt after nearly every change to the crate. `cargo
/// xtask aot-runtime-chart` builds it from the same source tree and
/// `build.rs` links it when `PINEWASM_AOT_CHART_RUNTIME` names it; `cargo
/// xtask wasm` does both for the JavaScript package.
#[cfg(pinewasm_chart_runtime)]
pub(crate) fn runtime_chart() -> Option<&'static Runtime> {
    static PARSED: OnceLock<Runtime> = OnceLock::new();
    Some(PARSED.get_or_init(|| {
        Runtime::parse(
            include_bytes!(concat!(env!("OUT_DIR"), "/pinewasm_aot_runtime_chart.wasm")),
            Variant::Chart,
            &[],
        )
        .expect("the linked chart runtime is well-formed")
    }))
}

#[cfg(not(pinewasm_chart_runtime))]
pub(crate) fn runtime_chart() -> Option<&'static Runtime> {
    None
}

/// The blob an artifact splices, by what it needs. Strategy standalone WebAssembly artifacts
/// take the broker blob (all three families); other standalone WebAssembly artifacts take the
/// standalone WebAssembly blob; a program using `ta.*` but not the standalone WebAssembly surface takes the middle
/// one; everything else uses the base blob from the same reproducible build.
pub(crate) fn runtime_for(standalone_wasm: bool, ta: bool, broker: bool) -> &'static Runtime {
    match (broker, standalone_wasm, ta) {
        (true, _, _) => runtime_broker(),
        (false, true, _) => runtime_standalone_wasm(),
        (false, false, true) => runtime_ta(),
        (false, false, false) => runtime(),
    }
}

impl Runtime {
    /// Function index of chart-family export `id` (a [`chart`] constant). A
    /// chart module has no imports, so the index is unshifted.
    ///
    /// # Panics
    ///
    /// On a runtime other than [`runtime_chart`].
    pub(crate) fn chart_entry(&self, id: u32) -> u32 {
        self.chart_entry
            .expect("only the chart runtime has the chart family")[id as usize]
    }

    /// The runtime's table section, for the one runtime that has one.
    pub(crate) fn table_section(&self) -> Option<wasm_encoder::RawSection<'static>> {
        self.table_section.map(|data| wasm_encoder::RawSection {
            id: wasm_encoder::SectionId::Table as u8,
            data,
        })
    }

    /// The runtime's element section, for the one runtime that has one.
    pub(crate) fn element_section(&self) -> Option<wasm_encoder::RawSection<'static>> {
        self.element_section.map(|data| wasm_encoder::RawSection {
            id: wasm_encoder::SectionId::Element as u8,
            data,
        })
    }

    /// Number of functions the runtime contributes; the artifact's own
    /// functions are numbered after them.
    pub(crate) fn func_count(&self) -> u32 {
        self.bodies.len() as u32
    }

    /// The blob's type section in order, exactly the prefix `splice` re-emits
    /// into an artifact.
    pub(crate) fn canonical_types(&self) -> &[wasmparser::SubType] {
        &self.sub_types
    }

    /// The blob's function section: the type index of every defined function,
    /// exactly the prefix `splice` re-emits.
    pub(crate) fn canonical_function_types(&self) -> &[u32] {
        &self.func_types
    }

    /// The blob's global types in order, without their initializers; a body's
    /// validity depends on the former and never on the latter.
    pub(crate) fn canonical_global_types(&self) -> impl Iterator<Item = &wasmparser::GlobalType> {
        self.globals.iter().map(|(ty, _)| ty)
    }

    /// How many data segments `splice` copies, all ahead of the artifact's own.
    pub(crate) fn data_segment_count(&self) -> usize {
        self.data.len()
    }

    /// Canonical body `index` as `splice` copies it — locals and code, without
    /// the size prefix — or `None` past the blob's functions.
    pub(crate) fn canonical_body(&self, index: usize) -> Option<&'static [u8]> {
        self.bodies
            .get(index)
            .map(wasmparser::FunctionBody::as_bytes)
    }

    /// Number of globals the runtime contributes.
    pub(crate) fn global_count(&self) -> u32 {
        self.globals.len() as u32
    }

    /// Number of types the runtime contributes.
    pub(crate) fn type_count(&self) -> u32 {
        self.types.len() as u32
    }

    /// Function index of a [`pinewasm_num`] pure id, given how many
    /// functions the artifact imports.
    pub(crate) fn entry(&self, id: u32, import_count: u32) -> u32 {
        shift_function_index(self.entry[id as usize], import_count)
    }

    /// Function index of an [`standalone_wasm`] family member, given how many functions
    /// the artifact imports (always zero in standalone WebAssembly mode, which refuses imports;
    /// the parameter keeps the two registries symmetric).
    ///
    /// Only meaningful on [`runtime_standalone_wasm`]'s instance; the base blob exports
    /// no `standalone_wasm_*` family, and asking it is a compile-path mix-up.
    pub(crate) fn standalone_wasm_entry(&self, id: u32, import_count: u32) -> u32 {
        let entries = self.standalone_wasm_entry.expect(
            "standalone_wasm_entry queried on the base runtime blob; use runtime_standalone_wasm()",
        );
        shift_function_index(entries[id as usize], import_count)
    }

    /// Function index of a [`ta`] family member, given how many functions the
    /// artifact imports.
    ///
    /// Only meaningful on a blob built with the `ta` feature; the base blob
    /// carries no engine, and asking it is a compile-path mix-up.
    pub(crate) fn ta_entry(&self, id: u32, import_count: u32) -> u32 {
        let entries = self
            .ta_entry
            .expect("ta_entry queried on a runtime blob without the engine");
        shift_function_index(entries[id as usize], import_count)
    }

    /// Function index of a [`broker`] family member, given how many
    /// functions the artifact imports (always zero: strategy artifacts are
    /// standalone WebAssembly-only, and standalone WebAssembly refuses imports).
    ///
    /// Only meaningful on [`runtime_broker`]'s instance; every other blob
    /// carries no emulator, and asking one is a compile-path mix-up.
    pub(crate) fn broker_entry(&self, id: u32, import_count: u32) -> u32 {
        let entries = self
            .broker_entry
            .expect("broker_entry queried on a runtime blob without the emulator");
        shift_function_index(entries[id as usize], import_count)
    }

    /// Number of active data segments the runtime contributes; an artifact's
    /// own segments (the standalone WebAssembly key table) are numbered after them.
    pub(crate) fn data_count(&self) -> u32 {
        self.data.len() as u32
    }

    /// Global index of the shadow-stack pointer, after the artifact's own
    /// globals are appended (the runtime's come first, so this is unshifted).
    pub(crate) fn stack_pointer_global(&self) -> u32 {
        self.stack_pointer
    }

    /// First byte above the reserved block: where the artifact's own lanes
    /// may start, and the value `prepare` gives the shadow-stack pointer
    /// (the stack grows down from here).
    pub(crate) fn reserved_hi(&self) -> u32 {
        self.reserved_hi
    }

    /// Which defined functions are reachable from the pure ids in `used`,
    /// plus the [`standalone_wasm`] family when `standalone_wasm_used`: all of
    /// it but its [`standalone_wasm::OPTIONAL`] members, of which only those
    /// in `optional` are kept.
    ///
    /// The splice already rejects a runtime module with a table or element
    /// section, so every call is direct and a `call`-edge walk is the whole
    /// call graph: nothing can be entered through a function pointer.
    fn reachable(
        &self,
        used: &[bool; pinewasm_num::id::COUNT as usize],
        standalone_wasm_used: bool,
        optional: &[u32],
        ta_used: bool,
        broker_used: bool,
    ) -> Vec<bool> {
        let standalone_wasm_roots = standalone_wasm_used
            .then(|| {
                self.standalone_wasm_entry
                    .expect("standalone WebAssembly splice requested against the base runtime blob; use runtime_standalone_wasm()")
            })
            .into_iter()
            .flat_map(|entries| {
                entries.into_iter().enumerate().filter_map(|(id, entry)| {
                    let id = id as u32;
                    (!standalone_wasm::OPTIONAL.contains(&id) || optional.contains(&id))
                        .then_some(entry)
                })
            });
        let ta_roots = ta_used
            .then(|| {
                self.ta_entry
                    .expect("ta splice requested against a blob without the engine")
            })
            .into_iter()
            .flatten();
        let broker_roots = broker_used
            .then(|| {
                self.broker_entry
                    .expect("broker splice requested against a blob without the emulator")
            })
            .into_iter()
            .flatten();
        let roots: Vec<u32> = used
            .iter()
            .enumerate()
            .filter(|&(_, &is_used)| is_used)
            .map(|(id, _)| self.entry[id])
            .chain(standalone_wasm_roots)
            .chain(ta_roots)
            .chain(broker_roots)
            .collect();
        reachable_functions(&roots, self.direct_calls, self.bodies.len())
    }

    /// Append the runtime's types, functions, globals, bodies, and data to
    /// the sections of an artifact under construction. Must be called before
    /// the artifact adds any of its own entries, so that the runtime occupies
    /// the low indices.
    // One parameter per wasm section it appends, plus the shift and the
    // tree-shake set; grouping them into a struct would only rename them.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn splice(
        &self,
        types: &mut TypeSection,
        funcs: &mut FunctionSection,
        globals: &mut GlobalSection,
        code: &mut CodeSection,
        data: &mut DataSection,
        import_count: u32,
        used: &[bool; pinewasm_num::id::COUNT as usize],
        standalone_wasm_used: bool,
        optional: &[u32],
        ta_used: bool,
        broker_used: bool,
    ) -> Result<(), AotError> {
        let mut shift = ImportShift { import_count };
        for ty in &self.types {
            let encoded = shift.func_type(ty.clone()).map_err(reencode_failed)?;
            types.ty().func_type(&encoded);
        }
        for &t in &self.func_types {
            funcs.function(t);
        }
        for (ty, init) in &self.globals {
            let val = match ty.content_type {
                wasmparser::ValType::I32 => ConstExpr::i32_const(*init as i32),
                wasmparser::ValType::I64 => ConstExpr::i64_const(*init),
                other => {
                    return Err(AotError::InvalidProgram(format!(
                        "AOT runtime global has unexpected type {other:?}"
                    )))
                }
            };
            globals.global(
                wasm_encoder::GlobalType {
                    val_type: shift.val_type(ty.content_type).map_err(reencode_failed)?,
                    mutable: ty.mutable,
                    shared: ty.shared,
                },
                &val,
            );
        }
        // Tree-shake. Every function keeps its index (the artifact's own
        // functions are numbered after these, and `entry` addresses them
        // positionally), but a body nothing reaches is replaced by a stub.
        // `unreachable` is stack-polymorphic, so one empty-locals body is
        // valid for every signature and no type index has to move.
        // The chart runtime calls through its table, which the direct-call
        // graph does not describe, so every body stays; and its table and
        // element sections name functions by index, so nothing may shift.
        let keep = if self.chart_entry.is_some() {
            if import_count != 0 {
                return Err(AotError::NotStandalone(
                    "the chart runtime cannot be linked beside an import".into(),
                ));
            }
            vec![true; self.bodies.len()]
        } else {
            self.reachable(used, standalone_wasm_used, optional, ta_used, broker_used)
        };
        for (i, body) in self.bodies.iter().enumerate() {
            if keep[i] {
                if import_count == 0 {
                    // Runtime types, globals, and functions occupy the low
                    // indices. Without imports there is nothing to relocate;
                    // keep the frozen body bytes rather than decode and encode
                    // every instruction again for each emitted artifact.
                    code.raw(body.as_bytes());
                } else {
                    shift
                        .parse_function_body(code, body.clone())
                        .map_err(reencode_failed)?;
                }
            } else {
                let mut stub = Function::new([]);
                stub.instruction(&Instruction::Unreachable);
                stub.instruction(&Instruction::End);
                code.function(&stub);
            }
        }
        for (addr, bytes) in &self.data {
            data.active(
                0,
                &ConstExpr::i32_const(*addr as i32),
                bytes.iter().copied(),
            );
        }
        Ok(())
    }

    /// `with_standalone_wasm` / `with_ta` / `with_broker` select which export surfaces to
    /// resolve: the base blob carries none of the families, so resolving
    /// their names against it would (correctly) fail — the base path must
    /// never ask.
    fn parse(
        bytes: &'static [u8],
        variant: Variant,
        direct_calls: &'static [&'static [u32]],
    ) -> Result<Runtime, String> {
        use wasmparser::Payload;
        let (with_standalone_wasm, with_ta, with_broker, with_chart) = (
            variant.standalone_wasm(),
            variant.ta(),
            variant.broker(),
            variant.chart(),
        );

        let mut types = Vec::new();
        let mut sub_types = Vec::new();
        let mut func_types = Vec::new();
        let mut globals = Vec::new();
        let mut bodies = Vec::new();
        let mut data = Vec::new();
        let mut exports: Vec<(&str, wasmparser::ExternalKind, u32)> = Vec::new();
        let mut table_section = None;
        let mut element_section = None;

        for payload in wasmparser::Parser::new(0).parse_all(bytes) {
            match payload.map_err(|e| e.to_string())? {
                Payload::TypeSection(reader) => {
                    for group in reader {
                        let group = group.map_err(|e| e.to_string())?;
                        // `splice` re-emits every runtime type as its own
                        // implicit group, so an explicit `rec` group would
                        // not survive into the artifact prefix the producer
                        // audit compares against `sub_types`.
                        if group.is_explicit_rec_group() {
                            return Err("AOT runtime declares an explicit rec group".into());
                        }
                        for ty in group.into_types() {
                            types.push(ty.unwrap_func().clone());
                            sub_types.push(ty);
                        }
                    }
                }
                Payload::FunctionSection(reader) => {
                    for t in reader {
                        func_types.push(t.map_err(|e| e.to_string())?);
                    }
                }
                Payload::GlobalSection(reader) => {
                    for g in reader {
                        let g = g.map_err(|e| e.to_string())?;
                        globals.push((g.ty, const_i64(&g.init_expr)?));
                    }
                }
                Payload::ExportSection(reader) => {
                    for e in reader {
                        let e = e.map_err(|e| e.to_string())?;
                        exports.push((e.name, e.kind, e.index));
                    }
                }
                Payload::CodeSectionEntry(body) => bodies.push(body),
                Payload::DataSection(reader) => {
                    for segment in reader {
                        let segment = segment.map_err(|e| e.to_string())?;
                        match segment.kind {
                            wasmparser::DataKind::Active {
                                memory_index: 0,
                                offset_expr,
                            } => data.push((const_i64(&offset_expr)? as u32, segment.data)),
                            _ => return Err("AOT runtime has a non-active data segment".into()),
                        }
                    }
                }
                // Anything below would change the artifact's own index spaces
                // or its instantiation behavior, and the splice does not
                // account for it. The runtime is a checked-in asset, so these
                // are assertions about how it was built, not input validation.
                Payload::ImportSection(_) => return Err("AOT runtime imports something".into()),
                Payload::TableSection(reader) if with_chart => {
                    table_section = Some(section_bytes(bytes, &reader.range())?);
                }
                Payload::TableSection(_) => return Err("AOT runtime declares a table".into()),
                Payload::ElementSection(reader) if with_chart => {
                    element_section = Some(section_bytes(bytes, &reader.range())?);
                }
                Payload::ElementSection(_) => {
                    return Err("AOT runtime has an element section".into())
                }
                Payload::StartSection { .. } => {
                    return Err("AOT runtime has a start function".into())
                }
                // Tags are not spliced either. A body throwing one would
                // validate against the blob's declaration and not the
                // artifact's, which is exactly the gap the producer audit's
                // verified-copy rule must never have.
                Payload::TagSection(_) => return Err("AOT runtime declares a tag".into()),
                _ => {}
            }
        }

        if func_types.len() != bodies.len() {
            return Err(format!(
                "AOT runtime declares {} functions but has {} bodies",
                func_types.len(),
                bodies.len()
            ));
        }

        // The chart runtime is spliced whole and has no call graph.
        if !with_chart && direct_calls.len() != bodies.len() {
            return Err("AOT runtime call graph does not match its function count".into());
        }

        let global_named = |name: &str| -> Result<u32, String> {
            exports
                .iter()
                .find(|(n, kind, _)| *n == name && *kind == wasmparser::ExternalKind::Global)
                .map(|&(_, _, index)| index)
                .ok_or_else(|| format!("AOT runtime does not export {name}"))
        };
        let stack_pointer = global_named("__stack_pointer")?;
        let heap_base = global_named("__heap_base")?;
        let heap_base = globals
            .get(heap_base as usize)
            .ok_or("__heap_base is out of range")?
            .1 as u32;

        let func_named = |name: &str| -> Result<u32, String> {
            exports
                .iter()
                .find(|(n, kind, _)| *n == name && *kind == wasmparser::ExternalKind::Func)
                .map(|&(_, _, index)| index)
                .ok_or_else(|| format!("AOT runtime does not export {name}"))
        };
        let mut entry = [0u32; pinewasm_num::id::COUNT as usize];
        for (id, name) in EXPORT_NAMES.iter().enumerate() {
            entry[id] = func_named(name)?;
        }
        let standalone_wasm_entry = if with_standalone_wasm {
            let mut entries = [0u32; standalone_wasm::COUNT as usize];
            for (id, name) in STANDALONE_WASM_EXPORT_NAMES.iter().enumerate() {
                entries[id] = func_named(name)?;
            }
            Some(entries)
        } else {
            None
        };
        let ta_entry = if with_ta {
            let mut entries = [0u32; ta::COUNT as usize];
            for (id, name) in TA_EXPORT_NAMES.iter().enumerate() {
                entries[id] = func_named(name)?;
            }
            Some(entries)
        } else {
            None
        };
        let broker_entry = if with_broker {
            let mut entries = [0u32; broker::COUNT as usize];
            for (id, name) in BROKER_EXPORT_NAMES.iter().enumerate() {
                entries[id] = func_named(name)?;
            }
            Some(entries)
        } else {
            None
        };

        // Rodata is the only thing at a fixed address; the shadow stack is
        // placed by `prepare`, immediately above it.
        let reserved_lo = data
            .iter()
            .map(|&(addr, _)| addr)
            .min()
            .ok_or("AOT runtime has no data segment")?;
        if reserved_lo < crate::header::SIZE + MAX_INPUTS * 8 {
            return Err(format!(
                "AOT runtime rodata at {reserved_lo} leaves room for fewer than {MAX_INPUTS} inputs"
            ));
        }
        // The whole memory map `prepare` relies on below the lane floor:
        // header, a full input table, every data segment in address order
        // without overlap, then the shadow stack, all inside the first page.
        // Like the checks above, an assertion about how the checked-in blob
        // was built (every shipped blob passes), not input validation.
        let segments = data
            .iter()
            .map(|&(addr, bytes)| {
                u32::try_from(bytes.len())
                    .map(|len| (addr, len))
                    .map_err(|_| "AOT runtime data segment exceeds wasm32".to_string())
            })
            .collect::<Result<Vec<(u32, u32)>, String>>()?;
        // The chart runtime's data does not fit the first page (the VM's
        // tables alone exceed it), so its reserved block is checked for the
        // same order without that bound, and its stack is the larger one.
        let reserved_hi = if with_chart {
            chart_reserved_hi(&segments, heap_base).ok_or_else(|| {
                format!(
                    "chart runtime data segments and shadow stack (heap base {heap_base}) are \
                     not in order above the input table"
                )
            })?
        } else {
            if !reserved_block_fits(&segments, heap_base) {
                return Err(format!(
                    "AOT runtime data segments and shadow stack (heap base {heap_base}) do not \
                     fit in order between the input table and the end of the first page"
                ));
            }
            reserved_hi_for(heap_base)
        };
        let chart_entry = if with_chart {
            let mut entries = [0u32; chart::COUNT as usize];
            for (id, name) in CHART_EXPORT_NAMES.iter().enumerate() {
                entries[id] = func_named(name)?;
            }
            Some(entries)
        } else {
            None
        };

        Ok(Runtime {
            types,
            sub_types,
            func_types,
            globals,
            bodies,
            direct_calls,
            data,
            chart_entry,
            table_section,
            element_section,
            entry,
            standalone_wasm_entry,
            ta_entry,
            broker_entry,
            stack_pointer,
            reserved_hi,
        })
    }
}

/// Re-encode the runtime's bodies with every function index moved past the
/// artifact's imports. Types and globals are spliced ahead of the artifact's
/// own, so those index spaces need no adjustment.
struct ImportShift {
    import_count: u32,
}

impl Reencode for ImportShift {
    type Error = std::convert::Infallible;

    fn function_index(&mut self, func: u32) -> Result<u32, wasm_encoder::reencode::Error> {
        Ok(shift_function_index(func, self.import_count))
    }
}

/// Function index, in an artifact importing `import_count` functions, of the
/// runtime's defined function `func`: imports occupy the low indices, so the
/// runtime's `func_count` functions occupy `import_count..import_count +
/// func_count` in order (`remap_injective_in_range`).
pub(crate) fn shift_function_index(func: u32, import_count: u32) -> u32 {
    func + import_count
}

/// Which of the `count` defined functions are reachable from `roots` along
/// the direct-call edges `direct_calls` (one callee list per function). An
/// out-of-range root or callee marks nothing. `direct_calls` has one entry
/// per function (`parse` checks it); the result is exactly the set reachable
/// from the in-range roots, so it is closed under
/// calls.
pub(crate) fn reachable_functions(
    roots: &[u32],
    direct_calls: &[&[u32]],
    count: usize,
) -> Vec<bool> {
    reachable_in(roots, direct_calls, count)
}

/// Read access to a call graph by function and callee position. The walk in
/// [`reachable_in`] goes through it rather than holding the `&[&[u32]]`
/// directly: a loop context that holds a borrow of borrows cannot be joined by
/// the extraction tooling, so the nested slice is only touched in these
/// loop-free accessors.
pub(crate) trait CallGraph {
    /// Number of direct callees of function `f`.
    fn callee_count(&self, f: usize) -> usize;
    /// The `j`-th direct callee of function `f`.
    fn callee(&self, f: usize, j: usize) -> u32;
}

impl CallGraph for [&[u32]] {
    fn callee_count(&self, f: usize) -> usize {
        self[f].len()
    }

    fn callee(&self, f: usize, j: usize) -> u32 {
        self[f][j]
    }
}

/// The worklist walk behind [`reachable_functions`], over any [`CallGraph`].
fn reachable_in<G: CallGraph + ?Sized>(roots: &[u32], graph: &G, count: usize) -> Vec<bool> {
    let mut mark = vec![false; count];
    let mut work: Vec<usize> = Vec::new();
    let mut r = 0;
    while r < roots.len() {
        mark_function(roots[r] as usize, &mut mark, &mut work);
        r += 1;
    }
    while let Some(f) = work.pop() {
        let callees = graph.callee_count(f);
        let mut j = 0;
        while j < callees {
            mark_function(graph.callee(f, j) as usize, &mut mark, &mut work);
            j += 1;
        }
    }
    mark
}

/// Mark `f` and queue it for [`reachable_functions`]'s walk, unless it is out
/// of range or already marked.
fn mark_function(f: usize, mark: &mut [bool], work: &mut Vec<usize>) {
    if f < mark.len() && !mark[f] {
        mark[f] = true;
        work.push(f);
    }
}

/// First byte above the reserved block: the shadow stack's `STACK_SIZE`
/// bytes sit immediately above the 16-aligned `__heap_base`.
pub(crate) fn reserved_hi_for(heap_base: u32) -> u32 {
    ((heap_base + 15) & !15) + STACK_SIZE
}

/// The end of the chart runtime's reserved block, or `None` when the block
/// is not laid out as `prepare` assumes: the header and a full
/// [`MAX_INPUTS`] input table from 0, then the data segments `(address,
/// length)` in increasing address order without overlap, then
/// [`CHART_STACK_SIZE`] of shadow stack from the 16-aligned `heap_base`.
/// [`reserved_block_fits`] without the first-page bound.
pub(crate) fn chart_reserved_hi(segments: &[(u32, u32)], heap_base: u32) -> Option<u32> {
    let mut cursor = crate::header::SIZE + MAX_INPUTS * 8;
    for &(addr, len) in segments {
        if addr < cursor {
            return None;
        }
        cursor = addr.checked_add(len)?;
    }
    let stack_lo = heap_base.checked_add(15)? & !15;
    if stack_lo < cursor {
        return None;
    }
    stack_lo.checked_add(CHART_STACK_SIZE)
}

/// The bytes of the section at `range` of `bytes`.
fn section_bytes<'a>(bytes: &'a [u8], range: &std::ops::Range<u64>) -> Result<&'a [u8], String> {
    let outside = || "AOT runtime section lies outside the module".to_string();
    let start = usize::try_from(range.start).map_err(|_| outside())?;
    let end = usize::try_from(range.end).map_err(|_| outside())?;
    bytes.get(start..end).ok_or_else(outside)
}

/// Bytes in one wasm page, the size of the first page every artifact has.
const PAGE_BYTES: u32 = 65536;

/// Whether the runtime's reserved block is laid out as `prepare` assumes:
/// the header and a full [`MAX_INPUTS`] input table from 0, then the
/// data segments `(address, length)` in increasing address order without
/// overlap, then the shadow stack at the 16-aligned `heap_base`, ending
/// ([`reserved_hi_for`]) inside the first page. No step can overflow.
pub(crate) fn reserved_block_fits(segments: &[(u32, u32)], heap_base: u32) -> bool {
    let mut cursor = crate::header::SIZE + MAX_INPUTS * 8;
    let mut i = 0;
    while i < segments.len() {
        let (addr, len) = segments[i];
        if addr < cursor {
            return false;
        }
        match addr.checked_add(len) {
            Some(seg_end) => cursor = seg_end,
            None => return false,
        }
        i += 1;
    }
    let stack_lo = match heap_base.checked_add(15) {
        Some(rounded) => rounded & !15,
        None => return false,
    };
    if stack_lo < cursor {
        return false;
    }
    match stack_lo.checked_add(STACK_SIZE) {
        Some(hi) => hi <= PAGE_BYTES,
        None => false,
    }
}

fn reencode_failed(e: wasm_encoder::reencode::Error) -> AotError {
    AotError::InvalidProgram(format!("AOT runtime could not be spliced: {e}"))
}

/// Read a single-instruction constant initializer (`i32.const` / `i64.const`).
fn const_i64(expr: &wasmparser::ConstExpr) -> Result<i64, String> {
    let mut ops = expr.get_operators_reader();
    let value = match ops.read().map_err(|e| e.to_string())? {
        wasmparser::Operator::I32Const { value } => i64::from(value),
        wasmparser::Operator::I64Const { value } => value,
        other => {
            return Err(format!(
                "AOT runtime has a non-constant initializer {other:?}"
            ))
        }
    };
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_encoder::Encode;

    #[test]
    fn frozen_call_graph_covers_every_call() {
        for rt in [
            runtime(),
            runtime_ta(),
            runtime_standalone_wasm(),
            runtime_broker(),
        ] {
            for (index, body) in rt.bodies.iter().enumerate() {
                let mut observed = std::collections::BTreeSet::new();
                for operator in body.get_operators_reader().unwrap() {
                    if let wasmparser::Operator::Call { function_index } = operator.unwrap() {
                        assert!((function_index as usize) < rt.bodies.len());
                        observed.insert(function_index);
                    }
                }
                assert_eq!(
                    rt.direct_calls[index],
                    observed.into_iter().collect::<Vec<_>>(),
                    "function {index}"
                );
            }
        }
    }

    /// The straightforward call-graph walk (iterator roots, `for` loops), kept
    /// to compare against [`reachable_functions`].
    fn reachable_reference(roots: &[u32], direct_calls: &[&[u32]], count: usize) -> Vec<bool> {
        let mut mark = vec![false; count];
        let mut work: Vec<usize> = Vec::new();
        for &f in roots {
            let f = f as usize;
            if f < mark.len() && !mark[f] {
                mark[f] = true;
                work.push(f);
            }
        }
        while let Some(f) = work.pop() {
            for &function_index in direct_calls[f] {
                let callee = function_index as usize;
                if callee < mark.len() && !mark[callee] {
                    mark[callee] = true;
                    work.push(callee);
                }
            }
        }
        mark
    }

    #[test]
    fn reachable_functions_matches_the_reference_walk() {
        for rt in [
            runtime(),
            runtime_ta(),
            runtime_standalone_wasm(),
            runtime_broker(),
        ] {
            let n = rt.bodies.len();
            // Every single root, every pure id, and pseudo-random root sets
            // including out-of-range indices.
            let mut cases: Vec<Vec<u32>> = (0..n as u32).map(|f| vec![f]).collect();
            cases.push(rt.entry.to_vec());
            cases.push(vec![n as u32, u32::MAX]);
            let mut seed = 0x9e37_79b9_u32;
            for _ in 0..64 {
                let mut roots = Vec::new();
                for _ in 0..(seed % 7) {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    roots.push(seed % (n as u32 + 3));
                }
                cases.push(roots);
            }
            for roots in cases {
                assert_eq!(
                    reachable_functions(&roots, rt.direct_calls, n),
                    reachable_reference(&roots, rt.direct_calls, n),
                    "roots {roots:?}"
                );
            }
        }
    }

    #[test]
    fn every_tier_reserved_block_fits_the_first_page() {
        for (bytes, rt) in [
            (MODULE_WASM, runtime()),
            (MODULE_WASM_TA, runtime_ta()),
            (MODULE_WASM_STANDALONE_WASM, runtime_standalone_wasm()),
            (MODULE_WASM_BROKER, runtime_broker()),
        ] {
            let segments: Vec<(u32, u32)> = rt
                .data
                .iter()
                .map(|&(addr, data)| (addr, data.len() as u32))
                .collect();
            let heap_base = rt.reserved_hi - STACK_SIZE;
            assert!(
                reserved_block_fits(&segments, heap_base),
                "{} bytes",
                bytes.len()
            );
            assert!(rt.reserved_hi <= PAGE_BYTES);
            assert_eq!(reserved_hi_for(heap_base), rt.reserved_hi);
        }
    }

    #[test]
    fn reserved_block_fits_rejects_bad_layouts() {
        let floor = crate::header::SIZE + MAX_INPUTS * 8;
        assert!(reserved_block_fits(&[(floor, 16)], floor + 16));
        // Rodata over the input table.
        assert!(!reserved_block_fits(&[(floor - 1, 16)], floor + 16));
        // Overlapping or out-of-order segments.
        assert!(!reserved_block_fits(
            &[(floor, 16), (floor + 8, 8)],
            floor + 32
        ));
        // Heap base below the end of rodata.
        assert!(!reserved_block_fits(&[(floor, 32)], floor + 16));
        // Stack past the first page, and arithmetic overflow.
        assert!(!reserved_block_fits(
            &[(floor, 16)],
            PAGE_BYTES - STACK_SIZE + 1
        ));
        assert!(!reserved_block_fits(&[(u32::MAX, 2)], u32::MAX));
        assert!(!reserved_block_fits(&[], u32::MAX - 3));
    }

    #[test]
    fn canonical_bodies_equal_original_zero_shift_reencoding() {
        // Covers every function, including ones a particular source would
        // tree-shake away. Future blob rebuilds must retain the byte identity
        // assumed by the fast splice, across all four runtime families.
        for (rt, original) in [
            (
                runtime(),
                Runtime::parse(MODULE_WASM, Variant::Base, runtime().direct_calls).unwrap(),
            ),
            (
                runtime_ta(),
                Runtime::parse(MODULE_WASM_TA, Variant::Ta, runtime_ta().direct_calls).unwrap(),
            ),
            (
                runtime_standalone_wasm(),
                Runtime::parse(
                    MODULE_WASM_STANDALONE_WASM,
                    Variant::StandaloneWasm,
                    runtime_standalone_wasm().direct_calls,
                )
                .unwrap(),
            ),
            (
                runtime_broker(),
                Runtime::parse(
                    MODULE_WASM_BROKER,
                    Variant::Broker,
                    runtime_broker().direct_calls,
                )
                .unwrap(),
            ),
        ] {
            assert_eq!(rt.bodies.len(), original.bodies.len());
            for (index, body) in original.bodies.iter().enumerate() {
                let mut copied = CodeSection::new();
                copied.raw(rt.bodies[index].as_bytes());
                let mut reencoded = CodeSection::new();
                ImportShift { import_count: 0 }
                    .parse_function_body(&mut reencoded, body.clone())
                    .unwrap();
                let mut copied_bytes = Vec::new();
                let mut reencoded_bytes = Vec::new();
                copied.encode(&mut copied_bytes);
                reencoded.encode(&mut reencoded_bytes);
                assert_eq!(copied_bytes, reencoded_bytes, "runtime function {index}");
            }
        }
    }
}
