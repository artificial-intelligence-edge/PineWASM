//! Module assembly: imports, memory, globals, `prepare` (layout + lane-base
//! computation), and the `run` shell (bar loop + typed per-bar preamble)
//! around the structured body emitted by `body.rs`.
//!
//! The emitted module imports at most one host function (see docs/aot.md §5):
//! `host.vm(site_id, bar)`, for programs that still route
//! heap/string/effect/request opcodes through the PineWASM interpreter.
//! Everything else is inside the module: transcendentals and the stateful
//! `ta.*` engine come from the linked runtime ([`crate::runtime`]), and fired
//! alertconditions land in an output lane. A program that does not need the
//! PineWASM VM compiles to a zero-import module.

use crate::analysis::{Analysis, Term, VTy};
use crate::body::{BodyCtx, BodyEmitter};
use crate::heap;
use crate::layout_footprint::{
    footprint_of_counts, FootprintCounts, FootprintError, LayoutFootprint, MaxBarsError,
    StandaloneRelocationBound, StrideName, BUILTIN_LANE_COUNT, CHART_MEMORY_MAX_PAGES,
    MEMORY_MAX_PAGES, STANDALONE_WASM_ARENA_PAD,
};
use crate::prepare_consts::{
    self, lane_offset, prepare_consts, prepare_layout_ops, PrepareConsts, PrepareConstsError,
    PrepareGlobals, PrepareHeapGlobals, PrepareOp, PrepareStackPointer,
};
use crate::runtime;

#[cfg(test)]
mod footprint_reference;
#[cfg(test)]
mod prepare_reference;
#[cfg(test)]
mod prepare_splice;
use crate::{header, status, AotError, Layout};
use pinewasm_csil::{CsilInstr, CsilProgram};
use pinewasm_hir::BuiltinSeries;
use std::collections::{BTreeMap, BTreeSet};
use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, CustomSection, DataCountSection, DataSection, ExportKind,
    ExportSection, Function, FunctionSection, GlobalSection, GlobalType, ImportSection,
    Instruction as I, MemArg, MemorySection, MemoryType, Module, TypeSection, ValType,
};

/// standalone WebAssembly emit parameters, resolved by [`crate::compile_standalone_wasm_program`].
pub(crate) struct StandaloneWasmSpec<'a> {
    /// Designated plot for the OUT `signals` series; `None` for an
    /// indicator-type module, whose evaluations serialize `signals: []`.
    pub signal_plot: Option<u32>,
    pub module_type: crate::StandaloneWasmModuleType,
    /// A strategy()-sourced module: the OUT series comes from the
    /// linked broker's posture lane (`standalone_wasm_serialize_posture`), never a plot.
    pub strategy_source: bool,
    /// Event contract for a designated indicator plot. Strategy sources use
    /// the broker posture lane regardless of this value.
    pub signal_semantics: crate::StandaloneWasmSignalSemantics,
    /// Native heap element ceiling baked into `prepare`; standalone WebAssembly has no loader
    /// that can overwrite this header cell after preparation.
    pub max_collection_elements: u64,
    /// The key table built by [`crate::schema::standalone_wasm_key_table`], emitted as a
    /// passive data segment the entry point `memory.init`s per call.
    pub keytab: &'a [u8],
    /// standalone WebAssembly-only lowering facts. Never present on ordinary CSAW modules.
    pub projection: &'a crate::projection::StandaloneWasmProjection,
}

/// Append the `parameter_schema` and `module_metadata` custom sections
/// (docs/aot.md §11).
///
/// Both tiers carry them: a hosted artifact's inputs are as real as a native
/// one's, and a consumer reading the search space has no reason to care which
/// tier produced the module. Custom sections are skipped by every conforming
/// decoder, and these go last so they cannot perturb the section order the
/// rest of the module depends on.
///
/// The payloads are a pure function of the program, so re-emitting reproduces
/// these bytes exactly and a distributor can hash the complete module.
fn append_metadata_sections(
    module: &mut Module,
    program: &CsilProgram,
    standalone_wasm: Option<&StandaloneWasmSpec>,
) {
    let parameter_schema = match standalone_wasm {
        Some(spec) => {
            crate::schema::parameter_schema_json_standalone_wasm(program, spec.projection)
        }
        None => crate::schema::parameter_schema_json(program),
    };
    module.section(&CustomSection {
        name: crate::schema::PARAMETER_SCHEMA_SECTION.into(),
        data: parameter_schema.as_bytes().into(),
    });
    // standalone WebAssembly mode adds `abi_version`/`signal`; gating them to standalone WebAssembly keeps existing
    // chart-interface artifacts byte-reproducible.
    let metadata = match standalone_wasm {
        Some(spec) => crate::schema::module_metadata_json_standalone_wasm(
            program,
            spec.module_type,
            spec.signal_plot,
            spec.strategy_source,
            spec.signal_semantics,
        ),
        None => crate::schema::module_metadata_json(program),
    };
    module.section(&CustomSection {
        name: crate::schema::MODULE_METADATA_SECTION.into(),
        data: metadata.as_bytes().into(),
    });
    if let Some(spec) = standalone_wasm {
        // Separate optional negotiation from the fixed consumer metadata
        // schema. Older hosts may ignore this section and keep using JSON.
        let mut capabilities = if spec.strategy_source {
            r#"{"schema_version":1,"input_protocols":["json-v1","PWSWBIN1","PWSWBIN2","PWSWREF1"],"output_projections":["retained-objectives-v1","broker-trades-v1"]"#.to_string()
        } else {
            r#"{"schema_version":1,"input_protocols":["json-v1","PWSWBIN1","PWSWBIN2","PWSWREF1"]"#
                .to_string()
        };
        if spec.strategy_source {
            capabilities.push_str(&broker_capabilities_json(program));
        }
        // Present only when erased drawing constructors bound the request
        // length (`projection::certify`); the entry point refuses more bars.
        if let Some(max_bars) = spec.projection.max_bars {
            capabilities.push_str(&format!(r#","max_bars":{max_bars}"#));
        }
        // Present only when the request must carry more than `version`,
        // `params` and `bars`: `timeframe` when the program reads the chart
        // timeframe, and the exchange's `timezone` and `session_hours` when
        // it groups bars into buckets. The entry point refuses a request
        // without them.
        let mut required = Vec::new();
        let groups_bars = !spec.projection.security_specs.is_empty();
        // A lane compares its timeframe with the chart's, too.
        if spec.projection.reads_timeframe() || groups_bars {
            required.push("timeframe");
        }
        if groups_bars {
            required.extend(["timezone", "session_hours"]);
        }
        if !required.is_empty() {
            let names: Vec<String> = required.iter().map(|name| format!("\"{name}\"")).collect();
            capabilities.push_str(&format!(r#","required_fields":[{}]"#, names.join(",")));
        }
        capabilities.push('}');
        module.section(&CustomSection {
            name: "pinewasm_capabilities".into(),
            data: capabilities.as_bytes().into(),
        });
    }
}

/// Emit the universal hosted tier. An ABI-compatible host binds this wrapper
/// to the exact validated program out of band, supplies `host.interpret`, runs
/// that program through the PineWASM VM, and returns the result out of band.
/// The exported `run` is a stub whose body is a single call to that import: the
/// module is a real, instantiable Wasm wrapper, but it compiles none of the
/// program's logic. The PineWASM VM still performs the whole run, so this is
/// a hosting adapter, not native codegen.
pub(crate) fn emit_interpreter_hosted(program: &CsilProgram) -> Vec<u8> {
    let mut types = TypeSection::new();
    types.ty().function([ValType::I32], [ValType::I32]); // 0: prepare
    types.ty().function([], [ValType::I32]); // 1: run / host.interpret

    let mut imports = ImportSection::new();
    imports.import("host", "interpret", wasm_encoder::EntityType::Function(1));

    let mut funcs = FunctionSection::new();
    funcs.function(0);
    funcs.function(1);

    let mut memory = MemorySection::new();
    memory.memory(MemoryType {
        minimum: 1,
        maximum: Some(MEMORY_MAX_PAGES),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });

    // prepare(bars) -> OK. Hosted execution validates market data and run
    // options inside the PineWASM VM, so no native lane layout is needed.
    let mut prepare = Function::new([]);
    prepare
        .instruction(&I::I32Const(status::OK))
        .instruction(&I::End);

    // run() -> host.interpret(). Import function index 0 precedes the two
    // defined functions.
    let mut run = Function::new([]);
    run.instruction(&I::Call(0)).instruction(&I::End);

    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    exports.export("prepare", ExportKind::Func, 1);
    exports.export("run", ExportKind::Func, 2);

    let mut code = CodeSection::new();
    code.function(&prepare);
    code.function(&run);

    let mut module = Module::new();
    module
        .section(&types)
        .section(&imports)
        .section(&funcs)
        .section(&memory)
        .section(&exports)
        .section(&code);
    append_metadata_sections(&mut module, program, None);
    module.finish()
}

/// Bytes the linked ta engine's region may occupy before emission refuses.
///
/// Each ring capacity is a baked const param clamped to 100,000, but nothing
/// bounds the *number* of call sites, so the product is unbounded: ~90 sites
/// each declaring a maximum-length window would already ask for more than
/// this, and enough of them would overflow the i32 the region size is emitted
/// as. Like the native layout-capacity check, this exists to be checked
/// rather than approached — a realistic program's region is kilobytes — and a program
/// over it falls back to the interpreter-hosted tier instead of getting a
/// layout whose arithmetic wrapped.
const MAX_TA_REGION_BYTES: u32 = 64 * 1024 * 1024;

/// Mirrors `RunOptions::default().max_instructions_per_bar`.
const DEFAULT_BUDGET: i64 = 64_000_000;
/// Mirrors `RunOptions::default().max_instructions_per_run`.
const DEFAULT_RUN_BUDGET: i64 = 1_000_000_000;
/// The instructions a standalone WebAssembly run may average per bar before
/// the cumulative budget stops it.
///
/// A standalone WebAssembly module has no loader to raise its run budget,
/// and a flat ceiling measures the wrong thing: a script runs once per bar,
/// and one that loops over its own history does more work per bar the longer
/// the chart is. A program within the per-bar budget on every bar could be
/// refused on a long input for the length of the input alone. The run budget is
/// therefore the larger of [`DEFAULT_RUN_BUDGET`] and this allowance times
/// the bars evaluated; the per-bar budget and the host's own fuel and time
/// limits are unchanged and still bound the run.
const STANDALONE_WASM_RUN_BUDGET_PER_BAR: i64 = 1_000_000;
/// Mirrors `MarketData::default().mintick` (the loader overwrites).
const DEFAULT_MINTICK: f64 = 0.01;

fn mem_u32(offset: u32) -> MemArg {
    MemArg {
        offset: u64::from(offset),
        align: 2,
        memory_index: 0,
    }
}
fn mem_u64(offset: u32) -> MemArg {
    MemArg {
        offset: u64::from(offset),
        align: 3,
        memory_index: 0,
    }
}
fn mem_f64_at(offset: u32) -> MemArg {
    MemArg {
        offset: u64::from(offset),
        align: 3,
        memory_index: 0,
    }
}
fn mem_f64() -> MemArg {
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

/// The base lanes a builtin read depends on (indices into the fixed
/// open/high/low/close/volume/time lane order).
fn builtin_lane_deps(source: BuiltinSeries) -> &'static [usize] {
    use BuiltinSeries as B;
    match source {
        B::Open => &[0],
        B::High => &[1],
        B::Low => &[2],
        B::Close => &[3],
        B::Volume => &[4],
        B::Hl2 => &[1, 2],
        B::Hlc3 => &[1, 2, 3],
        B::Ohlc4 => &[0, 1, 2, 3],
        B::Hlcc4 => &[1, 2, 3],
        B::Time | B::LastBarTime => &[5],
        _ => &[],
    }
}

/// Mutable-global registry: one i32 address global per lane plus `bars` and
/// the region bases. Indices are allocated during planning; `prepare`
/// computes every value once per run.
pub(crate) struct Globals {
    pub bars: u32,
    builtin: [Option<u32>; BUILTIN_LANE_COUNT as usize],
    source_lane: BTreeMap<u32, u32>,
    security_lane: Vec<u32>,
    slot: Vec<u32>,
    plot: Vec<u32>,
    plot_color: Vec<Vec<u32>>,
    visual_color: Vec<Vec<u32>>,
    fill_color: Vec<Vec<u32>>,
    pub written_base: u32,
    pub scratch_base: u32,
    /// Base of the linked ta engine's region, present only when the program
    /// has `CallTa` sites. A global rather than a header field: no loader
    /// reads it, so adding one would move every artifact's header for
    /// nobody's benefit.
    pub ta_base: Option<u32>,
    /// Heap runtime globals (base/ptr i32, budget i64s), present
    /// only for native-heap modules.
    pub heap: Option<heap::HeapGlobals>,
    /// First index this artifact owns (everything below belongs to the
    /// spliced runtime).
    base: u32,
    /// One past the last index, so `count - base` is what the artifact
    /// declares in its own global section.
    pub count: u32,
}

impl Globals {
    /// `base` is the first global index available to the artifact: the linked
    /// numeric runtime's globals are spliced in ahead of these, so its own
    /// indices survive unchanged (see [`crate::runtime`]).
    fn plan(
        program: &CsilProgram,
        layout: &Layout,
        base: u32,
        collects: bool,
        projection: Option<&crate::projection::StandaloneWasmProjection>,
    ) -> Globals {
        let mut count = base;
        let mut next = || {
            let i = count;
            count += 1;
            i
        };
        let bars = next();
        let mut builtin: [Option<u32>; 6] = [None; 6];
        for (pc, instr) in program.main().code.iter().enumerate() {
            // A `time(timeframe)` call the projection serves reads the bar's
            // own open time when the timeframe is the chart's.
            let served_time_open =
                projection.is_some_and(|projection| projection.time_open_site.contains_key(&pc));
            let source = match instr {
                CsilInstr::ReadBuiltin { source, .. }
                | CsilInstr::ReadBuiltinDynamic { source } => Some(*source),
                _ if served_time_open => Some(BuiltinSeries::Time),
                _ => None,
            };
            if let Some(source) = source {
                // A `time_close` or `timenow` the projection serves adds the
                // timeframe's length to the time lane. Elsewhere it is a host
                // site that reads no lane of the module's.
                let served_time = projection.is_some_and(|projection| {
                    matches!(
                        projection.chart_site.get(&pc),
                        Some(crate::projection::ChartSite::Builtin(
                            BuiltinSeries::TimeClose | BuiltinSeries::Timenow
                        ))
                    )
                });
                let deps = if served_time {
                    builtin_lane_deps(BuiltinSeries::Time)
                } else {
                    builtin_lane_deps(source)
                };
                for &lane in deps {
                    if builtin[lane].is_none() {
                        builtin[lane] = Some(next());
                    }
                }
            }
        }
        let source_lane: BTreeMap<u32, u32> = layout
            .source_inputs
            .iter()
            .map(|&input| (input, next()))
            .collect();
        let security_lane: Vec<u32> = (0..layout.security_lanes).map(|_| next()).collect();
        let slot: Vec<u32> = (0..layout.slot_count).map(|_| next()).collect();
        let plot: Vec<u32> = (0..layout.plot_count).map(|_| next()).collect();
        let plot_color: Vec<Vec<u32>> = layout
            .plot_color_roles
            .iter()
            .map(|&n| (0..n).map(|_| next()).collect())
            .collect();
        let visual_color: Vec<Vec<u32>> = layout
            .visual_color_roles
            .iter()
            .map(|&n| (0..n).map(|_| next()).collect())
            .collect();
        let fill_color: Vec<Vec<u32>> = layout
            .fill_color_roles
            .iter()
            .map(|&n| (0..n).map(|_| next()).collect())
            .collect();
        let written_base = next();
        let scratch_base = next();
        let ta_base = layout.links_ta.then(&mut next);
        let heap = layout.native_heap.then(|| heap::HeapGlobals {
            base: next(),
            ptr: next(),
            elems: next(),
            objs: next(),
            gc: collects.then(|| heap::GcGlobals {
                arena: next(),
                arena_due: next(),
                elems_due: next(),
                epoch: next(),
            }),
        });
        Globals {
            bars,
            builtin,
            source_lane,
            security_lane,
            slot,
            plot,
            plot_color,
            visual_color,
            fill_color,
            written_base,
            scratch_base,
            ta_base,
            heap,
            base,
            count,
        }
    }

    pub(crate) fn builtin_global(&self, lane: usize) -> u32 {
        self.builtin[lane].expect("builtin lane planned")
    }
    pub(crate) fn slot_global(&self, slot: u32) -> u32 {
        self.slot[slot as usize]
    }
    pub(crate) fn plot_global(&self, plot: u32) -> u32 {
        self.plot[plot as usize]
    }
    pub(crate) fn plot_color_global(&self, plot: u32, role: u32) -> u32 {
        self.plot_color[plot as usize][role as usize]
    }
    pub(crate) fn visual_color_global(&self, visual: u32, role: u32) -> u32 {
        self.visual_color[visual as usize][role as usize]
    }
    pub(crate) fn fill_color_global(&self, fill: u32, role: u32) -> u32 {
        self.fill_color[fill as usize][role as usize]
    }
    /// Every packed-color lane global in the flat order the region is laid
    /// out in: every plot's roles, then every visual's, then every fill's.
    /// `prepare`'s base assignment and the per-bar na-init both walk this,
    /// and the loader reconstructs the same order from the program's
    /// declarations, so the three cannot drift apart.
    fn color_lanes(&self) -> impl Iterator<Item = u32> + '_ {
        self.plot_color
            .iter()
            .chain(self.visual_color.iter())
            .chain(self.fill_color.iter())
            .flatten()
            .copied()
    }
    pub(crate) fn source_lane_global(&self, input: u32) -> Option<u32> {
        self.source_lane.get(&input).copied()
    }
    pub(crate) fn security_lane_global(&self, index: u32) -> Option<u32> {
        self.security_lane.get(index as usize).copied()
    }
    /// The `request.security` lane globals, in lane order.
    pub(crate) fn security_lanes(&self) -> &[u32] {
        &self.security_lane
    }
}

/// Packed-color lane count for a layout, in the [`Globals::color_lanes`]
/// order, or `None` if it overflows `u64`. Sizes the color region `prepare`
/// lays out (through [`LayoutFootprint::color_bytes_per_bar`]).
pub(crate) fn total_color_lanes(layout: &Layout) -> Option<u64> {
    prepare_consts::total_color_lanes(
        &layout.plot_color_roles,
        &layout.visual_color_roles,
        &layout.fill_color_roles,
    )
}

/// Function indices the body emitter calls: the surviving host imports, the
/// linked numeric runtime, and the heap helpers.
pub(crate) struct HostFns {
    /// `pinewasm_rt_ta_advance` in the spliced engine — the function `CallTa` calls.
    pub ta: Option<u32>,
    /// `pinewasm_rt_ta_init` in the spliced engine, called once per `run`.
    pub ta_init: Option<u32>,
    /// `host.vm(site_id, bar) -> status` — the VM host.
    pub vm: Option<u32>,
    /// The spliced numeric runtime, with its function indices already shifted
    /// past this module's imports. Indexed by [`pinewasm_num::id`].
    pub pure: Option<[u32; pinewasm_num::id::COUNT as usize]>,
    /// Native-heap helper functions (defined after `run`).
    pub heap: Option<heap::HeapFns>,
    /// The spliced broker family for strategy standalone WebAssembly modules, indexed by
    /// [`runtime::broker`] id.
    pub broker: Option<[u32; runtime::broker::COUNT as usize]>,
    /// `standalone_wasm_calendar(part, time, offset) -> part`, in a standalone
    /// module whose projection serves a calendar function.
    pub calendar: Option<u32>,
    /// `standalone_wasm_timestamp(year, month, day, hour, minute, second,
    /// offset, zoned) -> time`, in a standalone module whose projection
    /// serves a `timestamp` call.
    pub timestamp: Option<u32>,
    /// The shared stop functions ([`StopFns`]) every native module's
    /// per-bar code stops through.
    pub stops: Option<StopFns>,
    /// What a chart module's linked host adds to [`Self::vm`]
    /// (`compile_chart_wasm_program`); `None` for every other module.
    pub linked: Option<LinkedFns>,
}

/// The entries of a linked chart host that its module calls besides
/// [`HostFns::vm`] (docs/aot.md §14).
#[derive(Clone, Copy)]
pub(crate) struct LinkedFns {
    /// `pinewasm_rt_chart_stop(code, pc, bar) -> status`: what a stop the
    /// module's own code made becomes. The module's stop function calls it,
    /// so a stop that is a final error carries the interpreter's own error.
    pub stop: u32,
    /// The string budget, in a module that runs an instruction itself at
    /// which the interpreter reserves string bytes.
    pub strings: Option<StringFns>,
}

/// How a linked chart module keeps the interpreter's string budget
/// (`RunOptions::max_string_bytes_per_run`). The interpreter reserves string
/// bytes where it pushes a string constant, where an `alertcondition` fires
/// and where it copies an object's string fields, and a linked module runs
/// those instructions itself. So the module keeps what is left of the
/// budget, takes the same bytes from it at the same instructions, and passes
/// it through every hosted call, which takes the bytes its own instruction
/// reserves. When a reservation does not fit, the linked host makes it, and
/// the run stops with the interpreter's error.
#[derive(Clone, Copy)]
pub(crate) struct StringFns {
    /// The private `i64` global holding what is left of the budget.
    pub room: u32,
    /// `pinewasm_rt_chart_string_room() -> left`: what a run starts with.
    pub seed: u32,
    /// `pinewasm_rt_chart_vm_strings(site, bar, left) -> left`:
    /// [`HostFns::vm`] with the budget going in and coming back, or the
    /// call's nonzero status negated.
    pub vm: u32,
    /// `pinewasm_rt_chart_string_reserve(pc, bar, left, bytes) -> status`:
    /// the nonzero status to stop with after a reservation did not fit.
    pub reserve: u32,
    /// `pinewasm_rt_chart_string_len(handle) -> bytes`: the length of a run
    /// string, 0 for `na` and -1 for a cell that names none.
    pub len: u32,
}

/// The two internal functions a native module's per-bar code, chart or
/// standalone WebAssembly, stops through. Every site that stops the run calls
/// one and returns its result instead of writing the error triple inline. The
/// triple is three stores, and a large script has tens of thousands of such
/// sites (two per charged instruction), which made its per-bar function too
/// large for an engine's optimizing tier to compile in reasonable time.
#[derive(Clone, Copy)]
pub(crate) struct StopFns {
    /// `stop(code, pc, bar) -> code`: record `{code, bar, pc}` in the header.
    pub stop: u32,
    /// `budget_stop(pc, budget, cost, bar) -> status`: the instruction at
    /// `pc` could not be charged `cost`. Stops with the per-bar status when
    /// the per-bar counter `budget` is short (the original path checks it
    /// first), else with the run status.
    pub budget_stop: u32,
    /// `charge(pc, cost, bar) -> stopped`: the per-instruction charge of a
    /// window's fallback arm, on the counters while they live in
    /// [`Self::budget_global`] and [`Self::run_budget_global`]. Charges
    /// `cost` to both, or stops through `budget_stop` and returns 1.
    pub charge: u32,
    /// The per-bar counter during a window's fallback arm.
    pub budget_global: u32,
    /// The run counter during a window's fallback arm.
    pub run_budget_global: u32,
}

impl HostFns {
    /// Function index implementing a [`pinewasm_num`] pure id.
    pub(crate) fn pure_fn(&self, id: u32) -> Result<u32, AotError> {
        self.pure
            .map(|table| table[id as usize])
            .ok_or_else(|| AotError::InvalidProgram("numeric runtime not linked".into()))
    }

    /// Function index implementing a [`runtime::broker`] family id.
    pub(crate) fn broker_fn(&self, id: u32) -> Result<u32, AotError> {
        self.broker
            .map(|table| table[id as usize])
            .ok_or_else(|| AotError::InvalidProgram("broker runtime not linked".into()))
    }
}

// Liftoff snapshots the typed locals at structured control-flow boundaries.
// In a large inlined script the product of slot count and branch count can
// require substantial compiler memory, even though the guest's linear memory
// remains bounded. Keep small artifacts byte-identical: a chart module or
// standalone artifact with at most this many slots keeps every current-bar
// cell in a typed local.
const MAX_LOCAL_CURRENT_CELLS: u32 = 256;

// A larger artifact, chart module or standalone, keeps this many current-bar
// cells in typed locals, the ones its loops use most ([`CurCells::plan`]),
// and the rest in private, instance-owned typed globals. Bounding the local
// count limits the compiler's bookkeeping at structured branch boundaries
// while preserving local access for frequently used cells.
const MAX_HOT_CURRENT_CELLS: usize = 64;

/// Where one slot's current-bar cell lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CurCell {
    /// The `n`th current-cell local.
    Local(u32),
    /// The `n`th current-cell global.
    Global(u32),
}

/// The placement of every slot's current-bar cell: all in typed locals for
/// an artifact with at most [`MAX_LOCAL_CURRENT_CELLS`] slots; otherwise the
/// [`MAX_HOT_CURRENT_CELLS`] most used cells in locals and the rest in
/// globals. Chart modules and standalone artifacts share the rule.
pub(crate) struct CurCells {
    cells: Vec<CurCell>,
    /// Types of the current-cell locals, in local order.
    local_tys: Vec<VTy>,
    /// Types of the current-cell globals, in global order.
    global_tys: Vec<VTy>,
}

impl CurCells {
    fn plan(program: &CsilProgram, analysis: &Analysis, layout: &Layout) -> Self {
        let slot_count = layout.slot_count as usize;
        let ty = |slot: usize| analysis.slot_ty.get(slot).copied().unwrap_or(VTy::F64);
        if layout.slot_count <= MAX_LOCAL_CURRENT_CELLS {
            return CurCells {
                cells: (0..layout.slot_count).map(CurCell::Local).collect(),
                local_tys: (0..slot_count).map(ty).collect(),
                global_tys: Vec::new(),
            };
        }
        let weight = current_cell_weights(program, analysis, slot_count);
        let mut order: Vec<usize> = (0..slot_count).collect();
        // Heaviest first; the slot index breaks ties, so the plan is a
        // function of the program alone.
        order.sort_by(|&a, &b| weight[b].cmp(&weight[a]).then(a.cmp(&b)));
        let mut hot = vec![false; slot_count];
        for &slot in order.iter().take(MAX_HOT_CURRENT_CELLS) {
            // A cell no code reads or writes in the bar is not worth a local.
            hot[slot] = weight[slot] > 0;
        }
        let mut cells = Vec::with_capacity(slot_count);
        let (mut local_tys, mut global_tys) = (Vec::new(), Vec::new());
        for (slot, &is_hot) in hot.iter().enumerate() {
            if is_hot {
                cells.push(CurCell::Local(local_tys.len() as u32));
                local_tys.push(ty(slot));
            } else {
                cells.push(CurCell::Global(global_tys.len() as u32));
                global_tys.push(ty(slot));
            }
        }
        CurCells {
            cells,
            local_tys,
            global_tys,
        }
    }

    /// A split body's placement ([`crate::body::split`]): every slot's cell
    /// in its own global, in slot order, which each piece caches in its own
    /// locals.
    fn all_globals(analysis: &Analysis, layout: &Layout) -> Self {
        let ty = |slot: u32| {
            analysis
                .slot_ty
                .get(slot as usize)
                .copied()
                .unwrap_or(VTy::F64)
        };
        CurCells {
            cells: (0..layout.slot_count).map(CurCell::Global).collect(),
            local_tys: Vec::new(),
            global_tys: (0..layout.slot_count).map(ty).collect(),
        }
    }

    /// The number of current-cell globals.
    fn global_count(&self) -> u32 {
        self.global_tys.len() as u32
    }
}

/// How much the bar's code uses each slot's current cell: every current-bar
/// read and store counts `16^depth`, where `depth` is the number of loops
/// around it (counted up to four), so a cell a loop touches outweighs the
/// per-bar uses of almost any other.
fn current_cell_weights(program: &CsilProgram, analysis: &Analysis, slot_count: usize) -> Vec<u64> {
    let depth = block_loop_depths(analysis);
    let code = &program.main().code;
    let mut weight = vec![0u64; slot_count];
    for (b, block) in analysis.blocks.iter().enumerate() {
        let w = 16u64.pow(depth[b].min(4));
        for instr in &code[block.start..block.end] {
            let slot = match instr {
                CsilInstr::ReadUserSlot { slot, offset: 0 }
                | CsilInstr::StoreUserSlotCurrent { slot } => *slot as usize,
                _ => continue,
            };
            if let Some(cell) = weight.get_mut(slot) {
                *cell = cell.saturating_add(w);
            }
        }
    }
    weight
}

/// The number of natural loops around each block. A back edge is an edge to
/// a block that dominates its source; its loop is the header and every block
/// that reaches the source without passing the header.
pub(crate) fn block_loop_depths(analysis: &Analysis) -> Vec<u32> {
    let n = analysis.blocks.len();
    let succ = |b: usize| -> [Option<usize>; 2] {
        match analysis.term[b] {
            Term::Fall(t) | Term::Jump(t) => [Some(t), None],
            Term::Branch { taken, fall } => [Some(taken), Some(fall)],
            Term::Return => [None, None],
        }
    };
    let reachable = |b: usize| analysis.rpo.get(b).is_some_and(|&r| r != usize::MAX);
    let dominates = |h: usize, mut b: usize| -> bool {
        loop {
            if b == h {
                return true;
            }
            match analysis.idom.get(b) {
                Some(&up) if up != b && up != usize::MAX => b = up,
                _ => return false,
            }
        }
    };
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    for b in (0..n).filter(|&b| reachable(b)) {
        for t in succ(b).into_iter().flatten().filter(|&t| t < n) {
            preds[t].push(b);
        }
    }
    let mut depth = vec![0u32; n];
    for b in (0..n).filter(|&b| reachable(b)) {
        for h in succ(b).into_iter().flatten() {
            if h >= n || !dominates(h, b) {
                continue;
            }
            let mut in_loop = vec![false; n];
            in_loop[h] = true;
            let mut work = vec![b];
            while let Some(x) = work.pop() {
                if !in_loop[x] {
                    in_loop[x] = true;
                    work.extend(preds[x].iter().copied());
                }
            }
            for (d, inside) in depth.iter_mut().zip(&in_loop) {
                *d += u32::from(*inside);
            }
        }
    }
    depth
}

/// Local indices inside `run` (no params). The virtual CSIL stack lives in
/// `stack_base + depth*4 + class` (0 = f64, 1 = i64, 2 = i32,
/// 3 = independent integer validity); each
/// user slot caches its current-bar cell after that, in a typed local or a
/// private typed global ([`CurCells`]).
pub(crate) struct Locals {
    pub bar: u32,
    pub budget: u32,
    pub run_budget: u32,
    pub tf0: u32,
    pub tf1: u32,
    pub tf2: u32,
    pub ti0: u32,
    pub ti1: u32,
    pub ti2: u32,
    pub ti3: u32,
    pub t32a: u32,
    pub t32b: u32,
    pub t32c: u32,
    pub wm_i: u32,
    pub wm_sum: u32,
    pub stack_base: u32,
    /// Stack depths with declared locals: `stack_base..cur_base` holds
    /// four per depth.
    pub stack_depths: u32,
    cur_base: u32,
    cur_global_base: u32,
    cur: Vec<CurCell>,
}

impl Locals {
    fn new(max_stack: u32, cur: &CurCells, cur_global_base: u32) -> Result<Locals, AotError> {
        let cur_base = max_stack
            .checked_mul(4)
            .and_then(|n| n.checked_add(15))
            .ok_or_else(|| {
                AotError::UnsupportedOpcode("native stack local indices overflow".into())
            })?;
        Ok(Locals {
            bar: 0,
            budget: 1,
            run_budget: 2,
            tf0: 3,
            tf1: 4,
            tf2: 5,
            ti0: 6,
            ti1: 7,
            ti2: 8,
            ti3: 9,
            t32a: 10,
            t32b: 11,
            t32c: 12,
            wm_i: 13,
            wm_sum: 14,
            stack_base: 15,
            stack_depths: max_stack,
            cur_base,
            cur_global_base,
            cur: cur.cells.clone(),
        })
    }
    /// A split body's piece ([`crate::body::split`]): `max_stack` stack
    /// depths, and the cells of `hot` in typed locals after them, in that
    /// order; every other cell in its global.
    fn for_piece(
        max_stack: u32,
        hot: &[u32],
        slot_count: u32,
        cur_global_base: u32,
    ) -> Result<Locals, AotError> {
        let mut locals = Locals::new(
            max_stack,
            &CurCells {
                cells: Vec::new(),
                local_tys: Vec::new(),
                global_tys: Vec::new(),
            },
            cur_global_base,
        )?;
        locals.cur = (0..slot_count).map(CurCell::Global).collect();
        for (n, &slot) in hot.iter().enumerate() {
            locals.cur[slot as usize] = CurCell::Local(n as u32);
        }
        Ok(locals)
    }

    /// The first local after the current-cell locals of a piece with `hot`
    /// cached cells.
    fn after_cur(&self, hot: usize) -> u32 {
        self.cur_base + hot as u32
    }

    /// The global that holds `slot`'s current cell between the functions of
    /// a split body, where every slot has one.
    pub(crate) fn home(&self, slot: u32) -> u32 {
        self.cur_global_base + slot
    }

    pub(crate) fn cur_get(&self, slot: u32) -> I<'static> {
        match self.cur[slot as usize] {
            CurCell::Global(n) => I::GlobalGet(self.cur_global_base + n),
            CurCell::Local(n) => I::LocalGet(self.cur_base + n),
        }
    }
    pub(crate) fn cur_set(&self, slot: u32) -> I<'static> {
        match self.cur[slot as usize] {
            CurCell::Global(n) => I::GlobalSet(self.cur_global_base + n),
            CurCell::Local(n) => I::LocalSet(self.cur_base + n),
        }
    }
}

pub(crate) fn emit(
    program: &CsilProgram,
    layout: &Layout,
    analysis: &Analysis,
    carry: Option<&crate::fraction::Carried>,
) -> Result<Vec<u8>, AotError> {
    emit_module(program, layout, analysis, None, carry, None)
}

/// Chart emit mode with the host linked in (`compile_chart_wasm_program`):
/// the chart runtime is spliced, every host site calls its `vm` entry, and
/// `host_plan` (the program and site plan that runtime reads) is stored as
/// an active data segment. See [`emit_module`]'s `chart` parameter.
pub(crate) fn emit_chart(
    program: &CsilProgram,
    layout: &Layout,
    analysis: &Analysis,
    host_plan: &[u8],
    carry: Option<&crate::fraction::Carried>,
) -> Result<Vec<u8>, AotError> {
    emit_module(program, layout, analysis, None, carry, Some(host_plan))
}

/// standalone WebAssembly emit mode: same module assembly with the runtime's standalone WebAssembly
/// family kept live, the key table emitted, `evaluate_indicator_cabi`
/// appended, and the export surface replaced by the spec's five names.
pub(crate) fn emit_standalone_wasm(
    program: &CsilProgram,
    layout: &Layout,
    analysis: &Analysis,
    spec: &StandaloneWasmSpec,
) -> Result<Vec<u8>, AotError> {
    emit_module(program, layout, analysis, Some(spec), None, None)
}

/// The frozen runtime a module with this layout links, if any. Which blob
/// depends on what the program needs: standalone WebAssembly takes the widest
/// (numerics + the `standalone_wasm_*` family + the ta engine, plus the broker
/// for a strategy), a `ta.*` program the middle one, and everything else the
/// base blob, whose bytes are pinned to keep existing exports reproducible
/// (see `runtime::MODULE_WASM`). The audit that runs after emission asks the
/// same question, so it compares an artifact against the blob that was actually spliced.
pub(crate) fn linked_runtime(
    layout: &Layout,
    standalone_wasm: bool,
) -> Option<&'static runtime::Runtime> {
    layout
        .links_runtime
        .then(|| runtime::runtime_for(standalone_wasm, layout.links_ta, layout.links_broker))
}

fn emit_module(
    program: &CsilProgram,
    layout: &Layout,
    analysis: &Analysis,
    standalone_wasm: Option<&StandaloneWasmSpec>,
    carry: Option<&crate::fraction::Carried>,
    chart: Option<&[u8]>,
) -> Result<Vec<u8>, AotError> {
    let style_slots = style_slot_ids(program);
    let lanes = if standalone_wasm.is_some() {
        SlotLanes::Standalone
    } else if chart.is_some() {
        SlotLanes::Linked {
            styles: &style_slots,
        }
    } else {
        SlotLanes::All
    };
    let slots = SlotLayout::plan(program, layout, lanes)?;
    // Section order matters twice over. The wasm binary format fixes it, and
    // so does the index discipline: the linked numeric runtime is spliced in
    // before this module contributes anything, so its type, global, and
    // function-body indices survive unchanged and only its `call` immediates
    // need shifting past the imports (see `runtime::Runtime::splice`).
    // A chart module's host is the chart runtime, linked instead of
    // imported; every other module takes the smallest runtime it needs.
    let rt = if chart.is_some() {
        Some(runtime::runtime_chart().ok_or(AotError::ChartRuntimeUnavailable)?)
    } else {
        linked_runtime(layout, standalone_wasm.is_some())
    };
    let import_count = u32::from(layout.imports_vm && chart.is_none());
    if rt.is_some() && program.inputs.len() as u32 > runtime::MAX_INPUTS {
        return Err(AotError::UnsupportedOpcode(format!(
            "{} inputs exceeds the {} an artifact with a linked numeric runtime can address",
            program.inputs.len(),
            runtime::MAX_INPUTS
        )));
    }
    if ta_region_bytes(layout) > MAX_TA_REGION_BYTES {
        return Err(AotError::UnsupportedOpcode(format!(
            "the ta engine's state would need {} bytes, over the {MAX_TA_REGION_BYTES} an \
             artifact reserves",
            ta_region_bytes(layout)
        )));
    }
    if layout.links_ta && layout.ta_decls.len() != program.ta_states.len() {
        return Err(AotError::UnsupportedOpcode(format!(
            "a ta declaration carries more than the {} const params an artifact's table holds",
            pinewasm_ta::MAX_PARAMS
        )));
    }

    let type_base = rt.map_or(0, runtime::Runtime::type_count);
    let global_base = rt.map_or(0, runtime::Runtime::global_count);
    let gc_plan = analysis
        .heap
        .as_ref()
        .and_then(|plan| plan.gc.as_ref())
        .filter(|gc| collector_fits(gc, layout, &slots, standalone_wasm.is_some()));
    // A linked module's lane plan keeps what `SlotLanes::Linked` lists, and a
    // collector root is always among it (a `var` slot or one read back). If
    // the two ever disagree, refuse the module: `collector_fits` would
    // otherwise drop the collector without a word.
    if chart.is_some() {
        let unkept = analysis
            .heap
            .as_ref()
            .and_then(|plan| plan.gc.as_ref())
            .and_then(|gc| {
                gc.roots
                    .iter()
                    .find(|root| !slots.retains_history(root.slot))
            });
        if let Some(root) = unkept {
            return Err(AotError::InvalidProgram(format!(
                "collector root slot {} keeps no lane in a linked chart module",
                root.slot
            )));
        }
    }
    let globals = Globals::plan(
        program,
        layout,
        global_base,
        gc_plan.is_some(),
        standalone_wasm.map(|spec| spec.projection),
    );
    // A large per-bar body runs as several functions (`body/split.rs`),
    // including standalone modules. The broker phases stay in `bar_step`,
    // around the complete piece tree, so a function boundary cannot become
    // a new order-processing or equity-sampling boundary.
    let split = crate::body::split::SplitPlan::plan(program, analysis);
    let cur_cells = match &split {
        Some(_) => CurCells::all_globals(analysis, layout),
        None => CurCells::plan(program, analysis, layout),
    };

    let mut types = TypeSection::new();
    let mut funcs = FunctionSection::new();
    let mut globals_sec = GlobalSection::new();
    let mut code = CodeSection::new();
    let mut data = DataSection::new();
    // The members of the standalone family only some modules call: the
    // chart timeframe's fill, the calendar function a standalone
    // projection lowers fixed-timezone calendar calls to, and the
    // `timestamp` function.
    let calls_calendar = standalone_wasm.is_some_and(|spec| spec.projection.calls_calendar());
    let calls_timestamp = standalone_wasm.is_some_and(|spec| spec.projection.calls_timestamp());
    let optional: Vec<u32> = [
        (
            standalone_wasm.is_some_and(|spec| spec.projection.reads_timeframe()),
            runtime::standalone_wasm::FILL_TIMEFRAME,
        ),
        (calls_calendar, runtime::standalone_wasm::CALENDAR),
        (calls_timestamp, runtime::standalone_wasm::TIMESTAMP),
    ]
    .into_iter()
    .filter_map(|(used, id)| used.then_some(id))
    .collect();
    if let Some(rt) = rt {
        rt.splice(
            &mut types,
            &mut funcs,
            &mut globals_sec,
            &mut code,
            &mut data,
            import_count,
            &layout.used_pure_ids,
            standalone_wasm.is_some(),
            &optional,
            layout.links_ta,
            layout.links_broker,
        )?;
    }

    let prepare_ty = type_base;
    let run_ty = type_base + 1;
    types.ty().function([ValType::I32], [ValType::I32]); // prepare
    types.ty().function([], [ValType::I32]); // run
    let mut imports = ImportSection::new();
    let mut next_func = 0u32;
    let mut next_type = type_base + 2;
    let vm = if let (Some(_), Some(rt)) = (chart, rt) {
        // The host is a function of this module, not an import.
        (!layout.host_sites.is_empty()).then(|| rt.chart_entry(runtime::chart::VM))
    } else if layout.imports_vm {
        types
            .ty()
            .function([ValType::I32, ValType::I32], [ValType::I32]);
        imports.import("host", "vm", wasm_encoder::EntityType::Function(next_type));
        next_type += 1;
        let i = next_func;
        next_func += 1;
        Some(i)
    } else {
        None
    };
    debug_assert_eq!(next_func, import_count);
    // Imports occupy the low function indices, then the runtime's functions,
    // then this module's.
    next_func += rt.map_or(0, runtime::Runtime::func_count);
    let pure = rt.map(|rt| std::array::from_fn(|id| rt.entry(id as u32, import_count)));
    // `CallTa` now targets the spliced engine rather than an import; the call
    // site is otherwise unchanged, because `pinewasm_rt_ta_advance` deliberately keeps
    // the signature `host.ta` had.
    let ta = layout.links_ta.then(|| {
        rt.expect("a ta program links the runtime")
            .ta_entry(runtime::ta::ADVANCE, import_count)
    });
    let ta_init = layout.links_ta.then(|| {
        rt.expect("a ta program links the runtime")
            .ta_entry(runtime::ta::INIT, import_count)
    });
    // The spliced broker family (strategy standalone WebAssembly modules only), indexed by
    // [`runtime::broker`] id.
    let broker = layout.links_broker.then(|| {
        let rt = rt.expect("a strategy program links the runtime");
        std::array::from_fn(|id| rt.broker_entry(id as u32, import_count))
    });
    let calendar = calls_calendar.then(|| {
        rt.expect("standalone WebAssembly mode forces links_runtime")
            .standalone_wasm_entry(runtime::standalone_wasm::CALENDAR, import_count)
    });
    let timestamp = calls_timestamp.then(|| {
        rt.expect("standalone WebAssembly mode forces links_runtime")
            .standalone_wasm_entry(runtime::standalone_wasm::TIMESTAMP, import_count)
    });
    let prepare_idx = next_func;
    let run_idx = next_func + 1;

    funcs.function(prepare_ty);
    funcs.function(run_ty);

    // Heap helpers: types + function entries after `run`, bodies
    // appended after `run`'s in the same order.
    let mut next_idx = run_idx + 1;
    // `array.stdev`/`array.variance` helpers only for a plan that uses them.
    let uses_var = analysis.heap.as_ref().is_some_and(|plan| {
        plan.coll.values().any(|(op, _)| {
            matches!(
                op,
                crate::analysis::NativeCollOp::Stdev | crate::analysis::NativeCollOp::Variance
            )
        })
    });
    let heap_fns = if layout.native_heap {
        let helper = |params: &[ValType],
                      results: &[ValType],
                      types: &mut TypeSection,
                      funcs: &mut FunctionSection,
                      next_type: &mut u32,
                      next_idx: &mut u32|
         -> u32 {
            types.ty().function(params.to_vec(), results.to_vec());
            funcs.function(*next_type);
            *next_type += 1;
            let i = *next_idx;
            *next_idx += 1;
            i
        };
        // Function indices: imports, then prepare (= run_idx − 1), run, and
        // the helpers immediately after run.
        let fns = heap::HeapFns {
            alloc: helper(
                &[ValType::I32],
                &[ValType::I32],
                &mut types,
                &mut funcs,
                &mut next_type,
                &mut next_idx,
            ),
            new_f64: helper(
                &[ValType::I32, ValType::F64],
                &[ValType::I32],
                &mut types,
                &mut funcs,
                &mut next_type,
                &mut next_idx,
            ),
            new_i64: helper(
                &[ValType::I32, ValType::I64],
                &[ValType::I32],
                &mut types,
                &mut funcs,
                &mut next_type,
                &mut next_idx,
            ),
            new_i32: helper(
                &[ValType::I32, ValType::I32],
                &[ValType::I32],
                &mut types,
                &mut funcs,
                &mut next_type,
                &mut next_idx,
            ),
            push_f64: helper(
                &[ValType::I32, ValType::F64],
                &[ValType::I32],
                &mut types,
                &mut funcs,
                &mut next_type,
                &mut next_idx,
            ),
            push_i64: helper(
                &[ValType::I32, ValType::I64],
                &[ValType::I32],
                &mut types,
                &mut funcs,
                &mut next_type,
                &mut next_idx,
            ),
            push_i32: helper(
                &[ValType::I32, ValType::I32],
                &[ValType::I32],
                &mut types,
                &mut funcs,
                &mut next_type,
                &mut next_idx,
            ),
            shift_f64: helper(
                &[ValType::I32],
                &[ValType::F64],
                &mut types,
                &mut funcs,
                &mut next_type,
                &mut next_idx,
            ),
            shift_i64: helper(
                &[ValType::I32],
                &[ValType::I64],
                &mut types,
                &mut funcs,
                &mut next_type,
                &mut next_idx,
            ),
            shift_i32: helper(
                &[ValType::I32],
                &[ValType::I32],
                &mut types,
                &mut funcs,
                &mut next_type,
                &mut next_idx,
            ),
            pop_f64: helper(
                &[ValType::I32],
                &[ValType::F64],
                &mut types,
                &mut funcs,
                &mut next_type,
                &mut next_idx,
            ),
            pop_i64: helper(
                &[ValType::I32],
                &[ValType::I64],
                &mut types,
                &mut funcs,
                &mut next_type,
                &mut next_idx,
            ),
            pop_i32: helper(
                &[ValType::I32],
                &[ValType::I32],
                &mut types,
                &mut funcs,
                &mut next_type,
                &mut next_idx,
            ),
            agg_f64: helper(
                &[ValType::I32, ValType::I32],
                &[ValType::F64],
                &mut types,
                &mut funcs,
                &mut next_type,
                &mut next_idx,
            ),
            agg_i64: helper(
                &[ValType::I32, ValType::I32],
                &[ValType::F64],
                &mut types,
                &mut funcs,
                &mut next_type,
                &mut next_idx,
            ),
            var: uses_var.then(|| {
                let lane = |types: &mut TypeSection,
                            funcs: &mut FunctionSection,
                            next_type: &mut u32,
                            next_idx: &mut u32| {
                    helper(
                        &[ValType::I32, ValType::I32, ValType::I32],
                        &[ValType::F64],
                        types,
                        funcs,
                        next_type,
                        next_idx,
                    )
                };
                let f64_lane = lane(&mut types, &mut funcs, &mut next_type, &mut next_idx);
                let i64_lane = lane(&mut types, &mut funcs, &mut next_type, &mut next_idx);
                (f64_lane, i64_lane)
            }),
            collect: gc_plan.is_some().then(|| {
                helper(
                    &[ValType::I32],
                    &[ValType::I32],
                    &mut types,
                    &mut funcs,
                    &mut next_type,
                    &mut next_idx,
                )
            }),
        };
        Some(fns)
    } else {
        None
    };
    // A split body's pieces come after the heap helpers, so the stops still
    // sit just before `bar_step`, which stays last. They share one type,
    // `bar_step`'s signature.
    let piece_fns: Vec<u32> = match &split {
        Some(plan) => {
            types.ty().function([ValType::I32], [ValType::I32]);
            let ty = next_type;
            next_type += 1;
            plan.pieces
                .iter()
                .map(|_| {
                    funcs.function(ty);
                    let i = next_idx;
                    next_idx += 1;
                    i
                })
                .collect()
        }
        None => Vec::new(),
    };
    // The shared stop functions, after the heap helpers, then standalone
    // WebAssembly's `bar_step` and entry point. Chart modules stop through
    // them too: shared helpers reduce repeated error branches in large bodies.
    // Each structured branch is a point where V8's baseline compiler copies
    // the state of every local.
    // The fallback counters are the two globals after `bar_step`'s run-budget
    // global, which follows the current-cell globals.
    let fallback_globals = globals.count + cur_cells.global_count() + 1;
    let stops = Some({
        types
            .ty()
            .function([ValType::I32, ValType::I32, ValType::I32], [ValType::I32]);
        types.ty().function(
            [ValType::I32, ValType::I64, ValType::I64, ValType::I32],
            [ValType::I32],
        );
        types
            .ty()
            .function([ValType::I32, ValType::I64, ValType::I32], [ValType::I32]);
        funcs.function(next_type);
        funcs.function(next_type + 1);
        funcs.function(next_type + 2);
        next_type += 3;
        let fns = StopFns {
            stop: next_idx,
            budget_stop: next_idx + 1,
            charge: next_idx + 2,
            budget_global: fallback_globals,
            run_budget_global: fallback_globals + 1,
        };
        next_idx += 3;
        fns
    });
    // The pieces declared above get their code only when these stop
    // functions exist too: the global that hands the per-bar counter from
    // piece to piece follows the stops' two fallback counters (`bb_global`).
    // A split body without stops would leave its pieces declared without
    // bodies.
    if split.is_some() && stops.is_none() {
        return Err(AotError::InvalidProgram(
            "a split body's pieces need the stop functions".into(),
        ));
    }
    let standalone_step_idx = standalone_wasm.map(|_| {
        types.ty().function([ValType::I32], [ValType::I32]);
        funcs.function(next_type);
        next_type += 1;
        let i = next_idx;
        next_idx += 1;
        i
    });
    // standalone WebAssembly entry point: the last defined function, one new type after the
    // helper types; its body is appended after the helper bodies below.
    let evaluate_idx = standalone_wasm.map(|_| {
        types
            .ty()
            .function([ValType::I32, ValType::I32], [ValType::I64]);
        funcs.function(next_type);
        next_type += 1;
        let i = next_idx;
        next_idx += 1;
        i
    });
    // Every module runs each bar in its own function, `bar_step(bar)`. `run`
    // is one call that loops over every bar, and an engine without on-stack
    // replacement for WebAssembly (V8) cannot move a running call to
    // optimized code; a call per bar can be. A chart module's `bar_step` is
    // its last defined function; standalone WebAssembly's sits just before
    // the entry point, which stays last.
    let bar_step_idx = standalone_step_idx.or_else(|| {
        types.ty().function([ValType::I32], [ValType::I32]);
        funcs.function(next_type);
        next_type += 1;
        let i = next_idx;
        next_idx += 1;
        Some(i)
    });
    // A linked chart module's three entry wrappers come after every
    // function the body planned, so those keep the indices they have without
    // a host (`bar_step` is the last of them), and are defined last.
    let chart_entries = chart.map(|_| {
        types.ty().function([ValType::I32], [ValType::I32]);
        funcs.function(next_type);
        types
            .ty()
            .function([ValType::I32, ValType::I32], [ValType::I32]);
        funcs.function(next_type + 1);
        types.ty().function([], [ValType::I32]);
        funcs.function(next_type + 2);
        ChartEntries {
            prepare: next_idx,
            init: next_idx + 1,
            run: next_idx + 2,
        }
    });
    let _ = next_idx;
    let _ = next_type;

    let mut memory = MemorySection::new();

    let heap_globals = match globals.heap {
        Some(heap::HeapGlobals { gc: Some(_), .. }) => 8,
        Some(_) => 4,
        None => 0,
    };
    let i32_globals = globals.count - globals.base - heap_globals;
    for _ in 0..i32_globals {
        globals_sec.global(
            GlobalType {
                val_type: ValType::I32,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i32_const(0),
        );
    }
    if globals.heap.is_some() {
        // base, ptr (i32) then elems, objs (i64) — the plan order.
        for _ in 0..2 {
            globals_sec.global(
                GlobalType {
                    val_type: ValType::I32,
                    mutable: true,
                    shared: false,
                },
                &ConstExpr::i32_const(0),
            );
        }
        for _ in 0..2 {
            globals_sec.global(
                GlobalType {
                    val_type: ValType::I64,
                    mutable: true,
                    shared: false,
                },
                &ConstExpr::i64_const(0),
            );
        }
    }
    if let Some(heap::HeapGlobals { gc: Some(_), .. }) = globals.heap {
        // arena, arena_due, elems_due (i64) then epoch (i32) — the plan
        // order.
        for _ in 0..3 {
            globals_sec.global(
                GlobalType {
                    val_type: ValType::I64,
                    mutable: true,
                    shared: false,
                },
                &ConstExpr::i64_const(0),
            );
        }
        globals_sec.global(
            GlobalType {
                val_type: ValType::I32,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i32_const(0),
        );
    }

    for ty in &cur_cells.global_tys {
        let (val_type, init) = match ty {
            VTy::F64 => (ValType::F64, ConstExpr::f64_const(f64::NAN.into())),
            VTy::I64 => (ValType::I64, ConstExpr::i64_const(i64::MIN)),
            VTy::Bool3 => (ValType::I32, ConstExpr::i32_const(1)),
            VTy::Color | VTy::Str | VTy::Ref => (ValType::I32, ConstExpr::i32_const(-1)),
        };
        globals_sec.global(
            GlobalType {
                val_type,
                mutable: true,
                shared: false,
            },
            &init,
        );
    }
    // `bar_step` hands the cumulative run budget back to `run` in this global,
    // the last one, after any current-cell globals.
    let rb_global = bar_step_idx.map(|_| {
        globals_sec.global(
            GlobalType {
                val_type: ValType::I64,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i64_const(0),
        );
        globals.count + cur_cells.global_count()
    });
    if let Some(stops) = stops {
        debug_assert_eq!(rb_global.map(|g| g + 1), Some(stops.budget_global));
        for _ in 0..2 {
            globals_sec.global(
                GlobalType {
                    val_type: ValType::I64,
                    mutable: true,
                    shared: false,
                },
                &ConstExpr::i64_const(0),
            );
        }
    }
    // A split body hands the per-bar counter from piece to piece in this
    // global, the last one; the run counter travels in `rb_global`.
    let bb_global = match (&split, stops) {
        (Some(_), Some(stops)) => {
            globals_sec.global(
                GlobalType {
                    val_type: ValType::I64,
                    mutable: true,
                    shared: false,
                },
                &ConstExpr::i64_const(0),
            );
            Some(stops.run_budget_global + 1)
        }
        _ => None,
    };
    // A linked chart module that reserves string bytes itself keeps what is
    // left of the string budget in one more global, after all of these
    // ([`StringFns`]).
    let linked = match (chart, rt) {
        (Some(_), Some(rt)) => Some(LinkedFns {
            stop: rt.chart_entry(runtime::chart::STOP),
            strings: reserves_strings(program).then(|| {
                let room = globals_sec.len();
                globals_sec.global(
                    GlobalType {
                        val_type: ValType::I64,
                        mutable: true,
                        shared: false,
                    },
                    &ConstExpr::i64_const(0),
                );
                StringFns {
                    room,
                    seed: rt.chart_entry(runtime::chart::STRING_ROOM),
                    vm: rt.chart_entry(runtime::chart::VM_STRINGS),
                    reserve: rt.chart_entry(runtime::chart::STRING_RESERVE),
                    len: rt.chart_entry(runtime::chart::STRING_LEN),
                }
            }),
        }),
        _ => None,
    };
    let hosts = HostFns {
        ta,
        ta_init,
        vm,
        pure,
        heap: heap_fns,
        broker,
        calendar,
        timestamp,
        stops,
        linked,
    };
    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    match (standalone_wasm, evaluate_idx) {
        (Some(_), Some(evaluate_idx)) => {
            // The documented standalone WebAssembly consumer surface, and nothing else:
            // `prepare`/`run` stay internal, called by the entry point; the
            // allocator family is the blob's, exported under the spec names
            // at its spliced indices (zero imports, so no shift).
            use crate::runtime::standalone_wasm;
            let rt = rt.expect("standalone WebAssembly mode forces links_runtime");
            exports.export("evaluate_indicator_cabi", ExportKind::Func, evaluate_idx);
            exports.export(
                "alloc",
                ExportKind::Func,
                rt.standalone_wasm_entry(standalone_wasm::ALLOC, import_count),
            );
            exports.export(
                "dealloc",
                ExportKind::Func,
                rt.standalone_wasm_entry(standalone_wasm::DEALLOC, import_count),
            );
            exports.export(
                "last_error",
                ExportKind::Func,
                rt.standalone_wasm_entry(standalone_wasm::LAST_ERROR, import_count),
            );
            exports.export(
                "reset",
                ExportKind::Func,
                rt.standalone_wasm_entry(standalone_wasm::RESET, import_count),
            );
        }
        _ => match (chart_entries, rt) {
            // A linked chart module: `prepare` and `run` as every chart
            // module has them, and the three exports its loader passes the
            // run's request in and reads the host's results back through.
            (Some(entries), Some(rt)) => {
                exports.export("prepare", ExportKind::Func, entries.prepare);
                exports.export("run", ExportKind::Func, entries.run);
                exports.export(
                    "chartAlloc",
                    ExportKind::Func,
                    rt.chart_entry(runtime::chart::ALLOC),
                );
                exports.export("chartInit", ExportKind::Func, entries.init);
                exports.export(
                    "chartCommand",
                    ExportKind::Func,
                    rt.chart_entry(runtime::chart::COMMAND),
                );
            }
            _ => {
                exports.export("prepare", ExportKind::Func, prepare_idx);
                exports.export("run", ExportKind::Func, run_idx);
            }
        },
    }

    // Static tables sit in the gap between the linked runtime's reserved
    // block and the artifact's lanes: the standalone WebAssembly key table first (its address is
    // part of that contract), then the ta engine's declaration table. The
    // lane floor rises past whatever is there.
    let tables_base = rt.map_or(0, runtime::Runtime::reserved_hi);
    let keytab_len = standalone_wasm
        .map(|spec| checked_len_u32(spec.keytab.len(), "standalone key table"))
        .transpose()?
        .unwrap_or(0);
    let keytab_aligned = checked_align8(keytab_len, "standalone key table")?;
    let ta_decl_addr = checked_add_u32(tables_base, keytab_aligned, "TA declaration table")?;
    let ta_decl_bytes: Vec<u8> = layout
        .ta_decls
        .iter()
        .flat_map(pinewasm_ta::Decl::to_le_bytes)
        .collect();
    // The broker config block (strategy standalone WebAssembly modules): declaration scalars +
    // the program's string const pool, decoded by `pinewasm_rt_broker_init` (layout
    // defined in `runtime/src/broker.rs`). Empty for everything else, so
    // every non-strategy artifact's layout arithmetic — and its bytes — is
    // unchanged.
    let broker_cfg: Vec<u8> = if layout.links_broker {
        broker_config_bytes(program)
    } else {
        Vec::new()
    };
    let ta_decl_len = checked_len_u32(ta_decl_bytes.len(), "TA declaration table")?;
    let ta_decl_aligned = checked_align8(ta_decl_len, "TA declaration table")?;
    let broker_cfg_addr = checked_add_u32(ta_decl_addr, ta_decl_aligned, "broker configuration")?;
    let broker_cfg_len = checked_len_u32(broker_cfg.len(), "broker configuration")?;
    let broker_cfg_aligned = checked_align8(broker_cfg_len, "broker configuration")?;
    // A linked chart module's host plan follows, read once by `chartInit`.
    let host_plan_addr = checked_add_u32(broker_cfg_addr, broker_cfg_aligned, "chart host plan")?;
    let host_plan_len = chart
        .map(|plan| checked_len_u32(plan.len(), "chart host plan"))
        .transpose()?
        .unwrap_or(0);
    let host_plan_aligned = checked_align8(host_plan_len, "chart host plan")?;
    let lane_floor = if rt.is_some() {
        checked_add_u32(host_plan_addr, host_plan_aligned, "artifact lane floor")?
    } else {
        0
    };
    // Everything below the lane floor is initialized by active data
    // segments, so it must exist at instantiation. One page holds it for
    // every module but a linked chart module, whose runtime's data and host
    // plan run to megabytes.
    memory.memory(MemoryType {
        minimum: if chart.is_some() {
            u64::from(lane_floor).div_ceil(65536).max(1)
        } else {
            1
        },
        maximum: Some(if chart.is_some() {
            CHART_MEMORY_MAX_PAGES
        } else {
            MEMORY_MAX_PAGES
        }),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    if let Some(plan) = chart {
        data.active(
            0,
            &ConstExpr::i32_const(host_plan_addr as i32),
            plan.iter().copied(),
        );
    }
    let footprint = layout_footprint(program, layout, &slots, lane_floor);
    #[cfg(test)]
    footprint_reference::assert_matches(&footprint, program, layout, &slots, lane_floor);
    let footprint = footprint?;
    let relocation_bound = standalone_wasm
        .map(|_| {
            footprint.standalone_relocation().ok_or_else(|| {
                AotError::UnsupportedOpcode(
                    "standalone relocation layout exceeds wasm32 arithmetic".into(),
                )
            })
        })
        .transpose()?;
    let max_bars = footprint_max_bars(footprint, relocation_bound)?;
    let consts = prepare_consts(footprint).map_err(prepare_consts_error)?;

    let prepare = emit_prepare(
        layout,
        &slots,
        &globals,
        rt,
        &consts,
        max_bars,
        standalone_wasm,
    )?;
    #[cfg(test)]
    prepare_reference::assert_matches(
        &prepare,
        program,
        layout,
        &slots,
        &globals,
        rt,
        lane_floor,
        max_bars,
        standalone_wasm,
    );
    #[cfg(test)]
    prepare_splice::assert_splices(&prepare, &consts, max_bars, layout, &globals);
    code.function(&prepare);
    let (run_fn, step_fn, pieces) = emit_run(
        program,
        layout,
        &slots,
        analysis,
        &globals,
        &hosts,
        chart.is_some(),
        ta_decl_addr,
        standalone_wasm.map(|spec| spec.projection),
        carry,
        broker_cfg_addr,
        broker_cfg_len,
        bar_step_idx.zip(rb_global),
        &cur_cells,
        split
            .as_ref()
            .zip(bb_global)
            .map(|(plan, bb)| (plan, piece_fns.as_slice(), bb)),
    )?;
    code.function(&run_fn);
    if let (Some(g), Some(fns)) = (globals.heap, &hosts.heap) {
        // A strategy's heap shares the arena band with its broker.
        match (standalone_wasm, rt) {
            // A linked chart host allocates while `run()` executes, as a
            // linked broker does, so the heap takes its blocks from it too.
            (_, Some(rt)) if chart.is_some() => {
                code.function(&heap::arena_alloc_fn(rt.chart_entry(runtime::chart::ALLOC)))
            }
            (Some(spec), Some(rt)) if spec.strategy_source => code.function(&heap::arena_alloc_fn(
                rt.standalone_wasm_entry(runtime::standalone_wasm::ALLOC, import_count),
            )),
            _ => code.function(&heap::alloc_fn(g)),
        };
        code.function(&heap::new_fn(g, fns.alloc, heap::LANE_F64));
        code.function(&heap::new_fn(g, fns.alloc, heap::LANE_I64));
        code.function(&heap::new_fn(g, fns.alloc, heap::LANE_I32));
        code.function(&heap::push_fn(g, fns.alloc, heap::LANE_F64));
        code.function(&heap::push_fn(g, fns.alloc, heap::LANE_I64));
        code.function(&heap::push_fn(g, fns.alloc, heap::LANE_I32));
        code.function(&heap::shift_fn(g, heap::LANE_F64));
        code.function(&heap::shift_fn(g, heap::LANE_I64));
        code.function(&heap::shift_fn(g, heap::LANE_I32));
        code.function(&heap::pop_fn(g, heap::LANE_F64));
        code.function(&heap::pop_fn(g, heap::LANE_I64));
        code.function(&heap::pop_fn(g, heap::LANE_I32));
        code.function(&heap::agg_fn(false));
        code.function(&heap::agg_fn(true));
        if fns.var.is_some() {
            code.function(&heap::var_fn(false));
            code.function(&heap::var_fn(true));
        }
        if let (Some(plan), Some(_)) = (gc_plan, fns.collect) {
            // The arena band a strategy's heap shares with its broker cannot
            // be trimmed, and neither can the blocks a chart module's heap
            // takes from its linked host's allocator.
            let trim =
                chart.is_none() && !matches!(standalone_wasm, Some(spec) if spec.strategy_source);
            code.function(&emit_collect(
                plan, analysis, layout, &slots, &globals, &hosts, g, trim,
            )?);
        }
    }
    // The ta declaration table, for every artifact that links the engine.
    // Outside standalone WebAssembly this is an ordinary active segment: nothing writes below the
    // lane floor, so instantiation-time initialization is enough.
    if layout.links_ta && standalone_wasm.is_none() {
        data.active(
            0,
            &ConstExpr::i32_const(ta_decl_addr as i32),
            ta_decl_bytes.iter().copied(),
        );
    }
    for piece in &pieces {
        code.function(piece);
    }
    if let Some(stops) = hosts.stops {
        code.function(&emit_stop_fn(hosts.linked.map(|linked| linked.stop)));
        code.function(&emit_budget_stop_fn(stops.stop));
        code.function(&emit_charge_fn(stops));
    }
    let step_fn = match (standalone_step_idx, step_fn) {
        (Some(_), Some(step_fn)) => {
            code.function(&step_fn);
            None
        }
        (_, step_fn) => step_fn,
    };
    if let (Some(spec), Some(evaluate_idx)) = (standalone_wasm, evaluate_idx) {
        let rt = rt.expect("standalone WebAssembly mode forces links_runtime");
        // Both static tables ride as one passive segment `memory.init`ed per
        // call: active segments at `reserved_hi` would be overwritten by the
        // host's very first `alloc`, whose band starts there.
        let tables_segment = rt.data_count();
        let mut tables: Vec<u8> = spec.keytab.to_vec();
        tables.resize(keytab_aligned as usize, 0);
        tables.extend_from_slice(&ta_decl_bytes);
        if layout.links_broker {
            // Pad the ta table to the broker block's 8-aligned address; the
            // pad is emitted only for strategy modules so every existing
            // artifact's segment bytes are untouched.
            tables.resize((broker_cfg_addr - tables_base) as usize, 0);
            tables.extend_from_slice(&broker_cfg);
        }
        let tables_len = checked_len_u32(tables.len(), "standalone static tables")?;
        data.passive(tables);
        code.function(&emit_standalone_wasm_evaluate(
            layout,
            &globals,
            rt,
            spec,
            prepare_idx,
            run_idx,
            tables_segment,
            tables_len,
            max_bars,
            relocation_bound.expect("standalone mode has a relocation bound"),
        ));
        debug_assert_eq!(
            evaluate_idx,
            run_idx
                + 1
                + match &hosts.heap {
                    Some(fns) => {
                        15 + if fns.var.is_some() { 2 } else { 0 }
                            + u32::from(fns.collect.is_some())
                    }
                    None => 0,
                }
                + 4
                + piece_fns.len() as u32
        );
    }

    if let Some(step_fn) = &step_fn {
        code.function(step_fn);
    }

    if let (Some(entries), Some(rt)) = (chart_entries, rt) {
        debug_assert_eq!(entries.run, funcs.len() - 1);
        code.function(&emit_chart_prepare(prepare_idx));
        code.function(&emit_chart_init(
            host_plan_addr,
            host_plan_len,
            rt.chart_entry(runtime::chart::INIT),
        ));
        code.function(&emit_chart_run(
            run_idx,
            rt.chart_entry(runtime::chart::BEGIN),
        ));
    }
    let mut module = Module::new();
    module.section(&types);
    if import_count > 0 {
        module.section(&imports);
    }
    module.section(&funcs);
    // Only the chart runtime has a table and elements; they sit where the
    // binary format puts them, around the memory, globals and exports.
    if let Some(table) = rt.and_then(runtime::Runtime::table_section) {
        module.section(&table);
    }
    module
        .section(&memory)
        .section(&globals_sec)
        .section(&exports);
    if let Some(elements) = rt.and_then(runtime::Runtime::element_section) {
        module.section(&elements);
    }
    // `memory.init` requires the data-count section (bulk memory); emitted
    // only in standalone WebAssembly mode so every existing artifact's bytes are unchanged.
    if standalone_wasm.is_some() {
        let rt = rt.expect("standalone WebAssembly mode forces links_runtime");
        module.section(&DataCountSection {
            count: rt.data_count() + 1,
        });
    }
    module.section(&code);
    // The data section carries the linked runtime's rodata, so it must
    // precede the custom section the way the binary format requires.
    if rt.is_some() {
        module.section(&data);
    }
    append_metadata_sections(&mut module, program, standalone_wasm);
    Ok(module.finish())
}

/// Function indices of a linked chart module's three entry wrappers.
#[derive(Clone, Copy)]
struct ChartEntries {
    /// The exported `prepare` ([`emit_chart_prepare`]).
    prepare: u32,
    /// The exported `chartInit` ([`emit_chart_init`]).
    init: u32,
    /// The exported `run` ([`emit_chart_run`]).
    run: u32,
}

/// A linked chart module's exported `prepare(bars) -> status`: `prepare`,
/// once. The linked host's allocator takes its memory from `memory.grow`,
/// above the layout `prepare` made, so a second `prepare` (for more bars)
/// would lay its lanes over memory the host already owns. The header's
/// version cell is zero until `prepare` writes it, which is the test.
fn emit_chart_prepare(prepare: u32) -> Function {
    let mut f = Function::new([]);
    f.instruction(&I::I32Const(0))
        .instruction(&I::I32Load(mem_u32(header::OFF_ABI_VERSION)))
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::I32Const(status::ABI_MISMATCH))
        .instruction(&I::Return)
        .instruction(&I::End)
        .instruction(&I::LocalGet(0))
        .instruction(&I::Call(prepare))
        .instruction(&I::End);
    f
}

/// A linked chart module's exported `chartInit(request, len) -> status`:
/// the runtime's entry point with this module's host plan in front of the
/// loader's request.
fn emit_chart_init(host_plan_addr: u32, host_plan_len: u32, init: u32) -> Function {
    let mut f = Function::new([]);
    f.instruction(&I::I32Const(host_plan_addr as i32))
        .instruction(&I::I32Const(host_plan_len as i32))
        .instruction(&I::LocalGet(0))
        .instruction(&I::LocalGet(1))
        .instruction(&I::Call(init))
        .instruction(&I::End);
    f
}

/// A linked chart module's exported `run() -> status`: `run`, once, and
/// only after `chartInit` gave the module its host. `begin` is the linked
/// runtime's answer: 1 to run, 0 when no host was built, and anything else
/// when this instance already ran. A refused run is one the interpreter must
/// make, which is what the status tells the caller, and, when nothing ran,
/// the header. After a completed run the header is that run's record, which
/// the loader is still reading results by, so it is left alone.
fn emit_chart_run(run: u32, begin: u32) -> Function {
    let mut f = Function::new([(1, ValType::I32)]);
    f.instruction(&I::Call(begin))
        .instruction(&I::LocalTee(0))
        .instruction(&I::I32Const(1))
        .instruction(&I::I32Eq)
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::Call(run))
        .instruction(&I::Return)
        .instruction(&I::End)
        .instruction(&I::LocalGet(0))
        .instruction(&I::I32Eqz)
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::I32Const(0))
        .instruction(&I::I32Const(status::ABI_MISMATCH))
        .instruction(&I::I32Store(mem_u32(header::OFF_ERR_CODE)))
        .instruction(&I::End)
        .instruction(&I::I32Const(status::ABI_MISMATCH))
        .instruction(&I::End);
    f
}

/// Bytes `prepare` reserves for the linked ta engine: zero when the program
/// has no `CallTa` site.
fn ta_region_bytes(layout: &Layout) -> u32 {
    if layout.links_ta {
        layout.ta_layout.region_bytes()
    } else {
        0
    }
}

fn checked_len_u32(len: usize, what: &str) -> Result<u32, AotError> {
    u32::try_from(len).map_err(|_| {
        AotError::UnsupportedOpcode(format!("the {what} exceeds the wasm32 address space"))
    })
}

fn checked_align8(value: u32, what: &str) -> Result<u32, AotError> {
    value.checked_add(7).map(|n| n & !7).ok_or_else(|| {
        AotError::UnsupportedOpcode(format!("the aligned {what} address overflows wasm32"))
    })
}

fn checked_add_u32(lhs: u32, rhs: u32, what: &str) -> Result<u32, AotError> {
    lhs.checked_add(rhs)
        .ok_or_else(|| AotError::UnsupportedOpcode(format!("the {what} address overflows wasm32")))
}

/// [`LayoutFootprint::max_bars`] with its error mapped to the compiler's
/// diagnostic.
fn footprint_max_bars(
    footprint: LayoutFootprint,
    relocation: Option<StandaloneRelocationBound>,
) -> Result<u32, AotError> {
    footprint.max_bars(relocation).map_err(|error| match error {
        MaxBarsError::FixedLayoutExceedsMemory => AotError::UnsupportedOpcode(format!(
            "the fixed native memory layout exceeds the declared {MEMORY_MAX_PAGES}-page WebAssembly memory maximum"
        )),
    })
}

fn layout_footprint(
    program: &CsilProgram,
    layout: &Layout,
    slots: &SlotLayout,
    lane_floor: u32,
) -> Result<LayoutFootprint, AotError> {
    let inputs = u64::try_from(program.inputs.len())
        .map_err(|_| AotError::UnsupportedOpcode("input-table layout overflows".into()))?;
    let counts = FootprintCounts {
        inputs,
        lane_floor,
        // `request.security` lanes extend the builtin region exactly like
        // source lanes do, so the proved footprint counts them together.
        source_inputs: layout.source_inputs.len() as u64 + u64::from(layout.security_lanes),
        color_lanes: total_color_lanes(layout),
        plot_count: layout.plot_count,
        alert_sites: layout.alert_sites,
        slot_bytes: slots.bytes,
        slot_count: layout.slot_count,
        scratch_cells: layout.scratch_cells,
        ta_bytes: ta_region_bytes(layout),
    };
    footprint_of_counts(counts).map_err(footprint_error)
}

/// The compiler diagnostic for each [`FootprintError`] (the messages the
/// constructor has always reported).
fn footprint_error(error: FootprintError) -> AotError {
    let message = match error {
        FootprintError::InputTableOverflow => "input-table layout overflows".to_string(),
        FootprintError::BuiltinLaneCountOverflow => "builtin-lane count overflows".to_string(),
        FootprintError::ColorLaneCountOverflow => "color-lane count overflows".to_string(),
        FootprintError::BuiltinStrideOverflow => "builtin-lane stride overflows".to_string(),
        FootprintError::PlotStrideOverflow => "plot-lane stride overflows".to_string(),
        FootprintError::ColorStrideOverflow => "color-lane stride overflows".to_string(),
        FootprintError::AlertStrideOverflow => "alert-lane stride overflows".to_string(),
        FootprintError::ScratchSizeOverflow => "scratch-region size overflows".to_string(),
        FootprintError::StrideOutOfRange(name) => {
            let name = match name {
                StrideName::Builtin => "builtin-lane",
                StrideName::Plot => "plot-lane",
                StrideName::Color => "color-lane",
                StrideName::Alert => "alert-lane",
            };
            format!("the {name} per-bar layout coefficient exceeds wasm32 signed immediate range")
        }
    };
    AotError::UnsupportedOpcode(message)
}

/// Per-slot history rings for both tiers. An input-backed average or change
/// reserves the largest legal lookback only in the slots that operation
/// reads; every other slot keeps the program's ordinary history capacity, so
/// a few adjustable lookbacks do not multiply the maximum window by every
/// temporary in a large script. Rings are internal working state in the
/// module ABI (consumers never address them), so Universal Wasm takes the
/// same plan as standalone WebAssembly; only the lane omissions
/// (`history_observed`, `written_observed`) differ by tier ([`SlotLanes`]).
pub(crate) struct SlotLayout {
    rings: Vec<u32>,
    offsets: Vec<u32>,
    bytes: u32,
    history_observed: Vec<bool>,
    written_observed: Vec<bool>,
    sma: Vec<Option<SmaLayout>>,
}

/// Dense observation rings are independent of bar-indexed slot history.
/// Metadata stores last-bar-plus-one, observation count, bar-start count,
/// and the overwritten current-bar cell used for same-bar replacement.
pub(crate) const SMA_META_BYTES: u32 = 24;

#[derive(Clone, Copy)]
pub(crate) struct SmaLayout {
    pub relative_offset: u32,
    pub mask: u32,
}

/// Which slot lanes and written flags a module keeps written. A slot's
/// current value lives in its own cell either way; its lane holds the value
/// of each bar, and its written flag says whether this bar stored to it.
#[derive(Clone, Copy)]
pub(crate) enum SlotLanes<'a> {
    /// All of them: a module that imports its host keeps its lanes as it
    /// always has.
    All,
    /// What standalone WebAssembly keeps: the lanes and flags its own code
    /// reads back.
    Standalone,
    /// What a linked chart module keeps: the lanes and flags its own code
    /// reads back, and the lanes of `styles`, which its host reads to
    /// resolve the display styles (`chartCommand` 6). Nothing else can
    /// address a linked module's lanes: its host reaches the module only
    /// through the scratch cells, and its loader through `chartCommand`.
    Linked { styles: &'a BTreeSet<u32> },
}

/// The slots whose cell at the last visible bar gives a plot, visual or fill
/// one of its fixed presentation values.
fn style_slot_ids(program: &CsilProgram) -> BTreeSet<u32> {
    program
        .plots
        .iter()
        .map(|plot| &plot.style)
        .chain(program.visuals.iter().map(|visual| &visual.style))
        .chain(program.fills.iter().map(|fill| &fill.style))
        .flat_map(|style| style.runtime_scalars.iter().map(|binding| binding.slot.0))
        .collect()
}

/// The slot whose lane `instruction` reads: a history read or a window over
/// the slot's past cells. A read of the current bar (`ReadUserSlot` at
/// offset 0) takes the slot's current cell and reads no lane.
fn lane_read(instruction: &CsilInstr) -> Option<u32> {
    match instruction {
        CsilInstr::ReadUserSlot { slot, offset } if *offset != 0 => Some(*slot),
        CsilInstr::ReadUserSlotDynamic { slot }
        | CsilInstr::WindowMean { slot, .. }
        | CsilInstr::WindowMeanDynamic { slot }
        | CsilInstr::WindowExtremumDynamic { slot, .. }
        | CsilInstr::WindowStatisticDynamic { slot, .. }
        | CsilInstr::EmaSimpleDynamic { slot } => Some(*slot),
        _ => None,
    }
}

pub(crate) fn native_style_slots(
    program: &CsilProgram,
    analysis: &Analysis,
    layout: &Layout,
) -> Result<Vec<crate::NativeStyleSlot>, AotError> {
    // Offsets and masks are the same under every lane plan.
    let slots = SlotLayout::plan(program, layout, SlotLanes::All)?;
    style_slot_ids(program)
        .into_iter()
        .map(|slot| {
            let ty = *analysis.slot_ty.get(slot as usize).ok_or_else(|| {
                AotError::InvalidProgram(format!("style slot {slot} has no type"))
            })?;
            if ty == VTy::Ref {
                return Err(AotError::InvalidProgram(format!(
                    "style slot {slot} is a heap reference"
                )));
            }
            Ok(crate::NativeStyleSlot {
                slot,
                tag: crate::vty_tag(ty),
                offset: slots.offsets[slot as usize],
                mask: slots.mask(slot),
            })
        })
        .collect()
}

/// The ring size, in cells, of every user slot: a power of two above the
/// program's declared `max_bars_back`, every literal history offset and
/// window length on an ordinary slot, and, on an input-backed slot, its own
/// literal reads plus the largest legal input value.
fn slot_rings(program: &CsilProgram, slot_count: usize) -> Result<Vec<u32>, AotError> {
    let mut bound_depths = vec![0u32; slot_count];
    for binding in &program.ta_input_bindings {
        let Some((_, max)) = program.ta_input_bounds(binding) else {
            continue;
        };
        let (slot, depth) = match binding.target {
            pinewasm_hir::TaInputTarget::Sma { slot } => (slot, max as u32 - 1),
            pinewasm_hir::TaInputTarget::Change { slot } => (slot, max as u32),
            pinewasm_hir::TaInputTarget::State { .. } => continue,
        };
        bound_depths[slot as usize] = bound_depths[slot as usize].max(depth);
    }
    // A history ceiling permits reads; it does not require retaining every
    // temporary in an inlined helper function for that many bars.
    let mut own_depths = Vec::with_capacity(program.series_slots.len());
    for slot in &program.series_slots {
        own_depths.push(u32::from(slot.persistent || slot.varip));
    }
    for function in &program.functions {
        for instruction in &function.code {
            let (slot, depth) = match instruction {
                CsilInstr::ReadUserSlot { slot, offset } => (*slot as usize, *offset),
                CsilInstr::ReadUserSlotDynamic { slot } => {
                    (*slot as usize, program.metadata.max_bars_back)
                }
                CsilInstr::WindowMean { slot, length } => {
                    (*slot as usize, length.saturating_sub(1))
                }
                CsilInstr::WindowMeanDynamic { slot }
                | CsilInstr::WindowExtremumDynamic { slot, .. }
                | CsilInstr::WindowStatisticDynamic { slot, .. } => {
                    (*slot as usize, pinewasm_csil::MAX_WINDOW_LENGTH - 1)
                }
                _ => continue,
            };
            own_depths[slot] = own_depths[slot].max(depth);
        }
    }
    // Keep the result construction in explicit loops as well. The iterator
    // defaults for `zip`, `map`, and `collect` are not in the standard-library
    // subset this code is restricted to. Match `zip`'s left-to-right
    // truncation: request the second item only after the first iterator
    // produced one.
    let output_len = bound_depths.len().min(own_depths.len());
    let mut rings = Vec::with_capacity(output_len);
    let bounds = bound_depths.into_iter();
    let mut own = own_depths.into_iter();
    for bound in bounds {
        let own_depth = match own.next() {
            Some(own_depth) => own_depth,
            None => break,
        };
        let ring = bound
            .max(own_depth)
            .checked_add(1)
            .and_then(u32::checked_next_power_of_two)
            .ok_or_else(|| {
                AotError::UnsupportedOpcode(
                    "slot history ring exceeds the wasm32 address space".into(),
                )
            })?;
        rings.push(ring);
    }
    Ok(rings)
}

impl SlotLayout {
    fn plan(
        program: &CsilProgram,
        layout: &Layout,
        lanes: SlotLanes<'_>,
    ) -> Result<Self, AotError> {
        let standalone_wasm = matches!(lanes, SlotLanes::Standalone);
        let rings = slot_rings(program, layout.slot_count as usize)?;
        let mut bytes = 0u32;
        let mut offsets = Vec::with_capacity(rings.len());
        for ring in &rings {
            offsets.push(bytes);
            bytes = ring
                .checked_mul(8)
                .and_then(|lane_bytes| bytes.checked_add(lane_bytes))
                .ok_or_else(|| {
                    AotError::UnsupportedOpcode(
                        "slot history exceeds the wasm32 address space".into(),
                    )
                })?;
            if standalone_wasm && u64::from(bytes) > MEMORY_MAX_PAGES * 65536 {
                return Err(AotError::UnsupportedOpcode(
                    "slot history exceeds the standalone WebAssembly memory maximum".into(),
                ));
            }
        }
        let mut sma = vec![None; rings.len()];
        for instruction in program.functions.iter().flat_map(|function| &function.code) {
            let slot = match instruction {
                CsilInstr::WindowMean { slot, .. } | CsilInstr::WindowMeanDynamic { slot } => {
                    *slot as usize
                }
                _ => continue,
            };
            if sma[slot].is_some() {
                continue;
            }
            sma[slot] = Some(SmaLayout {
                relative_offset: bytes - offsets[slot],
                mask: rings[slot] - 1,
            });
            bytes = rings[slot]
                .checked_mul(8)
                .and_then(|cells| cells.checked_add(SMA_META_BYTES))
                .and_then(|state| bytes.checked_add(state))
                .ok_or_else(|| {
                    AotError::UnsupportedOpcode(
                        "SMA observation history exceeds the wasm32 address space".into(),
                    )
                })?;
            if standalone_wasm && u64::from(bytes) > MEMORY_MAX_PAGES * 65536 {
                return Err(AotError::UnsupportedOpcode(
                    "SMA observation history exceeds the standalone WebAssembly memory maximum"
                        .into(),
                ));
            }
        }
        let all = matches!(lanes, SlotLanes::All);
        let mut history_observed = vec![all; rings.len()];
        let mut written_observed = vec![all; rings.len()];
        if standalone_wasm {
            for (slot, def) in program.series_slots.iter().enumerate() {
                history_observed[slot] = def.persistent;
            }
            for instruction in program.functions.iter().flat_map(|function| &function.code) {
                match instruction {
                    CsilInstr::ReadUserSlot { slot, offset } if *offset != 0 => {
                        history_observed[*slot as usize] = true;
                    }
                    CsilInstr::ReadUserSlotDynamic { slot }
                    | CsilInstr::WindowMean { slot, .. } => {
                        history_observed[*slot as usize] = true;
                    }
                    CsilInstr::JumpIfSlotWritten { slot, .. } => {
                        written_observed[*slot as usize] = true;
                    }
                    _ => {}
                }
            }
        }
        if let SlotLanes::Linked { styles } = lanes {
            // A lane is read back by the bar entry of a slot that carries
            // its value forward, by a history read or window, by an
            // input-sized average or change, and by the host for a style.
            let mut observe = |slot: u32| {
                if let Some(observed) = history_observed.get_mut(slot as usize) {
                    *observed = true;
                }
            };
            for (slot, def) in program.series_slots.iter().enumerate() {
                if def.persistent || def.varip {
                    observe(slot as u32);
                }
            }
            for binding in &program.ta_input_bindings {
                if let pinewasm_hir::TaInputTarget::Sma { slot }
                | pinewasm_hir::TaInputTarget::Change { slot } = binding.target
                {
                    observe(slot);
                }
            }
            styles.iter().copied().for_each(&mut observe);
            for instruction in program.functions.iter().flat_map(|function| &function.code) {
                if let Some(slot) = lane_read(instruction) {
                    observe(slot);
                }
                if let CsilInstr::JumpIfSlotWritten { slot, .. } = instruction {
                    if let Some(observed) = written_observed.get_mut(*slot as usize) {
                        *observed = true;
                    }
                }
            }
        }
        Ok(Self {
            rings,
            offsets,
            bytes,
            history_observed,
            written_observed,
            sma,
        })
    }

    pub(crate) fn retains_history(&self, slot: u32) -> bool {
        self.history_observed[slot as usize]
    }

    pub(crate) fn observes_written(&self, slot: u32) -> bool {
        self.written_observed[slot as usize]
    }

    pub(crate) fn mask(&self, slot: u32) -> u32 {
        self.rings[slot as usize] - 1
    }

    pub(crate) fn sma(&self, slot: u32) -> SmaLayout {
        self.sma[slot as usize].expect("SMA instruction has a planned observation ring")
    }
}

/// The `wasm_encoder` instruction for one [`PrepareOp`] of `prepare`'s layout
/// prefix: a one-for-one lowering.
fn prepare_op_instruction(op: PrepareOp) -> I<'static> {
    match op {
        PrepareOp::LocalGet(index) => I::LocalGet(index),
        PrepareOp::LocalSet(index) => I::LocalSet(index),
        PrepareOp::GlobalSet(index) => I::GlobalSet(index),
        PrepareOp::I32Const(value) => I::I32Const(value),
        PrepareOp::I32Add => I::I32Add,
        PrepareOp::I32Sub => I::I32Sub,
        PrepareOp::I32Mul => I::I32Mul,
        PrepareOp::I32And => I::I32And,
        PrepareOp::I32ShrU => I::I32ShrU,
        PrepareOp::I32GtU => I::I32GtU,
        PrepareOp::I32Eq => I::I32Eq,
        PrepareOp::If => I::If(BlockType::Empty),
        PrepareOp::Return => I::Return,
        PrepareOp::End => I::End,
        PrepareOp::MemorySize => I::MemorySize(0),
        PrepareOp::MemoryGrow => I::MemoryGrow(0),
    }
}

/// `prepare(bars) -> status`: guard bars, size memory, compute every lane
/// base into globals + header, default the budget/mintick cells.
///
/// Every layout-derived immediate comes from `consts` (see
/// [`prepare_consts`]) or from [`lane_offset`] over its strides; this
/// function only splices them into a fixed instruction template.
fn emit_prepare(
    layout: &Layout,
    slots: &SlotLayout,
    globals: &Globals,
    rt: Option<&runtime::Runtime>,
    consts: &PrepareConsts,
    max_bars: u32,
    standalone_wasm: Option<&StandaloneWasmSpec>,
) -> Result<Function, AotError> {
    // locals: 0 = bars param; 1.. = region bases.
    let bars = prepare_consts::PREPARE_BARS;
    let builtin_base = prepare_consts::PREPARE_BUILTIN_BASE;
    let slots_base = prepare_consts::PREPARE_SLOTS_BASE;
    let written_base = prepare_consts::PREPARE_WRITTEN_BASE;
    let plots_base = prepare_consts::PREPARE_PLOTS_BASE;
    let colors_base = prepare_consts::PREPARE_COLORS_BASE;
    let scratch_base = prepare_consts::PREPARE_SCRATCH_BASE;
    let alerts_base = prepare_consts::PREPARE_ALERTS_BASE;
    let heap_end = prepare_consts::PREPARE_HEAP_END;
    let mut f = Function::new([(10, ValType::I32)]);

    // The layout prefix, from the bar guard to the memory growth, comes
    // from the extracted kernel (`prepare_consts::prepare_layout_ops`).
    let prepare_globals = PrepareGlobals {
        bars: globals.bars,
        stack_pointer: rt.map(|rt| PrepareStackPointer {
            global: rt.stack_pointer_global(),
            value: rt.reserved_hi() as i32,
        }),
        ta_base: globals.ta_base,
        heap: globals.heap.as_ref().map(|g| PrepareHeapGlobals {
            base: g.base,
            ptr: g.ptr,
        }),
    };
    for op in prepare_layout_ops(*consts, max_bars, prepare_globals) {
        f.instruction(&prepare_op_instruction(op));
    }

    // Header writes.
    let store_u32 = |f: &mut Function, off: u32, push: &dyn Fn(&mut Function)| {
        f.instruction(&I::I32Const(0));
        push(f);
        f.instruction(&I::I32Store(mem_u32(off)));
    };
    store_u32(&mut f, header::OFF_MAGIC, &|f| {
        f.instruction(&I::I32Const(header::MAGIC as i32));
    });
    store_u32(&mut f, header::OFF_ABI_VERSION, &|f| {
        f.instruction(&I::I32Const(header::ABI_VERSION as i32));
    });
    store_u32(&mut f, header::OFF_BARS, &|f| {
        f.instruction(&I::LocalGet(bars));
    });
    store_u32(&mut f, header::OFF_RING_MASK, &|f| {
        f.instruction(&I::I32Const(layout.mask() as i32));
    });
    store_u32(&mut f, header::OFF_BUILTIN_BASE, &|f| {
        f.instruction(&I::LocalGet(builtin_base));
    });
    store_u32(&mut f, header::OFF_SLOTS_BASE, &|f| {
        f.instruction(&I::LocalGet(slots_base));
    });
    store_u32(&mut f, header::OFF_PLOTS_BASE, &|f| {
        f.instruction(&I::LocalGet(plots_base));
    });
    store_u32(&mut f, header::OFF_COLORS_BASE, &|f| {
        f.instruction(&I::LocalGet(colors_base));
    });
    store_u32(&mut f, header::OFF_HEAP_END, &|f| {
        f.instruction(&I::LocalGet(heap_end));
    });
    store_u32(&mut f, header::OFF_ERR_CODE, &|f| {
        f.instruction(&I::I32Const(status::OK));
    });
    store_u32(&mut f, header::OFF_SCRATCH_BASE, &|f| {
        f.instruction(&I::LocalGet(scratch_base));
    });
    store_u32(&mut f, header::OFF_ALERTS_BASE, &|f| {
        f.instruction(&I::LocalGet(alerts_base));
    });
    store_u32(&mut f, header::OFF_ALERT_COUNT, &|f| {
        f.instruction(&I::I32Const(0));
    });
    f.instruction(&I::I32Const(0))
        .instruction(&I::I64Const(DEFAULT_BUDGET))
        .instruction(&I::I64Store(mem_u64(header::OFF_BUDGET_PER_BAR)));
    f.instruction(&I::I32Const(0))
        .instruction(&I::F64Const(DEFAULT_MINTICK.into()))
        .instruction(&I::F64Store(mem_f64_at(header::OFF_MINTICK)));
    if globals.heap.is_some() {
        let max_collection_elements = standalone_wasm
            .map_or(crate::heap_defaults::MAX_ELEMENTS, |spec| {
                spec.max_collection_elements
            });
        f.instruction(&I::I32Const(0))
            .instruction(&I::I64Const(max_collection_elements as i64))
            .instruction(&I::I64Store(mem_u64(header::OFF_MAX_ELEMS)));
        f.instruction(&I::I32Const(0))
            .instruction(&I::I64Const(crate::heap_defaults::MAX_OBJECTS as i64))
            .instruction(&I::I64Store(mem_u64(header::OFF_MAX_OBJS)));
    }

    // Lane-base globals.
    for (lane, cell) in globals.builtin.iter().enumerate() {
        if let Some(g) = cell {
            let offset = prepare_lane_offset(lane, 8, consts.builtin_stride)?;
            f.instruction(&I::LocalGet(builtin_base))
                .instruction(&I::LocalGet(bars))
                .instruction(&I::I32Const(offset))
                .instruction(&I::I32Mul)
                .instruction(&I::I32Add)
                .instruction(&I::GlobalSet(*g));
        }
    }
    for (j, &input) in layout.source_inputs.iter().enumerate() {
        let g = globals.source_lane_global(input).expect("planned");
        let offset =
            prepare_lane_offset(BUILTIN_LANE_COUNT as usize + j, 8, consts.builtin_stride)?;
        f.instruction(&I::LocalGet(builtin_base))
            .instruction(&I::LocalGet(bars))
            .instruction(&I::I32Const(offset))
            .instruction(&I::I32Mul)
            .instruction(&I::I32Add)
            .instruction(&I::GlobalSet(g));
    }
    for (k, &g) in globals.security_lanes().iter().enumerate() {
        let offset = prepare_lane_offset(
            BUILTIN_LANE_COUNT as usize + layout.source_inputs.len() + k,
            8,
            consts.builtin_stride,
        )?;
        f.instruction(&I::LocalGet(builtin_base))
            .instruction(&I::LocalGet(bars))
            .instruction(&I::I32Const(offset))
            .instruction(&I::I32Mul)
            .instruction(&I::I32Add)
            .instruction(&I::GlobalSet(g));
    }
    for (slot, &g) in globals.slot.iter().enumerate() {
        f.instruction(&I::LocalGet(slots_base))
            .instruction(&I::I32Const(slots.offsets[slot] as i32))
            .instruction(&I::I32Add)
            .instruction(&I::GlobalSet(g));
    }
    for (plot, &g) in globals.plot.iter().enumerate() {
        let offset = prepare_lane_offset(plot, 8, consts.plot_stride)?;
        f.instruction(&I::LocalGet(plots_base))
            .instruction(&I::LocalGet(bars))
            .instruction(&I::I32Const(offset))
            .instruction(&I::I32Mul)
            .instruction(&I::I32Add)
            .instruction(&I::GlobalSet(g));
    }
    for (color_lane, g) in globals.color_lanes().enumerate() {
        let offset = prepare_lane_offset(color_lane, 4, consts.color_stride)?;
        f.instruction(&I::LocalGet(colors_base))
            .instruction(&I::LocalGet(bars))
            .instruction(&I::I32Const(offset))
            .instruction(&I::I32Mul)
            .instruction(&I::I32Add)
            .instruction(&I::GlobalSet(g));
    }
    f.instruction(&I::LocalGet(written_base))
        .instruction(&I::GlobalSet(globals.written_base));
    f.instruction(&I::LocalGet(scratch_base))
        .instruction(&I::GlobalSet(globals.scratch_base));
    // Scratch cell 0 is idle between `prepare` and `run`: seed the cumulative
    // run budget there without changing ABI v6's header layout. A loader may
    // overwrite it after `prepare`; `run` copies it into a local before any
    // `host.vm` call reuses the cell for per-bar budget exchange.
    f.instruction(&I::LocalGet(scratch_base));
    if standalone_wasm.is_some() {
        // max(DEFAULT_RUN_BUDGET, bars * STANDALONE_WASM_RUN_BUDGET_PER_BAR);
        // a u32 bar count times the allowance cannot overflow an i64.
        let scaled = |f: &mut Function| {
            f.instruction(&I::LocalGet(bars))
                .instruction(&I::I64ExtendI32U)
                .instruction(&I::I64Const(STANDALONE_WASM_RUN_BUDGET_PER_BAR))
                .instruction(&I::I64Mul);
        };
        scaled(&mut f);
        f.instruction(&I::I64Const(DEFAULT_RUN_BUDGET));
        scaled(&mut f);
        f.instruction(&I::I64Const(DEFAULT_RUN_BUDGET))
            .instruction(&I::I64GtU)
            .instruction(&I::Select);
    } else {
        f.instruction(&I::I64Const(DEFAULT_RUN_BUDGET));
    }
    f.instruction(&I::I64Store(mem_u64(header::SCRATCH_OFF_BUDGET_PER_RUN)));

    // Zero the written flags (fresh run semantics even on instance reuse).
    f.instruction(&I::LocalGet(written_base))
        .instruction(&I::I32Const(0))
        .instruction(&I::I32Const(consts.written_len))
        .instruction(&I::MemoryFill(0));

    f.instruction(&I::I32Const(status::OK))
        .instruction(&I::Return)
        .instruction(&I::End);
    Ok(f)
}

/// [`lane_offset`] for lane `lane` of a region `width` bytes wide per lane,
/// with its error mapped to the compiler's diagnostic.
fn prepare_lane_offset(lane: usize, width: u64, stride: i32) -> Result<i32, AotError> {
    let lane = u64::try_from(lane)
        .map_err(|_| prepare_consts_error(PrepareConstsError::LaneOutOfRange))?;
    lane_offset(lane, width, stride).map_err(prepare_consts_error)
}

/// A [`PrepareConstsError`] as the compiler's diagnostic. Unreachable after
/// [`footprint_max_bars`] admits the layout: the fixed layout then fits the
/// declared memory, and `layout_footprint` bounds every per-bar stride.
fn prepare_consts_error(error: PrepareConstsError) -> AotError {
    AotError::UnsupportedOpcode(match error {
        PrepareConstsError::ImmediateOutOfRange => {
            "a prepare layout immediate exceeds the wasm32 signed immediate range".into()
        }
        PrepareConstsError::LaneOutOfRange => {
            "a prepare lane offset lies outside its region".into()
        }
    })
}

/// Whether a linked chart module of `program` keeps the string budget
/// ([`StringFns`]): the program has an instruction the interpreter reserves
/// string bytes at and a module may run itself. One that is a host site
/// reserves in the host. When every one of them is, the budget is kept for
/// nothing, which costs a run one call and each host call one operand.
fn reserves_strings(program: &CsilProgram) -> bool {
    program.main().code.iter().any(|instr| {
        matches!(
            instr,
            CsilInstr::PushStr(_) | CsilInstr::AlertCondition | CsilInstr::CopyObject { .. }
        )
    })
}

/// `stop(code, pc, bar) -> status` ([`StopFns::stop`]): record the stop in
/// the header and return its status. A linked chart module first asks its
/// host (`linked_stop`, [`LinkedFns::stop`]) what the stop becomes.
fn emit_stop_fn(linked_stop: Option<u32>) -> Function {
    let mut f = Function::new([]);
    if let Some(linked_stop) = linked_stop {
        f.instruction(&I::LocalGet(0))
            .instruction(&I::LocalGet(1))
            .instruction(&I::LocalGet(2))
            .instruction(&I::Call(linked_stop))
            .instruction(&I::LocalSet(0));
    }
    f.instruction(&I::I32Const(0))
        .instruction(&I::LocalGet(0))
        .instruction(&I::I32Store(mem_u32(header::OFF_ERR_CODE)))
        .instruction(&I::I32Const(0))
        .instruction(&I::LocalGet(2))
        .instruction(&I::I32Store(mem_u32(header::OFF_ERR_BAR)))
        .instruction(&I::I32Const(0))
        .instruction(&I::LocalGet(1))
        .instruction(&I::I32Store(mem_u32(header::OFF_ERR_PC)))
        .instruction(&I::LocalGet(0))
        .instruction(&I::End);
    f
}

/// Whether a module can carry `gc`'s collector; otherwise it keeps the
/// never-collect rule. A standalone module has no host heap. When the plan
/// has host objects (drawings), the module erases their sites, which
/// `projection::certify` allows only when nothing else in the program
/// allocates: that module has nothing of its own to collect, and the
/// certificate already keeps the interpreter inside its budgets. Any other
/// standalone module collects its native heap as a chart module does. A host
/// collection needs `host.vm` and three scratch cells. Every root must be a
/// lane cell no deeper than its ring: a root is a `var` slot or one read
/// back, and both keep their lane in every module.
fn collector_fits(
    gc: &crate::analysis::GcPlan,
    layout: &Layout,
    slots: &SlotLayout,
    standalone: bool,
) -> bool {
    if gc.hosted && (standalone || !layout.imports_vm || layout.scratch_cells < 3) {
        return false;
    }
    gc.roots
        .iter()
        .all(|root| slots.retains_history(root.slot) && root.depth <= slots.mask(root.slot))
}

/// The module's collector ([`heap::collect_fn`]) for `plan`, which
/// [`collector_fits`] has admitted.
#[allow(clippy::too_many_arguments)]
fn emit_collect(
    plan: &crate::analysis::GcPlan,
    analysis: &Analysis,
    layout: &Layout,
    slots: &SlotLayout,
    globals: &Globals,
    hosts: &HostFns,
    g: heap::HeapGlobals,
    trim: bool,
) -> Result<Function, AotError> {
    let invalid = |why: &str| AotError::InvalidProgram(format!("collector: {why}"));
    let mut roots = Vec::with_capacity(plan.roots.len());
    for root in &plan.roots {
        roots.push(heap::CollectRoot {
            lane: globals.slot_global(root.slot),
            mask: slots.mask(root.slot),
            depth: root.depth,
            kind: root.kind,
        });
    }
    let heap_plan = analysis
        .heap
        .as_ref()
        .ok_or_else(|| invalid("no heap plan"))?;
    let field_counts = heap_plan
        .layouts
        .iter()
        .map(|(&tid, layout)| (tid, layout.len() as u32))
        .collect();
    let host = if plan.hosted {
        let vm = hosts
            .vm
            .ok_or_else(|| invalid("host objects without host.vm"))?;
        let stop = hosts.stops.ok_or_else(|| invalid("no stop function"))?.stop;
        Some(heap::CollectHost {
            vm,
            scratch_base: globals.scratch_base,
            cells: layout.scratch_cells,
            stop,
        })
    } else {
        None
    };
    Ok(heap::collect_fn(
        g,
        &roots,
        &field_counts,
        &plan.traced,
        host,
        trim,
    ))
}

/// `budget_stop(pc, budget, cost, bar) -> status` ([`StopFns::budget_stop`]).
fn emit_budget_stop_fn(stop: u32) -> Function {
    let mut f = Function::new([]);
    f.instruction(&I::I32Const(status::BUDGET_EXCEEDED))
        .instruction(&I::I32Const(status::RUN_BUDGET_EXCEEDED))
        .instruction(&I::LocalGet(1))
        .instruction(&I::LocalGet(2))
        .instruction(&I::I64LtU)
        .instruction(&I::Select)
        .instruction(&I::LocalGet(0))
        .instruction(&I::LocalGet(3))
        .instruction(&I::Call(stop))
        .instruction(&I::End);
    f
}

/// `charge(pc, cost, bar) -> stopped` ([`StopFns::charge`]).
fn emit_charge_fn(stops: StopFns) -> Function {
    let (budget, run_budget) = (stops.budget_global, stops.run_budget_global);
    let mut f = Function::new([]);
    f.instruction(&I::GlobalGet(budget))
        .instruction(&I::LocalGet(1))
        .instruction(&I::I64LtU)
        .instruction(&I::GlobalGet(run_budget))
        .instruction(&I::LocalGet(1))
        .instruction(&I::I64LtU)
        .instruction(&I::I32Or)
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::LocalGet(0))
        .instruction(&I::GlobalGet(budget))
        .instruction(&I::LocalGet(1))
        .instruction(&I::LocalGet(2))
        .instruction(&I::Call(stops.budget_stop))
        .instruction(&I::Drop)
        .instruction(&I::I32Const(1))
        .instruction(&I::Return)
        .instruction(&I::End)
        .instruction(&I::GlobalGet(budget))
        .instruction(&I::LocalGet(1))
        .instruction(&I::I64Sub)
        .instruction(&I::GlobalSet(budget))
        .instruction(&I::GlobalGet(run_budget))
        .instruction(&I::LocalGet(1))
        .instruction(&I::I64Sub)
        .instruction(&I::GlobalSet(run_budget))
        .instruction(&I::I32Const(0))
        .instruction(&I::End);
    f
}

/// `run()`: the per-run prologue, then every bar. With `split` (`bar_step`'s
/// function index and the run-budget global), the bar's code goes into a
/// second function, returned beside `run`, that `run` calls once per bar.
/// With `pieces` too (the split plan, each piece's function index and the
/// per-bar budget global), that function runs the bar's preamble and calls
/// the first piece, and the pieces are returned last, in plan order.
#[allow(clippy::too_many_arguments)]
fn emit_run(
    program: &CsilProgram,
    layout: &Layout,
    slots: &SlotLayout,
    analysis: &Analysis,
    globals: &Globals,
    hosts: &HostFns,
    linked_chart: bool,
    ta_decl_addr: u32,
    projection: Option<&crate::projection::StandaloneWasmProjection>,
    carry: Option<&crate::fraction::Carried>,
    broker_cfg_addr: u32,
    broker_cfg_len: u32,
    split: Option<(u32, u32)>,
    cur_cells: &CurCells,
    pieces: Option<(&crate::body::split::SplitPlan, &[u32], u32)>,
) -> Result<(Function, Option<Function>, Vec<Function>), AotError> {
    let max_stack = program.main().max_stack;
    let locals = Locals::new(max_stack, cur_cells, globals.count)?;

    let mut decl: Vec<(u32, ValType)> = vec![
        (1, ValType::I32), // bar
        (2, ValType::I64), // per-bar budget, cumulative run budget
        (3, ValType::F64), // tf0..tf2
        (4, ValType::I64), // ti0..ti3
        (3, ValType::I32), // t32a, t32b, t32c
        (1, ValType::I32), // wm_i
        (1, ValType::F64), // wm_sum
    ];
    for _ in 0..max_stack {
        decl.push((1, ValType::F64));
        decl.push((1, ValType::I64));
        decl.push((1, ValType::I32));
        decl.push((1, ValType::I32)); // independent integer validity
    }
    if analysis.slot_ty.len() < layout.slot_count as usize {
        return Err(AotError::InvalidProgram(format!(
            "slot {} has no analyzed type",
            analysis.slot_ty.len()
        )));
    }
    for slot_ty in &cur_cells.local_tys {
        decl.push((
            1,
            match slot_ty {
                VTy::F64 => ValType::F64,
                VTy::I64 => ValType::I64,
                VTy::Bool3 | VTy::Color | VTy::Str | VTy::Ref => ValType::I32,
            },
        ));
    }
    // `bar_step(bar)` takes the bar as parameter 0, so its declared locals are
    // `run`'s minus the leading bar local, and every local index is the same
    // in both functions. No local carries a value from one bar to the next:
    // the bar's code reloads the per-bar budget, every current cell and every
    // output cell from memory, and the run budget travels in a global.
    let step_decl: Vec<(u32, ValType)> = decl[1..].to_vec();
    let mut f = Function::new(decl);

    // The loader may overwrite this scratch cell after `prepare`. Keep the
    // remaining run budget in a local from here on: hosted instructions reuse
    // the same scratch cell for their per-bar budget I/O.
    f.instruction(&I::GlobalGet(globals.scratch_base))
        .instruction(&I::I64Load(mem_u64(header::SCRATCH_OFF_BUDGET_PER_RUN)));
    match split {
        Some((_, rb)) => f.instruction(&I::GlobalSet(rb)),
        None => f.instruction(&I::LocalSet(locals.run_budget)),
    };

    // A linked module that reserves string bytes itself starts from what its
    // host says is left of the string budget.
    if let Some(strings) = hosts.linked.and_then(|linked| linked.strings) {
        f.instruction(&I::Call(strings.seed))
            .instruction(&I::GlobalSet(strings.room));
    }

    // Fresh-run semantics on instance reuse: the alert lane is append-only,
    // so a second `run` has to start from an empty one.
    if layout.alert_sites > 0 {
        f.instruction(&I::I32Const(0))
            .instruction(&I::I32Const(0))
            .instruction(&I::I32Store(mem_u32(header::OFF_ALERT_COUNT)));
    }

    for binding in &program.ta_input_bindings {
        let rule = ta_input_rule(program, binding);
        emit_ta_input_value(&mut f, binding, rule);
        f.instruction(&I::LocalSet(locals.ti0));
        match rule {
            Some(pinewasm_hir::TaParamRule::FloatBits { positive }) => {
                // Reinterpret the host's exact bits. Ordered comparison with
                // MAX rejects both infinities and NaNs.
                f.instruction(&I::LocalGet(locals.ti0))
                    .instruction(&I::F64ReinterpretI64)
                    .instruction(&I::F64Abs)
                    .instruction(&I::F64Const(f64::MAX.into()))
                    .instruction(&I::F64Le)
                    .instruction(&I::I32Eqz);
                if positive {
                    f.instruction(&I::LocalGet(locals.ti0))
                        .instruction(&I::F64ReinterpretI64)
                        .instruction(&I::F64Const(0.0.into()))
                        .instruction(&I::F64Le)
                        .instruction(&I::I32Or);
                }
            }
            Some(pinewasm_hir::TaParamRule::Percent) => {
                f.instruction(&I::LocalGet(locals.ti0))
                    .instruction(&I::F64ReinterpretI64)
                    .instruction(&I::F64Const(0.0.into()))
                    .instruction(&I::F64Lt)
                    .instruction(&I::LocalGet(locals.ti0))
                    .instruction(&I::F64ReinterpretI64)
                    .instruction(&I::F64Const(100.0.into()))
                    .instruction(&I::F64Gt)
                    .instruction(&I::I32Or)
                    .instruction(&I::LocalGet(locals.ti0))
                    .instruction(&I::F64ReinterpretI64)
                    .instruction(&I::F64Abs)
                    .instruction(&I::F64Const(f64::MAX.into()))
                    .instruction(&I::F64Le)
                    .instruction(&I::I32Eqz)
                    .instruction(&I::I32Or);
            }
            _ => {
                let (min, max) = program.ta_input_bounds(binding).expect("validated binding");
                f.instruction(&I::LocalGet(locals.ti0))
                    .instruction(&I::I64Const(min))
                    .instruction(&I::I64LtS)
                    .instruction(&I::LocalGet(locals.ti0))
                    .instruction(&I::I64Const(max))
                    .instruction(&I::I64GtS)
                    .instruction(&I::I32Or);
            }
        }
        f.instruction(&I::If(BlockType::Empty))
            .instruction(&I::I32Const(0))
            .instruction(&I::I32Const(status::INVALID_TA_LENGTH))
            .instruction(&I::I32Store(mem_u32(header::OFF_ERR_CODE)))
            .instruction(&I::I32Const(status::INVALID_TA_LENGTH))
            .instruction(&I::Return)
            .instruction(&I::End);
        if let pinewasm_hir::TaInputTarget::State { state, param } = binding.target {
            let offset = ta_decl_addr + state * pinewasm_ta::Decl::SIZE as u32 + 8 + param * 8;
            f.instruction(&I::I32Const(0))
                .instruction(&I::LocalGet(locals.ti0))
                .instruction(&I::I64Store(mem_u64(offset)));
        }
    }

    // Like the TA engine, SMA observation state starts fresh on every run,
    // including reuse of a resident module. Counts define readable cells;
    // stale ring contents cannot become observations in a new evaluation.
    for (slot, state) in slots.sma.iter().enumerate() {
        if let Some(state) = state {
            f.instruction(&I::GlobalGet(globals.slot[slot]))
                .instruction(&I::I32Const(state.relative_offset as i32))
                .instruction(&I::I32Add)
                .instruction(&I::I32Const(0))
                .instruction(&I::I32Const(SMA_META_BYTES as i32))
                .instruction(&I::MemoryFill(0));
        }
    }

    // A fresh engine per run: `pinewasm_rt_ta_init` re-derives every site's state from
    // the declaration table, which is what makes a second `run()` on a
    // resident instance produce identical output. The lane and
    // scratch bases come from the header rather than being threaded through
    // as globals — `prepare` wrote them moments ago, and one load each per
    // run costs nothing.
    if let (Some(g), Some(fns)) = (globals.ta_base, hosts.ta_init) {
        f.instruction(&I::GlobalGet(g))
            .instruction(&I::I32Const(ta_decl_addr as i32))
            .instruction(&I::I32Const(layout.ta_decls.len() as i32))
            .instruction(&I::I32Const(0))
            .instruction(&I::I32Load(mem_u32(header::OFF_BUILTIN_BASE)))
            .instruction(&I::GlobalGet(globals.bars))
            .instruction(&I::I32Const(0))
            .instruction(&I::I32Load(mem_u32(header::OFF_SCRATCH_BASE)))
            .instruction(&I::Call(fns))
            .instruction(&I::If(BlockType::Empty))
            .instruction(&I::I32Const(status::TA_INIT_FAILED))
            .instruction(&I::Return)
            .instruction(&I::End);
    }

    // A fresh broker per run: `pinewasm_rt_broker_init` rebuilds the whole
    // emulator from the config block, which — like `pinewasm_rt_ta_init` above — is
    // what keeps a resident standalone WebAssembly instance's evaluations byte-identical. A
    // nonzero status is a malformed config block, mapped like
    // every other run failure.
    if layout.links_broker {
        let init = hosts.broker_fn(runtime::broker::INIT)?;
        f.instruction(&I::I32Const(broker_cfg_addr as i32))
            .instruction(&I::I32Const(broker_cfg_len as i32))
            .instruction(&I::Call(init))
            .instruction(&I::If(BlockType::Empty))
            .instruction(&I::I32Const(status::BROKER_FAILED))
            .instruction(&I::Return)
            .instruction(&I::End);
    }

    // A fresh heap per run — reset the bump pointer and load the
    // (loader-written) element/object budgets.
    if let Some(g) = globals.heap {
        f.instruction(&I::GlobalGet(g.base))
            .instruction(&I::GlobalSet(g.ptr));
        f.instruction(&I::I32Const(0))
            .instruction(&I::I64Load(mem_u64(header::OFF_MAX_ELEMS)))
            .instruction(&I::GlobalSet(g.elems));
        f.instruction(&I::I32Const(0))
            .instruction(&I::I64Load(mem_u64(header::OFF_MAX_OBJS)))
            .instruction(&I::GlobalSet(g.objs));
        if let Some(gc) = g.gc {
            // The interpreter's fresh-heap thresholds: an arena of
            // `GC_MIN_ARENA`, and half the element limit rounded up (at
            // least 1), kept as the remaining budget at which they meet.
            f.instruction(&I::I64Const(0))
                .instruction(&I::GlobalSet(gc.arena))
                .instruction(&I::I64Const(heap::GC_MIN_ARENA))
                .instruction(&I::GlobalSet(gc.arena_due))
                .instruction(&I::I32Const(0))
                .instruction(&I::GlobalSet(gc.epoch));
            f.instruction(&I::GlobalGet(g.elems))
                .instruction(&I::GlobalGet(g.elems))
                .instruction(&I::I64Const(1))
                .instruction(&I::I64ShrU)
                .instruction(&I::GlobalGet(g.elems))
                .instruction(&I::I64Const(1))
                .instruction(&I::I64And)
                .instruction(&I::I64Add)
                .instruction(&I::LocalTee(locals.ti0))
                .instruction(&I::I64Const(1))
                .instruction(&I::LocalGet(locals.ti0))
                .instruction(&I::I64Const(1))
                .instruction(&I::I64GtU)
                .instruction(&I::Select)
                .instruction(&I::I64Sub)
                .instruction(&I::GlobalSet(gc.elems_due));
        }
    }

    let site_of: BTreeMap<usize, u32> = layout
        .host_sites
        .iter()
        .enumerate()
        .map(|(i, s)| (s.pc as usize, i as u32))
        .collect();
    // The start of every bar: reload the per-bar counter (into `budget_set`),
    // reset the plot and color cells to na, and set every slot's current cell
    // for the bar.
    let emit_bar_entry = |f: &mut Function,
                          locals: &Locals,
                          budget_set: I<'static>|
     -> Result<(), AotError> {
        f.instruction(&I::I32Const(0))
            .instruction(&I::I64Load(mem_u64(header::OFF_BUDGET_PER_BAR)))
            .instruction(&budget_set);

        // Per-bar na-init of plot cells and color cells (conditional writes may
        // fill them later; every cell starts na, as the interpreter does).
        for plot in 0..layout.plot_count {
            f.instruction(&I::GlobalGet(globals.plot_global(plot)))
                .instruction(&I::LocalGet(locals.bar))
                .instruction(&I::I32Const(3))
                .instruction(&I::I32Shl)
                .instruction(&I::I32Add)
                .instruction(&I::F64Const(f64::NAN.into()))
                .instruction(&I::F64Store(mem_f64()));
        }
        for g in globals.color_lanes() {
            f.instruction(&I::GlobalGet(g))
                .instruction(&I::LocalGet(locals.bar))
                .instruction(&I::I32Const(2))
                .instruction(&I::I32Shl)
                .instruction(&I::I32Add)
                .instruction(&I::I32Const(-1))
                .instruction(&I::I32Store(mem_i32()));
        }

        // Per-bar slot preamble (typed): persistent slots carry the previous
        // cell forward (na at bar 0); the rest start the bar as na. Mirrors
        // `pinewasm_vm::run`'s carry-forward exactly.
        for (slot, def) in program.series_slots.iter().enumerate() {
            let slot = slot as u32;
            let ty = analysis
                .slot_ty
                .get(slot as usize)
                .copied()
                .ok_or_else(|| AotError::InvalidProgram(format!("slot {slot} is out of range")))?;
            let vt = match ty {
                VTy::F64 => ValType::F64,
                VTy::I64 => ValType::I64,
                VTy::Bool3 | VTy::Color | VTy::Str | VTy::Ref => ValType::I32,
            };
            let na = |f: &mut Function| match ty {
                VTy::F64 => {
                    f.instruction(&I::F64Const(f64::NAN.into()));
                }
                VTy::I64 => {
                    f.instruction(&I::I64Const(i64::MIN));
                }
                VTy::Bool3 => {
                    f.instruction(&I::I32Const(1));
                }
                VTy::Color | VTy::Str | VTy::Ref => {
                    f.instruction(&I::I32Const(-1));
                }
            };
            if def.persistent {
                f.instruction(&I::LocalGet(locals.bar))
                    .instruction(&I::I32Eqz)
                    .instruction(&I::If(BlockType::Result(vt)));
                na(f);
                f.instruction(&I::Else)
                    .instruction(&I::GlobalGet(globals.slot_global(slot)))
                    .instruction(&I::LocalGet(locals.bar))
                    .instruction(&I::I32Const(1))
                    .instruction(&I::I32Sub)
                    .instruction(&I::I32Const(slots.mask(slot) as i32))
                    .instruction(&I::I32And)
                    .instruction(&I::I32Const(3))
                    .instruction(&I::I32Shl)
                    .instruction(&I::I32Add);
                match ty {
                    VTy::F64 => f.instruction(&I::F64Load(mem_f64())),
                    VTy::I64 => f.instruction(&I::I64Load(mem_u64(0))),
                    VTy::Bool3 | VTy::Color | VTy::Str | VTy::Ref => {
                        f.instruction(&I::I32Load(mem_i32()))
                    }
                };
                f.instruction(&I::End);
            } else {
                na(f);
            }
            f.instruction(&locals.cur_set(slot));
            // A current-only value of a standalone or linked chart module
            // lives only in its current cell: nothing can observe its lane
            // ([`SlotLanes`]). A module that imports its host keeps every
            // lane write.
            if !slots.retains_history(slot) {
                continue;
            }
            // write-through: lane[bar & mask] = cur
            f.instruction(&I::GlobalGet(globals.slot_global(slot)))
                .instruction(&I::LocalGet(locals.bar))
                .instruction(&I::I32Const(slots.mask(slot) as i32))
                .instruction(&I::I32And)
                .instruction(&I::I32Const(3))
                .instruction(&I::I32Shl)
                .instruction(&I::I32Add)
                .instruction(&locals.cur_get(slot));
            match ty {
                VTy::F64 => f.instruction(&I::F64Store(mem_f64())),
                VTy::I64 => f.instruction(&I::I64Store(mem_u64(0))),
                VTy::Bool3 | VTy::Color | VTy::Str | VTy::Ref => {
                    f.instruction(&I::I32Store(mem_i32()))
                }
            };
        }

        // When every heap operation stays on the linked host, there are no
        // native records to trace, but transient host arrays still need the
        // same bar-boundary reclamation as the interpreter. Every Ref lane
        // in this tier names a host object; send its retained cells as roots.
        if linked_chart && analysis.heap.is_none() && layout.imports_vm {
            let vm = hosts.vm.expect("a linked host has its entry point");
            let stop = hosts.stops.expect("host calls have a stop handler").stop;
            let call = |f: &mut Function, site: u32| {
                f.instruction(&I::GlobalGet(globals.scratch_base))
                    .instruction(&I::I32Const(site as i32))
                    .instruction(&I::LocalGet(locals.bar))
                    .instruction(&I::Call(vm))
                    .instruction(&I::I32Store(mem_u32(24)))
                    .instruction(&I::GlobalGet(globals.scratch_base))
                    .instruction(&I::I32Load(mem_u32(24)))
                    .instruction(&I::If(BlockType::Empty))
                    .instruction(&I::GlobalGet(globals.scratch_base))
                    .instruction(&I::I32Load(mem_u32(24)))
                    .instruction(&I::I32Const(0))
                    .instruction(&I::LocalGet(locals.bar))
                    .instruction(&I::Call(stop))
                    .instruction(&I::Return)
                    .instruction(&I::End);
            };
            for (slot, ty) in analysis.slot_ty.iter().enumerate() {
                let slot = slot as u32;
                if *ty != VTy::Ref || !slots.retains_history(slot) {
                    continue;
                }
                // Rings can be shorter than the permitted history ceiling.
                // Retaining their occupied cells is conservative and never
                // reclaims a reference a later native instruction can read.
                for offset in 0..=slots.mask(slot) {
                    f.instruction(&I::LocalGet(locals.bar))
                        .instruction(&I::I32Const(offset as i32))
                        .instruction(&I::I32GeU)
                        .instruction(&I::If(BlockType::Empty))
                        .instruction(&I::GlobalGet(globals.scratch_base))
                        .instruction(&I::I64Const(1))
                        .instruction(&I::I64Store(mem_u64(8)))
                        .instruction(&I::GlobalGet(globals.scratch_base))
                        .instruction(&I::GlobalGet(globals.slot_global(slot)))
                        .instruction(&I::LocalGet(locals.bar))
                        .instruction(&I::I32Const(offset as i32))
                        .instruction(&I::I32Sub)
                        .instruction(&I::I32Const(slots.mask(slot) as i32))
                        .instruction(&I::I32And)
                        .instruction(&I::I32Const(3))
                        .instruction(&I::I32Shl)
                        .instruction(&I::I32Add)
                        .instruction(&I::I32Load(mem_i32()))
                        .instruction(&I::I64ExtendI32S)
                        .instruction(&I::I64Store(mem_u64(16)));
                    // na (-1) is not a root. ROOTS checks unsigned handles.
                    f.instruction(&I::GlobalGet(globals.scratch_base))
                        .instruction(&I::I64Load(mem_u64(16)))
                        .instruction(&I::I64Const(-1))
                        .instruction(&I::I64Ne)
                        .instruction(&I::If(BlockType::Empty));
                    call(f, crate::host_gc_site::ROOTS);
                    f.instruction(&I::End).instruction(&I::End);
                }
            }
            call(f, crate::host_gc_site::COLLECT_IF_DUE);
        }

        // The interpreter's collection point: just after the preamble has
        // dropped every reference the previous bar left in a non-`var`
        // cell, when the arena or the live elements have reached their
        // thresholds (`Vm::maybe_collect_heap`). Part of every bar's start,
        // and it uses no local but the bar: a failed collection has already
        // recorded its status in the header (`stop`), which is read back to
        // return it.
        if let (Some(g), Some(collect)) = (
            globals.heap,
            hosts.heap.as_ref().and_then(|fns| fns.collect),
        ) {
            let gc = g.gc.expect("a collector has its globals");
            f.instruction(&I::GlobalGet(gc.arena))
                .instruction(&I::GlobalGet(gc.arena_due))
                .instruction(&I::I64GeS)
                .instruction(&I::GlobalGet(g.elems))
                .instruction(&I::GlobalGet(gc.elems_due))
                .instruction(&I::I64LeS)
                .instruction(&I::I32Or)
                .instruction(&I::If(BlockType::Empty))
                .instruction(&I::LocalGet(locals.bar))
                .instruction(&I::Call(collect))
                .instruction(&I::If(BlockType::Empty))
                .instruction(&I::I32Const(0))
                .instruction(&I::I32Load(mem_u32(header::OFF_ERR_CODE)))
                .instruction(&I::Return)
                .instruction(&I::End)
                .instruction(&I::End);
        }
        Ok(())
    };
    let emit_bar = |f: &mut Function| -> Result<(), AotError> {
        emit_bar_entry(f, &locals, I::LocalSet(locals.budget))?;

        // Broker pre-script phase: currency sync, risk session, and
        // the 4-tick fill walk. Fills happen BEFORE the script body: an order
        // placed on bar N fills on bar N+1's walk, or on bar N's close pass
        // under `process_orders_on_close`.
        if layout.links_broker {
            let begin = hosts.broker_fn(runtime::broker::BEGIN_BAR)?;
            f.instruction(&I::LocalGet(locals.bar))
                .instruction(&I::Call(begin))
                .instruction(&I::If(BlockType::Empty))
                .instruction(&I::I32Const(status::BROKER_FAILED))
                .instruction(&I::Return)
                .instruction(&I::End);
        }

        // block $bar_done { <structured body> }
        f.instruction(&I::Block(BlockType::Empty));
        {
            let ctx = BodyCtx {
                program,
                analysis,
                layout,
                slots,
                globals,
                locals: &locals,
                hosts,
                site_of: &site_of,
                projection,
                carry,
            };
            BodyEmitter::new(&ctx, f).emit()?;
        }
        f.instruction(&I::End);

        // Broker post-script phase: the `process_orders_on_close` close pass +
        // the bar's equity sample + this bar's posture cell. After the
        // `$bar_done` End so it runs even when the body returned early.
        if layout.links_broker {
            let finish = hosts.broker_fn(runtime::broker::FINISH_BAR)?;
            f.instruction(&I::LocalGet(locals.bar))
                .instruction(&I::Call(finish))
                .instruction(&I::If(BlockType::Empty))
                .instruction(&I::I32Const(status::BROKER_FAILED))
                .instruction(&I::Return)
                .instruction(&I::End);
        }
        Ok(())
    };

    // block $exit { loop $bars { ... } }
    f.instruction(&I::Block(BlockType::Empty));
    f.instruction(&I::Loop(BlockType::Empty));

    f.instruction(&I::LocalGet(locals.bar))
        .instruction(&I::GlobalGet(globals.bars))
        .instruction(&I::I32GeU)
        .instruction(&I::BrIf(1));

    let mut piece_fns = Vec::new();
    let step = match (split, pieces) {
        (Some((step_idx, rb)), Some((plan, functions, bb))) => {
            f.instruction(&I::LocalGet(locals.bar))
                .instruction(&I::Call(step_idx))
                .instruction(&I::LocalTee(locals.t32a))
                .instruction(&I::If(BlockType::Empty))
                .instruction(&I::LocalGet(locals.t32a))
                .instruction(&I::Return)
                .instruction(&I::End);
            // `bar_step` keeps no counter and no cell in a local: it starts
            // the bar and calls the first piece, which holds the whole body
            // and returns 0 (`status::OK`) when the bar completes, or a
            // stop's status. A strategy's one scratch local holds that
            // status while the wrapper decides whether its post-script
            // broker phase may run.
            let mut g = Function::new(if layout.links_broker {
                vec![(1, ValType::I32)]
            } else {
                Vec::new()
            });
            emit_bar_entry(&mut g, &locals, I::GlobalSet(bb))?;
            if layout.links_broker {
                let begin = hosts.broker_fn(runtime::broker::BEGIN_BAR)?;
                g.instruction(&I::LocalGet(locals.bar))
                    .instruction(&I::Call(begin))
                    .instruction(&I::If(BlockType::Empty))
                    .instruction(&I::I32Const(status::BROKER_FAILED))
                    .instruction(&I::Return)
                    .instruction(&I::End);
            }
            g.instruction(&I::LocalGet(locals.bar))
                .instruction(&I::Call(functions[0]));
            if layout.links_broker {
                // A fuel/runtime stop exits the run immediately, exactly
                // like the unsplit body. A normal CSIL Return leaves only
                // the body, so it still reaches the close pass and sample.
                const PIECE_STATUS: u32 = 1;
                let finish = hosts.broker_fn(runtime::broker::FINISH_BAR)?;
                g.instruction(&I::LocalTee(PIECE_STATUS))
                    .instruction(&I::If(BlockType::Empty))
                    .instruction(&I::LocalGet(PIECE_STATUS))
                    .instruction(&I::Return)
                    .instruction(&I::End)
                    .instruction(&I::LocalGet(locals.bar))
                    .instruction(&I::Call(finish))
                    .instruction(&I::If(BlockType::Empty))
                    .instruction(&I::I32Const(status::BROKER_FAILED))
                    .instruction(&I::Return)
                    .instruction(&I::End)
                    .instruction(&I::I32Const(status::OK));
            }
            g.instruction(&I::End);
            for (index, piece) in plan.pieces.iter().enumerate() {
                piece_fns.push(emit_piece_fn(
                    &BodyCtx {
                        program,
                        analysis,
                        layout,
                        slots,
                        globals,
                        locals: &Locals::for_piece(
                            piece.max_stack,
                            &piece.hot,
                            layout.slot_count,
                            globals.count,
                        )?,
                        hosts,
                        site_of: &site_of,
                        projection,
                        carry,
                    },
                    plan,
                    index,
                    functions,
                    (bb, rb),
                )?);
            }
            Some(g)
        }
        (Some((step_idx, rb)), None) => {
            f.instruction(&I::LocalGet(locals.bar))
                .instruction(&I::Call(step_idx))
                .instruction(&I::LocalTee(locals.t32a))
                .instruction(&I::If(BlockType::Empty))
                .instruction(&I::LocalGet(locals.t32a))
                .instruction(&I::Return)
                .instruction(&I::End);
            let mut g = Function::new(step_decl);
            g.instruction(&I::GlobalGet(rb))
                .instruction(&I::LocalSet(locals.run_budget));
            emit_bar(&mut g)?;
            g.instruction(&I::LocalGet(locals.run_budget))
                .instruction(&I::GlobalSet(rb))
                .instruction(&I::I32Const(status::OK))
                .instruction(&I::End);
            Some(g)
        }
        (None, _) => {
            emit_bar(&mut f)?;
            None
        }
    };

    f.instruction(&I::LocalGet(locals.bar))
        .instruction(&I::I32Const(1))
        .instruction(&I::I32Add)
        .instruction(&I::LocalSet(locals.bar))
        .instruction(&I::Br(0));
    f.instruction(&I::End); // loop
    f.instruction(&I::End); // block $exit

    f.instruction(&I::I32Const(0))
        .instruction(&I::I32Const(status::OK))
        .instruction(&I::I32Store(mem_u32(header::OFF_ERR_CODE)));
    f.instruction(&I::I32Const(status::OK))
        .instruction(&I::Return)
        .instruction(&I::End);
    Ok((f, step, piece_fns))
}

/// One piece of a split body (`body/split.rs`): `(bar) -> 0 or status`.
/// Its locals are `bar_step`'s with the piece's own stack depths and cached
/// cells, then a callee's result. It takes both counters and its cached
/// cells from their globals on entry; leaving through its exit puts them
/// back and returns 0. A stop's status returns at once, as it did from the
/// unsplit body.
fn emit_piece_fn(
    ctx: &BodyCtx<'_>,
    plan: &crate::body::split::SplitPlan,
    index: usize,
    functions: &[u32],
    (bb, rb): (u32, u32),
) -> Result<Function, AotError> {
    let piece = &plan.pieces[index];
    let locals = ctx.locals;
    let mut decl: Vec<(u32, ValType)> = vec![
        (2, ValType::I64), // per-bar budget, cumulative run budget
        (3, ValType::F64), // tf0..tf2
        (4, ValType::I64), // ti0..ti3
        (3, ValType::I32), // t32a, t32b, t32c
        (1, ValType::I32), // wm_i
        (1, ValType::F64), // wm_sum
    ];
    for _ in 0..piece.max_stack {
        decl.push((1, ValType::F64));
        decl.push((1, ValType::I64));
        decl.push((1, ValType::I32));
        decl.push((1, ValType::I32)); // independent integer validity
    }
    for &slot in &piece.hot {
        let ty = ctx
            .analysis
            .slot_ty
            .get(slot as usize)
            .copied()
            .ok_or_else(|| AotError::InvalidProgram(format!("slot {slot} has no analyzed type")))?;
        decl.push((
            1,
            match ty {
                VTy::F64 => ValType::F64,
                VTy::I64 => ValType::I64,
                VTy::Bool3 | VTy::Color | VTy::Str | VTy::Ref => ValType::I32,
            },
        ));
    }
    decl.push((1, ValType::I32)); // a callee's result
    let status_local = locals.after_cur(piece.hot.len());
    let mut g = Function::new(decl);
    g.instruction(&I::GlobalGet(bb))
        .instruction(&I::LocalSet(locals.budget))
        .instruction(&I::GlobalGet(rb))
        .instruction(&I::LocalSet(locals.run_budget));
    for &slot in &piece.hot {
        g.instruction(&I::GlobalGet(locals.home(slot)))
            .instruction(&locals.cur_set(slot));
    }
    g.instruction(&I::Block(BlockType::Empty));
    BodyEmitter::new(ctx, &mut g).emit_piece(&crate::body::split::PieceEnv {
        plan,
        piece: index,
        functions,
        status_local,
        budget_global: bb,
        run_budget_global: rb,
    })?;
    g.instruction(&I::End);
    g.instruction(&I::LocalGet(locals.budget))
        .instruction(&I::GlobalSet(bb))
        .instruction(&I::LocalGet(locals.run_budget))
        .instruction(&I::GlobalSet(rb));
    for (&slot, &written) in piece.hot.iter().zip(&piece.hot_written) {
        if written {
            g.instruction(&locals.cur_get(slot))
                .instruction(&I::GlobalSet(locals.home(slot)));
        }
    }
    g.instruction(&I::I32Const(crate::body::split::EXIT))
        .instruction(&I::End);
    Ok(g)
}

/// `evaluate_indicator_cabi(ptr, len) -> u64`: the standalone WebAssembly entry point's
/// emitted glue, sequencing the blob's standalone WebAssembly family around
/// the artifact's internal `prepare`/`run`:
///
/// 1. `standalone_wasm_scan` validates the input JSON, extracts `slice_end`
///    (`max(1, floor(bars × fraction))`), and doubles as the per-call
///    prologue: it clears the error slot and drops any leaked allocation
///    above the live input, which is what keeps a resident instance from
///    growing monotonically.
/// 2. The bars guard `prepare` would apply, applied early because step 3's
///    bound arithmetic multiplies by `slice_end`.
/// 3. `standalone_wasm_arena_reloc` moves the arena band — holding only the live input
///    JSON — above a conservative bound on the run layout's `heap_end`, so
///    neither `prepare` (whose written-flags fill lands mid-layout) nor
///    `run` can touch the JSON before it is consumed.
/// 4. `memory.init` re-materializes the key table at `reserved_hi`, in the
///    gap the raised lane floor keeps clear; a previous call's input may
///    have overwritten it, which is why the segment is passive.
/// 5. `prepare(slice_end)`; non-OK statuses map through
///    `standalone_wasm_set_status_error` to the documented error classes and a 0 return.
/// 6. `standalone_wasm_fill_lanes` writes the input table (defaults, then wire
///    `params`) and the builtin/source lanes from the JSON.
/// 7. The consumed input is dropped (LIFO dealloc), then `run()`.
/// 8. Native-heap modules relocate the (now empty) band once more, above
///    the post-run heap pointer, so the output cannot land under heap
///    allocations.
/// 9. `standalone_wasm_serialize` edge-detects the designated plot into the OUT
///    document and returns the packed `(ptr << 32) | len` — or, for an
///    indicator-type module, serializes the always-empty series.
// One parameter per thing the sequence has to name: the two functions it
// calls, the segment it materializes, and its length. Grouping them into a
// struct would only rename them.
#[allow(clippy::too_many_arguments)]
fn emit_standalone_wasm_evaluate(
    layout: &Layout,
    globals: &Globals,
    rt: &runtime::Runtime,
    spec: &StandaloneWasmSpec,
    prepare_idx: u32,
    run_idx: u32,
    tables_segment: u32,
    tables_len: u32,
    max_bars: u32,
    relocation_bound: StandaloneRelocationBound,
) -> Function {
    use crate::runtime::standalone_wasm;
    // standalone WebAssembly refuses imports, so the blob's indices are unshifted.
    let blob = |id: u32| I::Call(rt.standalone_wasm_entry(id, 0));

    // Params 0 = ptr, 1 = len.
    let slice_end = 2u32;
    let tmp = 3u32;
    let jsonp = 4u32;
    let scan64 = 5u32;
    let mut f = Function::new([(3, ValType::I32), (1, ValType::I64)]);
    let fail = |f: &mut Function| {
        f.instruction(&I::I64Const(0)).instruction(&I::Return);
    };

    // slice_end = standalone_wasm_scan(ptr, len), negative = error already set.
    f.instruction(&I::LocalGet(0))
        .instruction(&I::LocalGet(1))
        .instruction(&blob(standalone_wasm::SCAN))
        .instruction(&I::LocalTee(scan64))
        .instruction(&I::I64Const(0))
        .instruction(&I::I64LtS)
        .instruction(&I::If(BlockType::Empty));
    fail(&mut f);
    f.instruction(&I::End)
        .instruction(&I::LocalGet(scan64))
        .instruction(&I::I32WrapI64)
        .instruction(&I::LocalSet(slice_end));

    // Match `prepare`'s per-module memory bound before multiplying the bar
    // count into the relocation target.
    f.instruction(&I::LocalGet(slice_end))
        .instruction(&I::I32Const(max_bars as i32))
        .instruction(&I::I32GtU)
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::I32Const(status::TOO_MANY_BARS))
        .instruction(&blob(standalone_wasm::SET_STATUS_ERROR));
    fail(&mut f);
    f.instruction(&I::End);

    // Erased drawing constructors are certified not to fail only up to a
    // per-artifact bar bound (`projection::certify`); a longer request is a
    // request error, before any work. Failing closed here is what keeps the
    // erasure sound.
    if let Some(limit) = spec.projection.max_bars {
        f.instruction(&I::LocalGet(slice_end))
            .instruction(&I::I32Const(limit as i32))
            .instruction(&I::I32GtU)
            .instruction(&I::If(BlockType::Empty))
            .instruction(&I::LocalGet(slice_end))
            .instruction(&I::I32Const(limit as i32))
            .instruction(&blob(standalone_wasm::SET_BAR_BOUND_ERROR));
        fail(&mut f);
        f.instruction(&I::End);
    }

    // Exact checked coefficients are shared with the emitter's admission
    // calculation. The relocation bound includes the fixed regions, both
    // alignment slacks, and the input-arena pad.
    f.instruction(&I::LocalGet(slice_end))
        .instruction(&I::I32Const(relocation_bound.per_bar as i32))
        .instruction(&I::I32Mul)
        .instruction(&I::I32Const(relocation_bound.fixed as i32))
        .instruction(&I::I32Add)
        .instruction(&blob(standalone_wasm::ARENA_RELOC))
        .instruction(&I::LocalTee(tmp))
        .instruction(&I::I32Const(0))
        .instruction(&I::I32LtS)
        .instruction(&I::If(BlockType::Empty));
    fail(&mut f);
    f.instruction(&I::End)
        .instruction(&I::LocalGet(0))
        .instruction(&I::LocalGet(tmp))
        .instruction(&I::I32Add)
        .instruction(&I::LocalSet(jsonp));

    // Static tables (key table, then the ta declarations) into their slot
    // under the lane floor.
    f.instruction(&I::I32Const(rt.reserved_hi() as i32))
        .instruction(&I::I32Const(0))
        .instruction(&I::I32Const(tables_len as i32))
        .instruction(&I::MemoryInit {
            mem: 0,
            data_index: tables_segment,
        });

    // prepare(slice_end) — non-OK statuses map to the standalone WebAssembly error classes.
    let status_gate = |f: &mut Function, tmp: u32| {
        f.instruction(&I::LocalTee(tmp))
            .instruction(&I::If(BlockType::Empty))
            .instruction(&I::LocalGet(tmp))
            .instruction(&blob(standalone_wasm::SET_STATUS_ERROR))
            .instruction(&I::I64Const(0))
            .instruction(&I::Return)
            .instruction(&I::End);
    };
    f.instruction(&I::LocalGet(slice_end))
        .instruction(&I::Call(prepare_idx));
    status_gate(&mut f, tmp);

    // standalone_wasm_fill_lanes(json', len, slice_end, keytab, keytab_len).
    f.instruction(&I::LocalGet(jsonp))
        .instruction(&I::LocalGet(1))
        .instruction(&I::LocalGet(slice_end))
        .instruction(&I::I32Const(rt.reserved_hi() as i32))
        .instruction(&I::I32Const(spec.keytab.len() as i32))
        .instruction(&blob(standalone_wasm::FILL_LANES))
        .instruction(&I::If(BlockType::Empty));
    fail(&mut f);
    f.instruction(&I::End);

    // standalone_wasm_fill_timeframe(json', len, slice_end, first_cell), in a
    // module whose program reads the chart timeframe: the request must name
    // it.
    if spec.projection.reads_timeframe() {
        f.instruction(&I::LocalGet(jsonp))
            .instruction(&I::LocalGet(1))
            .instruction(&I::LocalGet(slice_end))
            .instruction(&I::I32Const(spec.projection.timeframe_cells as i32))
            .instruction(&blob(standalone_wasm::FILL_TIMEFRAME))
            .instruction(&I::If(BlockType::Empty));
        fail(&mut f);
        f.instruction(&I::End);
    }

    // The JSON is consumed: LIFO-drop it so the output starts at the band
    // base. The host's own dealloc of the original pointer after return
    // degrades to a no-op either way.
    f.instruction(&I::LocalGet(jsonp))
        .instruction(&I::LocalGet(1))
        .instruction(&blob(standalone_wasm::DEALLOC));

    f.instruction(&I::Call(run_idx));
    if spec.strategy_source {
        // `BROKER_FAILED` carries its own precise message in the error slot
        // (the blob wrote it before returning); routing it through
        // `standalone_wasm_set_status_error` would clobber that with a generic one.
        // Every other status maps exactly as before.
        f.instruction(&I::LocalTee(tmp))
            .instruction(&I::If(BlockType::Empty))
            .instruction(&I::LocalGet(tmp))
            .instruction(&I::I32Const(status::BROKER_FAILED))
            .instruction(&I::I32Ne)
            .instruction(&I::If(BlockType::Empty))
            .instruction(&I::LocalGet(tmp))
            .instruction(&blob(standalone_wasm::SET_STATUS_ERROR))
            .instruction(&I::End)
            .instruction(&I::I64Const(0))
            .instruction(&I::Return)
            .instruction(&I::End);
    } else {
        status_gate(&mut f, tmp);
    }

    // Heap growth during run may pass the pre-run bound; hop the
    // (now empty) band above the final heap pointer before serializing. A
    // strategy's heap lives in the band itself, beside the broker state the
    // serializer is about to read, so there is nothing to hop over.
    if let Some(g) = globals.heap.filter(|_| !spec.strategy_source) {
        f.instruction(&I::GlobalGet(g.ptr))
            .instruction(&I::I32Const(STANDALONE_WASM_ARENA_PAD as i32))
            .instruction(&I::I32Add)
            .instruction(&blob(standalone_wasm::ARENA_RELOC))
            .instruction(&I::I32Const(0))
            .instruction(&I::I32LtS)
            .instruction(&I::If(BlockType::Empty));
        fail(&mut f);
        f.instruction(&I::End);
    }

    // Serialize. A strategy()-sourced module's signals come from the linked
    // broker's posture lane under the `posture-events` semantics;
    // a plot-designated module edge-detects its plot according to its
    // declared position/posture semantics; an indicator-type
    // module serializes the always-empty window.
    if spec.strategy_source {
        f.instruction(&I::LocalGet(slice_end))
            .instruction(&I::I32Const(layout.plot_count as i32))
            .instruction(&I::Call(
                rt.broker_entry(crate::runtime::broker::SERIALIZE_POSTURE, 0),
            ))
            .instruction(&I::End);
        return f;
    }
    match spec.signal_plot {
        Some(plot) => {
            f.instruction(&I::I32Const(plot as i32))
                .instruction(&I::LocalGet(slice_end))
                .instruction(&I::I32Const(i32::from(matches!(
                    spec.signal_semantics,
                    crate::StandaloneWasmSignalSemantics::PostureEvents
                ))));
        }
        None => {
            f.instruction(&I::I32Const(0))
                .instruction(&I::I32Const(0))
                .instruction(&I::I32Const(0));
        }
    }
    f.instruction(&blob(standalone_wasm::SERIALIZE))
        .instruction(&I::End);
    f
}

/// Serialize the strategy declaration + the program's string const pool
/// into the broker config block `pinewasm_rt_broker_init` decodes (the byte layout
/// is defined on `runtime/src/broker.rs`, the blob side; this is the
/// emitter half of that contract).
fn broker_config_bytes(program: &CsilProgram) -> Vec<u8> {
    let info = program
        .metadata
        .strategy
        .as_ref()
        .expect("broker config emitted for strategy programs only");
    let mut out = Vec::new();
    out.extend_from_slice(&1u32.to_le_bytes()); // layout version
    let mut flags = 0u32;
    if info.process_orders_on_close {
        flags |= 1;
    }
    if info.calc_on_every_tick {
        flags |= 2;
    }
    // Upper three bytes pin an explicit account currency. The runtime admits
    // only an identical symbol quote currency, so no FX approximation occurs.
    if info.currency.len() == 3 && info.currency != "NONE" {
        for (index, byte) in info.currency.bytes().enumerate() {
            flags |= u32::from(byte) << (8 * (index + 1));
        }
    }
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&info.initial_capital.to_le_bytes());
    out.extend_from_slice(&info.default_qty_value.to_le_bytes());
    out.extend_from_slice(&info.commission_value.to_le_bytes());
    out.extend_from_slice(&info.margin_long.to_le_bytes());
    out.extend_from_slice(&info.margin_short.to_le_bytes());
    out.extend_from_slice(&info.slippage.to_le_bytes());
    out.extend_from_slice(&info.pyramiding.to_le_bytes());
    out.extend_from_slice(&info.backtest_fill_limits_assumption.to_le_bytes());
    out.push(match info.default_qty_type.as_str() {
        "cash" => 1,
        "percent_of_equity" => 2,
        _ => 0,
    });
    out.push(match info.commission_type.as_str() {
        "cash_per_contract" => 1,
        "cash_per_order" => 2,
        _ => 0,
    });
    out.push(u8::from(info.close_entries_rule == "ANY"));
    out.push(0);
    // The string const pool: `CsilInstr::Order`'s id/comment/token operands
    // are `PushStr` handles, and the blob resolves them against exactly
    // this table.
    let n = program.constants.len() as u32;
    out.extend_from_slice(&n.to_le_bytes());
    let mut offset = 68 + 8 * n;
    for constant in &program.constants {
        out.extend_from_slice(&offset.to_le_bytes());
        out.extend_from_slice(&(constant.len() as u32).to_le_bytes());
        offset += constant.len() as u32;
    }
    for constant in &program.constants {
        out.extend_from_slice(constant.as_bytes());
    }
    out
}

/// The `strategy` member of a strategy-source module's capabilities: the
/// account settings its linked broker runs with, in the declaration's own
/// units. A standalone WebAssembly request cannot override them, so a host
/// can pin or disclose a module's trading assumptions before it evaluates
/// one, and hold the account's `initialCapital` and
/// `slippageTicks` to these. Every value is read as [`broker_config_bytes`]
/// encodes it, so this describes the config block the module carries.
fn broker_capabilities_json(program: &CsilProgram) -> String {
    let info = program
        .metadata
        .strategy
        .as_ref()
        .expect("broker capabilities emitted for strategy programs only");
    let mut out = String::from(",\"strategy\":{\"initial_capital\":");
    crate::schema::write_f64(&mut out, info.initial_capital);
    out.push_str(&format!(",\"slippage_ticks\":{}", info.slippage));
    out.push_str(",\"commission_type\":");
    crate::schema::write_json_string(
        &mut out,
        match info.commission_type.as_str() {
            kind @ ("cash_per_contract" | "cash_per_order") => kind,
            _ => "percent",
        },
    );
    out.push_str(",\"commission_value\":");
    crate::schema::write_f64(&mut out, info.commission_value);
    // Present only for an explicit account currency, which the runtime
    // admits against an identical request quote currency and nothing else.
    if info.currency.len() == 3 && info.currency != "NONE" {
        out.push_str(",\"account_currency\":");
        crate::schema::write_json_string(&mut out, &info.currency);
    }
    out.push('}');
    out
}

/// Read the engine rule without widening the supported compiler facade.
pub(crate) fn ta_input_rule(
    program: &CsilProgram,
    binding: &pinewasm_hir::TaInputBinding,
) -> Option<pinewasm_hir::TaParamRule> {
    let pinewasm_hir::TaInputTarget::State { state, param } = binding.target else {
        return None;
    };
    program
        .ta_states
        .get(state as usize)?
        .func
        .engine_param_rules()
        .get(param as usize)
        .copied()
}

/// Push a run-constant TA parameter from the host-populated input table.
pub(crate) fn emit_ta_input_value(
    f: &mut Function,
    binding: &pinewasm_hir::TaInputBinding,
    rule: Option<pinewasm_hir::TaParamRule>,
) {
    let float_rule = matches!(
        rule,
        Some(pinewasm_hir::TaParamRule::FloatBits { .. } | pinewasm_hir::TaParamRule::Percent)
    );
    if binding.negated && !float_rule {
        f.instruction(&I::I64Const(0));
    }
    f.instruction(&I::I32Const(0))
        .instruction(&I::I64Load(mem_u64(header::SIZE + binding.input * 8)));
    if binding.negated {
        if float_rule {
            f.instruction(&I::F64ReinterpretI64)
                .instruction(&I::F64Neg)
                .instruction(&I::I64ReinterpretF64);
        } else {
            f.instruction(&I::I64Sub);
        }
    }
}

#[cfg(test)]
mod slot_write_tests {
    use super::*;

    #[test]
    fn integer_validity_local_indices_refuse_overflow() {
        let none = CurCells {
            cells: Vec::new(),
            local_tys: Vec::new(),
            global_tys: Vec::new(),
        };
        assert!(matches!(
            Locals::new(u32::MAX, &none, 0),
            Err(AotError::UnsupportedOpcode(_))
        ));
        assert!(matches!(
            Locals::new(u32::MAX / 4, &none, 0),
            Err(AotError::UnsupportedOpcode(_))
        ));
    }

    fn history_fixture() -> (CsilProgram, Layout) {
        let program = pinewasm_compiler::compile(
            "indicator('History layout')\nx = close\nplot(x[1])\n",
            pinewasm_compiler::CompileOptions::default(),
        )
        .program
        .unwrap();
        let analysis = crate::analysis::analyze(&program).unwrap();
        let layout = crate::plan_layout(&program, &analysis);
        (program, layout)
    }

    /// Test-only copy of the `slot_rings` implementation, kept unchanged so
    /// that `slot_rings` is checked against it: the returned vectors and
    /// errors must match.
    fn slot_rings_reference(
        program: &CsilProgram,
        slot_count: usize,
    ) -> Result<Vec<u32>, AotError> {
        let mut bound_depths = vec![0u32; slot_count];
        for binding in &program.ta_input_bindings {
            let Some((_, max)) = program.ta_input_bounds(binding) else {
                continue;
            };
            let (slot, depth) = match binding.target {
                pinewasm_hir::TaInputTarget::Sma { slot } => (slot, max as u32 - 1),
                pinewasm_hir::TaInputTarget::Change { slot } => (slot, max as u32),
                pinewasm_hir::TaInputTarget::State { .. } => continue,
            };
            bound_depths[slot as usize] = bound_depths[slot as usize].max(depth);
        }
        let mut own_depths: Vec<u32> = program
            .series_slots
            .iter()
            .map(|slot| u32::from(slot.persistent || slot.varip))
            .collect();
        for instruction in program.functions.iter().flat_map(|function| &function.code) {
            let (slot, depth) = match instruction {
                CsilInstr::ReadUserSlot { slot, offset } => (*slot as usize, *offset),
                CsilInstr::ReadUserSlotDynamic { slot } => {
                    (*slot as usize, program.metadata.max_bars_back)
                }
                CsilInstr::WindowMean { slot, length } => {
                    (*slot as usize, length.saturating_sub(1))
                }
                CsilInstr::WindowMeanDynamic { slot }
                | CsilInstr::WindowExtremumDynamic { slot, .. }
                | CsilInstr::WindowStatisticDynamic { slot, .. } => {
                    (*slot as usize, pinewasm_csil::MAX_WINDOW_LENGTH - 1)
                }
                _ => continue,
            };
            own_depths[slot] = own_depths[slot].max(depth);
        }
        bound_depths
            .into_iter()
            .zip(own_depths)
            .map(|(bound, own)| {
                bound
                    .max(own)
                    .checked_add(1)
                    .and_then(u32::checked_next_power_of_two)
                    .ok_or_else(|| {
                        AotError::UnsupportedOpcode(
                            "slot history ring exceeds the wasm32 address space".into(),
                        )
                    })
            })
            .collect()
    }

    fn assert_slot_rings_match(program: &CsilProgram, slot_count: usize) {
        let actual = slot_rings(program, slot_count);
        let reference = slot_rings_reference(program, slot_count);
        assert_eq!(actual, reference);
    }

    #[test]
    fn slot_rings_matches_no_window_and_duplicate_window_uses() {
        let mut program = pinewasm_compiler::compile(
            "indicator('Ring differential')\nx = close\nplot(x)\n",
            pinewasm_compiler::CompileOptions::default(),
        )
        .program
        .unwrap();
        let slot_count = program.series_slots.len();
        assert!(slot_count > 0);

        let no_window = slot_rings(&program, slot_count).unwrap();
        assert_slot_rings_match(&program, slot_count);
        assert!(no_window.iter().all(|ring| *ring == 1), "{no_window:?}");

        program.functions[0]
            .code
            .push(CsilInstr::WindowMean { slot: 0, length: 4 });
        program.functions[0]
            .code
            .push(CsilInstr::WindowMean { slot: 0, length: 8 });
        assert_slot_rings_match(&program, slot_count);
        assert_eq!(slot_rings(&program, slot_count).unwrap()[0], 8);

        program.functions[0]
            .code
            .push(CsilInstr::WindowMeanDynamic { slot: 0 });
        program.functions[0]
            .code
            .push(CsilInstr::WindowMeanDynamic { slot: 0 });
        assert_slot_rings_match(&program, slot_count);
        assert_eq!(
            slot_rings(&program, slot_count).unwrap()[0],
            pinewasm_csil::MAX_WINDOW_LENGTH.next_power_of_two()
        );
    }

    #[test]
    fn slot_rings_matches_sma_and_change_input_bounds() {
        let program = pinewasm_compiler::compile(
            concat!(
                "indicator('TA input ring bounds')\nlength = input.int(20, 'Length')\n",
                "mean = ta.sma(close, length)\nchange = ta.change(close, length)\n",
                "plot(mean)\nplot(change)\n",
            ),
            pinewasm_compiler::CompileOptions::default(),
        )
        .program
        .unwrap();
        assert!(program
            .ta_input_bindings
            .iter()
            .any(|binding| matches!(binding.target, pinewasm_hir::TaInputTarget::Sma { .. })));
        assert!(program
            .ta_input_bindings
            .iter()
            .any(|binding| matches!(binding.target, pinewasm_hir::TaInputTarget::Change { .. })));
        assert_slot_rings_match(&program, program.series_slots.len());
    }

    #[test]
    fn slot_rings_matches_zero_and_power_of_two_depth_boundaries() {
        let base = pinewasm_compiler::compile(
            "indicator('Ring depth boundaries')\nx = close\nplot(x)\n",
            pinewasm_compiler::CompileOptions::default(),
        )
        .program
        .unwrap();
        let expected = [
            (0, 1),
            (1, 2),
            (2, 4),
            (3, 4),
            (7, 8),
            (8, 16),
            (15, 16),
            (16, 32),
        ];
        for (depth, ring) in expected {
            let mut program = base.clone();
            program.metadata.max_bars_back = depth;
            program.functions[0]
                .code
                .push(CsilInstr::ReadUserSlotDynamic { slot: 0 });
            assert_slot_rings_match(&program, program.series_slots.len());
            assert_eq!(
                slot_rings(&program, program.series_slots.len()).unwrap()[0],
                ring
            );
        }
    }

    #[test]
    fn slot_rings_matches_mismatched_slot_count_truncation() {
        let program = pinewasm_compiler::compile(
            "indicator('Ring slot-count truncation')\nx = close\ny = open\nplot(x + y)\n",
            pinewasm_compiler::CompileOptions::default(),
        )
        .program
        .unwrap();
        let slot_count = program.series_slots.len();
        assert!(slot_count > 0);

        let short = slot_count - 1;
        assert_slot_rings_match(&program, short);
        assert_eq!(slot_rings(&program, short).unwrap().len(), short);

        let long = slot_count + 1;
        assert_slot_rings_match(&program, long);
        assert_eq!(slot_rings(&program, long).unwrap().len(), slot_count);
    }

    #[test]
    fn slot_rings_matches_overflow_error_class_and_message() {
        let base = pinewasm_compiler::compile(
            "indicator('Ring overflow')\nx = close\nplot(x)\n",
            pinewasm_compiler::CompileOptions::default(),
        )
        .program
        .unwrap();
        for depth in [1 << 31, u32::MAX] {
            let mut program = base.clone();
            program.metadata.max_bars_back = depth;
            program.functions[0]
                .code
                .push(CsilInstr::ReadUserSlotDynamic { slot: 0 });
            assert_slot_rings_match(&program, program.series_slots.len());
            let error = slot_rings(&program, program.series_slots.len()).unwrap_err();
            assert!(matches!(
                error,
                AotError::UnsupportedOpcode(message)
                    if message == "slot history ring exceeds the wasm32 address space"
            ));
            if depth == u32::MAX {
                // Negative mutation check: replacing the checked increment
                // with wrapping arithmetic silently produces a one-cell ring.
                let wrapped = depth.wrapping_add(1);
                let mutated_capacity = wrapped.checked_next_power_of_two().unwrap();
                assert_eq!(wrapped, 0);
                assert_eq!(mutated_capacity, 1);
                assert!(mutated_capacity <= depth);
            }
        }
    }

    #[test]
    fn history_ceiling_does_not_allocate_unused_temporary_history() {
        let mut program = pinewasm_compiler::compile(
            "indicator('History layout', max_bars_back=5000)\nx=close\nvar carried=1.0\ncarried:=carried+1\nplot(x[2]+carried)\n",
            pinewasm_compiler::CompileOptions::default(),
        ).program.unwrap();
        let rings = slot_rings(&program, program.series_slots.len()).unwrap();
        assert!(rings.iter().all(|ring| *ring <= 4), "{rings:?}");
        program.functions[0]
            .code
            .push(CsilInstr::ReadUserSlotDynamic { slot: 0 });
        let rings = slot_rings(&program, program.series_slots.len()).unwrap();
        assert_eq!(rings[0], 8192);
        assert!(rings[1..].iter().all(|ring| *ring <= 4));
    }

    #[test]
    fn history_layout_refuses_depth_and_power_of_two_overflow() {
        let (mut program, layout) = history_fixture();
        program.functions[0]
            .code
            .push(CsilInstr::ReadUserSlotDynamic { slot: 0 });
        // Internal planner defense: malformed/unvalidated metadata must not
        // panic even though the public CSIL validator rejects these depths.
        for depth in [u32::MAX, 1 << 31] {
            program.metadata.max_bars_back = depth;
            assert!(matches!(
                SlotLayout::plan(&program, &layout, SlotLanes::Standalone),
                Err(AotError::UnsupportedOpcode(_))
            ));
        }
    }

    #[test]
    fn history_layout_refuses_cell_stride_overflow() {
        let (mut program, layout) = history_fixture();
        program.functions[0]
            .code
            .push(CsilInstr::ReadUserSlotDynamic { slot: 0 });
        // One lane alone exceeds wasm32; check multiplication before the
        // aggregate addition so it cannot wrap to a zero-byte allocation.
        program.metadata.max_bars_back = (1 << 29) - 1;
        let styles = BTreeSet::new();
        for lanes in [
            SlotLanes::All,
            SlotLanes::Standalone,
            SlotLanes::Linked { styles: &styles },
        ] {
            assert!(matches!(
                SlotLayout::plan(&program, &layout, lanes),
                Err(AotError::UnsupportedOpcode(_))
            ));
        }
    }

    #[test]
    fn an_adjustable_average_widens_only_its_own_ring_on_both_tiers() {
        let program = pinewasm_compiler::compile(
            concat!(
                "indicator('Adjustable average')\nn = input.int(20, 'Length')\n",
                "x = close * 2.0\ny = x + 1.0\nplot(ta.sma(close, n))\nplot(y[3])\n",
            ),
            pinewasm_compiler::CompileOptions::default(),
        )
        .program
        .unwrap();
        let analysis = crate::analysis::analyze(&program).unwrap();
        let layout = crate::plan_layout(&program, &analysis);
        let (averaged, widest) = program
            .ta_input_bindings
            .iter()
            .find_map(|binding| match binding.target {
                pinewasm_hir::TaInputTarget::Sma { slot } => {
                    let (_, max) = program.ta_input_bounds(binding)?;
                    Some((slot, (max as u32).next_power_of_two()))
                }
                _ => None,
            })
            .expect("the length input is bound to the average");
        let ordinary = 4;
        assert!(ordinary < widest);
        // The header still publishes the deepest ring.
        assert_eq!(layout.ring, widest);
        let styles = BTreeSet::new();
        for lanes in [
            SlotLanes::All,
            SlotLanes::Standalone,
            SlotLanes::Linked { styles: &styles },
        ] {
            let slots = SlotLayout::plan(&program, &layout, lanes).unwrap();
            // The average reads its slot's lane in every plan.
            assert!(slots.retains_history(averaged));
            for slot in 0..layout.slot_count {
                let expected = if slot == averaged {
                    widest
                } else if program.series_slots[slot as usize].name == "y" {
                    ordinary
                } else {
                    1
                };
                assert_eq!(slots.mask(slot) + 1, expected, "slot {slot}");
            }
            assert!(u64::from(slots.bytes) < u64::from(layout.slot_count) * u64::from(widest) * 8);
        }
    }

    #[test]
    fn standalone_relocation_bound_caps_the_preflight_before_arena_target_wrap() {
        let footprint = LayoutFootprint {
            base: 96,
            builtin_bytes_per_bar: 48,
            plot_bytes_per_bar: 8,
            color_bytes_per_bar: 0,
            alert_bytes_per_bar: 0,
            slot_bytes: 0,
            slot_count: 0,
            scratch_bytes: 32,
            ta_bytes: 0,
        };
        let relocation = footprint
            .standalone_relocation()
            .expect("fixture relocation coefficients fit wasm32");
        let maximum = footprint
            .max_bars(Some(relocation))
            .expect("fixed layout fits declared memory");
        let memory_limit = MEMORY_MAX_PAGES * 65_536;
        let relocation_end = |bars: u32| {
            u64::from(relocation.fixed) + u64::from(relocation.per_bar) * u64::from(bars)
        };

        assert_eq!(maximum, 4_793_486);
        assert!(footprint.heap_end(u64::from(maximum)).unwrap() <= memory_limit);
        assert!(relocation_end(maximum) <= memory_limit);
        assert!(relocation_end(maximum + 1) > memory_limit);
    }

    #[test]
    fn standalone_wasm_omits_only_unobserved_lanes_and_universal_shares_its_rings() {
        let program = pinewasm_compiler::compile(
            concat!(
                "indicator('Slot write plan')\nx = close\ny = x * 2.0\n",
                "var float held = na\nheld := close\nplot(y)\nplot(x[2])\n",
            ),
            pinewasm_compiler::CompileOptions::default(),
        )
        .program
        .unwrap();
        let analysis = crate::analysis::analyze(&program).unwrap();
        let layout = crate::plan_layout(&program, &analysis);
        let standalone_wasm = SlotLayout::plan(&program, &layout, SlotLanes::Standalone).unwrap();
        let universal = SlotLayout::plan(&program, &layout, SlotLanes::All).unwrap();
        let slot = |name| {
            program
                .series_slots
                .iter()
                .position(|slot| slot.name == name)
                .unwrap() as u32
        };
        assert!(standalone_wasm.retains_history(slot("x")));
        assert!(!standalone_wasm.retains_history(slot("y")));
        assert!(!standalone_wasm.observes_written(slot("x")));
        assert!(!standalone_wasm.observes_written(slot("y")));
        assert!(standalone_wasm.retains_history(slot("held")));
        assert!(standalone_wasm.observes_written(slot("held")));
        assert!(universal.history_observed.iter().all(|observed| *observed));
        assert!(universal.written_observed.iter().all(|observed| *observed));
        assert_eq!(standalone_wasm.offsets, universal.offsets);
        assert_eq!(standalone_wasm.bytes, universal.bytes);
    }

    /// A linked chart module keeps a lane only for what reads one: a slot
    /// carried forward, a history read, and a slot its host reads for a
    /// display style. Its rings and offsets are the universal plan's.
    #[test]
    fn a_linked_chart_module_keeps_the_lanes_its_code_and_its_styles_read() {
        let program = pinewasm_compiler::compile(
            concat!(
                "indicator('Linked slot plan')\nn = input.int(3)\nx = close\ny = x * 2.0\n",
                "var float held = na\nheld := close\nwidth = int(math.pow(n, 2))\n",
                "plot(y, linewidth = width)\nplot(x[2])\n",
            ),
            pinewasm_compiler::CompileOptions::default(),
        )
        .program
        .unwrap();
        let analysis = crate::analysis::analyze(&program).unwrap();
        let layout = crate::plan_layout(&program, &analysis);
        let styles = style_slot_ids(&program);
        assert_eq!(styles.len(), 1, "the line width is one style slot");
        let style = *styles.iter().next().unwrap();
        let universal = SlotLayout::plan(&program, &layout, SlotLanes::All).unwrap();
        let standalone = SlotLayout::plan(&program, &layout, SlotLanes::Standalone).unwrap();
        let linked =
            SlotLayout::plan(&program, &layout, SlotLanes::Linked { styles: &styles }).unwrap();
        let slot = |name| {
            program
                .series_slots
                .iter()
                .position(|slot| slot.name == name)
                .unwrap() as u32
        };
        assert!(linked.retains_history(slot("x")), "read two bars back");
        assert!(
            !linked.retains_history(slot("y")),
            "read on its own bar only"
        );
        assert!(linked.retains_history(slot("held")), "carried forward");
        assert!(linked.observes_written(slot("held")));
        assert!(!linked.observes_written(slot("y")));
        // The host reads the style's slot, whatever else does. Every other
        // slot is kept exactly when standalone WebAssembly keeps it.
        assert!(linked.retains_history(style));
        // The style clause alone keeps a slot nothing else reads back.
        let only_a_style = BTreeSet::from([slot("y")]);
        let styled = SlotLayout::plan(
            &program,
            &layout,
            SlotLanes::Linked {
                styles: &only_a_style,
            },
        )
        .unwrap();
        assert!(styled.retains_history(slot("y")));
        assert!(!standalone.retains_history(slot("y")));
        for other in 0..layout.slot_count {
            if other != style {
                assert_eq!(
                    linked.retains_history(other),
                    standalone.retains_history(other),
                    "slot {other}"
                );
            }
            assert_eq!(
                linked.observes_written(other),
                standalone.observes_written(other),
                "slot {other}"
            );
        }
        assert_eq!(linked.offsets, universal.offsets);
        assert_eq!(linked.bytes, universal.bytes);
    }
}
