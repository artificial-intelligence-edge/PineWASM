//! Rust-owned TypeScript declarations for the serialized WebAssembly API.
//!
//! The exported functions still accept and return `JsValue`; these declarations
//! describe the values that the Serde adapter validates at that boundary. Keep
//! this module next to the adapter so the generated package has one source of
//! truth for its ergonomic JavaScript contract.

use wasm_bindgen::prelude::wasm_bindgen;

#[wasm_bindgen(typescript_custom_section)]
const TYPESCRIPT_CONTRACT: &str = r#"
export {};

/** A JavaScript input accepted for a Rust `i64`, `u64`, or `usize`. */
export type IntegerInput = number | bigint;

/** A scalar Pine value returned by the VM. Pine `na` is `null`. */
export type PineValue = null | boolean | number | bigint | string;

/** One in-memory Pine library: `[importPath, source]`. */
export type LibraryPair = readonly [importPath: string, source: string];
export type Libraries = ReadonlyArray<LibraryPair>;

/** String-keyed host tables accept either a plain object or a native JavaScript Map. */
export type StringTable<T> = { readonly [key: string]: T } | Map<string, T>;

interface CompileOptionFlags {
  readonly chartBackground?: readonly [number, number, number];
  readonly chartForeground?: readonly [number, number, number];
  readonly emitAst?: boolean;
  readonly emitHir?: boolean;
  readonly emitMir?: boolean;
  readonly disableDistanceScanOptimization?: boolean;
}

/**
 * Request at most one spelling of the CSIL debug output. `emitIl` is the
 * established wire name; `emitCsil` is its accepted descriptive alias.
 */
export type CompileOptions = CompileOptionFlags & (
  | { readonly emitIl?: boolean; readonly emitCsil?: never }
  | { readonly emitIl?: never; readonly emitCsil?: boolean }
);

export interface SourceSpan {
  readonly source: number;
  readonly start: number;
  readonly end: number;
}

export type DiagnosticSeverity = "error" | "warning";

export interface DiagnosticLabel {
  readonly span: SourceSpan;
  readonly message: string;
}

export interface Diagnostic {
  readonly severity: DiagnosticSeverity;
  readonly code: string | null;
  readonly message: string;
  readonly labels: ReadonlyArray<DiagnosticLabel>;
  readonly notes: ReadonlyArray<string>;
}

export interface PhaseTimings {
  readonly parseMs: number;
  readonly librariesMs: number;
  readonly semanticsMs: number;
  readonly mirMs: number;
  readonly ilMs: number;
  readonly validateMs: number;
  readonly dumpsMs: number;
  readonly totalMs: number;
}

declare const pineWasmProgram: unique symbol;

/**
 * A validated CSIL program produced by this package.
 *
 * Its input manifest is inspectable. The branded type stops callers from
 * constructing the rest of the versioned artifact in TypeScript.
 */
export interface Program {
  readonly inputs: ReadonlyArray<InputDeclaration>;
  readonly [pineWasmProgram]: never;
}

export interface CompileResult {
  readonly success: boolean;
  readonly diagnostics: ReadonlyArray<Diagnostic>;
  readonly astDebug: string | null;
  readonly hirDebug: string | null;
  readonly mirDebug: string | null;
  readonly ilText: string | null;
  readonly program: Program | null;
  readonly timings?: PhaseTimings;
}

/** Same compilation result, with immutable CSIL retained in this Wasm instance.
 * Free the handle on eviction. No source-edit or execution-state cache is used.
 */
export interface ResidentCompileResult extends Omit<CompileResult, "program"> {
  readonly program: CompiledProgram | null;
}

export type InputKind =
  | "Int"
  | "Float"
  | "Bool"
  | "String"
  | "Color"
  | "Source"
  | "Price"
  | "Time"
  | "Enum";

export type InputWidget =
  | "standard"
  | "string"
  | "text_area"
  | "timeframe"
  | "session"
  | "symbol";

export type InputDisplay = "all" | "none" | "data_window" | "status_line";

/** The eight builtin series Pine permits as `input.source` selections. */
export type InputSource =
  | "Open"
  | "High"
  | "Low"
  | "Close"
  | "Hl2"
  | "Hlc3"
  | "Ohlc4"
  | "Hlcc4";

/** A plot of another compiled script, identified by the host's id for that script, its plot slot and its plot label. */
export interface ExternalSourceRef {
  readonly scriptId: string;
  readonly plotSlot: number;
  readonly plotLabel: string;
}

/**
 * Input values use Serde's externally tagged representation, so exactly one
 * variant tag must be present. In particular, a raw number is not an integer
 * override; use `{ Int: value }`.
 * `InputValue` is the wider form a JavaScript host may supply when overriding
 * an input: an enum override may omit `member` and `title`. Compiler-authored
 * input metadata uses `DeclaredInputValue`, which always carries an enum's
 * `member` and nullable `title` and always emits `Int` as `bigint`.
 */
type InputValueTag =
  | "Int"
  | "Float"
  | "Bool"
  | "String"
  | "Color"
  | "Source"
  | "ExternalSource"
  | "Enum";

type TaggedInputValue<Tag extends InputValueTag, Value> =
  { readonly [Field in Tag]: Value } &
  { readonly [Other in Exclude<InputValueTag, Tag>]?: never };

type InputValueOf<IntValue, EnumValue> =
  | TaggedInputValue<"Int", IntValue>
  | TaggedInputValue<"Float", number>
  | TaggedInputValue<"Bool", boolean>
  | TaggedInputValue<"String", string>
  | TaggedInputValue<"Color", { readonly r: number; readonly g: number; readonly b: number; readonly t: number }>
  | TaggedInputValue<"Source", InputSource>
  | TaggedInputValue<"ExternalSource", ExternalSourceRef>
  | TaggedInputValue<"Enum", EnumValue>;

/** An input value a host may supply as an override. */
export type InputValue = InputValueOf<number | bigint | string, { readonly name: string; readonly index: number; readonly member?: string; readonly title?: string | null }>;

/** An input value in compiler-authored metadata. It is also a valid `InputValue`. */
export type DeclaredInputValue = InputValueOf<bigint, { readonly name: string; readonly index: number; readonly member: string; readonly title: string | null }>;

export interface InputConstraints {
  readonly min: DeclaredInputValue | null;
  readonly max: DeclaredInputValue | null;
  readonly step: DeclaredInputValue | null;
  readonly options: ReadonlyArray<DeclaredInputValue>;
}

export interface InputActiveDirect {
  readonly input: number;
  readonly inverted: boolean;
}

export type InputActiveUnaryOp = "not" | "neg" | "int_to_float";
export type InputActiveBinaryOp =
  | "add"
  | "sub"
  | "mul"
  | "div"
  | "rem"
  | "lt"
  | "le"
  | "gt"
  | "ge"
  | "eq"
  | "ne"
  | "and"
  | "or";

export type InputActiveExpression =
  | { readonly kind: "value"; readonly value: DeclaredInputValue }
  | { readonly kind: "na" }
  | { readonly kind: "input"; readonly input: number }
  | { readonly kind: "unary"; readonly op: InputActiveUnaryOp; readonly operand: InputActiveExpression }
  | { readonly kind: "binary"; readonly op: InputActiveBinaryOp; readonly lhs: InputActiveExpression; readonly rhs: InputActiveExpression }
  | { readonly kind: "ternary"; readonly condition: InputActiveExpression; readonly when_true: InputActiveExpression; readonly when_false: InputActiveExpression }
  | { readonly kind: "call"; readonly function: string; readonly args: ReadonlyArray<InputActiveExpression> };

export interface InputActive {
  readonly default: boolean;
  readonly dependencies: ReadonlyArray<number>;
  readonly direct: InputActiveDirect | null;
  readonly expression: InputActiveExpression | null;
}

export interface InputDeclaration {
  readonly id: number;
  readonly kind: InputKind;
  readonly widget: InputWidget;
  readonly title: string | null;
  readonly tooltip: string | null;
  readonly inline: string | null;
  readonly group: string | null;
  readonly confirm: boolean;
  readonly display: InputDisplay;
  readonly active: InputActive;
  readonly defval: DeclaredInputValue;
  readonly constraints: InputConstraints;
  readonly span: SourceSpan;
}

/**
 * Symbol metadata is a partial input object. Keys such as
 * `main_tickerid` and `recommendations_buy_strong` deliberately stay
 * snake_case; `type` and `expirationDate` are the two explicit exceptions.
 */
export interface SymbolInfo {
  readonly ticker?: string;
  readonly tickerid?: string;
  readonly currency?: string;
  readonly type?: string;
  /**
   * The exchange timezone: a fixed offset (`"UTC-5"`) or an IANA name
   * (`"America/New_York"`, whose daylight-saving changes are followed).
   * Market data naming any other is refused. Defaults to `"UTC"`.
   */
  readonly timezone?: string;
  readonly basecurrency?: string;
  readonly description?: string;
  readonly country?: string;
  readonly industry?: string;
  readonly sector?: string;
  readonly root?: string;
  readonly prefix?: string;
  readonly session?: string;
  /**
   * The hours the symbol trades, as a session specification on
   * the exchange's clock (`"24x7"`, `"0930-1600"`, `"1700-1600:1234567"`).
   * Higher-timeframe buckets on a non-UTC exchange need it. Empty declares
   * none.
   */
  readonly session_hours?: string;
  readonly volumetype?: string;
  readonly main_tickerid?: string;
  /** ISIN supplied by the symbol provider; empty when unavailable. */
  readonly isin?: string;
  /**
   * `syminfo.current_contract`: the contract the symbol currently follows,
   * as the host supplies it. Empty when unavailable, which reads as `na`.
   */
  readonly current_contract?: string;
  readonly minmove?: IntegerInput;
  readonly pricescale?: IntegerInput;
  readonly pointvalue?: number;
  readonly mincontract?: number;
  readonly expirationDate?: IntegerInput | null;
  readonly employees?: IntegerInput | null;
  readonly shareholders?: IntegerInput | null;
  readonly shares_outstanding_float?: number | null;
  readonly shares_outstanding_total?: number | null;
  readonly recommendations_buy?: IntegerInput | null;
  readonly recommendations_buy_strong?: IntegerInput | null;
  readonly recommendations_date?: IntegerInput | null;
  readonly recommendations_hold?: IntegerInput | null;
  readonly recommendations_sell?: IntegerInput | null;
  readonly recommendations_sell_strong?: IntegerInput | null;
  readonly recommendations_total?: IntegerInput | null;
  readonly target_price_average?: number | null;
  readonly target_price_date?: IntegerInput | null;
  readonly target_price_estimates?: IntegerInput | null;
  readonly target_price_high?: number | null;
  readonly target_price_low?: number | null;
  readonly target_price_median?: number | null;
}

/** Keys under `dividends.*` and `earnings.*` remain snake_case. */
export interface UpcomingEvents {
  readonly dividends_future_amount?: number | null;
  readonly dividends_future_ex_date?: IntegerInput | null;
  readonly dividends_future_pay_date?: IntegerInput | null;
  readonly earnings_future_eps?: number | null;
  readonly earnings_future_period_end_time?: IntegerInput | null;
  readonly earnings_future_revenue?: number | null;
  readonly earnings_future_time?: IntegerInput | null;
}

/** Keys under `volume_row.*` remain snake_case. */
export interface HostVolumeRow {
  readonly buy_volume?: number;
  readonly sell_volume?: number;
  readonly total_volume?: number;
  readonly delta?: number;
  readonly up_price?: number;
  readonly down_price?: number;
  readonly has_buy_imbalance?: boolean;
  readonly has_sell_imbalance?: boolean;
}

/** Keys under `footprint.*` remain snake_case. */
export interface HostFootprint {
  readonly buy_volume?: number;
  readonly sell_volume?: number;
  readonly total_volume?: number;
  readonly delta?: number;
  readonly poc?: number;
  readonly vah?: number;
  readonly val?: number;
  readonly rows?: ReadonlyArray<HostVolumeRow>;
}

/** Host-supplied same-symbol bars for one higher-timeframe request. */
export interface RequestTimeframeBars {
  readonly time: ReadonlyArray<IntegerInput>;
  readonly timeClose: ReadonlyArray<IntegerInput>;
  readonly open: ReadonlyArray<number>;
  readonly high: ReadonlyArray<number>;
  readonly low: ReadonlyArray<number>;
  readonly close: ReadonlyArray<number>;
  readonly volume: ReadonlyArray<number>;
}

export interface MarketData {
  readonly open: ReadonlyArray<number>;
  readonly high: ReadonlyArray<number>;
  readonly low: ReadonlyArray<number>;
  readonly close: ReadonlyArray<number>;
  readonly volume: ReadonlyArray<number>;
  readonly mintick?: number;
  readonly time?: ReadonlyArray<IntegerInput>;
  readonly timeClose?: ReadonlyArray<IntegerInput>;
  readonly requestTimeClose?: StringTable<ReadonlyArray<IntegerInput>>;
  readonly requestBars?: StringTable<RequestTimeframeBars>;
  readonly tradingDay?: ReadonlyArray<IntegerInput>;
  readonly timeframe?: string;
  readonly syminfo?: SymbolInfo;
  readonly requestData?: StringTable<ReadonlyArray<number>>;
  readonly footprintData?: StringTable<ReadonlyArray<HostFootprint | null>>;
  readonly bid?: ReadonlyArray<number>;
  readonly ask?: ReadonlyArray<number>;
  readonly securityLowerTfData?: StringTable<ReadonlyArray<ReadonlyArray<number>>>;
  readonly chartLeftVisibleBarTime?: IntegerInput | null;
  readonly chartRightVisibleBarTime?: IntegerInput | null;
  readonly upcomingEvents?: UpcomingEvents;
}

export interface Tick {
  readonly barIndex: IntegerInput;
  readonly open: number;
  readonly high: number;
  readonly low: number;
  readonly close: number;
  readonly volume: number;
  readonly time: IntegerInput;
  readonly tradingDay?: IntegerInput | null;
  readonly isBarClose: boolean;
}

export interface ExternalSourceLane {
  readonly source: ExternalSourceRef;
  readonly values: ReadonlyArray<number | null>;
}

export interface StrategyRunOverrides {
  readonly initialCapital?: number | null;
  readonly defaultQtyType?: "fixed" | "cash" | "percent_of_equity" | null;
  readonly defaultQtyValue?: number | null;
  readonly commissionType?: "percent" | "cash_per_contract" | "cash_per_order" | null;
  readonly commissionValue?: number | null;
  readonly slippage?: number | null;
  readonly pyramiding?: number | null;
  readonly backtestFillLimitsAssumption?: number | null;
  readonly processOrdersOnClose?: boolean | null;
  readonly closeEntriesRule?: "FIFO" | "ANY" | null;
  readonly calcOnEveryTick?: boolean | null;
  readonly calcOnOrderFills?: boolean | null;
  readonly calcOnEveryHistoryTick?: boolean | null;
  readonly currency?: string | null;
  readonly marginLong?: number | null;
  readonly marginShort?: number | null;
  readonly useBarMagnifier?: boolean | null;
  readonly fillOrdersOnStandardOhlc?: boolean | null;
  readonly calcBarsCount?: number | null;
}

/** Omit unused properties; Serde rejects a property set to JavaScript `undefined`. */
export interface RunOptions {
  readonly maxInstructionsPerBar?: IntegerInput;
  readonly maxInstructionsPerRun?: IntegerInput;
  readonly maxStringBytesPerRun?: IntegerInput;
  readonly maxVmBytesPerRun?: IntegerInput;
  readonly maxStackDepth?: IntegerInput;
  readonly maxBars?: IntegerInput | null;
  readonly realtime?: { readonly ticks: ReadonlyArray<Tick> } | null;
  readonly realtimeTrace?: boolean;
  readonly profile?: boolean;
  /** A native JavaScript Map keyed by the numeric id from Program.inputs. */
  readonly inputs?: Map<number, InputValue>;
  readonly externalSources?: Map<number, ExternalSourceLane>;
  readonly strategy?: StrategyRunOverrides;
  readonly maxCollectionElements?: IntegerInput;
}

export interface RenderingContract {
  readonly schemaVersion: number;
  readonly coordinateUnits: Readonly<Record<string, string>>;
  readonly drawingIdentity: string;
  readonly missingNumeric: string;
  readonly colorModel: {
    readonly rgb: { readonly type: "u8"; readonly range: readonly [0, 255] };
    readonly transparency: { readonly type: "f64"; readonly range: readonly [0, 100]; readonly wholeJson: "integer"; readonly fractionJson: "number" };
    readonly paintAlpha: { readonly bits: 8; readonly normalizationDecimalPlaces: 4; readonly rounding: "nearest-half-up" };
    readonly accessorResult: "float";
    readonly nativeTransparency: "proven-whole-or-reference";
  };
  readonly integerWire: { readonly domain: string; readonly safe: string; readonly unsafe: string; readonly presentation: string };
  readonly styleDefaults: DisplayStyle;
  readonly drawingFields: Readonly<Record<string, Readonly<Record<string, { readonly type: string; readonly default: unknown }>>>>;
  readonly cellFields: Readonly<Record<string, { readonly type: string; readonly default: unknown }>>;
  readonly drawingOptionDomains: Readonly<Record<string, Readonly<Record<string, Readonly<Record<string, string>>>>>>;
  readonly cellOptionDomains: Readonly<Record<string, Readonly<Record<string, string>>>>;
  readonly nominalScalarDomains: Readonly<Record<string, ReadonlyArray<string>>>;
  readonly dtoFields: Readonly<Record<string, ReadonlyArray<string>>>;
  readonly optionalDtoFields: Readonly<Record<string, ReadonlyArray<string>>>;
  readonly programHeader: { readonly magic: string; readonly il_version: number; readonly language_version: number; readonly runtime_abi_version: number };
  readonly taggedFields: Readonly<Record<string, Readonly<Record<string, ReadonlyArray<string>>>>>;
  readonly runtimeScalarQualifiers: ReadonlyArray<"Const" | "Input" | "Simple">;
  readonly constants: Readonly<Record<string, string>>;
  readonly bindingCalls: Readonly<Record<string, readonly [number, number]>>;
  readonly bindingFields: ReadonlyArray<string>;
  readonly bindingQualifiers: Readonly<Record<string, string>>;
  readonly colorRoleValueDomains: Readonly<Record<PlotColorRole, "float-or-na" | "color-or-na">>;
  readonly colorRolesByKind: Readonly<Record<string, ReadonlyArray<PlotColorRole>>>;
  readonly optionalStyleFields: ReadonlyArray<string>;
  readonly automaticColorDefaults: { readonly editable: boolean; readonly palette: ReadonlyArray<StyleColorPaletteEntry> };
  readonly outputColorDefaults: Readonly<Record<string, Readonly<Partial<Record<PlotColorRole, readonly [number, number, number, number]>>>>>;
  readonly enums: Readonly<Record<string, ReadonlyArray<string>>>;
  readonly programRuntimeAbi: { readonly default: number; readonly taInputBindings: number; readonly distanceScans: number };
}

export type DisplayKind =
  | "plot"
  | "plot_shape"
  | "plot_char"
  | "plot_arrow"
  | "plot_candle"
  | "plot_bar"
  | "hline"
  | "bgcolor"
  | "barcolor";
export type PlotStyleKind =
  | "line"
  | "line_break"
  | "step_line"
  | "step_line_break"
  | "step_line_diamond"
  | "histogram"
  | "cross"
  | "area"
  | "area_break"
  | "columns"
  | "circles";
export type PlotLineStyle = "solid" | "dotted" | "dashed";
export type PlotDisplay = "all" | "none" | "pane" | "data_window" | "price_scale" | "status_line" | "pane_price_scale" | "pane_status_line" | "price_scale_status_line" | "pane_price_scale_status_line" | "pane_data_window" | "price_scale_data_window" | "pane_price_scale_data_window" | "status_line_data_window" | "pane_status_line_data_window" | "price_scale_status_line_data_window" | "pane_price_scale_status_line_data_window" | "pine_screener" | "pane_pine_screener" | "price_scale_pine_screener" | "pane_price_scale_pine_screener" | "status_line_pine_screener" | "pane_status_line_pine_screener" | "price_scale_status_line_pine_screener" | "pane_price_scale_status_line_pine_screener" | "data_window_pine_screener" | "pane_data_window_pine_screener" | "price_scale_data_window_pine_screener" | "pane_price_scale_data_window_pine_screener" | "status_line_data_window_pine_screener" | "pane_status_line_data_window_pine_screener" | "price_scale_status_line_data_window_pine_screener";
export type OhlcField = "open" | "high" | "low" | "close";
export type MarkerLocation = "above_bar" | "below_bar" | "in_bar" | "top" | "bottom" | "absolute";
export type MarkerShape =
  | "x_cross"
  | "cross"
  | "circle"
  | "triangle_up"
  | "triangle_down"
  | "flag"
  | "arrow_up"
  | "arrow_down"
  | "square"
  | "diamond"
  | "label_up"
  | "label_down";
export type MarkerSize = "auto" | "tiny" | "small" | "normal" | "large" | "huge";

export interface UnsupportedVisualOption {
  readonly field: string;
  readonly reason: string;
}

export interface BoundStyleScalar {
  readonly field: string;
  readonly expression: InputActiveExpression;
}

/** Compiler-owned metadata. Runtime snapshots replace these with literals. */
export interface RuntimeStyleScalar {
  readonly field: string;
  readonly slot: number;
  readonly qualifier: "Const" | "Input" | "Simple";
}

/** Static arrow style provenance; dynamic does not imply a colorer lane. */
export type ArrowStaticColorOrigin = "omitted" | "constant" | "missing" | "dynamic";
export interface ArrowStaticRoleColor {
  readonly origin: ArrowStaticColorOrigin;
  readonly r: number;
  readonly g: number;
  readonly b: number;
  /** Exact transparency percentage. Static alpha is 1 - t / 100. */
  readonly t: number;
}
export interface ArrowStaticColors {
  readonly up: ArrowStaticRoleColor;
  readonly down: ArrowStaticRoleColor;
}
/** Public paint binding. False preserves the raw lane but uses static paint. */
export interface ArrowRuntimeColorers {
  readonly up: boolean;
  readonly down: boolean;
}
export interface OhlcBodyColor {
  readonly r: number;
  readonly g: number;
  readonly b: number;
  readonly t: number;
}
export interface OhlcBodyStyle {
  readonly staticColorPresent: boolean;
  readonly staticColor: OhlcBodyColor | null;
  readonly runtimeColorer: boolean;
}
export interface DisplayStyle {
  readonly sourceOrder?: number;
  readonly kind: DisplayKind;
  readonly plotStyle: PlotStyleKind;
  readonly lineStyle: PlotLineStyle;
  readonly lineWidth: number;
  readonly display: PlotDisplay;
  readonly histogramBase: number;
  readonly offset: number;
  readonly showLast: number | null;
  readonly forceOverlay: boolean;
  readonly trackPrice: boolean;
  readonly join: boolean;
  /** Continue fills across missing plot values; omitted means false. */
  readonly fillGaps?: boolean;
  /** Qualifier-derived fill paint registration; absent on non-fill outputs. */
  readonly fillColorMode?: FillColorMode;
  readonly plotColorMode?: PlotColorMode;
  readonly arrowStaticColors?: ArrowStaticColors;
  readonly arrowRuntimeColorers?: ArrowRuntimeColorers;
  readonly ohlcBodyStyle?: OhlcBodyStyle;
  readonly editable: boolean;
  readonly colorEditable?: boolean;
  readonly colorPalette?: ReadonlyArray<StyleColorPaletteEntry>;
  readonly format: string | null;
  readonly precision: number | null;
  readonly group: number | null;
  readonly ohlcField: OhlcField | null;
  readonly markerShape: MarkerShape | null;
  readonly markerLocation: MarkerLocation | null;
  readonly markerSize: MarkerSize | null;
  readonly character: string | null;
  readonly text: string | null;
  readonly minHeight: number | null;
  readonly maxHeight: number | null;
  readonly unsupported: ReadonlyArray<UnsupportedVisualOption>;
  readonly boundScalars?: ReadonlyArray<BoundStyleScalar>;
  readonly runtimeScalars?: ReadonlyArray<RuntimeStyleScalar>;
}

export type PlotColorRole = "color" | "up" | "down" | "wick" | "border" | "text" | "top_color" | "bottom_color" | "top_value" | "bottom_value";

export type FillColorMode = "fixed" | "series";
export type PlotColorMode = "fixed" | "series";

/** Serialized CSIL plot declaration retained by the compiler contract. */
export interface CsilPlot {
  readonly title: string | null;
  readonly style: DisplayStyle;
  /** Omitted when the plot declares no color argument. */
  readonly color_roles?: ReadonlyArray<PlotColorRole>;
}

/** Serialized color-only visual declaration retained by the compiler contract. */
export interface CsilVisual {
  readonly title: string | null;
  readonly style: DisplayStyle;
  readonly color_roles: ReadonlyArray<PlotColorRole>;
}

/** Serialized fill declaration; its endpoints identify plot slots. */
export interface CsilFill {
  readonly plot1: number;
  readonly plot2: number;
  readonly title: string | null;
  readonly style: DisplayStyle;
  readonly color_roles: ReadonlyArray<PlotColorRole>;
}

/** Source-mapped HIR metadata for one numeric plot declaration. */
export interface PlotInfo {
  readonly slot: number;
  readonly title: string | null;
  readonly style: DisplayStyle;
  /** Omitted when the plot declares no color argument. */
  readonly color_roles?: ReadonlyArray<PlotColorRole>;
  readonly span: SourceSpan;
}

/** Source-mapped HIR metadata for one color-only chart visual. */
export interface VisualInfo {
  readonly slot: number;
  readonly title: string | null;
  readonly style: DisplayStyle;
  readonly color_roles: ReadonlyArray<PlotColorRole>;
  readonly span: SourceSpan;
}

/** Source-mapped HIR metadata for one fill between two plot slots. */
export interface FillInfo {
  readonly slot: number;
  readonly plot1: number;
  readonly plot2: number;
  readonly title: string | null;
  readonly style: DisplayStyle;
  readonly color_roles: ReadonlyArray<PlotColorRole>;
  readonly span: SourceSpan;
}

/** Per-kind drawing limits inherited from an indicator or strategy declaration. */
export interface DrawingCaps {
  readonly max_lines_count: number | null;
  readonly max_labels_count: number | null;
  readonly max_boxes_count: number | null;
  readonly max_polylines_count: number | null;
}

/** Const-folded presentation and history metadata from indicator(...). */
export interface IndicatorInfo {
  readonly title: string;
  readonly shorttitle: string | null;
  readonly format: string | null;
  readonly precision: number | null;
  readonly scale: string | null;
  readonly explicit_plot_zorder: boolean;
  readonly behind_chart: boolean;
  readonly max_bars_back: number | null;
  readonly max_lines_count: number | null;
  readonly max_labels_count: number | null;
  readonly max_boxes_count: number | null;
  readonly max_polylines_count: number | null;
  readonly calc_on_every_tick: boolean;
  readonly overlay: boolean;
  readonly span: SourceSpan;
}

/** Strategy declaration values serialized with a compiled CSIL program. */
export interface StrategyInfo {
  readonly title: string;
  readonly shorttitle: string;
  readonly overlay: boolean;
  readonly format: string;
  readonly precision: number | null;
  readonly scale: string | null;
  readonly max_bars_back: number | null;
  readonly initial_capital: number;
  readonly default_qty_type: string;
  readonly default_qty_value: number;
  readonly commission_type: string;
  readonly commission_value: number;
  readonly slippage: number;
  readonly pyramiding: number;
  readonly backtest_fill_limits_assumption: number;
  readonly process_orders_on_close: boolean;
  readonly close_entries_rule: string;
  readonly calc_on_every_tick: boolean;
  readonly calc_on_order_fills: boolean;
  readonly currency: string;
  readonly margin_long: number;
  readonly margin_short: number;
  readonly explicit_plot_zorder: boolean;
  readonly max_lines_count: number | null;
  readonly max_labels_count: number | null;
  readonly max_boxes_count: number | null;
  readonly max_polylines_count: number | null;
  readonly calc_bars_count: number;
  readonly risk_free_rate: number;
  readonly use_bar_magnifier: boolean;
  readonly fill_orders_on_standard_ohlc: boolean;
  readonly dynamic_requests: boolean;
  readonly behind_chart: boolean;
  readonly calc_on_every_history_tick: boolean;
  readonly alert_message: string;
  readonly span: SourceSpan;
}

/** Versioned metadata header carried by the serialized CSIL program. */
export interface ProgramMetadata {
  readonly rendering_contract_version?: number;
  readonly title: string | null;
  readonly indicator?: IndicatorInfo;
  readonly overlay: boolean;
  readonly strategy?: StrategyInfo;
  readonly drawing_caps?: DrawingCaps;
  readonly max_bars_back: number;
}

/** Compiler-internal specialization links emitted with the opaque CSIL program. */
export type TaInputTarget =
  | { readonly Sma: { readonly slot: number } }
  | { readonly Change: { readonly slot: number } }
  | { readonly State: { readonly state: number; readonly param: number } };

export interface TaInputBinding {
  readonly input: number;
  readonly negated: boolean;
  readonly target: TaInputTarget;
}

export interface StyleColorPaletteEntry {
  readonly role: PlotColorRole;
  readonly source: { readonly kind: "constant"; readonly r: number; readonly g: number; readonly b: number; readonly t: number } | { readonly kind: "input"; readonly input: number };
  readonly resolvedColor?: string;
}

export interface PlotColorSeries {
  readonly role: PlotColorRole;
  readonly values: ReadonlyArray<PineValue>;
  /** Compiler palette entry indices, one per value; independent of RGBA. */
  readonly paletteIndices: ReadonlyArray<number | null>;
}

export interface PlotSeries {
  readonly slot: number;
  readonly title: string | null;
  readonly style: DisplayStyle;
  readonly colors?: ReadonlyArray<PlotColorSeries>;
  readonly values: ReadonlyArray<PineValue>;
}

export interface VisualSeries {
  readonly slot: number;
  readonly title: string | null;
  readonly style: DisplayStyle;
  readonly colors: ReadonlyArray<PlotColorSeries>;
}

export interface FillSeries {
  readonly slot: number;
  readonly plot1: number;
  readonly plot2: number;
  readonly title: string | null;
  readonly style: DisplayStyle;
  readonly colors: ReadonlyArray<PlotColorSeries>;
}

export interface PolyPoint {
  readonly index: PineValue;
  readonly time: PineValue;
  readonly price: PineValue;
}

export interface TableCell {
  readonly column: bigint;
  readonly row: bigint;
  readonly merged?: boolean;
  readonly merge?: readonly [bigint, bigint, bigint, bigint];
  readonly attrs: Readonly<Record<string, PineValue>>;
}

export interface DrawingObject {
  readonly kind: string;
  readonly id: number;
  readonly fields: Readonly<Record<string, PineValue>>;
  readonly cells?: ReadonlyArray<TableCell>;
  readonly points?: ReadonlyArray<PolyPoint>;
  readonly lineRefs?: readonly [number | null, number | null];
}

export interface Drawings {
  readonly lines?: ReadonlyArray<DrawingObject>;
  readonly labels?: ReadonlyArray<DrawingObject>;
  readonly boxes?: ReadonlyArray<DrawingObject>;
  readonly tables?: ReadonlyArray<DrawingObject>;
  readonly polylines?: ReadonlyArray<DrawingObject>;
  readonly linefills?: ReadonlyArray<DrawingObject>;
}

export type LogLevel = "info" | "warning" | "error";

export interface LogEntry {
  readonly level: LogLevel;
  readonly bar: number;
  readonly message: string;
}

export interface AlertEvent {
  readonly bar: number;
  readonly message: string;
  readonly freq: string;
}

export type RuntimeErrorKind =
  | "invalid_ta_length"
  | "malformed_market_data"
  | "instruction_limit_exceeded"
  | "run_instruction_limit_exceeded"
  | "stack_overflow"
  | "stack_underflow"
  | "type_mismatch"
  | "invalid_program"
  | "collection_index_out_of_bounds"
  | "string_index_out_of_bounds"
  | "collection_limit_exceeded"
  | "vm_memory_limit_exceeded"
  | "string_limit_exceeded"
  | "na_method_receiver"
  | "na_field_access"
  | "matrix_dimension_mismatch"
  | "matrix_singular"
  | "history_buffer_limit_exceeded"
  | "security_timeframe_below_chart"
  | "user_error"
  | "unsupported";

export interface RuntimeError {
  readonly kind: RuntimeErrorKind;
  readonly message: string;
  readonly barIndex: number | null;
  readonly instructionOffset: number | null;
  readonly span: SourceSpan | null;
}

export interface StrategyTrade {
  readonly index?: number;
  readonly entryId: string;
  readonly entryBar: number;
  readonly entryPrice: number;
  readonly exitBar: number;
  readonly exitPrice: number;
  readonly qty: number;
  readonly comment?: string;
  readonly pnl: number;
  readonly cumulativePnl?: number;
  readonly entryNotional?: number;
  readonly sign: number;
  readonly commission: number;
  readonly maxRunup: number;
  readonly maxDrawdown: number;
  readonly exitId?: string;
  /** Exit order name for a `strategy.close` or `strategy.close_all` order without its own id. */
  readonly exitOrderName?: string;
  readonly entryComment?: string;
}

export interface StrategyPosition {
  readonly size: number;
  readonly avgPrice: number;
}

export interface StrategyReportTradeHighlight {
  readonly tradeIndex: number;
  readonly entryId: string;
  readonly sign: number;
  readonly pnl: number;
  readonly returnPercent?: number;
}

/** Trades whose profit lies outside the mean plus or minus three population standard deviations of realized profit. Fence values are absent when undefined. */
export interface StrategyReportOutliers {
  readonly count: number;
  readonly totalPnl: number;
  readonly lowerFence?: number;
  readonly upperFence?: number;
  readonly tradeIndices: ReadonlyArray<number>;
}

export interface StrategyReportScopeAggregate {
  readonly closedTrades: number;
  readonly openTrades: number;
  readonly winTrades: number;
  readonly lossTrades: number;
  readonly evenTrades: number;
  readonly netProfit: number;
  readonly openProfit: number;
  readonly grossProfit: number;
  readonly grossLoss: number;
  readonly commission: number;
  readonly returnPercentSum: number;
  readonly returnPercentCount: number;
  readonly winningReturnPercentSum: number;
  readonly winningReturnPercentCount: number;
  readonly losingReturnPercentSum: number;
  readonly losingReturnPercentCount: number;
  readonly barsHeldSum: bigint;
  readonly barsHeldCount: number;
  readonly winningBarsHeldSum: bigint;
  readonly winningBarsHeldCount: number;
  readonly losingBarsHeldSum: bigint;
  readonly losingBarsHeldCount: number;
  readonly favorableExcursionSum: number;
  readonly favorableExcursionCount: number;
  readonly favorableExcursionMax?: number;
  readonly favorableExcursionPercentSum: number;
  readonly favorableExcursionPercentCount: number;
  readonly favorableExcursionPercentMax?: number;
  readonly adverseExcursionSum: number;
  readonly adverseExcursionCount: number;
  readonly adverseExcursionMax?: number;
  readonly adverseExcursionPercentSum: number;
  readonly adverseExcursionPercentCount: number;
  readonly adverseExcursionPercentMax?: number;
  readonly largestWinningTrade?: StrategyReportTradeHighlight;
  readonly largestLosingTrade?: StrategyReportTradeHighlight;
  readonly outliers: StrategyReportOutliers;
}

export interface StrategyReportAggregates {
  readonly all: StrategyReportScopeAggregate;
  readonly long: StrategyReportScopeAggregate;
  readonly short: StrategyReportScopeAggregate;
}

export interface StrategyOutcome {
  readonly initialCapital: number;
  readonly trades: ReadonlyArray<StrategyTrade>;
  readonly firstRetainedTradeIndex?: number;
  readonly retainedTradePnlBaseline?: number;
  readonly firstEntryBar?: number;
  readonly firstEntryPrice?: number;
  readonly firstEntryAccountNotionalPerUnit?: number;
  readonly accountCurrency: string;
  readonly symbolCurrency: string;
  readonly currencyRates: ReadonlyArray<number | null>;
  readonly currencyConversionComplete: boolean;
  readonly position: StrategyPosition;
  readonly equity: ReadonlyArray<number>;
  readonly maxContractsHeld: number;
  readonly maxContractsHeldLong: number;
  readonly maxContractsHeldShort: number;
  readonly marginUsed: ReadonlyArray<number>;
  readonly maximumMarginUsed: number;
  readonly requiredAccountSize?: number;
  readonly reportAggregates?: StrategyReportAggregates;
  readonly marginCalls: number;
  readonly totalLiquidated: number;
  readonly largestLiquidation: number;
  readonly netProfit: number;
  readonly openProfit: number;
  readonly grossProfit: number;
  readonly grossLoss: number;
  readonly closedTrades: number;
  readonly winTrades: number;
  readonly lossTrades: number;
  readonly evenTrades: number;
  readonly avgTrade: number;
  readonly maxDrawdown: number;
  readonly maxDrawdownPercent?: number;
  readonly openEntryCommission?: number;
  readonly maxRunup: number;
}

export interface PlotTickSample {
  readonly tickOrdinal: number;
  readonly barIndex: number;
  readonly isBarClose: boolean;
  readonly values: ReadonlyArray<PineValue>;
}

export interface VmProfile {
  /** Adjacent instruction tails executed without a separate dispatch-loop iteration.
   * Logical counts and instruction units still include every instruction.
   * Absent in older runtime packages; treat absence as zero. */
  readonly combinedDispatchCount?: number;
  /** Per-PC skipped tails, parallel to count; sums to combinedDispatchCount.
   * Subtract from count to rank remaining physical dispatch work.
   * Absent in older runtime packages. */
  readonly combinedDispatchByPc?: ReadonlyArray<number>;
  readonly spanSource: ReadonlyArray<number>;
  readonly spanStart: ReadonlyArray<number>;
  readonly spanEnd: ReadonlyArray<number>;
  readonly count: ReadonlyArray<number>;
  readonly units: ReadonlyArray<number>;
  readonly flow: ReadonlyArray<number>;
}

/**
 * Fields marked with `?` may be omitted when empty or unavailable. Fields
 * typed with `| null` remain present and use null for an absent payload.
 */
export interface RunResult {
  readonly diagnostics: ReadonlyArray<Diagnostic>;
  readonly runtimeErrors: ReadonlyArray<RuntimeError>;
  readonly plots: ReadonlyArray<PlotSeries>;
  readonly visuals?: ReadonlyArray<VisualSeries>;
  readonly fills?: ReadonlyArray<FillSeries>;
  readonly barsExecuted: number;
  readonly realtimeTrace?: ReadonlyArray<PlotTickSample>;
  readonly drawings?: Drawings;
  readonly logs?: ReadonlyArray<LogEntry>;
  readonly alerts?: ReadonlyArray<AlertEvent>;
  readonly strategy?: StrategyOutcome;
  readonly compile: CompileResult | null;
  readonly runMs?: number;
  readonly profile?: VmProfile;
}

/** Owned numeric plot values; Pine `na` is represented by NaN. */
export type NumericPlotSeries = Omit<PlotSeries, "values" | "colors"> & {
  readonly values: Float64Array;
};

/** Complete strategy metadata with owned numeric equity and margin lanes. */
export type NumericStrategyOutcome = Omit<StrategyOutcome, "equity" | "marginUsed"> & {
  readonly equity: Float64Array;
  readonly marginUsed: Float64Array;
};

/** Explicit numeric projection; execution still computes all script effects. */
export type NumericRunResult = Omit<RunResult, "plots" | "strategy" | "visuals" | "fills" | "drawings"> & {
  readonly plots: ReadonlyArray<NumericPlotSeries>;
  readonly strategy?: NumericStrategyOutcome;
};

export type OutlineKind =
  | "declaration"
  | "import"
  | "group"
  | "input"
  | "variable"
  | "function"
  | "local"
  | "type"
  | "field"
  | "enum"
  | "member"
  | "plot"
  | "alert"
  | "strategy";

export interface OutlineNode {
  readonly label: string;
  readonly kind: OutlineKind;
  readonly detail: string | null;
  readonly line: number;
  readonly children: ReadonlyArray<OutlineNode>;
}

export type DocumentSymbolKind =
  | "variable"
  | "function"
  | "parameter"
  | "type"
  | "field"
  | "enum member"
  | "method"
  | "library alias";

export interface DocumentSymbol {
  readonly name: string;
  readonly kind: DocumentSymbolKind;
  readonly detail: string;
  readonly signature: string | null;
  readonly container: string | null;
  readonly line: number;
  readonly character: number;
  readonly endLine: number;
}

export interface SearchFile {
  readonly path: string;
  readonly content: string;
}

export interface SearchOptions {
  readonly caseSensitive: boolean;
  readonly smartCase?: boolean;
  readonly wholeWord: boolean;
  readonly isRegex: boolean;
  readonly maxResults: number;
}

export interface SearchMatch {
  readonly line: bigint;
  readonly lineText: string;
  readonly ranges: ReadonlyArray<readonly [startColumn: number, endColumn: number]>;
}

export interface FileMatches {
  readonly path: string;
  readonly matches: ReadonlyArray<SearchMatch>;
}

export interface SearchResults {
  readonly files: ReadonlyArray<FileMatches>;
  readonly hitLimit: boolean;
  readonly total: bigint;
}

export interface CsawModule {
  readonly wasm: Uint8Array;
  readonly abiVersion: number;
  readonly slotCount: number;
  readonly plotCount: number;
  readonly ring: number;
  readonly maxStack: number;
  readonly executionMode: "native" | "hosted";
  readonly nativeBlocker: string | null;
  readonly linksTa: boolean;
  readonly linksRuntime: boolean;
  readonly alertSites: number;
  readonly importsVm: boolean;
  readonly nativeHeap: boolean;
  readonly nativeHeapHot: boolean;
  /** Ordered Rust search kernels; their work is opaque to static host-site counts. */
  readonly distanceScanKernels: number;
  readonly vmHeapResidual: boolean;
  readonly importsInterpreter: boolean;
  /** A strategy chart module whose broker is the `AotHost`'s. */
  readonly hostBroker: boolean;
  /**
   * How many of the program's instructions the module leaves to its host:
   * the `host.vm` sites of a module that imports its host, and the calls a
   * linked module makes into the VM it carries.
   */
  readonly hostSites: number;
  /**
   * How many of those instructions are inside a loop. One outside every loop
   * runs at most once a bar; one inside may run on every iteration.
   */
  readonly hostSitesInLoops: number;
  /**
   * The module links the VM as its own host instead of importing
   * `host.vm` (`compileChartWasmProgram`). Its loader passes the request to
   * the module's `chartInit` export and reads the host's results through
   * `chartCommand`.
   */
  readonly linkedVm: boolean;
  readonly scratchCells: number;
  readonly sourceInputs: ReadonlyArray<number>;
  readonly styleSlots: ReadonlyArray<NativeStyleSlot>;
}

export interface NativeStyleSlot {
  readonly slot: number;
  readonly tag: number;
  readonly offset: number;
  readonly mask: number;
}

export interface NativeDisplayStyles {
  readonly plots: ReadonlyArray<DisplayStyle>;
  readonly visuals: ReadonlyArray<DisplayStyle>;
  readonly fills: ReadonlyArray<DisplayStyle>;
}

export interface UnsupportedCsawModule {
  readonly unsupported: string;
}

export type CsawCompileResult = CsawModule | UnsupportedCsawModule;

export interface StandaloneWasmOptions {
  readonly signal_plot?: IntegerInput | null;
  readonly module_type: "strategy" | "indicator";
  readonly signal_semantics?: "position-events" | "posture-events" | null;
  readonly max_collection_elements?: number | null;
  /** Compile only what can reach the designated plot. Defaults to `true`. */
  readonly signal_slice?: boolean | null;
}

export type StandaloneWasmRefusalCode =
  | "StandaloneWasmNotStandalone"
  | "StandaloneWasmSourceInputUnsupported"
  | "StandaloneWasmNoDesignatedPlot"
  | "StandaloneWasmAuditFailed"
  | "StandaloneWasmStrategyFeatureUnsupported"
  | "StandaloneWasmProgramUnsupported";

/**
 * `capability`: a declared limit of the compiler or of the requested build.
 * `internal`: not reachable for a program this compiler produced; for such a
 * program it is a compiler defect to report, not a script to change.
 */
export type StandaloneWasmRefusalClass = "capability" | "internal";

/**
 * Where a refusal points. `pc` is an instruction index in the program the
 * compiler examined, which with `signal_slice` may be the signal slice rather
 * than the supplied program; `input` is an input declaration id; `plot` is
 * the `signal_plot` the caller passed.
 */
export type StandaloneWasmRefusalSite =
  | { readonly pc: number }
  | { readonly input: number }
  | { readonly plot: number };

export interface StandaloneWasmRefusal {
  readonly code: StandaloneWasmRefusalCode;
  readonly message: string;
  readonly imports?: ReadonlyArray<string>;
  /** What in the script the refusal is about, comma-separated: the strategy feature, or the Pine calls that need `host.vm`. */
  readonly feature?: string;
  /** Classify on this, never on `message`. `code` does not separate the two classes. */
  readonly class: StandaloneWasmRefusalClass;
  /** The compiler's name for the refusal, finer than `code` (`UnsupportedOpcode`, `TypeConflict`, ...). */
  readonly kind: string;
  /** Absent where the refusal has no single site or the compiler does not report one. */
  readonly site?: StandaloneWasmRefusalSite;
}

export interface StandaloneWasmModule {
  readonly wasm: Uint8Array;
  readonly moduleType: "strategy" | "indicator";
  readonly signalPlot: number | null;
  readonly moduleSha256: string;
}

export interface RefusedStandaloneWasmModule {
  readonly refused: StandaloneWasmRefusal;
}

export type StandaloneWasmCompileResult = StandaloneWasmModule | RefusedStandaloneWasmModule;

export interface VersionInfo {
  readonly compilerVersion: string;
  readonly ilVersion: number;
  readonly languageVersion: number;
  /** Rendering contract version: `renderingContract().schemaVersion` and the `rendering_contract_version` of every program this compiler emits. */
  readonly renderingContractVersion: number;
  /** The IANA Time Zone Database release timezone names are read with, e.g. `"2026e"`. */
  readonly tzdataVersion: string;
  readonly sourceRevision?: string;
}
"#;

#[cfg(feature = "migrate")]
#[wasm_bindgen(typescript_custom_section)]
const MIGRATION_CONTRACT: &str = r#"
/** One applied migration rule, or a site the migration cannot translate. */
export interface MigrationFinding {
  /** Stable identifier of the PineWASM rewrite rule applied, for example `C1`. */
  readonly rule: string;
  /**
   * `exact`: a local rewrite. `exactWithHoisting`: exact, but needs new
   * temporaries or restructured statements. `approximate`: compiles, but may
   * differ from the v5 code in edge cases. `diagnostic`: no v6 spelling
   * reproduces the v5 behavior; review by hand.
   */
  readonly class: "exact" | "exactWithHoisting" | "approximate" | "diagnostic";
  readonly severity: "info" | "warning" | "error";
  readonly message: string;
  /** UTF-8 byte range in the original source. */
  readonly start: number;
  readonly end: number;
  readonly applied: boolean;
}

export interface Migration {
  readonly status: "migrated" | "alreadyV6" | "unsupportedVersion" | "parseError";
  readonly sourceVersion: number | null;
  readonly output: string;
  readonly edits: ReadonlyArray<{ readonly start: number; readonly end: number; readonly newText: string }>;
  readonly findings: ReadonlyArray<MigrationFinding>;
  readonly parseErrors: ReadonlyArray<string>;
}
"#;

#[cfg(test)]
mod rendering_contract_tests {
    const DECLARATIONS: &str = include_str!("typescript.rs");
    /// The TypeScript contract text alone. `DECLARATIONS` also contains this
    /// test module, whose literals must not satisfy or confuse a check.
    static CONTRACT: std::sync::LazyLock<&str> = std::sync::LazyLock::new(|| {
        // The first occurrences are the contract itself, which precedes this module.
        let (_, tail) = DECLARATIONS
            .split_once("const TYPESCRIPT_CONTRACT: &str = r#\"")
            .unwrap();
        tail.split_once("\"#;").unwrap().0
    });
    #[test]
    fn rendering_contract_declares_every_generated_top_level_field() {
        let contract = pinewasm_compiler::rendering_contract();
        let (_, declaration) = DECLARATIONS
            .split_once("export interface RenderingContract {")
            .unwrap();
        let declaration = declaration.split("\n}\n").next().unwrap();
        let actual: std::collections::BTreeSet<_> = declaration
            .lines()
            .filter_map(|line| line.strip_prefix("  readonly "))
            .filter_map(|line| line.split_once(':').map(|(name, _)| name))
            .collect();
        let expected: std::collections::BTreeSet<_> = contract
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            actual, expected,
            "stale top-level RenderingContract declaration"
        );
    }
    #[test]
    fn cell_field_declarations_match_generated_metadata_properties() {
        let contract = pinewasm_compiler::rendering_contract();
        let declaration = DECLARATIONS
            .lines()
            .find(|line| line.starts_with("  readonly cellFields:"))
            .unwrap();
        let properties = declaration
            .split_once('{')
            .unwrap()
            .1
            .split_once('}')
            .unwrap()
            .0;
        let actual: std::collections::BTreeSet<_> = properties
            .split(';')
            .filter_map(|property| property.trim().strip_prefix("readonly "))
            .filter_map(|property| property.split_once(':').map(|(name, _)| name))
            .collect();
        for (name, metadata) in contract["cellFields"].as_object().unwrap() {
            let expected: std::collections::BTreeSet<_> = metadata
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(
                actual, expected,
                "stale table cell metadata declaration: {name}"
            );
        }
    }

    #[test]
    fn rendering_declarations_match_serde_domains_and_fields() {
        let contract = pinewasm_compiler::rendering_contract();
        for (name, values) in contract["enums"].as_object().unwrap() {
            // The host calls this InputActiveExpression but operator names agree.
            let prefix = format!("export type {name} =");
            let Some((_, tail)) = DECLARATIONS.split_once(&prefix) else {
                panic!("missing declaration {name}");
            };
            let declaration = tail.split(';').next().unwrap();
            let actual: std::collections::BTreeSet<_> = declaration
                .split('"')
                .enumerate()
                .filter_map(|(i, token)| (i % 2 == 1).then_some(token))
                .collect();
            let expected: std::collections::BTreeSet<_> = values
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            assert_eq!(actual, expected, "stale TypeScript enum {name}");
        }
        if let Err(error) = check_dto_declarations(*CONTRACT, &contract) {
            panic!("{error}");
        }
        let (_, tail) = DECLARATIONS
            .split_once("export interface DisplayStyle {")
            .unwrap();
        let body = tail.split("\n}\n").next().unwrap();
        for field in contract["styleDefaults"].as_object().unwrap().keys().chain(
            contract["optionalStyleFields"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect::<Vec<_>>()
                .iter(),
        ) {
            assert!(
                body.contains(&format!("readonly {field}:"))
                    || body.contains(&format!("readonly {field}?:")),
                "missing style field {field}"
            );
        }
    }
    use std::collections::{BTreeMap, BTreeSet};

    /// Top-level properties of `export interface {name} {`, each mapped to
    /// whether it is declared optional (`?:`).
    fn interface_fields(declarations: &str, name: &str) -> Result<BTreeMap<String, bool>, String> {
        let prefix = format!("export interface {name} {{");
        let (_, tail) = declarations
            .split_once(&prefix)
            .ok_or_else(|| format!("missing declaration {name}"))?;
        // Inline tagged-source fields contain braces. The interface closes
        // on its own line; stopping at an inner brace misses later fields.
        let body = tail.split("\n}\n").next().unwrap();
        let mut fields = BTreeMap::new();
        for line in body.lines() {
            let Some(property) = line.strip_prefix("  readonly ") else {
                // Beside the properties only comments, blank lines, and the
                // deeper-indented or closing lines of a multi-line property
                // type may appear. Anything else is a property this check
                // would not see, such as one declared without `readonly`.
                let trimmed = line.trim_start();
                let indent = line.len() - trimmed.len();
                let comment = ["/*", "*", "//"]
                    .iter()
                    .any(|start| trimmed.starts_with(start));
                let closing = trimmed.starts_with(['}', ']', ')']);
                let continuation = indent > 2 || (indent == 2 && closing);
                if !trimmed.is_empty() && !comment && !continuation {
                    return Err(format!(
                        "{name}: cannot read line {line:?}; declare each property `readonly`"
                    ));
                }
                continue;
            };
            let Some((field, _)) = property.split_once(':') else {
                return Err(format!("{name}: cannot read property line {line:?}"));
            };
            let (field, optional) = match field.strip_suffix('?') {
                Some(field) => (field, true),
                None => (field, false),
            };
            if fields.insert(field.to_string(), optional).is_some() {
                return Err(format!("{name}.{field} is declared twice"));
            }
        }
        Ok(fields)
    }

    /// Fields of an inline `{ readonly a: ...; readonly b?: ... }` object
    /// type, each mapped to whether it is optional. A property declared
    /// without `readonly` is an error, so it cannot escape the check.
    fn object_literal_fields(literal: &str) -> Result<BTreeMap<String, bool>, String> {
        let mut fields = BTreeMap::new();
        for property in literal.split(';').map(str::trim) {
            if property.is_empty() {
                continue;
            }
            let (name, _) = property
                .strip_prefix("readonly ")
                .and_then(|property| property.split_once(':'))
                .ok_or_else(|| {
                    format!("cannot read property {property:?}; declare it `readonly`")
                })?;
            let (name, optional) = match name.strip_suffix('?') {
                Some(name) => (name, true),
                None => (name, false),
            };
            if fields.insert(name.to_string(), optional).is_some() {
                return Err(format!("{name} is declared twice in {literal:?}"));
            }
        }
        Ok(fields)
    }

    /// The Rust struct behind a rendering contract DTO name.
    fn rust_struct_name(name: &str) -> &str {
        match name {
            "TableCell" => "TableCellDto",
            "SourceSpan" => "Span",
            "DiagnosticLabel" => "Label",
            "PolyPoint" => "PolyPointDto",
            name => name,
        }
    }

    fn rust_sources(dir: &std::path::Path, files: &mut Vec<(std::path::PathBuf, String)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_sources(&path, files);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                files.push((path, text));
            }
        }
    }

    static WORKSPACE_SOURCES: std::sync::LazyLock<Vec<(std::path::PathBuf, String)>> =
        std::sync::LazyLock::new(|| {
            let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap();
            let mut files = Vec::new();
            for entry in std::fs::read_dir(crates).unwrap() {
                let src = entry.unwrap().path().join("src");
                if src.is_dir() {
                    rust_sources(&src, &mut files);
                }
            }
            files
        });

    /// The words of every `serde(...)` attribute in `attributes` (one or more
    /// `#[...]`), including one behind `cfg_attr(feature = "serde", ...)`.
    fn serde_words(attributes: &str) -> Vec<&str> {
        attributes
            .split("#[")
            .filter_map(|attribute| attribute.trim().strip_suffix(']'))
            .filter_map(|attribute| {
                let attribute = match attribute.strip_prefix("cfg_attr(") {
                    Some(inner) => inner
                        .trim_start()
                        .strip_prefix("feature = \"serde\"")?
                        .trim_start()
                        .strip_prefix(',')?
                        .trim()
                        .strip_suffix(')')?
                        .trim_end(),
                    None => attribute,
                };
                attribute.strip_prefix("serde(")?.strip_suffix(')')
            })
            .flat_map(|attribute| attribute.split(','))
            .map(str::trim)
            .filter(|word| !word.is_empty())
            .collect()
    }

    /// Serde's serialized form of a derived struct, read from its Rust source:
    /// each wire name mapped to whether `skip_serializing_if` can omit it.
    /// Fields Serde never serializes (`skip`, `skip_serializing`) are left
    /// out. The derived `Deserialize` field list cannot show either fact, so
    /// this is the source of truth for what the wire may omit. Anything the
    /// reader does not model fails instead of being guessed.
    fn serde_wire_fields(
        sources: &[(std::path::PathBuf, String)],
        struct_name: &str,
    ) -> Result<BTreeMap<String, bool>, String> {
        Ok(serde_fields(sources, struct_name, "Serialize")?
            .into_iter()
            .map(|(wire, field)| (wire, field.skip_serializing_if))
            .collect())
    }

    /// One field of a derived Serde struct, as its Rust source declares it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct SerdeField {
        /// `skip_serializing_if` may leave the field out of the output.
        skip_serializing_if: bool,
        /// Deserialization accepts an object without it: the field or its
        /// container has `default`, or the field is an `Option`, which
        /// Serde's derive fills with `None` when it is missing.
        absent_on_input: bool,
    }

    /// The wire fields of a struct that derives `derive` (`Serialize` for a
    /// value the package returns, `Deserialize` for one a host passes in).
    /// Fields Serde never serializes or deserializes (`skip`) are left out.
    fn serde_fields(
        sources: &[(std::path::PathBuf, String)],
        struct_name: &str,
        derive: &str,
    ) -> Result<BTreeMap<String, SerdeField>, String> {
        // `crate_name::Struct` restricts the search to one crate, for a name
        // that more than one crate declares.
        let (crate_dir, struct_name) = match struct_name.split_once("::") {
            Some((krate, name)) => (Some(format!("/{krate}/")), name),
            None => (None, struct_name),
        };
        let heads = [
            format!("\npub struct {struct_name} {{\n"),
            format!("\nstruct {struct_name} {{\n"),
        ];
        let found: Vec<_> = sources
            .iter()
            .filter(|(path, _)| {
                crate_dir
                    .as_deref()
                    .is_none_or(|dir| path.to_string_lossy().contains(dir))
            })
            .flat_map(|(path, text)| {
                heads
                    .iter()
                    .filter_map(move |head| text.find(head).map(|at| (path, text, at, head.len())))
            })
            .collect();
        let [(path, text, at, head_len)] = found[..] else {
            return Err(format!(
                "expected one `pub struct {struct_name}` in the workspace, found {}",
                found.len()
            ));
        };
        if text.contains(&format!("{derive} for {struct_name} "))
            || text.contains(&format!("{derive} for {struct_name}<"))
            || text.contains(&format!("{derive}<'de> for {struct_name} "))
        {
            return Err(format!("{struct_name} has a hand-written {derive} impl"));
        }
        // The item's own attributes sit directly above it.
        let attributes: Vec<&str> = text[..at]
            .lines()
            .rev()
            .take_while(|line| line.starts_with("#[") || line.starts_with("///"))
            .filter(|line| line.starts_with("#["))
            .collect();
        if !attributes.iter().any(|line| {
            (line.starts_with("#[derive(")
                || line.starts_with("#[cfg_attr(feature = \"serde\", derive("))
                && line.contains(derive)
        }) {
            return Err(format!("{struct_name} does not derive {derive}"));
        }
        let mut camel_case = false;
        let mut container_default = false;
        for word in attributes.iter().flat_map(|line| serde_words(line)) {
            match word {
                "rename_all = \"camelCase\"" => camel_case = true,
                "default" => container_default = true,
                // Identity for snake_case fields, or deserialize-only.
                "rename_all = \"snake_case\"" | "deny_unknown_fields" => {}
                other => {
                    return Err(format!(
                        "{struct_name}: unmodelled container attribute {other:?} in {}",
                        path.display()
                    ))
                }
            }
        }
        let body = text[at + head_len..].split("\n}\n").next().unwrap();
        let mut fields = BTreeMap::new();
        let mut serde = String::new();
        let mut open_attribute = 0_i32;
        for line in body.lines() {
            let trimmed = line.trim_start();
            let indent = line.len() - trimmed.len();
            if open_attribute > 0 || (indent == 4 && trimmed.starts_with("#[")) {
                open_attribute += trimmed.matches('[').count() as i32;
                open_attribute -= trimmed.matches(']').count() as i32;
                serde.push(' ');
                serde.push_str(trimmed);
                continue;
            }
            if indent != 4 || trimmed.starts_with("//") || trimmed.is_empty() {
                continue;
            }
            let declaration = trimmed
                .strip_prefix("pub(crate) ")
                .or_else(|| trimmed.strip_prefix("pub "))
                .unwrap_or(trimmed);
            let (field, rust_type) = declaration
                .split_once(':')
                .ok_or_else(|| format!("{struct_name}: cannot read field line {line:?}"))?;
            let optional_type = rust_type.trim_start().starts_with("Option<");
            let field = field.trim_start_matches("r#");
            let attributes = std::mem::take(&mut serde);
            let mut wire = if camel_case {
                let mut parts = field.split('_');
                let mut name = parts.next().unwrap().to_string();
                for part in parts {
                    let mut chars = part.chars();
                    if let Some(first) = chars.next() {
                        name.extend(first.to_uppercase());
                        name.push_str(chars.as_str());
                    }
                }
                name
            } else {
                field.to_string()
            };
            let mut omissible = false;
            let mut never = false;
            let mut field_default = false;
            for word in serde_words(&attributes) {
                let key = word.split('=').next().unwrap().trim();
                match key {
                    "default" => field_default = true,
                    "rename" => {
                        wire = word
                            .split('"')
                            .nth(1)
                            .ok_or_else(|| format!("{struct_name}.{field}: {word}"))?
                            .to_string();
                    }
                    "skip_serializing_if" => omissible = true,
                    "skip" | "skip_serializing" => never = true,
                    "alias" | "deserialize_with" | "with" | "serialize_with" => {}
                    _ => {
                        return Err(format!(
                            "{struct_name}.{field}: unmodelled serde attribute {word:?}"
                        ))
                    }
                }
            }
            let field = SerdeField {
                skip_serializing_if: omissible,
                absent_on_input: container_default || field_default || optional_type,
            };
            if !never && fields.insert(wire.clone(), field).is_some() {
                return Err(format!(
                    "{struct_name}: wire field {wire} is declared twice"
                ));
            }
        }
        Ok(fields)
    }

    /// The text after `{keyword} =` up to the end of that type declaration.
    fn type_alias<'a>(declarations: &'a str, keyword: &str) -> Result<&'a str, String> {
        let prefix = format!("{keyword} =");
        let (_, tail) = declarations
            .split_once(&prefix)
            .ok_or_else(|| format!("missing declaration {keyword}"))?;
        Ok(tail.split_once(";\n").map_or(tail, |(head, _)| head))
    }

    fn string_set(values: &serde_json::Value) -> BTreeSet<&str> {
        values
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect()
    }

    /// Every serialized DTO in the rendering contract has a declaration whose
    /// top-level fields are exactly the Serde wire fields: none missing and
    /// none extra. Fields the contract lists as possibly absent must be
    /// declared optional.
    fn check_dto_declarations(
        declarations: &str,
        contract: &serde_json::Value,
    ) -> Result<(), String> {
        let optional = contract["optionalDtoFields"].as_object().unwrap();
        for (name, fields) in contract["dtoFields"].as_object().unwrap() {
            // The complete serialized program and its version header are
            // not exported; callers receive the branded `Program` type.
            // Rendering admission still checks these runtime wire shapes.
            if matches!(name.as_str(), "CsilProgram" | "CsilHeader") {
                continue;
            }
            let public_name = if name == "InputDecl" {
                "InputDeclaration"
            } else {
                name.as_str()
            };
            let declared = interface_fields(declarations, public_name)?;
            let expected = string_set(fields);
            // `dtoFields` is Serde's Deserialize field list. Confirm it is
            // also what the serializer writes, then take each field's
            // omissibility from the serializer.
            let wire = serde_wire_fields(&WORKSPACE_SOURCES, rust_struct_name(name))?;
            let serialized: BTreeSet<&str> = wire.keys().map(String::as_str).collect();
            if serialized != expected {
                return Err(format!(
                    "{name}: Rust serializes {serialized:?}, but dtoFields lists {expected:?}"
                ));
            }
            let actual: BTreeSet<&str> = declared.keys().map(String::as_str).collect();
            let missing: Vec<_> = expected.difference(&actual).collect();
            let extra: Vec<_> = actual.difference(&expected).collect();
            if !missing.is_empty() || !extra.is_empty() {
                return Err(format!(
                    "stale {public_name} declaration: missing {missing:?}, not serialized {extra:?}"
                ));
            }
            // A field may be absent when the serializer can skip it, or
            // when the contract admits its absence from older packages.
            let mut absent: BTreeSet<&str> = wire
                .iter()
                .filter(|(_, omissible)| **omissible)
                .map(|(field, _)| field.as_str())
                .collect();
            if let Some(fields) = optional.get(name) {
                absent.extend(string_set(fields));
            }
            for (field, declared_optional) in &declared {
                let may_be_absent = absent.contains(field.as_str());
                if *declared_optional != may_be_absent {
                    return Err(if may_be_absent {
                        format!(
                            "{public_name}.{field} may be absent on the wire but is declared required"
                        )
                    } else {
                        format!(
                            "{public_name}.{field} is always serialized but is declared optional"
                        )
                    });
                }
            }
        }
        Ok(())
    }

    /// The `InputValue` tagged union names exactly the Serde variants, and each
    /// inline object body declares exactly the serialized fields. The
    /// compiler-authored form declares every Enum field the serializer always
    /// emits as required; the host override form may leave optional only the
    /// Enum fields Serde fills in when they are absent.
    fn check_input_value_declarations(
        declarations: &str,
        contract: &serde_json::Value,
    ) -> Result<(), String> {
        let tagged = contract["taggedFields"]["InputValue"].as_object().unwrap();
        let expected: BTreeSet<&str> = tagged.keys().map(String::as_str).collect();
        let tags: BTreeSet<&str> = type_alias(declarations, "type InputValueTag")?
            .split('"')
            .skip(1)
            .step_by(2)
            .collect();
        if tags != expected {
            return Err(format!(
                "stale InputValueTag: declared {tags:?}, serialized {expected:?}"
            ));
        }
        let union = type_alias(declarations, "type InputValueOf<IntValue, EnumValue>")?;
        // Payloads that are not an inline object: a type parameter, a scalar,
        // or a named declaration that has its own check (`ExternalSourceRef`
        // as a DTO, `InputSource` against Serde below).
        let named_payloads = [
            ("Int", "IntValue"),
            ("Float", "number"),
            ("Bool", "boolean"),
            ("String", "string"),
            ("Source", "InputSource"),
            ("ExternalSource", "ExternalSourceRef"),
            ("Enum", "EnumValue"),
        ];
        let mut arms = BTreeSet::new();
        for arm in union.split("TaggedInputValue<\"").skip(1) {
            let (tag, body) = arm
                .split_once("\", ")
                .ok_or_else(|| format!("malformed InputValueOf arm {arm:?}"))?;
            arms.insert(tag);
            let fields = string_set(
                tagged
                    .get(tag)
                    .ok_or_else(|| format!("InputValueOf declares unknown tag {tag}"))?,
            );
            if let Some(literal) = body.strip_prefix('{') {
                let declared = object_literal_fields(literal.split('}').next().unwrap())?;
                let names: BTreeSet<&str> = declared.keys().map(String::as_str).collect();
                if names != fields || declared.values().any(|optional| *optional) {
                    return Err(format!(
                        "stale InputValue {tag} body: declared {declared:?}, serialized {fields:?}"
                    ));
                }
            } else {
                let payload = body.split_once('>').map_or(body, |(payload, _)| payload);
                let expected = named_payloads
                    .iter()
                    .find(|(name, _)| *name == tag)
                    .map(|(_, payload)| *payload);
                if expected != Some(payload) {
                    return Err(format!(
                        "InputValue {tag} declares payload {payload:?}, expected {expected:?}"
                    ));
                }
            }
        }
        // `InputSource` names exactly the `Source` variants that are
        // `input.source` choices (the validator rejects any other default),
        // read from Serde: its error for an unknown variant lists them all.
        let declared_sources: BTreeSet<&str> = type_alias(declarations, "export type InputSource")?
            .split('"')
            .skip(1)
            .step_by(2)
            .collect();
        let error = serde_json::from_value::<pinewasm_compiler::InputValue>(
            serde_json::json!({ "Source": "NotASource" }),
        )
        .unwrap_err()
        .to_string();
        let accepted: BTreeSet<&str> = error
            .split_once("expected one of ")
            .ok_or_else(|| format!("cannot read the Source variants from {error:?}"))?
            .1
            .split('`')
            .skip(1)
            .step_by(2)
            .filter(|name| {
                serde_json::from_value::<pinewasm_compiler::BuiltinSeries>(serde_json::json!(name))
                    .is_ok_and(pinewasm_compiler::BuiltinSeries::is_input_source)
            })
            .collect();
        if declared_sources != accepted {
            return Err(format!(
                "stale InputSource: declared {declared_sources:?}, input sources {accepted:?}"
            ));
        }
        if arms != expected {
            return Err(format!(
                "stale InputValueOf arms: declared {arms:?}, serialized {expected:?}"
            ));
        }
        let enum_fields = string_set(&tagged["Enum"]);
        // The serializer emits every Enum field, including `title: null`.
        let emitted = serde_json::to_value(pinewasm_compiler::InputValue::Enum {
            name: "Fixture".into(),
            index: 0,
            member: "first".into(),
            title: None,
        })
        .unwrap();
        let emitted: BTreeSet<&str> = emitted["Enum"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        if emitted != enum_fields {
            return Err(format!(
                "Enum emits {emitted:?}, but the manifest lists {enum_fields:?}"
            ));
        }
        // Enum fields that Serde accepts as absent in a host override.
        let defaulted: BTreeSet<&str> = enum_fields
            .iter()
            .copied()
            .filter(|field| {
                let mut value = serde_json::json!({
                    "Enum": { "name": "Fixture", "index": 0, "member": "first", "title": null }
                });
                value["Enum"].as_object_mut().unwrap().remove(*field);
                serde_json::from_value::<pinewasm_compiler::InputValue>(value).is_ok()
            })
            .collect();
        // A host override may give `Int` as a number or a decimal string as
        // well as a bigint; the Wasm serializer writes every i64 as a bigint
        // (`serialize_large_number_types_as_bigints` in `to_js`).
        for override_int in [serde_json::json!(20), serde_json::json!("20")] {
            serde_json::from_value::<pinewasm_compiler::InputValue>(
                serde_json::json!({ "Int": override_int }),
            )
            .map_err(|error| format!("an Int override is no longer accepted: {error}"))?;
        }
        for (alias, compiler_authored, int_payload) in [
            ("DeclaredInputValue", true, "bigint"),
            ("InputValue", false, "number | bigint | string"),
        ] {
            let text = type_alias(declarations, &format!("export type {alias}"))?;
            let (int_declared, literal) = text
                .split_once("InputValueOf<")
                .and_then(|(_, tail)| tail.split_once(", {"))
                .and_then(|(int, tail)| tail.split_once("}>").map(|(literal, _)| (int, literal)))
                .ok_or_else(|| format!("{alias} is not InputValueOf<Int, {{ ... }}>"))?;
            if int_declared != int_payload {
                return Err(format!(
                    "{alias} declares Int as {int_declared:?}, expected {int_payload:?}"
                ));
            }
            let declared = object_literal_fields(literal)?;
            let names: BTreeSet<&str> = declared.keys().map(String::as_str).collect();
            if names != enum_fields {
                return Err(format!(
                    "stale {alias} Enum body: declared {names:?}, serialized {enum_fields:?}"
                ));
            }
            for (field, optional) in &declared {
                let must_be_optional = !compiler_authored && defaulted.contains(field.as_str());
                if *optional != must_be_optional {
                    return Err(format!(
                        "{alias} Enum.{field} is declared {} but must be {}",
                        if *optional { "optional" } else { "required" },
                        if must_be_optional {
                            "optional"
                        } else {
                            "required"
                        }
                    ));
                }
            }
        }
        Ok(())
    }

    /// `VersionInfo` declares exactly the serialized `version()` fields.
    fn check_version_info_declaration(declarations: &str) -> Result<(), String> {
        let emitted = serde_json::to_value(pinewasm_compiler::version_info()).unwrap();
        let mut expected: BTreeMap<String, bool> = emitted
            .as_object()
            .unwrap()
            .keys()
            .map(|field| (field.clone(), false))
            .collect();
        // Present only in release builds that embed a source revision.
        expected.insert("sourceRevision".into(), true);
        let declared = interface_fields(declarations, "VersionInfo")?;
        if declared != expected {
            return Err(format!(
                "stale VersionInfo declaration: declared {declared:?}, serialized {expected:?}"
            ));
        }
        Ok(())
    }

    #[test]
    fn input_value_declarations_match_serde_variants_and_enum_presence() {
        let contract = pinewasm_compiler::rendering_contract();
        if let Err(error) = check_input_value_declarations(*CONTRACT, &contract) {
            panic!("{error}");
        }
    }

    #[test]
    fn version_info_declares_the_rendering_contract_version() {
        if let Err(error) = check_version_info_declaration(*CONTRACT) {
            panic!("{error}");
        }
        let info = pinewasm_compiler::version_info();
        let contract = pinewasm_compiler::rendering_contract();
        assert_eq!(
            contract["schemaVersion"],
            serde_json::json!(info.rendering_contract_version)
        );
        let compiled = pinewasm_compiler::compile(
            "//@version=6\nindicator(\"Version\")\nplot(close)\n",
            pinewasm_compiler::CompileOptions::default(),
        );
        assert_eq!(
            compiled
                .program
                .unwrap()
                .metadata
                .rendering_contract_version,
            info.rendering_contract_version
        );
    }

    /// `renderingContract()` publishes the descriptor with JavaScript numbers,
    /// which fails on an integer outside the safe range. Every integer in it
    /// must stay a small schema constant.
    #[test]
    fn rendering_contract_integers_are_safe_javascript_numbers() {
        const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;
        fn walk(value: &serde_json::Value, path: &str, unsafe_paths: &mut Vec<String>) {
            match value {
                serde_json::Value::Number(number) => {
                    let safe = match (number.as_u64(), number.as_i64()) {
                        (Some(unsigned), _) => unsigned <= MAX_SAFE_INTEGER,
                        (None, Some(signed)) => signed.unsigned_abs() <= MAX_SAFE_INTEGER,
                        (None, None) => number.as_f64().is_some_and(f64::is_finite),
                    };
                    if !safe {
                        unsafe_paths.push(path.to_owned());
                    }
                }
                serde_json::Value::Array(items) => {
                    for (index, item) in items.iter().enumerate() {
                        walk(item, &format!("{path}[{index}]"), unsafe_paths);
                    }
                }
                serde_json::Value::Object(fields) => {
                    for (key, item) in fields {
                        walk(item, &format!("{path}.{key}"), unsafe_paths);
                    }
                }
                _ => {}
            }
        }
        let mut unsafe_paths = Vec::new();
        walk(
            &pinewasm_compiler::rendering_contract(),
            "renderingContract()",
            &mut unsafe_paths,
        );
        assert!(unsafe_paths.is_empty(), "{unsafe_paths:?}");
    }

    /// The checks must reject a declaration that drifts from Serde in either
    /// direction, not only accept the current text.
    #[test]
    fn declaration_checks_reject_dropped_and_extra_fields() {
        let contract = pinewasm_compiler::rendering_contract();
        let replace_once = |from: &str, to: &str| {
            assert_eq!(CONTRACT.matches(from).count(), 1, "{from:?}");
            CONTRACT.replacen(from, to, 1)
        };
        // A serialized DTO field dropped from its declaration.
        let dropped = replace_once("  readonly plotLabel: string;\n", "");
        assert!(check_dto_declarations(&dropped, &contract).is_err());
        // A declared DTO field that Serde never emits.
        let extra = replace_once(
            "export interface ExternalSourceRef {\n",
            "export interface ExternalSourceRef {\n  readonly notSerialized: number;\n",
        );
        assert!(check_dto_declarations(&extra, &contract).is_err());
        // A field the wire may omit, declared required.
        let required = replace_once(
            "  readonly timings?: PhaseTimings;",
            "  readonly timings: PhaseTimings;",
        );
        assert!(check_dto_declarations(&required, &contract).is_err());
        // The compiler-authored Enum body loosened back to optional fields.
        let loosened = replace_once(
            "readonly member: string; readonly title: string | null }>;",
            "readonly member?: string; readonly title?: string | null }>;",
        );
        assert!(check_input_value_declarations(&loosened, &contract).is_err());
        // A tag missing from the tagged union.
        let untagged = replace_once("  | \"ExternalSource\"\n", "");
        assert!(check_input_value_declarations(&untagged, &contract).is_err());
        // A Color body that drops a channel.
        let colorless = replace_once(
            "<\"Color\", { readonly r: number; readonly g: number; readonly b: number; readonly t: number }>",
            "<\"Color\", { readonly r: number; readonly g: number; readonly b: number }>",
        );
        assert!(check_input_value_declarations(&colorless, &contract).is_err());
        // VersionInfo without the rendering contract version.
        let versionless = replace_once("  readonly renderingContractVersion: number;\n", "");
        assert!(check_version_info_declaration(&versionless).is_err());
        // A field Serde may skip, declared required (`color_roles` is
        // skipped when empty).
        let roles_required = replace_once(
            "export interface CsilPlot {\n  readonly title: string | null;\n  readonly style: DisplayStyle;\n  /** Omitted when the plot declares no color argument. */\n  readonly color_roles?:",
            "export interface CsilPlot {\n  readonly title: string | null;\n  readonly style: DisplayStyle;\n  /** Omitted when the plot declares no color argument. */\n  readonly color_roles:",
        );
        let error = check_dto_declarations(&roles_required, &contract).unwrap_err();
        assert!(
            error.contains("CsilPlot.color_roles may be absent"),
            "{error}"
        );
        // A field Serde always writes, declared optional.
        let always_optional = replace_once(
            "  readonly plotSlot: number;\n",
            "  readonly plotSlot?: number;\n",
        );
        let error = check_dto_declarations(&always_optional, &contract).unwrap_err();
        assert!(error.contains("always serialized"), "{error}");
        // A property declared without `readonly` cannot slip past the check.
        let bare = replace_once(
            "export interface ExternalSourceRef {\n",
            "export interface ExternalSourceRef {\n  notSerialized: number;\n",
        );
        assert!(check_dto_declarations(&bare, &contract).is_err());
        let bare_literal = replace_once(
            "{ readonly r: number; readonly g: number; readonly b: number; readonly t: number }",
            "{ readonly r: number; readonly g: number; readonly b: number; readonly t: number; a: number }",
        );
        assert!(check_input_value_declarations(&bare_literal, &contract).is_err());
        // A named payload swapped for another type.
        let external_as_source = replace_once(
            "TaggedInputValue<\"ExternalSource\", ExternalSourceRef>",
            "TaggedInputValue<\"ExternalSource\", InputSource>",
        );
        assert!(check_input_value_declarations(&external_as_source, &contract).is_err());
        // An InputSource member Serde accepts, dropped.
        let sourceless = replace_once("  | \"Hlcc4\";\n", ";\n");
        assert!(check_input_value_declarations(&sourceless, &contract).is_err());
        // Compiler metadata widened back to a number Int it never emits.
        let numeric_int = replace_once(
            "export type DeclaredInputValue = InputValueOf<bigint,",
            "export type DeclaredInputValue = InputValueOf<number | bigint | string,",
        );
        assert!(check_input_value_declarations(&numeric_int, &contract).is_err());
    }

    /// The Rust-source reader takes omissibility from `skip_serializing_if`,
    /// leaves out fields Serde never writes, applies renames, and refuses
    /// what it does not model.
    #[test]
    fn serde_wire_fields_reads_skip_and_rename_attributes() {
        let source = |text: &str| vec![(std::path::PathBuf::from("fixture.rs"), text.to_string())];
        let fixture = source(concat!(
            "\n/// A fixture.\n",
            "#[derive(Debug, Serialize, Deserialize)]\n",
            "#[serde(rename_all = \"camelCase\")]\n",
            "pub struct Fixture {\n",
            "    /// Always written.\n",
            "    pub plot_slot: u32,\n",
            "    #[serde(default, skip_serializing_if = \"Vec::is_empty\")]\n",
            "    pub color_roles: Vec<u8>,\n",
            "    #[serde(\n",
            "        default,\n",
            "        skip_serializing_if = \"Option::is_none\",\n",
            "        rename = \"lineRefs\"\n",
            "    )]\n",
            "    pub line_refs: Option<[Option<u32>; 2]>,\n",
            "    #[serde(skip_serializing)]\n",
            "    pub retained: u32,\n",
            "    #[serde(skip)]\n",
            "    pub(crate) cache: u32,\n",
            "    #[cfg_attr(feature = \"serde\", serde(default, skip_serializing_if = \"Option::is_none\"))]\n",
            "    pub exit_id: Option<String>,\n",
            "    #[cfg_attr(\n",
            "        feature = \"serde\",\n",
            "        serde(default, skip_serializing_if = \"Option::is_none\")\n",
            "    )]\n",
            "    pub lower_fence: Option<f64>,\n",
            "    #[cfg_attr(feature = \"serde\", serde(skip))]\n",
            "    pub(crate) ordinal: u32,\n",
            "}\n",
        ));
        let fields = serde_wire_fields(&fixture, "Fixture").unwrap();
        assert_eq!(
            fields,
            BTreeMap::from([
                ("plotSlot".to_string(), false),
                ("colorRoles".to_string(), true),
                ("lineRefs".to_string(), true),
                ("exitId".to_string(), true),
                ("lowerFence".to_string(), true),
            ])
        );
        let flattened = source(concat!(
            "\n#[derive(Serialize, Deserialize)]\n",
            "pub struct Flat {\n",
            "    #[serde(flatten)]\n",
            "    pub inner: Inner,\n",
            "}\n",
        ));
        assert!(serde_wire_fields(&flattened, "Flat").is_err());
        let hand_written = source(concat!(
            "\n#[derive(Debug)]\n",
            "pub struct Manual {\n",
            "    pub a: u32,\n",
            "}\n",
            "impl Serialize for Manual {}\n",
        ));
        assert!(serde_wire_fields(&hand_written, "Manual").is_err());
        assert!(serde_wire_fields(&fixture, "Missing").is_err());
    }

    /// Serde structs a host passes into the package (`Deserialize`) whose
    /// declarations the rendering descriptor does not list, as
    /// `(declaration, Rust struct)`. A field is optional in the declaration
    /// exactly when deserialization accepts an object without it.
    const HOST_INPUT_DTOS: &[(&str, &str)] = &[
        ("MarketData", "pinewasm_vm::MarketData"),
        ("SymbolInfo", "SymInfo"),
        ("RequestTimeframeBars", "RequestTimeframeBars"),
        ("HostFootprint", "HostFootprint"),
        ("HostVolumeRow", "HostVolumeRow"),
        ("UpcomingEvents", "UpcomingEvents"),
        ("Tick", "Tick"),
        ("ExternalSourceLane", "ExternalSourceLane"),
        ("RunOptions", "RunOptions"),
        ("StrategyRunOverrides", "StrategyRunOverrides"),
        ("StandaloneWasmOptions", "StandaloneWasmOptionsInput"),
        ("SearchFile", "SearchFile"),
        ("SearchOptions", "SearchOptions"),
    ];

    /// Serde structs the package returns (`Serialize`) whose declarations
    /// the rendering descriptor does not list. A field is optional exactly
    /// when `skip_serializing_if` can leave it out.
    const PACKAGE_OUTPUT_DTOS: &[(&str, &str)] = &[
        ("NativeDisplayStyles", "NativeDisplayStyles"),
        ("VersionInfo", "VersionInfo"),
        ("OutlineNode", "OutlineTreeNode"),
        ("SearchMatch", "SearchMatch"),
        ("FileMatches", "FileMatches"),
        ("SearchResults", "SearchResults"),
    ];

    /// Host inputs whose every field the declaration requires although Serde
    /// would default it, because validation then refuses the defaulted value.
    /// Each entry names the refusal.
    const REQUIRED_BY_VALIDATION: &[(&str, &str)] = &[(
        "RequestTimeframeBars",
        "MarketData::validate refuses a requestBars feed unless all seven arrays share one nonzero length, so a defaulted (empty) array is never accepted.",
    )];

    /// Declared interfaces with no derived Serde struct behind them, each
    /// with where its fields are held to the package. Every other interface
    /// is a descriptor DTO or listed above, so a new interface fails
    /// `every_declared_interface_is_held_to_its_rust_source` until it is
    /// classified.
    const NON_SERDE_INTERFACES: &[(&str, &str)] = &[
        ("CompileOptionFlags", "Read field by field from the options object by the compile entry points; not a Serde struct."),
        ("Program", "Branded handle over CsilProgram; the descriptor lists CsilProgram's fields."),
        ("ResidentCompileResult", "CompileResult with `program` replaced by the CompiledProgram class handle; CompileResult is a descriptor DTO."),
        ("RenderingContract", "The descriptor itself; rendering_contract_declares_every_generated_top_level_field holds its keys."),
        ("DocumentSymbol", "Built by hand in the documentSymbols binding from the language service's symbols."),
        ("CsawModule", "Built by hand in aot_result_to_js; chart_module_declarations_match_the_objects_the_binding_builds holds its keys."),
        ("UnsupportedCsawModule", "Built by hand in aot_result_to_js; chart_module_declarations_match_the_objects_the_binding_builds holds its keys."),
        ("NativeStyleSlot", "Built by hand in aot_result_to_js; chart_module_declarations_match_the_objects_the_binding_builds holds its keys."),
        ("StandaloneWasmRefusal", "Built by hand in the compileStandaloneWasmProgram binding from the AOT refusal."),
        ("StandaloneWasmModule", "Built by hand in the compileStandaloneWasmProgram binding."),
        ("RefusedStandaloneWasmModule", "Built by hand in the compileStandaloneWasmProgram binding."),
    ];

    /// Each listed interface declares exactly its Rust struct's Serde wire
    /// fields, with optionality from the direction the value crosses.
    fn check_boundary_dto_declarations(declarations: &str) -> Result<(), String> {
        let mut errors = Vec::new();
        for (direction, list) in [
            ("Deserialize", HOST_INPUT_DTOS),
            ("Serialize", PACKAGE_OUTPUT_DTOS),
        ] {
            for (public_name, rust_name) in list {
                let declared = interface_fields(declarations, public_name)?;
                let wire = serde_fields(&WORKSPACE_SOURCES, rust_name, direction)?;
                let expected: BTreeSet<&str> = wire.keys().map(String::as_str).collect();
                let actual: BTreeSet<&str> = declared.keys().map(String::as_str).collect();
                let missing: Vec<_> = expected.difference(&actual).collect();
                let extra: Vec<_> = actual.difference(&expected).collect();
                if !missing.is_empty() || !extra.is_empty() {
                    errors.push(format!(
                        "stale {public_name} declaration ({rust_name}): missing {missing:?}, not in Rust {extra:?}"
                    ));
                    continue;
                }
                for (field, declared_optional) in &declared {
                    let serde = wire[field];
                    let may_be_absent = if direction == "Serialize" {
                        serde.skip_serializing_if
                    } else {
                        serde.absent_on_input
                    };
                    let required_by_validation = direction == "Deserialize"
                        && !*declared_optional
                        && REQUIRED_BY_VALIDATION
                            .iter()
                            .any(|(name, _)| name == public_name);
                    if *declared_optional != may_be_absent && !required_by_validation {
                        errors.push(format!(
                            "{public_name}.{field}: declared {}, but {rust_name} {}",
                            if *declared_optional {
                                "optional"
                            } else {
                                "required"
                            },
                            match (direction, may_be_absent) {
                                ("Serialize", true) => "may skip it",
                                ("Serialize", false) => "always serializes it",
                                (_, true) => "accepts an object without it",
                                (_, false) => "requires it",
                            }
                        ));
                    }
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("\n"))
        }
    }

    #[test]
    fn boundary_declarations_match_serde_fields_and_optionality() {
        if let Err(error) = check_boundary_dto_declarations(*CONTRACT) {
            panic!("{error}");
        }
    }

    /// Every `interface` in the contract is a descriptor DTO, a boundary DTO
    /// checked above, or a reviewed non-Serde shape. Nothing is unclassified,
    /// and nothing is classified twice.
    #[test]
    fn every_declared_interface_is_held_to_its_rust_source() {
        let contract = pinewasm_compiler::rendering_contract();
        let descriptor: BTreeSet<String> = contract["dtoFields"]
            .as_object()
            .unwrap()
            .keys()
            .map(|name| {
                if name == "InputDecl" {
                    "InputDeclaration".to_string()
                } else {
                    name.clone()
                }
            })
            .collect();
        let mut classified: BTreeMap<&str, &str> = BTreeMap::new();
        for (name, class) in HOST_INPUT_DTOS
            .iter()
            .map(|(name, _)| (*name, "host input"))
            .chain(
                PACKAGE_OUTPUT_DTOS
                    .iter()
                    .map(|(name, _)| (*name, "package output")),
            )
            .chain(
                NON_SERDE_INTERFACES
                    .iter()
                    .map(|(name, _)| (*name, "non-Serde")),
            )
        {
            assert!(
                !descriptor.contains(name),
                "{name} is a descriptor DTO and also {class}"
            );
            assert!(
                classified.insert(name, class).is_none(),
                "{name} is classified twice"
            );
        }
        let mut declared = BTreeSet::new();
        for line in CONTRACT.lines() {
            let rest = line.strip_prefix("export ").unwrap_or(line);
            if let Some(rest) = rest.strip_prefix("interface ") {
                let name = rest.split([' ', '<', '{']).next().unwrap();
                declared.insert(name.to_string());
            }
        }
        let unclassified: Vec<_> = declared
            .iter()
            .filter(|name| !descriptor.contains(*name) && !classified.contains_key(name.as_str()))
            .collect();
        assert!(
            unclassified.is_empty(),
            "unclassified interfaces: {unclassified:?}"
        );
        let stale: Vec<_> = classified
            .keys()
            .filter(|name| !declared.contains(**name))
            .collect();
        assert!(stale.is_empty(), "classified but not declared: {stale:?}");
        for name in &descriptor {
            if !matches!(name.as_str(), "CsilProgram" | "CsilHeader") {
                assert!(
                    declared.contains(name),
                    "descriptor DTO {name} has no interface"
                );
            }
        }
    }

    /// The keys `aot_result_to_js` sets are exactly the fields of the
    /// declarations it promises: `CsawModule` on success,
    /// `UnsupportedCsawModule` on refusal, and `NativeStyleSlot` per style
    /// slot.
    #[test]
    fn chart_module_declarations_match_the_objects_the_binding_builds() {
        let source = include_str!("lib.rs");
        let (_, tail) = source.split_once("\nfn aot_result_to_js(").unwrap();
        let body = tail.split("\n}\n").next().unwrap();
        let (ok_arm, err_arm) = body.split_once("Err(reason) =>").unwrap();
        let (module_arm, slot_arm) = ok_arm
            .split_once("for slot in &layout.style_slots")
            .unwrap();
        let quoted_after = |text: &str, marker: &str| -> BTreeSet<String> {
            text.split(marker)
                .skip(1)
                .filter_map(|rest| rest.trim_start().strip_prefix('"'))
                .map(|rest| rest.split('"').next().unwrap().to_string())
                .collect()
        };
        let mut module_keys = quoted_after(module_arm, "set(");
        // `styleSlots` is set after the slot loop.
        module_keys.extend(quoted_after(slot_arm, "set("));
        let slot_keys: BTreeSet<String> = slot_arm
            .split_once("for (key, value) in [")
            .unwrap()
            .1
            .split(']')
            .next()
            .unwrap()
            .split("(\"")
            .skip(1)
            .map(|rest| rest.split('"').next().unwrap().to_string())
            .collect();
        let unsupported_keys = quoted_after(err_arm, "set(");
        for (name, keys) in [
            ("CsawModule", module_keys),
            ("NativeStyleSlot", slot_keys),
            ("UnsupportedCsawModule", unsupported_keys),
        ] {
            let declared = interface_fields(*CONTRACT, name).unwrap();
            let declared: BTreeSet<String> = declared.keys().cloned().collect();
            assert_eq!(
                declared, keys,
                "{name} declaration differs from aot_result_to_js"
            );
        }
    }

    /// The boundary check fails when a declaration drifts from its struct.
    #[test]
    fn boundary_declaration_check_rejects_drift() {
        let replace_once = |from: &str, to: &str| {
            assert_eq!(CONTRACT.matches(from).count(), 1, "{from:?}");
            CONTRACT.replacen(from, to, 1)
        };
        // A required host input declared optional.
        let loose = replace_once(
            "  readonly tradingDay?: IntegerInput | null;\n  readonly isBarClose: boolean;\n",
            "  readonly tradingDay?: IntegerInput | null;\n  readonly isBarClose?: boolean;\n",
        );
        let error = check_boundary_dto_declarations(&loose).unwrap_err();
        assert!(error.contains("Tick.isBarClose"), "{error}");
        // A defaulted host input declared required.
        let strict = replace_once(
            "  readonly tradingDay?: IntegerInput | null;\n  readonly isBarClose",
            "  readonly tradingDay: IntegerInput | null;\n  readonly isBarClose",
        );
        let error = check_boundary_dto_declarations(&strict).unwrap_err();
        assert!(error.contains("Tick.tradingDay"), "{error}");
        // A field the struct does not have.
        let extra = replace_once(
            "export interface ExternalSourceLane {\n",
            "export interface ExternalSourceLane {\n  readonly stale: number;\n",
        );
        let error = check_boundary_dto_declarations(&extra).unwrap_err();
        assert!(error.contains("ExternalSourceLane"), "{error}");
    }

    /// The alternatives of a union alias: each `"literal"` or `{ ... }`
    /// object type at the top level, in order. `None` when an alternative is
    /// anything else (a named type, a scalar), so the alias is not a variant
    /// union.
    fn union_alternatives(body: &str) -> Option<Vec<&str>> {
        let mut alternatives = Vec::new();
        let (mut depth, mut start) = (0usize, 0usize);
        let mut in_string = false;
        for (index, ch) in body.char_indices() {
            match ch {
                '"' => in_string = !in_string,
                '{' if !in_string => depth += 1,
                '}' if !in_string => depth -= 1,
                '|' if !in_string && depth == 0 => {
                    alternatives.push(body[start..index].trim());
                    start = index + 1;
                }
                _ => {}
            }
        }
        alternatives.push(body[start..].trim());
        alternatives.retain(|alternative| !alternative.is_empty());
        let variant = |alternative: &&str| {
            let quoted = alternative.len() >= 2
                && alternative.starts_with('"')
                && alternative.ends_with('"')
                && alternative[1..alternative.len() - 1].find('"').is_none();
            quoted || (alternative.starts_with('{') && alternative.ends_with('}'))
        };
        (!alternatives.is_empty() && alternatives.iter().all(variant)).then_some(alternatives)
    }

    /// Exported aliases whose every alternative is a string literal or an
    /// object type, mapped to their alternatives.
    fn variant_unions(declarations: &str) -> BTreeMap<String, Vec<String>> {
        let mut unions = BTreeMap::new();
        for (at, _) in declarations.match_indices("\nexport type ") {
            let tail = &declarations[at + "\nexport type ".len()..];
            let Some((name, rest)) = tail.split_once(" =") else {
                continue;
            };
            if name.contains(['<', ' ']) {
                continue;
            }
            let body = rest.split_once(";\n").map_or(rest, |(head, _)| head);
            if let Some(alternatives) = union_alternatives(body) {
                unions.insert(
                    name.to_string(),
                    alternatives.into_iter().map(str::to_string).collect(),
                );
            }
        }
        unions
    }

    fn literal_set(alternatives: &[String]) -> Result<BTreeSet<&str>, String> {
        alternatives
            .iter()
            .map(|alternative| {
                alternative
                    .strip_prefix('"')
                    .and_then(|rest| rest.strip_suffix('"'))
                    .ok_or_else(|| format!("{alternative:?} is not a string literal"))
            })
            .collect()
    }

    /// Each object alternative of a tagged union, as its tag and the
    /// declared fields of its payload. `internal` names the tag property
    /// (`kind`); otherwise the union is externally tagged:
    /// `{ readonly Tag: { ...payload } }`.
    fn tagged_alternatives(
        alternatives: &[String],
        internal: Option<&str>,
    ) -> Result<BTreeMap<String, BTreeSet<String>>, String> {
        let mut variants = BTreeMap::new();
        for alternative in alternatives {
            let inner = alternative
                .strip_prefix('{')
                .and_then(|rest| rest.strip_suffix('}'))
                .ok_or_else(|| format!("{alternative:?} is not an object type"))?;
            let (tag, fields) = match internal {
                Some(tag_field) => {
                    let declared = object_literal_fields(inner)?;
                    let marker = format!("readonly {tag_field}: \"");
                    let tag = inner
                        .split_once(&marker)
                        .and_then(|(_, rest)| rest.split_once('"'))
                        .map(|(tag, _)| tag.to_string())
                        .ok_or_else(|| format!("{alternative:?} has no literal {tag_field}"))?;
                    if declared.values().any(|optional| *optional) {
                        return Err(format!("{alternative:?} declares an optional field"));
                    }
                    let fields = declared
                        .into_keys()
                        .filter(|field| field != tag_field)
                        .collect();
                    (tag, fields)
                }
                None => {
                    let (head, payload) = inner
                        .trim()
                        .strip_prefix("readonly ")
                        .and_then(|rest| rest.split_once(": {"))
                        .ok_or_else(|| {
                            format!("{alternative:?} is not {{ readonly Tag: {{ ... }} }}")
                        })?;
                    let payload = payload
                        .trim_end()
                        .strip_suffix('}')
                        .ok_or_else(|| format!("{alternative:?} has an unclosed payload"))?;
                    let declared = object_literal_fields(payload)?;
                    if declared.values().any(|optional| *optional) {
                        return Err(format!("{alternative:?} declares an optional field"));
                    }
                    (head.to_string(), declared.into_keys().collect())
                }
            };
            if variants.insert(tag.clone(), fields).is_some() {
                return Err(format!("variant {tag} is declared twice"));
            }
        }
        Ok(variants)
    }

    /// Tagged unions from the descriptor's `taggedFields` other than
    /// `InputValue` (held by `check_input_value_declarations`): the
    /// declaration names exactly the serialized variants, and each payload
    /// declares exactly the serialized fields.
    const TAGGED_UNIONS: &[(&str, &str, Option<&str>)] = &[
        ("InputActiveExpression", "InputActiveExpr", Some("kind")),
        ("TaInputTarget", "TaInputTarget", None),
    ];

    /// String unions that are not Serde enums: the strings are written by
    /// hand in Rust, so the check reads them from that code.
    const HAND_WRITTEN_STRING_UNIONS: &[(&str, &str)] = &[
        ("OutlineKind", "The `kind` literals pinewasm_lsp::outline passes to `node` and returns for output calls."),
        ("DocumentSymbolKind", "LocalSymbolKind::label for every variant; the documentSymbols binding writes that label."),
        ("StandaloneWasmRefusalCode", "The codes classify_standalone_wasm_refusal returns."),
        ("StandaloneWasmRefusalClass", "standalone_wasm_refusal_class for every pinewasm_aot::RefusalClass."),
    ];

    /// Object unions built by hand in the binding.
    const HAND_WRITTEN_OBJECT_UNIONS: &[(&str, &str)] = &[(
        "StandaloneWasmRefusalSite",
        "StandaloneWasmRefusalSite::wire for every variant: one property each.",
    )];

    fn hand_written_string_union(name: &str) -> BTreeSet<&'static str> {
        match name {
            "OutlineKind" => {
                let source = include_str!("../../pinewasm_lsp/src/outline.rs");
                let source = source.split("#[cfg(test)]").next().unwrap();
                let mut kinds = BTreeSet::new();
                // `node(label, "kind", start)`: the second argument.
                for (at, _) in source.match_indices("node(") {
                    let call = &source[at + "node(".len()..];
                    let call = call.split_once(')').map_or(call, |(call, _)| call);
                    if let Some(kind) = call
                        .split(',')
                        .nth(1)
                        .map(str::trim)
                        .and_then(|arg| arg.strip_prefix('"'))
                        .and_then(|arg| arg.strip_suffix('"'))
                    {
                        kinds.insert(kind);
                    }
                }
                // `Some(("group", "kind"))` for plot, alert and strategy calls.
                for (at, _) in source.match_indices("Some((\"") {
                    let rest = &source[at + "Some((\"".len()..];
                    if let Some((kind, _)) = rest
                        .split_once("\", \"")
                        .and_then(|(_, tail)| tail.split_once('"'))
                    {
                        kinds.insert(kind);
                    }
                }
                kinds
            }
            "DocumentSymbolKind" => {
                use pinewasm_lsp::analysis::LocalSymbolKind as Kind;
                // Exhaustive on purpose: a new kind must be listed here.
                let every = |kind: Kind| match kind {
                    Kind::Variable
                    | Kind::Function
                    | Kind::Parameter
                    | Kind::Type
                    | Kind::Field
                    | Kind::EnumMember
                    | Kind::Method
                    | Kind::Module => kind.label(),
                };
                [
                    Kind::Variable,
                    Kind::Function,
                    Kind::Parameter,
                    Kind::Type,
                    Kind::Field,
                    Kind::EnumMember,
                    Kind::Method,
                    Kind::Module,
                ]
                .into_iter()
                .map(every)
                .collect()
            }
            "StandaloneWasmRefusalCode" => {
                let source = include_str!("lib.rs");
                let (_, tail) = source
                    .split_once("\nfn classify_standalone_wasm_refusal(")
                    .unwrap();
                let body = tail.split("\n}\n").next().unwrap();
                body.split('"')
                    .skip(1)
                    .step_by(2)
                    .filter(|literal| literal.starts_with("StandaloneWasm"))
                    .collect()
            }
            "StandaloneWasmRefusalClass" => {
                use pinewasm_aot::RefusalClass;
                let every = |class: RefusalClass| match class {
                    RefusalClass::Capability | RefusalClass::Internal => {
                        crate::standalone_wasm_refusal_class(class)
                    }
                };
                [RefusalClass::Capability, RefusalClass::Internal]
                    .into_iter()
                    .map(every)
                    .collect()
            }
            other => panic!("no source for {other}"),
        }
    }

    fn check_variant_unions(declarations: &str) -> Result<(), String> {
        let contract = pinewasm_compiler::rendering_contract();
        let enums = contract["enums"].as_object().unwrap();
        let unions = variant_unions(declarations);
        let mut errors = Vec::new();
        let mut classified = BTreeSet::new();
        for (public_name, manifest_name, internal) in TAGGED_UNIONS {
            classified.insert(*public_name);
            let Some(alternatives) = unions.get(*public_name) else {
                errors.push(format!("{public_name} is not declared as a tagged union"));
                continue;
            };
            let declared = tagged_alternatives(alternatives, *internal)?;
            let serialized: BTreeMap<String, BTreeSet<String>> = contract["taggedFields"]
                [*manifest_name]
                .as_object()
                .ok_or_else(|| format!("taggedFields has no {manifest_name}"))?
                .iter()
                .map(|(tag, fields)| {
                    (
                        tag.clone(),
                        string_set(fields).into_iter().map(str::to_string).collect(),
                    )
                })
                .collect();
            if declared != serialized {
                errors.push(format!(
                    "stale {public_name}: declared {declared:?}, serialized {serialized:?}"
                ));
            }
        }
        for (name, _) in HAND_WRITTEN_STRING_UNIONS {
            classified.insert(*name);
            let Some(alternatives) = unions.get(*name) else {
                errors.push(format!("{name} is not declared as a string union"));
                continue;
            };
            let declared = literal_set(alternatives)?;
            let written: BTreeSet<&str> = hand_written_string_union(name);
            if declared != written {
                errors.push(format!(
                    "stale {name}: declared {declared:?}, Rust writes {written:?}"
                ));
            }
        }
        for (name, _) in HAND_WRITTEN_OBJECT_UNIONS {
            classified.insert(*name);
            let alternatives = unions
                .get(*name)
                .ok_or_else(|| format!("{name} is not declared as an object union"))?;
            let mut declared = BTreeSet::new();
            for alternative in alternatives {
                let inner = &alternative[1..alternative.len() - 1];
                let fields = object_literal_fields(inner)?;
                if fields.len() != 1 || fields.values().any(|optional| *optional) {
                    errors.push(format!(
                        "{name}: {alternative:?} must have one required property"
                    ));
                }
                declared.extend(fields.into_keys());
            }
            let site = |site: crate::StandaloneWasmRefusalSite| match site {
                crate::StandaloneWasmRefusalSite::Pc(_)
                | crate::StandaloneWasmRefusalSite::Input(_)
                | crate::StandaloneWasmRefusalSite::Plot(_) => site.wire().0.to_string(),
            };
            let written: BTreeSet<String> = [
                crate::StandaloneWasmRefusalSite::Pc(0),
                crate::StandaloneWasmRefusalSite::Input(0),
                crate::StandaloneWasmRefusalSite::Plot(0),
            ]
            .into_iter()
            .map(site)
            .collect();
            if declared != written {
                errors.push(format!(
                    "stale {name}: declared {declared:?}, Rust writes {written:?}"
                ));
            }
        }
        // `InputValueTag`, `InputSource` and the `InputValueOf` arms are
        // held by check_input_value_declarations; the descriptor's `enums`
        // by rendering_declarations_match_serde_domains_and_fields.
        classified.insert("InputSource");
        let unclassified: Vec<_> = unions
            .keys()
            .filter(|name| !enums.contains_key(*name) && !classified.contains(name.as_str()))
            .collect();
        if !unclassified.is_empty() {
            errors.push(format!(
                "union declarations with no Rust source check: {unclassified:?}"
            ));
        }
        let missing: Vec<_> = enums
            .keys()
            .map(String::as_str)
            .chain(classified.iter().copied())
            .filter(|name| !unions.contains_key(*name))
            .collect();
        if !missing.is_empty() {
            errors.push(format!("checked unions that are not declared: {missing:?}"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("\n"))
        }
    }

    /// Every exported union of string literals or object types is held to
    /// the Rust that writes it: a descriptor enum, a descriptor tagged
    /// union, or a reviewed hand-written producer read from its source.
    #[test]
    fn every_declared_union_is_held_to_its_rust_variants() {
        if let Err(error) = check_variant_unions(*CONTRACT) {
            panic!("{error}");
        }
    }

    /// The union check fails when a declaration drifts from Rust.
    #[test]
    fn union_check_rejects_drift() {
        let replace_once = |from: &str, to: &str| {
            assert_eq!(CONTRACT.matches(from).count(), 1, "{from:?}");
            CONTRACT.replacen(from, to, 1)
        };
        // A tagged variant the serializer never writes.
        let extra = replace_once(
            "  | { readonly kind: \"na\" }\n",
            "  | { readonly kind: \"na\" }\n  | { readonly kind: \"stale\" }\n",
        );
        let error = check_variant_unions(&extra).unwrap_err();
        assert!(error.contains("stale InputActiveExpression"), "{error}");
        // A dropped payload field.
        let dropped = replace_once(
            "{ readonly State: { readonly state: number; readonly param: number } }",
            "{ readonly State: { readonly state: number } }",
        );
        let error = check_variant_unions(&dropped).unwrap_err();
        assert!(error.contains("stale TaInputTarget"), "{error}");
        // A hand-written string the producer does not write.
        let kind = replace_once(
            "  | \"library alias\";",
            "  | \"library alias\"\n  | \"module\";",
        );
        let error = check_variant_unions(&kind).unwrap_err();
        assert!(error.contains("stale DocumentSymbolKind"), "{error}");
        let outline = replace_once(
            "  | \"strategy\";\n\nexport interface OutlineNode",
            "  | \"strategy\"\n  | \"study\";\n\nexport interface OutlineNode",
        );
        let error = check_variant_unions(&outline).unwrap_err();
        assert!(error.contains("stale OutlineKind"), "{error}");
        // A new union nobody holds to Rust.
        let new = replace_once(
            "export type LogLevel =",
            "export type Unchecked = \"a\" | \"b\";\nexport type LogLevel =",
        );
        let error = check_variant_unions(&new).unwrap_err();
        assert!(error.contains("Unchecked"), "{error}");
    }
}
