//! Host-side backing for the AOT backend's one remaining import
//! (docs/aot.md §5).
//!
//! `host.ta` and `host.pure` are both gone: an artifact links the ta engine
//! (`pinewasm_ta`) and the numeric crate (`pinewasm_num`) rather
//! than calling out for them, which is what makes it standalone.
//! What is left is `host.vm` — the interpreter executing the
//! heap/string/effect/request opcodes a native artifact deliberately does not
//! compile — plus the two numeric entry points the interpreter-hosted tier
//! and the differential test harness still call directly.
//!
//! Value encoding across the import boundary (all f64):
//! - numeric: the value; na = NaN.
//! - bool operands/results: 0.0 = false, 1.0 = true, NaN = na.
//! - int results: exact in f64 (the engine's int results are bar offsets /
//!   counts, far below 2^53).

use crate::{
    AlertEvent, AotVmState, Drawings, LogEntry, MarketData, RunOptions, RuntimeError,
    StrategyOutcome,
};
use pinewasm_csil::CsilProgram;
use pinewasm_hir::PureFunc;
use pinewasm_mir::{HandleId, Value};
use std::sync::Arc;

mod color_cells;

/// Evaluate a pure numeric function with the interpreter's exact semantics:
/// na (NaN) propagates like `pure_num`'s short-circuit and results normalize
/// non-finite → na like `Value::from_f64`.
///
/// Every arm delegates to [`pinewasm_num`], which is also what emitted
/// artifacts call: `pinewasm_aot` splices a WebAssembly build of that
/// crate into each module, so the two agree in every bit rather than
/// approximately. `MathRound` here is exclusively its two-argument (value,
/// precision) form; 1-arg rounding compiles inline.
///
/// This entry point survives the removal of the `host.pure` import because
/// the interpreter-hosted Wasm tier still routes through it.
pub fn pure_eval(func: PureFunc, a: f64, b: f64) -> f64 {
    match func {
        PureFunc::MathExp => pinewasm_num::pure_exp(a),
        PureFunc::MathLog => pinewasm_num::pure_log(a),
        PureFunc::MathLog10 => pinewasm_num::pure_log10(a),
        PureFunc::MathSin => pinewasm_num::pure_sin(a),
        PureFunc::MathCos => pinewasm_num::pure_cos(a),
        PureFunc::MathTan => pinewasm_num::pure_tan(a),
        PureFunc::MathAsin => pinewasm_num::pure_asin(a),
        PureFunc::MathAcos => pinewasm_num::pure_acos(a),
        PureFunc::MathAtan => pinewasm_num::pure_atan(a),
        PureFunc::MathPow => pinewasm_num::pure_pow(a, b),
        PureFunc::MathRound => pinewasm_num::pure_round2(a, b),
        _ => f64::NAN,
    }
}

/// Floating remainder with the interpreter's exact Rust `%` semantics.
/// WebAssembly has no f64 remainder opcode, so compiled artifacts call the
/// linked runtime rather than using a lossy arithmetic expansion for large
/// quotients.
pub fn f64_rem(a: f64, b: f64) -> f64 {
    pinewasm_num::pure_rem(a, b)
}

// ---------------------------------------------------------------------------
// host.vm: the VM as the AOT host (docs/aot.md §5)
// ---------------------------------------------------------------------------

/// Scratch-cell encodings for `host.vm` arguments and results. The emitter
/// writes each operand into an 8-byte cell per its static wasm type; these
/// tags tell the host how to decode the cell back into a [`Value`] (and how
/// to encode the result). Must match `pinewasm_aot`'s `VTy` → tag map.
pub mod tag {
    /// f64 bits; NaN = na.
    pub const F64: u8 = 0;
    /// i64; `i64::MIN` = na.
    pub const I64: u8 = 1;
    /// Kleene bool 0/1/2 (1 = na).
    pub const BOOL3: u8 = 2;
    /// Packed rgbt in the low 32 bits; -1 = na.
    pub const COLOR: u8 = 3;
    /// Run string-table handle; -1 = na.
    pub const STR: u8 = 4;
    /// Heap handle; -1 = na.
    pub const REF: u8 = 5;
    /// Result-only: immediately-popped value; encodes na for any type.
    pub const DISCARD: u8 = 6;
}

/// Statuses [`VmHost::vm_call`] returns to the emitted module. Zero is
/// success; the failure codes distinguish "the run aborted with a
/// structured runtime error" (read it via [`VmHost::take_error`]) from the
/// two that mean "rerun the request on the interpreter": the
/// static cell typing did not fit the runtime value (see
/// [`VmHost::abi_mismatch`]), or the host heap budget was spent.
pub mod vm_status {
    pub const OK: i32 = 0;
    pub const HOST_ERROR: i32 = 5;
    pub const ABI_MISMATCH: i32 = 6;
    /// The host heap ran out of its element or object budget. This is not
    /// the interpreter's verdict: the interpreter collects unreachable
    /// objects at bar boundaries, and this host collects only when a module
    /// with a collector asks it to ([`super::gc_site`]), since only the module can
    /// see the references it keeps in its own slots. Until then it counts
    /// every object it ever allocated where the interpreter counts the live
    /// ones. The request must rerun on the interpreter, which
    /// either finishes or raises the exact structured error. Equal to
    /// `pinewasm_aot`'s `status::HEAP_REROUTE`, which a module returns for
    /// the same reason from its own heap.
    pub const HEAP_REROUTE: i32 = 7;
}

/// Do not let a larger safety allowance postpone reclamation by tens of
/// millions of dead elements. Preserve the normal threshold for smaller runs.
pub(crate) fn host_gc_threshold(live: usize, normal_due: usize) -> usize {
    normal_due.min(live.saturating_add(1_000_000))
}

/// Reserved site ids of a module's collection, equal to `pinewasm_aot`'s
/// `host_gc_site`. Real sites index the module's site table, which never
/// comes near them, and neither reads or writes `cells[0]`, the per-bar
/// budget cell.
pub mod gc_site {
    /// `cells[1]` = n, `cells[2..2 + n]` = host references the module holds:
    /// roots of the next [`COLLECT`].
    pub const ROOTS: u32 = 0x7FFF_FFFE;
    /// Collect the host heap from the drawing registry and the roots sent
    /// since the last collection; then `cells[1]` = its live elements and
    /// `cells[2]` = its live objects.
    pub const COLLECT: u32 = 0x7FFF_FFFF;
    /// A host-only module requests a sweep only under allocation pressure.
    pub const COLLECT_IF_DUE: u32 = 0x7FFF_FFFD;
}

/// The `run` statuses a module returns before its first bar, so the run
/// executed none. [`VmHost::drain_strategy`] reads them: the interpreter
/// rejects the same requests before its first bar, with an empty outcome
/// that has no strategy results. Equal to `pinewasm_aot`'s
/// `status::TA_INIT_FAILED` and `status::INVALID_TA_LENGTH`.
pub mod pre_bar_status {
    /// The linked `ta.*` engine refused the module's declaration table.
    pub const TA_INIT_FAILED: i32 = 8;
    /// An input-backed TA parameter is outside its legal range.
    pub const INVALID_TA_LENGTH: i32 = 12;
}

/// The statuses a module stops with from its own code that are final
/// errors, not requests for the interpreter. Equal to `pinewasm_aot`'s
/// `status::BUDGET_EXCEEDED`, `status::HISTORY_LIMIT` and
/// `status::RUN_BUDGET_EXCEEDED`. The loader of a module that imports its
/// host builds the error of such a stop from the header. A linked module
/// asks its host what the stop becomes ([`VmHost::native_stop`]).
pub mod native_status {
    /// A bar spent its instruction budget.
    pub const BUDGET_EXCEEDED: i32 = 1;
    /// A history offset computed at run time is past `max_bars_back`.
    pub const HISTORY_LIMIT: i32 = 4;
    /// The run spent its cumulative instruction budget.
    pub const RUN_BUDGET_EXCEEDED: i32 = 9;
}

/// Why a linked host hands back an instruction budget stop that a hosted
/// instruction made. After such a stop the interpreter runs the program as
/// it was before optimization (`CsilFunction::budget_reference`) and, when
/// that run stops on the same bar with the same results, reports the source
/// location that run stopped at. Only a second run can say which location
/// that is.
const BUDGET_STOP_REASON: &str =
    "an instruction budget stop takes its source location from the interpreter's reference run";

/// Why a linked host hands back an instruction budget stop that the
/// module's own code made. Besides the source location above, the error of
/// such a stop depends on the order in which an instruction makes its own
/// checks before it charges for its work (copying an object looks at the
/// object first), and that order is the interpreter's to define. A stop
/// handed back is right whatever order the module's code makes them in.
const NATIVE_BUDGET_STOP_REASON: &str =
    "an instruction budget stop in the module's own code is the interpreter's to report";

/// One `host.vm` call site: the instruction offset it executes and the
/// static cell encodings of its stack operands (argument order — deepest
/// first) and pushed result. Produced by `pinewasm_aot::host_site_plan`
/// from the same type analysis that drives emission, so the module and the
/// host always agree by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteSpec {
    pub pc: u32,
    pub args: Vec<u8>,
    pub ret: Option<u8>,
}

/// Host state for an AOT run whose program uses VM-owned surfaces
/// (collections, UDT objects, strings, drawings, request.*, effects). Each
/// `host.vm` import call decodes the scratch cells into [`Value`]s, swaps
/// the persistent state into a real `Vm`, executes the same handler the
/// interpreter dispatches to, and encodes the result back — one semantic
/// definition, per instruction.
pub struct VmHost {
    core: HostCore<VmRuntime>,
    /// The decoded operands of the call in progress; kept so a call does
    /// not allocate a vector for them.
    operands: Vec<Value>,
    /// What a host linked into its module keeps ([`VmHost::linked`]);
    /// `None` for a host a module imports, whose cells are unchanged.
    linked: Option<Linked>,
}

/// The cell state of a host linked into its module.
struct Linked {
    /// The handle of each run string: equal strings share one.
    strings: std::collections::HashMap<String, u32>,
    /// Full colors, which a packed cell cannot hold.
    colors: color_cells::ColorCells,
}

impl Linked {
    fn new(strings: &[String]) -> Linked {
        Linked {
            strings: string_index(strings),
            colors: color_cells::ColorCells::default(),
        }
    }
}

/// The fields of a [`VmHost`], generic over the per-run pieces so that
/// construction ([`fresh_host`]) and [`VmHost::reset`] ([`reset_host`]) are
/// Aeneas-extractable; [`VmRuntime`] is the only instantiation.
pub(crate) struct HostCore<R: HostRuntime> {
    program: R::Program,
    options: R::Options,
    bars: usize,
    sites: Vec<SiteSpec>,
    state: R::State,
    /// The run string table: constant-pool ids first (so `PushStr` handles
    /// work unchanged), dynamic strings appended as host ops produce them.
    strings: R::Strings,
    last_bar: Option<u32>,
    error: Option<R::Error>,
    mismatch: Option<String>,
}

/// The per-run pieces of a host that [`fresh_host`] builds and
/// [`reset_host`] rebuilds. The only implementation is [`VmRuntime`]; the
/// trait exists so that the two can be extracted by Aeneas without the VM.
pub(crate) trait HostRuntime {
    type Program;
    type Options;
    type Data;
    type State;
    type Strings;
    type Error;
    /// The per-run VM state of a fresh run of `bars` bars over `data`.
    fn fresh_state(
        program: &Self::Program,
        data: Self::Data,
        options: &Self::Options,
        bars: usize,
    ) -> Self::State;
    /// The market data the current run state was built over.
    fn retained_data(state: &Self::State) -> Self::Data;
    /// The initial run string table (the constant pool).
    fn string_table(program: &Self::Program) -> Self::Strings;
}

/// The VM as a [`HostRuntime`].
pub(crate) enum VmRuntime {}

impl HostRuntime for VmRuntime {
    // Shared, not owned: every `host.vm` call assembles a `Vm` around them.
    type Program = Arc<CsilProgram>;
    type Options = Arc<RunOptions>;
    type Data = MarketData;
    type State = AotVmState;
    type Strings = Vec<String>;
    type Error = RuntimeError;

    fn fresh_state(
        program: &Arc<CsilProgram>,
        data: MarketData,
        options: &Arc<RunOptions>,
        bars: usize,
    ) -> AotVmState {
        AotVmState::new(program, data, options, bars)
    }

    fn retained_data(state: &AotVmState) -> MarketData {
        state.data().clone()
    }

    fn string_table(program: &Arc<CsilProgram>) -> Vec<String> {
        program.constants.clone()
    }
}

/// A host over a fresh run (the body of [`VmHost::new`]).
pub(crate) fn fresh_host<R: HostRuntime>(
    program: R::Program,
    data: R::Data,
    options: R::Options,
    bars: usize,
    sites: Vec<SiteSpec>,
) -> HostCore<R> {
    let state = R::fresh_state(&program, data, &options, bars);
    let strings = R::string_table(&program);
    HostCore {
        program,
        options,
        bars,
        sites,
        state,
        strings,
        last_bar: None,
        error: None,
        mismatch: None,
    }
}

/// Re-arm `host` for a fresh run over the same program/data/options (the
/// body of [`VmHost::reset`]).
pub(crate) fn reset_host<R: HostRuntime>(host: &mut HostCore<R>) {
    let data = R::retained_data(&host.state);
    host.state = R::fresh_state(&host.program, data, &host.options, host.bars);
    host.strings = R::string_table(&host.program);
    host.last_bar = None;
    host.error = None;
    host.mismatch = None;
}

impl VmHost {
    pub fn new(
        program: CsilProgram,
        data: MarketData,
        options: RunOptions,
        bars: usize,
        sites: Vec<SiteSpec>,
    ) -> VmHost {
        VmHost {
            core: fresh_host(Arc::new(program), data, Arc::new(options), bars, sites),
            operands: Vec::new(),
            linked: None,
        }
    }

    /// Make this the host of a module that links it (`compile_chart_wasm_program`),
    /// which changes two kinds of cell.
    ///
    /// Every run string gets one handle: a string a hosted instruction
    /// produces takes the handle of the table entry with the same text, and
    /// a new entry only when there is none. Two handles are then equal
    /// exactly when their strings are, provided the program's constant pool
    /// (the table's first entries) holds no text twice, which
    /// [`VmHost::strings_are_distinct`] reports; such a module compares
    /// string handles itself.
    ///
    /// A color that a packed cell cannot hold (a fractional transparency, a
    /// color read from a color input) gets a handle into a run-local table
    /// instead of failing the call. A module that imports its host has no
    /// such handles: there the same color still ends the native run.
    #[must_use]
    pub fn linked(mut self) -> VmHost {
        self.linked = Some(Linked::new(&self.core.strings));
        self
    }

    /// Whether no two entries of the run string table hold the same text.
    /// Only a linked host knows; any other answers `false`.
    pub fn strings_are_distinct(&self) -> bool {
        self.linked
            .as_ref()
            .is_some_and(|linked| linked.strings.len() == self.core.strings.len())
    }

    /// Re-arm for a fresh run over the same program/data/options: rebuild
    /// the per-run VM state (heap, drawings, logs, memos) and string table.
    /// Reusing a host across runs skips the expensive construction path
    /// (program/data deserialization at the wasm boundary).
    pub fn reset(&mut self) {
        reset_host(&mut self.core);
        if self.linked.is_some() {
            self.linked = Some(Linked::new(&self.core.strings));
        }
    }

    fn decode(&self, cell: i64, tag: u8) -> Result<Value, String> {
        match (&self.linked, tag) {
            (Some(linked), tag::COLOR) => linked.colors.decode(cell),
            _ => decode_cell(cell, tag, &self.core.strings),
        }
    }

    fn encode(&mut self, value: &Value, tag: u8) -> Result<i64, String> {
        match (&mut self.linked, tag, value) {
            (Some(linked), tag::COLOR, _) => linked.colors.encode(value),
            (Some(linked), tag::STR, Value::String(text)) => {
                if let Some(&handle) = linked.strings.get(text) {
                    return Ok(i64::from(handle));
                }
                // A handle travels in a cell as a non-negative `i32`.
                let handle = i32::try_from(self.core.strings.len())
                    .ok()
                    .and_then(|handle| u32::try_from(handle).ok())
                    .ok_or_else(|| "string table overflow".to_string())?;
                self.core.strings.push(text.clone());
                linked.strings.insert(text.clone(), handle);
                Ok(i64::from(handle))
            }
            _ => encode_cell(value, tag, &mut self.core.strings),
        }
    }

    /// The cell a module compiled for a linked host reads for `input`
    /// instead of calling the host: the input's value for this run, encoded
    /// as the hosted `ReadInput` encodes it. `None` for an input the module
    /// reads from the loader's own table (every kind but string and color).
    ///
    /// A string or color input is the same on every bar, so one cell written
    /// before the run stands for every read.
    pub fn input_cell(&mut self, input: u32) -> Result<Option<i64>, String> {
        let tag = match self
            .core
            .program
            .inputs
            .get(input as usize)
            .map(|decl| decl.kind)
        {
            Some(pinewasm_hir::InputKind::String) => tag::STR,
            Some(pinewasm_hir::InputKind::Color) => tag::COLOR,
            _ => return Ok(None),
        };
        let value = self
            .core
            .state
            .enter(&self.core.program, &self.core.options, self.core.bars, 0)
            .resolve_input(input, 0)
            .map_err(|error| error.message)?;
        self.encode(&value, tag).map(Some)
    }

    /// Decode a complete color lane and select its palette entries through
    /// the same source-identity rule as the interpreter's output writer.
    pub fn resolve_color_series(
        &self,
        family: u32,
        slot: u32,
        role: u32,
        cells: &[i32],
    ) -> Result<crate::PlotColorSeries, String> {
        let program = &self.core.program;
        let declared = match family {
            0 => program
                .plots
                .get(slot as usize)
                .map(|d| (&d.color_roles, &d.style)),
            1 => program
                .visuals
                .get(slot as usize)
                .map(|d| (&d.color_roles, &d.style)),
            2 => program
                .fills
                .get(slot as usize)
                .map(|d| (&d.color_roles, &d.style)),
            _ => None,
        };
        let (roles, style) = declared
            .ok_or_else(|| "native color output is outside its declarations".to_string())?;
        let role = *roles
            .get(role as usize)
            .ok_or_else(|| "native color role is outside its declaration".to_string())?;
        if cells.len() > self.core.bars {
            return Err("native color lane exceeds its run length".into());
        }
        let values = cells
            .iter()
            .map(|&cell| self.decode(i64::from(cell), tag::COLOR))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(crate::PlotColorSeries::from_values(
            role,
            &values,
            &style.color_palette,
        ))
    }

    /// Mixed fill outputs keep their numeric boundaries at full precision in the linked host.
    pub fn resolve_hosted_fill_series(
        &mut self,
        slot: u32,
        role: u32,
        visible: usize,
    ) -> Result<Option<crate::PlotColorSeries>, String> {
        let fill = self
            .core
            .program
            .fills
            .get(slot as usize)
            .ok_or("invalid fill slot")?;
        if !fill
            .color_roles
            .iter()
            .any(|role| role.value_domain() == "float-or-na")
        {
            return Ok(None);
        }
        let output_role = *fill
            .color_roles
            .get(role as usize)
            .ok_or("invalid fill role")?;
        let palette = fill.style.color_palette.clone();
        let state =
            self.core
                .state
                .enter(&self.core.program, &self.core.options, self.core.bars, 0);
        let values = state
            .fill_colors
            .get(slot as usize)
            .and_then(|channels| channels.get(role as usize))
            .and_then(|values| values.get(..visible))
            .ok_or("hosted fill exceeds its run length")?;
        Ok(Some(crate::PlotColorSeries::from_values(
            output_role,
            values,
            &palette,
        )))
    }

    /// Resolve a string handle (a `host.alert` message id may exceed the
    /// constant pool once dynamic strings exist).
    pub fn string(&self, id: i64) -> Option<&str> {
        usize::try_from(id)
            .ok()
            .and_then(|i| self.core.strings.get(i))
            .map(String::as_str)
    }

    /// Resolve fixed presentation metadata from the native run's last
    /// completed bar using the host's actual string table and shared VM
    /// cell decoder. The slot/tag order comes from the emission planner.
    pub fn resolve_display_styles(
        &self,
        slots: &[u32],
        tags: &[u8],
        cells: &[i64],
        visible_bars: usize,
    ) -> Result<crate::NativeDisplayStyles, String> {
        if visible_bars > self.core.bars {
            return Err("native style snapshot exceeds the prepared bar count".into());
        }
        if slots.len() != tags.len() || slots.len() != cells.len() {
            return Err("native style cell count does not match its plan".into());
        }
        let values: std::collections::HashMap<u32, Value> = slots
            .iter()
            .zip(tags)
            .zip(cells)
            .map(|((&slot, &tag), &cell)| {
                if tag == tag::REF || tag == tag::DISCARD {
                    return Err("native styles must be scalar cells".into());
                }
                Ok((
                    slot,
                    if visible_bars == 0 {
                        Value::Na
                    } else {
                        self.decode(cell, tag)?
                    },
                ))
            })
            .collect::<Result<_, String>>()?;
        let resolve = |style: &pinewasm_csil::DisplayStyle, is_fill| {
            crate::resolve_display_style(
                style,
                is_fill,
                |slot| values.get(&slot),
                |input| {
                    self.core.program.inputs.get(input as usize).map(|decl| {
                        self.core
                            .options
                            .inputs
                            .get(&input)
                            .filter(|value| crate::input_value_matches_decl(value, decl))
                            .unwrap_or(&decl.defval)
                    })
                },
                |input| {
                    self.core
                        .program
                        .inputs
                        .get(input as usize)
                        .and_then(|decl| {
                            self.core
                                .options
                                .inputs
                                .get(&input)
                                .filter(|value| crate::input_value_matches_decl(value, decl))
                        })
                },
            )
        };
        Ok(crate::NativeDisplayStyles {
            plots: self
                .core
                .program
                .plots
                .iter()
                .map(|plot| resolve(&plot.style, false))
                .collect(),
            visuals: self
                .core
                .program
                .visuals
                .iter()
                .map(|visual| resolve(&visual.style, false))
                .collect(),
            fills: self
                .core
                .program
                .fills
                .iter()
                .map(|fill| resolve(&fill.style, true))
                .collect(),
        })
    }

    /// The structured runtime error a `HOST_ERROR` status refers to.
    pub fn take_error(&mut self) -> Option<RuntimeError> {
        self.core.error.take()
    }

    /// Why an `ABI_MISMATCH` status was returned (the request must rerun on
    /// the interpreter, which handles the value dynamically).
    pub fn abi_mismatch(&self) -> Option<&str> {
        self.core.mismatch.as_deref()
    }

    pub fn drain_logs(&mut self) -> Vec<LogEntry> {
        self.core.state.take_logs()
    }

    pub fn drain_alerts(&mut self) -> Vec<AlertEvent> {
        self.core.state.take_alerts()
    }

    /// Why this run's `strategy()` must rerun on the interpreter
    /// (a declaration setting, or a bar among the host's `bars` that the
    /// native bar loop cannot follow), or `None` when it can run natively.
    /// A strategy module's first host call returns `ABI_MISMATCH` with this
    /// reason, and [`VmHost::drain_strategy`] refuses, so the loader may
    /// check it first and skip the run.
    pub fn strategy_refusal(&self) -> Option<&'static str> {
        self.core.state.strategy_refusal(self.core.bars)
    }

    /// The run's strategy results, exactly the interpreter's
    /// `RunOutcome::strategy`, for a `strategy()` chart module whose `run`
    /// returned `status` with `bars_executed` complete bars (all of them on
    /// success; the error bar otherwise). The broker finishes the bars the
    /// module made no host call on, and its order-fill alerts join the alert
    /// stream, so drain this before [`VmHost::drain_alerts`]. `Ok(None)` for
    /// an indicator, an already-drained run, or a run that stopped before its
    /// first bar ([`pre_bar_status`]), which the interpreter reports with no
    /// strategy results; `Err` with the [`VmHost::strategy_refusal`] reason
    /// when the run must use the interpreter.
    pub fn drain_strategy(
        &mut self,
        status: i32,
        bars_executed: u32,
    ) -> Result<Option<StrategyOutcome>, String> {
        if let Some(why) = self.strategy_refusal() {
            return Err(why.to_string());
        }
        // A zero `bars_executed` also follows an error on the first bar,
        // whose pre-script broker work ran; only the status tells them apart.
        if matches!(
            status,
            pre_bar_status::TA_INIT_FAILED | pre_bar_status::INVALID_TA_LENGTH
        ) {
            return Ok(None);
        }
        let bars = self.core.bars;
        let mut open = self.core.state.broker_open;
        let vm = self
            .core
            .state
            .enter(&self.core.program, &self.core.options, bars, 0);
        let outcome = vm.aot_strategy_outcome((bars_executed as usize).min(bars), bars, &mut open);
        self.core.state.broker_open = open;
        Ok(outcome)
    }

    /// Serialize the final live drawings exactly as `run()` does.
    pub fn drain_drawings(&mut self) -> Drawings {
        self.core
            .state
            .enter(&self.core.program, &self.core.options, self.core.bars, 0)
            .serialize_drawings()
    }

    /// Execute the host site `site` on chart bar `bar`. `cells[0]` carries
    /// the remaining per-bar budget in and out; `cells[1..]` carry the
    /// operands (argument order) and, on success, the result in `cells[1]`.
    pub fn vm_call(&mut self, site: u32, bar: u32, cells: &mut [i64]) -> i32 {
        // A collection runs at a bar boundary, before the bar's first site,
        // and touches no broker or per-bar state.
        match site {
            gc_site::ROOTS => return self.gc_roots(cells),
            gc_site::COLLECT => return self.gc_collect(bar, cells, false),
            gc_site::COLLECT_IF_DUE => return self.gc_collect(bar, cells, true),
            _ => {}
        }
        // A strategy module's broker follows the module's bars; one this run
        // cannot follow, or a call for a bar the broker has already closed,
        // reroutes the run before any effect.
        if let Some(why) = self.strategy_refusal() {
            self.core.mismatch = Some(format!("strategy() needs the interpreter: {why}"));
            return vm_status::ABI_MISMATCH;
        }
        if self
            .core
            .state
            .broker_open
            .is_some_and(|open| (bar as usize) < open)
        {
            self.core.mismatch = Some(format!(
                "host call for bar {bar} after the broker reached a later bar"
            ));
            return vm_status::ABI_MISMATCH;
        }
        let mut fx = CallCtx {
            args: std::mem::take(&mut self.operands),
            host: self,
            result: None,
            error: None,
        };
        let status = vm_call_protocol(&mut fx, site, bar, cells);
        let mut operands = fx.args;
        // A mismatch can leave operands decoded before it; no call reads them.
        operands.clear();
        self.operands = operands;
        if status == vm_status::HOST_ERROR && self.heap_budget_spent() {
            // Drop the error: it reports this host's cumulative count, which
            // the interpreter's rerun may never reach.
            self.core.error = None;
            return vm_status::HEAP_REROUTE;
        }
        if status == vm_status::HOST_ERROR && self.budget_stop_needs_interpreter() {
            // Drop the error: its source location is the stopped
            // instruction's, which is not always the one the interpreter
            // reports.
            self.core.error = None;
            self.core.mismatch = Some(BUDGET_STOP_REASON.to_string());
            return vm_status::ABI_MISMATCH;
        }
        status
    }

    /// What is left of the run's string budget: the interpreter's
    /// `max_string_bytes_per_run` less what the run has reserved so far.
    ///
    /// The interpreter reserves string bytes at instructions a linked module
    /// runs itself (a string constant pushed, an `alertcondition` that
    /// fires, a copied object's string fields), and no host call tells the
    /// host of them. Such a module therefore keeps this number: it starts
    /// from it, takes each of its own reservations from it, hands it to
    /// every hosted call ([`VmHost::vm_call_metered`]) and takes back what
    /// the call left. The count is then the interpreter's at every
    /// instruction, so the run stops on the budget where the interpreter
    /// stops, with its error.
    ///
    /// A budget of every byte `usize` counts is no budget: the interpreter's
    /// sum saturates there and no reservation fails. What is left of it is
    /// the largest number, which no run's reservations use up.
    pub fn string_room(&self) -> i64 {
        let budget = self.core.options.max_string_bytes_per_run;
        if budget == usize::MAX {
            return i64::MAX;
        }
        let left = budget.saturating_sub(self.core.state.string_bytes_allocated());
        i64::try_from(left).unwrap_or(i64::MAX)
    }

    /// Take `room` as what is left of the string budget: the module's count
    /// after its own reservations since it last heard this host's
    /// ([`VmHost::string_room`]). `false`, after recording why, for a number
    /// no such sequence of reservations leaves.
    fn adopt_string_room(&mut self, room: i64) -> bool {
        // The sum saturates as the interpreter's does. Below the largest
        // budget a saturated sum is over the budget, and so refused.
        let reserved = self
            .string_room()
            .checked_sub(room)
            .filter(|&reserved| room >= 0 && reserved >= 0)
            .map(|reserved| {
                self.core
                    .state
                    .string_bytes_allocated()
                    .saturating_add(usize::try_from(reserved).unwrap_or(usize::MAX))
            })
            .filter(|&allocated| allocated <= self.core.options.max_string_bytes_per_run);
        match reserved {
            Some(allocated) => {
                self.core.state.set_string_bytes_allocated(allocated);
                true
            }
            None => {
                self.core.mismatch = Some(format!(
                    "the module's string budget of {room} bytes is not this run's"
                ));
                false
            }
        }
    }

    /// [`VmHost::vm_call`] for a linked module that keeps the string budget
    /// ([`VmHost::string_room`]): `room` is the module's count going in.
    /// Returns what is left after the call, which is never negative, or the
    /// call's nonzero status negated.
    pub fn vm_call_metered(&mut self, site: u32, bar: u32, cells: &mut [i64], room: i64) -> i64 {
        if !self.adopt_string_room(room) {
            return -i64::from(vm_status::ABI_MISMATCH);
        }
        match self.vm_call(site, bar, cells) {
            vm_status::OK => self.string_room(),
            status => -i64::from(status),
        }
    }

    /// The status a linked module stops with when `bytes` of the string
    /// budget, which the instruction at `pc` reserves on bar `bar`, do not
    /// fit the `room` the module has left. The interpreter's own reservation
    /// decides: it fails, with the interpreter's error, which
    /// [`VmHost::take_error`] then returns for the
    /// [`vm_status::HOST_ERROR`] this returns. A reservation that fits is
    /// the module's to make, so one that succeeds here means the module's
    /// count is not this host's, and the run is the interpreter's
    /// ([`vm_status::ABI_MISMATCH`]).
    pub fn string_reserve(&mut self, pc: u32, bar: u32, room: i64, bytes: i64) -> i32 {
        if !self.adopt_string_room(room) {
            return vm_status::ABI_MISMATCH;
        }
        // A size `usize` cannot hold is one the interpreter's own sum
        // overflows on; it reports that itself.
        let Ok(bytes) = usize::try_from(bytes) else {
            self.core.mismatch = Some(format!("a string reservation of {bytes} bytes"));
            return vm_status::ABI_MISMATCH;
        };
        let reserved = self
            .core
            .state
            .enter(
                &self.core.program,
                &self.core.options,
                self.core.bars,
                bar as usize,
            )
            .reserve_string_bytes(bytes, pc as usize);
        match reserved {
            Ok(()) => {
                self.core.mismatch = Some(format!(
                    "a string reservation of {bytes} bytes fits the budget the module found spent"
                ));
                vm_status::ABI_MISMATCH
            }
            Err(error) => {
                self.core.error = Some(error);
                vm_status::HOST_ERROR
            }
        }
    }

    /// The length in bytes of the run string a `STR` cell names: 0 for `na`
    /// (the interpreter's empty message), and -1 for a cell that names no
    /// string.
    pub fn string_len(&self, handle: i32) -> i64 {
        if handle == -1 {
            return 0;
        }
        self.string(i64::from(handle))
            .and_then(|text| i64::try_from(text.len()).ok())
            .unwrap_or(-1)
    }

    /// The status a linked module stops with after its own code stopped the
    /// run with `code` on bar `bar` at instruction `pc`.
    ///
    /// A history stop ([`native_status::HISTORY_LIMIT`]) becomes
    /// [`vm_status::HOST_ERROR`], and [`VmHost::take_error`] returns the
    /// error the interpreter raises for it, built by the interpreter's own
    /// constructor from the offending offset, which the module left in
    /// `cells[1]`. An instruction budget stop becomes
    /// [`vm_status::ABI_MISMATCH`]: its error is the interpreter's to
    /// report, since its kind follows the order of an instruction's own
    /// checks and its location can come from the interpreter's second run.
    /// Any other `code` is returned as it is.
    pub fn native_stop(&mut self, code: i32, pc: u32, bar: u32, cells: &[i64]) -> i32 {
        match code {
            native_status::BUDGET_EXCEEDED | native_status::RUN_BUDGET_EXCEEDED => {
                self.core.mismatch = Some(NATIVE_BUDGET_STOP_REASON.to_string());
                vm_status::ABI_MISMATCH
            }
            native_status::HISTORY_LIMIT => {
                // The module stops only on an offset past the limit.
                let limit = u64::from(self.core.program.metadata.max_bars_back);
                let offset = cells
                    .get(1)
                    .and_then(|&cell| u64::try_from(cell).ok())
                    .filter(|&offset| offset > limit);
                let Some(offset) = offset else {
                    self.core.mismatch =
                        Some("a history stop without an offset past max_bars_back".to_string());
                    return vm_status::ABI_MISMATCH;
                };
                let error = self
                    .core
                    .state
                    .enter(
                        &self.core.program,
                        &self.core.options,
                        self.core.bars,
                        bar as usize,
                    )
                    .dynamic_history_too_deep(offset, pc as usize);
                self.core.error = Some(error);
                vm_status::HOST_ERROR
            }
            _ => code,
        }
    }

    /// Whether the recorded error is an instruction budget stop that only
    /// the interpreter can place: a hosted instruction spent the bar's
    /// budget in a linked module of a program that carries its unoptimized
    /// form ([`BUDGET_STOP_REASON`]). Without that form the error is the
    /// interpreter's own code's, stopped instruction and all, and it stands.
    /// A host a module imports reports the error as before.
    fn budget_stop_needs_interpreter(&self) -> bool {
        self.linked.is_some()
            && self
                .core
                .program
                .functions
                .first()
                .is_some_and(|main| main.budget_reference.is_some())
            && self.core.error.as_ref().is_some_and(|error| {
                matches!(
                    error.kind,
                    crate::RuntimeErrorKind::InstructionLimitExceeded
                        | crate::RuntimeErrorKind::RunInstructionLimitExceeded
                )
            })
    }

    /// [`gc_site::ROOTS`]: keep `cells[2..2 + cells[1]]` as roots of the
    /// next collection.
    fn gc_roots(&mut self, cells: &[i64]) -> i32 {
        let batch = cells
            .get(1)
            .and_then(|&count| usize::try_from(count).ok())
            .and_then(|count| cells.get(2..count.checked_add(2)?));
        let Some(batch) = batch else {
            self.core.mismatch = Some("collection roots overrun the scratch region".to_string());
            return vm_status::ABI_MISMATCH;
        };
        for &cell in batch {
            let Ok(index) = u32::try_from(cell) else {
                self.core.mismatch = Some(format!("collection root {cell} is not a handle"));
                return vm_status::ABI_MISMATCH;
            };
            self.core.state.module_roots.push(HandleId(index));
        }
        vm_status::OK
    }

    /// [`gc_site::COLLECT`]: collect the host heap and report what stays
    /// live in `cells[1]` (elements) and `cells[2]` (objects).
    fn gc_collect(&mut self, bar: u32, cells: &mut [i64], conditional: bool) -> i32 {
        if cells.len() < 3 {
            self.core.mismatch = Some(format!(
                "a collection needs three scratch cells, got {}",
                cells.len()
            ));
            return vm_status::ABI_MISMATCH;
        }
        self.core.state.unpark();
        let roots = std::mem::take(&mut self.core.state.module_roots);
        if conditional
            && self.core.state.heap.arena_len() < self.core.state.host_gc_arena_due
            && self.core.state.heap.live_elements() < self.core.state.host_gc_elems_due
        {
            cells[1] = self.core.state.heap.live_elements() as i64;
            cells[2] = self.core.state.heap.len() as i64;
            return vm_status::OK;
        }
        let (elements, objects) = self
            .core
            .state
            .enter(
                &self.core.program,
                &self.core.options,
                self.core.bars,
                bar as usize,
            )
            .collect_aot_host_heap(&roots);
        self.core.state.unpark();
        self.core.state.host_gc_arena_due =
            crate::GC_MIN_ARENA.max(self.core.state.heap.arena_len().saturating_mul(2));
        self.core.state.host_gc_elems_due = host_gc_threshold(
            self.core.state.heap.live_elements(),
            crate::re_arm_elem_threshold(&self.core.state.heap),
        );
        let (Ok(elements), Ok(objects)) = (i64::try_from(elements), i64::try_from(objects)) else {
            self.core.mismatch = Some("live heap counts overflow a scratch cell".to_string());
            return vm_status::ABI_MISMATCH;
        };
        cells[1] = elements;
        cells[2] = objects;
        vm_status::OK
    }

    /// Whether the recorded error is a heap element or object budget failure
    /// (see [`vm_status::HEAP_REROUTE`]).
    fn heap_budget_spent(&self) -> bool {
        self.core
            .error
            .as_ref()
            .is_some_and(|error| error.kind == crate::RuntimeErrorKind::CollectionLimitExceeded)
    }
}

/// The handle of each string of `strings`: the first entry holding a text
/// names it, as a constant pool that repeats a text would have it.
fn string_index(strings: &[String]) -> std::collections::HashMap<String, u32> {
    let mut index = std::collections::HashMap::with_capacity(strings.len());
    for (handle, text) in strings.iter().enumerate() {
        if let Ok(handle) = u32::try_from(handle) {
            index.entry(text.clone()).or_insert(handle);
        }
    }
    index
}

/// The host effects of [`VmHost::vm_call`], in the order
/// [`vm_call_protocol`] sequences them. The protocol owns every scratch-cell
/// access (through the kernels below) and every status code; the effects own
/// everything else (site table, string table, the VM,
/// diagnostics). The only implementation is [`CallCtx`]; the protocol is
/// generic so that it can be extracted by Aeneas without the VM.
pub(crate) trait CallEffects {
    /// Operand count and result presence of `site`, or `None` after
    /// recording the out-of-range mismatch.
    fn site_shape(&mut self, site: u32) -> Option<(usize, bool)>;
    /// Record the scratch-size mismatch.
    fn layout_mismatch(&mut self, site: u32, nargs: usize, has_ret: bool, len: usize);
    /// Per-bar bookkeeping before the call.
    fn enter_bar(&mut self, bar: u32);
    /// Decode operand `i` of `site` from its cell; `false` after recording
    /// the mismatch.
    fn decode_arg(&mut self, site: u32, i: usize, cell: i64) -> bool;
    /// Run `site` on the decoded operands with the per-bar `budget`:
    /// success, and the remaining budget.
    fn exec(&mut self, site: u32, bar: u32, budget: u64) -> (bool, u64);
    /// Record the runtime error of a failed [`CallEffects::exec`].
    fn record_error(&mut self);
    /// Encode the result of a successful [`CallEffects::exec`]; `None` after
    /// recording the mismatch.
    fn encode_result(&mut self, site: u32) -> Option<i64>;
}

/// Decode the `nargs` operands in order, stopping at the first mismatch.
fn decode_args<E: CallEffects>(fx: &mut E, site: u32, nargs: usize, cells: &[i64]) -> bool {
    let mut i = 0;
    let mut ok = true;
    while ok && i < nargs {
        ok = fx.decode_arg(site, i, operand_cell(cells, i));
        i += 1;
    }
    ok
}

/// The call protocol of [`VmHost::vm_call`] (Aeneas-extractable): site
/// lookup, layout check, bar bookkeeping, operand decoding, budget in,
/// execution, budget out, then the error or the result write.
pub(crate) fn vm_call_protocol<E: CallEffects>(
    fx: &mut E,
    site: u32,
    bar: u32,
    cells: &mut [i64],
) -> i32 {
    let Some((nargs, has_ret)) = fx.site_shape(site) else {
        return vm_status::ABI_MISMATCH;
    };
    if !check_layout(nargs, has_ret, cells.len()) {
        fx.layout_mismatch(site, nargs, has_ret, cells.len());
        return vm_status::ABI_MISMATCH;
    }
    // A fresh bar resets the once-per-bar alert dedup (matching `run()`).
    fx.enter_bar(bar);
    // Decode operands before assembling the Vm (decode borrows strings).
    if !decode_args(fx, site, nargs, cells) {
        return vm_status::ABI_MISMATCH;
    }
    let budget = load_budget(cells);
    let (ok, budget) = fx.exec(site, bar, budget);
    save_budget(cells, budget);
    if !ok {
        fx.record_error();
        return vm_status::HOST_ERROR;
    }
    if has_ret {
        let Some(cell) = fx.encode_result(site) else {
            return vm_status::ABI_MISMATCH;
        };
        store_result(cells, cell);
    }
    vm_status::OK
}

/// [`CallEffects`] of a [`VmHost`] for one call: the decoded operands, the
/// popped result and the runtime error travel between the protocol steps.
struct CallCtx<'a> {
    host: &'a mut VmHost,
    args: Vec<Value>,
    result: Option<Value>,
    error: Option<RuntimeError>,
}

impl CallEffects for CallCtx<'_> {
    fn site_shape(&mut self, site: u32) -> Option<(usize, bool)> {
        match self.host.core.sites.get(site as usize) {
            Some(spec) => Some((spec.args.len(), spec.ret.is_some())),
            None => {
                self.host.core.mismatch = Some(format!("host site {site} out of range"));
                None
            }
        }
    }

    fn layout_mismatch(&mut self, site: u32, nargs: usize, has_ret: bool, len: usize) {
        self.host.core.mismatch = Some(format!(
            "host site {site} needs {} scratch cells, got {}",
            needed_cells(nargs, has_ret).unwrap_or(usize::MAX),
            len
        ));
    }

    fn enter_bar(&mut self, bar: u32) {
        // Site calls arrive in execution order, so a bar change is exact.
        if self.host.core.last_bar != Some(bar) {
            self.host.core.state.clear_alert_dedup();
            self.host.core.last_bar = Some(bar);
        }
    }

    fn decode_arg(&mut self, site: u32, i: usize, cell: i64) -> bool {
        let t = self.host.core.sites[site as usize].args[i];
        match self.host.decode(cell, t) {
            Ok(v) => {
                self.args.push(v);
                true
            }
            Err(why) => {
                self.host.core.mismatch = Some(format!("site {site} arg {i}: {why}"));
                false
            }
        }
    }

    fn exec(&mut self, site: u32, bar: u32, budget: u64) -> (bool, u64) {
        let spec = &self.host.core.sites[site as usize];
        let pc = spec.pc as usize;
        let has_ret = spec.ret.is_some();
        let mut budget = budget;
        let core = &mut self.host.core;
        let mut open = core.state.broker_open;
        let vm = core
            .state
            .enter(&core.program, &core.options, core.bars, bar as usize);
        // A strategy's broker reaches this bar's script execution first, as
        // the interpreter's bar loop would have (see `Vm::aot_broker_reach`).
        vm.aot_broker_reach(bar as usize, &mut open);
        // The emitted module charges the dispatch unit against both its
        // per-bar and cumulative counters before entering `host.vm`. This
        // handler charges only the opcode's data-dependent extra work and
        // returns the remaining per-bar counter for mirrored run-wide
        // accounting on the Wasm side.
        vm.stack.clear();
        for v in self.args.drain(..) {
            vm.stack.push(v);
        }
        let outcome = vm.exec_host_site(pc, &mut budget);
        self.result = match outcome {
            Ok(()) => {
                if has_ret {
                    vm.stack.pop()
                } else {
                    None
                }
            }
            Err(_) => None,
        };
        core.state.broker_open = open;
        match outcome {
            Ok(()) => (true, budget),
            Err(e) => {
                self.error = Some(e);
                (false, budget)
            }
        }
    }

    fn record_error(&mut self) {
        self.host.core.error = self.error.take();
    }

    fn encode_result(&mut self, site: u32) -> Option<i64> {
        let t = self.host.core.sites[site as usize].ret?;
        let Some(value) = self.result.take() else {
            self.host.core.mismatch = Some(format!("site {site} produced no result"));
            return None;
        };
        match self.host.encode(&value, t) {
            Ok(cell) => Some(cell),
            Err(why) => {
                self.host.core.mismatch = Some(format!("site {site} result: {why}"));
                None
            }
        }
    }
}

/// The number of scratch cells a site needs: the budget cell plus
/// `max(nargs, has_ret)` cells, because operands occupy `cells[1..=nargs]`
/// and a result is written to `cells[1]`. `None` if the count does not fit
/// `usize` (then no buffer can hold it).
// Plain `if`s rather than `usize::from(bool)` / `Ord::max`, so the Aeneas
// extraction needs no external trait models.
#[allow(clippy::bool_to_int_with_if)]
fn needed_cells(nargs: usize, has_ret: bool) -> Option<usize> {
    let ret_cells: usize = if has_ret { 1 } else { 0 };
    let body = if nargs >= ret_cells { nargs } else { ret_cells };
    body.checked_add(1)
}

/// The scratch-cell layout check `vm_call` applies before touching `cells`:
/// the budget cell plus room for every operand and for the result.
///
/// A check of `len >= 1 + nargs` that ignored `has_ret` would admit a
/// one-cell buffer for a site with no operands and a result, and the result
/// write in [`store_result`] would then panic on `cells[1]`. With this check,
/// every cell access `vm_call` makes is in bounds whenever it returns
/// `true`. Emitted modules are unaffected:
/// `pinewasm_aot` sizes the scratch region as at least `1 + max(args, 1)`
/// cells.
fn check_layout(nargs: usize, has_ret: bool, len: usize) -> bool {
    match needed_cells(nargs, has_ret) {
        Some(needed) => len >= needed,
        None => false,
    }
}

/// Decode the per-bar budget carried in `cells[0]`: a plain `as` cast, so
/// a negative cell reads as a budget of at least `2^63` (effectively
/// unlimited); see `vm_call_negative_budget_is_effectively_unlimited`.
fn budget_in(cell: i64) -> u64 {
    cell as u64
}

/// Read operand `i` from its cell, `cells[1 + i]`, which follows the
/// budget cell.
fn operand_cell(cells: &[i64], i: usize) -> i64 {
    cells[1 + i]
}

/// Read the per-bar budget cell `cells[0]` and decode it with [`budget_in`].
fn load_budget(cells: &[i64]) -> u64 {
    budget_in(cells[0])
}

/// Write the remaining per-bar budget back into `cells[0]` with a plain
/// `as` cast (the inverse reinterpretation of [`budget_in`]).
fn save_budget(cells: &mut [i64], budget: u64) {
    cells[0] = budget as i64;
}

/// Write an encoded result into its cell, `cells[1]`. Panics on a buffer
/// shorter than two cells, which [`check_layout`] rejects for every site
/// with a result.
fn store_result(cells: &mut [i64], cell: i64) {
    cells[1] = cell;
}

fn decode_cell(cell: i64, t: u8, strings: &[String]) -> Result<Value, String> {
    Ok(match t {
        tag::F64 => {
            let v = f64::from_bits(cell as u64);
            if v.is_nan() {
                Value::Na
            } else {
                Value::from_f64(v)
            }
        }
        tag::I64 => {
            if cell == i64::MIN {
                Value::Na
            } else {
                Value::Int(cell)
            }
        }
        tag::BOOL3 => match cell {
            0 => Value::Bool(false),
            2 => Value::Bool(true),
            _ => Value::Na,
        },
        tag::COLOR => {
            let raw = cell as i32;
            if raw == -1 {
                Value::Na
            } else {
                let u = raw as u32;
                Value::Color {
                    r: (u >> 24) as u8,
                    g: (u >> 16) as u8,
                    b: (u >> 8) as u8,
                    t: f64::from((u & 0xFF).min(100)),
                    input_origin: None,
                }
            }
        }
        tag::STR => {
            let raw = cell as i32;
            if raw == -1 {
                Value::Na
            } else {
                let s = strings
                    .get(raw as usize)
                    .ok_or_else(|| format!("string handle {raw} out of range"))?;
                Value::String(s.clone())
            }
        }
        tag::REF => {
            let raw = cell as i32;
            if raw == -1 {
                Value::Na
            } else if raw < 0 {
                return Err(format!("negative heap handle {raw}"));
            } else {
                Value::Ref(HandleId(raw as u32))
            }
        }
        other => return Err(format!("unknown cell tag {other}")),
    })
}

fn encode_cell(value: &Value, t: u8, strings: &mut Vec<String>) -> Result<i64, String> {
    let mismatch = |value: &Value, want: &str| {
        Err(format!(
            "runtime {} does not fit the static {want} cell",
            value.type_name()
        ))
    };
    Ok(match (value, t) {
        // The consumer is a bare `Pop`: the value is dead, na suffices.
        (_, tag::DISCARD) => f64::NAN.to_bits() as i64,
        (Value::Na, tag::F64) => f64::NAN.to_bits() as i64,
        (Value::Na, tag::I64) => i64::MIN,
        (Value::Na, tag::BOOL3) => 1,
        (Value::Na, tag::COLOR | tag::STR | tag::REF) => -1,
        (Value::Float(x), tag::F64) => x.to_bits() as i64,
        // A dynamically-typed int landing in a float cell (a UDT field or
        // collection element whose consumers were all numeric-polymorphic):
        // the interpreter promotes ints losslessly in float contexts, so an
        // exactly-representable int rides the float lane; anything else
        // aborts into the interpreter.
        (Value::Int(i), tag::F64) => {
            let f = *i as f64;
            if f as i64 == *i && *i != i64::MIN {
                f.to_bits() as i64
            } else {
                return mismatch(value, "float");
            }
        }
        (Value::Int(i), tag::I64) if *i != i64::MIN => *i,
        (Value::Int(_), tag::I64) => {
            return Err("valid i64::MIN collides with the AOT integer na sentinel".to_string())
        }
        (Value::Bool(b), tag::BOOL3) => {
            if *b {
                2
            } else {
                0
            }
        }
        (
            Value::Color {
                r,
                g,
                b,
                t: tr,
                input_origin: None,
            },
            tag::COLOR,
        ) if tr.is_finite() && (0.0..=100.0).contains(tr) && tr.fract() == 0.0 => i64::from(
            ((u32::from(*r) << 24) | (u32::from(*g) << 16) | (u32::from(*b) << 8) | *tr as u32)
                as i32,
        ),
        (Value::String(s), tag::STR) => {
            let id = strings.len();
            strings.push(s.clone());
            i64::try_from(id).map_err(|_| "string table overflow".to_string())?
        }
        (Value::Ref(h), tag::REF) => i64::from(h.0),
        (v, tag::F64) => return mismatch(v, "float"),
        (v, tag::I64) => return mismatch(v, "int"),
        (v, tag::BOOL3) => return mismatch(v, "bool"),
        (v, tag::COLOR) => return mismatch(v, "color"),
        (v, tag::STR) => return mismatch(v, "string"),
        (v, tag::REF) => return mismatch(v, "ref"),
        (_, other) => return Err(format!("unknown cell tag {other}")),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        budget_in, check_layout, decode_cell, encode_cell, load_budget, native_status,
        needed_cells, operand_cell, save_budget, store_result, tag, vm_status, SiteSpec, VmHost,
        BUDGET_STOP_REASON, NATIVE_BUDGET_STOP_REASON,
    };
    use crate::{MarketData, RunOptions, RuntimeErrorKind, Vm};
    use pinewasm_csil::{lower_mir, validate, CsilInstr, CsilProgram};
    use pinewasm_mir::Value;
    use pinewasm_span::has_errors;

    #[test]
    fn displaced_table_cannot_be_collapsed_to_a_native_ref_cell() {
        assert!(encode_cell(
            &Value::DisplacedTable(std::num::NonZeroU64::new(7).unwrap()),
            tag::REF,
            &mut Vec::new()
        )
        .is_err());
        assert_eq!(
            encode_cell(&Value::Na, tag::REF, &mut Vec::new()).unwrap(),
            -1
        );
    }

    #[test]
    fn host_collection_growth_is_bounded_without_changing_small_allowances() {
        assert_eq!(super::host_gc_threshold(0, 50_000), 50_000);
        assert_eq!(super::host_gc_threshold(0, 16_000_000), 1_000_000);
        assert_eq!(super::host_gc_threshold(2_000_000, 17_000_000), 3_000_000);
        assert_eq!(
            super::host_gc_threshold(usize::MAX - 1, usize::MAX),
            usize::MAX
        );
    }

    /// Inline copy of the layout rejection test in `vm_call`, for comparison in the tests below.
    fn old_rejects_layout(nargs: usize, len: usize) -> bool {
        len < 1 + nargs
    }

    /// Inline copy of the budget decode in `vm_call`, for comparison in the tests below.
    fn old_budget_in(cell: i64) -> u64 {
        cell as u64
    }

    /// `check_layout` agrees with the inline copy everywhere except one class
    /// the copy wrongly admits (no operands, a result, one cell), where the
    /// result write would panic, and `nargs = usize::MAX`, where the copy's
    /// `1 + nargs` overflows.
    #[test]
    fn check_layout_matches_old_check_off_the_gap() {
        for nargs in 0..40usize {
            for len in 0..45usize {
                for has_ret in [false, true] {
                    let gap = has_ret && nargs == 0 && len == 1;
                    assert_eq!(
                        check_layout(nargs, has_ret, len),
                        !old_rejects_layout(nargs, len) && !gap,
                        "nargs={nargs} has_ret={has_ret} len={len}"
                    );
                }
            }
        }
        for (nargs, len) in [
            (usize::MAX - 1, usize::MAX),
            (usize::MAX - 1, usize::MAX - 1),
            (0, usize::MAX),
        ] {
            for has_ret in [false, true] {
                assert_eq!(
                    check_layout(nargs, has_ret, len),
                    !old_rejects_layout(nargs, len)
                );
            }
        }
        // The gap is closed: a result-producing site with no operands needs
        // two cells.
        assert!(!check_layout(0, true, 1));
        assert!(check_layout(0, true, 2));
        assert!(check_layout(0, false, 1));
        assert!(!check_layout(0, true, 0));
        assert!(!check_layout(2, false, 2));
        // No overflow: the count does not fit `usize`, so nothing is admitted.
        assert_eq!(needed_cells(usize::MAX, false), None);
        assert!(!check_layout(usize::MAX, true, usize::MAX));
        assert_eq!(needed_cells(0, true), Some(2));
        assert_eq!(needed_cells(3, true), Some(4));
        assert_eq!(needed_cells(0, false), Some(1));
    }

    #[test]
    fn budget_in_matches_old_cast() {
        let edges = [
            0i64,
            1,
            -1,
            2,
            -2,
            1_000_000,
            -1_000_000,
            i64::MAX,
            i64::MAX - 1,
            i64::MIN,
            i64::MIN + 1,
        ];
        for cell in edges {
            assert_eq!(budget_in(cell), old_budget_in(cell), "cell={cell}");
        }
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..10_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let cell = x as i64;
            assert_eq!(budget_in(cell), old_budget_in(cell));
        }
        assert_eq!(budget_in(-1), u64::MAX);
        assert_eq!(budget_in(i64::MIN), 1u64 << 63);
    }

    #[test]
    fn cell_access_kernels_match_inline_indexing() {
        let cells = [7i64, -3, 11];
        assert_eq!(operand_cell(&cells, 0), cells[1]);
        assert_eq!(operand_cell(&cells, 1), cells[2]);
        for c in [0i64, 1, -1, i64::MIN, i64::MAX, 123_456] {
            let mut buf = [c, 5];
            assert_eq!(load_budget(&buf), c as u64);
            let b = load_budget(&buf);
            save_budget(&mut buf, b);
            assert_eq!(buf, [c, 5], "budget round trip for {c}");
        }
        let mut buf = [0i64];
        save_budget(&mut buf, u64::MAX);
        assert_eq!(buf[0], -1);
    }

    #[test]
    #[should_panic(expected = "index out of bounds")]
    fn operand_cell_past_end_panics() {
        let cells = [0i64, 1];
        let _ = operand_cell(&cells, 1);
    }

    #[test]
    fn store_result_writes_cell_one() {
        let mut cells = [7i64, 8, 9];
        store_result(&mut cells, 42);
        assert_eq!(cells, [7, 42, 9]);
    }

    #[test]
    #[should_panic(expected = "index out of bounds: the len is 1 but the index is 1")]
    fn store_result_panics_on_one_cell_buffer() {
        let mut cells = [7i64];
        store_result(&mut cells, 42);
    }

    fn compile(source: &str) -> CsilProgram {
        let parsed = pinewasm_parse::parse(source);
        assert!(!has_errors(&parsed.diagnostics), "{:?}", parsed.diagnostics);
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        assert!(
            !has_errors(&analysis.diagnostics),
            "{:?}",
            analysis.diagnostics
        );
        let mir = pinewasm_mir::lower(&analysis.hir);
        let program = lower_mir(&mir).expect("CSIL lowering");
        validate(&program).expect("validation");
        program
    }

    /// A host whose only site is the zero-operand, result-producing
    /// `label.all` read (`ReadDrawingsAll`, stack effect `(0, 1)`).
    fn zero_arg_ret_host() -> VmHost {
        let program =
            compile("//@version=6\nindicator(\"t\")\na = label.all\nplot(array.size(a))\n");
        let pc = program
            .main()
            .code
            .iter()
            .position(|i| matches!(i, CsilInstr::ReadDrawingsAll { .. }))
            .expect("a label.all read");
        let close = vec![1.0, 2.0, 3.0];
        let data = MarketData {
            open: close.clone(),
            high: close.clone(),
            low: close.clone(),
            close: close.clone(),
            volume: vec![1.0; close.len()],
            ..MarketData::default()
        };
        VmHost::new(
            program,
            data,
            RunOptions::default(),
            3,
            vec![SiteSpec {
                pc: pc as u32,
                args: vec![],
                ret: Some(tag::REF),
            }],
        )
    }

    #[test]
    fn vm_call_zero_arg_ret_site_with_two_cells_succeeds() {
        let mut host = zero_arg_ret_host();
        let mut cells = [1_000_000i64, -1];
        assert_eq!(host.vm_call(0, 0, &mut cells), vm_status::OK);
        assert!(cells[1] >= 0, "a heap handle lands in cells[1]");
        assert!(cells[0] <= 1_000_000);
    }

    /// A layout check that ignored the result would admit a one-cell buffer
    /// for a zero-operand site with a result, and the result write would then
    /// index `cells[1]` out of bounds (a panic). The call is rejected before
    /// any effect, and the buffer is untouched.
    #[test]
    fn vm_call_one_cell_buffer_is_abi_mismatch() {
        let mut host = zero_arg_ret_host();
        let mut cells = [1_000_000i64];
        assert_eq!(host.vm_call(0, 0, &mut cells), vm_status::ABI_MISMATCH);
        assert_eq!(cells, [1_000_000]);
        assert_eq!(
            host.abi_mismatch(),
            Some("host site 0 needs 2 scratch cells, got 1")
        );
        assert!(host.take_error().is_none());
    }

    #[test]
    fn vm_call_empty_buffer_is_abi_mismatch() {
        let mut host = zero_arg_ret_host();
        let mut cells: [i64; 0] = [];
        assert_eq!(host.vm_call(0, 0, &mut cells), vm_status::ABI_MISMATCH);
        assert!(host.abi_mismatch().is_some());
    }

    /// Inline copy of `VmHost::vm_call`, with the call protocol written out in
    /// full (`self` renamed to `host`, and the `core` field accessed directly).
    /// `vm_call_matches_inline_reference` compares it with `vm_call`.
    fn vm_call_reference(host: &mut VmHost, site: u32, bar: u32, cells: &mut [i64]) -> i32 {
        let Some(spec) = host.core.sites.get(site as usize) else {
            host.core.mismatch = Some(format!("host site {site} out of range"));
            return vm_status::ABI_MISMATCH;
        };
        if !check_layout(spec.args.len(), spec.ret.is_some(), cells.len()) {
            host.core.mismatch = Some(format!(
                "host site {site} needs {} scratch cells, got {}",
                needed_cells(spec.args.len(), spec.ret.is_some()).unwrap_or(usize::MAX),
                cells.len()
            ));
            return vm_status::ABI_MISMATCH;
        }
        let pc = spec.pc as usize;
        if host.core.last_bar != Some(bar) {
            host.core.state.clear_alert_dedup();
            host.core.last_bar = Some(bar);
        }
        let mut args = Vec::with_capacity(spec.args.len());
        for (i, &t) in spec.args.iter().enumerate() {
            match decode_cell(operand_cell(cells, i), t, &host.core.strings) {
                Ok(v) => args.push(v),
                Err(why) => {
                    host.core.mismatch = Some(format!("site {site} arg {i}: {why}"));
                    return vm_status::ABI_MISMATCH;
                }
            }
        }
        let mut budget = load_budget(cells);
        let ret_tag = spec.ret;
        let mut vm = Vm::from_aot_state(
            &host.core.program,
            &host.core.options,
            host.core.bars,
            bar as usize,
            &mut host.core.state,
        );
        vm.stack.clear();
        for v in args {
            vm.stack.push(v);
        }
        let outcome = vm.exec_host_site(pc, &mut budget);
        let result = match outcome {
            Ok(()) => {
                if ret_tag.is_some() {
                    vm.stack.pop()
                } else {
                    None
                }
            }
            Err(_) => None,
        };
        vm.into_aot_state(&mut host.core.state);
        save_budget(cells, budget);
        match outcome {
            Ok(()) => {
                if let Some(t) = ret_tag {
                    let Some(value) = result else {
                        host.core.mismatch = Some(format!("site {site} produced no result"));
                        return vm_status::ABI_MISMATCH;
                    };
                    match encode_cell(&value, t, &mut host.core.strings) {
                        Ok(cell) => store_result(cells, cell),
                        Err(why) => {
                            host.core.mismatch = Some(format!("site {site} result: {why}"));
                            return vm_status::ABI_MISMATCH;
                        }
                    }
                }
                vm_status::OK
            }
            Err(e) => {
                host.core.error = Some(e);
                vm_status::HOST_ERROR
            }
        }
    }

    /// A linked host over a long feed that runs only a few bars sizes its
    /// typed (gradient) fill lanes to the bars it runs, not to the feed, and
    /// a reset keeps that size. Lanes are sized by the run length, not the
    /// feed length: sized by the feed, a short run over a long feed would
    /// allocate four lanes per gradient fill for every unused bar.
    #[test]
    fn typed_fill_lanes_follow_the_host_run_length_not_the_feed() {
        let program = compile(
            "//@version=6\nindicator(\"g\", overlay = true)\n\
             p1 = plot(high)\np2 = plot(low)\n\
             fill(p1, p2, high, low, color.red, color.blue)\n",
        );
        let feed = 4_096;
        let close: Vec<f64> = (0..feed).map(|bar| 100.0 + bar as f64).collect();
        let data = MarketData {
            open: close.clone(),
            high: close.iter().map(|c| c + 1.0).collect(),
            low: close.iter().map(|c| c - 1.0).collect(),
            close,
            volume: vec![1.0; feed],
            ..MarketData::default()
        };
        let options = RunOptions {
            max_bars: Some(3),
            ..RunOptions::default()
        };
        let mut host = VmHost::new(program, data, options, 3, Vec::new()).linked();
        let lanes = |host: &VmHost| -> Vec<usize> {
            host.core
                .state
                .fill_colors
                .iter()
                .flatten()
                .map(Vec::len)
                .collect()
        };
        assert_eq!(lanes(&host), vec![3; 4], "typed lanes before reset");
        host.reset();
        assert_eq!(lanes(&host), vec![3; 4], "typed lanes after reset");
    }

    /// Inline copy of `VmHost::reset`, with the body written out in full
    /// (`self` renamed to `host`, and the `core` field accessed directly).
    fn reset_reference(host: &mut VmHost) {
        // `data()` rather than the field: a parked interpreter holds it.
        let data = host.core.state.data().clone();
        host.core.state =
            crate::AotVmState::new(&host.core.program, data, &host.core.options, host.core.bars);
        host.core.strings = host.core.program.constants.clone();
        host.core.last_bar = None;
        host.core.error = None;
        host.core.mismatch = None;
    }

    /// Differential test of `reset`: after an arbitrary prefix of `vm_call`s,
    /// `reset` (through `reset_host`) and the inline copy leave hosts that
    /// agree on every observable
    /// effect, on the retained program/options/bars/sites, and on every
    /// later call; and a reset host behaves as a freshly constructed one.
    #[test]
    fn reset_matches_inline_reference_and_fresh_host() {
        let sources = [
            "//@version=6\nindicator(\"t\")\na = label.all\nplot(array.size(a))\n",
            "//@version=6\nindicator(\"STR\")\ns = str.tostring(close)\nlog.info(s + \"!\")\nplot(str.length(s))\n",
        ];
        for source in sources {
            let (program, data) = differential_host(source);
            let sites: Vec<SiteSpec> = (0..program.main().code.len())
                .map(|pc| SiteSpec {
                    pc: pc as u32,
                    args: vec![],
                    ret: Some(tag::DISCARD),
                })
                .collect();
            let new_host = || {
                VmHost::new(
                    program.clone(),
                    data.clone(),
                    RunOptions::default(),
                    data.close.len(),
                    sites.clone(),
                )
            };
            let drive = |host: &mut VmHost, rounds: usize| -> Vec<(i32, Vec<i64>)> {
                let mut out = Vec::new();
                for site in 0..rounds.min(sites.len() + 1) {
                    let mut cells = vec![1_000_000i64, 0];
                    let status = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        host.vm_call(site as u32, (site % 3) as u32, &mut cells)
                    }))
                    .unwrap_or(-1);
                    out.push((status, cells));
                }
                out
            };
            for prefix in [0usize, 1, 3, usize::MAX] {
                let mut a = new_host();
                let mut b = new_host();
                let mut fresh = new_host();
                drive(&mut a, prefix);
                drive(&mut b, prefix);
                a.reset();
                reset_reference(&mut b);
                let ctx = format!("source={source:?} prefix={prefix}");
                assert_eq!(a.core.program, b.core.program, "{ctx}");
                assert_eq!(a.core.sites, b.core.sites, "{ctx}");
                assert_eq!(a.core.bars, b.core.bars, "{ctx}");
                assert_eq!(a.core.program, fresh.core.program, "{ctx}");
                assert_eq!(a.core.sites, fresh.core.sites, "{ctx}");
                assert_eq!(a.core.bars, fresh.core.bars, "{ctx}");
                let ra = drive(&mut a, usize::MAX);
                let rb = drive(&mut b, usize::MAX);
                let rf = drive(&mut fresh, usize::MAX);
                assert_eq!(ra, rb, "{ctx}");
                assert_eq!(ra, rf, "{ctx}");
                let oa = observe(&mut a);
                assert_eq!(oa, observe(&mut b), "{ctx}");
                assert_eq!(oa, observe(&mut fresh), "{ctx}");
            }
        }
    }

    /// Everything a `vm_call` can change that a caller can observe.
    fn observe(host: &mut VmHost) -> String {
        let logs = host.drain_logs();
        let alerts = host.drain_alerts();
        let drawings = host.drain_drawings();
        format!(
            "mismatch={:?} error={:?} last_bar={:?} strings={:?} logs={logs:?} alerts={alerts:?} drawings={drawings:?}",
            host.core.mismatch, host.core.error, host.core.last_bar, host.core.strings,
        )
    }

    fn differential_host(source: &str) -> (CsilProgram, MarketData) {
        let program = compile(source);
        let close = vec![1.0, 2.5, 3.0, 2.0];
        let data = MarketData {
            open: close.clone(),
            high: close.iter().map(|c| c + 1.0).collect(),
            low: close.iter().map(|c| c - 1.0).collect(),
            close: close.clone(),
            volume: vec![1.0; close.len()],
            ..MarketData::default()
        };
        (program, data)
    }

    /// A host over `source` whose one site is its first `StrConcat`, with
    /// string operands and a string result.
    fn concat_host(source: &str) -> VmHost {
        let (program, data) = differential_host(source);
        let pc = program
            .main()
            .code
            .iter()
            .position(|i| matches!(i, CsilInstr::StrConcat))
            .expect("a string concatenation");
        let bars = data.bars();
        VmHost::new(
            program,
            data,
            RunOptions::default(),
            bars,
            vec![SiteSpec {
                pc: pc as u32,
                args: vec![tag::STR, tag::STR],
                ret: Some(tag::STR),
            }],
        )
    }

    /// Concatenate the strings at handles `a` and `b` through the host's
    /// site and return the result's handle.
    fn concat(host: &mut VmHost, a: i64, b: i64) -> i64 {
        let mut cells = [i64::MAX, a, b];
        assert_eq!(host.vm_call(0, 0, &mut cells), vm_status::OK);
        cells[1]
    }

    /// An interning host gives equal strings one handle: a produced string
    /// takes the handle of the constant with its text, or of the earlier
    /// string with its text, and a new one otherwise. A host that does not
    /// intern appends every result, as it always has.
    #[test]
    fn an_interning_host_gives_equal_strings_one_handle() {
        let source = "//@version=6\nindicator(\"t\")\na = \"ab\"\nb = \"a\"\nc = \"b\"\nd = b + c\nplot(str.length(a) + str.length(d))\n";
        let mut plain = concat_host(source);
        let constants = plain.core.strings.clone();
        let handle = |text: &str| constants.iter().position(|c| c == text).unwrap() as i64;
        let (ab, a, b) = (handle("ab"), handle("a"), handle("b"));
        assert!(
            !plain.strings_are_distinct(),
            "only an interning host knows"
        );
        let first = concat(&mut plain, a, b);
        let second = concat(&mut plain, a, b);
        assert_eq!(plain.string(first), Some("ab"));
        assert!(
            first != ab && second != first,
            "every result is a new entry"
        );

        let mut host = concat_host(source).linked();
        assert!(host.strings_are_distinct());
        assert_eq!(concat(&mut host, a, b), ab, "the constant's handle");
        assert_eq!(concat(&mut host, a, b), ab);
        let new = concat(&mut host, b, a);
        assert_eq!(host.string(new), Some("ba"));
        assert_eq!(new, constants.len() as i64, "one new entry");
        assert_eq!(concat(&mut host, b, a), new, "and its handle from then on");
        assert_eq!(host.core.strings.len(), constants.len() + 1);

        // A reset host starts from the constant pool again.
        host.reset();
        assert_eq!(host.core.strings.len(), constants.len());
        assert_eq!(concat(&mut host, b, a), constants.len() as i64);
    }

    /// The program of `source` with `reference` as whether it carries its
    /// unoptimized form: the form itself is never run here, only asked for.
    fn with_reference(source: &str, reference: bool) -> (CsilProgram, MarketData) {
        let (mut program, data) = differential_host(source);
        let main = &mut program.functions[0];
        main.budget_reference = reference.then(|| pinewasm_csil::BudgetReference {
            version: pinewasm_csil::BUDGET_REFERENCE_VERSION,
            code: main.code.clone(),
            source_map: main.source_map.clone(),
            max_stack: main.max_stack,
        });
        (program, data)
    }

    /// A linked module keeps the string budget with its host: what it
    /// reserved for its own instructions goes in with each call, and what
    /// the hosted instruction left comes back. The count is the
    /// interpreter's, so a reservation that does not fit fails with the
    /// interpreter's error.
    #[test]
    fn a_linked_host_keeps_the_string_budget_with_its_module() {
        let source = "//@version=6\nindicator(\"t\")\nd = \"ab\" + \"cd\"\nplot(str.length(d))\n";
        let host = |budget: usize| {
            let mut host = concat_host(source).linked();
            let options = RunOptions {
                max_string_bytes_per_run: budget,
                ..RunOptions::default()
            };
            host.core.options = std::sync::Arc::new(options);
            host
        };
        let constants = concat_host(source).core.strings;
        let handle = |text: &str| constants.iter().position(|c| c == text).unwrap() as i64;
        let (ab, cd) = (handle("ab"), handle("cd"));

        let mut linked = host(100);
        assert_eq!(linked.string_room(), 100);
        // The module pushed both constants, four bytes; the concatenation
        // reserves the four of its result.
        let mut cells = [i64::MAX, ab, cd];
        assert_eq!(linked.vm_call_metered(0, 0, &mut cells, 96), 92);
        assert_eq!(linked.string(cells[1]), Some("abcd"));
        assert_eq!(linked.string_room(), 92);
        // The same count going in again leaves four fewer.
        let mut cells = [i64::MAX, ab, cd];
        assert_eq!(linked.vm_call_metered(0, 0, &mut cells, 92), 88);

        // A count the module's own reservations cannot have left: more than
        // the host last reported, or less than nothing.
        for room in [89, 1 << 40, -1, i64::MIN] {
            let mut cells = [i64::MAX, ab, cd];
            assert_eq!(
                linked.vm_call_metered(0, 0, &mut cells, room),
                -i64::from(vm_status::ABI_MISMATCH),
                "{room}"
            );
            assert_eq!(
                linked.abi_mismatch(),
                Some(
                    format!("the module's string budget of {room} bytes is not this run's")
                        .as_str()
                )
            );
            assert_eq!(cells, [i64::MAX, ab, cd], "the call did not run");
            assert_eq!(linked.string_room(), 88);
        }

        // Three bytes are left when the concatenation wants four: the
        // interpreter's error, counting what the module reserved.
        let mut cells = [i64::MAX, ab, cd];
        assert_eq!(
            linked.vm_call_metered(0, 2, &mut cells, 3),
            -i64::from(vm_status::HOST_ERROR)
        );
        let error = linked.take_error().expect("the reservation's error");
        assert_eq!(error.kind, RuntimeErrorKind::StringLimitExceeded);
        assert_eq!(
            error.message,
            "run exceeded the cumulative string limit of 100 bytes (attempted 101)"
        );
        assert_eq!(error.bar_index, Some(2));

        // The budget's last byte is spendable, and a budget of nothing
        // leaves nothing.
        let mut exact = host(8);
        let mut cells = [i64::MAX, ab, cd];
        assert_eq!(exact.vm_call_metered(0, 0, &mut cells, 4), 0);
        assert_eq!(host(0).string_room(), 0);
    }

    /// The reservation a module's own instruction could not make from what
    /// it had left is the interpreter's: its error names the instruction,
    /// the bar and the bytes the run would have reserved in all.
    #[test]
    fn a_reservation_that_does_not_fit_is_the_interpreters() {
        let source = "//@version=6\nindicator(\"t\")\nd = \"ab\" + \"cd\"\nplot(str.length(d))\n";
        let mut host = concat_host(source).linked();
        host.core.options = std::sync::Arc::new(RunOptions {
            max_string_bytes_per_run: 10,
            ..RunOptions::default()
        });
        let pc = host
            .core
            .program
            .main()
            .code
            .iter()
            .position(|i| matches!(i, CsilInstr::PushStr(_)))
            .expect("a string constant");
        // Seven of the ten bytes are spent; four more do not fit.
        assert_eq!(
            host.string_reserve(pc as u32, 3, 3, 4),
            vm_status::HOST_ERROR
        );
        let error = host.take_error().expect("the reservation's error");
        assert_eq!(error.kind, RuntimeErrorKind::StringLimitExceeded);
        assert_eq!(
            error.message,
            "run exceeded the cumulative string limit of 10 bytes (attempted 11)"
        );
        assert_eq!(error.bar_index, Some(3));
        assert_eq!(error.instruction_offset, Some(pc as u32));
        assert_eq!(
            error.span,
            host.core.program.main().source_map.get(pc).copied()
        );
        // The failed reservation reserved nothing.
        assert_eq!(host.string_room(), 3);
        // A module makes the reservations that fit itself. One that reaches
        // the host and fits says the two counts differ: the run is the
        // interpreter's.
        assert_eq!(
            host.string_reserve(pc as u32, 3, 3, 3),
            vm_status::ABI_MISMATCH
        );
        assert_eq!(
            host.abi_mismatch(),
            Some("a string reservation of 3 bytes fits the budget the module found spent")
        );
        // No reservation is of fewer than no bytes.
        assert_eq!(
            host.string_reserve(pc as u32, 3, 0, -1),
            vm_status::ABI_MISMATCH
        );
        assert!(host.take_error().is_none());
    }

    /// A budget of every byte `usize` counts is none: the interpreter's sum
    /// saturates and no reservation fails, so the module is told it has the
    /// largest number left, whatever the run has reserved.
    #[test]
    fn the_largest_string_budget_is_no_budget() {
        let source = "//@version=6\nindicator(\"t\")\nd = \"ab\" + \"cd\"\nplot(str.length(d))\n";
        let mut host = concat_host(source).linked();
        host.core.options = std::sync::Arc::new(RunOptions {
            max_string_bytes_per_run: usize::MAX,
            ..RunOptions::default()
        });
        let constants = host.core.strings.clone();
        let handle = |text: &str| constants.iter().position(|c| c == text).unwrap() as i64;
        assert_eq!(host.string_room(), i64::MAX);
        // The module reserved 1,000 bytes; the concatenation reserves four.
        let mut cells = [i64::MAX, handle("ab"), handle("cd")];
        assert_eq!(
            host.vm_call_metered(0, 0, &mut cells, i64::MAX - 1_000),
            i64::MAX
        );
        assert_eq!(host.core.state.string_bytes_allocated(), 1_004);
        // However much the module reserved, the answer is the same.
        let mut cells = [i64::MAX, handle("ab"), handle("cd")];
        assert_eq!(host.vm_call_metered(0, 0, &mut cells, 0), i64::MAX);
        assert!(host.core.state.string_bytes_allocated() > 1_008);
        assert!(host.take_error().is_none() && host.abi_mismatch().is_none());
    }

    #[test]
    fn string_len_is_the_length_of_the_run_string() {
        let source = "//@version=6\nindicator(\"t\")\nd = \"héllo\" + \"\"\nplot(str.length(d))\n";
        let mut host = concat_host(source).linked();
        let constants = host.core.strings.clone();
        for (handle, text) in constants.iter().enumerate() {
            assert_eq!(
                host.string_len(handle as i32),
                text.len() as i64,
                "{text:?}"
            );
        }
        assert!(constants
            .iter()
            .any(|text| text == "héllo" && text.len() == 6));
        // `na` is the interpreter's empty message.
        assert_eq!(host.string_len(-1), 0);
        // A string a hosted instruction made has a length too.
        let a = constants.iter().position(|c| c == "héllo").unwrap() as i64;
        let made = concat(&mut host, a, a);
        assert_eq!(host.string_len(made as i32), 12);
        // No other cell names a string.
        for handle in [-2, i32::MIN, host.core.strings.len() as i32, i32::MAX] {
            assert_eq!(host.string_len(handle), -1, "{handle}");
        }
    }

    /// A history stop a linked module makes in its own code becomes the
    /// interpreter's error for it, built from the bar, the instruction and
    /// the offset the module names, and the error is the one the interpreter
    /// raises for the same request.
    #[test]
    fn a_native_history_stop_becomes_the_interpreters_error() {
        let source = "//@version=6\nindicator(\"t\")\nplot(close[bar_index])\n";
        for reference in [false, true] {
            let (mut program, data) = with_reference(source, reference);
            program.metadata.max_bars_back = 2;
            let bars = data.bars();
            let outcome = crate::run(&program, &data, &RunOptions::default());
            let expected = outcome.errors.first().expect("the interpreter stops");
            assert_eq!(expected.kind, RuntimeErrorKind::HistoryBufferLimitExceeded);
            assert_eq!(
                expected.message,
                "history offset 3 exceeds the max_bars_back limit of 2"
            );
            let host = || {
                VmHost::new(
                    program.clone(),
                    data.clone(),
                    RunOptions::default(),
                    bars,
                    Vec::new(),
                )
                .linked()
            };
            let (pc, bar) = (
                expected.instruction_offset.unwrap(),
                expected.bar_index.unwrap(),
            );
            let mut linked = host();
            assert_eq!(
                linked.native_stop(native_status::HISTORY_LIMIT, pc, bar, &[0, 3]),
                vm_status::HOST_ERROR
            );
            assert_eq!(linked.take_error().as_ref(), Some(expected));
            assert!(linked.abi_mismatch().is_none());
            // The module stops only on an offset past the limit, so any
            // other cell is no history stop, and neither is a missing one.
            for cells in [&[0i64, 2][..], &[0, -1], &[0, i64::MIN], &[0]] {
                let mut linked = host();
                assert_eq!(
                    linked.native_stop(native_status::HISTORY_LIMIT, pc, bar, cells),
                    vm_status::ABI_MISMATCH,
                    "{cells:?}"
                );
                assert_eq!(
                    linked.abi_mismatch(),
                    Some("a history stop without an offset past max_bars_back")
                );
                assert!(linked.take_error().is_none());
            }
            // Every status that is not a stop of the module's own code is
            // the module's to return as it is.
            let mut linked = host();
            for code in [
                vm_status::OK,
                vm_status::HOST_ERROR,
                vm_status::ABI_MISMATCH,
                vm_status::HEAP_REROUTE,
                10,
                -1,
            ] {
                assert_eq!(linked.native_stop(code, pc, bar, &[0, 3]), code);
            }
            assert!(linked.take_error().is_none() && linked.abi_mismatch().is_none());
        }
    }

    /// An instruction budget stop is the interpreter's to report. One the
    /// module's own code makes is always handed back. One a hosted
    /// instruction makes is the interpreter's own code's, so it stands,
    /// unless the program carries its unoptimized form: the interpreter then
    /// reports the stop where a second run stopped, which a linked host
    /// cannot know. A host that a module imports reports the hosted one as
    /// it always has.
    #[test]
    fn an_instruction_budget_stop_is_handed_back_unless_the_interpreter_made_it() {
        let source = "//@version=6\nindicator(\"t\")\nd = \"aaaaaaaaaaaa\" + \"bbbbbbbbbbbb\"\nplot(str.length(d))\n";
        let host = |reference: bool, linked: bool| {
            let (program, data) = with_reference(source, reference);
            let pc = program
                .main()
                .code
                .iter()
                .position(|i| matches!(i, CsilInstr::StrConcat))
                .expect("a string concatenation");
            let bars = data.bars();
            let host = VmHost::new(
                program,
                data,
                RunOptions::default(),
                bars,
                vec![SiteSpec {
                    pc: pc as u32,
                    args: vec![tag::STR, tag::STR],
                    ret: Some(tag::STR),
                }],
            );
            if linked {
                host.linked()
            } else {
                host
            }
        };
        let constants = host(true, true).core.strings;
        let handle = |text: &str| constants.iter().position(|c| c == text).unwrap() as i64;
        let (a, b) = (handle("aaaaaaaaaaaa"), handle("bbbbbbbbbbbb"));

        // The module's own stops, with and without the unoptimized form.
        for reference in [false, true] {
            for code in [
                native_status::BUDGET_EXCEEDED,
                native_status::RUN_BUDGET_EXCEEDED,
            ] {
                let mut linked = host(reference, true);
                assert_eq!(
                    linked.native_stop(code, 0, 1, &[0, 0]),
                    vm_status::ABI_MISMATCH
                );
                assert_eq!(linked.abi_mismatch(), Some(NATIVE_BUDGET_STOP_REASON));
                assert!(linked.take_error().is_none());
            }
        }

        // A hosted instruction's. The concatenation costs its result's
        // length, which a budget of five does not cover.
        let spend = |host: &mut VmHost| {
            let mut cells = [5, a, b];
            host.vm_call(0, 1, &mut cells)
        };
        let mut linked = host(true, true);
        assert_eq!(spend(&mut linked), vm_status::ABI_MISMATCH);
        assert_eq!(linked.abi_mismatch(), Some(BUDGET_STOP_REASON));
        assert!(linked.take_error().is_none());
        for (reference, is_linked) in [(false, true), (true, false), (false, false)] {
            let mut host = host(reference, is_linked);
            assert_eq!(
                spend(&mut host),
                vm_status::HOST_ERROR,
                "reference {reference}, linked {is_linked}"
            );
            let error = host.take_error().expect("the budget's error");
            assert_eq!(error.kind, RuntimeErrorKind::InstructionLimitExceeded);
            assert_eq!(error.bar_index, Some(1));
            assert!(host.abi_mismatch().is_none());
        }
    }

    /// A constant pool that holds a text twice cannot give equal strings one
    /// handle, and the host says so.
    #[test]
    fn a_repeated_constant_is_reported() {
        let (mut program, data) =
            differential_host("//@version=6\nindicator(\"t\")\ns = \"x\"\nplot(str.length(s))\n");
        let repeated = program.constants[0].clone();
        program.constants.push(repeated);
        let host = VmHost::new(program, data, RunOptions::default(), 4, Vec::new()).linked();
        assert!(!host.strings_are_distinct());
    }

    /// A host that a module imports has no color table: a color that a
    /// packed cell cannot hold ends the native run there, as it always has.
    /// Only a linked host answers it with a handle.
    #[test]
    fn only_a_linked_host_hands_out_color_handles() {
        let source =
            "//@version=6\nindicator(\"t\")\nc = input.color(#102030)\nplot(close, color=c)\n";
        let (program, data) = differential_host(source);
        let pc = program
            .main()
            .code
            .iter()
            .position(|i| matches!(i, CsilInstr::ReadInput { .. }))
            .expect("the color input read");
        let host = || {
            VmHost::new(
                program.clone(),
                data.clone(),
                RunOptions::default(),
                4,
                vec![SiteSpec {
                    pc: pc as u32,
                    args: Vec::new(),
                    ret: Some(tag::COLOR),
                }],
            )
        };

        let mut imported = host();
        let mut cells = [i64::MAX, 0];
        assert_eq!(imported.vm_call(0, 0, &mut cells), vm_status::ABI_MISMATCH);
        assert!(
            imported
                .abi_mismatch()
                .is_some_and(|why| why.contains("color")),
            "{:?}",
            imported.abi_mismatch()
        );

        let mut linked = host().linked();
        let mut cells = [i64::MAX, 0];
        assert_eq!(linked.vm_call(0, 0, &mut cells), vm_status::OK);
        assert_eq!(cells[1] & 0xFF, 254, "a table handle, not a packed color");
        assert_eq!(
            linked.decode(cells[1], tag::COLOR),
            Ok(Value::Color {
                r: 0x10,
                g: 0x20,
                b: 0x30,
                t: 0.0,
                input_origin: Some(0),
            })
        );
    }

    /// `input_cell` is the hosted `ReadInput` of a string or color input,
    /// encoded once: an interned handle, or a color cell that keeps the
    /// input's identity. Every other kind is the loader's.
    #[test]
    fn input_cells_hold_string_and_color_inputs() {
        // "slow" is also a constant of the program; "fast" is only the
        // input's default.
        let source = "//@version=6\nindicator(\"t\")\nn = input.int(3)\ns = input.string(\"fast\", options=[\"fast\", \"slow\"])\nc = input.color(#102030)\nplot(n + (s == \"slow\" ? 1 : 0), color=c)\n";
        let (program, data) = differential_host(source);
        let slow = program.constants.iter().position(|c| c == "slow").unwrap() as i64;
        let pool = program.constants.len() as i64;
        assert!(!program.constants.iter().any(|c| c == "fast"));
        let (int_input, string_input, color_input) = (0, 1, 2);
        let host_with = |options: RunOptions| {
            VmHost::new(program.clone(), data.clone(), options, 4, Vec::new()).linked()
        };

        let mut host = host_with(RunOptions::default());
        assert_eq!(host.input_cell(int_input), Ok(None));
        assert_eq!(host.input_cell(99), Ok(None), "no such input");
        // A default that is not a constant becomes one new entry, once.
        assert_eq!(host.input_cell(string_input), Ok(Some(pool)));
        assert_eq!(host.input_cell(string_input), Ok(Some(pool)));
        assert_eq!(host.string(pool), Some("fast"));
        let color = host.input_cell(color_input).unwrap().unwrap();
        assert_eq!(
            host.decode(color, tag::COLOR),
            Ok(Value::Color {
                r: 0x10,
                g: 0x20,
                b: 0x30,
                t: 0.0,
                input_origin: Some(color_input),
            })
        );

        // A value that is a constant takes the constant's handle, which is
        // what lets the module compare the input with that constant itself.
        let mut options = RunOptions::default();
        options.inputs.insert(
            string_input,
            pinewasm_hir::InputValue::String("slow".into()),
        );
        let mut host = host_with(options);
        assert_eq!(host.input_cell(string_input), Ok(Some(slow)));
        assert_eq!(host.core.strings.len() as i64, pool, "no new entry");
    }

    /// Differential test of `vm_call`: `vm_call` (through `vm_call_protocol`
    /// and `CallCtx`) and the inline copy agree on status, cells and every
    /// observable host effect, over every
    /// instruction of several programs as a site, several operand/result
    /// tag shapes, out-of-range sites, every buffer length up to two past the
    /// needed count (including the one-cell gap), negative/zero/large budget
    /// cells, malformed operands and bar changes. A panic in one must be a
    /// panic in the other.
    #[test]
    fn vm_call_matches_inline_reference() {
        use std::panic::{catch_unwind, AssertUnwindSafe};
        let sources = [
            "//@version=6\nindicator(\"t\")\na = label.all\nplot(array.size(a))\n",
            "//@version=6\nindicator(\"ARR\")\nvar prices = array.new_float(0)\narray.push(prices, close)\nplot(array.size(prices))\n",
            "//@version=6\nindicator(\"S\")\nplot(close, title = \"x\")\nlabel.new(bar_index, close, \"hi\")\n",
            "//@version=6\nindicator(\"STR\")\ns = str.tostring(close)\nlog.info(s + \"!\")\nplot(str.length(s))\n",
        ];
        let tag_rows: [&[u8]; 4] = [
            &[tag::F64, tag::I64, tag::BOOL3],
            &[tag::REF, tag::F64, tag::STR],
            &[tag::STR, tag::COLOR, tag::I64],
            &[tag::I64, tag::REF, tag::DISCARD],
        ];
        let budgets = [1_000_000i64, 0, 3, -1, i64::MIN];
        let operands = [0i64, 1, -1, 2, 7, i64::MIN, 1.5f64.to_bits() as i64, 99];
        let mut calls = 0usize;
        let mut panics = 0usize;
        let mut seen = [0usize; 3];
        for source in sources {
            let (program, data) = differential_host(source);
            let code_len = program.main().code.len();
            let mut sites = Vec::new();
            for pc in 0..code_len {
                let instr = &program.main().code[pc];
                let Ok(effect) = instr.resolve_stack_effect(&program.ta_states) else {
                    continue;
                };
                let pops = effect.pops as usize;
                if pops > 3 || effect.pushes > 1 {
                    continue;
                }
                for row in tag_rows {
                    let ret_tags: &[Option<u8>] = if effect.pushes == 1 {
                        &[Some(row[0]), Some(tag::DISCARD)]
                    } else {
                        &[None]
                    };
                    for &ret in ret_tags {
                        sites.push(SiteSpec {
                            pc: pc as u32,
                            args: row[..pops].to_vec(),
                            ret,
                        });
                    }
                }
            }
            let new_host = || {
                VmHost::new(
                    program.clone(),
                    data.clone(),
                    RunOptions::default(),
                    data.close.len(),
                    sites.clone(),
                )
            };
            let mut a = new_host();
            let mut b = new_host();
            for site in 0..=sites.len() {
                let needed = sites
                    .get(site)
                    .map_or(1, |s| needed_cells(s.args.len(), s.ret.is_some()).unwrap());
                for (k, &budget) in budgets.iter().enumerate() {
                    let len = (k + needed).saturating_sub(2) % (needed + 3);
                    let bar = ((site + k) / 3 % 3) as u32;
                    let mut cells: Vec<i64> = (0..len)
                        .map(|j| {
                            if j == 0 {
                                budget
                            } else {
                                operands[(site * 5 + k * 3 + j) % operands.len()]
                            }
                        })
                        .collect();
                    let mut cells_b = cells.clone();
                    let ra =
                        catch_unwind(AssertUnwindSafe(|| a.vm_call(site as u32, bar, &mut cells)));
                    let rb = catch_unwind(AssertUnwindSafe(|| {
                        vm_call_reference(&mut b, site as u32, bar, &mut cells_b)
                    }));
                    calls += 1;
                    match (ra, rb) {
                        (Ok(sa), Ok(sb)) => {
                            let ctx = format!(
                                "source={source:?} site={site} len={len} budget={budget} bar={bar}"
                            );
                            assert_eq!(sa, sb, "status: {ctx}");
                            match sa {
                                vm_status::OK => seen[0] += 1,
                                vm_status::HOST_ERROR => seen[1] += 1,
                                _ => seen[2] += 1,
                            }
                            assert_eq!(cells, cells_b, "cells: {ctx}");
                            assert_eq!(observe(&mut a), observe(&mut b), "host: {ctx}");
                        }
                        (Err(_), Err(_)) => {
                            panics += 1;
                            a = new_host();
                            b = new_host();
                        }
                        (ra, rb) => panic!(
                            "one side panicked: site={site} len={len} new={:?} ref={:?}",
                            ra.is_ok(),
                            rb.is_ok()
                        ),
                    }
                }
            }
        }
        eprintln!("calls={calls} panics={panics} ok/host_error/abi_mismatch={seen:?}");
        assert!(calls > 1000, "calls={calls}");
        assert!(
            seen.iter().all(|&n| n > 0),
            "every status is exercised: {seen:?}"
        );
        assert!(panics * 10 < calls, "panics={panics} calls={calls}");
    }

    /// Pins current behavior (not a contract): a negative budget cell decodes
    /// through `as u64` to a budget of at least 2^63, so the call runs as if
    /// unlimited and the remaining budget is written back (via `as i64`)
    /// still negative.
    #[test]
    fn vm_call_negative_budget_is_effectively_unlimited() {
        for start in [-1i64, i64::MIN] {
            let mut host = zero_arg_ret_host();
            let mut cells = [start, -1];
            assert_eq!(host.vm_call(0, 0, &mut cells), vm_status::OK);
            assert!(cells[1] >= 0);
            assert!(cells[0] < 0, "start={start} end={}", cells[0]);
            assert!(budget_in(cells[0]) <= budget_in(start));
        }
        // Zero budget, by contrast, is not unlimited.
        let mut host = zero_arg_ret_host();
        let mut cells = [0i64, -1];
        let status = host.vm_call(0, 0, &mut cells);
        assert!(status == vm_status::OK || status == vm_status::HOST_ERROR);
        assert!(cells[0] >= 0);
    }
}

#[cfg(test)]
mod integer_cell_tests {
    use super::{decode_cell, encode_cell, tag};
    use crate::Value;

    #[test]
    fn integer_cell_rejects_valid_min_but_roundtrips_na_and_admitted_boundaries() {
        let mut strings = Vec::new();
        assert!(encode_cell(&Value::Int(i64::MIN), tag::I64, &mut strings)
            .expect_err("MIN cannot be encoded as missing")
            .contains("collides"));
        for value in [
            Value::Na,
            Value::Int(i64::MIN + 1),
            Value::Int(0),
            Value::Int(i64::MAX),
        ] {
            let cell = encode_cell(&value, tag::I64, &mut strings).unwrap();
            assert_eq!(decode_cell(cell, tag::I64, &strings).unwrap(), value);
        }
    }
}

#[cfg(test)]
mod color_origin_cell_tests {
    use super::{decode_cell, encode_cell, tag};
    use crate::Value;

    #[test]
    fn packed_color_cell_refuses_to_discard_input_origin() {
        let mut strings = Vec::new();
        let tagged = Value::Color {
            r: 255,
            g: 0,
            b: 0,
            t: 0.0,
            input_origin: Some(1),
        };
        assert!(encode_cell(&tagged, tag::COLOR, &mut strings).is_err());
        let plain = Value::Color {
            r: 255,
            g: 0,
            b: 0,
            t: 0.0,
            input_origin: None,
        };
        let cell = encode_cell(&plain, tag::COLOR, &mut strings).unwrap();
        assert!(matches!(
            decode_cell(cell, tag::COLOR, &strings).unwrap(),
            Value::Color {
                input_origin: None,
                ..
            }
        ));
    }
}
