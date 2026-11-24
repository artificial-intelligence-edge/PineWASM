//! MIR: mid-level IR with explicit control flow and explicit effects.
//!
//! MIR makes execution order explicit. Expression trees become sequences of
//! instructions writing virtual registers ([`ValueId`]); `if` becomes basic
//! blocks joined by [`Terminator`]s; series reads/writes and plot effects
//! are dedicated instructions rather than function calls. MIR is the direct
//! source of the stack CSIL, and today that translation is essentially 1:1 —
//! no pass folds, deduplicates, or eliminates anything — so MIR's present
//! value is structural: the explicit CFG, the explicit effects, and the
//! shared [`Value`] model. Optimization passes are future work; this is the
//! layer they would slot into.
#![forbid(unsafe_code)]

/// Aeneas extraction roots for the heap glue of the
/// varip rollback. Built only with `--features aeneas`.
#[cfg(feature = "aeneas")]
pub mod aeneas_roots_heap_glue;
/// Extraction roots for the Aeneas `mir-alloc` set. Built only with `--features aeneas`.
#[cfg(feature = "aeneas")]
#[allow(dead_code)] // Extraction roots for Charon/Aeneas; never called from Rust.
mod aeneas_roots_mir_alloc;
/// Extraction roots for the Aeneas `mir-lower` sets. Built only with `--features aeneas`.
#[cfg(feature = "aeneas")]
#[allow(dead_code)] // Extraction roots for Charon/Aeneas; never called from Rust.
mod aeneas_roots_mir_lower;
/// Aeneas extraction roots for the series-slot table.
/// Built only with `--features aeneas`.
#[cfg(feature = "aeneas")]
#[allow(dead_code)] // Extraction roots for Charon/Aeneas; never called from Rust.
mod aeneas_roots_series_slots;
/// Extraction roots for the Aeneas `vm-snapshot` set (varip rollback).
/// Built only with `--features aeneas`.
#[cfg(feature = "aeneas")]
pub mod aeneas_roots_vm_snapshot_fix;
/// Extraction roots for the opt-in derived-Clone extraction.
#[cfg(feature = "aeneas-color-clone")]
pub mod color_clone_current_root;
pub mod heap;
pub mod heap_rollback;
mod lower;
mod slot_table;
pub mod value;

pub use heap::{
    ElemType, Heap, HeapLimitError, HeapLimitKind, HeapObj, HeapRestoreError,
    DEFAULT_MAX_COLLECTION_ELEMENTS, DEFAULT_MAX_LIVE_OBJECTS,
};
pub use lower::{
    lower, lower_with_diagnostics, MAX_RISK_HOISTED_NODES, MAX_RISK_OPERAND_DEPTH,
    MAX_RISK_OPERAND_NODES,
};
pub use value::{HandleId, Value};

use pinewasm_hir::{
    default_max_bars_back, BinOp, BuiltinSeries, CellAttr, CollectionFunc, DrawKind, DrawingCaps,
    FillInfo, FillSlot, HtfExpr, InputDecl, LogLevel, ObjectTypeDecl, OrderKind, PivotSystem,
    PlotColorRole, PlotInfo, PlotSlot, PureFunc, RequestFunc, SeriesSlot, StrategyInfo, TaFunc,
    TaStateDecl, UnOp, VisualInfo, VisualSlot,
};
use pinewasm_span::Span;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

/// A virtual register holding one value for the current bar's execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ValueId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BlockId(pub u32);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MirInstr {
    Const {
        dst: ValueId,
        value: Value,
        span: Span,
    },
    ReadBuiltinSeries {
        dst: ValueId,
        source: BuiltinSeries,
        offset: u32,
        span: Span,
    },
    ReadUserSeries {
        dst: ValueId,
        slot: SeriesSlot,
        offset: u32,
        span: Span,
    },
    /// Read builtin `source` at a **runtime** bar offset (`close[n]`, `n` a
    /// series/simple int). `offset` holds the int operand (already on the
    /// stack); the VM reads `current_bar - offset` (out-of-range / `na` →
    /// `na`). Lowers to `pinewasm_csil::CsilInstr::ReadBuiltinDynamic`.
    ReadBuiltinSeriesDynamic {
        dst: ValueId,
        source: BuiltinSeries,
        offset: ValueId,
        span: Span,
    },
    /// Read user `slot` at a **runtime** bar offset. `offset` holds the int
    /// operand (already on the stack); the VM reads `current_bar - offset`
    /// (out-of-range / `na` → `na`). Lowers to
    /// `pinewasm_csil::CsilInstr::ReadUserSlotDynamic`.
    ReadUserSeriesDynamic {
        dst: ValueId,
        slot: SeriesSlot,
        offset: ValueId,
        span: Span,
    },
    /// Read one `input.*` value ([`MirMetadata::inputs`] entry `input`). The
    /// VM resolves the host override or the declared default at run time.
    ReadInput {
        dst: ValueId,
        input: u32,
        span: Span,
    },
    /// `request.security(sym, tf, expr)` over a plain builtin `source` series.
    /// `selector=None` reads `source` from the chart feed resampled to
    /// `htf_seconds`, back-mapped per chart bar under `gaps_on`/`lookahead_on`;
    /// `selector=Some(...)` reads a host-provided external-symbol series.
    /// Lowers 1:1 to `pinewasm_csil::CsilInstr::ReadSecurity`.
    ReadSecurity {
        dst: ValueId,
        selector: Option<String>,
        source: BuiltinSeries,
        htf_seconds: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeframe_expr: Option<String>,
        calendar_months: Option<u32>,
        gaps_on: bool,
        lookahead_on: bool,
        span: Span,
    },
    /// `request.security(sym, tf, expr)` over a COMPOUND `expr`. The
    /// secondary context evaluates the `HtfExpr` tree over the resampled HTF
    /// bars, back-mapped per chart bar under `gaps_on`/`lookahead_on`. Lowers
    /// 1:1 to `pinewasm_csil::CsilInstr::ReadSecurityExpr`.
    ReadSecurityExpr {
        dst: ValueId,
        expr: HtfExpr,
        htf_seconds: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeframe_expr: Option<String>,
        calendar_months: Option<u32>,
        gaps_on: bool,
        lookahead_on: bool,
        span: Span,
    },
    /// A scalar `request.*` external-data interface read. Reads the
    /// per-bar value for `selector` from the optional host table, or `na`.
    /// Lowers 1:1 to `pinewasm_csil::CsilInstr::ReadRequest`.
    ReadRequest {
        dst: ValueId,
        func: RequestFunc,
        selector: String,
        span: Span,
    },
    /// Build the current bar's order-flow `footprint` object from the optional
    /// host table. Lowers 1:1 to
    /// `pinewasm_csil::CsilInstr::BuildFootprint`.
    BuildFootprint {
        dst: ValueId,
        selector: String,
        span: Span,
    },
    /// Build the current bar's `array<float>` of intrabar values from the
    /// optional host table (`request.security_lower_tf`). Lowers 1:1
    /// to `pinewasm_csil::CsilInstr::ReadSecurityLowerTf`.
    ReadSecurityLowerTf {
        dst: ValueId,
        selector: String,
        span: Span,
    },
    /// Build the current bar's `array<float>` of pivot levels for the pivot
    /// `system` from the prior completed `anchor_seconds`-period HLC
    /// (`ta.pivot_point_levels`). Lowers 1:1 to
    /// `pinewasm_csil::CsilInstr::ReadPivotLevels`.
    ReadPivotLevels {
        dst: ValueId,
        system: PivotSystem,
        anchor_seconds: i64,
        span: Span,
    },
    /// Evaluate the series `type`, `anchor`, and `developing` arguments and
    /// advance the `ta.pivot_point_levels` state at this call site.
    CallPivotPointLevels {
        dst: ValueId,
        args: Vec<ValueId>,
        span: Span,
    },
    /// Baked-timeframe form of `time()` / `time_close()`. Runtime-string/session
    /// calls lower through `PureFunc::TimeOpen` /
    /// `PureFunc::TimeClose`; this remains for compatibility and lowers 1:1 to
    /// `pinewasm_csil::CsilInstr::ReadTimeTf`.
    ReadTimeTf {
        dst: ValueId,
        close: bool,
        tf_seconds: i64,
        span: Span,
    },
    /// `footprint.get_row_by_price(footprint, price)` — the matching
    /// `volume_row` handle (or `na`). Lowers 1:1 to
    /// `pinewasm_csil::CsilInstr::FootprintRowByPrice`. `price` is on top of the
    /// stack, the footprint handle below it.
    FootprintRowByPrice {
        dst: ValueId,
        footprint: ValueId,
        price: ValueId,
        span: Span,
    },
    StoreUserSeriesCurrent {
        slot: SeriesSlot,
        value: ValueId,
        span: Span,
    },
    Unary {
        dst: ValueId,
        op: UnOp,
        operand: ValueId,
        /// True for integer negation (`-` on an int operand): selects the
        /// checked-integer opcode at CSIL lowering. Always false for `not`.
        is_int: bool,
        span: Span,
    },
    Binary {
        dst: ValueId,
        op: BinOp,
        lhs: ValueId,
        rhs: ValueId,
        /// True for int arithmetic (`+ - * / %` with two int operands, i.e.
        /// an int result): selects the checked-integer opcodes at CSIL
        /// lowering. Comparisons, equality, and boolean logic do not need
        /// the flag (their opcodes are shared), so it stays false there.
        is_int: bool,
        /// True for string `+` concatenation (both operands strings): selects
        /// the `StrConcat` opcode at CSIL lowering. Mutually exclusive with
        /// `is_int`; both false means a float-arithmetic or shared opcode.
        is_string: bool,
        span: Span,
    },
    /// Short-circuit guard sitting between a lazy boolean's lhs and rhs
    /// instructions (`and`/`or` evaluate the rhs lazily). When `lhs` alone
    /// decides the result — `false` for `and`, `true` for `or` — execution
    /// jumps over the rhs instructions *and* the combining
    /// [`MirInstr::Binary`], leaving `lhs` as the expression's value. An
    /// `na` lhs falls through: the rhs still evaluates so the strong-Kleene
    /// combine can decide (`na and false = false`, `na or true = true`).
    /// CSIL lowering pairs the guard with the downstream `Binary` sharing its
    /// `lhs` register (registers are single-use, so the pairing is unique)
    /// and patches the emitted `JUMP_IF_EXACT_*` to land just past the
    /// combine. A `Binary { op: And | Or }` with no preceding guard (an old
    /// serialized program) still lowers 1:1 to its eager opcode.
    LazyBoolGuard {
        /// [`BinOp::And`] or [`BinOp::Or`].
        op: BinOp,
        lhs: ValueId,
        span: Span,
    },
    /// Explicit int → float coercion (from [`pinewasm_hir::HirExprKind::IntToFloat`]).
    IntToFloat {
        dst: ValueId,
        operand: ValueId,
        span: Span,
    },
    /// Ternary conditional select (from
    /// [`pinewasm_hir::HirExprKind::Ternary`]): `dst` becomes `lhs` when
    /// `cond` is true, `rhs` when it is false **or `na`**
    /// (na-condition-is-false, consistent with `if`). Both operands were
    /// already evaluated — the ternary is eager by design.
    Select {
        dst: ValueId,
        cond: ValueId,
        lhs: ValueId,
        rhs: ValueId,
        span: Span,
    },
    /// Arithmetic mean over the last `length` cells of a user series slot
    /// (bars `bar - length + 1 ..= bar`); `na` while fewer than `length`
    /// bars exist or when any cell in the window is `na`. Backs `ta.sma`.
    WindowMean {
        dst: ValueId,
        slot: SeriesSlot,
        length: u32,
        span: Span,
    },
    /// Runtime-sized `ta.sma` window. `length` is evaluated after storing the
    /// current source value and must be a positive int at execution time.
    WindowMeanDynamic {
        dst: ValueId,
        slot: SeriesSlot,
        length: ValueId,
        span: Span,
    },
    /// Runtime-sized highest or lowest window over a user series slot.
    WindowExtremumDynamic {
        dst: ValueId,
        slot: SeriesSlot,
        length: ValueId,
        highest: bool,
        span: Span,
    },
    /// Runtime-sized median, mode, or range over a user series slot.
    WindowStatisticDynamic {
        dst: ValueId,
        slot: SeriesSlot,
        length: ValueId,
        func: TaFunc,
        span: Span,
    },
    /// EMA with a runtime `simple int` length, retaining recursive state by
    /// call-site slot rather than storing a source-history window.
    EmaSimpleDynamic {
        dst: ValueId,
        slot: SeriesSlot,
        source: ValueId,
        length: ValueId,
        span: Span,
    },
    /// A pure builtin call (`math.*`, `na`, `nz`): consumes its argument
    /// registers in order and produces one value. Stateless — no slots, no
    /// bar-to-bar memory (`math.random` draws from the VM's per-run PRNG).
    CallPure {
        dst: ValueId,
        func: PureFunc,
        args: Vec<ValueId>,
        span: Span,
    },
    /// A reference-typed collection call (`array.*`): consumes its argument
    /// registers in order (receiver handle first) and produces exactly one
    /// value. Void mutators (`array.push`, `array.set`, ...) push a
    /// placeholder `na` so the stack discipline is uniform; a `Discard`
    /// pops it. May mutate the VM heap and may abort with a structured
    /// runtime error (OOB, na-receiver, heap-limit).
    CallCollection {
        dst: ValueId,
        func: CollectionFunc,
        args: Vec<ValueId>,
        span: Span,
    },
    /// An engine-backed stateful builtin call (`ta.ema`, `fixnan`, ...):
    /// consumes the runtime source operands in order and produces
    /// `dsts.len()` values. Scalar functions produce one (`dsts` has length
    /// 1); the tuple-returning functions (`ta.macd`/`ta.bb`/`ta.kc`/`ta.dmi`)
    /// produce three, pushed left to right (so `dsts[0]` is deepest on the
    /// stack and `dsts[N-1]` on top). `state` indexes
    /// [`MirMetadata::ta_states`]; the VM owns the per-call-site state and
    /// advances it only when this instruction executes.
    CallTa {
        dsts: Vec<ValueId>,
        state: u32,
        args: Vec<ValueId>,
        span: Span,
    },
    /// `Name.new(...)` — construct a user-defined-type object. Consumes `fields` value
    /// registers in order (declaration order) and produces one `Value::Ref`
    /// handle to a fresh `HeapObj::Object`. `type_id` indexes the program's
    /// object-type table (carried for dumps/validation; the runtime layout is
    /// positional). May abort with a heap-limit runtime error.
    NewObject {
        dst: ValueId,
        type_id: u32,
        fields: Vec<ValueId>,
        span: Span,
    },
    /// `Name.copy(obj)` — shallow-copy an object: consumes the source handle
    /// and produces a NEW handle whose field values are copied. An `na` source
    /// is a structured runtime error.
    CopyObject {
        dst: ValueId,
        type_id: u32,
        obj: ValueId,
        span: Span,
    },
    /// `obj.field` — read field `field_idx` of the object in `obj`. An `na`
    /// object is a structured runtime error.
    GetField {
        dst: ValueId,
        obj: ValueId,
        field_idx: u32,
        span: Span,
    },
    /// `obj.field := value` — write field `field_idx` of the object in `obj`.
    /// Produces no value. An `na` object is a structured runtime error.
    SetField {
        obj: ValueId,
        field_idx: u32,
        value: ValueId,
        span: Span,
    },
    /// `<ns>.new(...)` for a registered drawing kind: consumes the freshly
    /// constructed object handle in `obj`, registers it in the kind's live
    /// registry (running deterministic oldest-first GC), and produces the same
    /// handle. See `docs/ir.md` (drawing objects).
    RegisterDrawing {
        dst: ValueId,
        kind: DrawKind,
        obj: ValueId,
        span: Span,
    },
    /// `<ns>.all` — build a fresh `array<drawing>` of the kind's live objects.
    /// Takes no operand; produces the array handle in `dst`.
    ReadDrawingsAll {
        dst: ValueId,
        kind: DrawKind,
        span: Span,
    },
    /// `<ns>.delete(id)` — remove `id` from the kind's live registry. A void
    /// effect; to keep the stack discipline uniform it produces a placeholder
    /// `na` in `dst` (a `Discard` pops it), mirroring the void collection
    /// mutators. An `na` receiver is a structured runtime error.
    DeleteDrawing {
        dst: ValueId,
        kind: DrawKind,
        id: ValueId,
        span: Span,
    },
    /// `table.cell(...)` / `table.cell_set_<attr>(...)`: write the
    /// attributes in `attrs` (one `ValueId` per attribute, positionally aligned)
    /// of the cell at (`column`, `row`) of table `obj`. A void effect; produces a
    /// placeholder `na` in `dst`. An `na` receiver or an out-of-range coordinate
    /// is a structured runtime error. See `docs/ir.md` (table cells).
    SetCell {
        dst: ValueId,
        obj: ValueId,
        column: ValueId,
        row: ValueId,
        attrs: Vec<CellAttr>,
        values: Vec<ValueId>,
        span: Span,
    },
    /// `table.clear(...)`: clear every cell in the inclusive
    /// (`start_column`,`start_row`)..=(`end_column`,`end_row`) range of `obj`.
    /// Void; placeholder `na` in `dst`. `na` receiver / out-of-range = error.
    ClearCells {
        dst: ValueId,
        obj: ValueId,
        start_column: ValueId,
        start_row: ValueId,
        end_column: ValueId,
        end_row: ValueId,
        span: Span,
    },
    /// `table.merge_cells(...)`: mark the rectangular span merged.
    /// Void; placeholder `na` in `dst`. `na` receiver / out-of-range = error.
    MergeCells {
        dst: ValueId,
        obj: ValueId,
        start_column: ValueId,
        start_row: ValueId,
        end_column: ValueId,
        end_row: ValueId,
        span: Span,
    },
    Plot {
        plot_slot: PlotSlot,
        value: ValueId,
        colors: Vec<(PlotColorRole, ValueId)>,
        span: Span,
    },
    Visual {
        visual_slot: VisualSlot,
        colors: Vec<(PlotColorRole, ValueId)>,
        span: Span,
    },
    /// `fill(id1, id2, color)`. Color-only like [`MirInstr::Visual`]; the band's
    /// two endpoint plots are named in the program's `fills` table.
    Fill {
        fill_slot: FillSlot,
        colors: Vec<(PlotColorRole, ValueId)>,
        span: Span,
    },
    /// `runtime.error(message)`. Evaluates `message` and aborts the run
    /// with it as a structured user error; no value produced.
    RuntimeError { message: ValueId, span: Span },
    /// `log.info/warning/error(...)`. `operands[0]` is the format string;
    /// the rest are substitution values (in order). Records a log event; no
    /// value produced.
    Log {
        level: LogLevel,
        operands: Vec<ValueId>,
        span: Span,
    },
    /// `alert(message, freq?)`. Records an alert event; no value.
    Alert {
        message: ValueId,
        freq: ValueId,
        span: Span,
    },
    /// `alertcondition(condition, title?, message?)`. Records an alert
    /// event only when `condition` is true; no value.
    AlertCondition {
        condition: ValueId,
        title: ValueId,
        message: ValueId,
        span: Span,
    },
    /// A `strategy.*` order primitive. `kind` names the primitive;
    /// `operands` are the value ids for its fixed slot layout (in slot order —
    /// see [`pinewasm_hir::OrderKind`]). Places an order the broker emulator
    /// fills; no value produced.
    Order {
        kind: OrderKind,
        operands: Vec<ValueId>,
        span: Span,
    },
    /// Evaluate-and-discard (bare expression statements). Keeping the
    /// discard explicit lets the stack-CSIL lowering emit a balancing `Pop`.
    Discard { value: ValueId, span: Span },
}

impl MirInstr {
    pub fn span(&self) -> Span {
        match self {
            MirInstr::Const { span, .. }
            | MirInstr::ReadBuiltinSeries { span, .. }
            | MirInstr::ReadUserSeries { span, .. }
            | MirInstr::ReadBuiltinSeriesDynamic { span, .. }
            | MirInstr::ReadUserSeriesDynamic { span, .. }
            | MirInstr::ReadInput { span, .. }
            | MirInstr::ReadSecurity { span, .. }
            | MirInstr::ReadSecurityExpr { span, .. }
            | MirInstr::ReadRequest { span, .. }
            | MirInstr::BuildFootprint { span, .. }
            | MirInstr::ReadSecurityLowerTf { span, .. }
            | MirInstr::ReadPivotLevels { span, .. }
            | MirInstr::CallPivotPointLevels { span, .. }
            | MirInstr::ReadTimeTf { span, .. }
            | MirInstr::FootprintRowByPrice { span, .. }
            | MirInstr::StoreUserSeriesCurrent { span, .. }
            | MirInstr::Unary { span, .. }
            | MirInstr::Binary { span, .. }
            | MirInstr::LazyBoolGuard { span, .. }
            | MirInstr::IntToFloat { span, .. }
            | MirInstr::Select { span, .. }
            | MirInstr::WindowMean { span, .. }
            | MirInstr::WindowMeanDynamic { span, .. }
            | MirInstr::WindowExtremumDynamic { span, .. }
            | MirInstr::WindowStatisticDynamic { span, .. }
            | MirInstr::EmaSimpleDynamic { span, .. }
            | MirInstr::CallPure { span, .. }
            | MirInstr::CallCollection { span, .. }
            | MirInstr::CallTa { span, .. }
            | MirInstr::NewObject { span, .. }
            | MirInstr::CopyObject { span, .. }
            | MirInstr::GetField { span, .. }
            | MirInstr::SetField { span, .. }
            | MirInstr::RegisterDrawing { span, .. }
            | MirInstr::ReadDrawingsAll { span, .. }
            | MirInstr::DeleteDrawing { span, .. }
            | MirInstr::SetCell { span, .. }
            | MirInstr::ClearCells { span, .. }
            | MirInstr::MergeCells { span, .. }
            | MirInstr::Plot { span, .. }
            | MirInstr::Visual { span, .. }
            | MirInstr::Fill { span, .. }
            | MirInstr::RuntimeError { span, .. }
            | MirInstr::Log { span, .. }
            | MirInstr::Alert { span, .. }
            | MirInstr::AlertCondition { span, .. }
            | MirInstr::Order { span, .. }
            | MirInstr::Discard { span, .. } => *span,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Terminator {
    /// End of the per-bar program.
    Return {
        span: Span,
    },
    Jump {
        target: BlockId,
        span: Span,
    },
    Branch {
        cond: ValueId,
        then_block: BlockId,
        else_block: BlockId,
        span: Span,
    },
    /// Branch on a slot's persistent written flag (set the first time
    /// `StoreUserSeriesCurrent` executes for the slot, never by
    /// carry-forward). Used to lower `var` declarations: `unwritten_block`
    /// evaluates the initializer and stores it; `written_block` skips it.
    BranchIfSlotWritten {
        slot: SeriesSlot,
        written_block: BlockId,
        unwritten_block: BlockId,
        span: Span,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MirBlock {
    pub id: BlockId,
    pub instrs: Vec<MirInstr>,
    pub terminator: Terminator,
}

/// One user series slot: its source name plus its persistence mode.
/// Persistent slots (`var` declarations) carry the previous bar's final
/// value forward at the start of each bar; non-persistent slots start each
/// bar as `na`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotDef {
    pub name: String,
    pub persistent: bool,
    /// True only for `varip` slots: a persistent slot that additionally
    /// survives realtime-tick rollback (the VM's snapshot/restore skips it).
    /// Implies `persistent`. Defaults to false so programs serialized before
    /// the field existed deserialize as plain `var`/non-persistent slots.
    #[serde(default)]
    pub varip: bool,
    /// Declared lane: every value stored here is na or has this runtime tag.
    /// `None` means no claim; the slot is dynamically typed. Omitted from the
    /// serialized form when `None`, so a program without declarations
    /// serializes exactly as it did before the field existed. A declaration
    /// is a claim, not a fact: nothing reads it until a checker has
    /// established it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<pinewasm_hir::Lane>,
}

/// Names and titles carried through for debug dumps and runtime output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MirMetadata {
    pub title: Option<String>,
    /// Indicator presentation metadata, kept distinct from strategy execution settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indicator: Option<pinewasm_hir::IndicatorInfo>,
    /// `indicator(overlay=)` — `true` draws on the price pane, `false`
    /// (default) uses a separate pane. Presentation-only; carried for hosts.
    #[serde(default)]
    pub overlay: bool,
    /// The `strategy(...)` declaration, when the script is a strategy.
    /// Honored parameters drive the broker emulator; the rest is metadata.
    #[serde(default)]
    pub strategy: Option<StrategyInfo>,
    /// The `indicator(max_*_count=)` per-kind drawing GC caps. The VM's
    /// `DrawingRegistry` reads these to override its default oldest-first
    /// eviction limits. Defaults to all-`None`.
    #[serde(default)]
    pub drawing_caps: DrawingCaps,
    /// Effective historical-buffer ceiling used by dynamic history-offset
    /// runtime checks.
    #[serde(default = "default_max_bars_back")]
    pub max_bars_back: u32,
    /// Definition of each user series slot, indexed by slot.
    pub series_slots: Vec<SlotDef>,
    /// Data-bearing display declarations, indexed by plot slot.
    pub plots: Vec<PlotInfo>,
    /// Color-only chart visual declarations, indexed by visual slot.
    #[serde(default)]
    pub visuals: Vec<VisualInfo>,
    /// `fill(...)` band declarations, indexed by fill slot.
    #[serde(default)]
    pub fills: Vec<FillInfo>,
    /// Engine-backed stateful call-site declarations, indexed by state id
    /// (`CallTa`'s `state` operand).
    pub ta_states: Vec<TaStateDecl>,
    /// Input-backed TA parameters resolved once before each run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ta_input_bindings: Vec<pinewasm_hir::TaInputBinding>,
    /// `input.*` declarations, indexed by input id (`ReadInput`'s `input`
    /// operand).
    pub inputs: Vec<InputDecl>,
    /// User-defined-type layouts, indexed by `type_id` (`NewObject` /
    /// `CopyObject`'s `type_id` operand). The VM reads the field count to size a
    /// fresh heap object.
    pub object_types: Vec<ObjectTypeDecl>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MirBody {
    pub blocks: Vec<MirBlock>,
    pub entry: BlockId,
    pub value_count: u32,
    pub metadata: MirMetadata,
}

impl MirBody {
    pub fn block(&self, id: BlockId) -> &MirBlock {
        &self.blocks[id.0 as usize]
    }
}

// ---------------------------------------------------------------------------
// Debug dump
// ---------------------------------------------------------------------------

/// Render a span for an IR dump comment: bare `start..end` for the main
/// buffer, `source:start..end` for an imported library (matching the CSIL dump
/// in `pinewasm_csil::text`, so hosts parse every stage identically).
fn render_span(span: Span) -> String {
    if span.source.0 == 0 {
        format!("{}..{}", span.start, span.end)
    } else {
        format!("{}:{}..{}", span.source.0, span.start, span.end)
    }
}

/// Splice a `; span` comment in before the first newline at or after `from`.
/// A MIR instruction writes exactly one line (or a primary line plus indented
/// sub-lines, e.g. `plot`'s colors), so annotating up to the first newline
/// tags the instruction's own line and leaves its sub-lines clean.
fn splice_span(out: &mut String, from: usize, span: Span) {
    if let Some(rel) = out[from..].find('\n') {
        let at = from + rel;
        out.insert_str(at, &format!("  ; {}", render_span(span)));
    }
}

/// Human-readable MIR listing (span-free — the form snapshot tests pin).
pub fn dump_mir(body: &MirBody) -> String {
    dump_mir_impl(body, false)
}

/// MIR listing with a trailing `; span` source map on every instruction and
/// terminator, for the browser's line-linked IR view.
pub fn dump_mir_annotated(body: &MirBody) -> String {
    dump_mir_impl(body, true)
}

fn dump_mir_impl(body: &MirBody, annotate: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "MirBody title={:?} slots={} plots={} values={}",
        body.metadata.title.as_deref().unwrap_or("<none>"),
        body.metadata.series_slots.len(),
        body.metadata.plots.len(),
        body.value_count
    );
    for (i, slot) in body.metadata.series_slots.iter().enumerate() {
        let suffix = if slot.varip {
            " varip"
        } else if slot.persistent {
            " var"
        } else {
            ""
        };
        let _ = writeln!(out, "  slot {i} `{}`{suffix}", slot.name);
    }
    for (i, plot) in body.metadata.plots.iter().enumerate() {
        let _ = writeln!(
            out,
            "  plot {i} title={:?}",
            plot.title.as_deref().unwrap_or("<none>")
        );
    }
    for (i, decl) in body.metadata.ta_states.iter().enumerate() {
        let _ = writeln!(
            out,
            "  ta_state {i} {} params={:?}",
            decl.func.name(),
            decl.params
        );
    }
    for decl in &body.metadata.inputs {
        let _ = writeln!(
            out,
            "  input {} input.{} title={:?}",
            decl.id,
            decl.kind.describe(),
            decl.title.as_deref().unwrap_or("<none>")
        );
    }
    for block in &body.blocks {
        let _ = writeln!(out, "bb{}:", block.id.0);
        for instr in &block.instrs {
            let line_start = out.len();
            match instr {
                MirInstr::Const { dst, value, .. } => {
                    let _ = writeln!(out, "  v{} = const {}", dst.0, value.render());
                }
                MirInstr::ReadBuiltinSeries {
                    dst,
                    source,
                    offset,
                    ..
                } => {
                    let _ = writeln!(
                        out,
                        "  v{} = read_builtin {}[{offset}]",
                        dst.0,
                        source.name()
                    );
                }
                MirInstr::ReadUserSeries {
                    dst, slot, offset, ..
                } => {
                    let _ = writeln!(out, "  v{} = read_user slot{}[{offset}]", dst.0, slot.0);
                }
                MirInstr::ReadBuiltinSeriesDynamic {
                    dst,
                    source,
                    offset,
                    ..
                } => {
                    let _ = writeln!(
                        out,
                        "  v{} = read_builtin_dyn {}[v{}]",
                        dst.0,
                        source.name(),
                        offset.0
                    );
                }
                MirInstr::ReadUserSeriesDynamic {
                    dst, slot, offset, ..
                } => {
                    let _ = writeln!(
                        out,
                        "  v{} = read_user_dyn slot{}[v{}]",
                        dst.0, slot.0, offset.0
                    );
                }
                MirInstr::ReadInput { dst, input, .. } => {
                    let _ = writeln!(out, "  v{} = read_input input{input}", dst.0);
                }
                MirInstr::ReadSecurity {
                    dst,
                    selector,
                    source,
                    htf_seconds,
                    gaps_on,
                    lookahead_on,
                    ..
                } => {
                    let _ = writeln!(
                        out,
                        "  v{} = read_security {}{}[{htf_seconds}s] gaps={} lookahead={}",
                        dst.0,
                        selector
                            .as_ref()
                            .map(|s| format!("{s} "))
                            .unwrap_or_default(),
                        source.name(),
                        if *gaps_on { "on" } else { "off" },
                        if *lookahead_on { "on" } else { "off" },
                    );
                }
                MirInstr::ReadSecurityExpr {
                    dst,
                    htf_seconds,
                    gaps_on,
                    lookahead_on,
                    ..
                } => {
                    let _ = writeln!(
                        out,
                        "  v{} = read_security_expr [{htf_seconds}s] gaps={} lookahead={}",
                        dst.0,
                        if *gaps_on { "on" } else { "off" },
                        if *lookahead_on { "on" } else { "off" },
                    );
                }
                MirInstr::ReadRequest {
                    dst,
                    func,
                    selector,
                    ..
                } => {
                    let _ = writeln!(
                        out,
                        "  v{} = read_request {}[{selector}]",
                        dst.0,
                        func.member()
                    );
                }
                MirInstr::BuildFootprint { dst, selector, .. } => {
                    let _ = writeln!(out, "  v{} = build_footprint [{selector}]", dst.0);
                }
                MirInstr::ReadSecurityLowerTf { dst, selector, .. } => {
                    let _ = writeln!(out, "  v{} = read_security_lower_tf [{selector}]", dst.0);
                }
                MirInstr::ReadPivotLevels {
                    dst,
                    system,
                    anchor_seconds,
                    ..
                } => {
                    let _ = writeln!(
                        out,
                        "  v{} = read_pivot_levels {} [{anchor_seconds}s]",
                        dst.0,
                        system.type_name(),
                    );
                }
                MirInstr::CallPivotPointLevels { dst, args, .. } => {
                    let _ = writeln!(
                        out,
                        "  v{} = call_pivot_point_levels argc={}",
                        dst.0,
                        args.len()
                    );
                }
                MirInstr::ReadTimeTf {
                    dst,
                    close,
                    tf_seconds,
                    ..
                } => {
                    let _ = writeln!(
                        out,
                        "  v{} = read_time_tf {} [{tf_seconds}s]",
                        dst.0,
                        if *close { "close" } else { "open" },
                    );
                }
                MirInstr::FootprintRowByPrice {
                    dst,
                    footprint,
                    price,
                    ..
                } => {
                    let _ = writeln!(
                        out,
                        "  v{} = footprint_row_by_price v{}, v{}",
                        dst.0, footprint.0, price.0
                    );
                }
                MirInstr::StoreUserSeriesCurrent { slot, value, .. } => {
                    let _ = writeln!(out, "  store_user_current slot{}, v{}", slot.0, value.0);
                }
                MirInstr::Unary {
                    dst,
                    op,
                    operand,
                    is_int,
                    ..
                } => {
                    let suffix = if *is_int { " (int)" } else { "" };
                    let _ = writeln!(
                        out,
                        "  v{} = unary `{}` v{}{suffix}",
                        dst.0,
                        op.symbol(),
                        operand.0
                    );
                }
                MirInstr::Binary {
                    dst,
                    op,
                    lhs,
                    rhs,
                    is_int,
                    is_string,
                    ..
                } => {
                    let suffix = if *is_int {
                        " (int)"
                    } else if *is_string {
                        " (str)"
                    } else {
                        ""
                    };
                    let _ = writeln!(
                        out,
                        "  v{} = binary `{}` v{}, v{}{suffix}",
                        dst.0,
                        op.symbol(),
                        lhs.0,
                        rhs.0
                    );
                }
                MirInstr::LazyBoolGuard { op, lhs, .. } => {
                    let _ = writeln!(out, "  lazy_bool_guard `{}` v{}", op.symbol(), lhs.0);
                }
                MirInstr::IntToFloat { dst, operand, .. } => {
                    let _ = writeln!(out, "  v{} = int_to_float v{}", dst.0, operand.0);
                }
                MirInstr::Select {
                    dst,
                    cond,
                    lhs,
                    rhs,
                    ..
                } => {
                    let _ = writeln!(
                        out,
                        "  v{} = select v{}, v{}, v{}",
                        dst.0, cond.0, lhs.0, rhs.0
                    );
                }
                MirInstr::WindowMean {
                    dst, slot, length, ..
                } => {
                    let _ = writeln!(
                        out,
                        "  v{} = window_mean slot{} len={length}",
                        dst.0, slot.0
                    );
                }
                MirInstr::WindowMeanDynamic {
                    dst, slot, length, ..
                } => {
                    let _ = writeln!(
                        out,
                        "  v{} = window_mean_dynamic slot{} len=v{}",
                        dst.0, slot.0, length.0
                    );
                }
                MirInstr::WindowExtremumDynamic {
                    dst,
                    slot,
                    length,
                    highest,
                    ..
                } => {
                    let name = if *highest { "highest" } else { "lowest" };
                    let _ = writeln!(
                        out,
                        "  v{} = window_{name}_dynamic slot{} len=v{}",
                        dst.0, slot.0, length.0
                    );
                }
                MirInstr::WindowStatisticDynamic {
                    dst,
                    slot,
                    length,
                    func,
                    ..
                } => {
                    let _ = writeln!(
                        out,
                        "  v{} = window_{}_dynamic slot{} len=v{}",
                        dst.0,
                        func.name(),
                        slot.0,
                        length.0
                    );
                }
                MirInstr::EmaSimpleDynamic {
                    dst,
                    slot,
                    source,
                    length,
                    ..
                } => {
                    let _ = writeln!(
                        out,
                        "  v{} = ema_simple_dynamic slot{} source=v{} len=v{}",
                        dst.0, slot.0, source.0, length.0
                    );
                }
                MirInstr::CallPure {
                    dst, func, args, ..
                } => {
                    let rendered: Vec<String> = args.iter().map(|a| format!("v{}", a.0)).collect();
                    let _ = writeln!(
                        out,
                        "  v{} = call_pure {} {}",
                        dst.0,
                        func.name(),
                        rendered.join(", ")
                    );
                }
                MirInstr::CallCollection {
                    dst, func, args, ..
                } => {
                    let rendered: Vec<String> = args.iter().map(|a| format!("v{}", a.0)).collect();
                    let _ = writeln!(
                        out,
                        "  v{} = call_collection {} {}",
                        dst.0,
                        func.name(),
                        rendered.join(", ")
                    );
                }
                MirInstr::CallTa {
                    dsts, state, args, ..
                } => {
                    let rendered: Vec<String> = args.iter().map(|a| format!("v{}", a.0)).collect();
                    let targets: Vec<String> = dsts.iter().map(|d| format!("v{}", d.0)).collect();
                    let _ = writeln!(
                        out,
                        "  {} = call_ta state{state} {}",
                        targets.join(", "),
                        rendered.join(", ")
                    );
                }
                MirInstr::NewObject {
                    dst,
                    type_id,
                    fields,
                    ..
                } => {
                    let rendered: Vec<String> =
                        fields.iter().map(|a| format!("v{}", a.0)).collect();
                    let _ = writeln!(
                        out,
                        "  v{} = new_object ty{type_id} {}",
                        dst.0,
                        rendered.join(", ")
                    );
                }
                MirInstr::CopyObject {
                    dst, type_id, obj, ..
                } => {
                    let _ = writeln!(out, "  v{} = copy_object ty{type_id} v{}", dst.0, obj.0);
                }
                MirInstr::GetField {
                    dst,
                    obj,
                    field_idx,
                    ..
                } => {
                    let _ = writeln!(out, "  v{} = get_field v{}.{field_idx}", dst.0, obj.0);
                }
                MirInstr::SetField {
                    obj,
                    field_idx,
                    value,
                    ..
                } => {
                    let _ = writeln!(out, "  set_field v{}.{field_idx}, v{}", obj.0, value.0);
                }
                MirInstr::RegisterDrawing { dst, kind, obj, .. } => {
                    let _ = writeln!(
                        out,
                        "  v{} = register_drawing {} v{}",
                        dst.0,
                        kind.namespace(),
                        obj.0
                    );
                }
                MirInstr::ReadDrawingsAll { dst, kind, .. } => {
                    let _ = writeln!(out, "  v{} = read_drawings_all {}", dst.0, kind.namespace());
                }
                MirInstr::DeleteDrawing { dst, kind, id, .. } => {
                    let _ = writeln!(
                        out,
                        "  v{} = delete_drawing {} v{}",
                        dst.0,
                        kind.namespace(),
                        id.0
                    );
                }
                MirInstr::SetCell {
                    dst,
                    obj,
                    column,
                    row,
                    attrs,
                    values,
                    ..
                } => {
                    let rendered: Vec<String> = attrs
                        .iter()
                        .zip(values)
                        .map(|(a, v)| format!("{}=v{}", a.name(), v.0))
                        .collect();
                    let _ = writeln!(
                        out,
                        "  v{} = set_cell v{}[v{}, v{}] {}",
                        dst.0,
                        obj.0,
                        column.0,
                        row.0,
                        rendered.join(", ")
                    );
                }
                MirInstr::ClearCells {
                    dst,
                    obj,
                    start_column,
                    start_row,
                    end_column,
                    end_row,
                    ..
                } => {
                    let _ = writeln!(
                        out,
                        "  v{} = clear_cells v{}[v{}, v{}..v{}, v{}]",
                        dst.0, obj.0, start_column.0, start_row.0, end_column.0, end_row.0
                    );
                }
                MirInstr::MergeCells {
                    dst,
                    obj,
                    start_column,
                    start_row,
                    end_column,
                    end_row,
                    ..
                } => {
                    let _ = writeln!(
                        out,
                        "  v{} = merge_cells v{}[v{}, v{}..v{}, v{}]",
                        dst.0, obj.0, start_column.0, start_row.0, end_column.0, end_row.0
                    );
                }
                MirInstr::Plot {
                    plot_slot,
                    value,
                    colors,
                    ..
                } => {
                    let _ = writeln!(out, "  plot p{}, v{}", plot_slot.0, value.0);
                    for (role, color) in colors {
                        let _ = writeln!(out, "    color {role:?} v{}", color.0);
                    }
                }
                MirInstr::Visual {
                    visual_slot,
                    colors,
                    ..
                } => {
                    let rendered = colors
                        .iter()
                        .map(|(role, value)| format!("{role:?}=v{}", value.0))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let _ = writeln!(out, "  visual v{} {rendered}", visual_slot.0);
                }
                MirInstr::Fill {
                    fill_slot, colors, ..
                } => {
                    let rendered = colors
                        .iter()
                        .map(|(role, value)| format!("{role:?}=v{}", value.0))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let _ = writeln!(out, "  fill f{} {rendered}", fill_slot.0);
                }
                MirInstr::RuntimeError { message, .. } => {
                    let _ = writeln!(out, "  runtime.error v{}", message.0);
                }
                MirInstr::Log {
                    level, operands, ..
                } => {
                    let rendered: Vec<String> =
                        operands.iter().map(|a| format!("v{}", a.0)).collect();
                    let _ = writeln!(out, "  log.{} {}", level.member(), rendered.join(", "));
                }
                MirInstr::Alert { message, freq, .. } => {
                    let _ = writeln!(out, "  alert v{}, v{}", message.0, freq.0);
                }
                MirInstr::AlertCondition {
                    condition,
                    title,
                    message,
                    ..
                } => {
                    let _ = writeln!(
                        out,
                        "  alertcondition v{}, v{}, v{}",
                        condition.0, title.0, message.0
                    );
                }
                MirInstr::Order { kind, operands, .. } => {
                    let rendered: Vec<String> =
                        operands.iter().map(|a| format!("v{}", a.0)).collect();
                    let _ = writeln!(out, "  strategy.{} {}", kind.member(), rendered.join(", "));
                }
                MirInstr::Discard { value, .. } => {
                    let _ = writeln!(out, "  discard v{}", value.0);
                }
            }
            if annotate {
                splice_span(&mut out, line_start, instr.span());
            }
        }
        let term_start = out.len();
        let term_span = match &block.terminator {
            Terminator::Return { span } => {
                let _ = writeln!(out, "  return");
                *span
            }
            Terminator::Jump { target, span } => {
                let _ = writeln!(out, "  jump bb{}", target.0);
                *span
            }
            Terminator::Branch {
                cond,
                then_block,
                else_block,
                span,
            } => {
                let _ = writeln!(
                    out,
                    "  branch v{}, bb{}, bb{}",
                    cond.0, then_block.0, else_block.0
                );
                *span
            }
            Terminator::BranchIfSlotWritten {
                slot,
                written_block,
                unwritten_block,
                span,
            } => {
                let _ = writeln!(
                    out,
                    "  branch_if_slot_written slot{}, bb{}, bb{}",
                    slot.0, written_block.0, unwritten_block.0
                );
                *span
            }
        };
        if annotate {
            splice_span(&mut out, term_start, term_span);
        }
    }
    out
}

#[cfg(test)]
mod tests;
