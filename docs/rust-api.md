# Rust API support and stability

This document is the source of truth for which Rust interfaces PineWASM asks
embedders to depend on. A Rust item being `pub` is not, by itself, a support
promise: most public items connect crates inside this workspace and are visible
only because those crates are independently compiled.

The initial supported surface is intentionally small. It gives native callers
one compiler/runtime facade while leaving the parser, intermediate
representations, virtual machine implementation, AOT emitter, and host adapters
free to evolve together.

## Classification

| Classification | Meaning |
| --- | --- |
| **Supported Rust facade** | Intended for native embedding. Its documented crate-root API is reviewed as one surface and denies missing documentation for items defined at that boundary. |
| **Supported non-Rust contract** | A protocol, generated package, binary, or artifact ABI has its own documented compatibility rules; the implementing crate's Rust API is not thereby supported. |
| **Workspace composition** | Public so another workspace crate can use it. Downstream Rust source compatibility is not promised. |
| **Repository tooling** | Used to build, test, or release this repository; not an embedding library. |

## Complete crate inventory

All workspace members are classified below. “Supported module” means the
precise Rust module path covered by the support boundary.

| Crate | Classification | Supported module | Reason |
| --- | --- | --- | --- |
| `pinewasm_compiler` | **Supported Rust facade** | crate root (`pinewasm_compiler`) | Native compile, validated-program cache, one-shot execution, incremental execution, version metadata, and the host-facing data types those operations require. |
| `pinewasm_bindings` | **Supported non-Rust contract** | none | Implements the generated JavaScript/WebAssembly package. Its supported surface is the generated package and TypeScript declarations described in [browser.md](browser.md), not imports from this Rust crate. |
| `pinewasm_lsp` | **Supported non-Rust contract** | none | The `pinewasm` binary and Language Server Protocol behavior are documented in [language-server.md](language-server.md). Its Rust library modules are implementation details shared with the Wasm adapter. |
| `pinewasm_aot` | **Workspace composition** | none | The direct Rust emitter API is not yet a stability boundary. Emitted modules follow the independently versioned [CSAW module ABI](spec/csaw-module-abi.md); the ABI contract does not make every emitter module public API. |
| `pinewasm_search` | **Workspace composition** | none | Bounded in-memory search implementation used through the host binding. |
| `pinewasm_migrate` | **Workspace composition** | none | PineWASM's own Pine v5 → v6 source migration used by the `pinewasm migrate` command, the language server, and the optional `migrateToV6` binding; its behavior is documented in [migration.md](migration.md). |
| `pinewasm_num` | **Workspace composition** | none | Shared numeric implementation for the VM and AOT runtime. |
| `pinewasm_ta` | **Workspace composition** | none | Shared stateful technical-analysis engine. |
| `pinewasm_tz` | **Workspace composition** | none | Shared time zone rules (pinned IANA release), fixed offsets and session shapes. |
| `pinewasm_broker` | **Workspace composition** | none | Shared strategy broker and report model. Host-facing report types are re-exported by `pinewasm_compiler`. |
| `pinewasm_span` | **Workspace composition** | none | Source-location and diagnostic implementation. Host-facing diagnostic types are re-exported by `pinewasm_compiler`. |
| `pinewasm_syntax` | **Workspace composition** | none | Parser-owned abstract syntax tree. |
| `pinewasm_parse` | **Workspace composition** | none | Lexer and parser implementation. |
| `pinewasm_hir` | **Workspace composition** | none | Resolved high-level representation and shared language metadata. Required host metadata types are re-exported by `pinewasm_compiler`. |
| `pinewasm_semantics` | **Workspace composition** | none | Name resolution, type checking, and HIR lowering. |
| `pinewasm_mir` | **Workspace composition** | none | Mid-level control-flow representation and runtime value implementation. `Value` and `HandleId` are re-exported by the facade. |
| `pinewasm_csil` | **Workspace composition** | none | Bytecode, validator, and lowering implementation. The compiled `Program` type is re-exported by the facade. |
| `pinewasm_vm` | **Workspace composition** | none | PineWASM VM implementation. Host inputs, options, outputs, and errors are re-exported by the facade. |
| `pinewasm_xtask` | **Repository tooling** | none | Maintainer-only build, validation, packaging, and release automation. |

The separately built `crates/pinewasm_aot/runtime` package is not a workspace
member or an embedding API. Its checked-in Wasm binaries are implementation
components of emitted artifacts.

## Supported facade inventory

`pinewasm_compiler` has no public child modules. The entire supported Rust
surface lives at its crate root and is grouped as follows:

| Surface | Items |
| --- | --- |
| Compilation | `compile`, `compile_with_libraries`, `compile_with_libraries_profiled`, `CompileOptions`, `CompileResult`, `PhaseTimings`, `NowMs`, and compiler input-size constants |
| Execution | `compile_and_run`, `compile_and_run_with_libraries`, `compile_and_run_with_libraries_profiled`, `run_program`, `run_program_profiled`, `RunResult`, and `IncrementalSession` |
| Artifact and version identity | `Program`, `COMPILER_VERSION`, `VersionInfo`, `version_info`, and `rendering_contract` |
| Builtin metadata | `builtin_catalog`: the builtin tables the compiler consults (registry signatures, TA engine signatures, collection call surfaces, drawing-call overloads, builtin series, namespaced constants, type casts, `footprint`/`volume_row` accessors, `strategy()` declaration parameters) as one JSON document, each table under its own key. Calls analyzed without a table (`request.footprint`, `library()`, `footprint.get_row_by_price`) are not listed. Its layout is versioned by `formatVersion` and is tooling metadata, not a stable host contract |
| Host inputs and controls | `MarketData`, `SymInfo`, `UpcomingEvents`, `HostFootprint`, `HostVolumeRow`, `RunOptions`, `RealtimeSim`, `Tick`, input metadata/override types, external-source types, strategy overrides, and default resource-limit constants |
| Host outputs and failures | diagnostics/source locations, `Value`/`HandleId`, plot/visual/fill metadata and lanes, drawings, logs, alerts, strategy outcomes/report types, VM profiles, and runtime error types |

`MarketData::validate` exposes the same lane and metadata checks that the
interpreter and incremental-session constructors apply before execution. A
host building a linked runtime can use it to reject malformed auxiliary
request lanes before creating runtime state.

<details>
<summary>Exact supported compiler-root item manifest</summary>

<!-- rust-api-supported-root-items-begin -->
```text
const COMPILER_VERSION
const MAX_LIBRARY_COUNT
const MAX_LIBRARY_PATH_LEN
const MAX_LIBRARY_SOURCE_BYTES
const MAX_SOURCE_LEN
fn builtin_catalog
fn compile
fn compile_and_run
fn compile_and_run_with_libraries
fn compile_and_run_with_libraries_profiled
fn compile_with_libraries
fn compile_with_libraries_profiled
fn rendering_contract
fn run_program
fn run_program_profiled
fn version_info
reexport AlertEvent = pinewasm_vm::AlertEvent
reexport ArrowRuntimeColorers = pinewasm_hir::ArrowRuntimeColorers
reexport ArrowStaticColorOrigin = pinewasm_hir::ArrowStaticColorOrigin
reexport ArrowStaticColors = pinewasm_hir::ArrowStaticColors
reexport ArrowStaticRoleColor = pinewasm_hir::ArrowStaticRoleColor
reexport BoundStyleScalar = pinewasm_hir::BoundStyleScalar
reexport BuiltinSeries = pinewasm_hir::builtins::BuiltinSeries
reexport DEFAULT_MAX_COLLECTION_ELEMENTS = pinewasm_mir::heap::DEFAULT_MAX_COLLECTION_ELEMENTS
reexport DEFAULT_MAX_INSTRUCTIONS_PER_BAR = pinewasm_vm::DEFAULT_MAX_INSTRUCTIONS_PER_BAR
reexport DEFAULT_MAX_INSTRUCTIONS_PER_RUN = pinewasm_vm::DEFAULT_MAX_INSTRUCTIONS_PER_RUN
reexport DEFAULT_MAX_STACK_DEPTH = pinewasm_vm::DEFAULT_MAX_STACK_DEPTH
reexport DEFAULT_MAX_STRING_BYTES_PER_RUN = pinewasm_vm::DEFAULT_MAX_STRING_BYTES_PER_RUN
reexport DEFAULT_MAX_VM_BYTES_PER_RUN = pinewasm_vm::DEFAULT_MAX_VM_BYTES_PER_RUN
reexport Diagnostic = pinewasm_span::Diagnostic
reexport DisplayKind = pinewasm_hir::DisplayKind
reexport DisplayStyle = pinewasm_hir::DisplayStyle
reexport DrawingObject = pinewasm_vm::DrawingObject
reexport Drawings = pinewasm_vm::Drawings
reexport ExternalSourceLane = pinewasm_vm::ExternalSourceLane
reexport ExternalSourceRef = pinewasm_hir::ExternalSourceRef
reexport FillSeries = pinewasm_vm::FillSeries
reexport HandleId = pinewasm_mir::value::HandleId
reexport HostFootprint = pinewasm_vm::HostFootprint
reexport HostVolumeRow = pinewasm_vm::HostVolumeRow
reexport InputActive = pinewasm_hir::InputActive
reexport InputActiveBinaryOp = pinewasm_hir::InputActiveBinaryOp
reexport InputActiveDirect = pinewasm_hir::InputActiveDirect
reexport InputActiveExpr = pinewasm_hir::InputActiveExpr
reexport InputActiveUnaryOp = pinewasm_hir::InputActiveUnaryOp
reexport InputConstraints = pinewasm_hir::InputConstraints
reexport InputDecl = pinewasm_hir::InputDecl
reexport InputDisplay = pinewasm_hir::InputDisplay
reexport InputKind = pinewasm_hir::InputKind
reexport InputValue = pinewasm_hir::InputValue
reexport InputWidget = pinewasm_hir::InputWidget
reexport Label = pinewasm_span::Label
reexport LogEntry = pinewasm_vm::LogEntry
reexport LogLevel = pinewasm_hir::LogLevel
reexport MAX_INCREMENTAL_BARS = pinewasm_vm::MAX_INCREMENTAL_BARS
reexport MarkerLocation = pinewasm_hir::MarkerLocation
reexport MarkerShape = pinewasm_hir::MarkerShape
reexport MarkerSize = pinewasm_hir::MarkerSize
reexport MarketData = pinewasm_vm::MarketData
reexport OhlcBodyColor = pinewasm_hir::OhlcBodyColor
reexport OhlcBodyStyle = pinewasm_hir::OhlcBodyStyle
reexport OhlcField = pinewasm_hir::OhlcField
reexport PlotColorRole = pinewasm_hir::PlotColorRole
reexport PlotColorSeries = pinewasm_vm::PlotColorSeries
reexport PlotDisplay = pinewasm_hir::PlotDisplay
reexport PlotLineStyle = pinewasm_hir::PlotLineStyle
reexport PlotSeries = pinewasm_vm::PlotSeries
reexport PlotStyleKind = pinewasm_hir::PlotStyleKind
reexport PlotTickSample = pinewasm_vm::PlotTickSample
reexport PolyPointDto = pinewasm_vm::PolyPointDto
reexport Program = pinewasm_csil::CsilProgram
reexport RealtimeSim = pinewasm_vm::RealtimeSim
reexport RequestTimeframeBars = pinewasm_vm::RequestTimeframeBars
reexport RunOptions = pinewasm_vm::RunOptions
reexport RuntimeError = pinewasm_vm::RuntimeError
reexport RuntimeErrorKind = pinewasm_vm::RuntimeErrorKind
reexport Severity = pinewasm_span::Severity
reexport SourceId = pinewasm_span::SourceId
reexport Span = pinewasm_span::Span
reexport StrategyOutcome = pinewasm_vm::StrategyOutcome
reexport StrategyPosition = pinewasm_broker::StrategyPosition
reexport StrategyReportAggregates = pinewasm_broker::StrategyReportAggregates
reexport StrategyReportOutliers = pinewasm_broker::StrategyReportOutliers
reexport StrategyReportScopeAggregate = pinewasm_broker::StrategyReportScopeAggregate
reexport StrategyReportTradeHighlight = pinewasm_broker::StrategyReportTradeHighlight
reexport StrategyRunOverrides = pinewasm_vm::StrategyRunOverrides
reexport StrategyTrade = pinewasm_broker::StrategyTrade
reexport SymInfo = pinewasm_vm::SymInfo
reexport TaInputBinding = pinewasm_hir::builtins::TaInputBinding
reexport TaInputTarget = pinewasm_hir::builtins::TaInputTarget
reexport TableCellDto = pinewasm_vm::TableCellDto
reexport Tick = pinewasm_vm::Tick
reexport UnsupportedVisualOption = pinewasm_hir::UnsupportedVisualOption
reexport UpcomingEvents = pinewasm_vm::UpcomingEvents
reexport Value = pinewasm_mir::value::Value
reexport VisualSeries = pinewasm_vm::VisualSeries
reexport VmProfile = pinewasm_vm::VmProfile
struct CompileOptions
struct CompileResult
struct IncrementalSession
struct PhaseTimings
struct PreparedProgram
struct RunResult
struct VersionInfo
type NowMs < 'a > = & 'a dyn Fn () -> f64
```
<!-- rust-api-supported-root-items-end -->

</details>

<details>
<summary>Exact supported facade-member manifest</summary>

<!-- rust-api-supported-members-begin -->
```text
associated-const BuiltinSeries::ALL
associated-const SourceId::MAIN
field AlertEvent::bar
field AlertEvent::freq
field AlertEvent::message
field ArrowRuntimeColorers::down
field ArrowRuntimeColorers::up
field ArrowStaticColors::down
field ArrowStaticColors::up
field ArrowStaticRoleColor::b
field ArrowStaticRoleColor::g
field ArrowStaticRoleColor::origin
field ArrowStaticRoleColor::r
field ArrowStaticRoleColor::t
field BoundStyleScalar::expression
field BoundStyleScalar::field
field CompileOptions::chart_background
field CompileOptions::chart_foreground
field CompileOptions::disable_distance_scan_optimization
field CompileOptions::emit_ast
field CompileOptions::emit_csil
field CompileOptions::emit_hir
field CompileOptions::emit_mir
field CompileOptions::validate_passes
field CompileResult::ast_debug
field CompileResult::csil_text
field CompileResult::diagnostics
field CompileResult::hir_debug
field CompileResult::mir_debug
field CompileResult::program
field CompileResult::success
field CompileResult::timings
field Diagnostic::code
field Diagnostic::labels
field Diagnostic::message
field Diagnostic::notes
field Diagnostic::severity
field DisplayStyle::arrow_runtime_colorers
field DisplayStyle::arrow_static_colors
field DisplayStyle::bound_scalars
field DisplayStyle::character
field DisplayStyle::color_editable
field DisplayStyle::color_palette
field DisplayStyle::display
field DisplayStyle::editable
field DisplayStyle::fill_color_mode
field DisplayStyle::fill_gaps
field DisplayStyle::force_overlay
field DisplayStyle::format
field DisplayStyle::group
field DisplayStyle::histogram_base
field DisplayStyle::join
field DisplayStyle::kind
field DisplayStyle::line_style
field DisplayStyle::line_width
field DisplayStyle::marker_location
field DisplayStyle::marker_shape
field DisplayStyle::marker_size
field DisplayStyle::max_height
field DisplayStyle::min_height
field DisplayStyle::offset
field DisplayStyle::ohlc_body_style
field DisplayStyle::ohlc_field
field DisplayStyle::plot_color_mode
field DisplayStyle::plot_style
field DisplayStyle::precision
field DisplayStyle::runtime_scalars
field DisplayStyle::show_last
field DisplayStyle::source_order
field DisplayStyle::text
field DisplayStyle::track_price
field DisplayStyle::unsupported
field DrawingObject::cells
field DrawingObject::fields
field DrawingObject::id
field DrawingObject::kind
field DrawingObject::line_refs
field DrawingObject::points
field Drawings::boxes
field Drawings::labels
field Drawings::linefills
field Drawings::lines
field Drawings::polylines
field Drawings::tables
field ExternalSourceLane::source
field ExternalSourceLane::values
field ExternalSourceRef::plot_label
field ExternalSourceRef::plot_slot
field ExternalSourceRef::script_id
field FillSeries::colors
field FillSeries::plot1
field FillSeries::plot2
field FillSeries::slot
field FillSeries::style
field FillSeries::title
field HandleId::0
field HostFootprint::buy_volume
field HostFootprint::delta
field HostFootprint::poc
field HostFootprint::rows
field HostFootprint::sell_volume
field HostFootprint::total_volume
field HostFootprint::vah
field HostFootprint::val
field HostVolumeRow::buy_volume
field HostVolumeRow::delta
field HostVolumeRow::down_price
field HostVolumeRow::has_buy_imbalance
field HostVolumeRow::has_sell_imbalance
field HostVolumeRow::sell_volume
field HostVolumeRow::total_volume
field HostVolumeRow::up_price
field InputActive::default
field InputActive::dependencies
field InputActive::direct
field InputActive::expression
field InputActiveDirect::input
field InputActiveDirect::inverted
field InputActiveExpr::Binary.lhs
field InputActiveExpr::Binary.op
field InputActiveExpr::Binary.rhs
field InputActiveExpr::Call.args
field InputActiveExpr::Call.function
field InputActiveExpr::Input.input
field InputActiveExpr::Ternary.condition
field InputActiveExpr::Ternary.when_false
field InputActiveExpr::Ternary.when_true
field InputActiveExpr::Unary.op
field InputActiveExpr::Unary.operand
field InputActiveExpr::Value.value
field InputConstraints::max
field InputConstraints::min
field InputConstraints::options
field InputConstraints::step
field InputDecl::active
field InputDecl::confirm
field InputDecl::constraints
field InputDecl::defval
field InputDecl::display
field InputDecl::group
field InputDecl::id
field InputDecl::inline
field InputDecl::kind
field InputDecl::span
field InputDecl::title
field InputDecl::tooltip
field InputDecl::widget
field InputValue::Bool.0
field InputValue::Color.b
field InputValue::Color.g
field InputValue::Color.r
field InputValue::Color.t
field InputValue::Enum.index
field InputValue::Enum.member
field InputValue::Enum.name
field InputValue::Enum.title
field InputValue::ExternalSource.0
field InputValue::Float.0
field InputValue::Int.0
field InputValue::Source.0
field InputValue::String.0
field Label::message
field Label::span
field LogEntry::bar
field LogEntry::level
field LogEntry::message
field MarketData::ask
field MarketData::bid
field MarketData::chart_left_visible_bar_time
field MarketData::chart_right_visible_bar_time
field MarketData::close
field MarketData::footprint_data
field MarketData::high
field MarketData::low
field MarketData::mintick
field MarketData::open
field MarketData::request_bars
field MarketData::request_data
field MarketData::request_time_close
field MarketData::security_lower_tf_data
field MarketData::syminfo
field MarketData::time
field MarketData::time_close
field MarketData::timeframe
field MarketData::trading_day
field MarketData::upcoming_events
field MarketData::volume
field OhlcBodyColor::b
field OhlcBodyColor::g
field OhlcBodyColor::r
field OhlcBodyColor::t
field OhlcBodyStyle::runtime_colorer
field OhlcBodyStyle::static_color
field OhlcBodyStyle::static_color_present
field PhaseTimings::csil_ms
field PhaseTimings::dumps_ms
field PhaseTimings::libraries_ms
field PhaseTimings::mir_ms
field PhaseTimings::parse_ms
field PhaseTimings::semantics_ms
field PhaseTimings::total_ms
field PhaseTimings::validate_ms
field PlotColorSeries::palette_indices
field PlotColorSeries::role
field PlotColorSeries::values
field PlotSeries::colors
field PlotSeries::slot
field PlotSeries::style
field PlotSeries::title
field PlotSeries::values
field PlotTickSample::bar_index
field PlotTickSample::is_bar_close
field PlotTickSample::tick_ordinal
field PlotTickSample::values
field PolyPointDto::index
field PolyPointDto::price
field PolyPointDto::time
field Program::constants
field Program::fills
field Program::functions
field Program::header
field Program::inputs
field Program::metadata
field Program::object_types
field Program::plots
field Program::series_slots
field Program::ta_input_bindings
field Program::ta_states
field Program::visuals
field RealtimeSim::ticks
field RequestTimeframeBars::close
field RequestTimeframeBars::high
field RequestTimeframeBars::low
field RequestTimeframeBars::open
field RequestTimeframeBars::time
field RequestTimeframeBars::time_close
field RequestTimeframeBars::volume
field RunOptions::external_sources
field RunOptions::inputs
field RunOptions::max_bars
field RunOptions::max_collection_elements
field RunOptions::max_instructions_per_bar
field RunOptions::max_instructions_per_run
field RunOptions::max_stack_depth
field RunOptions::max_string_bytes_per_run
field RunOptions::max_vm_bytes_per_run
field RunOptions::profile
field RunOptions::realtime
field RunOptions::realtime_trace
field RunOptions::strategy
field RunResult::alerts
field RunResult::bars_executed
field RunResult::compile
field RunResult::diagnostics
field RunResult::drawings
field RunResult::fills
field RunResult::logs
field RunResult::plots
field RunResult::profile
field RunResult::realtime_trace
field RunResult::run_ms
field RunResult::runtime_errors
field RunResult::strategy
field RunResult::visuals
field RuntimeError::bar_index
field RuntimeError::instruction_offset
field RuntimeError::kind
field RuntimeError::message
field RuntimeError::span
field SourceId::0
field Span::end
field Span::source
field Span::start
field StrategyOutcome::account_currency
field StrategyOutcome::avg_trade
field StrategyOutcome::closed_trades
field StrategyOutcome::currency_conversion_complete
field StrategyOutcome::currency_rates
field StrategyOutcome::equity
field StrategyOutcome::even_trades
field StrategyOutcome::first_entry_account_notional_per_unit
field StrategyOutcome::first_entry_bar
field StrategyOutcome::first_entry_price
field StrategyOutcome::first_retained_trade_index
field StrategyOutcome::gross_loss
field StrategyOutcome::gross_profit
field StrategyOutcome::initial_capital
field StrategyOutcome::largest_liquidation
field StrategyOutcome::loss_trades
field StrategyOutcome::margin_calls
field StrategyOutcome::margin_used
field StrategyOutcome::max_contracts_held
field StrategyOutcome::max_contracts_held_long
field StrategyOutcome::max_contracts_held_short
field StrategyOutcome::max_drawdown
field StrategyOutcome::max_drawdown_percent
field StrategyOutcome::max_runup
field StrategyOutcome::maximum_margin_used
field StrategyOutcome::net_profit
field StrategyOutcome::open_entry_commission
field StrategyOutcome::open_profit
field StrategyOutcome::position
field StrategyOutcome::report_aggregates
field StrategyOutcome::required_account_size
field StrategyOutcome::retained_trade_pnl_baseline
field StrategyOutcome::symbol_currency
field StrategyOutcome::total_liquidated
field StrategyOutcome::trades
field StrategyOutcome::win_trades
field StrategyPosition::avg_price
field StrategyPosition::size
field StrategyReportAggregates::all
field StrategyReportAggregates::long
field StrategyReportAggregates::short
field StrategyReportOutliers::count
field StrategyReportOutliers::lower_fence
field StrategyReportOutliers::total_pnl
field StrategyReportOutliers::trade_indices
field StrategyReportOutliers::upper_fence
field StrategyReportScopeAggregate::adverse_excursion_count
field StrategyReportScopeAggregate::adverse_excursion_max
field StrategyReportScopeAggregate::adverse_excursion_percent_count
field StrategyReportScopeAggregate::adverse_excursion_percent_max
field StrategyReportScopeAggregate::adverse_excursion_percent_sum
field StrategyReportScopeAggregate::adverse_excursion_sum
field StrategyReportScopeAggregate::bars_held_count
field StrategyReportScopeAggregate::bars_held_sum
field StrategyReportScopeAggregate::closed_trades
field StrategyReportScopeAggregate::commission
field StrategyReportScopeAggregate::even_trades
field StrategyReportScopeAggregate::favorable_excursion_count
field StrategyReportScopeAggregate::favorable_excursion_max
field StrategyReportScopeAggregate::favorable_excursion_percent_count
field StrategyReportScopeAggregate::favorable_excursion_percent_max
field StrategyReportScopeAggregate::favorable_excursion_percent_sum
field StrategyReportScopeAggregate::favorable_excursion_sum
field StrategyReportScopeAggregate::gross_loss
field StrategyReportScopeAggregate::gross_profit
field StrategyReportScopeAggregate::largest_losing_trade
field StrategyReportScopeAggregate::largest_winning_trade
field StrategyReportScopeAggregate::losing_bars_held_count
field StrategyReportScopeAggregate::losing_bars_held_sum
field StrategyReportScopeAggregate::losing_return_percent_count
field StrategyReportScopeAggregate::losing_return_percent_sum
field StrategyReportScopeAggregate::loss_trades
field StrategyReportScopeAggregate::net_profit
field StrategyReportScopeAggregate::open_profit
field StrategyReportScopeAggregate::open_trades
field StrategyReportScopeAggregate::outliers
field StrategyReportScopeAggregate::return_percent_count
field StrategyReportScopeAggregate::return_percent_sum
field StrategyReportScopeAggregate::win_trades
field StrategyReportScopeAggregate::winning_bars_held_count
field StrategyReportScopeAggregate::winning_bars_held_sum
field StrategyReportScopeAggregate::winning_return_percent_count
field StrategyReportScopeAggregate::winning_return_percent_sum
field StrategyReportTradeHighlight::entry_id
field StrategyReportTradeHighlight::pnl
field StrategyReportTradeHighlight::return_percent
field StrategyReportTradeHighlight::sign
field StrategyReportTradeHighlight::trade_index
field StrategyRunOverrides::backtest_fill_limits_assumption
field StrategyRunOverrides::calc_bars_count
field StrategyRunOverrides::calc_on_every_history_tick
field StrategyRunOverrides::calc_on_every_tick
field StrategyRunOverrides::calc_on_order_fills
field StrategyRunOverrides::close_entries_rule
field StrategyRunOverrides::commission_type
field StrategyRunOverrides::commission_value
field StrategyRunOverrides::currency
field StrategyRunOverrides::default_qty_type
field StrategyRunOverrides::default_qty_value
field StrategyRunOverrides::fill_orders_on_standard_ohlc
field StrategyRunOverrides::initial_capital
field StrategyRunOverrides::margin_long
field StrategyRunOverrides::margin_short
field StrategyRunOverrides::process_orders_on_close
field StrategyRunOverrides::pyramiding
field StrategyRunOverrides::slippage
field StrategyRunOverrides::use_bar_magnifier
field StrategyTrade::comment
field StrategyTrade::commission
field StrategyTrade::cumulative_pnl
field StrategyTrade::entry_bar
field StrategyTrade::entry_comment
field StrategyTrade::entry_id
field StrategyTrade::entry_notional
field StrategyTrade::entry_price
field StrategyTrade::exit_bar
field StrategyTrade::exit_id
field StrategyTrade::exit_order_name
field StrategyTrade::exit_price
field StrategyTrade::index
field StrategyTrade::max_drawdown
field StrategyTrade::max_runup
field StrategyTrade::pnl
field StrategyTrade::qty
field StrategyTrade::sign
field SymInfo::basecurrency
field SymInfo::country
field SymInfo::currency
field SymInfo::current_contract
field SymInfo::description
field SymInfo::employees
field SymInfo::expiration_date
field SymInfo::industry
field SymInfo::isin
field SymInfo::kind
field SymInfo::main_tickerid
field SymInfo::mincontract
field SymInfo::minmove
field SymInfo::pointvalue
field SymInfo::prefix
field SymInfo::pricescale
field SymInfo::recommendations_buy
field SymInfo::recommendations_buy_strong
field SymInfo::recommendations_date
field SymInfo::recommendations_hold
field SymInfo::recommendations_sell
field SymInfo::recommendations_sell_strong
field SymInfo::recommendations_total
field SymInfo::root
field SymInfo::sector
field SymInfo::session
field SymInfo::session_hours
field SymInfo::shareholders
field SymInfo::shares_outstanding_float
field SymInfo::shares_outstanding_total
field SymInfo::target_price_average
field SymInfo::target_price_date
field SymInfo::target_price_estimates
field SymInfo::target_price_high
field SymInfo::target_price_low
field SymInfo::target_price_median
field SymInfo::ticker
field SymInfo::tickerid
field SymInfo::timezone
field SymInfo::volumetype
field TaInputBinding::input
field TaInputBinding::negated
field TaInputBinding::target
field TaInputTarget::Change.slot
field TaInputTarget::Sma.slot
field TaInputTarget::State.param
field TaInputTarget::State.state
field TableCellDto::attrs
field TableCellDto::column
field TableCellDto::merge
field TableCellDto::merged
field TableCellDto::row
field Tick::bar_index
field Tick::close
field Tick::high
field Tick::is_bar_close
field Tick::low
field Tick::open
field Tick::time
field Tick::trading_day
field Tick::volume
field UnsupportedVisualOption::field
field UnsupportedVisualOption::reason
field UpcomingEvents::dividends_future_amount
field UpcomingEvents::dividends_future_ex_date
field UpcomingEvents::dividends_future_pay_date
field UpcomingEvents::earnings_future_eps
field UpcomingEvents::earnings_future_period_end_time
field UpcomingEvents::earnings_future_revenue
field UpcomingEvents::earnings_future_time
field Value::Bool.0
field Value::Color.b
field Value::Color.g
field Value::Color.input_origin
field Value::Color.r
field Value::Color.t
field Value::DisplacedTable.0
field Value::Float.0
field Value::Int.0
field Value::Ref.0
field Value::String.0
field VersionInfo::compiler_version
field VersionInfo::csil_version
field VersionInfo::language_version
field VersionInfo::rendering_contract_version
field VersionInfo::source_revision
field VersionInfo::tzdata_version
field VisualSeries::colors
field VisualSeries::slot
field VisualSeries::style
field VisualSeries::title
field VmProfile::combined_dispatch_by_pc
field VmProfile::combined_dispatch_count
field VmProfile::count
field VmProfile::flow
field VmProfile::span_end
field VmProfile::span_source
field VmProfile::span_start
field VmProfile::units
method BuiltinSeries::current_read_qualifier
method BuiltinSeries::is_bool
method BuiltinSeries::is_input
method BuiltinSeries::is_input_source
method BuiltinSeries::is_int
method BuiltinSeries::is_price_source
method BuiltinSeries::is_simple
method BuiltinSeries::is_strategy
method BuiltinSeries::is_string
method BuiltinSeries::name
method BuiltinSeries::value_type
method CompileOptions::all
method Diagnostic::error
method Diagnostic::is_error
method Diagnostic::primary_span
method Diagnostic::warning
method Diagnostic::with_label
method Diagnostic::with_note
method DisplayKind::color_roles
method Drawings::is_empty
method ExternalSourceRef::is_valid
method ExternalSourceRef::same_lane
method HandleId::index
method IncrementalSession::advance
method IncrementalSession::advance_profiled
method IncrementalSession::advance_samples
method IncrementalSession::committed_bars
method IncrementalSession::is_failed
method IncrementalSession::new
method IncrementalSession::resident_bars
method IncrementalSession::result
method InputConstraints::is_empty
method InputKind::describe
method LogLevel::from_tag
method LogLevel::from_u8
method LogLevel::member
method LogLevel::text_tag
method LogLevel::to_u8
method MarketData::bars
method MarketData::time_at
method MarketData::time_close_at
method MarketData::trading_day_at
method MarketData::validate
method PlotColorRole::value_domain
method PreparedProgram::new
method PreparedProgram::run_profiled
method Program::from_json
method Program::history_depth
method Program::history_depth_with_ta_inputs
method Program::history_ring
method Program::main
method Program::resolve_ta_input
method Program::set_ta_inputs
method Program::ta_input_bounds
method Program::to_json
method Span::dummy
method Span::is_empty
method Span::len
method Span::new
method Span::to
method Value::as_ref
method Value::color_hex
method Value::from_f64
method Value::into_na_observation
method Value::is_na
method Value::na_observation
method Value::render
method Value::type_name
variant ArrowStaticColorOrigin::Constant
variant ArrowStaticColorOrigin::Dynamic
variant ArrowStaticColorOrigin::Missing
variant ArrowStaticColorOrigin::Omitted
variant BuiltinSeries::Ask
variant BuiltinSeries::BarIndex
variant BuiltinSeries::BarstateIsconfirmed
variant BuiltinSeries::BarstateIsfirst
variant BuiltinSeries::BarstateIshistory
variant BuiltinSeries::BarstateIslast
variant BuiltinSeries::BarstateIslastconfirmedhistory
variant BuiltinSeries::BarstateIsnew
variant BuiltinSeries::BarstateIsrealtime
variant BuiltinSeries::Bid
variant BuiltinSeries::ChartLeftVisibleBarTime
variant BuiltinSeries::ChartRightVisibleBarTime
variant BuiltinSeries::Close
variant BuiltinSeries::DayOfMonth
variant BuiltinSeries::DayOfWeek
variant BuiltinSeries::DividendsFutureAmount
variant BuiltinSeries::DividendsFutureExDate
variant BuiltinSeries::DividendsFuturePayDate
variant BuiltinSeries::EarningsFutureEps
variant BuiltinSeries::EarningsFuturePeriodEndTime
variant BuiltinSeries::EarningsFutureRevenue
variant BuiltinSeries::EarningsFutureTime
variant BuiltinSeries::High
variant BuiltinSeries::Hl2
variant BuiltinSeries::Hlc3
variant BuiltinSeries::Hlcc4
variant BuiltinSeries::Hour
variant BuiltinSeries::LastBarIndex
variant BuiltinSeries::LastBarTime
variant BuiltinSeries::Low
variant BuiltinSeries::Minute
variant BuiltinSeries::Month
variant BuiltinSeries::Ohlc4
variant BuiltinSeries::Open
variant BuiltinSeries::Second
variant BuiltinSeries::SessionIsfirstbar
variant BuiltinSeries::SessionIsfirstbarRegular
variant BuiltinSeries::SessionIslastbar
variant BuiltinSeries::SessionIslastbarRegular
variant BuiltinSeries::SessionIsmarket
variant BuiltinSeries::SessionIspostmarket
variant BuiltinSeries::SessionIspremarket
variant BuiltinSeries::StrategyAccountCurrency
variant BuiltinSeries::StrategyAvgLosingTrade
variant BuiltinSeries::StrategyAvgLosingTradePercent
variant BuiltinSeries::StrategyAvgTrade
variant BuiltinSeries::StrategyAvgTradePercent
variant BuiltinSeries::StrategyAvgWinningTrade
variant BuiltinSeries::StrategyAvgWinningTradePercent
variant BuiltinSeries::StrategyClosedTrades
variant BuiltinSeries::StrategyClosedTradesFirstIndex
variant BuiltinSeries::StrategyEquity
variant BuiltinSeries::StrategyEvenTrades
variant BuiltinSeries::StrategyGrossLoss
variant BuiltinSeries::StrategyGrossLossPercent
variant BuiltinSeries::StrategyGrossProfit
variant BuiltinSeries::StrategyGrossProfitPercent
variant BuiltinSeries::StrategyInitialCapital
variant BuiltinSeries::StrategyLossTrades
variant BuiltinSeries::StrategyMarginLiquidationPrice
variant BuiltinSeries::StrategyMaxContractsHeldAll
variant BuiltinSeries::StrategyMaxContractsHeldLong
variant BuiltinSeries::StrategyMaxContractsHeldShort
variant BuiltinSeries::StrategyMaxDrawdown
variant BuiltinSeries::StrategyMaxDrawdownPercent
variant BuiltinSeries::StrategyMaxRunup
variant BuiltinSeries::StrategyMaxRunupPercent
variant BuiltinSeries::StrategyNetProfit
variant BuiltinSeries::StrategyNetProfitPercent
variant BuiltinSeries::StrategyOpenProfit
variant BuiltinSeries::StrategyOpenProfitPercent
variant BuiltinSeries::StrategyOpenTrades
variant BuiltinSeries::StrategyOpenTradesCapitalHeld
variant BuiltinSeries::StrategyPositionAvgPrice
variant BuiltinSeries::StrategyPositionEntryName
variant BuiltinSeries::StrategyPositionSize
variant BuiltinSeries::StrategyWinTrades
variant BuiltinSeries::SyminfoBasecurrency
variant BuiltinSeries::SyminfoCountry
variant BuiltinSeries::SyminfoCurrency
variant BuiltinSeries::SyminfoCurrentContract
variant BuiltinSeries::SyminfoDescription
variant BuiltinSeries::SyminfoEmployees
variant BuiltinSeries::SyminfoExpirationDate
variant BuiltinSeries::SyminfoIndustry
variant BuiltinSeries::SyminfoIsin
variant BuiltinSeries::SyminfoMainTickerid
variant BuiltinSeries::SyminfoMincontract
variant BuiltinSeries::SyminfoMinmove
variant BuiltinSeries::SyminfoMintick
variant BuiltinSeries::SyminfoPointvalue
variant BuiltinSeries::SyminfoPrefix
variant BuiltinSeries::SyminfoPricescale
variant BuiltinSeries::SyminfoRecommendationsBuy
variant BuiltinSeries::SyminfoRecommendationsBuyStrong
variant BuiltinSeries::SyminfoRecommendationsDate
variant BuiltinSeries::SyminfoRecommendationsHold
variant BuiltinSeries::SyminfoRecommendationsSell
variant BuiltinSeries::SyminfoRecommendationsSellStrong
variant BuiltinSeries::SyminfoRecommendationsTotal
variant BuiltinSeries::SyminfoRoot
variant BuiltinSeries::SyminfoSector
variant BuiltinSeries::SyminfoSession
variant BuiltinSeries::SyminfoShareholders
variant BuiltinSeries::SyminfoSharesOutstandingFloat
variant BuiltinSeries::SyminfoSharesOutstandingTotal
variant BuiltinSeries::SyminfoTargetPriceAverage
variant BuiltinSeries::SyminfoTargetPriceDate
variant BuiltinSeries::SyminfoTargetPriceEstimates
variant BuiltinSeries::SyminfoTargetPriceHigh
variant BuiltinSeries::SyminfoTargetPriceLow
variant BuiltinSeries::SyminfoTargetPriceMedian
variant BuiltinSeries::SyminfoTicker
variant BuiltinSeries::SyminfoTickerid
variant BuiltinSeries::SyminfoTimezone
variant BuiltinSeries::SyminfoType
variant BuiltinSeries::SyminfoVolumetype
variant BuiltinSeries::Time
variant BuiltinSeries::TimeClose
variant BuiltinSeries::TimeTradingday
variant BuiltinSeries::TimeframeIsdaily
variant BuiltinSeries::TimeframeIsdwm
variant BuiltinSeries::TimeframeIsintraday
variant BuiltinSeries::TimeframeIsminutes
variant BuiltinSeries::TimeframeIsmonthly
variant BuiltinSeries::TimeframeIsseconds
variant BuiltinSeries::TimeframeIsticks
variant BuiltinSeries::TimeframeIsweekly
variant BuiltinSeries::TimeframeMainPeriod
variant BuiltinSeries::TimeframeMultiplier
variant BuiltinSeries::TimeframePeriod
variant BuiltinSeries::Timenow
variant BuiltinSeries::Volume
variant BuiltinSeries::WeekOfYear
variant BuiltinSeries::Year
variant DisplayKind::Barcolor
variant DisplayKind::Bgcolor
variant DisplayKind::Hline
variant DisplayKind::Plot
variant DisplayKind::PlotArrow
variant DisplayKind::PlotBar
variant DisplayKind::PlotCandle
variant DisplayKind::PlotChar
variant DisplayKind::PlotShape
variant InputActiveBinaryOp::Add
variant InputActiveBinaryOp::And
variant InputActiveBinaryOp::Div
variant InputActiveBinaryOp::Eq
variant InputActiveBinaryOp::Ge
variant InputActiveBinaryOp::Gt
variant InputActiveBinaryOp::Le
variant InputActiveBinaryOp::Lt
variant InputActiveBinaryOp::Mul
variant InputActiveBinaryOp::Ne
variant InputActiveBinaryOp::Or
variant InputActiveBinaryOp::Rem
variant InputActiveBinaryOp::Sub
variant InputActiveExpr::Binary
variant InputActiveExpr::Call
variant InputActiveExpr::Input
variant InputActiveExpr::Na
variant InputActiveExpr::Ternary
variant InputActiveExpr::Unary
variant InputActiveExpr::Value
variant InputActiveUnaryOp::IntToFloat
variant InputActiveUnaryOp::Neg
variant InputActiveUnaryOp::Not
variant InputDisplay::All
variant InputDisplay::DataWindow
variant InputDisplay::None
variant InputDisplay::StatusLine
variant InputKind::Bool
variant InputKind::Color
variant InputKind::Enum
variant InputKind::Float
variant InputKind::Int
variant InputKind::Price
variant InputKind::Source
variant InputKind::String
variant InputKind::Time
variant InputValue::Bool
variant InputValue::Color
variant InputValue::Enum
variant InputValue::ExternalSource
variant InputValue::Float
variant InputValue::Int
variant InputValue::Source
variant InputValue::String
variant InputWidget::Session
variant InputWidget::Standard
variant InputWidget::String
variant InputWidget::Symbol
variant InputWidget::TextArea
variant InputWidget::Timeframe
variant LogLevel::Error
variant LogLevel::Info
variant LogLevel::Warning
variant MarkerLocation::AboveBar
variant MarkerLocation::Absolute
variant MarkerLocation::BelowBar
variant MarkerLocation::Bottom
variant MarkerLocation::InBar
variant MarkerLocation::Top
variant MarkerShape::ArrowDown
variant MarkerShape::ArrowUp
variant MarkerShape::Circle
variant MarkerShape::Cross
variant MarkerShape::Diamond
variant MarkerShape::Flag
variant MarkerShape::LabelDown
variant MarkerShape::LabelUp
variant MarkerShape::Square
variant MarkerShape::TriangleDown
variant MarkerShape::TriangleUp
variant MarkerShape::XCross
variant MarkerSize::Auto
variant MarkerSize::Huge
variant MarkerSize::Large
variant MarkerSize::Normal
variant MarkerSize::Small
variant MarkerSize::Tiny
variant OhlcField::Close
variant OhlcField::High
variant OhlcField::Low
variant OhlcField::Open
variant PlotColorRole::Border
variant PlotColorRole::BottomColor
variant PlotColorRole::BottomValue
variant PlotColorRole::Color
variant PlotColorRole::Down
variant PlotColorRole::Text
variant PlotColorRole::TopColor
variant PlotColorRole::TopValue
variant PlotColorRole::Up
variant PlotColorRole::Wick
variant PlotDisplay::All
variant PlotDisplay::DataWindow
variant PlotDisplay::DataWindowPineScreener
variant PlotDisplay::None
variant PlotDisplay::Pane
variant PlotDisplay::PaneDataWindow
variant PlotDisplay::PaneDataWindowPineScreener
variant PlotDisplay::PanePineScreener
variant PlotDisplay::PanePriceScale
variant PlotDisplay::PanePriceScaleDataWindow
variant PlotDisplay::PanePriceScaleDataWindowPineScreener
variant PlotDisplay::PanePriceScalePineScreener
variant PlotDisplay::PanePriceScaleStatusLine
variant PlotDisplay::PanePriceScaleStatusLineDataWindow
variant PlotDisplay::PanePriceScaleStatusLinePineScreener
variant PlotDisplay::PaneStatusLine
variant PlotDisplay::PaneStatusLineDataWindow
variant PlotDisplay::PaneStatusLineDataWindowPineScreener
variant PlotDisplay::PaneStatusLinePineScreener
variant PlotDisplay::PineScreener
variant PlotDisplay::PriceScale
variant PlotDisplay::PriceScaleDataWindow
variant PlotDisplay::PriceScaleDataWindowPineScreener
variant PlotDisplay::PriceScalePineScreener
variant PlotDisplay::PriceScaleStatusLine
variant PlotDisplay::PriceScaleStatusLineDataWindow
variant PlotDisplay::PriceScaleStatusLineDataWindowPineScreener
variant PlotDisplay::PriceScaleStatusLinePineScreener
variant PlotDisplay::StatusLine
variant PlotDisplay::StatusLineDataWindow
variant PlotDisplay::StatusLineDataWindowPineScreener
variant PlotDisplay::StatusLinePineScreener
variant PlotLineStyle::Dashed
variant PlotLineStyle::Dotted
variant PlotLineStyle::Solid
variant PlotStyleKind::Area
variant PlotStyleKind::AreaBreak
variant PlotStyleKind::Circles
variant PlotStyleKind::Columns
variant PlotStyleKind::Cross
variant PlotStyleKind::Histogram
variant PlotStyleKind::Line
variant PlotStyleKind::LineBreak
variant PlotStyleKind::StepLine
variant PlotStyleKind::StepLineBreak
variant PlotStyleKind::StepLineDiamond
variant RuntimeErrorKind::CollectionIndexOutOfBounds
variant RuntimeErrorKind::CollectionLimitExceeded
variant RuntimeErrorKind::HistoryBufferLimitExceeded
variant RuntimeErrorKind::InstructionLimitExceeded
variant RuntimeErrorKind::InvalidProgram
variant RuntimeErrorKind::InvalidTaLength
variant RuntimeErrorKind::MalformedMarketData
variant RuntimeErrorKind::MatrixDimensionMismatch
variant RuntimeErrorKind::MatrixSingular
variant RuntimeErrorKind::NaFieldAccess
variant RuntimeErrorKind::NaMethodReceiver
variant RuntimeErrorKind::RunInstructionLimitExceeded
variant RuntimeErrorKind::SecurityTimeframeBelowChart
variant RuntimeErrorKind::StackOverflow
variant RuntimeErrorKind::StackUnderflow
variant RuntimeErrorKind::StringIndexOutOfBounds
variant RuntimeErrorKind::StringLimitExceeded
variant RuntimeErrorKind::TypeMismatch
variant RuntimeErrorKind::Unsupported
variant RuntimeErrorKind::UserError
variant RuntimeErrorKind::VmMemoryLimitExceeded
variant Severity::Error
variant Severity::Warning
variant TaInputTarget::Change
variant TaInputTarget::Sma
variant TaInputTarget::State
variant Value::Bool
variant Value::Color
variant Value::DisplacedTable
variant Value::Float
variant Value::Int
variant Value::Na
variant Value::Ref
variant Value::String
```
<!-- rust-api-supported-members-end -->

</details>

### Member-surface policy

These data types are re-exported so a normal VM embedder needs only a direct
dependency on `pinewasm_compiler`. Re-export support applies through the facade
path; it does not turn every item in the type's implementation crate into a
supported API. Every public inherent member of a nominal facade type (a struct,
enum, or union) is part of that supported pre-1.0 surface unless the explicit
`Program` representation exception below applies; hiding a callable item from
Rustdoc is not treated as an API boundary. A direct type alias is instead pinned
to its complete generic declaration and target type. Members of that underlying
type are not separately presented as first-party facade members.

`Program` is the one representation-public exception. Its `inputs` and
`ta_input_bindings` fields are supported for read-only host inspection through
the facade's `InputDecl` and `TaInputBinding` types. The other currently public
CSIL representation fields and `main` accessor remain in the exact member
manifest so any visibility change requires review, but they are provisional
visibility debt, not a supported construction, mutation, or nested-type import
surface. The nested types needed to construct or edit those tables are
deliberately not all re-exported through the compiler facade. Serialization,
history-planning, and TA-input methods remain supported through the facade.
Obtain a validated program from `compile`, persist its Serde form, and pass it
to execution or artifact boundaries. A future opaque facade newtype can remove
this pre-1.0 debt without promoting the CSIL object graph.

Rust's `missing_docs` lint is crate-local, so the facade lint covers items
defined here but cannot certify members defined in another crate. The
documentation gate therefore compares every direct crate-root item and
re-export against the exact manifest above, parses each directly named defining
module and recursively scans the defining crate's module graph. It rejects
nested inherent implementations, nested module or macro items, statement
macros in every item body, unreviewed item attributes or derive macros, and
inherent implementations written through any crate-local type alias or renamed
import until expansion-aware name resolution exists. The alias check is
deliberately crate-wide and syntax-only: an unrelated nominal type with the
same final name as an alias elsewhere in the crate is rejected too, requiring a
maintainer to remove the ambiguity rather than trusting an incomplete resolver.
Ordinary trait-qualified methods are not counted as inherent facade members.
Visible, explicitly written trait implementations remain accepted only when
the facade's rendered Rustdoc under `cfg(doc)` records that callable surface.
Any trait implementation carrying `doc(hidden)` or Rustdoc-sensitive
`cfg`/`cfg_attr` is rejected: hiding the impl cannot remove its methods or
operators from downstream Rust lookup. The configuration check recursively
recognizes raw or ordinary `doc`/`doctest` predicates while preserving ordinary
feature and target configuration that the isolated source and Rustdoc audit
build identically. The same predicates are rejected on every enclosing module
boundary, whether written on an inline `mod` item or as a child file's inner
attribute: Rust configuration is inherited, so an implementation inside
`#[cfg(not(doc))] mod hidden` is compiled into every normal build while
Rustdoc omits it exactly as an impl-level predicate would. An explicit
`Deref`/`DerefMut`
implementation can additionally change inherent member lookup and expose a
doc-hidden target's fields or methods through a supported type. The gate
therefore rejects autoderef-shaped trait implementations throughout a defining
crate until their member surface can be inventoried explicitly. It recognizes
both the trait path and the fixed `deref`/`deref_mut` member names so renaming
`Deref` or spelling those identifiers in raw form cannot evade the check, and
rejects impl-item macros rather than assuming they cannot generate an aliased
autoderef body. Visible, explicitly written `Default` and similar trait
implementations remain accepted because they do not widen the target's
inherent member surface through automatic dereference lookup.

### Macro and attribute provenance

Likewise, macros in statement, associated-item, type, pattern, and other
non-expression positions, plus unreviewed expression macros, are rejected: even
an expression or associated-item macro can emit a body containing a non-local
`impl`.

The syntax gate accepts an exact allowlist of unshadowed standard/compiler
expression macros, and rejects opaque arguments containing unreviewed or
ambiguous nested macros, attribute syntax, `impl`, or block-local macro-binding
constructs such as `use` and `macro_rules`.

Compiler-provided derives are accepted only when no explicit import can shadow
their bare names and their module has no unresolved external glob. A local glob
also fails closed when the crate has an external glob elsewhere, because the
local module could relay that binding.
Only an unconditional item in Rust's type namespace is treated as a local path
root when classifying a glob. A value- or macro-namespace item with the same
name does not shadow an extern-prelude crate for import resolution. An item root
behind `cfg`, or behind a `cfg_attr` that can apply `cfg`, is not unconditional:
another active binding can occupy that name when the local item is configured
out. Import and `extern crate` aliases remain conservatively unresolved because
this bounded gate does not claim Rust-grade name resolution.
Serde and thiserror derives additionally require a direct, normal, unrenamed
crates.io dependency whose identity and source also match in Cargo's locked
all-features resolved graph (including an activated optional provider), plus an
unambiguous provider path or exact provider import. Trust follows the resolved
provider edge to `serde_derive` or `thiserror-impl` and requires that
implementation package itself to be the locked crates.io proc-macro package;
checking only the `serde` or `thiserror` facade package is insufficient. A path
patch, local module, re-export, alias, or glob that could replace either layer
fails closed, including an `extern crate` alias used through an absolute path.
Serde's `serde` helper attributes and thiserror's `error` helper attributes are
accepted only within the same item tree as the corresponding
provenance-checked derive and only when the helper name is not imported or
glob-shadowable. Explicit imports inside item bodies participate in the same
crate-wide taint set; a body-local glob fails closed because its enclosing
lexical environment is outside this bounded syntax/provenance check. This is
deliberately not a partial Rust resolver.

### Cargo provenance and trusted-builder boundary

Cargo's metadata source identifier is logical rather than a content-authenticated
record of source-replacement bytes. Before invoking Cargo for this census, the
gate therefore inspects both Cargo configuration filenames at the repository
and every parent plus the effective Cargo home. Dependency/source overrides,
configuration includes, crates.io index overrides, injected source/Cargo-home
environment, and symlinked or non-file configuration fail closed; benign
aliases, network settings, credentials, and the official crates.io transport
protocol may remain present in ambient configuration. The isolated audit does
not inherit that configuration: each complete documentation audit allocates an
empty Cargo home and target directory and runs every child Cargo command from
the filesystem root, using absolute manifest paths. Cargo configuration
discovery therefore cannot reach the repository or its ancestors, and the
audit requires ordinary fresh registry access through the process's network
environment.

Registry providers therefore come through Cargo's locked fresh-download path,
and neither mutable extracted sources nor compiled proc-macro artifacts from an
earlier build can satisfy the provenance check. Inherited compiler wrappers,
profile/target overrides, Rust flags, and bootstrap selection are removed from
these child commands, including Cargo's encoded Rust and Rustdoc flag channels.
Build, target, profile, host, or environment controls in the configuration
Cargo can actually discover at the filesystem root or fresh home also fail
closed. Moving Cargo's configuration discovery outside the repository also
makes a repository or ancestor config created after preflight irrelevant to
the child launch. This is an explicit trusted-builder/configuration
precondition, not a claim that Cargo directory-source checksum metadata
authenticates hostile bytes or that the gate sandboxes a concurrently malicious
process running under the builder's own OS account. The audited source tree is
likewise a reviewed-input precondition: a hostile checked-in build script could
edit the fresh Cargo home just as it could edit this gate, so the audit does not
claim to defend against the repository it validates (the workspace currently
has no build scripts).
Manifest `[patch]` and `[replace]` entries remain subject to the exact
resolved-package checks above.

### Exact manifest enforcement

The gate also rejects intermediate re-exports, re-exported type aliases, public
`doc(hidden)` members or trait implementations, module or implementation macros,
path-overridden modules, and untracked public child modules. It then compares
every complete rendered item docblock (including constants) and reconciles
source members with rendered Rustdoc. The root manifest pins each re-export to
its defining crate and module path and each direct alias to its exact generic
declaration and target token stream; the member manifest pins all public fields,
variants, inherent methods, and associated constants by identity. Adding,
removing, renaming, relocating, or retargeting a supported item therefore
requires an explicit API-manifest review even when the total item count is
unchanged. The source reconciliation counts tuple-variant payloads independently
so a Rustdoc format change cannot silently remove them from the debt register.

<!-- rust-api-member-doc-debt: undocumented-variants=99 undocumented-fields=0 undocumented-methods=0 undocumented-associated-constants=0 -->

<details>
<summary>Exact residual variant-documentation debt</summary>

<!-- rust-api-undocumented-variants-begin -->
```text
BuiltinSeries::Close
BuiltinSeries::High
BuiltinSeries::Low
BuiltinSeries::Open
BuiltinSeries::Volume
DisplayKind::Barcolor
DisplayKind::Bgcolor
DisplayKind::Hline
DisplayKind::Plot
DisplayKind::PlotArrow
DisplayKind::PlotBar
DisplayKind::PlotCandle
DisplayKind::PlotChar
DisplayKind::PlotShape
InputActiveBinaryOp::Add
InputActiveBinaryOp::And
InputActiveBinaryOp::Div
InputActiveBinaryOp::Eq
InputActiveBinaryOp::Ge
InputActiveBinaryOp::Gt
InputActiveBinaryOp::Le
InputActiveBinaryOp::Lt
InputActiveBinaryOp::Mul
InputActiveBinaryOp::Ne
InputActiveBinaryOp::Or
InputActiveBinaryOp::Rem
InputActiveBinaryOp::Sub
InputActiveUnaryOp::IntToFloat
InputActiveUnaryOp::Neg
InputActiveUnaryOp::Not
InputDisplay::All
InputDisplay::DataWindow
InputDisplay::None
InputDisplay::StatusLine
InputKind::Bool
InputKind::Color
InputKind::Float
InputKind::Int
InputKind::String
InputValue::Bool
InputValue::Float
InputValue::Int
InputValue::String
LogLevel::Error
LogLevel::Info
LogLevel::Warning
MarkerLocation::AboveBar
MarkerLocation::Absolute
MarkerLocation::BelowBar
MarkerLocation::Bottom
MarkerLocation::Top
MarkerShape::ArrowDown
MarkerShape::ArrowUp
MarkerShape::Circle
MarkerShape::Cross
MarkerShape::Diamond
MarkerShape::Flag
MarkerShape::LabelDown
MarkerShape::LabelUp
MarkerShape::Square
MarkerShape::TriangleDown
MarkerShape::TriangleUp
MarkerShape::XCross
MarkerSize::Auto
MarkerSize::Huge
MarkerSize::Large
MarkerSize::Normal
MarkerSize::Small
MarkerSize::Tiny
OhlcField::Close
OhlcField::High
OhlcField::Low
OhlcField::Open
PlotColorRole::Border
PlotColorRole::Color
PlotColorRole::Down
PlotColorRole::Text
PlotColorRole::Up
PlotColorRole::Wick
PlotDisplay::All
PlotDisplay::DataWindow
PlotDisplay::None
PlotDisplay::Pane
PlotDisplay::PriceScale
PlotDisplay::StatusLine
PlotLineStyle::Dashed
PlotLineStyle::Dotted
PlotLineStyle::Solid
PlotStyleKind::Area
PlotStyleKind::AreaBreak
PlotStyleKind::Circles
PlotStyleKind::Columns
PlotStyleKind::Cross
PlotStyleKind::Histogram
PlotStyleKind::Line
PlotStyleKind::LineBreak
PlotStyleKind::StepLine
PlotStyleKind::StepLineBreak
PlotStyleKind::StepLineDiamond
```
<!-- rust-api-undocumented-variants-end -->

</details>

That census is a bounded debt register, not a claim that an undocumented member
is supported by implication. Every public struct and enum-payload field,
inherent method, and associated constant in the current supported graph is
documented. The 99 entries listed above are the exact known variant-level
documentation debt, including concise wire-domain variants such as
`PlotStyleKind::Line` and `MarkerSize::Small`. Their enclosing enum contracts
provide the current domain context, but these variants do not yet have
individual Rustdoc blocks. A count-preserving identity swap still fails `cargo
xtask docs-check`; changing either the count or list requires explicit review.

Treat `Program` as an opaque cache value even though its current pre-1.0 Rust
representation has public fields. Construct it through `compile`, persist it
through its Serde representation, and pass it back through `run_program` or an
artifact compiler. Editing its tables or instructions is unsupported. The
member manifest records those fields and the `main` representation accessor as
provisional visibility debt; it does not make their nested CSIL types a facade
re-export closure. Serialization, history-planning, and TA-input methods remain
supported through the facade. Facade consumers validate an externally supplied
cache value at admission. `run_program` revalidates it on each call; prepared
programs and incremental sessions validate once on construction, then retain
immutable code for repeated execution. Low-level VM entry points require
previously validated CSIL.

AST, HIR, MIR, and CSIL debug strings are human-readable inspection aids. Their
format is not a parser or compatibility contract.

## Compatibility posture

All repository crates keep the fixed internal crate version `0.1.0` and
`publish = false`. The product version hosts see is the compiler release
version in `crates/pinewasm_compiler/VERSION`
(`pinewasm_compiler::COMPILER_VERSION`, `version().compilerVersion`), which
follows Semantic Versioning and moves with every compiler change; see
[versioning.md](versioning.md).
The supported facade identifies the intended dependency direction; it does not
claim a 1.0 API freeze or crates.io availability. Before 1.0, an intentional
source-breaking change to the facade must be called out in release notes and
reviewed as an API change rather than hidden in an implementation refactor.

This inventory defines the supported Rust surface. Treat `StrategyTrade` as
runtime output rather than constructing it by struct literal. Source-breaking
changes are called out in release notes.

Serialized programs carry explicit CSIL, language, and runtime-ABI versions.
The current runtime accepts the format version it implements and rejects a
version mismatch as `RuntimeErrorKind::InvalidProgram`. Versioning prevents a
cache from being misinterpreted; it is not a promise that one release will read
every earlier or future program version. Persist source alongside long-lived
program caches so a host can recompile after an upgrade.

The CSAW module ABI is a separate compatibility surface. Consumers of emitted
Wasm must validate the artifact ABI version described in the normative
specification; Rust crate versions and CSAW versions are not interchangeable.

## Failure and resource contract

- Compile failures are `Diagnostic` values in `CompileResult` or `RunResult`.
  They are not Rust `Err` values because a call can return several diagnostics
  and completed debug views.
- Cached-program validation, malformed market data, script failures, and
  execution limits are `RuntimeError` values. One-shot calls can retain output
  for bars completed before a runtime abort.
- The documented compilation and execution entry points have no documented
  panic condition for caller-controlled source, program, data, options, or
  ticks. Constructors and inspection helpers on re-exported data types are not
  therefore panic-free; for example, `Program::main` requires a validated
  program with a main function. A supplied timing callback is caller code, so
  its panic propagates. Allocation exhaustion and other process-level failures
  are not converted into diagnostics.
- Source/library byte caps bound the immediate compile payload. They do not form
  a hard compiler CPU or resident-memory sandbox.
- `RunOptions` instruction, stack, string, VM-state, collection, and bar limits
  are deterministic limits enforced by the VM. They are not a direct ceiling
  on allocator overhead or total process resident memory.

`RunOptions::default()` uses the exported `DEFAULT_MAX_*` constants for its
instruction, stack, string, VM-state, and collection limits; `max_bars` is
`None`. The facade contract test pins those relationships so documentation and
runtime defaults cannot drift independently.

Use an operating-system process boundary in addition to the API limits when an
application requires hard isolation from untrusted-input CPU or memory use.

## Determinism and concurrency

For a fixed PineWASM build and target, semantic results are deterministic for
identical source, library order and contents, market data, and run options.
Native convenience entry points measure elapsed time, so `CompileResult::timings` and
`RunResult::run_ms` intentionally vary. The profiled entry points accept
`clock = None` to omit those fields for exact result comparison.

Compiler inputs and immutable artifacts are `Send + Sync`, and independent
calls may execute concurrently. `IncrementalSession` is owned mutable state: it
can move to a worker thread, but one session advances through exclusive
`&mut self` access. No facade call creates a background worker.

## Review and CI gate

The supported facade opts into `missing_docs` and broken intra-doc-link denial
at its crate root. Hosted CI runs the complete source/rendered-documentation
reconciliation rather than only checking that Rustdoc exits successfully.
Documentation examples cover source compilation and VM
execution, cached-program validation, JavaScript/Wasm adapter wiring, and
standalone AOT artifact emission plus byte-level audit. The binding example is
compile-only because `JsValue` requires a Wasm host at runtime; it does not
promote the binding crate's Rust functions into the supported surface. The AOT
example exercises the real emitter while retaining the direct Rust emitter's
provisional classification and the emitted CSAW module's separate contract.

The repository quality gate runs every example on both the pinned toolchain and
minimum supported Rust version:

```sh
cargo xtask docs-check
cargo test --locked --doc \
  -p pinewasm_compiler -p pinewasm_bindings -p pinewasm_aot
```

The minimum-version CI job also runs the complete `pinewasm_xtask` unit and
artifact-integration suite with `PINEWASM_FIXTURE_RUST_TOOLCHAIN` set to that
same version. The rustc and rustdoc checks therefore exercise the
hostile callable-hidden API fixtures on Rust 1.86 itself rather than inheriting
the repository's newer pinned toolchain, while the artifact tests cover their
descriptor-anchored cache and AOT boundary at the same floor.

When another crate or module becomes supported, update this inventory, enable
the same lints at that exact boundary, add compiling examples and contract
documentation, and review its public dependency graph before changing the
classification.

Primary OHLCV JSON samples accept numbers, `null` as NaN, and the strings
`"Infinity"` and `"-Infinity"`. This matches `chart_request_json` and the linked
chart runtime; auxiliary request lanes keep their existing decoding rules.
