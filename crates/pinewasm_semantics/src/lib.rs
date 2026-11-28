//! Semantic analysis: name resolution, type and qualifier checking, and
//! AST → HIR lowering.
//!
//! The analyzer is designed to return a (possibly partial) [`HirProgram`] plus
//! diagnostics for malformed input. A panic is a defect rather than a
//! supported outcome.
//! Callers must gate later pipeline stages on the absence of error
//! diagnostics.
#![forbid(unsafe_code)]

mod arg_order;
mod binder;
mod branch_prefixes;
mod calls;
mod collections;
mod const_color_accessors;
mod drawings;
mod exprs;
mod inputs;
mod outputs;
mod persistent_writes;
mod registry;
mod request_prefix;
mod requests;
mod stmts;
mod suggestions;
mod ta;

pub use registry::{
    generic_input_source_params, BuiltinConstant, BuiltinFunction, BuiltinFunctionKind,
    BuiltinRegistry, DisplayKind, ParamSpec, ParamTypeRule, PureResult, ScalarTy, TaParam,
    TaParamKind, GENERIC_INPUT_SOURCE_PARAM_NAMES, PIVOT_POINT_LEVELS_PARAM_NAMES,
    SECURITY_LOWER_TF_PARAM_NAMES, SECURITY_PARAM_NAMES, TIMESTAMP_DATE_STRING_PARAM_NAMES,
    TIMESTAMP_TZ_PARAM_NAMES, TIMESTAMP_UTC_PARAM_NAMES, TIME_FUNCTION_PARAM_NAMES,
    VWAP_PARAM_NAMES,
};

use pinewasm_hir::drawing::{CellAttr, DrawDefault, DrawKind};
use pinewasm_hir::{
    input_active_call_is_supported, BinOp, BoundStyleScalar, BuiltinSeries, CollectionFunc,
    DisplayKind as OutputDisplayKind, DisplayStyle, FillInfo, FillSlot, HirExpr, HirExprKind,
    HirProgram, HirStmt, HtfExpr, IndicatorInfo, InputActive, InputActiveBinaryOp,
    InputActiveDirect, InputActiveExpr, InputActiveUnaryOp, InputConstraints, InputDecl,
    InputDisplay, InputKind, InputValue, InputWidget, LogLevel, MarkerLocation, MarkerShape,
    MarkerSize, ObjectTypeDecl, OhlcField, OrderKind, PlotColorRole, PlotDisplay, PlotInfo,
    PlotLineStyle, PlotSlot, PlotStyleKind, PureFunc, Qualifier, RequestFunc, SeriesSlot,
    StrategyInfo, Symbol, SymbolId, TaFunc, TaStateDecl, Type, UnOp, UnsupportedVisualOption,
    VisualInfo, VisualSlot, MAX_INPUT_ACTIVE_STRING_BYTES, STRATEGY_DECL_PARAMS,
    STR_FORMAT_MAX_ARGS,
};
use pinewasm_span::{codes, Diagnostic, Diagnostics, Span};
use pinewasm_syntax as ast;
use std::collections::{HashMap, HashSet};
mod arrow_runtime_colorers;
mod arrow_static_colors;
mod automatic_colors;
mod ohlc_body_style;
mod rendering_inline_provenance;
mod temporal_fixedness;

pub struct AnalysisResult {
    pub hir: HirProgram,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn analyze(program: &ast::Program) -> AnalysisResult {
    analyze_with_registry(program, BuiltinRegistry::standard_ref())
}

pub fn analyze_with_registry(program: &ast::Program, registry: &BuiltinRegistry) -> AnalysisResult {
    analyze_with_libraries(program, registry, ImportedLibraries::default())
}

/// Analyze a main script together with its linked libraries. `libs` is the
/// library arena the compiler built from the provided source map (no network).
/// A call `alias.fn(...)` in the main script inlines the exported function, but
/// the exported function's body resolves its free names against the *library's*
/// namespace, so an importer can never hijack a library-internal name.
///
/// The diagnostics hold at most [`pinewasm_span::MAX_ERRORS`] errors and
/// [`pinewasm_span::MAX_WARNINGS`] warnings. When there are more, an `E0002`
/// or a `W0007` after them says that the rest are left out; the analysis is
/// the same either way.
pub fn analyze_with_libraries(
    program: &ast::Program,
    registry: &BuiltinRegistry,
    libs: ImportedLibraries,
) -> AnalysisResult {
    analyze_into(program, registry, libs, Diagnostics::new())
}

/// [`analyze_with_libraries`], reporting into `diagnostics`. A test passes a
/// list with other limits to check that nothing but the list depends on
/// them.
fn analyze_into(
    program: &ast::Program,
    registry: &BuiltinRegistry,
    libs: ImportedLibraries,
    diagnostics: Diagnostics,
) -> AnalysisResult {
    let mut analyzer = Analyzer::new(registry, libs);
    analyzer.diagnostics = diagnostics;
    let before_analysis = analyzer.diagnostics.mark();
    analyzer.seed_drawing_types();
    analyzer.seed_footprint_types();
    // After seeding (so built-in object receivers such as `line` resolve) and
    // before the main script's own types are collected.
    analyzer.collect_library_types();
    analyzer.qualify_library_signatures();
    analyzer.collect_library_methods();
    let mut body = analyzer.analyze_program(program);
    analyzer.check_unevaluated_defaults();
    let max_bars_back = analyzer.effective_max_bars_back();
    // Prepend every used library's global-init statements so a
    // library-scoped global's slot is initialized before any call site — in the
    // main body or in another library — reads it.
    if !analyzer.lib_global_stmts.is_empty() {
        let mut prefixed = std::mem::take(&mut analyzer.lib_global_stmts);
        prefixed.append(&mut body);
        body = prefixed;
    }
    analyzer.finalize_temporal_fixedness(&mut body);
    // A script with an error may hold a request that was rejected after its
    // expression lowered, so only a clean analysis says what its requests keep.
    if !analyzer.diagnostics.errors_since(before_analysis) {
        analyzer.report_request_default_lengths();
    }
    analyzer.finalize_automatic_colors(&body);
    analyzer.finalize_arrow_static_colors(&body);
    analyzer.finalize_arrow_runtime_colorers(&body);
    analyzer.finalize_ohlc_body_style(&body);
    AnalysisResult {
        hir: HirProgram {
            indicator: analyzer.indicator,
            strategy: analyzer.strategy,
            body,
            max_bars_back,
            symbols: analyzer.symbols,
            plots: analyzer.plots,
            visuals: analyzer.visuals,
            fills: analyzer.fills,
            ta_states: analyzer.ta_states,
            ta_input_bindings: analyzer.ta_input_bindings,
            inputs: analyzer.inputs,
            object_types: analyzer.object_types,
        },
        diagnostics: analyzer.diagnostics.finish(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConditionalStatefulContext {
    LazyBoolean,
    Ternary,
}

/// A resolved `fill` endpoint whose plot slot holds only while the `var` or
/// `varip` symbol `root` is never reassigned.
#[derive(Debug, Clone)]
struct PersistentFillEndpoint {
    root: SymbolId,
    /// The `fill` argument, for the diagnostic.
    span: Span,
    /// The parameter name (`plot1`, `hline2`, ...).
    param: &'static str,
    /// `plot` or `hline`.
    call: &'static str,
}

struct Analyzer<'r> {
    registry: &'r BuiltinRegistry,
    diagnostics: Diagnostics,
    symbols: Vec<Symbol>,
    /// Lexical scopes mapping names to symbols. Index 0 is the global scope;
    /// `if` branches push and pop nested scopes.
    scopes: Vec<HashMap<String, SymbolId>>,
    plots: Vec<PlotInfo>,
    /// `color.new`/`color.rgb` with non-const operands, or any `color.from_gradient`.
    dynamic_color_calculation: bool,
    visuals: Vec<VisualInfo>,
    fills: Vec<FillInfo>,
    /// Compile-time plot IDs bound by assignments such as `p = plot(close)`.
    plot_handles: HashMap<SymbolId, PlotSlot>,
    /// For a handle copied through per-bar aliases (`q = p`), the `var` and
    /// `varip` symbols it was read from. Such a source keeps a later `:=`
    /// across bars, so reassigning it anywhere in the script changes what the
    /// alias names from the next bar on.
    plot_handle_roots: HashMap<SymbolId, Vec<SymbolId>>,
    /// `fill` endpoints that resolved through a `var`/`varip` symbol, keyed
    /// by that symbol. A `:=` of the symbol later in the source reports each
    /// one (`E0205`), since the band would otherwise keep the first-bar slot.
    persistent_fill_endpoints: Vec<PersistentFillEndpoint>,
    /// True while analyzing the final expression of a value-producing block.
    block_tail: bool,
    /// A direct function call used only for effects by an expression statement.
    discarded_call: Option<Span>,
    indicator: Option<IndicatorInfo>,
    /// The `strategy(...)` declaration, when the script declared one.
    /// Mutually exclusive with `indicator` (`E0240`).
    strategy: Option<StrategyInfo>,
    /// The history ceiling the script has asked for so far. `Some(n)` once
    /// something requests a buffer of `n` bars: `indicator(max_bars_back=n)`,
    /// a top-level `max_bars_back(source, n)` call, or (settled at the end of
    /// analysis, see [`Self::check_max_bars_back_offsets`]) a literal history
    /// offset deeper than every other request. `None` means nothing has, and
    /// the default ceiling [`MAX_BARS_BACK_DEFAULT`] applies. When requests
    /// disagree, the larger wins, so a script asking for more buffer gets it.
    max_bars_back: Option<u32>,
    /// Every literal history offset seen during analysis, paired with its
    /// source span, kept until the whole program is analyzed. Each one is a
    /// buffer requirement known at compile time, and a
    /// `max_bars_back(source, n)` call or the `indicator(max_bars_back=n)`
    /// declaration may appear anywhere in the source, so the ceiling they
    /// settle together is only final once every item has been visited.
    max_bars_back_offsets: Vec<(u32, Span)>,
    /// The "did you mean" searches of this analysis, and the bound on the
    /// work they may do.
    suggestions: suggestions::Suggestions,
    /// Number of synthetic (hidden) symbols allocated so far; used to mint
    /// the non-colliding `%h0`, `%h1`, ... names for
    /// [`HirExprKind::MaterializeHistory`] slots.
    synthetic_slots: u32,
    /// Number of `for … in` loops desugared so far; used to mint the
    /// non-colliding `%forin_iterN` / `%forin_iN` synthetic names.
    forin_counter: u32,
    /// Number of `timeframe.change` calls lowered so far; used to mint the
    /// non-colliding `%tfcN_*` synthetic names, unique even when one call
    /// is nested in another's timeframe.
    timeframe_change_counter: u32,
    /// Number of chained method-call receivers bound so far; used to mint the
    /// non-colliding `%recvN` synthetic names (method chaining).
    method_recv_counter: u32,
    /// Engine-backed stateful call sites declared so far, in syntactic
    /// order; each successful site allocates the next `TaStateId`.
    ta_states: Vec<TaStateDecl>,
    ta_input_bindings: Vec<pinewasm_hir::TaInputBinding>,
    ta_int_inputs: HashMap<SymbolId, (u32, bool)>,
    /// `input.*` declaration sites, in syntactic order; each successful site
    /// allocates the next input id.
    inputs: Vec<InputDecl>,
    /// Source-proven static property transparency, separate from raw input defaults.
    input_color_property_transparencies: HashMap<u32, f64>,
    /// Single-analysis const color provenance, consumed by an exact input declaration.
    const_input_property_transparencies: HashMap<Span, f64>,
    /// Scalar values of ordinary const-qualified `=` aliases, keyed by their
    /// symbol. Input metadata is compile-time data, so documented const
    /// arguments must be able to see through aliases without weakening the
    /// qualifier requirement to input/simple/series.
    const_input_values: HashMap<SymbolId, InputValue>,
    /// Symbols whose cached integer value depends on an input's default.
    ints_from_input_defaults: HashSet<SymbolId>,
    /// Per run-time `ta.ema` call site whose length is computed from inputs:
    /// the length those inputs' defaults give. A `request.security` compound
    /// has no run-time lengths, so its secondary context keeps this value.
    input_default_ema_lengths: HashMap<SeriesSlot, i64>,
    /// `ta.ema` length arguments that a `request.*()` expression lowered with
    /// the length the input defaults give. Lowering can visit one site more
    /// than once, so each is reported once (`W0010`) when analysis ends.
    request_default_ema_lengths: std::cell::RefCell<Vec<(Span, i64)>>,
    /// Arguments that already carry a `W0010`. An inlined helper is analyzed
    /// once per call, and each call would repeat the same warning.
    input_default_warnings: HashSet<(Span, i64)>,
    /// Input-display verdict (`input_display_bits`) of the initializer of a
    /// const `plot_display` alias. The folded set alone cannot tell
    /// `display.all - display.data_window - display.status_line` (valid for
    /// inputs) from `display.pane + display.price_scale` (plot-only).
    held_input_display_bits: HashMap<SymbolId, u8>,
    default_display_masks: HashMap<SymbolId, u8>,
    /// Run-time display set operations, keyed by the hidden slot holding the
    /// left operand: `true` for a union (`+`), `false` for a difference
    /// (`-`). Lets an input-bound display binding apply the operation to its
    /// operands' bindings (see `runtime_display_set_op`).
    display_set_ops: HashMap<SymbolId, bool>,
    /// Whole-percent const accessor provenance; raw color values stay intact.
    const_color_transparencies: HashMap<SymbolId, f64>,
    /// Transitive const provenance excluding any persistent/input/series read.
    const_accessor_stable_symbols: HashSet<SymbolId>,
    /// Parameter symbols of the user functions being inlined. A function may
    /// not reassign its parameters.
    function_parameters: HashSet<SymbolId>,
    /// Plot counts used so far, by this compiler's plot-count rules (see
    /// [`Self::add_plot_counts`]), and the call that first went over
    /// [`MAX_PLOT_COUNTS`].
    plot_counts: u32,
    plot_count_overflow: Option<Span>,
    /// Inlined argument slots retain a separate arrow-colorer boundary.
    /// This does not change function qualifiers or executable VM expressions.
    arrow_inline_parameters: HashSet<SymbolId>,
    /// Source destructuring targets lose their tuple origin when user-function
    /// tuple elements lower to independent Assign statements. OHLC metadata
    /// retains that origin without changing executable HIR or qualifiers.
    ohlc_destructure_targets: HashSet<SymbolId>,
    /// Per-lowered-call metadata only; never enters executable HIR or CSIL.
    rendering_inline: rendering_inline_provenance::InlineProvenance,
    /// Source-only history provenance for rendering metadata. A direct `[0]`
    /// read shares the runtime HIR with a current read, but arrow styles
    /// distinguish explicit history from an effective current constant.
    /// Span includes its `SourceId`; this is never serialized into HIR or CSIL.
    arrow_history_spans: HashSet<Span>,
    /// Explicit variable qualifiers constrain later writes as well as initializers.
    /// Each entry also records the conditional-branch depth of the declaration.
    declared_qualifiers: HashMap<SymbolId, (Qualifier, u32)>,
    /// Direct provenance for variables assigned from an input declaration.
    /// This is analyzer-only metadata used to turn `active = enabled` (whose
    /// HIR is a `ReadVar`) back into the input dependency hosts need. Aliases
    /// and a single logical inversion retain provenance; `:=` clears it.
    input_origins: HashMap<SymbolId, InputActiveDirect>,
    /// Complete host-evaluable scalar input expressions assigned to ordinary
    /// variables. This lets a later `active = derivedFlag` recover more than
    /// the direct-input provenance retained by `input_origins`.
    input_active_origins: HashMap<SymbolId, InputActiveExpr>,
    /// Compiler-only proof that an active origin depends on source literals,
    /// not merely on a host-evaluable literal representation. Never infer this
    /// fact from a serialized Value or an empty input-dependency list. Symbol
    /// ids survive inlined returns and library scope swaps; source writes
    /// invalidate the proof alongside the corresponding active origin.
    source_closed_active_origins: HashSet<SymbolId>,
    /// Input-stable expressions that are supported only for request context
    /// selectors. This separate map keeps request-specific conversions (such
    /// as `str.tostring(input.int(...))`) out of the general host-active ABI.
    request_input_expr_origins: HashMap<SymbolId, InputActiveExpr>,
    /// Numeric expressions assigned once outside conditional control flow.
    /// Requests re-evaluate these expressions in their own timeframe rather
    /// than reading the chart-context value of an ordinary series alias.
    htf_expr_origins: HashMap<SymbolId, HirExpr>,
    /// Provenance for ordinary variables and inlined function parameters that
    /// refer to `syminfo.ticker` or `syminfo.tickerid`. Their runtime values
    /// are real host metadata, so request analysis cannot identify the chart's
    /// own feed by comparing opaque constant-string tokens.
    current_feed_symbols: HashMap<SymbolId, BuiltinSeries>,
    /// Variables known to carry the chart's `timeframe.period`. This value is
    /// resolved against `MarketData` at runtime, not frozen during analysis.
    current_chart_timeframes: HashSet<SymbolId>,
    /// True while analyzing the initializer of a `var` declaration. A `var`
    /// initializer executes only on the first bar, so a series-materializing
    /// construct inside it records a single bar of history (`W0004`).
    in_var_initializer: bool,
    /// Depth of enclosing loop bodies. `break`/`continue` are valid only when
    /// this is > 0 (`E0223` otherwise), and stateful `ta.*` calls / compound
    /// history materialization inside a loop body are a compile error
    /// (`E0222`): per-call-site state assumes once-per-bar execution.
    loop_depth: u32,
    /// Symbols that name a loop counter; assigning to one with `:=` is
    /// `E0217` ("cannot reassign the loop counter"). Loop counters are not
    /// removed when their loop's scope ends — the scope removal already hides
    /// the name from lookup — so this vector only grows.
    loop_counters: Vec<SymbolId>,
    /// Control-flow statements produced by desugaring an `if`/`switch`
    /// **expression** (each stores the taken branch's value into a hidden
    /// result slot). They must run *before* the statement whose expression
    /// contains the `if`/`switch`, so the statement-collection loops drain
    /// this buffer ahead of the statement's own HIR. Accumulated in
    /// evaluation order within a single statement; always empty at statement
    /// boundaries.
    pending_stmts: Vec<HirStmt>,
    /// The arguments of the calls being analyzed, innermost last, so an
    /// argument followed by one that hoists statements is captured in
    /// parameter order (see `arg_order.rs`).
    arg_order_frames: Vec<arg_order::ArgOrderFrame>,
    /// Nesting depth of `analyze_expr_expecting`; matches an argument to the
    /// frame of the call it belongs to.
    arg_order_depth: u32,
    /// True where captures must not run (see `arg_order.rs`).
    arg_order_suppressed: bool,
    /// User-defined functions, keyed by name. Each name holds its overloads
    /// in declaration order (Pine lets definitions share a name when their
    /// parameter counts or types differ); a call picks one with
    /// [`Self::select_overload`]. Collected from the top-level `FuncDef` items
    /// before any statement is analyzed, so a call may reference a function
    /// defined later in the source. Each call site monomorphizes (inlines) the
    /// stored AST template afresh (per-call-site instantiation), so no function
    /// or call frame survives into HIR.
    functions: HashMap<String, Vec<ast::FuncDef>>,
    /// The spans of the typed parameter defaults that a call has reached
    /// because it omitted the argument. That call holds the default to the
    /// declared type; [`Self::check_unevaluated_defaults`] does it for the
    /// defaults no call reached.
    evaluated_defaults: HashSet<Span>,
    /// Names of the functions currently being inlined, innermost last. A
    /// callee already on this stack is direct or mutual recursion (`E0224`);
    /// otherwise a function may call other (acyclic) functions, expanded
    /// transitively.
    call_stack: Vec<String>,
    /// First scope owned by each active inlined function, or by a default
    /// argument being evaluated in its definition's scope. Inside it, name
    /// lookup sees the scopes from this base up and then the global scope
    /// (`scopes[0]`); the scopes in between belong to call sites and stay
    /// hidden ([`Self::lookup_variable`]). Parameter and local bindings may
    /// retain their existing reassignment behavior; a resolved binding below
    /// the base is a global of the script or library and may not be rebound by
    /// the function. Bases are recorded after library scope swaps and
    /// suspended while separately initializing library globals.
    function_scope_bases: Vec<FunctionBoundary>,
    /// Count of body statements expanded across all call-site instantiations.
    /// A hard cap (`MAX_EXPANSION_NODES`) turns a blow-up bomb (deep or
    /// fan-out call chains) into a structured diagnostic instead of a hang or
    /// OOM.
    expansion_nodes: usize,
    /// Set once the expansion budget is exceeded, so the structured
    /// diagnostic is reported exactly once and further call-site expansions
    /// short-circuit (no cascade of identical errors, no runaway work).
    expansion_aborted: bool,
    /// Units of analyzer recursion in progress: one per expression level, two
    /// per compound statement or method call, across inlined function bodies.
    /// Bounded by `MAX_ANALYSIS_NESTING`.
    nesting: u32,
    /// Set once the nesting budget is exceeded, so the diagnostic is reported
    /// once and not at every sibling that also reaches the limit.
    nesting_reported: bool,
    /// Set by `analyze_if` just before it analyzes an `else if` block, so that
    /// the single nested `if` it holds is charged as an `else if` link.
    else_if_link: bool,
    /// Depth of genuine **conditional-branch** contexts (`if`/`switch`
    /// statement and expression branches, loop bodies). Drives `W0004` and the
    /// `:=` path-dependence join. Distinct from scope depth: function-inlining
    /// pushes param/body scopes that are *not* conditional, so a stateful
    /// construct in a function called at top level does not warn, while one in
    /// a function called from a branch does (the instantiation inherits the
    /// branch context).
    branch_depth: u32,
    /// If/switch controls unresolved from source, independently of per-bar
    /// scheduling. Non-dynamic requests also reject input or opaque simple
    /// controls, while source-closed controls and sole fallbacks remain
    /// unconditional. Inlined helpers inherit the caller's request context.
    request_branch_depth: u32,
    /// Enclosing if/switch conditions, distinct from series-only warning depth.
    /// Input/simple control still excludes const for writes to outer variables.
    assignment_controls: Vec<Qualifier>,
    /// Controls already enclosing each variable's declaration do not control
    /// its local writes. This also keeps pure inlined helper locals independent
    /// of the caller's branch while preserving control for captured writes.
    assignment_declaration_depths: HashMap<SymbolId, usize>,
    /// Source/scope-resolved persistent mutation floors, computed before
    /// consumer checks so carried reads cannot retain an initializer's const.
    persistent_write_plan: persistent_writes::Summary,
    /// Analyzer-only occurrence snapshots for deferred fixed-value proofs.
    temporal_fixedness: std::cell::RefCell<temporal_fixedness::TemporalFixedness>,
    /// Definition-only lexical scans are independent of call-site arguments.
    temporal_summarized_functions: HashSet<Span>,
    /// Built-in variables the script declares a variable over, by name, at
    /// the first declaration (shadowing).
    shadowed_builtins: HashMap<String, Span>,
    /// Built-in variables the script reads by name, at the first read.
    builtin_reads: HashMap<String, Span>,
    /// Series-controlled ternary branches enclosing the current expression.
    /// Ternaries lower to real branches, so a series-materializing construct
    /// in an arm records nothing on bars where the other arm is selected
    /// (`W0004`). Kept apart from `branch_depth` because an expression arm
    /// does not make reassignments path-dependent.
    ternary_branch_depth: u32,
    /// Depth of ternary operands being analyzed, whatever the condition's
    /// qualifier. Global-only calls (`plot`, `hline`, `alertcondition`, ...)
    /// may not appear there.
    ternary_operand_depth: u32,
    /// The nearest Pine expression context that conditionally evaluates a
    /// stateful call. Lazy boolean RHS expressions and ternary branches get
    /// different warning codes. The context is saved and
    /// restored around each nested expression so it cannot leak to siblings.
    conditional_stateful_context: Option<ConditionalStatefulContext>,
    /// True only while analyzing an expression that is allowed to be a tuple:
    /// the RHS of a destructuring declaration or the trailing value of a
    /// user-function body. A tuple expression (`[a, b]`) anywhere else is
    /// `E0225`. Set narrowly (around the tail expression only) and always
    /// restored, so a tuple nested *inside* an operand — `plot([a, b] + 1)` —
    /// still fails.
    allow_tuple_expr: bool,
    /// Defer bare tuple-component `na` types until conditional siblings unify.
    defer_tuple_na: bool,
    /// User-defined enums, keyed by name. Collected from top-level `EnumDef`
    /// items before any statement is analyzed, so a member reference or a `na`
    /// type annotation may name an enum defined later in the source. A
    /// reference `Name.member` resolves to a const integer variant index typed
    /// as [`Type::Enum`].
    enums: HashMap<String, EnumInfo>,
    /// User-defined types, keyed by name. Collected from top-level
    /// `TypeDef` items before any statement is analyzed, so a field typed as
    /// another UDT (or a construction `Name.new(...)`) may name a type declared
    /// later in the source. Each entry records its fields (resolved types +
    /// defaults) in declaration order.
    types: HashMap<String, TypeInfo>,
    /// User-defined-type layouts in declaration order, indexed by `type_id`.
    /// Emitted into [`HirProgram::object_types`]; a type's `type_id` is its
    /// index here.
    object_types: Vec<ObjectTypeDecl>,
    /// User-defined methods, keyed by `(receiver-type key, method name)`.
    /// The receiver-type key is the canonical string form of the method's
    /// receiver type (a UDT name, or a collection form like `array<float>`),
    /// so two different types may each define a method of the same name without
    /// colliding. A method is a `FuncDef` whose first parameter is the receiver;
    /// `obj.f(args)` resolves here and inlines the stored template with the
    /// receiver prepended as the first argument (reusing the plain-function
    /// per-call-site machinery). Several methods may share a key when they
    /// differ in their other parameters (overloads); a call picks one with
    /// [`Self::select_overload`].
    methods: HashMap<(String, String), Vec<ast::FuncDef>>,
    /// Each library unit's methods, indexed like [`ImportedLibraries::units`]
    /// and keyed like [`Self::methods`]. Built once from [`LibUnit::methods`]
    /// when the analyzer starts, after library types have been registered.
    lib_methods: Vec<HashMap<(String, String), Vec<ast::FuncDef>>>,
    /// The `library(...)` declaration span, when the script declared one.
    /// Mutually exclusive with `indicator`/`strategy` (`E0240`). A library
    /// script is the only kind in which a definition may carry `export`.
    library: Option<Span>,
    /// The const `dynamic_requests` argument of an `indicator(...)` or
    /// `library(...)` declaration, when one is given. A strategy keeps its
    /// own in [`StrategyInfo`]; [`Self::dynamic_requests_enabled`] reads both.
    declared_dynamic_requests: Option<bool>,
    /// The linked library arena: every imported library unit plus the
    /// main script's alias→unit map. Empty for the single-source [`analyze`]
    /// entry; populated by [`analyze_with_libraries`] after the compiler
    /// resolves each `import` against the provided source map.
    libs: ImportedLibraries,
    /// The active library-namespace context while inlining a library function's
    /// body, innermost last. Each frame swaps name resolution onto a library
    /// unit: its function table, its own imports, and its global scope. Empty
    /// while analyzing the *main* script; pushed by [`Self::expand_lib_call`] so
    /// a library body's free names resolve against the library, never the
    /// importer.
    lib_ctx: Vec<LibFrame>,
    /// Per-library global scopes, built once on first use and cached by
    /// arena index. A library global is analyzed a single time so its slot is
    /// shared across every call site (library-scoped state).
    lib_globals: HashMap<usize, HashMap<String, SymbolId>>,
    /// The HIR init statements of every used library's top-level globals, in the
    /// order the globals were first touched. Prepended to the main body so a
    /// library global's slot is initialized before any call site reads it.
    lib_global_stmts: Vec<HirStmt>,
}

/// One active function boundary on [`Analyzer::function_scope_bases`]: an
/// inlined body, or an omitted parameter's default, being analyzed in its
/// definition's scope.
#[derive(Debug, Clone, Copy)]
struct FunctionBoundary {
    /// First scope the function owns (its parameter scope).
    base: usize,
    /// The definition's span. A global is visible to the function only when
    /// it was declared above this span in the same source.
    definition: Span,
}

/// One active library-namespace frame. While it is on
/// [`Analyzer::lib_ctx`], name/function/import resolution targets `unit` instead
/// of the main script's namespace. The body's variable scope (the library's
/// global scope + parameter bindings) is installed separately on
/// [`Analyzer::scopes`] by the expansion path.
struct LibFrame {
    /// Arena index of the library whose namespace is active.
    unit: usize,
}

/// One compiled library unit in the [`ImportedLibraries`] arena. A unit
/// captures a library's **whole** namespace — every function (private and
/// exported) with its body, its top-level global statements, and its own
/// resolved imports (as arena indices) — so an exported function's body resolves
/// its free names (sibling calls, private helpers, top-level globals, the
/// library's own imports) against the *library's* namespace and never against
/// the importer's. This is what makes an importer unable to hijack a
/// library-internal name (and a library-internal name unable to leak into the
/// importer).
#[derive(Debug, Clone, Default)]
pub struct LibUnit {
    /// Every top-level function, keyed by its bare name (private **and**
    /// exported), with a name's overloads in declaration order. A
    /// sibling/private call inside a library body resolves here.
    pub functions: HashMap<String, Vec<ast::FuncDef>>,
    /// Names carrying the `export` modifier — the only functions callable as
    /// `alias.name(...)` from an importer. An overload is callable that way
    /// only when its own definition is exported.
    pub exported: HashSet<String>,
    /// Every top-level `method` definition (private and exported), in source
    /// order. An importer calls an exported one on a value of its receiver
    /// type (`x.m(...)`) or through the alias (`alias.m(x, ...)`).
    pub methods: Vec<ast::FuncDef>,
    /// Types retain their library namespace and export visibility.
    pub types: Vec<ast::TypeDecl>,
    pub enums: Vec<ast::EnumDecl>,
    /// The library's top-level global statements (declarations/assignments), in
    /// source order. Analyzed once into the library's own global scope on first
    /// use, so a body's reference to a library global resolves to the library's
    /// slot, not an importer same-named global.
    pub globals: Vec<ast::Stmt>,
    /// The library's own `import`s, mapping each local alias to the arena index
    /// of the imported unit. A body's `alias.fn(...)` resolves through this map.
    pub imports: HashMap<String, usize>,
}

/// The full set of libraries linked into one compile, as a flat arena.
/// `units` is the arena (a library may appear once and be referenced by several
/// importers); `main_aliases` maps each *main-script* import alias to its unit.
/// An import whose path was not in the source map has no entry (the analyzer
/// reports that alias as `E0243`). Built by the compiler's `resolve_imports`
/// and handed to [`analyze_with_libraries`].
#[derive(Debug, Clone, Default)]
pub struct ImportedLibraries {
    /// The library arena. A unit's index is stable and referenced by
    /// [`LibUnit::imports`] and [`Self::main_aliases`].
    pub units: Vec<LibUnit>,
    /// Main-script alias → unit index. Absent for an unresolved import.
    pub main_aliases: HashMap<String, usize>,
}

/// A collected user-defined type: its fields (name, resolved type, and whether
/// a default was declared) in declaration order, plus the declaration span for
/// diagnostics. The field index is the runtime slot order.
struct TypeInfo {
    /// This type's `type_id`, its index into [`HirProgram::object_types`].
    type_id: u32,
    fields: Vec<FieldInfo>,
    /// Span of the type name, for "first defined here" labels.
    name_span: Span,
}

/// One resolved field of a [`TypeInfo`].
struct FieldInfo {
    name: String,
    ty: Type,
    /// The declared default value, already analyzed and coerced to the field
    /// type, or `None` when the field has no `= default` (it takes `na` unless
    /// the constructor supplies it).
    default: Option<HirExpr>,
    span: Span,
}

impl TypeInfo {
    /// The index and info of `field`, or `None` if this type has no such field.
    fn field(&self, field: &str) -> Option<(usize, &FieldInfo)> {
        self.fields
            .iter()
            .enumerate()
            .find(|(_, f)| f.name == field)
    }
}

/// A collected enum declaration: its members in declaration order (the index
/// is the runtime variant tag) and the declaration span for diagnostics.
struct EnumInfo {
    variants: Vec<EnumVariantInfo>,
    /// Span of the enum name, for "first defined here" labels.
    name_span: Span,
}

struct EnumVariantInfo {
    name: String,
    /// The optional source `= "title"`; `None` means display the member name.
    title: Option<String>,
}

impl EnumInfo {
    /// The variant index for `member`, or `None` if this enum has no such
    /// member.
    fn variant_index(&self, member: &str) -> Option<usize> {
        self.variants
            .iter()
            .position(|variant| variant.name == member)
    }
}

/// Hard cap on the total number of function-body statements expanded across
/// all call-site instantiations in one program. Prevents an exponential
/// blow-up (`f0` calls `f1` twice, `f1` calls `f2` twice, ...) from hanging
/// or exhausting memory; exceeding it is a structured `E0224` diagnostic.
const MAX_EXPANSION_NODES: usize = 10_000;

/// Maximum depth of inlined user-function calls. A deep acyclic chain slips
/// under [`MAX_EXPANSION_NODES`] (few statements total) while recursing once
/// per link through the analyzer, so this separately bounds the native-stack
/// recursion. Generous for any realistic call graph (Pine has no recursion),
/// small enough that the heavy per-level frames cannot overflow the stack.
const MAX_INLINE_DEPTH: usize = 64;

/// Budget for how deeply the analyzer may recurse through one script's
/// expressions and blocks, counted across inlined function calls.
///
/// The parser caps one expression's height and one block's nesting at 200
/// each, but a call is analyzed by inlining the callee's body at the call
/// site, so the analyzer's recursion is the sum along a call path: three
/// functions that each hold a 190-term sum and call the next recurse 570
/// levels. The browser package's 1 MiB stack ends near 480 levels of a binary
/// chain (about 2.2 KiB a level), and compound statements and method calls
/// cost about half as much again, which is why they are charged two units
/// ([`Analyzer::enter_nesting`]). At 200 units the worst case stays near
/// two-thirds of that stack, and a debug build stays inside a 2 MiB thread.
/// Exceeding it is a structured `E0224`.
const MAX_ANALYSIS_NESTING: u32 = 200;

/// The default history ceiling: how far back a history offset computed at run
/// time may reach in a script that asks for nothing more. A script raises it,
/// up to [`MAX_BARS_BACK_LIMIT`], with `indicator(max_bars_back=N)`, a
/// top-level `max_bars_back(source, N)` call, or simply a deeper literal
/// offset, which states its own requirement.
///
/// Buffers are sized from the history references a script makes, and
/// `max_bars_back` extends that. This compiler models that part with a fixed,
/// documented default so the extension is visible. This fixed default of 500
/// bars is a PineWASM constant, and the history ceiling is a compile-time
/// constant in this crate.
const MAX_BARS_BACK_DEFAULT: u32 = 500;

/// Plot counts a script may use.
const MAX_PLOT_COUNTS: u32 = 64;

/// Variables one scope may declare.
const MAX_VARIABLES_PER_SCOPE: usize = 1_000;

/// The hard maximum of the history ceiling is 5000 bars. An
/// explicit `max_bars_back` request for more is `E0219`; a literal history
/// offset past it is `E0233`.
const MAX_BARS_BACK_LIMIT: u32 = 5000;

/// `E0209` for `[]` written directly on an object field (`obj.field[1]`,
/// `(arr.get(0).field)[1]`). A field read is `series`, so the generic "must be
/// a series value" message would contradict the field's own type; this one
/// names the actual rule and the way around it.
const OBJECT_FIELD_HISTORY_MESSAGE: &str = "`[]` cannot be applied directly to an object field";
const OBJECT_FIELD_HISTORY_LABEL: &str =
    "a field keeps no per-bar history; assign it to a variable and take that variable's history";

/// The maximum value a `indicator(max_*_count=)` drawing-count cap may take
/// (the per-kind ceiling). A request outside `1..=500` is `E0219`.
const DRAWING_COUNT_LIMIT: i64 = 500;

/// The resolved right-hand side of a destructuring declaration. Either a
/// single tuple-returning `ta.*` call (one multi-push `CallTaState`, whose
/// N results are stored into the targets), or N independent element
/// expressions (a user tuple function's inlined result, one store each). A
/// scalar RHS becomes a one-element `Elements` so the arity check reports the
/// mismatch.
enum TupleRhs {
    /// A tuple-returning `ta.*` call: one node, `arity` results all of
    /// `elem_ty`.
    TaCall {
        call: HirExpr,
        elem_ty: Type,
        arity: u32,
    },
    /// N independent element values (each stored into its own target slot).
    Elements(Vec<HirExpr>),
    /// The RHS failed to analyze; a diagnostic was already reported.
    Error,
}

impl<'r> Analyzer<'r> {
    /// Resolve source spellings to a stable library-qualified object identity.
    /// Bare library types never leak into the importer; aliases expose exports only.
    pub(crate) fn object_type_name(&self, name: &str) -> Option<String> {
        if let Some((alias, member)) = name.split_once('.') {
            if let Some(unit) = self.resolve_alias(alias) {
                if self.libs.units[unit]
                    .types
                    .iter()
                    .any(|ty| ty.exported && ty.name.name == member)
                {
                    return Some(format!("@lib{unit}.{member}"));
                }
                return None;
            }
        }
        if let Some(frame) = self.lib_ctx.last() {
            if self.libs.units[frame.unit]
                .types
                .iter()
                .any(|ty| ty.name.name == name)
            {
                return Some(format!("@lib{}.{name}", frame.unit));
            }
            // A library cannot capture an importer type with the same bare name.
            if !name.starts_with("@lib")
                && !DrawKind::all().iter().any(|kind| kind.type_name() == name)
                && name != "footprint"
                && name != "volume_row"
            {
                return None;
            }
        }
        self.types.contains_key(name).then(|| name.to_string())
    }

    /// Resolve enum names with the same namespace and export rules as objects.
    pub(crate) fn enum_type_name(&self, name: &str) -> Option<String> {
        if let Some((alias, member)) = name.split_once('.') {
            if let Some(unit) = self.resolve_alias(alias) {
                if self.libs.units[unit]
                    .enums
                    .iter()
                    .any(|ty| ty.exported && ty.name.name == member)
                {
                    return Some(format!("@lib{unit}.{member}"));
                }
                return None;
            }
        }
        if let Some(frame) = self.lib_ctx.last() {
            if self.libs.units[frame.unit]
                .enums
                .iter()
                .any(|ty| ty.name.name == name)
            {
                return Some(format!("@lib{}.{name}", frame.unit));
            }
            // A library cannot capture an importer type with the same bare name.
            if !name.starts_with("@lib") {
                return None;
            }
        }
        self.enums.contains_key(name).then(|| name.to_string())
    }

    fn qualify_library_type(&self, ty: &mut ast::FieldType) {
        match &mut ty.kind {
            ast::FieldTypeKind::Named(name) => {
                if let Some(resolved) = self
                    .object_type_name(name)
                    .or_else(|| self.enum_type_name(name))
                {
                    *name = resolved;
                }
            }
            ast::FieldTypeKind::Array(inner) | ast::FieldTypeKind::Matrix(inner) => {
                self.qualify_library_type(inner);
            }
            ast::FieldTypeKind::Map(key, value) => {
                self.qualify_library_type(key);
                self.qualify_library_type(value);
            }
            _ => {}
        }
    }

    fn qualify_library_signatures(&mut self) {
        for unit_index in 0..self.libs.units.len() {
            let mut unit = self.libs.units[unit_index].clone();
            self.lib_ctx.push(LibFrame { unit: unit_index });
            for def in unit
                .functions
                .values_mut()
                .flatten()
                .chain(unit.methods.iter_mut())
            {
                if let Some(receiver) = &mut def.receiver {
                    self.qualify_library_type(receiver);
                }
                for param in &mut def.params {
                    if let Some(ty) = &mut param.ty {
                        self.qualify_library_type(ty);
                    }
                }
            }
            self.lib_ctx.pop();
            self.libs.units[unit_index] = unit;
        }
    }

    fn collect_library_types(&mut self) {
        for unit in 0..self.libs.units.len() {
            let program = ast::Program {
                version: None,
                strategy_alert_message: None,
                span: Span::dummy(),
                items: self.libs.units[unit]
                    .enums
                    .iter()
                    .cloned()
                    .map(ast::Item::EnumDef)
                    .chain(
                        self.libs.units[unit]
                            .types
                            .iter()
                            .cloned()
                            .map(ast::Item::TypeDef),
                    )
                    .collect(),
            };
            self.lib_ctx.push(LibFrame { unit });
            self.collect_enums(&program);
            self.collect_types(&program);
            self.lib_ctx.pop();
        }
    }

    fn new(registry: &'r BuiltinRegistry, libs: ImportedLibraries) -> Self {
        Analyzer {
            registry,
            diagnostics: Diagnostics::new(),
            symbols: Vec::new(),
            scopes: vec![HashMap::new()],
            plots: Vec::new(),
            dynamic_color_calculation: false,
            visuals: Vec::new(),
            fills: Vec::new(),
            plot_handles: HashMap::new(),
            plot_handle_roots: HashMap::new(),
            persistent_fill_endpoints: Vec::new(),
            block_tail: false,
            discarded_call: None,
            indicator: None,
            strategy: None,
            max_bars_back: None,
            max_bars_back_offsets: Vec::new(),
            suggestions: suggestions::Suggestions::new(),
            synthetic_slots: 0,
            forin_counter: 0,
            timeframe_change_counter: 0,
            method_recv_counter: 0,
            ta_states: Vec::new(),
            ta_input_bindings: Vec::new(),
            ta_int_inputs: HashMap::new(),
            inputs: Vec::new(),
            input_color_property_transparencies: HashMap::new(),
            const_input_property_transparencies: HashMap::new(),
            const_input_values: HashMap::new(),
            ints_from_input_defaults: HashSet::new(),
            input_default_ema_lengths: HashMap::new(),
            request_default_ema_lengths: std::cell::RefCell::new(Vec::new()),
            input_default_warnings: HashSet::new(),
            held_input_display_bits: HashMap::new(),
            default_display_masks: HashMap::new(),
            display_set_ops: HashMap::new(),
            const_color_transparencies: HashMap::new(),
            const_accessor_stable_symbols: HashSet::new(),
            arrow_inline_parameters: HashSet::new(),
            function_parameters: HashSet::new(),
            plot_counts: 0,
            plot_count_overflow: None,
            ohlc_destructure_targets: HashSet::new(),
            rendering_inline: rendering_inline_provenance::InlineProvenance::default(),
            arrow_history_spans: HashSet::new(),
            declared_qualifiers: HashMap::new(),
            input_origins: HashMap::new(),
            input_active_origins: HashMap::new(),
            source_closed_active_origins: HashSet::new(),
            request_input_expr_origins: HashMap::new(),
            htf_expr_origins: HashMap::new(),
            current_feed_symbols: HashMap::new(),
            current_chart_timeframes: HashSet::new(),
            in_var_initializer: false,
            loop_depth: 0,
            loop_counters: Vec::new(),
            pending_stmts: Vec::new(),
            arg_order_frames: Vec::new(),
            arg_order_depth: 0,
            arg_order_suppressed: false,
            functions: HashMap::new(),
            evaluated_defaults: HashSet::new(),
            call_stack: Vec::new(),
            function_scope_bases: Vec::new(),
            expansion_nodes: 0,
            expansion_aborted: false,
            nesting: 0,
            nesting_reported: false,
            else_if_link: false,
            branch_depth: 0,
            request_branch_depth: 0,
            assignment_controls: Vec::new(),
            assignment_declaration_depths: HashMap::new(),
            persistent_write_plan: persistent_writes::Summary::default(),
            temporal_fixedness: std::cell::RefCell::default(),
            temporal_summarized_functions: HashSet::new(),
            shadowed_builtins: HashMap::new(),
            builtin_reads: HashMap::new(),
            ternary_branch_depth: 0,
            ternary_operand_depth: 0,
            conditional_stateful_context: None,
            allow_tuple_expr: false,
            defer_tuple_na: false,
            enums: HashMap::new(),
            types: HashMap::new(),
            object_types: Vec::new(),
            methods: HashMap::new(),
            lib_methods: Vec::new(),
            library: None,
            declared_dynamic_requests: None,
            libs,
            lib_ctx: Vec::new(),
            lib_globals: HashMap::new(),
            lib_global_stmts: Vec::new(),
        }
    }

    fn error(
        &mut self,
        code: &str,
        message: impl Into<String>,
        span: Span,
        label: impl Into<String>,
    ) {
        self.diagnostics
            .push(Diagnostic::error(code, message).with_label(span, label));
    }

    /// Charge `cost` units of the nesting budget for a construct at `span`
    /// that the analyzer is about to recurse into. Returns `false`, after
    /// reporting `E0224` (once per script), when the budget is spent; the
    /// caller must then not recurse. A `true` result is paired with
    /// [`Self::leave_nesting`].
    fn enter_nesting(&mut self, cost: u32, span: Span) -> bool {
        if self.nesting + cost > MAX_ANALYSIS_NESTING {
            if !std::mem::replace(&mut self.nesting_reported, true) {
                let through_calls = if self.call_stack.is_empty() {
                    ""
                } else {
                    ", counting the function calls that lead here"
                };
                self.diagnostics.push(
                    Diagnostic::error(codes::E0224, "code is nested too deeply")
                        .with_label(
                            span,
                            format!(
                                "expressions and blocks nest past the \
                                 {MAX_ANALYSIS_NESTING}-level limit here{through_calls}"
                            ),
                        )
                        .with_note(
                            "assign part of the expression to a variable, or flatten the nested \
                             blocks or function calls",
                        ),
                );
            }
            return false;
        }
        self.nesting += cost;
        true
    }

    fn leave_nesting(&mut self, cost: u32) {
        self.nesting -= cost;
    }

    fn warning(
        &mut self,
        code: &str,
        message: impl Into<String>,
        span: Span,
        label: impl Into<String>,
    ) {
        self.diagnostics
            .push(Diagnostic::warning(code, message).with_label(span, label));
    }

    fn in_local_scope(&self) -> bool {
        self.scopes.len() > 1
    }

    /// Whether the script's declaration leaves dynamic requests on: the
    /// default, unless `indicator`, `strategy` or `library` passes
    /// `dynamic_requests = false`.
    pub(crate) fn dynamic_requests_enabled(&self) -> bool {
        match &self.strategy {
            Some(strategy) => strategy.dynamic_requests,
            None => self.declared_dynamic_requests.unwrap_or(true),
        }
    }

    /// Enforce the declaration-level `dynamic_requests = false` restriction
    /// before dispatching any `request.*()` family member. Dynamic requests
    /// are enabled by default; disabling them forbids request calls in
    /// unresolved conditional branches and expression operands. An inlined
    /// function inherits its caller's request context independently of the
    /// series-only depth used for intermittent-call warnings.
    fn reject_non_dynamic_request_context(&mut self, name: &str, span: Span) -> bool {
        let dynamic_requests = self.dynamic_requests_enabled();
        let conditional = self.conditional_stateful_context.is_some();
        if dynamic_requests
            || (self.branch_depth == 0 && self.request_branch_depth == 0 && !conditional)
        {
            return false;
        }
        self.error(
            codes::E0242,
            format!(
                "`{name}` cannot execute in a local or conditional scope when the declaration sets `dynamic_requests = false`"
            ),
            span,
            "move the request to global scope or enable `dynamic_requests`",
        );
        true
    }

    /// A simple qualifier alone does not say whether a branch can be resolved
    /// from source. Preserve closed scalar provenance without evaluating
    /// input defaults or chart metadata. Const control retains its existing
    /// behavior; Series control remains subject to `branch_depth` as well.
    fn unresolved_request_control(&self, condition: &HirExpr) -> bool {
        condition.qualifier != Qualifier::Const
            && self.source_closed_active_expr_from_hir(condition).is_none()
    }

    fn analyze_expr_in_conditional_context(
        &mut self,
        expr: &ast::Expr,
        expected: Option<&Type>,
        context: ConditionalStatefulContext,
        active: bool,
    ) -> HirExpr {
        if !active {
            return self.analyze_expr_expecting(expr, expected);
        }
        let saved = self.conditional_stateful_context;
        self.conditional_stateful_context = Some(context);
        let ternary = matches!(context, ConditionalStatefulContext::Ternary);
        if ternary {
            self.ternary_branch_depth += 1;
        }
        let result = self.analyze_expr_expecting(expr, expected);
        if ternary {
            self.ternary_branch_depth -= 1;
        }
        self.conditional_stateful_context = saved;
        result
    }

    fn warn_stateful_call_in_conditional(&mut self, function_name: &str, span: Span) {
        let Some(context) = self.conditional_stateful_context else {
            return;
        };
        match context {
            ConditionalStatefulContext::LazyBoolean => self.warning(
                codes::W0008,
                format!(
                    "Stateful call `{function_name}()` is guarded by a lazy boolean condition, so some bars can skip it and its history can drift"
                ),
                span,
                "evaluate the call once before the boolean expression, then reference its result",
            ),
            ConditionalStatefulContext::Ternary => self.warning(
                codes::W0009,
                format!(
                    "Stateful call `{function_name}()` appears in a conditional branch, so its history can advance unevenly"
                ),
                span,
                "evaluate the call before the ternary on every bar, then select its saved result",
            ),
        }
    }

    /// Bind a call's arguments to a parameter list by position then by name.
    ///
    /// `param_names` are the callee's parameters in declaration order. Returns
    /// a vector parallel to `param_names`: slot `i` holds the argument bound to
    /// parameter `i`, or `None` when that parameter was not supplied (the
    /// caller then applies the parameter's default / required rules exactly as
    /// it did for positional-only calls). Positional arguments fill slots
    /// left-to-right; a named argument (`name = value`) binds the parameter it
    /// names. Reports `E0228` for a name that does not match any parameter
    /// (with a did-you-mean) or a parameter bound twice, and returns `None`
    /// when any such error occurred so the caller can bail cleanly. The
    /// positional-after-named case is already a parse error, so `CallArg`s
    /// always arrive as `positional* named*`.
    ///
    /// The index-level decision is [`binder::bind_slots`]; this wrapper only
    /// resolves names and turns its result into expressions and diagnostics.
    fn bind_call_args<'a>(
        &mut self,
        callee_name: &str,
        param_names: &[&str],
        args: &'a [ast::CallArg],
    ) -> Option<(Vec<Option<&'a ast::Expr>>, Vec<&'a ast::Expr>)> {
        let shapes = binder::arg_shapes(param_names, args);
        // Positional arguments beyond the fixed parameter count: variadic
        // functions (`math.max`, ...) legitimately repeat their last
        // parameter, so these are appended to the dense list rather than
        // dropped; a non-variadic callee's downstream arity check then reports
        // the surplus. Overflow only happens for all-positional calls (a named
        // argument cannot follow a positional-overflow one and still name a
        // valid distinct parameter), which is exactly the variadic case.
        match binder::bind_slots(param_names.len(), &shapes) {
            Ok(slots) => {
                let bound = slots
                    .slots
                    .iter()
                    .map(|slot| slot.map(|j| &args[j].value))
                    .collect();
                let overflow = slots.overflow.iter().map(|&j| &args[j].value).collect();
                Some((bound, overflow))
            }
            Err(errors) => {
                self.report_slot_errors(callee_name, param_names, args, &errors);
                None
            }
        }
    }

    /// Emit the `E0228` diagnostics for [`binder::bind_slots`] errors, in call
    /// order.
    fn report_slot_errors(
        &mut self,
        callee_name: &str,
        param_names: &[&str],
        args: &[ast::CallArg],
        errors: &[binder::SlotErr],
    ) {
        for error in errors {
            let (binder::SlotErr::Duplicate(j) | binder::SlotErr::UnknownName(j)) = *error;
            // The kernel only reports named arguments.
            let Some(name) = &args[j].name else {
                continue;
            };
            match error {
                binder::SlotErr::Duplicate(_) => self.error(
                    codes::E0228,
                    format!(
                        "`{callee_name}` argument `{}` is bound more than once",
                        name.name
                    ),
                    name.span,
                    "this parameter already has an argument",
                ),
                binder::SlotErr::UnknownName(_) => {
                    let mut diag = Diagnostic::error(
                        codes::E0228,
                        format!("`{callee_name}` has no parameter named `{}`", name.name),
                    )
                    .with_label(name.span, "unknown parameter name");
                    if let Some(suggestion) = self
                        .suggestions
                        .among(&name.name, || param_names.iter().copied().map(Some))
                    {
                        diag = diag.with_note(format!("did you mean `{suggestion}`?"));
                    }
                    self.diagnostics.push(diag);
                }
            }
        }
    }

    /// Bind `args` (positional + named) to `param_names`, then flatten to a
    /// dense positional argument list the downstream positional checkers
    /// consume. Every built-in and user signature lists its required
    /// parameters before its optional ones, so after binding the only legal
    /// shape is a run of supplied leading arguments followed by omitted
    /// trailing optionals: a `None` slot *before* the last supplied argument
    /// means a required parameter was skipped by name, which is reported as
    /// `E0204` (missing argument). Returns the cloned positional arguments, or
    /// `None` after reporting a binding (`E0228`) or gap (`E0204`) error.
    ///
    /// `callee_span` is used for the missing-argument label.
    fn bind_positional(
        &mut self,
        callee_name: &str,
        param_names: &[&str],
        args: &[ast::CallArg],
        callee_span: Span,
    ) -> Option<Vec<ast::Expr>> {
        self.bind_positional_with_defaults(callee_name, param_names, &[], args, callee_span)
    }

    /// Bind a built-in with optional parameters without manufacturing AST
    /// defaults for gaps introduced by named arguments. Display calls use the
    /// sparse result so `plot(close, color = color.red)` retains the built-in
    /// signature instead of incorrectly reporting the omitted `title`.
    // The `arg.cloned()` closure below cannot become `Option::cloned`: the
    // `&T` and `&mut T` impls are both in scope, so the path form is E0034.
    #[allow(clippy::redundant_closure_for_method_calls)]
    fn bind_optional_args(
        &mut self,
        callee_name: &str,
        param_names: &[&str],
        required: usize,
        args: &[ast::CallArg],
        span: Span,
    ) -> Option<Vec<Option<ast::Expr>>> {
        let (bound, overflow) = self.bind_call_args(callee_name, param_names, args)?;
        let mut ok = true;
        if !overflow.is_empty() {
            self.error(
                codes::E0204,
                format!(
                    "`{callee_name}` expects {required} to {} argument(s), found {}",
                    param_names.len(),
                    args.len()
                ),
                span,
                format!("expected at most {} argument(s)", param_names.len()),
            );
            ok = false;
        }
        for (index, name) in param_names.iter().enumerate().take(required) {
            if bound[index].is_none() {
                self.error(
                    codes::E0204,
                    format!("`{callee_name}` is missing required argument `{name}`"),
                    span,
                    "required argument is missing",
                );
                ok = false;
            }
        }
        ok.then(|| {
            bound
                .into_iter()
                .map(|arg| arg.cloned())
                .collect::<Vec<_>>()
        })
    }

    /// Like [`Self::bind_positional`], but `defaults[i]` supplies the AST
    /// expression to substitute when parameter `i` is left as an interior gap
    /// by a named argument (used for user functions, whose optional parameters
    /// carry an AST default). An empty `defaults` slice means no parameter has
    /// a fillable default, so any interior gap is `E0204`. Built-in signatures
    /// list optionals last, so an interior gap there is always a genuinely
    /// missing required argument.
    ///
    /// The index-level decision is [`binder::bind`]; this wrapper only resolves
    /// names and turns its result into cloned expressions and diagnostics.
    fn bind_positional_with_defaults(
        &mut self,
        callee_name: &str,
        param_names: &[&str],
        defaults: &[Option<&ast::Expr>],
        args: &[ast::CallArg],
        callee_span: Span,
    ) -> Option<Vec<ast::Expr>> {
        let shapes = binder::arg_shapes(param_names, args);
        let has_default: Vec<bool> = defaults.iter().map(Option::is_some).collect();
        match binder::bind(param_names.len(), &shapes, &has_default) {
            // Arguments and interior defaults form the dense positional list,
            // with any variadic overflow at the end (empty for fixed-arity
            // callees; a surplus there is caught by arity checking).
            Ok(choices) => Some(Self::dense_exprs(args, defaults, &choices)),
            Err(err) => {
                self.report_bind_error(callee_name, param_names, args, callee_span, err);
                None
            }
        }
    }

    /// The dense positional argument list for [`binder::bind`]'s choices:
    /// each call argument or parameter default, cloned.
    fn dense_exprs(
        args: &[ast::CallArg],
        defaults: &[Option<&ast::Expr>],
        choices: &[binder::Choice],
    ) -> Vec<ast::Expr> {
        choices
            .iter()
            .map(|choice| match *choice {
                binder::Choice::Arg(j) => args[j].value.clone(),
                binder::Choice::Default(i) => (*defaults[i]
                    .as_ref()
                    .expect("the binder only chooses a default that exists"))
                .clone(),
            })
            .collect()
    }

    /// Report a [`binder::bind`] error: the `E0228` slot errors, or one
    /// `E0204` per gap.
    fn report_bind_error(
        &mut self,
        callee_name: &str,
        param_names: &[&str],
        args: &[ast::CallArg],
        callee_span: Span,
        err: binder::BindErr,
    ) {
        match err {
            binder::BindErr::Slots(errors) => {
                self.report_slot_errors(callee_name, param_names, args, &errors);
            }
            binder::BindErr::Gaps(gaps) => {
                for i in gaps {
                    self.error(
                        codes::E0204,
                        format!(
                            "`{callee_name}` is missing a value for parameter `{}`",
                            param_names[i]
                        ),
                        callee_span,
                        "a later argument was given by name, but this earlier parameter was not supplied",
                    );
                }
            }
        }
    }

    /// An expression of type `Error` used when checking failed; carries the
    /// span so downstream stages stay aligned, and suppresses cascades.
    fn error_expr(&self, span: Span) -> HirExpr {
        HirExpr {
            kind: HirExprKind::ConstFloat(0.0),
            ty: Type::Error,
            qualifier: Qualifier::Const,
            span,
        }
    }

    /// Wrap an int-typed expression in an explicit [`HirExprKind::IntToFloat`]
    /// coercion; anything else passes through unchanged. Applied wherever a
    /// float is required but an int is given (mixed arithmetic/comparisons,
    /// mixed equality, `plot(...)` values).
    fn coerce_int_to_float(expr: HirExpr) -> HirExpr {
        if expr.ty != Type::Int {
            return expr;
        }
        HirExpr {
            ty: Type::Float,
            qualifier: expr.qualifier,
            span: expr.span,
            kind: HirExprKind::IntToFloat(Box::new(expr)),
        }
    }

    // -- program ------------------------------------------------------------

    fn analyze_program(&mut self, program: &ast::Program) -> Vec<HirStmt> {
        self.persistent_write_plan = persistent_writes::summarize(program, self.registry);
        if let Some(v) = &program.version {
            match v.version.cmp(&6) {
                std::cmp::Ordering::Less => {
                    // Compiling earlier versions as v6 would change what
                    // unchanged-looking code computes, without any error.
                    self.error(
                        codes::E0249,
                        format!(
                            "`//@version={}` source must be migrated to version 6 before it can compile",
                            v.version
                        ),
                        v.span,
                        if v.version == 5 {
                            "run `pinewasm migrate FILE` to rewrite this script as version 6"
                        } else {
                            "convert the script to version 5, then run `pinewasm migrate FILE`"
                        },
                    );
                }
                std::cmp::Ordering::Greater => {
                    self.warning(
                        codes::W0003,
                        format!(
                            "`//@version={}` is not supported; treating as version 6",
                            v.version
                        ),
                        v.span,
                        "only version 6 semantics are implemented",
                    );
                }
                std::cmp::Ordering::Equal => {}
            }
        }

        // Collect user-defined functions before analyzing statements, so a
        // call may reference a function defined later in the source. Each
        // definition is validated here (duplicate names, duplicate params);
        // recursion and arity are checked at each call site.
        // Collect enum declarations before functions and statements, so a
        // member reference (`Name.member`) or an enum-typed `na` annotation may
        // name an enum declared anywhere in the source.
        // Pre-scan for a `library(...)` declaration so the script kind is
        // known before `export` modifiers are validated — `library(...)` may
        // appear anywhere among the top-level items, yet `export` is legal only
        // in a library script (`E0243`), and an `export`ed function must still be
        // collected regardless of ordering.
        self.detect_library_kind(program);
        // `export` is legal only in a library script; flag every misuse now
        // (`E0243`), before functions/types are collected.
        self.check_export_usage(program);
        self.collect_enums(program);
        // Collect type declarations after enums (a field may be enum-typed) and
        // before functions/statements, so a construction or a field typed as
        // another UDT may name a type declared anywhere in the source.
        self.collect_types(program);
        self.collect_functions(program);
        // Register imported library functions under their `alias.name`
        // keys, so a call `alias.fn(...)` resolves through the ordinary
        // function-inlining path. Done after local functions so a local name
        // never collides with a dotted import key.
        self.collect_imports(program);

        let mut body = Vec::new();
        for item in &program.items {
            match item {
                ast::Item::Indicator(decl) => self.analyze_indicator_decl(decl),
                ast::Item::Strategy(decl) => self.analyze_strategy_decl(decl),
                ast::Item::Library(decl) => self.analyze_library_decl(decl),
                // Import declarations were resolved in `collect_imports`; no
                // executable code and nothing more to analyze here.
                ast::Item::Import(_) => {}
                // Function definitions are templates, not executable code:
                // they were collected above and are expanded at call sites.
                ast::Item::FuncDef(_) => {}
                // Enum declarations are type definitions, not executable code:
                // they were collected above and referenced by member accesses.
                ast::Item::EnumDef(_) => {}
                // Type declarations are type definitions, not executable code:
                // they were collected above and referenced by constructions and
                // field accesses.
                ast::Item::TypeDef(_) => {}
                ast::Item::Stmt(stmt) => self.analyze_stmt_into(stmt, &mut body),
            }
        }

        // Attach the `//@strategy_alert_message=...` annotation (parsed in the
        // parser, carried on the Program) as strategy metadata. It is meaningful
        // only for a strategy script; on an indicator/library it is ignored.
        if let (Some(strategy), Some(msg)) =
            (self.strategy.as_mut(), &program.strategy_alert_message)
        {
            strategy.alert_message.clone_from(msg);
        }

        if self.indicator.is_none() && self.strategy.is_none() && self.library.is_none() {
            self.diagnostics.push(
                Diagnostic::warning(
                    codes::W0001,
                    "script has no `indicator(...)` or `strategy(...)` declaration",
                )
                .with_note("This script has no indicator(), strategy() or library() declaration; the compiler accepts it with warning W0001."),
            );
        }
        self.check_builtin_shadowing();
        self.check_max_bars_back_offsets();
        self.check_plot_counts();
        body
    }

    /// Add one call's plot counts. A plot-family call uses one count per
    /// value series it draws plus one per color argument in the series form;
    /// `bgcolor`, `barcolor` and `alertcondition` use one; `fill` uses one only
    /// with a series color; `hline` uses none.
    pub(crate) fn add_plot_counts(&mut self, counts: u32, span: Span) {
        let before = self.plot_counts;
        self.plot_counts = before.saturating_add(counts);
        if before <= MAX_PLOT_COUNTS && self.plot_counts > MAX_PLOT_COUNTS {
            self.plot_count_overflow = Some(span);
        }
    }

    fn check_plot_counts(&mut self) {
        if let Some(span) = self.plot_count_overflow {
            self.error(
                codes::E0253,
                format!(
                    "the script uses {} plot counts; at most {MAX_PLOT_COUNTS} are allowed",
                    self.plot_counts
                ),
                span,
                "this call goes over the plot count limit",
            );
        }
    }

    /// Report the declaration that takes the current scope past
    /// [`MAX_VARIABLES_PER_SCOPE`], once per scope. Compiler temporaries
    /// (names starting with `%`) are not script variables and do not count.
    pub(crate) fn check_scope_variable_limit(&mut self, name: &str, span: Span) {
        if name.starts_with('%') {
            return;
        }
        let over = self.scopes.last().is_some_and(|scope| {
            scope.len() > MAX_VARIABLES_PER_SCOPE
                && scope.keys().filter(|key| !key.starts_with('%')).count()
                    == MAX_VARIABLES_PER_SCOPE + 1
        });
        if over {
            self.error(
                codes::E0253,
                format!("a scope may declare at most {MAX_VARIABLES_PER_SCOPE} variables"),
                span,
                "this declaration goes over the limit for its scope",
            );
        }
    }

    /// The effective history ceiling: the largest request the script has made
    /// (see [`Self::raise_max_bars_back`]), else the default
    /// [`MAX_BARS_BACK_DEFAULT`].
    fn effective_max_bars_back(&self) -> u32 {
        self.max_bars_back.unwrap_or(MAX_BARS_BACK_DEFAULT)
    }

    /// Record a request for a history buffer of `limit` bars; the largest
    /// request wins.
    fn raise_max_bars_back(&mut self, limit: u32) {
        self.max_bars_back = Some(match self.max_bars_back {
            Some(existing) => existing.max(limit),
            None => limit,
        });
    }

    /// After the whole program is analyzed (so every `max_bars_back` request
    /// is known regardless of source order), settle the history ceiling
    /// against the literal offsets the script uses.
    ///
    /// A literal offset is a requirement known at compile time, so it needs no
    /// declaration: a constant offset sizes its own buffer, and
    /// `max_bars_back` is for the offsets it cannot see. The deepest literal
    /// therefore counts as one more request, and the ceiling rises to it when
    /// it is past the default and every declared limit. Only an offset past
    /// [`MAX_BARS_BACK_LIMIT`] cannot be met by any buffer; that is `E0233`.
    ///
    /// Raising the ceiling, and not merely admitting the offset, keeps what
    /// the stages after this one may rely on: no literal offset in an
    /// analyzed program is deeper than the ceiling the program carries. The
    /// ceiling settled here is also the one a history offset computed at run
    /// time is checked against (`HistoryBufferLimitExceeded`).
    fn check_max_bars_back_offsets(&mut self) {
        let offsets = std::mem::take(&mut self.max_bars_back_offsets);
        let mut deepest = 0;
        for (offset, span) in offsets {
            if offset > MAX_BARS_BACK_LIMIT {
                self.error(
                    codes::E0233,
                    format!(
                        "history offset {offset} exceeds the max_bars_back limit of \
                         {MAX_BARS_BACK_LIMIT}"
                    ),
                    span,
                    format!(
                        "the history buffer cannot exceed {MAX_BARS_BACK_LIMIT} bars \
                         (the compiler's history ceiling); offset {offset} is unreachable"
                    ),
                );
            } else {
                deepest = deepest.max(offset);
            }
        }
        if deepest > self.effective_max_bars_back() {
            self.raise_max_bars_back(deepest);
        }
    }

    /// Pre-scan the top-level items for a `library(...)` declaration and record
    /// its span. This runs before functions are collected so `export`
    /// modifiers can be validated regardless of where `library(...)` appears.
    /// Only the *first* library declaration is recorded here; a duplicate is
    /// diagnosed later in [`Self::analyze_library_decl`].
    fn detect_library_kind(&mut self, program: &ast::Program) {
        for item in &program.items {
            if let ast::Item::Library(decl) = item {
                self.library = Some(decl.span);
                return;
            }
        }
    }

    /// Emit `E0243` for every `export`ed definition (function, method, type,
    /// enum) in a script that is not a library. `export` marks a
    /// definition for import by another script and only makes sense in a
    /// `library(...)` script.
    fn check_export_usage(&mut self, program: &ast::Program) {
        if let Some(library_span) = self.library {
            // A library must export at least one definition.
            let exports = program.items.iter().any(|item| match item {
                ast::Item::FuncDef(d) => d.exported,
                ast::Item::TypeDef(d) => d.exported,
                ast::Item::EnumDef(d) => d.exported,
                _ => false,
            });
            if !exports {
                self.error(
                    codes::E0243,
                    "a library must export at least one function, method, type or enum",
                    library_span,
                    "mark a definition the library provides with `export`",
                );
            }
            return;
        }
        for item in &program.items {
            let (exported, span, what) = match item {
                ast::Item::FuncDef(d) if d.exported => (
                    true,
                    d.span,
                    if d.receiver.is_some() {
                        "method"
                    } else {
                        "function"
                    },
                ),
                ast::Item::TypeDef(d) if d.exported => (true, d.span, "type"),
                ast::Item::EnumDef(d) if d.exported => (true, d.span, "enum"),
                _ => continue,
            };
            if exported {
                self.error(
                    codes::E0243,
                    format!(
                        "`export` is only allowed in a `library(...)` script, not on this {what}"
                    ),
                    span,
                    "declare the script with `library(\"Title\")` to export definitions",
                );
            }
        }
    }

    /// Validate every `import` in the *main* script: an alias whose path
    /// was not in the source map (no arena unit) is an unresolved import
    /// (`E0243`); a duplicate alias among the main script's imports is `E0243`
    /// (last-wins would silently mask one library). Resolution itself (parsing
    /// each library, building the arena) already happened in the compiler; here
    /// we only surface the main script's link errors. Library bodies dispatch
    /// their own imports through the active [`LibFrame`], not this table.
    fn collect_imports(&mut self, program: &ast::Program) {
        let mut seen: HashMap<&str, Span> = HashMap::new();
        for item in &program.items {
            let ast::Item::Import(decl) = item else {
                continue;
            };
            if let Some(&first) = seen.get(decl.alias.name.as_str()) {
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::E0243,
                        format!("import alias `{}` is already used", decl.alias.name),
                    )
                    .with_label(decl.span, "reused here")
                    .with_label(first, "first bound here")
                    .with_note("each import must bind a distinct alias"),
                );
                continue;
            }
            seen.insert(decl.alias.name.as_str(), decl.span);
            if !self.libs.main_aliases.contains_key(&decl.alias.name) {
                self.error(
                    codes::E0243,
                    format!(
                        "unresolved import `{}/{}/{}`",
                        decl.publisher.name, decl.library.name, decl.version
                    ),
                    decl.span,
                    "the library is not in the provided source map; imports never reach the network",
                );
            }
        }
    }
}

/// Validate parameter declaration rules shared by local scripts and imported
/// library sources. These rules apply even when no call expands the body.
pub fn validate_function_parameter_declarations(def: &ast::FuncDef) -> Vec<Diagnostic> {
    fn is_na(expr: &ast::Expr) -> bool {
        match expr {
            ast::Expr::Na { .. } => true,
            ast::Expr::Group { inner, .. } => is_na(inner),
            _ => false,
        }
    }
    let mut diagnostics = Diagnostics::new();
    for (index, param) in def.params.iter().enumerate() {
        let has_type = param.ty.is_some() || (index == 0 && def.receiver.is_some());
        let reason = if param.qualifier == Some(ast::ParamQualifier::Input) {
            Some("function parameters cannot use the input qualifier")
        } else if param.qualifier.is_some() && !has_type {
            Some("a qualified parameter requires an explicit type")
        } else if def.exported && param.qualifier == Some(ast::ParamQualifier::Const) {
            Some("exported function parameters cannot use the const qualifier")
        } else if def.exported && !has_type {
            Some("exported function parameters require explicit types")
        } else if !has_type && param.default.as_ref().is_some_and(is_na) {
            Some("a parameter with an na default requires an explicit type")
        } else {
            None
        };
        if let Some(reason) = reason {
            diagnostics.push(
                Diagnostic::error(
                    codes::E0224,
                    format!("invalid parameter `{}`: {reason}", param.name.name),
                )
                .with_label(param.span, reason),
            );
        }
    }
    diagnostics.finish()
}

/// True for the numeric types (`int` and `float`).
fn is_numeric(ty: &Type) -> bool {
    matches!(ty, Type::Int | Type::Float)
}

fn const_string_hir(expr: &HirExpr) -> Option<String> {
    match &expr.kind {
        HirExprKind::ConstString(value) => Some(value.clone()),
        _ => None,
    }
}

fn const_i64_hir(expr: &HirExpr) -> Option<i64> {
    match &expr.kind {
        HirExprKind::ConstInt(value) => Some(*value),
        HirExprKind::Unary {
            op: UnOp::Neg,
            operand,
        } => const_i64_hir(operand).and_then(i64::checked_neg),
        _ => None,
    }
}

fn const_f64_hir(expr: &HirExpr) -> Option<f64> {
    match &expr.kind {
        HirExprKind::ConstFloat(value) => Some(*value),
        HirExprKind::ConstInt(value) => Some(*value as f64),
        HirExprKind::IntToFloat(inner) => const_i64_hir(inner).map(|value| value as f64),
        HirExprKind::Unary {
            op: UnOp::Neg,
            operand,
        } => const_f64_hir(operand).map(|value| -value),
        _ => None,
    }
}

fn const_bool_hir(expr: &HirExpr) -> Option<bool> {
    match &expr.kind {
        HirExprKind::ConstBool(value) => Some(*value),
        _ => None,
    }
}

fn plot_style_kind(value: String) -> Option<PlotStyleKind> {
    Some(match value.as_str() {
        "plot.style_line" => PlotStyleKind::Line,
        "plot.style_linebr" => PlotStyleKind::LineBreak,
        "plot.style_stepline" => PlotStyleKind::StepLine,
        "plot.style_steplinebr" => PlotStyleKind::StepLineBreak,
        "plot.style_stepline_diamond" => PlotStyleKind::StepLineDiamond,
        "plot.style_histogram" => PlotStyleKind::Histogram,
        "plot.style_cross" => PlotStyleKind::Cross,
        "plot.style_area" => PlotStyleKind::Area,
        "plot.style_areabr" => PlotStyleKind::AreaBreak,
        "plot.style_columns" => PlotStyleKind::Columns,
        "plot.style_circles" => PlotStyleKind::Circles,
        _ => return None,
    })
}

fn plot_line_style(value: String) -> Option<PlotLineStyle> {
    Some(match value.as_str() {
        "plot.linestyle_solid" | "hline.style_solid" => PlotLineStyle::Solid,
        "plot.linestyle_dotted" | "hline.style_dotted" => PlotLineStyle::Dotted,
        "plot.linestyle_dashed" | "hline.style_dashed" => PlotLineStyle::Dashed,
        _ => return None,
    })
}

fn plot_display(expr: &HirExpr) -> Option<PlotDisplay> {
    fn mask(expr: &HirExpr) -> Option<u8> {
        if let Some(value) = const_string_hir(expr) {
            return pinewasm_hir::display_set::mask(&value);
        }
        match &expr.kind {
            HirExprKind::Binary { op, lhs, rhs } => {
                let left = mask(lhs)?;
                let right = mask(rhs)?;
                match op {
                    BinOp::Add => Some(left | right),
                    BinOp::Sub => Some(left & !right),
                    _ => None,
                }
            }
            _ => None,
        }
    }
    Some(match mask(expr)? {
        0 => PlotDisplay::None,
        1 => PlotDisplay::Pane,
        2 => PlotDisplay::PriceScale,
        4 => PlotDisplay::StatusLine,
        8 => PlotDisplay::DataWindow,
        31 => PlotDisplay::All,
        3 => PlotDisplay::PanePriceScale,
        5 => PlotDisplay::PaneStatusLine,
        6 => PlotDisplay::PriceScaleStatusLine,
        7 => PlotDisplay::PanePriceScaleStatusLine,
        9 => PlotDisplay::PaneDataWindow,
        10 => PlotDisplay::PriceScaleDataWindow,
        11 => PlotDisplay::PanePriceScaleDataWindow,
        12 => PlotDisplay::StatusLineDataWindow,
        13 => PlotDisplay::PaneStatusLineDataWindow,
        14 => PlotDisplay::PriceScaleStatusLineDataWindow,
        15 => PlotDisplay::PanePriceScaleStatusLineDataWindow,
        16 => PlotDisplay::PineScreener,
        17 => PlotDisplay::PanePineScreener,
        18 => PlotDisplay::PriceScalePineScreener,
        19 => PlotDisplay::PanePriceScalePineScreener,
        20 => PlotDisplay::StatusLinePineScreener,
        21 => PlotDisplay::PaneStatusLinePineScreener,
        22 => PlotDisplay::PriceScaleStatusLinePineScreener,
        23 => PlotDisplay::PanePriceScaleStatusLinePineScreener,
        24 => PlotDisplay::DataWindowPineScreener,
        25 => PlotDisplay::PaneDataWindowPineScreener,
        26 => PlotDisplay::PriceScaleDataWindowPineScreener,
        27 => PlotDisplay::PanePriceScaleDataWindowPineScreener,
        28 => PlotDisplay::StatusLineDataWindowPineScreener,
        29 => PlotDisplay::PaneStatusLineDataWindowPineScreener,
        30 => PlotDisplay::PriceScaleStatusLineDataWindowPineScreener,
        _ => return None,
    })
}

fn marker_shape(value: String) -> Option<MarkerShape> {
    Some(match value.as_str() {
        "shape.xcross" => MarkerShape::XCross,
        "shape.cross" => MarkerShape::Cross,
        "shape.circle" => MarkerShape::Circle,
        "shape.triangleup" => MarkerShape::TriangleUp,
        "shape.triangledown" => MarkerShape::TriangleDown,
        "shape.flag" => MarkerShape::Flag,
        "shape.arrowup" => MarkerShape::ArrowUp,
        "shape.arrowdown" => MarkerShape::ArrowDown,
        "shape.square" => MarkerShape::Square,
        "shape.diamond" => MarkerShape::Diamond,
        "shape.labelup" => MarkerShape::LabelUp,
        "shape.labeldown" => MarkerShape::LabelDown,
        _ => return None,
    })
}

fn marker_location(value: String) -> Option<MarkerLocation> {
    Some(match value.as_str() {
        "location.abovebar" => MarkerLocation::AboveBar,
        "location.belowbar" => MarkerLocation::BelowBar,
        "location.top" => MarkerLocation::Top,
        "location.bottom" => MarkerLocation::Bottom,
        "location.absolute" => MarkerLocation::Absolute,
        _ => return None,
    })
}

fn marker_size(value: String) -> Option<MarkerSize> {
    Some(match value.as_str() {
        "size.auto" | "auto" => MarkerSize::Auto,
        "size.tiny" | "tiny" => MarkerSize::Tiny,
        "size.small" | "small" => MarkerSize::Small,
        "size.normal" | "normal" => MarkerSize::Normal,
        "size.large" | "large" => MarkerSize::Large,
        "size.huge" | "huge" => MarkerSize::Huge,
        _ => return None,
    })
}

fn push_unsupported(style: &mut DisplayStyle, field: &str, reason: &str) {
    if style
        .unsupported
        .iter()
        .any(|entry| entry.field == field && entry.reason == reason)
    {
        return;
    }
    style.unsupported.push(UnsupportedVisualOption {
        field: field.to_string(),
        reason: reason.to_string(),
    });
}

fn is_const_style_expr(expr: &HirExpr) -> bool {
    matches!(
        expr.kind,
        HirExprKind::ConstInt(_)
            | HirExprKind::ConstFloat(_)
            | HirExprKind::ConstBool(_)
            | HirExprKind::ConstString(_)
    ) || matches!(
        &expr.kind,
        HirExprKind::IntToFloat(inner) if matches!(inner.kind, HirExprKind::ConstInt(_))
    ) || matches!(
        &expr.kind,
        HirExprKind::Unary { op: UnOp::Neg, operand }
            if const_f64_hir(operand).is_some()
    )
}

/// Turns a checked style argument into a host-resolvable input expression, or
/// `None` when it genuinely varies per bar. This is
/// [`Analyzer::input_active_expr_from_hir`] passed as a closure, because the
/// style helpers below are free functions shared by `plot` and the whole
/// display family.
type StyleBinder<'a> = dyn Fn(&HirExpr) -> Option<InputActiveExpr> + 'a;

/// Whether a binding is within the protocol's size limits: at most
/// [`pinewasm_hir::MAX_INPUT_ACTIVE_EXPRESSION_NODES`] nodes and
/// [`pinewasm_hir::MAX_INPUT_ACTIVE_EXPRESSION_DEPTH`] levels, counted as CSIL
/// validation counts them (the root is level 1). A larger binding would make
/// validation reject the whole program.
fn fits_binding_protocol_limits(expression: &InputActiveExpr) -> bool {
    fn visit(expr: &InputActiveExpr, depth: usize, nodes: &mut usize) -> bool {
        *nodes += 1;
        if *nodes > pinewasm_hir::MAX_INPUT_ACTIVE_EXPRESSION_NODES
            || depth > pinewasm_hir::MAX_INPUT_ACTIVE_EXPRESSION_DEPTH
        {
            return false;
        }
        match expr {
            InputActiveExpr::Value { .. } | InputActiveExpr::Na | InputActiveExpr::Input { .. } => {
                true
            }
            InputActiveExpr::Unary { operand, .. } => visit(operand, depth + 1, nodes),
            InputActiveExpr::Binary { lhs, rhs, .. } => {
                visit(lhs, depth + 1, nodes) && visit(rhs, depth + 1, nodes)
            }
            InputActiveExpr::Ternary {
                condition,
                when_true,
                when_false,
            } => {
                visit(condition, depth + 1, nodes)
                    && visit(when_true, depth + 1, nodes)
                    && visit(when_false, depth + 1, nodes)
            }
            InputActiveExpr::Call { args, .. } => {
                args.iter().all(|arg| visit(arg, depth + 1, nodes))
            }
        }
    }
    visit(expression, 1, &mut 0)
}

#[cfg(test)]
mod binding_limit_tests {
    use super::*;

    #[test]
    fn binding_limits_count_nodes_and_levels_like_validation() {
        let leaf = || InputActiveExpr::Input { input: 0 };
        // A left-leaning chain of `n` binary nodes has 2n + 1 nodes and
        // n + 1 levels.
        let chain = |n: usize| {
            (0..n).fold(leaf(), |lhs, _| InputActiveExpr::Binary {
                op: InputActiveBinaryOp::Add,
                lhs: Box::new(lhs),
                rhs: Box::new(leaf()),
            })
        };
        assert!(fits_binding_protocol_limits(&chain(63))); // 64 levels
        assert!(!fits_binding_protocol_limits(&chain(64))); // 65 levels
                                                            // A balanced tree of depth 8 has 255 nodes; one wrapper makes 256,
                                                            // two are past the limit.
        fn balanced(depth: u32) -> InputActiveExpr {
            if depth == 1 {
                return InputActiveExpr::Input { input: 0 };
            }
            InputActiveExpr::Binary {
                op: InputActiveBinaryOp::Add,
                lhs: Box::new(balanced(depth - 1)),
                rhs: Box::new(balanced(depth - 1)),
            }
        }
        let wrap = |expr: InputActiveExpr| InputActiveExpr::Unary {
            op: InputActiveUnaryOp::Not,
            operand: Box::new(expr),
        };
        assert!(fits_binding_protocol_limits(&balanced(8)));
        assert!(fits_binding_protocol_limits(&wrap(balanced(8))));
        assert!(!fits_binding_protocol_limits(&wrap(wrap(balanced(8)))));
    }
}

fn push_bound_scalar(style: &mut DisplayStyle, field: &str, expression: InputActiveExpr) {
    if style.bound_scalars.iter().any(|entry| entry.field == field) {
        return;
    }
    style.bound_scalars.push(BoundStyleScalar {
        field: field.to_string(),
        expression,
    });
}

/// Classify a non-literal *scalar* style argument.
///
/// An expression at or below the `input` qualifier holds one value for the
/// whole run. Bind it, and the host applies the resolved value before creating
/// the output. Only a `simple`- or `series`-qualified expression is a real gap,
/// because the portable display contract carries scalar style metadata once
/// per output rather than once per bar.
///
/// The qualifier is the gate, *not* whether the expression happens to be
/// serializable. An integer draw from 1 to 4 used as a width serializes fine (a
/// pure call over constants) yet redraws differently every bar, and binding it would
/// trade an honest report for a silently wrong width. Our registry declares
/// these parameters `Qualifier::Series`, so a stronger-than-`input` argument
/// really does reach here rather than being rejected upstream.
///
/// String-valued enum parameters use the same binding. Hosts decode their
/// canonical Pine tokens into the corresponding semantic enum. Display set
/// operations retain their expression tree so union/difference survives too.
fn bind_dynamic_style(
    style: &mut DisplayStyle,
    field: &str,
    supplied: bool,
    expr: Option<&HirExpr>,
    bind: &StyleBinder<'_>,
) {
    let Some(expr) = expr.filter(|_| supplied) else {
        return;
    };
    if is_const_style_expr(expr) {
        return;
    }
    // A binding larger than the protocol accepts is not bound: the engine
    // resolves the value after the run instead (`bind_runtime_style_fallbacks`).
    // Display arithmetic can multiply a few input choices into such a tree.
    let bound = (expr.qualifier <= Qualifier::Input)
        .then(|| bind(expr))
        .flatten()
        .filter(fits_binding_protocol_limits);
    match bound {
        Some(expression) => push_bound_scalar(style, field, expression),
        None => push_unsupported(
            style,
            field,
            "the portable display contract carries this metadata once per output, not per bar",
        ),
    }
}

fn style_bool(
    style: &mut DisplayStyle,
    field: &str,
    supplied: bool,
    expr: Option<&HirExpr>,
    bind: &StyleBinder<'_>,
    default: bool,
) -> bool {
    bind_dynamic_style(style, field, supplied, expr, bind);
    expr.and_then(const_bool_hir).unwrap_or(default)
}

fn style_float(
    style: &mut DisplayStyle,
    field: &str,
    supplied: bool,
    expr: Option<&HirExpr>,
    bind: &StyleBinder<'_>,
    default: f64,
) -> f64 {
    bind_dynamic_style(style, field, supplied, expr, bind);
    expr.and_then(const_f64_hir).unwrap_or(default)
}

fn style_i32(
    style: &mut DisplayStyle,
    field: &str,
    supplied: bool,
    expr: Option<&HirExpr>,
    bind: &StyleBinder<'_>,
    default: i32,
) -> i32 {
    bind_dynamic_style(style, field, supplied, expr, bind);
    match expr.and_then(const_i64_hir) {
        Some(value) if i32::try_from(value).is_ok() => value as i32,
        Some(_) => {
            push_unsupported(
                style,
                field,
                "value is outside the portable display contract's 32-bit range",
            );
            default
        }
        None => default,
    }
}

fn style_optional_u8(
    style: &mut DisplayStyle,
    field: &str,
    supplied: bool,
    expr: Option<&HirExpr>,
    bind: &StyleBinder<'_>,
    min: u8,
    max: u8,
) -> Option<u8> {
    bind_dynamic_style(style, field, supplied, expr, bind);
    match expr.and_then(const_i64_hir) {
        Some(value) if (min as i64..=max as i64).contains(&value) => Some(value as u8),
        Some(_) => {
            push_unsupported(
                style,
                field,
                "value is outside the portable display contract's supported range",
            );
            None
        }
        None => None,
    }
}

fn style_optional_u32(
    style: &mut DisplayStyle,
    field: &str,
    supplied: bool,
    expr: Option<&HirExpr>,
    bind: &StyleBinder<'_>,
) -> Option<u32> {
    bind_dynamic_style(style, field, supplied, expr, bind);
    match expr.and_then(const_i64_hir) {
        Some(value) if u32::try_from(value).is_ok() => Some(value as u32),
        Some(_) => {
            push_unsupported(
                style,
                field,
                "value is outside the portable display contract's non-negative range",
            );
            None
        }
        None => None,
    }
}

fn style_positive_u32(
    style: &mut DisplayStyle,
    field: &str,
    supplied: bool,
    expr: Option<&HirExpr>,
    bind: &StyleBinder<'_>,
) -> u32 {
    bind_dynamic_style(style, field, supplied, expr, bind);
    match expr.and_then(const_i64_hir) {
        Some(value) if value > 0 && u32::try_from(value).is_ok() => value as u32,
        Some(_) => {
            push_unsupported(
                style,
                field,
                "value is outside the positive 32-bit display range",
            );
            1
        }
        None => 1,
    }
}

fn plotchar_literal_glyph(value: &str) -> String {
    let mut scalars = value.chars();
    let Some(first) = scalars.next() else {
        return String::new();
    };
    let mut glyph = String::from(first);
    // A variation selector (U+FE0E or U+FE0F) right after the first character
    // is kept. Other suffixes are truncated to the first character.
    if let Some(selector @ ('\u{fe0e}' | '\u{fe0f}')) = scalars.next() {
        glyph.push(selector);
    }
    glyph
}

fn display_style_for_call(
    kind: DisplayKind,
    param_names: &[&str],
    bound: &[Option<ast::Expr>],
    checked: &[Option<HirExpr>],
    bind: &StyleBinder<'_>,
) -> DisplayStyle {
    let index = |name: &str| param_names.iter().position(|candidate| *candidate == name);
    let supplied = |name: &str| index(name).is_some_and(|i| bound[i].is_some());
    let arg = |name: &str| index(name).and_then(|i| checked[i].as_ref());

    let mut style = DisplayStyle {
        kind: match kind {
            DisplayKind::PlotShape => OutputDisplayKind::PlotShape,
            DisplayKind::PlotChar => OutputDisplayKind::PlotChar,
            DisplayKind::PlotArrow => OutputDisplayKind::PlotArrow,
            DisplayKind::PlotCandle => OutputDisplayKind::PlotCandle,
            DisplayKind::PlotBar => OutputDisplayKind::PlotBar,
            DisplayKind::Hline => OutputDisplayKind::Hline,
            DisplayKind::BgColor => OutputDisplayKind::Bgcolor,
            DisplayKind::BarColor => OutputDisplayKind::Barcolor,
            DisplayKind::Fill => OutputDisplayKind::Plot,
        },
        ..DisplayStyle::default()
    };

    style.offset = style_i32(
        &mut style,
        "offset",
        supplied("offset"),
        arg("offset"),
        bind,
        0,
    );
    style.show_last = style_optional_u32(
        &mut style,
        "show_last",
        supplied("show_last"),
        arg("show_last"),
        bind,
    );
    style.force_overlay = style_bool(
        &mut style,
        "force_overlay",
        supplied("force_overlay"),
        arg("force_overlay"),
        bind,
        false,
    );
    style.fill_gaps = style_bool(
        &mut style,
        "fillgaps",
        supplied("fillgaps"),
        arg("fillgaps"),
        bind,
        false,
    );
    style.editable = style_bool(
        &mut style,
        "editable",
        supplied("editable"),
        arg("editable"),
        bind,
        true,
    );
    style.display = arg("display").and_then(plot_display).unwrap_or_default();
    if arg("display").and_then(plot_display).is_none() {
        bind_dynamic_style(
            &mut style,
            "display",
            supplied("display"),
            arg("display"),
            bind,
        );
    }
    style.format = arg("format").and_then(const_string_hir);
    bind_dynamic_style(
        &mut style,
        "format",
        supplied("format"),
        arg("format"),
        bind,
    );
    style.precision = style_optional_u8(
        &mut style,
        "precision",
        supplied("precision"),
        arg("precision"),
        bind,
        0,
        16,
    );

    match kind {
        DisplayKind::PlotShape => {
            style.marker_shape = arg("style")
                .and_then(const_string_hir)
                .and_then(marker_shape)
                .or_else(|| (!supplied("style")).then_some(MarkerShape::XCross));
            style.marker_location = arg("location")
                .and_then(const_string_hir)
                .and_then(marker_location)
                .or_else(|| (!supplied("location")).then_some(MarkerLocation::AboveBar));
            style.marker_size = arg("size")
                .and_then(const_string_hir)
                .and_then(marker_size)
                .or_else(|| (!supplied("size")).then_some(MarkerSize::Auto));
            style.text = arg("text").and_then(const_string_hir);
        }
        DisplayKind::PlotChar => {
            style.character = arg("char").and_then(|expr| {
                if let HirExprKind::ConstString(value) = &expr.kind {
                    // Input strings retain their bound value for the
                    // renderer's separate runtime glyph selection.
                    return Some(plotchar_literal_glyph(value));
                }
                if expr.qualifier == Qualifier::Const {
                    if let Some(InputActiveExpr::Value {
                        value: InputValue::String(value),
                    }) = bind(expr)
                    {
                        // A present constant alias has the same compiler
                        // metadata coercion as a literal. Input-qualified
                        // aliases keep their full runtime binding below.
                        return Some(plotchar_literal_glyph(&value));
                    }
                }
                let mut source = index("char").and_then(|i| bound[i].as_ref())?;
                while let ast::Expr::Group { inner, .. } = source {
                    source = inner;
                }
                if matches!(source, ast::Expr::Na { .. }) {
                    return Some("n".to_string());
                }
                // A typed-na string argument (`string(na)` or an alias to it)
                // yields empty plotchar text. A bare `na` yields "n", handled above.
                (expr.ty == Type::String && matches!(bind(expr), Some(InputActiveExpr::Na)))
                    .then(String::new)
            });
            style.marker_location = arg("location")
                .and_then(const_string_hir)
                .and_then(marker_location)
                .or_else(|| (!supplied("location")).then_some(MarkerLocation::AboveBar));
            style.marker_size = arg("size")
                .and_then(const_string_hir)
                .and_then(marker_size)
                .or_else(|| (!supplied("size")).then_some(MarkerSize::Auto));
            style.text = arg("text").and_then(const_string_hir);
        }
        DisplayKind::PlotArrow => {
            // Presence survives separately from defaulted runtime color lanes.
            // The final source-order pass resolves supplied expressions.
            let role = |up, supplied| pinewasm_hir::ArrowStaticRoleColor {
                origin: if supplied {
                    pinewasm_hir::ArrowStaticColorOrigin::Dynamic
                } else {
                    pinewasm_hir::ArrowStaticColorOrigin::Omitted
                },
                r: if up { 0 } else { 255 },
                g: if up { 255 } else { 0 },
                b: 0,
                t: 0.0,
            };
            style.arrow_static_colors = Some(pinewasm_hir::ArrowStaticColors {
                up: role(true, supplied("colorup")),
                down: role(false, supplied("colordown")),
            });
            style.min_height = style_optional_u32(
                &mut style,
                "minheight",
                supplied("minheight"),
                arg("minheight"),
                bind,
            );
            style.max_height = style_optional_u32(
                &mut style,
                "maxheight",
                supplied("maxheight"),
                arg("maxheight"),
                bind,
            );
        }
        DisplayKind::Hline => {
            style.line_width = style_positive_u32(
                &mut style,
                "linewidth",
                supplied("linewidth"),
                arg("linewidth"),
                bind,
            );
            style.line_style = arg("linestyle")
                .and_then(const_string_hir)
                .and_then(plot_line_style)
                .unwrap_or_default();
        }
        DisplayKind::PlotCandle | DisplayKind::PlotBar => {
            // An omitted color is recorded as a present null property, distinct
            // from a supplied expression that may yield no value.
            style.ohlc_body_style = Some(pinewasm_hir::OhlcBodyStyle {
                static_color_present: true,
                static_color: None,
                runtime_colorer: false,
            });
        }
        DisplayKind::BgColor | DisplayKind::BarColor | DisplayKind::Fill => {}
    }
    for field in ["style", "location", "size", "char", "text", "linestyle"] {
        if field == "char" && style.character.is_some() {
            continue;
        }
        bind_dynamic_style(&mut style, field, supplied(field), arg(field), bind);
    }
    style
}

/// The length of a timeframe string in seconds, for the compile-time
/// `request.security` ordering check. MUST agree with
/// `pinewasm_vm::timeframe_in_seconds` (the runtime canonical) — this crate
/// is upstream of the VM and cannot depend on it, so the small parser is
/// mirrored here; the agreement is a documented invariant. Recognizes the
/// second (`"30S"`), minute (bare `"60"`), and day/week/month
/// (`"1D"`/`"1W"`/`"1M"`, `1M` = 30 days) forms; an unrecognized string falls
/// back to one day, matching the runtime.
fn timeframe_seconds(tf: &str) -> i64 {
    let tf = tf.trim();
    if tf.is_empty() {
        return 86_400;
    }
    let (num_part, unit) = match tf.chars().last() {
        Some(c) if c.is_ascii_alphabetic() => (&tf[..tf.len() - 1], Some(c.to_ascii_uppercase())),
        _ => (tf, None),
    };
    // `saturating_mul` so an absurd numeral (e.g. "999999999999999M") can never
    // overflow `i64` into a panic (debug/test builds keep overflow-checks on);
    // it saturates to `i64::MAX`, which the caller treats as out of range.
    let mult: i64 = num_part.parse().unwrap_or(1).max(1);
    match unit {
        None => mult.saturating_mul(60),
        Some('S') => mult,
        Some('D') => mult.saturating_mul(86_400),
        Some('W') => mult.saturating_mul(7).saturating_mul(86_400),
        Some('M') => mult.saturating_mul(30).saturating_mul(86_400),
        Some(_) => 86_400,
    }
}

/// Encode an object element type's `type_id` as the collection element-type tag
/// passed through the CSIL. MUST agree with
/// `pinewasm_mir::heap::ElemType::{to_tag, from_tag}` (object tags are
/// `OBJECT_TAG_BASE + type_id`, `OBJECT_TAG_BASE = 1000`). Kept here because
/// `pinewasm_semantics` is upstream of `pinewasm_mir` and cannot depend on
/// its `ElemType`; the agreement is covered by a round-trip test.
fn object_elem_tag(type_id: u32) -> i64 {
    1000 + type_id as i64
}

/// Resolve a scalar element-type name (`int`/`float`/`bool`/`string`/`color`)
/// to its [`Type`], or `None` when the name is not a scalar type.
fn scalar_type_from_name(name: &str) -> Option<Type> {
    match name {
        "int" => Some(Type::Int),
        "float" => Some(Type::Float),
        "bool" => Some(Type::Bool),
        "string" => Some(Type::String),
        "color" => Some(Type::Color),
        _ => None,
    }
}

/// The canonical string key for a method's declared receiver [`ast::FieldType`].
/// Agrees with [`Type::describe`] for the same underlying type, so a method
/// declared on `array<float>` (or a UDT `Point`) is found from a receiver
/// value of that type.
fn fmt_field_type_key(ty: &ast::FieldType) -> String {
    match &ty.kind {
        ast::FieldTypeKind::Scalar(a) => a.keyword().to_string(),
        ast::FieldTypeKind::Color => "color".to_string(),
        ast::FieldTypeKind::Array(elem) => format!("array<{}>", elem.describe()),
        ast::FieldTypeKind::Matrix(elem) => format!("matrix<{}>", elem.describe()),
        ast::FieldTypeKind::Map(key, value) => {
            format!("map<{}, {}>", key.describe(), value.describe())
        }
        ast::FieldTypeKind::Named(name) => name.clone(),
    }
}

/// Whether two definitions share a signature, which makes the second one a
/// redefinition rather than an overload: the same parameter count and, at
/// each position, the same declared type (an untyped parameter matches only
/// another untyped one). The compiler applies the same rule to each imported
/// library's functions.
pub fn same_signature(a: &ast::FuncDef, b: &ast::FuncDef) -> bool {
    a.params.len() == b.params.len()
        && a.params.iter().zip(&b.params).all(|(x, y)| {
            x.ty.as_ref().map(fmt_field_type_key) == y.ty.as_ref().map(fmt_field_type_key)
        })
}

/// Why [`Analyzer::select_overload`] found no single overload for a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverloadMiss {
    /// No overload can bind the arguments, or every one that can declares a
    /// type that a known argument type does not match.
    NoneFits,
    /// Several overloads remain, and the argument types known before analysis
    /// do not single one out.
    Ambiguous,
}

/// The index of the parameter of `def` that the argument at `index` binds to:
/// by position, or by name for a named argument. Returning an index keeps the
/// resolver's source-level binding decision independent of borrowed AST
/// lifetimes; `param_for_arg` below preserves the borrowing interface.
fn param_index_for_arg(def: &ast::FuncDef, index: usize, arg: &ast::CallArg) -> Option<usize> {
    match &arg.name {
        None => {
            if index < def.params.len() {
                Some(index)
            } else {
                None
            }
        }
        Some(name) => def.params.iter().position(|p| p.name.name == name.name),
    }
}

/// The parameter of `def` that the argument at `index` binds to: by position,
/// or by name for a named argument.
fn param_for_arg<'d>(
    def: &'d ast::FuncDef,
    index: usize,
    arg: &ast::CallArg,
) -> Option<&'d ast::FuncParam> {
    param_index_for_arg(def, index, arg).and_then(|param_index| def.params.get(param_index))
}

#[cfg(test)]
mod param_index_for_arg_tests {
    use super::{ast, param_for_arg, param_index_for_arg, Span};

    fn ident(name: &str) -> ast::Ident {
        ast::Ident {
            name: name.to_owned(),
            span: Span::dummy(),
        }
    }

    fn definition(names: &[&str]) -> ast::FuncDef {
        ast::FuncDef {
            name: ident("f"),
            params: names
                .iter()
                .map(|name| ast::FuncParam {
                    name: ident(name),
                    qualifier: None,
                    ty: None,
                    default: None,
                    span: Span::dummy(),
                })
                .collect(),
            body: Vec::new(),
            receiver: None,
            exported: false,
            span: Span::dummy(),
        }
    }

    fn arg(name: Option<&str>) -> ast::CallArg {
        ast::CallArg {
            name: name.map(ident),
            value: ast::Expr::Int {
                value: 1,
                span: Span::dummy(),
            },
            span: Span::dummy(),
        }
    }

    #[test]
    fn resolves_positional_indices_and_refuses_out_of_range() {
        let def = definition(&["a", "b"]);
        assert_eq!(param_index_for_arg(&def, 0, &arg(None)), Some(0));
        assert_eq!(param_index_for_arg(&def, 1, &arg(None)), Some(1));
        assert_eq!(param_index_for_arg(&def, 2, &arg(None)), None);
        assert_eq!(
            param_for_arg(&def, 0, &arg(None)).map(|p| p.name.name.as_str()),
            Some("a")
        );
    }

    #[test]
    fn resolves_named_indices_and_selects_the_first_duplicate() {
        let def = definition(&["a", "b", "a"]);
        assert_eq!(param_index_for_arg(&def, 0, &arg(Some("b"))), Some(1));
        assert_eq!(param_index_for_arg(&def, 0, &arg(Some("a"))), Some(0));
        assert_eq!(param_index_for_arg(&def, 0, &arg(Some("missing"))), None);
    }

    #[test]
    fn index_helper_matches_original_lookup_for_small_definition_grid() {
        fn original(def: &ast::FuncDef, index: usize, arg: &ast::CallArg) -> Option<usize> {
            match &arg.name {
                None => def.params.get(index).map(|_| index),
                Some(name) => def
                    .params
                    .iter()
                    .position(|param| param.name.name == name.name),
            }
        }

        let definitions = [
            &[][..],
            &["a"][..],
            &["a", "b"][..],
            &["a", "b", "a"][..],
            &["b", "a", "c"][..],
        ];
        let argument_names = [None, Some("a"), Some("b"), Some("c"), Some("missing")];
        for names in definitions {
            let def = definition(names);
            for index in 0..=4 {
                for name in argument_names {
                    let call_arg = arg(name);
                    assert_eq!(
                        param_index_for_arg(&def, index, &call_arg),
                        original(&def, index, &call_arg),
                        "params {names:?}, argument index {index}, name {name:?}"
                    );
                }
            }
        }
    }
}

/// How well arguments of the given (possibly unknown) `types` match `def`'s
/// declared parameter types: 2 for each exact match, 1 for each `int`
/// argument to a `float` parameter, nothing when either side's type is
/// unknown, and `None` for a known mismatch.
fn overload_type_score(
    def: &ast::FuncDef,
    args: &[ast::CallArg],
    types: &[Option<Type>],
) -> Option<u32> {
    let mut score = 0;
    for (index, arg) in args.iter().enumerate() {
        let Some(declared) = param_for_arg(def, index, arg).and_then(|p| p.ty.as_ref()) else {
            continue;
        };
        let Some(actual) = &types[index] else {
            continue;
        };
        let declared = fmt_field_type_key(declared);
        let actual = Analyzer::value_type_key(actual);
        if declared == actual {
            score += 2;
        } else if declared == "float" && actual == "int" {
            score += 1;
        } else {
            return None;
        }
    }
    Some(score)
}

/// The type of `expr` when it is built from literals alone and is well typed:
/// a literal, or a sign, `not`, arithmetic, a comparison, `and`/`or` or `?:`
/// over such expressions, where every operator has operands it takes. The
/// result is the type the analyzer gives the expression (`int / int` is
/// `int`, `1 < 2` is `bool`).
///
/// `None` for anything else: `na`, a name, a call, or an operator applied to
/// operands it does not take (`1 and 2`, `"a" < "b"`). Such an expression has
/// no type until the analyzer evaluates it, and what is wrong with it is then
/// the analyzer's to say.
fn literal_type(expr: &ast::Expr) -> Option<Type> {
    match expr {
        ast::Expr::Int { .. } => Some(Type::Int),
        ast::Expr::Float { .. } => Some(Type::Float),
        ast::Expr::Bool { .. } => Some(Type::Bool),
        ast::Expr::Str { .. } => Some(Type::String),
        ast::Expr::Color { .. } => Some(Type::Color),
        ast::Expr::Group { inner, .. } => literal_type(inner),
        ast::Expr::Unary { op, operand, .. } => {
            let operand = literal_type(operand)?;
            match op {
                ast::UnaryOp::Not => (operand == Type::Bool).then_some(Type::Bool),
                ast::UnaryOp::Neg | ast::UnaryOp::Pos => is_numeric(&operand).then_some(operand),
            }
        }
        ast::Expr::Binary { op, lhs, rhs, .. } => {
            let (lhs, rhs) = (literal_type(lhs)?, literal_type(rhs)?);
            let both_numeric = is_numeric(&lhs) && is_numeric(&rhs);
            match op {
                ast::BinaryOp::Add if lhs == Type::String && rhs == Type::String => {
                    Some(Type::String)
                }
                ast::BinaryOp::Add
                | ast::BinaryOp::Sub
                | ast::BinaryOp::Mul
                | ast::BinaryOp::Div
                | ast::BinaryOp::Rem => both_numeric.then(|| {
                    if lhs == Type::Int && rhs == Type::Int {
                        Type::Int
                    } else {
                        Type::Float
                    }
                }),
                ast::BinaryOp::Lt | ast::BinaryOp::Le | ast::BinaryOp::Gt | ast::BinaryOp::Ge => {
                    both_numeric.then_some(Type::Bool)
                }
                ast::BinaryOp::Eq | ast::BinaryOp::Ne => (both_numeric
                    || (lhs == rhs && matches!(lhs, Type::Bool | Type::String | Type::Color)))
                .then_some(Type::Bool),
                ast::BinaryOp::And | ast::BinaryOp::Or => {
                    (lhs == Type::Bool && rhs == Type::Bool).then_some(Type::Bool)
                }
            }
        }
        ast::Expr::Ternary {
            cond,
            then_e,
            else_e,
            ..
        } => {
            if literal_type(cond)? != Type::Bool {
                return None;
            }
            match (literal_type(then_e)?, literal_type(else_e)?) {
                (then_ty, else_ty) if then_ty == else_ty => Some(then_ty),
                (then_ty, else_ty) if is_numeric(&then_ty) && is_numeric(&else_ty) => {
                    Some(Type::Float)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Whether `args` can bind to `def`'s parameters: positional arguments fill
/// the leading parameters, each named argument names a parameter not already
/// filled, and every parameter left unfilled has a default.
fn accepts_call_args(def: &ast::FuncDef, args: &[ast::CallArg]) -> bool {
    let positional = args.iter().take_while(|a| a.name.is_none()).count();
    if positional > def.params.len() {
        return false;
    }
    let mut filled = vec![false; def.params.len()];
    filled[..positional].fill(true);
    for arg in &args[positional..] {
        let Some(name) = &arg.name else {
            return false;
        };
        match def.params.iter().position(|p| p.name.name == name.name) {
            Some(i) if !filled[i] => filled[i] = true,
            _ => return false,
        }
    }
    def.params
        .iter()
        .zip(&filled)
        .all(|(param, filled)| *filled || param.default.is_some())
}

/// Prefix of an identifier the compiler builds to read a built-in variable
/// (see [`builtin_ident`]). `@` cannot start a source identifier.
pub(crate) const BUILTIN_IDENT_PREFIX: &str = "@builtin:";

/// An identifier that always reads the built-in variable `name`, even where
/// the script declares a variable of the same name, and that does not count
/// as the script using the built-in.
pub(crate) fn builtin_ident(name: &str, span: Span) -> ast::Ident {
    ast::Ident {
        name: format!("{BUILTIN_IDENT_PREFIX}{name}"),
        span,
    }
}

/// `_` names an unused variable: a script may declare any number of
/// them, several in one scope, and can never read or reassign one. A `_`
/// declaration still evaluates its initializer; the variable never enters a
/// scope.
pub(crate) const DISCARD_NAME: &str = "_";

/// Type-cast builtin names. Kept separate from
/// [`BuiltinRegistry`] because several names are also lexer type keywords.
pub const CAST_FUNCTION_NAMES: [&str; 10] = [
    "int", "float", "bool", "string", "color", "line", "label", "box", "table", "linefill",
];

/// Result type of a cast builtin, or `None` for a non-cast name.
pub fn cast_result_type(name: &str) -> Option<Type> {
    match name {
        "int" => Some(Type::Int),
        "float" => Some(Type::Float),
        "bool" => Some(Type::Bool),
        "string" => Some(Type::String),
        "color" => Some(Type::Color),
        drawing => {
            DrawKind::from_cast_name(drawing).map(|kind| Type::Object(kind.type_name().to_string()))
        }
    }
}

/// Whether `name` is one of the type-cast builtins: the
/// scalar casts `int`/`float`/`bool`/`string`/`color` and the drawing casts
/// `line`/`label`/`box`/`table`/`linefill`. `polyline` is not a cast in this
/// compiler and is excluded.
fn is_cast_name(name: &str) -> bool {
    cast_result_type(name).is_some()
}

fn annotated_type_to_type(ty: ast::AnnotatedType) -> Type {
    match ty {
        ast::AnnotatedType::Int => Type::Int,
        ast::AnnotatedType::Float => Type::Float,
        ast::AnnotatedType::Bool => Type::Bool,
        ast::AnnotatedType::String => Type::String,
    }
}

/// Note attached to unresolved dotted names (`E0201`/`E0202`).
const NAMESPACE_NOTE: &str =
    "namespaced built-ins look like `ta.sma(...)`; field access is not supported";

/// How [`binder::arg_shapes`] reads a call argument: its name, compared with
/// the parameter names.
impl binder::ArgName<&str> for ast::CallArg {
    fn is_named(&self) -> bool {
        self.name.is_some()
    }

    fn names(&self, param: &&str) -> bool {
        match &self.name {
            Some(name) => *param == name.name,
            None => false,
        }
    }
}

#[cfg(test)]
mod binder_reference;
#[cfg(test)]
mod def_arity_sweep;
#[cfg(test)]
mod diagnostic_limit_tests;
#[cfg(test)]
mod inner_warning_tests;
#[cfg(test)]
mod tests;

// Extraction-only roots expose the concrete `ArgName<&str>` methods;
// normal builds exclude this module.
#[cfg(charon)]
mod aeneas_argname_roots;
