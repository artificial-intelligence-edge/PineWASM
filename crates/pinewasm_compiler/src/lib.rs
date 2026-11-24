//! PineWASM's supported Rust embedding facade.
//!
//! The facade orchestrates the complete pipeline without exposing a host to
//! its parser and lowering crates:
//!
//! ```text
//! source → parse → AST → semantics → HIR → MIR → CSIL → validate → VM
//! ```
//!
//! [`compile`] returns a validated [`Program`]. [`compile_and_run`] is the
//! shortest path from source and market data to output series, while
//! [`run_program`] revalidates and executes a cached program. The same data
//! transfer objects back the JavaScript/WebAssembly adapter.
//!
//! # Compile and run
//!
//! ```
//! use pinewasm_compiler::{
//!     compile_and_run, CompileOptions, MarketData, RunOptions, Value,
//! };
//!
//! let close = vec![10.0, 11.5, 11.0];
//! let market = MarketData {
//!     open: close.clone(),
//!     high: close.clone(),
//!     low: close.clone(),
//!     volume: vec![100.0; close.len()],
//!     close,
//!     ..MarketData::default()
//! };
//! let result = compile_and_run(
//!     "//@version=6\nindicator(\"Example\")\nplot(close)",
//!     &market,
//!     CompileOptions::default(),
//!     &RunOptions::default(),
//! );
//!
//! assert!(result.diagnostics.is_empty());
//! assert!(result.runtime_errors.is_empty());
//! assert_eq!(
//!     result.plots[0].values,
//!     vec![Value::Float(10.0), Value::Float(11.5), Value::Float(11.0)],
//! );
//! ```
//!
//! # Compile once and run more than once
//!
//! A serialized program is an untrusted cache artifact when it returns to the
//! process. [`run_program`] therefore checks its header, operands, control
//! flow, and abstract stack before execution.
//!
//! ```
//! use pinewasm_compiler::{
//!     compile, run_program, CompileOptions, MarketData, RunOptions,
//! };
//!
//! # fn main() -> Result<(), &'static str> {
//! let compiled = compile(
//!     "//@version=6\nindicator(\"Cached\")\nplot(bar_index)",
//!     CompileOptions::default(),
//! );
//! let program = compiled.program.as_ref().ok_or("compilation failed")?;
//! let market = MarketData {
//!     open: vec![1.0, 2.0],
//!     high: vec![1.0, 2.0],
//!     low: vec![1.0, 2.0],
//!     close: vec![1.0, 2.0],
//!     volume: vec![1.0, 1.0],
//!     ..MarketData::default()
//! };
//! let result = run_program(program, &market, &RunOptions::default());
//! assert!(result.runtime_errors.is_empty());
//! # Ok(())
//! # }
//! ```
//!
//! # Host data and re-exported contracts
//!
//! The facade re-exports the host-facing data needed by its entry points so an
//! embedder normally needs only `pinewasm_compiler` as a direct dependency.
//! Compiler-authored input metadata such as [`InputDecl`] can be inspected on
//! a [`Program`] and paired with host overrides in [`RunOptions::inputs`].
//! Display metadata such as [`DisplayStyle`] and [`BoundStyleScalar`] describes
//! how returned [`PlotSeries`] values should be rendered. Runtime values and
//! run-local heap handles use [`Value`] and [`HandleId`]. Compilation failures
//! carry [`Diagnostic`], [`Label`], and [`Span`] source locations, while
//! execution failures use [`RuntimeError`]. Market data, resource controls,
//! outputs, and strategy reports are likewise available from this crate root.
//!
//! These re-exports retain the item documentation from the modules that define
//! them. Their public members are part of the facade's supported pre-1.0
//! surface. Unrelated public items in the implementation crates are not.
//! [`Program::inputs`] and [`Program::ta_input_bindings`] are supported for
//! read-only host inspection through the re-exported [`InputDecl`] and
//! [`TaInputBinding`] types. Its other currently public CSIL representation
//! fields and `main` accessor remain provisional: embedders should not construct, edit, or depend on those tables. The
//! nested CSIL types needed to do so are intentionally not all promoted through
//! this facade. Obtain a program from [`compile`], persist its Serde form, and
//! pass it to the execution or artifact boundaries. Its serialization,
//! history-planning, and TA-input methods remain supported through the facade.
//!
//! # Failure model
//!
//! Source and library errors are returned as [`Diagnostic`] values. Invalid
//! cached programs, malformed market data, exhausted budgets, and script
//! failures are returned as [`RuntimeError`] values. These entry points have
//! no documented panic condition for caller-controlled source, programs,
//! market data, or run options. A panic from a caller-supplied [`NowMs`]
//! callback propagates, and process-level failures such as allocation
//! exhaustion are outside this API's recoverable error model.
//!
//! Caller-supplied cached programs are validated by `run_program` at each call.
//! Prepared programs and incremental sessions validate at admission and retain
//! immutable code for subsequent runs or ticks. For example, a
//! corrupted cache entry produces a structured failure before any bar runs:
//!
//! ```
//! use pinewasm_compiler::{
//!     compile, run_program, CompileOptions, MarketData, RunOptions,
//!     RuntimeErrorKind,
//! };
//!
//! let mut program = compile(
//!     "//@version=6\nindicator(\"Validation\")\nplot(close)",
//!     CompileOptions::default(),
//! )
//! .program
//! .expect("example source compiles");
//! program.header.magic = "corrupted-cache".to_owned();
//! let result = run_program(&program, &MarketData::default(), &RunOptions::default());
//! assert_eq!(result.bars_executed, 0);
//! assert_eq!(result.runtime_errors[0].kind, RuntimeErrorKind::InvalidProgram);
//! ```
//!
//! Runtime limits are deterministic accounting boundaries, not a measurement
//! of process resident memory. Compilation has byte-size caps but no hard CPU
//! or total-allocation sandbox; isolate the compiler at the process boundary
//! when hostile-input latency or memory must be enforced independently.
//!
//! # Determinism and timing
//!
//! For a fixed PineWASM build and target, identical source, libraries, market
//! data, and options produce deterministic semantic outputs and structured
//! failures. The ordinary native entry points include elapsed wall-clock
//! fields, so complete result values are not byte-for-byte deterministic. Use
//! the `*_profiled` entry points with `None` to omit timing fields when
//! comparing or caching results.
//! A supplied clock is called synchronously and should be finite, monotonic,
//! and non-panicking; only differences between readings are recorded.
//!
//! # Threading
//!
//! Compilation and one-shot execution use no caller-visible global mutable
//! state and may run concurrently. Programs, market data, and options are
//! `Send + Sync`. [`IncrementalSession`] owns mutable execution state: it may
//! be moved between threads, but advancing one session requires exclusive
//! access and PineWASM does not spawn worker threads on the caller's behalf.
//!
//! The repository's complete support and stability classification is in
//! `docs/rust-api.md`.
#![recursion_limit = "256"]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

#[doc(inline)]
pub use pinewasm_broker::{
    StrategyPosition, StrategyReportAggregates, StrategyReportOutliers,
    StrategyReportScopeAggregate, StrategyReportTradeHighlight, StrategyTrade,
};
#[doc(inline)]
pub use pinewasm_csil::CsilProgram as Program;
mod builtin_catalog;
mod indexed_search;
mod rendering_contract;

/// Return the versioned rendering domains, DTO fields, and ownership metadata
/// consumed by first-party rendering hosts. The record is derived from the
/// compiler's Serde models.
pub fn rendering_contract() -> serde_json::Value {
    rendering_contract::rendering_contract()
}

/// Return the builtin tables the compiler consults as one JSON document:
/// registry signatures (parameter names, order, types, qualifier ceilings,
/// requiredness and defaults), TA engine signatures, collection call surfaces,
/// drawing-call overloads, builtin series, namespaced constants, the type
/// casts, the `footprint`/`volume_row` accessors and the `strategy()`
/// declaration parameters. Calls analyzed by bespoke code without a table
/// (`request.footprint`, `library()`, `footprint.get_row_by_price`) are not
/// listed. Each table is reported under its own key, so tooling can compare
/// the tables with each other. The
/// layout is versioned by its `formatVersion` field and is not a stable host
/// contract.
pub fn builtin_catalog() -> serde_json::Value {
    builtin_catalog::builtin_catalog()
}

use pinewasm_csil::{dump_csil, lower_mir, lower_mir_validated, validate, validate_lowered};
#[doc(inline)]
pub use pinewasm_hir::builtins::{BuiltinSeries, TaInputBinding, TaInputTarget};
#[doc(inline)]
pub use pinewasm_hir::{
    ArrowRuntimeColorers, ArrowStaticColorOrigin, ArrowStaticColors, ArrowStaticRoleColor,
    BoundStyleScalar, DisplayKind, DisplayStyle, ExternalSourceRef, InputActive,
    InputActiveBinaryOp, InputActiveDirect, InputActiveExpr, InputActiveUnaryOp, InputConstraints,
    InputDecl, InputDisplay, InputKind, InputValue, InputWidget, LogLevel, MarkerLocation,
    MarkerShape, MarkerSize, OhlcBodyColor, OhlcBodyStyle, OhlcField, PlotColorRole, PlotDisplay,
    PlotLineStyle, PlotStyleKind, UnsupportedVisualOption,
};
#[doc(inline)]
pub use pinewasm_mir::{
    heap::DEFAULT_MAX_COLLECTION_ELEMENTS,
    value::{HandleId, Value},
};
use pinewasm_semantics::{ImportedLibraries, LibUnit};
use pinewasm_span::{codes, has_errors, Diagnostics};
#[doc(inline)]
pub use pinewasm_span::{Diagnostic, Label, Severity, SourceId, Span};
#[doc(inline)]
pub use pinewasm_vm::{
    AlertEvent, DrawingObject, Drawings, ExternalSourceLane, FillSeries, HostFootprint,
    HostVolumeRow, LogEntry, MarketData, PlotColorSeries, PlotSeries, PlotTickSample, PolyPointDto,
    RealtimeSim, RequestTimeframeBars, RunOptions, RuntimeError, RuntimeErrorKind, StrategyOutcome,
    StrategyRunOverrides, SymInfo, TableCellDto, Tick, UpcomingEvents, VisualSeries, VmProfile,
    DEFAULT_MAX_INSTRUCTIONS_PER_BAR, DEFAULT_MAX_INSTRUCTIONS_PER_RUN, DEFAULT_MAX_STACK_DEPTH,
    DEFAULT_MAX_STRING_BYTES_PER_RUN, DEFAULT_MAX_VM_BYTES_PER_RUN, MAX_INCREMENTAL_BARS,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Which intermediate representations to render as debug text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CompileOptions {
    /// Host chart background RGB; omitted uses the headless white default.
    pub chart_background: Option<[u8; 3]>,
    /// Host chart foreground RGB; omitted uses the headless black default.
    pub chart_foreground: Option<[u8; 3]>,
    /// Include a textual abstract-syntax-tree dump in [`CompileResult::ast_debug`].
    pub emit_ast: bool,
    /// Include a textual high-level-IR dump in [`CompileResult::hir_debug`].
    pub emit_hir: bool,
    /// Include a textual mid-level-IR dump in [`CompileResult::mir_debug`].
    pub emit_mir: bool,
    /// Keep the literal ordered-search loop. Useful for diagnosing the indexed
    /// search optimization and for targets restricted to the standalone native-heap subset.
    pub disable_distance_scan_optimization: bool,
    /// Emit the human-readable CSIL listing.
    #[serde(rename = "emitIl", alias = "emitCsil")]
    pub emit_csil: bool,
    /// Run the in-process translation validator on the CSIL optimization
    /// passes ([`pinewasm_csil::lower_mir_validated`]); a rejected pass output
    /// becomes an internal-error diagnostic. Off by default and not part of the
    /// JSON/TypeScript option surface (Rust callers and tests only). A native
    /// test run can also turn it on for every compilation by setting the
    /// environment variable `PINEWASM_VALIDATE_PASSES=1`, which is how CI runs
    /// the workspace test suite.
    #[serde(skip)]
    pub validate_passes: bool,
}

impl CompileOptions {
    /// Everything on — useful for pipeline inspection clients.
    pub fn all() -> Self {
        CompileOptions {
            chart_background: None,
            chart_foreground: None,
            emit_ast: true,
            emit_hir: true,
            emit_mir: true,
            disable_distance_scan_optimization: false,
            emit_csil: true,
            validate_passes: false,
        }
    }
}

/// Whether `PINEWASM_VALIDATE_PASSES=1` requests pass validation for every
/// compilation (see [`CompileOptions::validate_passes`]). CI sets it for the
/// workspace tests; it is never read on `wasm32`, where the variable does not
/// exist.
fn validate_passes_from_env() -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::env::var_os("PINEWASM_VALIDATE_PASSES").is_some_and(|value| value == "1")
    }
    #[cfg(target_arch = "wasm32")]
    {
        false
    }
}

/// The compile pipeline's diagnostics, optional debug views, and validated
/// program artifact.
///
/// `success` is equivalent to `program.is_some()`. Diagnostics can still
/// contain warnings on success. Debug strings are developer views rather than
/// stable machine formats; persist [`Program`] instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompileResult {
    /// True when no error-severity diagnostics were produced and a validated
    /// program is present.
    pub success: bool,
    /// Ordered parser and semantic diagnostics, including warnings on a
    /// successful compile.
    ///
    /// At most 1,000 errors and 1,000 warnings are reported, the first of
    /// each. When a compile has more, the list ends with an `E0002` (errors
    /// left out) or a `W0007` (warnings left out). Leaving diagnostics out
    /// never changes `success` or the program.
    pub diagnostics: Vec<Diagnostic>,
    /// Abstract-syntax-tree debug text when [`CompileOptions::emit_ast`] is set.
    pub ast_debug: Option<String>,
    /// Annotated high-level-IR debug text when [`CompileOptions::emit_hir`] is set.
    pub hir_debug: Option<String>,
    /// Annotated mid-level-IR debug text when [`CompileOptions::emit_mir`] is set.
    pub mir_debug: Option<String>,
    /// Human-readable CSIL listing (also gated by `emit_csil`).
    #[serde(rename = "ilText", alias = "csilText")]
    pub csil_text: Option<String>,
    /// The compiled, validated program. Serialized in full so a host can
    /// cache it and re-run without recompiling.
    pub program: Option<Program>,
    /// Per-stage wall-clock timings for this compile, present when the call
    /// supplied a clock. Native convenience entry points supply an
    /// `Instant`-backed clock, explicit profiled calls may pass `None`, and
    /// the Wasm adapter injects `performance.now`. See [`PhaseTimings`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timings: Option<PhaseTimings>,
}

/// Wall-clock milliseconds per compile stage. Stages that were skipped
/// (because an earlier stage reported errors) stay `0.0`. `dumps_ms`
/// isolates pretty-printing of requested debug dumps (AST/HIR/MIR/CSIL) so
/// IR-view overhead never masquerades as a slow stage; `total_ms` is the
/// whole compile call, so `total_ms - (sum of stages)` is glue.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PhaseTimings {
    /// Main-source parse.
    pub parse_ms: f64,
    /// Import resolution: parsing + collecting every imported library.
    pub libraries_ms: f64,
    /// Name resolution + typecheck + HIR lowering (`analyze`).
    pub semantics_ms: f64,
    /// HIR → MIR lowering.
    pub mir_ms: f64,
    /// MIR → CSIL lowering.
    #[serde(rename = "ilMs", alias = "csilMs")]
    pub csil_ms: f64,
    /// CSIL structural validation.
    pub validate_ms: f64,
    /// Pretty-printing whichever debug dumps were requested.
    pub dumps_ms: f64,
    /// The whole compile call.
    pub total_ms: f64,
}

/// A millisecond clock supplied by the host. Only differences are taken, so
/// the origin is arbitrary; it should be monotonic within one call.
/// `pinewasm_bindings` passes `performance.now` (a `std::time::Instant` read
/// traps on wasm32-unknown-unknown); native entry points default to an
/// `Instant`-backed clock. Readings are not normalized or validated: a
/// non-finite or decreasing clock produces non-finite or negative timing
/// fields, respectively. A callback panic propagates to the caller.
pub type NowMs<'a> = &'a dyn Fn() -> f64;

/// Run `f` with the platform default clock: `Instant`-backed natively,
/// `None` on wasm32 (the wasm adapter injects a JS clock explicitly).
#[cfg(not(target_arch = "wasm32"))]
fn with_default_clock<T>(f: impl FnOnce(Option<NowMs<'_>>) -> T) -> T {
    let start = std::time::Instant::now();
    let clock = move || start.elapsed().as_secs_f64() * 1000.0;
    f(Some(&clock))
}

#[cfg(target_arch = "wasm32")]
fn with_default_clock<T>(f: impl FnOnce(Option<NowMs<'_>>) -> T) -> T {
    f(None)
}

/// Phase-boundary stopwatch over an optional host clock. With no clock every
/// lap is `0.0` and `finish` yields `None`, so timing is free when disabled.
struct Stopwatch<'a> {
    clock: Option<NowMs<'a>>,
    start: f64,
    last: f64,
    timings: PhaseTimings,
}

impl<'a> Stopwatch<'a> {
    fn new(clock: Option<NowMs<'a>>) -> Self {
        let now = clock.map_or(0.0, |c| c());
        Stopwatch {
            clock,
            start: now,
            last: now,
            timings: PhaseTimings::default(),
        }
    }

    /// Milliseconds since the previous lap (or construction), and advance.
    fn lap(&mut self) -> f64 {
        let Some(clock) = self.clock else {
            return 0.0;
        };
        let now = clock();
        let elapsed = now - self.last;
        self.last = now;
        elapsed
    }

    fn finish(mut self) -> Option<PhaseTimings> {
        let clock = self.clock?;
        self.timings.total_ms = clock() - self.start;
        Some(self.timings)
    }
}

/// Compile a Pine Script® v6 source buffer.
///
/// Syntax, semantic, and defensive-size failures produce `success: false`
/// with one or more [`Diagnostic`] values. Later stages stop after an error,
/// while any requested debug view completed before that error remains in the
/// result. Successful calls return a validator-approved [`Program`].
///
/// This native convenience entry point records wall-clock timings. Call
/// [`compile_with_libraries_profiled`] with no libraries and `clock = None`
/// when the result itself must exclude nondeterministic timing fields.
///
/// # Panics
///
/// This function has no documented panic condition for caller-controlled
/// source. Process-level allocation exhaustion is not a recoverable API error.
pub fn compile(source: &str, options: CompileOptions) -> CompileResult {
    compile_with_libraries(source, &[], options)
}

/// The defensive per-source input cap. A browser tab handing us 100 MB of text
/// should get a diagnostic, not an OOM. Applied to the main source and every
/// library source alike. It is the parser's own cap, so this facade and a
/// direct parse agree on which sources are too large to read.
pub const MAX_SOURCE_LEN: usize = pinewasm_parse::MAX_SOURCE_LEN;
/// Number of caller-supplied library entries accepted by one compile.
pub const MAX_LIBRARY_COUNT: usize = 128;
/// UTF-8 bytes accepted for one import path.
pub const MAX_LIBRARY_PATH_LEN: usize = 256;
/// Aggregate UTF-8 source bytes across the caller-supplied library table.
pub const MAX_LIBRARY_SOURCE_BYTES: usize = 8 << 20;

/// Compile a main Pine Script source together with a set of importable
/// libraries. `libraries` maps an import path (`Publisher/LibName/Version`)
/// to that library's source; each `import` in `source` resolves against this map
/// **only** — there is no network. An unresolved import, a bad import path, or an
/// `export` in a non-library script is a compile diagnostic (`E0243`), never a
/// panic. `compile()` is `compile_with_libraries(source, &[], options)`, so the
/// single-source entry keeps working unchanged.
///
/// The exported functions of a resolved library are inlined at the main script's
/// `alias.fn(...)` call sites exactly like local functions (per-call-site
/// instantiation), so linking adds no new CSIL opcode or runtime
/// concept — see docs/ir.md.
///
/// # Panics
///
/// This function has no documented panic condition for caller-controlled
/// source or library buffers. Process-level allocation exhaustion is outside
/// the recoverable diagnostic model.
pub fn compile_with_libraries(
    source: &str,
    libraries: &[(String, String)],
    options: CompileOptions,
) -> CompileResult {
    with_default_clock(|clock| compile_with_libraries_profiled(source, libraries, options, clock))
}

/// [`compile_with_libraries`] with an explicit host clock, so per-stage wall
/// times land in `CompileResult::timings`. Pass `None` to skip timing
/// (`timings` stays `None`). Hosts whose clock is not `std::time::Instant`
/// (the WebAssembly adapter) enter here.
///
/// # Panics
///
/// A panic from `clock` propagates. There is no other documented panic
/// condition for caller-controlled source or library buffers; process-level
/// allocation exhaustion is outside the recoverable diagnostic model.
pub fn compile_with_libraries_profiled(
    source: &str,
    libraries: &[(String, String)],
    options: CompileOptions,
    clock: Option<NowMs<'_>>,
) -> CompileResult {
    let mut sw = Stopwatch::new(clock);
    let mut result = compile_stages(source, libraries, options, &mut sw);
    // Each stage bounds the diagnostics it reports. Reported into one list
    // they are bounded together, and that list says once, at its end, when
    // errors or warnings were left out (`E0002`, `W0007`).
    result.diagnostics = std::mem::take(&mut result.diagnostics)
        .into_iter()
        .collect::<Diagnostics>()
        .finish();
    result.timings = sw.finish();
    result
}

/// The staged pipeline body; `sw` records a lap at every stage boundary.
fn compile_stages(
    source: &str,
    libraries: &[(String, String)],
    options: CompileOptions,
    sw: &mut Stopwatch<'_>,
) -> CompileResult {
    let mut result = CompileResult {
        success: false,
        diagnostics: Vec::new(),
        ast_debug: None,
        hir_debug: None,
        mir_debug: None,
        csil_text: None,
        program: None,
        timings: None,
    };

    if source.len() > MAX_SOURCE_LEN {
        result.diagnostics.push(Diagnostic::error(
            codes::E0001,
            format!(
                "source is {} bytes; the maximum supported size is {MAX_SOURCE_LEN}",
                source.len()
            ),
        ));
        return result;
    }

    if let Some(diagnostic) = validate_library_payload(libraries) {
        result.diagnostics.push(diagnostic);
        return result;
    }

    // Parse the main source.
    let parsed = pinewasm_parse::parse(source);
    sw.timings.parse_ms = sw.lap();
    result.diagnostics.extend(parsed.diagnostics);
    if options.emit_ast {
        result.ast_debug = Some(pinewasm_syntax::dump_program(&parsed.program));
        sw.timings.dumps_ms += sw.lap();
    }
    if has_errors(&result.diagnostics) {
        return result;
    }

    // Resolve each `import` against the provided source map (no network) into a
    // flat library arena, compiling each library as an INDEPENDENT unit in its
    // own namespace (private + exported functions with bodies, top-level
    // globals, and its own resolved imports). Resolution diagnostics (a bad
    // import path, a library source that fails to parse) surface here; an
    // *unresolved* import (alias absent from the arena) is reported by the
    // analyzer as `E0243`.
    let resolved = resolve_imports(&parsed.program, libraries);
    sw.timings.libraries_ms = sw.lap();
    let libs = match resolved {
        Ok(libs) => libs,
        Err(diagnostics) => {
            result.diagnostics.extend(diagnostics);
            return result;
        }
    };

    let themed_registry =
        (options.chart_background.is_some() || options.chart_foreground.is_some()).then(|| {
            pinewasm_semantics::BuiltinRegistry::standard_ref()
                .with_chart_colors(options.chart_background, options.chart_foreground)
        });
    // Resolve + typecheck (with the resolved libraries linked in).
    let analysis = pinewasm_semantics::analyze_with_libraries(
        &parsed.program,
        themed_registry
            .as_ref()
            .unwrap_or_else(|| pinewasm_semantics::BuiltinRegistry::standard_ref()),
        libs,
    );
    sw.timings.semantics_ms = sw.lap();
    result.diagnostics.extend(analysis.diagnostics);
    if options.emit_hir {
        // Annotated: each line carries a `; span` source map for the browser's
        // line-linked IR view (the plain dump_hir is what snapshot tests pin).
        result.hir_debug = Some(pinewasm_hir::dump_hir_annotated(&analysis.hir));
        sw.timings.dumps_ms += sw.lap();
    }
    if has_errors(&result.diagnostics) {
        return result;
    }

    // Lower to MIR.
    let optimized_hir = if options.disable_distance_scan_optimization {
        std::borrow::Cow::Borrowed(&analysis.hir)
    } else {
        indexed_search::optimize(&analysis.hir)
    };
    let (mir, mir_diagnostics) = pinewasm_mir::lower_with_diagnostics(&optimized_hir);
    sw.timings.mir_ms = sw.lap();
    result.diagnostics.extend(mir_diagnostics);
    if has_errors(&result.diagnostics) {
        return result;
    }
    if options.emit_mir {
        result.mir_debug = Some(pinewasm_mir::dump_mir_annotated(&mir));
        sw.timings.dumps_ms += sw.lap();
    }

    // Lower to CSIL.
    let lowered = if options.validate_passes || validate_passes_from_env() {
        lower_mir_validated(&mir)
    } else {
        lower_mir(&mir)
    };
    let program = match lowered {
        Ok(p) => p,
        Err(e) => {
            result
                .diagnostics
                .push(internal_error(format!("CSIL lowering failed: {e}")));
            return result;
        }
    };
    sw.timings.csil_ms = sw.lap();

    // Validate. The program came from the lowering above, so the two checks
    // that would only repeat that lowering are left out.
    if let Err(errors) = validate_lowered(&program) {
        for e in errors {
            result
                .diagnostics
                .push(internal_error(format!("CSIL validation failed: {e}")));
        }
        return result;
    }
    sw.timings.validate_ms = sw.lap();

    if options.emit_csil {
        result.csil_text = Some(dump_csil(&program));
        sw.timings.dumps_ms += sw.lap();
    }
    result.program = Some(program);
    result.success = true;
    result
}

/// Reject oversized library tables before building lookup maps or parsing a
/// single library. This applies to supplied-but-unused entries too: callers
/// cannot turn the import table into an unmetered memory side channel.
fn validate_library_payload(libraries: &[(String, String)]) -> Option<Diagnostic> {
    if libraries.len() > MAX_LIBRARY_COUNT {
        return Some(Diagnostic::error(
            codes::E0001,
            format!(
                "{} libraries were provided; the maximum supported count is {MAX_LIBRARY_COUNT}",
                libraries.len()
            ),
        ));
    }

    let mut source_bytes = 0usize;
    for (path, source) in libraries {
        if path.len() > MAX_LIBRARY_PATH_LEN {
            return Some(Diagnostic::error(
                codes::E0001,
                format!(
                    "library path is {} bytes; the maximum supported size is {MAX_LIBRARY_PATH_LEN}",
                    path.len()
                ),
            ));
        }
        if source.len() > MAX_SOURCE_LEN {
            return Some(Diagnostic::error(
                codes::E0001,
                format!(
                    "library `{path}` source is {} bytes; the maximum supported size is {MAX_SOURCE_LEN}",
                    source.len()
                ),
            ));
        }
        source_bytes = source_bytes.saturating_add(source.len());
        if source_bytes > MAX_LIBRARY_SOURCE_BYTES {
            return Some(Diagnostic::error(
                codes::E0001,
                format!(
                    "library sources total {source_bytes} bytes; the maximum supported total is {MAX_LIBRARY_SOURCE_BYTES}"
                ),
            ));
        }
    }
    None
}

fn internal_error(message: String) -> Diagnostic {
    Diagnostic::error(codes::E9999, message)
        .with_note("this is a compiler bug, not an error in your script; please report it")
}

/// Resolve every `import` against the provided `libraries` map into a flat
/// arena of [`LibUnit`]s. The map is keyed by the import path
/// `Publisher/LibName/Version`. Each resolved library is compiled as an
/// INDEPENDENT unit — its **whole** namespace (every function with its body,
/// exported or not; its top-level global statements; and its own resolved
/// imports as arena indices) — so an exported function's body resolves its free
/// names against the library, never the importer. Libraries resolve
/// transitively (a library may itself `import` another), deduplicated by path so
/// the arena holds each library once; an import **cycle** terminates (the
/// in-progress path is reused rather than re-parsed). An import whose path is
/// **not** in the map produces no arena entry — the analyzer reports the missing
/// alias as `E0243`. A library whose source fails to parse, or is oversized, is
/// a hard error returned in `Err`.
///
/// Each library is parsed under a **distinct** [`SourceId`] derived from its
/// position in the `libraries` slice — `libraries[i]` is source id `i + 1`
/// (`SourceId::MAIN` == 0 is the main buffer). A library's parse/analysis
/// diagnostics therefore carry the library's own source id and byte offsets,
/// so a host can map an editor marker back to the right library buffer (see
/// docs/browser.md); they are never clamped onto the main source. A duplicate
/// import **path** in `libraries` is rejected up front (`E0243`) rather than
/// silently last-wins.
///
/// No network access: an import path absent from `libraries` is simply
/// unresolved.
fn resolve_imports(
    program: &pinewasm_syntax::Program,
    libraries: &[(String, String)],
) -> Result<ImportedLibraries, Vec<Diagnostic>> {
    let mut errors = Diagnostics::new();

    // Build the path → source map and the path → source-id map together,
    // rejecting a duplicate import path in the `libraries` slice (E0243) rather
    // than silently keeping the last one. The source id is the 1-based slice
    // position, so it is stable and host-predictable regardless of arena order.
    let mut map: HashMap<&str, &str> = HashMap::new();
    let mut src_ids: HashMap<&str, SourceId> = HashMap::new();
    for (i, (path, src)) in libraries.iter().enumerate() {
        if map.insert(path.as_str(), src.as_str()).is_some() {
            errors.push(
                Diagnostic::error(
                    codes::E0243,
                    format!("library path `{path}` was provided more than once"),
                )
                .with_note("each importable library path must be supplied exactly once"),
            );
        }
        // Reserve the id from the *first* occurrence so it is deterministic even
        // when a later duplicate is rejected.
        src_ids
            .entry(path.as_str())
            .or_insert_with(|| SourceId((i + 1) as u32));
    }
    if !errors.is_empty() {
        return Err(errors.finish());
    }

    let mut arena: Vec<LibUnit> = Vec::new();
    // Path → arena index, so a library imported through several paths (or
    // transitively) is compiled once and an import cycle terminates.
    let mut by_path: HashMap<String, usize> = HashMap::new();

    let main_aliases = resolve_import_items(
        &program.items,
        &map,
        &src_ids,
        &mut arena,
        &mut by_path,
        &mut errors,
    );

    if errors.is_empty() {
        Ok(ImportedLibraries {
            units: arena,
            main_aliases,
        })
    } else {
        Err(errors.finish())
    }
}

/// Resolve the `import` items of one script (the main script or a library) into
/// an alias → arena-index map, appending any newly-resolved library units to
/// `arena`. Shared by the main script and every library so imports resolve
/// transitively; `by_path` deduplicates and breaks cycles.
fn resolve_import_items(
    items: &[pinewasm_syntax::Item],
    map: &HashMap<&str, &str>,
    src_ids: &HashMap<&str, SourceId>,
    arena: &mut Vec<LibUnit>,
    by_path: &mut HashMap<String, usize>,
    errors: &mut Diagnostics,
) -> HashMap<String, usize> {
    use pinewasm_syntax::Item;
    let mut aliases: HashMap<String, usize> = HashMap::new();

    for item in items {
        let Item::Import(decl) = item else {
            continue;
        };
        let path = format!(
            "{}/{}/{}",
            decl.publisher.name, decl.library.name, decl.version
        );
        let Some(&lib_source) = map.get(path.as_str()) else {
            // Not in the map: the importer's alias is reported unresolved
            // (`E0243`) by the analyzer; nothing to resolve here.
            continue;
        };
        // Every library in `map` has a reserved source id; default to MAIN only
        // to stay total (unreachable in practice).
        let source_id = src_ids
            .get(path.as_str())
            .copied()
            .unwrap_or(SourceId::MAIN);
        match resolve_library(
            &path, lib_source, source_id, decl.span, map, src_ids, arena, by_path, errors,
        ) {
            Some(idx) => {
                // Duplicate alias among these imports: the analyzer reports it
                // (`E0243`); keep the first binding so resolution is stable.
                aliases.entry(decl.alias.name.clone()).or_insert(idx);
            }
            None => continue,
        }
    }
    aliases
}

/// Resolve one library by path to its arena index, compiling it (parse, collect
/// functions/globals, resolve its own imports) on first encounter and caching it
/// in `by_path`. Returns `None` only when the library source fails to parse or
/// is oversized (a hard error pushed to `errors`). Cycle-safe: the arena slot is
/// reserved and cached *before* the library's own imports are resolved, so a
/// re-entrant request for a library already in progress returns its index.
#[allow(clippy::too_many_arguments)]
fn resolve_library(
    path: &str,
    lib_source: &str,
    source_id: SourceId,
    import_span: pinewasm_span::Span,
    map: &HashMap<&str, &str>,
    src_ids: &HashMap<&str, SourceId>,
    arena: &mut Vec<LibUnit>,
    by_path: &mut HashMap<String, usize>,
    errors: &mut Diagnostics,
) -> Option<usize> {
    use pinewasm_syntax::Item;

    if let Some(&idx) = by_path.get(path) {
        return Some(idx);
    }

    if lib_source.len() > MAX_SOURCE_LEN {
        errors.push(
            Diagnostic::error(
                codes::E0243,
                format!(
                    "library `{path}` source is {} bytes; the maximum supported size is {MAX_SOURCE_LEN}",
                    lib_source.len()
                ),
            )
            .with_label(import_span, "imported here"),
        );
        return None;
    }

    // Parse the library under its own source id so every token/diagnostic span
    // carries that id and the library's byte offsets — a host maps the marker to
    // the correct library buffer, never onto the main source (docs/browser.md).
    let parsed = pinewasm_parse::parse_with_source_id(lib_source, source_id);
    if has_errors(&parsed.diagnostics) {
        // The library's diagnostics already carry the library's source id and
        // offsets, so surface them verbatim (a host renders them on that
        // buffer). Also emit a summary anchored at the import site so a host
        // that only inspects the main buffer still sees *something* went wrong.
        errors.push(
            Diagnostic::error(codes::E0243, format!("library `{path}` failed to parse"))
                .with_label(import_span, "imported here")
                .with_note("fix the library source before importing it"),
        );
        for d in &parsed.diagnostics {
            if d.severity == pinewasm_span::Severity::Error {
                errors.push(d.clone());
            }
        }
        return None;
    }

    // Reserve the arena slot and cache it before resolving this library's own
    // imports, so an import cycle (A imports B imports A) terminates.
    let idx = arena.len();
    arena.push(LibUnit::default());
    by_path.insert(path.to_string(), idx);

    let mut functions: HashMap<String, Vec<pinewasm_syntax::FuncDef>> = HashMap::new();
    let mut exported = std::collections::HashSet::new();
    let mut methods: Vec<pinewasm_syntax::FuncDef> = Vec::new();
    let mut globals: Vec<pinewasm_syntax::Stmt> = Vec::new();
    let mut types = Vec::new();
    let mut enums = Vec::new();
    for lib_item in &parsed.program.items {
        if let Item::FuncDef(def) = lib_item {
            errors.extend(pinewasm_semantics::validate_function_parameter_declarations(def));
        }
        match lib_item {
            // Every plain function is kept (private and exported alike, with
            // all overloads of a name) so a sibling/private call inside a
            // library body resolves. Only exported ones are callable through
            // an alias.
            Item::FuncDef(def) if def.receiver.is_none() => {
                // A second definition with an identical signature is a
                // redefinition, as in a main script, not another overload.
                if let Some(first) = functions.get(&def.name.name).and_then(|defs| {
                    defs.iter()
                        .find(|d| pinewasm_semantics::same_signature(d, def))
                }) {
                    errors.push(
                        Diagnostic::error(
                            codes::E0224,
                            format!("function `{}` is already defined", def.name.name),
                        )
                        .with_label(def.name.span, "redefined here")
                        .with_label(first.name.span, "first defined here"),
                    );
                    continue;
                }
                if def.exported {
                    exported.insert(def.name.name.clone());
                }
                functions
                    .entry(def.name.name.clone())
                    .or_default()
                    .push(def.clone());
            }
            // Methods link too: the library's own bodies call them, and an
            // importer calls the exported ones on its values.
            Item::FuncDef(def) => methods.push(def.clone()),
            // A library's top-level statements are its module-level globals,
            // shared across call sites when analyzed.
            Item::Stmt(stmt) => globals.push(stmt.clone()),
            Item::TypeDef(decl) => types.push(decl.clone()),
            Item::EnumDef(decl) => enums.push(decl.clone()),
            _ => {}
        }
    }

    // Resolve this library's own imports (transitive), mapping each alias to an
    // arena index.
    let imports = resolve_import_items(&parsed.program.items, map, src_ids, arena, by_path, errors);

    let bodies: Vec<&pinewasm_syntax::FuncDef> =
        functions.values().flatten().chain(&methods).collect();
    let globals = prune_library_globals(&bodies, globals);

    arena[idx] = LibUnit {
        functions,
        exported,
        methods,
        types,
        enums,
        globals,
        imports,
    };
    Some(idx)
}

/// Keep only the library globals its functions and methods can observe. Top-level
/// demonstration code, such as sample computations and `plot(...)` calls that
/// produce output when the library is loaded, is not needed by an importer. Only
/// globals that exported code depends on matter, so that display code is dropped.
/// The rule: keep a global
/// statement iff a name it assigns is (transitively) read by some library
/// function body or by another kept global. Everything else — including every
/// bare expression statement, which binds nothing — is dropped. (Limitation: a
/// global expression statement that mutates kept state through a call, e.g. a
/// top-level `array.push(g, 1)`, is also dropped; function bodies are the
/// supported place for library logic.)
fn prune_library_globals(
    bodies: &[&pinewasm_syntax::FuncDef],
    globals: Vec<pinewasm_syntax::Stmt>,
) -> Vec<pinewasm_syntax::Stmt> {
    use std::collections::HashSet;

    // A dotted read (`cfg.field`, `helper(x)`) depends on its head segment;
    // module globals are always plain names.
    fn head(name: &str) -> &str {
        name.split('.').next().unwrap_or(name)
    }

    // Names a function body reads OR reassigns (private functions included:
    // they only run via exported ones, so their dependencies must survive). A
    // `:=` inside a function targets an outer/module variable, so a global used
    // *only* as a reassignment target — `export set(v) => state := v` — is a
    // real dependency whose declaration must be kept, even though it is never
    // read there.
    let mut needed: HashSet<String> = HashSet::new();
    for def in bodies {
        let mut visit = |ident: &pinewasm_syntax::Ident| {
            needed.insert(head(&ident.name).to_string());
        };
        for stmt in &def.body {
            stmt.for_each_read_ident(&mut visit);
            stmt.for_each_reassigned_name(&mut visit);
        }
        for param in &def.params {
            if let Some(default) = &param.default {
                default.for_each_ident(&mut visit);
            }
        }
    }

    let reads: Vec<HashSet<String>> = globals
        .iter()
        .map(|s| {
            let mut set = HashSet::new();
            s.for_each_read_ident(&mut |i| {
                set.insert(head(&i.name).to_string());
            });
            set
        })
        .collect();
    let writes: Vec<HashSet<String>> = globals
        .iter()
        .map(|s| {
            let mut set = HashSet::new();
            s.for_each_assigned_name(&mut |i| {
                set.insert(head(&i.name).to_string());
            });
            set
        })
        .collect();

    // Fixpoint: keeping a statement makes its reads needed, which can keep
    // an earlier statement on the next pass.
    let mut keep = vec![false; globals.len()];
    loop {
        let mut changed = false;
        for i in 0..globals.len() {
            if keep[i] || writes[i].is_disjoint(&needed) {
                continue;
            }
            keep[i] = true;
            changed = true;
            for name in &reads[i] {
                needed.insert(name.clone());
            }
        }
        if !changed {
            break;
        }
    }
    globals
        .into_iter()
        .zip(keep)
        .filter_map(|(stmt, kept)| kept.then_some(stmt))
        .collect()
}

/// A compilation/execution response for native and WebAssembly hosts.
///
/// Compilation failures add an error-severity item to [`Self::diagnostics`]
/// and leave execution outputs empty; warnings may be present after a
/// successful compile. Execution failures populate [`Self::runtime_errors`],
/// and output collections can retain values from bars completed before the
/// abort. A call succeeds when diagnostics contain no [`Severity::Error`] and
/// runtime errors are empty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunResult {
    /// Ordered compile-stage diagnostics, including warnings on success.
    /// Direct [`run_program`] calls leave this empty.
    pub diagnostics: Vec<Diagnostic>,
    /// Errors raised during execution (instruction limits, type failures).
    pub runtime_errors: Vec<RuntimeError>,
    /// Numeric/string/color plot lanes in declaration order.
    pub plots: Vec<PlotSeries>,
    /// Color-only visual lanes such as `bgcolor` and `barcolor`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub visuals: Vec<VisualSeries>,
    /// Fill color lanes and the plot slots defining each band's geometry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fills: Vec<FillSeries>,
    /// Number of distinct feed bars committed. Appending a realtime bar
    /// increments this only when the bar closes; re-executing the already
    /// counted final historical bar does not.
    pub bars_executed: u32,
    /// Per-tick plot trace for the realtime segment, present only when
    /// `RunOptions::realtime_trace` was set and a realtime segment ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realtime_trace: Option<Vec<PlotTickSample>>,
    /// The final live drawing objects, grouped per kind. Omitted when the
    /// script creates no drawings. See docs/browser.md.
    #[serde(default, skip_serializing_if = "Drawings::is_empty")]
    pub drawings: Drawings,
    /// The recorded `log.*` event stream, in creation order. Omitted when
    /// empty. See docs/browser.md.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub logs: Vec<LogEntry>,
    /// The recorded alert event stream, in creation order. Omitted when
    /// empty. See docs/browser.md.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alerts: Vec<AlertEvent>,
    /// The strategy section: present only for `strategy(...)` scripts.
    /// Holds the broker emulator's trades, open position, per-bar equity
    /// series, and summary metrics. Omitted for indicators. See docs/browser.md.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<StrategyOutcome>,
    /// The complete compile-stage result for `compile_and_run*` calls,
    /// including its program, optional debug views, and timings. Direct
    /// [`run_program`] calls leave this as `None`.
    pub compile: Option<CompileResult>,
    /// Wall-clock milliseconds the VM spent executing (compile excluded).
    /// Present when the host supplied a clock and execution reached the VM;
    /// absent when compilation or cached-program validation stopped first.
    /// Compile-stage timings live in `compile.timings`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_ms: Option<f64>,
    /// Per-instruction profiling counters, present only when
    /// [`RunOptions::profile`] was set. See [`VmProfile`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<VmProfile>,
}

/// Compile and, if compilation succeeds, execute against `market_data`.
///
/// Invalid source is reported through [`RunResult::diagnostics`]. Malformed
/// market data, resource exhaustion, and script-raised errors are reported
/// through [`RunResult::runtime_errors`]. No VM work occurs after a compile
/// error. On an execution error, completed-bar output can be retained.
///
/// The five OHLCV vectors must have equal length. Supplied time vectors must
/// satisfy the constraints documented on [`MarketData`]. Runtime budgets come
/// from [`RunOptions`]; they are accounting limits rather than process-memory
/// or wall-clock limits.
///
/// # Panics
///
/// This function has no documented panic condition for caller-controlled
/// source, market data, or run options. Process-level allocation exhaustion is
/// not a recoverable API error.
pub fn compile_and_run(
    source: &str,
    market_data: &MarketData,
    compile_options: CompileOptions,
    run_options: &RunOptions,
) -> RunResult {
    compile_and_run_with_libraries(source, &[], market_data, compile_options, run_options)
}

/// Compile a main script against a set of importable libraries and, on
/// success, execute the result against `market_data`. The library-linking
/// counterpart of [`compile_and_run`]; see [`compile_with_libraries`] for how
/// imports resolve.
///
/// # Panics
///
/// This function has no documented panic condition for caller-controlled
/// source, libraries, market data, or run options. Process-level allocation
/// exhaustion is outside the recoverable error model.
pub fn compile_and_run_with_libraries(
    source: &str,
    libraries: &[(String, String)],
    market_data: &MarketData,
    compile_options: CompileOptions,
    run_options: &RunOptions,
) -> RunResult {
    with_default_clock(|clock| {
        compile_and_run_with_libraries_profiled(
            source,
            libraries,
            market_data,
            compile_options,
            run_options,
            clock,
        )
    })
}

/// [`compile_and_run_with_libraries`] with an explicit host clock: compile
/// stages land in `compile.timings`, VM execution in `run_ms`. Pass `None`
/// to skip timing.
///
/// # Panics
///
/// A panic from `clock` propagates. There is no other documented panic
/// condition for caller-controlled inputs; process-level allocation exhaustion
/// is outside the recoverable error model.
pub fn compile_and_run_with_libraries_profiled(
    source: &str,
    libraries: &[(String, String)],
    market_data: &MarketData,
    compile_options: CompileOptions,
    run_options: &RunOptions,
    clock: Option<NowMs<'_>>,
) -> RunResult {
    let compiled = compile_with_libraries_profiled(source, libraries, compile_options, clock);
    let Some(program) = compiled.program.as_ref() else {
        return RunResult {
            diagnostics: compiled.diagnostics.clone(),
            runtime_errors: Vec::new(),
            plots: Vec::new(),
            visuals: Vec::new(),
            fills: Vec::new(),
            bars_executed: 0,
            realtime_trace: None,
            drawings: Drawings::default(),
            logs: Vec::new(),
            alerts: Vec::new(),
            strategy: None,
            compile: Some(compiled),
            run_ms: None,
            profile: None,
        };
    };

    let run_start = clock.map(|c| c());
    let outcome = pinewasm_vm::run(program, market_data, run_options);
    let run_ms = clock.zip(run_start).map(|(c, start)| c() - start);
    RunResult {
        diagnostics: compiled.diagnostics.clone(),
        runtime_errors: outcome.errors,
        plots: outcome.plots,
        visuals: outcome.visuals,
        fills: outcome.fills,
        bars_executed: outcome.bars_executed,
        realtime_trace: outcome.realtime_trace,
        drawings: outcome.drawings,
        logs: outcome.logs,
        alerts: outcome.alerts,
        strategy: outcome.strategy,
        compile: Some(compiled),
        run_ms,
        profile: outcome.profile,
    }
}

/// Execute an already-compiled program against `market_data` — the
/// cached-artifact counterpart of [`compile_and_run`]. A host can persist a
/// [`Program`] and re-run it with new market data or input overrides without
/// recompiling, provided its CSIL version is accepted by this crate version.
///
/// The program is re-validated before execution — defense in depth, since it
/// typically crossed a host boundary (worker message, HTTP body) since the
/// compile that produced it. A validation failure is reported as a
/// structured `InvalidProgram` runtime error, never a panic. `diagnostics`
/// is empty and `compile` is `None`: compile-stage artifacts belong to the
/// compile that produced the program, which the host already has.
///
/// A malformed [`MarketData`] value is returned as
/// [`RuntimeErrorKind::MalformedMarketData`], while an integer input override
/// outside a technical-analysis function's supported length range is returned
/// as [`RuntimeErrorKind::InvalidTaLength`]. Resource and script failures can
/// retain completed-bar outputs; a validation, market-data, or invalid-length
/// failure executes zero bars and returns empty outputs.
///
/// # Panics
///
/// This function has no documented panic condition for caller-controlled
/// programs, market data, or run options. Process-level allocation exhaustion
/// is not a recoverable API error.
pub fn run_program(
    program: &Program,
    market_data: &MarketData,
    run_options: &RunOptions,
) -> RunResult {
    with_default_clock(|clock| run_program_profiled(program, market_data, run_options, clock))
}

/// [`run_program`] with an explicit host clock: VM execution time lands in
/// `run_ms`. Pass `None` to skip timing. A program rejected during validation
/// never enters the VM and therefore returns no timing value.
///
/// # Panics
///
/// A panic from `clock` propagates. There is no other documented panic
/// condition for caller-controlled inputs; process-level allocation exhaustion
/// is outside the recoverable error model.
pub fn run_program_profiled(
    program: &Program,
    market_data: &MarketData,
    run_options: &RunOptions,
    clock: Option<NowMs<'_>>,
) -> RunResult {
    let empty = |runtime_errors| RunResult {
        diagnostics: Vec::new(),
        runtime_errors,
        plots: Vec::new(),
        visuals: Vec::new(),
        fills: Vec::new(),
        bars_executed: 0,
        realtime_trace: None,
        drawings: Drawings::default(),
        logs: Vec::new(),
        alerts: Vec::new(),
        strategy: None,
        compile: None,
        run_ms: None,
        profile: None,
    };
    if let Err(errors) = validate(program) {
        let message = errors.first().map_or_else(
            || "program failed validation".to_string(),
            std::string::ToString::to_string,
        );
        return empty(vec![RuntimeError {
            kind: RuntimeErrorKind::InvalidProgram,
            message: format!("cached program rejected by the validator: {message}"),
            bar_index: None,
            instruction_offset: None,
            span: None,
        }]);
    }
    let run_start = clock.map(|c| c());
    let outcome = pinewasm_vm::run(program, market_data, run_options);
    let run_ms = clock.zip(run_start).map(|(c, start)| c() - start);
    run_result_from_outcome(outcome, run_ms)
}

/// Immutable compiled code retained for independent, fresh-state evaluations.
/// Validation happens at admission; the program cannot subsequently be mutated.
pub struct PreparedProgram {
    program: pinewasm_vm::PreparedProgram,
}

impl PreparedProgram {
    /// Validate `program` once and retain it for repeated fresh-state runs.
    ///
    /// Returns the first validator diagnostic when the code is rejected, so a
    /// prepared program can never carry code that [`compile`] would refuse.
    pub fn new(program: Program) -> Result<Self, String> {
        Ok(Self {
            program: pinewasm_vm::PreparedProgram::new(program)?,
        })
    }

    /// Reuses only immutable code. Market data, input specialization, VM,
    /// broker and all execution budgets are fresh on every call.
    pub fn run_profiled(
        &self,
        data: &MarketData,
        options: &RunOptions,
        clock: Option<NowMs<'_>>,
    ) -> RunResult {
        let start = clock.map(|c| c());
        let outcome = self.program.run(data, options);
        run_result_from_outcome(outcome, clock.zip(start).map(|(c, start)| c() - start))
    }
}

/// A validated persistent VM session for worker-owned live execution.
///
/// The compiled program and historical prefix are installed once. Each
/// [`Self::advance`] call sends only ordered tick snapshots into the VM
/// while preserving rollback, commit, broker, drawing, alert, and `varip`
/// state across calls. A session holds at most [`MAX_INCREMENTAL_BARS`]
/// resident bars.
///
/// The session is `Send` and can be moved to an owning worker thread. Mutating
/// it requires `&mut self`; PineWASM does not synchronize concurrent advances.
pub struct IncrementalSession {
    inner: pinewasm_vm::IncrementalSession,
}

impl IncrementalSession {
    /// Validate `program`, install `market_data`, and execute its confirmed
    /// historical prefix once.
    ///
    /// `run_options.realtime` is ignored because realtime work enters through
    /// [`Self::advance`]. Construction fails when the program or market data is
    /// invalid, history exceeds [`MAX_INCREMENTAL_BARS`], `max_bars` would
    /// truncate the supplied history, a resource budget is insufficient, or
    /// historical execution raises a runtime error.
    ///
    /// # Errors
    ///
    /// Returns the first structured validation, market-data, resource, or
    /// historical-execution [`RuntimeError`]. No partially initialized session
    /// escapes on failure.
    ///
    /// # Panics
    ///
    /// This function has no documented panic condition for caller-controlled
    /// inputs. Process-level allocation exhaustion is not a recoverable API
    /// error.
    pub fn new(
        program: &Program,
        market_data: &MarketData,
        run_options: &RunOptions,
    ) -> Result<Self, RuntimeError> {
        validate_cached_program(program)?;
        pinewasm_vm::IncrementalSession::new(program, market_data, run_options)
            .map(|inner| IncrementalSession { inner })
    }

    /// Snapshot the current visible result without advancing or mutating the
    /// session. Timing fields are absent because no execution occurs.
    pub fn result(&self) -> RunResult {
        run_result_from_outcome(self.inner.result(), None)
    }

    /// Apply `ticks` in order and return the current visible state.
    ///
    /// A tick can re-execute the forming final bar or append its immediate
    /// successor; invalid ordering is a structured error. A closing tick
    /// commits its bar. If any tick fails, the session becomes terminal and
    /// later calls return that same failure; rebuild it from the complete input
    /// history before continuing.
    ///
    /// # Panics
    ///
    /// This method has no documented panic condition for caller-controlled
    /// ticks. Process-level allocation exhaustion is not a recoverable API
    /// error.
    pub fn advance(&mut self, ticks: &[Tick]) -> RunResult {
        with_default_clock(|clock| self.advance_profiled(ticks, clock))
    }

    /// [`Self::advance`] with an explicit host clock.
    ///
    /// `clock = None` omits [`RunResult::run_ms`]. With a clock, this wrapper
    /// records elapsed call time even when the underlying session was already
    /// terminal and therefore performs no new VM work.
    ///
    /// # Panics
    ///
    /// A panic from `clock` propagates. There is no other documented panic
    /// condition for caller-controlled ticks; process-level allocation
    /// exhaustion is outside the recoverable error model.
    pub fn advance_profiled(&mut self, ticks: &[Tick], clock: Option<NowMs<'_>>) -> RunResult {
        let run_start = clock.map(|timer| timer());
        let outcome = self.inner.advance(ticks);
        let run_ms = clock.zip(run_start).map(|(timer, start)| timer() - start);
        run_result_from_outcome(outcome, run_ms)
    }

    /// Apply `ticks` in order and return one [`PlotTickSample`] per tick
    /// instead of the whole visible history.
    ///
    /// [`Self::advance`] answers with every plot lane from bar zero, so its
    /// cost grows with the resident history even when a tick touches a single
    /// bar. A host that only repaints the forming bar wants this entry point:
    /// it runs the identical tick path and returns one value per plot slot, in
    /// slot order, after each tick, without copying a plot lane.
    ///
    /// Tick ordering, commit semantics, and terminal failure are identical to
    /// [`Self::advance`]; only the shape of the answer differs. Call
    /// [`Self::result`] when the full series is wanted afterwards.
    ///
    /// # Errors
    ///
    /// Returns the structured [`RuntimeError`] that failed this advance, or the
    /// one that already made the session terminal.
    ///
    /// # Panics
    ///
    /// This method has no documented panic condition for caller-controlled
    /// ticks. Process-level allocation exhaustion is not a recoverable API
    /// error.
    pub fn advance_samples(&mut self, ticks: &[Tick]) -> Result<Vec<PlotTickSample>, RuntimeError> {
        self.inner.advance_samples(ticks)
    }

    /// Return the number of distinct committed feed bars.
    pub fn committed_bars(&self) -> u32 {
        self.inner.committed_bars()
    }

    /// Return the number of bars currently resident, including an open forming
    /// realtime bar when present.
    pub fn resident_bars(&self) -> usize {
        self.inner.resident_bars()
    }

    /// Return whether an advance failed and made the session terminal.
    pub fn is_failed(&self) -> bool {
        self.inner.is_failed()
    }
}

fn validate_cached_program(program: &Program) -> Result<(), RuntimeError> {
    validate(program).map_err(|errors| {
        let message = errors.first().map_or_else(
            || "program failed validation".to_string(),
            ToString::to_string,
        );
        RuntimeError {
            kind: RuntimeErrorKind::InvalidProgram,
            message: format!("cached program rejected by the validator: {message}"),
            bar_index: None,
            instruction_offset: None,
            span: None,
        }
    })
}

fn run_result_from_outcome(outcome: pinewasm_vm::RunOutcome, run_ms: Option<f64>) -> RunResult {
    RunResult {
        diagnostics: Vec::new(),
        runtime_errors: outcome.errors,
        plots: outcome.plots,
        visuals: outcome.visuals,
        fills: outcome.fills,
        bars_executed: outcome.bars_executed,
        realtime_trace: outcome.realtime_trace,
        drawings: outcome.drawings,
        logs: outcome.logs,
        alerts: outcome.alerts,
        strategy: outcome.strategy,
        compile: None,
        run_ms,
        profile: outcome.profile,
    }
}

/// The PineWASM compiler release version, from `crates/pinewasm_compiler/VERSION`.
///
/// This is the one product version hosts show and record. It follows Semantic
/// Versioning and every change to compiler sources bumps it
/// (`cargo xtask bump-version`, enforced by
/// `cargo xtask check-compiler-version`). Crate versions in `Cargo.toml` are
/// internal and stay fixed so a release does not churn `Cargo.lock` or the
/// reproducible AOT runtime modules.
pub const COMPILER_VERSION: &str = include_str!("../VERSION").trim_ascii();

/// Version metadata surfaced to hosts (`version()` in the wasm API).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionInfo {
    /// Semantic version of the compiler release ([`COMPILER_VERSION`]).
    pub compiler_version: String,
    /// CSIL wire-format version accepted and emitted by this compiler.
    #[serde(rename = "ilVersion", alias = "csilVersion")]
    pub csil_version: u32,
    /// Pine Script source-language version targeted by this compiler.
    pub language_version: u32,
    /// Rendering contract version: the `schemaVersion` of
    /// [`rendering_contract`] and the `rendering_contract_version` of every
    /// program this compiler emits. A host compares it at load, before it
    /// renders any output.
    pub rendering_contract_version: u32,
    /// The IANA Time Zone Database release the compiler and interpreter
    /// read timezone names with ([`pinewasm_tz::TZDATA_VERSION`], e.g.
    /// `"2026e"`). A new release is a new compiler release.
    #[serde(default)]
    pub tzdata_version: String,
    /// Immutable source revision embedded by a release build. Local/dirty
    /// builds omit it rather than claiming an inexact source tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<String>,
}

/// Return compiler, CSIL, language, rendering contract, and optional
/// source-revision metadata.
///
/// `source_revision` is present only when the build set a non-empty
/// `PINEWASM_SOURCE_REVISION` environment variable at compile time.
pub fn version_info() -> VersionInfo {
    VersionInfo {
        compiler_version: COMPILER_VERSION.to_string(),
        csil_version: pinewasm_csil::CSIL_VERSION,
        language_version: pinewasm_csil::LANGUAGE_VERSION,
        rendering_contract_version: pinewasm_csil::RENDERING_CONTRACT_VERSION,
        tzdata_version: pinewasm_tz::TZDATA_VERSION.to_string(),
        source_revision: option_env!("PINEWASM_SOURCE_REVISION")
            .map(str::trim)
            .filter(|revision| !revision.is_empty())
            .map(str::to_owned),
    }
}
