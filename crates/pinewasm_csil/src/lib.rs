//! CSIL — the Chart Script Intermediate Language.
//!
//! A compact, versioned, stack-based bytecode executed once per bar by the
//! VM. The CSIL is the compiler's durable artifact: it carries a version
//! header, a constant pool, slot tables, a full source map (one [`Span`]
//! per instruction), and it is validated before execution.
//!
//! Serialization is `serde`-based (JSON across the wasm boundary); a compact
//! binary encoding can be added behind the same header without changing
//! semantics.
#![forbid(unsafe_code)]

mod budget_reference_size;
pub mod flow;
pub mod fraction;
mod history;
mod history_binding_walk;
mod lower;
mod plot_table;
mod proof_branch_view;
mod proof_literal;
mod proof_scalar_view;
mod ta_inputs;
mod text;
mod translation_validation;
mod validate;

pub use history::{MAX_DYNAMIC_WINDOW_DEPTH, MIN_HISTORY_DEPTH};
pub use lower::{
    lower_mir, lower_mir_validated, lower_mir_with_pass_record, CoalescingCapture, LowerError,
    LoweredWithPassRecord, PassSnapshot,
};
pub use proof_branch_view::{
    ProofBoundBranchView, ProofBranchInstr, ProofBranchView, ProofBranchViewError,
    BOUND_BRANCH_VIEW_FORMAT, BRANCH_VIEW_FORMAT,
};
pub use proof_literal::{ProofLiteral, ProofLiteralError};
pub use proof_scalar_view::{
    ProofBoundScalarView, ProofScalarInstr, ProofScalarView, ProofScalarViewError,
    BOUND_SCALAR_VIEW_FORMAT, SCALAR_VIEW_FORMAT,
};
pub use text::dump_csil;
pub use translation_validation::{check_pair_erasure, instr_exact_eq, PairCheckError};
pub use validate::lanes;
pub use validate::validate_lowered_program as validate_lowered;
pub use validate::validate_program as validate;
pub use validate::{
    collection_result_ty, pure_result_ty, validate_with_lane_facts, AbstractTy, ValidationError,
    MAX_DECLARED_STACK, MAX_VALIDATION_ABSTRACT_CELLS, MAX_WINDOW_LENGTH,
};

use pinewasm_hir::{default_max_bars_back, BuiltinSeries, HtfExpr};
pub use pinewasm_hir::{
    CalendarField, CellAttr, CollectionFunc, DisplayKind, DisplayStyle, DrawKind, DrawingCaps,
    ExternalSourceRef, InputActive, InputActiveBinaryOp, InputActiveDirect, InputActiveExpr,
    InputActiveUnaryOp, InputConstraints, InputDecl, InputDisplay, InputKind, InputValue,
    InputWidget, Lane, LogLevel, ObjectTypeDecl, OrderKind, PivotSystem, PlotColorRole,
    PlotDisplay, PlotLineStyle, PlotStyleKind, PureFunc, RequestFunc, StrategyInfo, TaFunc,
    TaStateDecl, TickerFunc, TradeAccessor,
};
use pinewasm_span::Span;
use serde::{Deserialize, Serialize};

pub use pinewasm_mir::SlotDef;

/// Magic identifying a CSIL program.
pub const CSIL_MAGIC: &str = "CSIL";
pub const MAX_BARS_BACK_DEFAULT: u32 = 500;
pub const MAX_BARS_BACK_LIMIT: u32 = 5000;
/// Bytecode format version. Version 1 defines the int/float numeric tower:
/// `PushI64`, the checked integer opcodes (`IntAdd`..`IntNeg`), `F64Rem`,
/// `IntToF64`, and `ReadBuiltin` (`BarIndex` pushes an int). Version 1 also
/// carries the `var` slot model: series slots are [`SlotDef`]s with a
/// `persistent` flag, and `JumpIfSlotWritten` branches on a slot's written
/// flag. The stateful `ta.*` builtins add `WindowMean`, the VM-native ta
/// engine adds `CallTa` plus the `ta_states` table, and the color type adds
/// `PushColor`.
/// Version 2 expands the fixed operand layouts of strategy order opcodes, so
/// the validator rejects version 1 strategy artifacts instead of risking stack
/// misinterpretation.
pub const CSIL_VERSION: u32 = 2;
/// Source-language version tag for the language subset this compiler accepts.
pub const LANGUAGE_VERSION: u32 = 6;
/// Version of the VM ↔ CSIL contract (opcode semantics, slot model).
pub const RUNTIME_ABI_VERSION: u32 = 0;
/// Rendering contract version this compiler emits in
/// `ProgramMetadata::rendering_contract_version` and publishes as the
/// rendering contract's `schemaVersion`. Version 0 denotes metadata that
/// carries no contract version; the validator rejects anything newer than
/// this.
pub const RENDERING_CONTRACT_VERSION: u32 = 1;
/// Programs with TA input bindings require this ABI; readers without it must
/// reject them instead of silently executing the instruction/declaration defaults.
pub const TA_INPUT_ABI_VERSION: u32 = 1;
/// Ordered indexed-scan operations and their actual-work metering require this ABI.
/// Readers without it reject such programs before attempting to execute them.
pub const INDEXED_SCAN_ABI_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CsilHeader {
    pub magic: String,
    /// Canonical Rust name; the stable CSIL v2 wire key remains `il_version`.
    #[serde(rename = "il_version", alias = "csil_version")]
    pub csil_version: u32,
    pub language_version: u32,
    pub runtime_abi_version: u32,
}

impl Default for CsilHeader {
    fn default() -> Self {
        CsilHeader {
            magic: CSIL_MAGIC.to_string(),
            csil_version: CSIL_VERSION,
            language_version: LANGUAGE_VERSION,
            runtime_abi_version: RUNTIME_ABI_VERSION,
        }
    }
}

/// One plot output declaration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CsilPlot {
    pub title: Option<String>,
    #[serde(default)]
    pub style: DisplayStyle,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub color_roles: Vec<PlotColorRole>,
}

/// One color-only chart visual declaration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CsilVisual {
    pub title: Option<String>,
    #[serde(default)]
    pub style: DisplayStyle,
    pub color_roles: Vec<PlotColorRole>,
}

/// One `fill(...)` band declaration: the two plot slots it spans plus the
/// per-bar color channels. Color-only like [`CsilVisual`] — the geometry is read
/// from the two referenced plot series.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CsilFill {
    pub plot1: u32,
    pub plot2: u32,
    pub title: Option<String>,
    #[serde(default)]
    pub style: DisplayStyle,
    pub color_roles: Vec<PlotColorRole>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CsilFunction {
    pub name: String,
    pub code: Vec<CsilInstr>,
    /// Exactly one span per instruction (the source map).
    pub source_map: Vec<Span>,
    /// Compiler-produced code before the first instruction-deleting pass.
    /// Used only to attribute a matching instruction-budget failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_reference: Option<BudgetReference>,
    /// Maximum operand-stack depth, computed during lowering and re-checked
    /// by the validator.
    pub max_stack: u32,
}

/// Versioned reference for failure-only budget attribution. The ordinary
/// `source_map` continues to describe surviving optimized instructions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetReference {
    /// The optimization pipeline that turns `code` into the function's code:
    /// [`BUDGET_REFERENCE_VERSION_THREE_STAGE`] (dead literal stores,
    /// single-use transfers, entry-prefix aliases; no flow-alias or
    /// jump-threading stages) or
    /// [`BUDGET_REFERENCE_VERSION`] (those three, then flow-sensitive aliases
    /// and jump threading). Validation replays exactly that pipeline.
    pub version: u32,
    pub code: Vec<CsilInstr>,
    pub source_map: Vec<Span>,
    pub max_stack: u32,
}

/// [`BudgetReference::version`] of artifacts optimized by the three-stage
/// pipeline. Still accepted and replayed with that pipeline.
pub const BUDGET_REFERENCE_VERSION_THREE_STAGE: u32 = 1;

/// [`BudgetReference::version`] the compiler emits: the five-stage pipeline.
pub const BUDGET_REFERENCE_VERSION: u32 = 2;

/// Whether validation knows how to replay a reference of this version.
pub(crate) fn budget_reference_version_known(version: u32) -> bool {
    version == BUDGET_REFERENCE_VERSION_THREE_STAGE || version == BUDGET_REFERENCE_VERSION
}

/// Maximum counted resident and JSON-encoded reference payload.
pub const MAX_BUDGET_REFERENCE_BYTES: usize = 16 * 1024 * 1024;

/// Count reserved instruction storage and opcode-owned dynamic allocations.
/// This is an admission estimate, not a process allocator or RSS measurement.
#[doc(hidden)]
pub fn csil_code_allocated_bytes(code: &Vec<CsilInstr>) -> Option<usize> {
    let mut bytes = code
        .capacity()
        .checked_mul(std::mem::size_of::<CsilInstr>());
    let mut i = 0usize;
    while i < code.len() && bytes.is_some() {
        if let Some(current) = bytes {
            bytes = instruction_bytes(&code[i], current);
        }
        i += 1;
    }

    bytes
}

fn htf_step<'a>(
    node: &'a HtfExpr,
    pending: &mut Vec<&'a HtfExpr>,
    mut bytes: usize,
) -> Option<usize> {
    match node {
        HtfExpr::BoolResult(inner) => {
            bytes = bytes.checked_add(std::mem::size_of::<HtfExpr>())?;
            pending.push(inner);
        }
        HtfExpr::HistoryInput { offset, source, .. } => {
            bytes = bytes
                .checked_add(offset.allocated_bytes()?)?
                .checked_add(std::mem::size_of::<HtfExpr>())?;
            pending.push(source);
        }
        HtfExpr::History { source, .. }
        | HtfExpr::HistoryRealtime { source, .. }
        | HtfExpr::Neg(source)
        | HtfExpr::Ta { source, .. }
        | HtfExpr::TaInput { source, .. } => {
            bytes = bytes.checked_add(std::mem::size_of::<HtfExpr>())?;
            pending.push(source);
        }
        HtfExpr::EnumSelect {
            when_equal,
            otherwise,
            ..
        } => {
            bytes = bytes.checked_add(std::mem::size_of::<HtfExpr>())?;
            pending.push(when_equal);
            if let Some(otherwise) = otherwise {
                bytes = bytes.checked_add(std::mem::size_of::<HtfExpr>())?;
                pending.push(otherwise);
            }
        }
        HtfExpr::Binary { lhs, rhs, .. } => {
            bytes = bytes.checked_add(2 * std::mem::size_of::<HtfExpr>())?;
            pending.push(lhs);
            pending.push(rhs);
        }
        HtfExpr::Const(_)
        | HtfExpr::Source(_)
        | HtfExpr::InputSource { .. }
        | HtfExpr::BarstateIslast
        | HtfExpr::BarstateIsRealtime => {}
    }
    if bytes > MAX_BUDGET_REFERENCE_BYTES {
        None
    } else {
        Some(bytes)
    }
}
fn htf_boxes(expr: &HtfExpr) -> Option<usize> {
    let mut bytes = Some(0usize);
    let mut pending = vec![expr];
    // Keep failure in the loop state instead of returning from a loop body.
    // No later node is visited after the first failed allocation estimate.
    while !pending.is_empty() && bytes.is_some() {
        if let (Some(node), Some(current)) = (pending.pop(), bytes) {
            bytes = htf_step(node, &mut pending, current);
        }
    }
    bytes
}

fn instruction_bytes(instr: &CsilInstr, bytes: usize) -> Option<usize> {
    let extra = match instr {
        CsilInstr::ReadSecurity {
            selector,
            timeframe_expr,
            ..
        } => selector
            .as_ref()
            .map_or(0, String::capacity)
            .checked_add(timeframe_expr.as_ref().map_or(0, String::capacity))?,
        CsilInstr::ReadRequest { selector, .. }
        | CsilInstr::BuildFootprint { selector }
        | CsilInstr::ReadSecurityLowerTf { selector } => selector.capacity(),
        CsilInstr::SetCell { attrs } => attrs
            .capacity()
            .checked_mul(std::mem::size_of::<CellAttr>())?,
        CsilInstr::ReadSecurityExpr {
            expr,
            timeframe_expr,
            ..
        } => htf_boxes(expr)?.checked_add(timeframe_expr.as_ref().map_or(0, String::capacity))?,
        _ => 0,
    };
    bytes.checked_add(extra)
}

/// Count the reference's reserved vectors and nested opcode payloads.
#[doc(hidden)]
pub fn budget_reference_allocated_bytes(reference: &BudgetReference) -> Option<usize> {
    csil_code_allocated_bytes(&reference.code)?.checked_add(
        budget_reference_size::checked_span_storage(
            reference.source_map.capacity(),
            std::mem::size_of::<Span>(),
        )?,
    )
}

/// Bound counted resident storage and encoded content without allocating a
/// second serialized copy of untrusted reference data.
pub(crate) fn budget_reference_within_limit(reference: &BudgetReference) -> bool {
    if budget_reference_allocated_bytes(reference)
        .is_none_or(|bytes| bytes > MAX_BUDGET_REFERENCE_BYTES)
    {
        return false;
    }
    serde_json::to_writer(budget_reference_size::Counter(0), reference).is_ok()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProgramMetadata {
    /// Semantic rendering boundary. Zero means no rendering contract is declared.
    #[serde(default, skip_serializing_if = "rendering_contract_is_unversioned")]
    pub rendering_contract_version: u32,
    pub title: Option<String>,
    /// Indicator presentation metadata; programs without it omit the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indicator: Option<pinewasm_hir::IndicatorInfo>,
    /// `indicator(overlay=)` — `true` draws the script's plots on the price
    /// pane, `false` (default) in a separate pane. Presentation metadata for
    /// hosts; it has no effect on computed values. Defaults to `false` when
    /// the field is absent.
    #[serde(default)]
    pub overlay: bool,
    /// The `strategy(...)` declaration, const-folded during analysis.
    /// `Some` for strategy scripts; `None` for indicators. Defaults to `None`
    /// when the field is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<StrategyInfo>,
    /// The `indicator(max_*_count=)` per-kind drawing GC caps. Consumed
    /// by the VM's `DrawingRegistry`. Defaults to all-`None`.
    #[serde(default, skip_serializing_if = "DrawingCaps::is_empty")]
    pub drawing_caps: DrawingCaps,
    /// Effective history-buffer ceiling used by runtime dynamic history-offset
    /// checks. Defaults to `default_max_bars_back()` when the field is absent.
    #[serde(default = "default_max_bars_back")]
    pub max_bars_back: u32,
}

// Serde's skip_serializing_if callback receives a reference.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn rendering_contract_is_unversioned(version: &u32) -> bool {
    *version == 0
}

/// Serializable bytecode program consumed by the reference VM and artifact
/// compiler. Compiler-produced values are validated before they are returned;
/// deserialized or manually constructed values are untrusted until validated
/// by an execution or artifact boundary.
///
/// The supported compiler facade exports this as `Program` and treats its
/// representation as opaque; callers should construct it by compiling source
/// rather than assembling or editing these tables.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CsilProgram {
    /// Format, source-language, and runtime compatibility identifiers.
    pub header: CsilHeader,
    /// String constant pool ([`CsilInstr::PushStr`] indexes into it).
    pub constants: Vec<String>,
    /// User series slot definitions (name + persistence), indexed by slot
    /// number. Persistent slots carry the previous bar's final value forward
    /// at the start of each bar.
    pub series_slots: Vec<SlotDef>,
    /// Plot slots, indexed by plot number.
    pub plots: Vec<CsilPlot>,
    /// Color-only chart visuals, indexed by visual number.
    #[serde(default)]
    pub visuals: Vec<CsilVisual>,
    /// `fill(...)` band declarations, indexed by fill slot.
    #[serde(default)]
    pub fills: Vec<CsilFill>,
    /// Engine-backed stateful call-site declarations, indexed by the state
    /// id that `CALL_TA { state }` carries. One entry per syntactic call
    /// site; the validator requires each entry to be referenced by at most
    /// one instruction. Const parameters (window lengths, `ta.alma`'s
    /// offset/sigma as `f64` bit patterns, boolean flags as 0/1) are baked
    /// here so the instruction stays small. Defaults to empty when the field
    /// is absent.
    #[serde(default)]
    pub ta_states: Vec<TaStateDecl>,
    /// Input-backed TA parameters resolved once before each run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ta_input_bindings: Vec<pinewasm_hir::TaInputBinding>,
    /// `input.*` declarations, indexed by the input id that `READ_INPUT`
    /// carries. Serialized for hosts (id, kind, widget, title, tooltip, inline,
    /// group, confirm, display, active, defval, constraints, span)
    /// and read by the VM to resolve the override-or-default value. Defaults
    /// to empty when the field is absent.
    #[serde(default)]
    pub inputs: Vec<InputDecl>,
    /// User-defined-type layouts, indexed by the `type_id` that
    /// `NEW_OBJECT` / `COPY_OBJECT` carry. The VM reads the field count to size
    /// a fresh heap object. Defaults to empty when the field is absent.
    #[serde(default)]
    pub object_types: Vec<ObjectTypeDecl>,
    /// `functions[0]` is `main`, executed once per bar. User functions are
    /// inlined per call site today; the vector keeps the durable format open
    /// to additional compiled functions without changing its shape.
    pub functions: Vec<CsilFunction>,
    /// Script declaration and runtime sizing metadata.
    pub metadata: ProgramMetadata,
}

impl CsilProgram {
    /// Entry function executed once per bar.
    ///
    /// # Panics
    ///
    /// Panics if the representation was manually constructed or modified to
    /// contain no functions. Supported execution entry points validate that
    /// invariant before calling this method.
    #[inline(always)]
    pub fn main(&self) -> &CsilFunction {
        &self.functions[0]
    }

    /// Serialize this program to its JSON cache representation.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("CsilProgram serialization is infallible")
    }

    /// Deserialize a JSON cache representation without validating bytecode.
    /// Call the validator or a supported execution entry point before use.
    pub fn from_json(json: &str) -> Result<CsilProgram, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// The deepest bar offset any legal execution of this program can read
    /// from a user series slot.
    ///
    /// Four things set the bound, and it is the largest of them:
    ///
    /// * `max_bars_back`, but only when the program reads a slot at a
    ///   *dynamic* offset (`ReadUserSlotDynamic`). It caps every such read,
    ///   and reads past it abort with `HistoryBufferLimitExceeded`, so
    ///   nothing beyond it is observable. A program with no dynamic slot read
    ///   cannot observe `max_bars_back` through slot history at all, so it
    ///   does not pay for it: slots of such programs keep only the minimum ring.
    /// * literal `ReadUserSlot { offset }`, which is not checked at runtime
    ///   because the offset is a compile-time constant.
    /// * `WindowMean { length }`, which scans `length` cells ending at the
    ///   current bar, i.e. reaches `length - 1` back; runtime-length windows
    ///   reach [`MAX_DYNAMIC_WINDOW_DEPTH`].
    /// * [`MIN_HISTORY_DEPTH`] (one bar): the bar preamble carries a
    ///   `var` slot forward from the previous bar's cell, and re-running the
    ///   last committed bar in realtime must find that cell distinct from the
    ///   current one.
    ///
    /// Anything deeper is unobservable by construction, which is what lets
    /// the interpreter store slot history in a ring of
    /// `(depth + 1).next_power_of_two()` cells instead of one cell per bar.
    /// `pinewasm_vm` sizes its slot storage from this. `pinewasm_aot` plans
    /// its own per-slot rings and still floors them at `max_bars_back`,
    /// which only ever over-allocates.
    pub fn history_depth(&self) -> u32 {
        self.history_depth_with_ta_inputs(&std::collections::HashMap::new())
    }

    /// Plan a fresh run's ring before cloning or allocating its program/state.
    /// Values must have passed `resolve_ta_input`'s parameter bounds checks;
    /// a value outside `u32` is read as `u32::MAX` (a checked cast, see
    /// `history::ta_override_u32`), so the plan never under-sizes the ring.
    pub fn history_depth_with_ta_inputs(
        &self,
        values: &std::collections::HashMap<pinewasm_hir::TaInputTarget, i64>,
    ) -> u32 {
        let observers = self.history_depth_observers(values);
        let base = history::history_depth_base(
            self.metadata.max_bars_back,
            history_binding_walk::reads_slot_dynamically(&self.functions),
        );
        history::history_depth_scan(base, &observers)
    }

    /// Every instruction that can reach backwards, in program order, with its
    /// TA override looked up and cast. This is all the `HashMap`/`CsilInstr`
    /// glue; the depth itself is the slice kernel `history::history_depth_scan`.
    fn history_depth_observers(
        &self,
        values: &std::collections::HashMap<pinewasm_hir::TaInputTarget, i64>,
    ) -> Vec<history::DepthObserver> {
        history_binding_walk::observers(&self.functions, values)
    }

    /// Ring size for slot history: a power of two strictly greater than
    /// [`CsilProgram::history_depth`], so `bar & (ring - 1)` indexes it.
    pub fn history_ring(&self) -> u32 {
        history::history_ring_aot(self.history_depth())
    }
}

/// The `f64` literal carried by [`CsilInstr::PushF64`], stored as its IEEE-754
/// bit pattern.
///
/// A zero-cost wrapper (`repr(transparent)` over `u64`; `to_bits`/`from_bits`
/// are free) that behaves exactly like the `f64` it replaced: equality is
/// float equality (`NaN != NaN`, `0.0 == -0.0`), `Debug`/`Display` print the
/// float, and serde reads and writes a plain JSON number, so the CSIL wire
/// format is unchanged. Holding the payload as an integer keeps `f64` out of
/// the `CsilInstr` type definition, which Aeneas cannot translate.
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct F64Bits(u64);

impl F64Bits {
    /// Wrap a float.
    pub const fn new(value: f64) -> Self {
        F64Bits(value.to_bits())
    }

    /// The wrapped float.
    pub const fn get(self) -> f64 {
        f64::from_bits(self.0)
    }

    /// Transparency keeps whole JSON numbers backward readable.
    pub fn serialize_transparency<S: serde::Serializer>(
        value: &Self,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        pinewasm_hir::serialize_transparency(&value.get(), serializer)
    }

    /// The raw IEEE-754 bit pattern.
    pub const fn to_bits(self) -> u64 {
        self.0
    }
}

impl From<f64> for F64Bits {
    fn from(value: f64) -> Self {
        F64Bits::new(value)
    }
}

impl From<F64Bits> for f64 {
    fn from(value: F64Bits) -> Self {
        value.get()
    }
}

impl PartialEq for F64Bits {
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}

impl std::fmt::Debug for F64Bits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.get(), f)
    }
}

impl std::fmt::Display for F64Bits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.get(), f)
    }
}

impl Serialize for F64Bits {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.get().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for F64Bits {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        f64::deserialize(deserializer).map(F64Bits::new)
    }
}

/// Stack-machine opcodes, version 2.
///
/// Conventions:
/// - Binary operators pop `rhs` then `lhs` and push the result.
/// - `offset` operands are history offsets in bars (0 = current bar).
/// - Jump targets are absolute instruction indices within the function.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
// Direct tags avoid niche decoding at every interpreter opcode selection.
#[repr(u8)]
pub enum CsilInstr {
    // Stack manipulation.
    PushNa,
    PushBool(bool),
    PushI64(#[serde(deserialize_with = "pinewasm_hir::deserialize_i64_number_or_string")] i64),
    PushF64(F64Bits),
    /// Push a string from the constant pool.
    PushStr(u32),
    /// Push a color constant. The four bytes are `r`, `g`, `b`, and
    /// transparency `t` (`0..=100`); the validator rejects `t > 100`. Colors
    /// are a distinct runtime type (`AbstractTy::Color`); the accessors and
    /// display effects consume them. Metering: no runtime cost (a push).
    PushColor {
        r: u8,
        g: u8,
        b: u8,
        #[serde(serialize_with = "F64Bits::serialize_transparency")]
        t: F64Bits,
    },
    Dup,
    Pop,

    // Series access. `BarIndex` pushes an int; the OHLCV series push floats.
    ReadBuiltin {
        source: BuiltinSeries,
        offset: u32,
    },
    ReadUserSlot {
        slot: u32,
        offset: u32,
    },
    /// Pop an int-typed `offset` from the stack and push builtin `source`
    /// read at `current_bar - offset`. A fractional runtime value produced
    /// by Pine int division is truncated toward zero. A non-numeric (or `na`)
    /// offset, a negative truncated offset, or unavailable history pushes
    /// `na`.
    /// The literal-offset form stays [`CsilInstr::ReadBuiltin`]. Stack effect
    /// `(1, 1)`.
    ReadBuiltinDynamic {
        source: BuiltinSeries,
    },
    /// Pop an int-typed `offset` from the stack and push user `slot` read at
    /// `current_bar - offset`; fractional runtime values truncate toward
    /// zero (out-of-range / `na` offset → `na`). The
    /// literal-offset form stays [`CsilInstr::ReadUserSlot`]. Stack effect
    /// `(1, 1)`.
    ReadUserSlotDynamic {
        slot: u32,
    },
    StoreUserSlotCurrent {
        slot: u32,
    },
    /// Push the value of `inputs[input]`, resolved by the VM from the host
    /// override map (keyed by input id) or the declared default. The pushed
    /// runtime type is fixed by the declaration's [`InputKind`]
    /// (`int`/`float`/`bool`/`string`/`color`, or a `float` series sample for
    /// `input.source`); the validator consults the `inputs` table for the
    /// abstract type, since the instruction alone does not carry it. Metering:
    /// no runtime cost beyond a push (source reads a single series cell).
    ReadInput {
        input: u32,
    },
    /// Push the `request.security` value for the current bar. With
    /// `selector=None`, read builtin series `source` over the chart feed
    /// RESAMPLED to `htf_seconds`, mapped back onto this chart bar honoring
    /// `gaps`/`lookahead` (see `pinewasm_vm::resample` and docs/runtime.md,
    /// "secondary contexts"). With `selector=Some(...)`, read the host-provided
    /// external-symbol series from `MarketData::request_data`.
    /// `htf_seconds` is positive literal seconds, zero for the chart period,
    /// or `-(input_id + 1)` for a run-constant string input. `gaps_on` yields `na`
    /// between HTF closes; `lookahead_on` reads the forming bucket (repaints).
    /// Only a plain OHLCV/derived builtin series `expression` lowers here; a
    /// same-feed compound sub-expression lowers to `ReadSecurityExpr`, while
    /// external-symbol compound expressions are rejected at analysis (`E0244`).
    /// Metering: costs 1 unit (the VM memoizes the resampled series per run).
    ReadSecurity {
        selector: Option<String>,
        source: BuiltinSeries,
        #[serde(deserialize_with = "pinewasm_hir::deserialize_i64_number_or_string")]
        htf_seconds: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeframe_expr: Option<String>,
        /// Preserves calendar-month identity separately from nominal seconds,
        /// so `1M` is calendar-aligned while `30D` remains fixed-width.
        #[serde(default)]
        calendar_months: Option<u32>,
        gaps_on: bool,
        lookahead_on: bool,
    },
    /// Push the `request.security` value for a COMPOUND `expression`: a
    /// secondary context evaluates the [`HtfExpr`] tree over the chart feed
    /// RESAMPLED to `htf_seconds` — with its own per-context `ta.*` state,
    /// distinct from `main` — producing one value per HTF bucket, then maps
    /// back onto this chart bar honoring `gaps_on`/`lookahead_on` (see
    /// `pinewasm_vm::resample` and docs/runtime.md, "secondary contexts").
    /// Metering: costs 1 unit (the VM memoizes the whole back-mapped series per
    /// site per run). Pushes **bool** when the tree has an `HtfExpr::BoolResult`
    /// root, otherwise **float**. An HTF gap or `na` sub-result reads `na`.
    ReadSecurityExpr {
        expr: HtfExpr,
        #[serde(deserialize_with = "pinewasm_hir::deserialize_i64_number_or_string")]
        htf_seconds: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeframe_expr: Option<String>,
        #[serde(default)]
        calendar_months: Option<u32>,
        gaps_on: bool,
        lookahead_on: bool,
    },
    /// Push the current bar's value for a scalar `request.*` external-data
    /// interface. The VM looks `selector` up in the optional host
    /// table `MarketData::request_data`: a hit yields that per-bar float (with
    /// the usual non-finite → `na` normalization), a miss yields `na`. This is
    /// a pure host-table read with no second data feed — the interface exists
    /// so the `request.*` language surface is complete and testable while the
    /// data sourcing stays a host concern (see docs/ir.md and docs/runtime.md).
    /// Always pushes **float**. Metering: costs 1 unit.
    ReadRequest {
        func: RequestFunc,
        selector: String,
    },
    /// Build and push the current bar's order-flow `footprint` object. The VM
    /// materializes the object's scalar
    /// order-flow fields and its trailing `array<volume_row>` rows from the
    /// optional host table `MarketData::footprint_data`, keyed by `selector`; a
    /// miss (the default) yields an object whose scalar fields are all `na`,
    /// whose imbalance flags are `false`, and whose `rows` array is empty. No
    /// second data feed is involved — the interface makes the `footprint`
    /// language surface complete and testable while data sourcing stays a host
    /// concern (see docs/ir.md and docs/runtime.md). Always pushes a `footprint`
    /// object reference. Metering: costs 1 unit.
    BuildFootprint {
        selector: String,
    },
    /// Build the current bar's `array<float>` of intrabar values
    /// (`request.security_lower_tf`) from the optional host table
    /// `MarketData::security_lower_tf_data`, keyed by `selector`; a miss (the
    /// default) or an uncovered bar yields an EMPTY float array. This VM only
    /// aggregates UP, so lower-timeframe intrabar decomposition is a host
    /// concern — no real intrabar feed is involved. Always pushes
    /// an `array<float>` reference. Metering: costs 1 unit.
    ReadSecurityLowerTf {
        selector: String,
    },
    /// Build and push the current bar's `array<float>` of pivot levels for the
    /// pivot `system` (`ta.pivot_point_levels`). The chart feed is
    /// resampled to `anchor_seconds` (the same machinery as `READ_SECURITY`);
    /// the VM reads the PRIOR completed anchor bucket's High/Low/Close
    /// (`lookahead_off` — never the forming bucket, so no future leak),
    /// computes `[P, R1, S1, R2, S2, R3, S3, R4, S4, R5, S5]` for the system
    /// (a level a system omits is `na`), and pushes a fresh `array<float>`
    /// reference. Before any anchor bucket has completed, pushes an EMPTY
    /// array. Deterministic; never panics; allocating the array charges the
    /// heap budget (a structured error past the cap). Metering: costs 1 unit
    /// (the VM memoizes the resampled feed per run).
    ReadPivotLevels {
        system: PivotSystem,
        #[serde(deserialize_with = "pinewasm_hir::deserialize_i64_number_or_string")]
        anchor_seconds: i64,
    },
    /// Execute Pine `ta.pivot_point_levels` with the series string/bool/bool
    /// operands on the value stack. The VM retains period state by call site
    /// and pushes a fresh 11-slot `array<float>` reference.
    CallPivotPointLevels {
        argc: u8,
    },
    /// Push the epoch-ms time of the chart bar's enclosing `tf_seconds`
    /// timeframe bucket (the FUNCTION forms `time(timeframe)` /
    /// `time_close(timeframe)`). The chart feed is resampled to `tf_seconds`
    /// (the same machinery as `READ_SECURITY`/`READ_PIVOT_LEVELS`); at each
    /// chart bar the VM finds the bucket that bar belongs to and pushes its
    /// OPEN time when `close` is false, or its CLOSE time (bucket open +
    /// `tf_seconds * 1000`) when `close` is true. The bucket is the bar's own
    /// (currently-forming) enclosing bucket — this is the bucket time the bar
    /// falls in, NOT a look-back, so it never leaks a future bar. Always pushes
    /// a `series int` (epoch ms). The baked-timeframe path is kept for
    /// serialized-program compatibility; the source-level timeframe/session
    /// overloads use `CALL_PURE TimeOpen/TimeClose`.
    /// Deterministic; never panics. Metering: costs 1 unit (the VM memoizes the
    /// resampled feed per `tf_seconds`).
    ReadTimeTf {
        close: bool,
        #[serde(deserialize_with = "pinewasm_hir::deserialize_i64_number_or_string")]
        tf_seconds: i64,
    },
    /// Pop a price (top) and a `footprint` object handle, and push the object's
    /// matching `volume_row` handle — the row whose `[down_price, up_price]`
    /// range contains the price — or `na` when no row matches (or the footprint
    /// has no rows). An `na` footprint receiver is a structured runtime error
    /// (mirrors field access); never panics. Backs
    /// `footprint.get_row_by_price(fp, price)`. Metering: costs 1 unit.
    FootprintRowByPrice,
    /// Push the arithmetic mean of the last `length` cells of a user series
    /// slot (bars `bar - length + 1 ..= bar`); `na` while fewer than
    /// `length` bars exist or when any cell in the window is `na`. The
    /// validator requires `1 <= length <= 100000`. Metering: costs `length`
    /// units of the per-bar instruction budget (the VM scans the window).
    /// Backs `ta.sma`.
    WindowMean {
        slot: u32,
        length: u32,
    },
    /// Pop a runtime integer length and push the mean of that many recent
    /// cells. The VM checks the [`MAX_WINDOW_LENGTH`] maximum on each execution; this
    /// instruction retains enough source history for any legal runtime length.
    WindowMeanDynamic {
        slot: u32,
    },
    /// Pop a runtime length and push the highest or lowest value in the
    /// current window of a user series slot.
    WindowExtremumDynamic {
        slot: u32,
        highest: bool,
    },
    /// Pop a runtime int length and compute median, mode, or range from a
    /// user-series window. The function tag preserves integer result types.
    WindowStatisticDynamic {
        slot: u32,
        func: TaFunc,
    },
    /// Pop a simple integer length and float source, then advance one
    /// call-site EMA state. The state slot does not retain source history.
    EmaSimpleDynamic {
        slot: u32,
    },
    /// Pop `argc` arguments (last argument on top) and push the pure
    /// function's value. Most numeric functions are stateless and O(1);
    /// `MathRandom` advances the deterministic per-run PRNG, time/calendar
    /// functions read run context, and string functions may scan or allocate
    /// proportional to their UTF-8 inputs/results. Functions propagate `na`
    /// except where their contract inspects or replaces it (`Na`/`Nz`). The
    /// validator enforces per-function argument bounds and operand types.
    /// Metering charges the dispatch unit plus string bytes inspected or
    /// projected, and string-producing calls reserve the run-wide string-byte
    /// budget (see docs/ir.md and docs/runtime.md).
    CallPure {
        func: PureFunc,
        argc: u8,
    },
    /// Pop `argc` arguments (last argument on top; the receiver handle is the
    /// first argument, so it is deepest) and push the collection function's
    /// result — one value always (void mutators push a placeholder `na`). May
    /// mutate the VM heap and may abort with a structured runtime error (OOB
    /// index, na-receiver, heap-limit). The validator enforces per-function
    /// `argc` bounds and abstract operand categories (see
    /// `CollectionFunc::argc_bounds`/`operand_reqs` and docs/ir.md). Metering:
    /// O(1) functions cost 1 unit; O(n) functions (that touch every element)
    /// cost the element count they process.
    CallCollection {
        func: CollectionFunc,
        argc: u8,
    },
    /// Pop this call site's runtime source operands (per
    /// `ta_states[state].func`'s operand table — the **last** operand is on
    /// top of the stack), advance the site's VM-owned state, and push the
    /// function's value. Each state id may be referenced by at most one
    /// instruction (validated); the state advances **only** when this
    /// instruction executes, so conditional-execution gaps freeze it.
    /// Metering: costs `TaFunc::engine_cost` units (window length for
    /// window-rescanning functions, 1 for O(1) functions; see docs/ir.md).
    /// The pop count depends on the `ta_states` table, so
    /// [`CsilInstr::stack_effect_rule`] cannot report it alone — the shared
    /// contextual resolver consults the table to determine the call's arity.
    CallTa {
        state: u32,
    },

    // Integer arithmetic (na-propagating; checked — overflow and
    // division/remainder by zero push na).
    IntAdd,
    IntSub,
    IntMul,
    /// Integer division truncating toward zero.
    IntDiv,
    /// Pine int/int division: statically int, but an uneven quotient retains
    /// its fractional runtime value. Separate from the truncating `IntDiv`.
    PineIntDiv,
    /// Integer remainder; the result takes the sign of the dividend.
    IntRem,
    IntNeg,

    // Float arithmetic (na-propagating).
    F64Add,
    F64Sub,
    F64Mul,
    F64Div,
    /// Float remainder; the result takes the sign of the dividend.
    F64Rem,
    F64Neg,

    /// Widen an int to a float (na passes through).
    IntToF64,

    /// Pop `rhs` then `lhs` (both strings) and push their concatenation.
    /// na-propagating: an `na` operand yields `na` (Pine treats string `+`
    /// like the numeric operators). Backs the `+` operator when both operands
    /// are strings. Metering and allocation reservation charge the combined
    /// UTF-8 byte length.
    StrConcat,

    /// Ternary conditional select: pops `rhs`, then `lhs`, then `cond`
    /// (bool), and pushes `lhs` when `cond` is true, `rhs` when it is false
    /// **or `na`** (na-condition-is-false, consistent with `JumpIfFalse`).
    /// Both branch values are evaluated eagerly before the select.
    /// Metering: costs 1 unit.
    Select,

    // Comparison (na-propagating; push bool). Numeric-polymorphic: int-int
    // compares exactly, float-float as f64, mixed promotes the int side.
    CmpGt,
    CmpGe,
    CmpLt,
    CmpLe,
    // Equality over matching runtime types.
    CmpEq,
    CmpNe,

    // Boolean logic (Kleene three-valued over na). The combines pop two
    // already-evaluated operands; source-level `and`/`or` short-circuit
    // because lowering brackets the rhs with `Dup` + [`CsilInstr::JumpIfExact`].
    BoolAnd,
    BoolOr,
    BoolNot,

    // Control flow.
    Jump {
        target: u32,
    },
    /// Pops a bool; `na` is treated as false (Pine-style conditions).
    JumpIfFalse {
        target: u32,
    },
    /// Pops a bool; jumps only when it is exactly `on` — `na` never jumps
    /// (unlike [`CsilInstr::JumpIfFalse`]'s na-is-false rule). Emitted after a
    /// `DUP` of a lazy boolean's lhs to short-circuit `and` (`on = false`) /
    /// `or` (`on = true`): the retained lhs copy is the expression's result
    /// on the jump path, and an `na` lhs falls through so the strong-Kleene
    /// `BOOL_AND`/`BOOL_OR` combine still sees the rhs (`na and false =
    /// false`, `na or true = true`). Metering: costs 1 unit.
    JumpIfExact {
        on: bool,
        target: u32,
    },
    /// Jumps when the slot has ever been written by `StoreUserSlotCurrent`
    /// (the flag persists across bars and is *not* set by carry-forward).
    /// No stack effect. Lowered from `var` declarations: the fall-through
    /// path runs the initializer exactly once.
    JumpIfSlotWritten {
        slot: u32,
        target: u32,
    },

    // Effects.
    PlotF64 {
        plot_slot: u32,
    },
    /// Plot a float plus `color_count` color channels declared by the plot.
    /// The color operands are above the float on the stack in declaration
    /// order. Pops `1 + color_count`, pushes 0.
    PlotStyledF64 {
        plot_slot: u32,
        color_count: u8,
    },
    /// Record the declared color channels for a color-only chart visual.
    VisualColor {
        visual_slot: u32,
        color_count: u8,
    },
    /// Record the per-bar color channels for a `fill(...)` band. Pops
    /// `color_count`, pushes 0.
    FillColor {
        fill_slot: u32,
        color_count: u8,
    },
    /// Pop `message` and abort the run with it as a structured user runtime
    /// error (`runtime.error`). A `na` message aborts with empty text.
    /// The run stops on the bar/instruction where this executes; bars completed
    /// earlier keep their recorded output. Pops 1, pushes 0. Metering: 1 unit.
    /// See docs/ir.md (runtime.error).
    RuntimeError,

    // ---- user-defined types ----------------------------------------------
    /// Pop `field_count` field values (declaration order; the last field is on
    /// top of the stack), allocate a fresh `HeapObj::Object` for `type_id`, and
    /// push its `Value::Ref` handle. May abort with a heap-limit runtime error.
    /// `field_count` is validated against `object_types[type_id].field_count`.
    /// Metering: costs `field_count` units.
    NewObject {
        type_id: u32,
        field_count: u32,
    },
    /// Pop an object handle and push a NEW handle whose field values are copied
    /// (shallow copy). An `na` source is a structured runtime error. Metering:
    /// costs the source object's field count.
    CopyObject {
        type_id: u32,
    },
    /// Pop an object handle and push field `field_idx`. An `na` object is a
    /// structured runtime error. Metering: costs 1 unit.
    GetField {
        field_idx: u32,
    },
    /// Pop the value then the object handle (value on top), and write field
    /// `field_idx`. No value pushed. An `na` object is a structured runtime
    /// error. Metering: costs 1 unit.
    SetField {
        field_idx: u32,
    },

    // ---- drawing objects -------------------------------------------------
    /// Pop a freshly constructed drawing-object handle, register it in `kind`'s
    /// per-run live registry (running deterministic oldest-first GC if the max
    /// count is exceeded), and push the same handle. Only registered kinds
    /// (`line`/`label`/`box`) use this. Metering: 1 unit.
    /// See docs/ir.md (drawing objects).
    RegisterDrawing {
        kind: DrawKind,
    },
    /// Push a fresh `array<drawing>` handle holding `kind`'s live drawing
    /// objects (in creation order). Takes no operand. Only registered kinds
    /// (`line`/`label`/`box`/`table`/`polyline`/`linefill`) use this. Metering:
    /// costs the live count (it copies that many handles). See docs/ir.md.
    ReadDrawingsAll {
        kind: DrawKind,
    },
    /// Pop a drawing-object handle and remove it from `kind`'s live registry
    /// (push nothing). An `na` receiver is a structured runtime error; deleting
    /// an already-absent handle is a no-op. Metering: 1 unit.
    DeleteDrawing {
        kind: DrawKind,
    },

    // ---- table cells -----------------------------------------------------
    /// Pop `attrs.len()` attribute values (last attr on top), then `row`, then
    /// `column`, then the table object handle, and write each attribute of the
    /// cell at (`column`, `row`). Pushes nothing. An `na` receiver or an
    /// out-of-range coordinate (against the table's declared `columns`×`rows`)
    /// is a structured runtime error. Metering: `attrs.len()` units (min 1).
    /// See docs/ir.md (table cells).
    SetCell {
        attrs: Vec<CellAttr>,
    },
    /// Pop `end_row`, `end_column`, `start_row`, `start_column`, then the table
    /// handle (table lowest), and clear every cell in the inclusive range.
    /// Pushes nothing. `na` receiver / out-of-range = structured error.
    /// Metering: 1 unit.
    ClearCells,
    /// Pop `end_row`, `end_column`, `start_row`, `start_column`, then the table
    /// handle, and mark the span merged. Pushes nothing. `na` receiver /
    /// out-of-range = structured error. Metering: 1 unit.
    MergeCells,

    // ---- event streams ---------------------------------------------------
    /// Pop `argc` values (the format string first was pushed, then substitution
    /// args in order — so the last arg is on top), build the message via
    /// `str.format` placeholder rules, and record a `(level, bar, message)`
    /// entry in the run's `logs` stream. Pushes nothing. Metering: `argc` units.
    /// See docs/ir.md (event streams).
    Log {
        level: LogLevel,
        argc: u8,
    },
    /// Pop `freq` (top) then `message`, and record a `(bar, message, freq)`
    /// entry in the run's `alerts` stream honoring the `alert.freq_*` firing
    /// rules (once-per-bar dedup within a bar). Pushes nothing. Metering: 1 unit.
    Alert,
    /// Pop `message` (top), `title`, then `condition`, and — only when
    /// `condition` is true — record a `(bar, message, freq_once_per_bar_close)`
    /// entry in the `alerts` stream. Pushes nothing. Metering: 1 unit.
    AlertCondition,

    // ---- strategy order primitives ---------------------------------------
    /// Pop `kind.operand_count()` operands (the primitive's fixed slot layout,
    /// pushed in slot order — so the last slot is on top), and place the order
    /// with the run's broker emulator (which fills it on a later bar per the
    /// emulator's fill rules). Pushes nothing. Metering: 1 unit. Only valid in a
    /// strategy script. See docs/ir.md (order primitives) and docs/runtime.md.
    Order {
        kind: OrderKind,
    },

    /// End of the per-bar program.
    Return,
}

/// A context-free stack rule for one CSIL instruction.
///
/// Fixed effects depend only on the opcode. Payload effects use a count carried
/// by the instruction (for example `argc` or `color_count`). TA calls require
/// the validated state table and are resolved by
/// [`CsilInstr::resolve_stack_effect`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackEffectRule {
    Fixed { pops: u32, pushes: u32 },
    Payload { pops: usize, pushes: usize },
    TaState { state: u32 },
}

/// Resolved stack transition. Counts are bounded to `u32`, matching CSIL's
/// declared stack height and the validator's representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StackEffect {
    pub pops: u32,
    pub pushes: u32,
}

/// Why a payload- or state-dependent stack transition could not be resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackEffectError {
    InvalidTaState { state: u32, state_count: usize },
    CountExceedsU32,
}

impl std::fmt::Display for StackEffectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTaState { state, state_count } => {
                write!(
                    f,
                    "ta state {state} is out of range (table has {state_count} entries)"
                )
            }
            Self::CountExceedsU32 => f.write_str("stack effect exceeds the u32 count limit"),
        }
    }
}

impl std::error::Error for StackEffectError {}

impl CsilInstr {
    /// Return the exhaustive stack rule for this instruction.
    ///
    /// The exhaustive match makes adding a CSIL opcode a compile-time reminder
    /// to assign its stack behavior. Consumers that have a program should use
    /// [`CsilInstr::resolve_stack_effect`] so contextual rules cannot be
    /// mistaken for fixed ones.
    pub fn stack_effect_rule(&self) -> StackEffectRule {
        use StackEffectRule::{Fixed, Payload, TaState};
        match self {
            CsilInstr::CallTa { state } => TaState { state: *state },
            CsilInstr::PushNa
            | CsilInstr::PushBool(_)
            | CsilInstr::PushI64(_)
            | CsilInstr::PushF64(_)
            | CsilInstr::PushStr(_)
            | CsilInstr::PushColor { .. }
            | CsilInstr::ReadBuiltin { .. }
            | CsilInstr::ReadUserSlot { .. }
            | CsilInstr::ReadInput { .. }
            | CsilInstr::ReadSecurity { .. }
            | CsilInstr::ReadSecurityExpr { .. }
            | CsilInstr::ReadRequest { .. }
            | CsilInstr::BuildFootprint { .. }
            | CsilInstr::ReadSecurityLowerTf { .. }
            | CsilInstr::ReadPivotLevels { .. }
            | CsilInstr::ReadTimeTf { .. }
            | CsilInstr::WindowMean { .. } => Fixed { pops: 0, pushes: 1 },
            CsilInstr::WindowMeanDynamic { .. }
            | CsilInstr::WindowExtremumDynamic { .. }
            | CsilInstr::WindowStatisticDynamic { .. } => Fixed { pops: 1, pushes: 1 },
            CsilInstr::EmaSimpleDynamic { .. } => Fixed { pops: 2, pushes: 1 },
            CsilInstr::Dup => Fixed { pops: 1, pushes: 2 },
            CsilInstr::Pop
            | CsilInstr::StoreUserSlotCurrent { .. }
            | CsilInstr::JumpIfFalse { .. }
            | CsilInstr::JumpIfExact { .. }
            | CsilInstr::PlotF64 { .. }
            | CsilInstr::RuntimeError => Fixed { pops: 1, pushes: 0 },
            CsilInstr::PlotStyledF64 { color_count, .. } => Payload {
                pops: 1 + usize::from(*color_count),
                pushes: 0,
            },
            CsilInstr::VisualColor { color_count, .. }
            | CsilInstr::FillColor { color_count, .. } => Payload {
                pops: usize::from(*color_count),
                pushes: 0,
            },
            CsilInstr::ReadBuiltinDynamic { .. } | CsilInstr::ReadUserSlotDynamic { .. } => {
                Fixed { pops: 1, pushes: 1 }
            }
            CsilInstr::IntNeg | CsilInstr::F64Neg | CsilInstr::IntToF64 | CsilInstr::BoolNot => {
                Fixed { pops: 1, pushes: 1 }
            }
            CsilInstr::IntAdd
            | CsilInstr::IntSub
            | CsilInstr::IntMul
            | CsilInstr::IntDiv
            | CsilInstr::PineIntDiv
            | CsilInstr::IntRem
            | CsilInstr::F64Add
            | CsilInstr::F64Sub
            | CsilInstr::F64Mul
            | CsilInstr::F64Div
            | CsilInstr::F64Rem
            | CsilInstr::CmpGt
            | CsilInstr::CmpGe
            | CsilInstr::CmpLt
            | CsilInstr::CmpLe
            | CsilInstr::CmpEq
            | CsilInstr::CmpNe
            | CsilInstr::BoolAnd
            | CsilInstr::BoolOr
            | CsilInstr::FootprintRowByPrice
            | CsilInstr::StrConcat => Fixed { pops: 2, pushes: 1 },
            CsilInstr::Select => Fixed { pops: 3, pushes: 1 },
            CsilInstr::CallPure { argc, .. } | CsilInstr::CallPivotPointLevels { argc } => {
                Payload {
                    pops: usize::from(*argc),
                    pushes: 1,
                }
            }
            // Pops `argc`, pushes exactly one value (void mutators push `na`).
            CsilInstr::CallCollection { argc, .. } => Payload {
                pops: usize::from(*argc),
                pushes: 1,
            },
            // Pops each field value, pushes the object handle.
            CsilInstr::NewObject { field_count, .. } => Payload {
                pops: *field_count as usize,
                pushes: 1,
            },
            CsilInstr::CopyObject { .. } | CsilInstr::GetField { .. } => {
                Fixed { pops: 1, pushes: 1 }
            }
            // Pops the object handle and the value; pushes nothing.
            CsilInstr::SetField { .. } => Fixed { pops: 2, pushes: 0 },
            // Pops the handle, pushes the same handle back (registration).
            CsilInstr::RegisterDrawing { .. } => Fixed { pops: 1, pushes: 1 },
            // Takes no operand; pushes a fresh array handle.
            CsilInstr::ReadDrawingsAll { .. } => Fixed { pops: 0, pushes: 1 },
            // Pops the handle, pushes nothing (deletion).
            CsilInstr::DeleteDrawing { .. } => Fixed { pops: 1, pushes: 0 },
            // Pops the table handle, column, row, then one value per attribute;
            // pushes nothing.
            CsilInstr::SetCell { attrs } => Payload {
                pops: 3usize.saturating_add(attrs.len()),
                pushes: 0,
            },
            // Pops the table handle + four coordinate ints; pushes nothing.
            CsilInstr::ClearCells | CsilInstr::MergeCells => Fixed { pops: 5, pushes: 0 },
            CsilInstr::Log { argc, .. } => Payload {
                pops: usize::from(*argc),
                pushes: 0,
            },
            CsilInstr::Alert => Fixed { pops: 2, pushes: 0 },
            CsilInstr::AlertCondition => Fixed { pops: 3, pushes: 0 },
            // Pops the primitive's fixed operand layout; pushes nothing.
            CsilInstr::Order { kind } => Payload {
                pops: usize::from(order_operand_count(*kind)),
                pushes: 0,
            },
            CsilInstr::Jump { .. } | CsilInstr::JumpIfSlotWritten { .. } | CsilInstr::Return => {
                Fixed { pops: 0, pushes: 0 }
            }
        }
    }

    /// Resolve a stack transition against the program context.
    ///
    /// The TA state id is checked here instead of being indexed directly by
    /// each consumer. Oversized payload counts are rejected instead of
    /// truncating them into CSIL's `u32` stack-height domain.
    pub fn resolve_stack_effect(
        &self,
        ta_states: &[TaStateDecl],
    ) -> Result<StackEffect, StackEffectError> {
        let (pops, pushes) = match self.stack_effect_rule() {
            StackEffectRule::Fixed { pops, pushes } => (pops as usize, pushes as usize),
            StackEffectRule::Payload { pops, pushes } => (pops, pushes),
            StackEffectRule::TaState { state } => {
                let declaration =
                    ta_states
                        .get(state as usize)
                        .ok_or(StackEffectError::InvalidTaState {
                            state,
                            state_count: ta_states.len(),
                        })?;
                (
                    declaration.func.engine_operand_count(),
                    declaration.func.engine_result_arity() as usize,
                )
            }
        };
        Ok(StackEffect {
            pops: u32::try_from(pops).map_err(|_| StackEffectError::CountExceedsU32)?,
            pushes: u32::try_from(pushes).map_err(|_| StackEffectError::CountExceedsU32)?,
        })
    }
}

#[cfg(test)]
mod stack_effect_tests {
    use super::*;

    #[test]
    fn fixed_and_payload_rules_resolve_to_their_declared_counts() {
        let cases = [
            (CsilInstr::PushI64(7), 0, 1),
            (CsilInstr::Dup, 1, 2),
            (
                CsilInstr::PlotStyledF64 {
                    plot_slot: 0,
                    color_count: 2,
                },
                3,
                0,
            ),
            (
                CsilInstr::VisualColor {
                    visual_slot: 0,
                    color_count: 3,
                },
                3,
                0,
            ),
            (
                CsilInstr::FillColor {
                    fill_slot: 0,
                    color_count: 1,
                },
                1,
                0,
            ),
            (
                CsilInstr::CallPure {
                    func: PureFunc::MathSqrt,
                    argc: 4,
                },
                4,
                1,
            ),
            (
                CsilInstr::CallCollection {
                    func: CollectionFunc::ArrayFrom,
                    argc: 7,
                },
                7,
                1,
            ),
            (
                CsilInstr::NewObject {
                    type_id: 0,
                    field_count: 5,
                },
                5,
                1,
            ),
            (
                CsilInstr::SetCell {
                    attrs: vec![CellAttr::Text, CellAttr::Bgcolor],
                },
                5,
                0,
            ),
            (CsilInstr::Return, 0, 0),
        ];

        for (instruction, pops, pushes) in cases {
            let effect = instruction
                .resolve_stack_effect(&[])
                .expect("context-free stack rule resolves");
            assert_eq!(
                (effect.pops, effect.pushes),
                (pops, pushes),
                "{instruction:?}"
            );
        }
    }

    #[test]
    fn call_ta_arity_is_resolved_from_the_selected_state_descriptor() {
        let states = [
            TaStateDecl {
                func: TaFunc::Change,
                params: vec![],
            },
            TaStateDecl {
                func: TaFunc::VwapBands,
                params: vec![],
            },
            TaStateDecl {
                func: TaFunc::Supertrend,
                params: vec![],
            },
        ];
        let expected = [(1, 1), (3, 3), (0, 2)];

        for (state, expected) in expected.into_iter().enumerate() {
            let instruction = CsilInstr::CallTa {
                state: state as u32,
            };
            assert!(matches!(
                instruction.stack_effect_rule(),
                StackEffectRule::TaState { .. }
            ));
            let effect = instruction
                .resolve_stack_effect(&states)
                .expect("declared TA state resolves");
            assert_eq!((effect.pops, effect.pushes), expected);
        }
    }

    #[test]
    fn invalid_ta_state_fails_closed() {
        let error = CsilInstr::CallTa { state: 3 }
            .resolve_stack_effect(&[
                TaStateDecl {
                    func: TaFunc::Change,
                    params: vec![],
                },
                TaStateDecl {
                    func: TaFunc::VwapBands,
                    params: vec![],
                },
                TaStateDecl {
                    func: TaFunc::Supertrend,
                    params: vec![],
                },
            ])
            .expect_err("missing state must not use a placeholder effect");
        assert_eq!(
            error,
            StackEffectError::InvalidTaState {
                state: 3,
                state_count: 3
            }
        );
    }
}

/// [`OrderKind::operand_count`] spelled as a direct table, so extracting
/// [`CsilInstr::stack_effect_rule`] (Aeneas) does not reach the `&'static [&str]`
/// parameter-name table it is derived from. `order_operand_count_matches_hir`
/// pins the two to each other for every kind.
const fn order_operand_count(kind: OrderKind) -> u8 {
    match kind {
        OrderKind::Entry => 10,
        OrderKind::Order => 10,
        OrderKind::Exit => 21,
        OrderKind::Close => 7,
        OrderKind::CloseAll => 4,
        OrderKind::Cancel => 1,
        OrderKind::CancelAll => 0,
        OrderKind::RiskMaxPositionSize => 1,
        OrderKind::RiskAllowEntryIn => 1,
        OrderKind::RiskMaxDrawdown => 3,
        OrderKind::RiskMaxIntradayLoss => 3,
        OrderKind::RiskMaxIntradayFilledOrders => 2,
        OrderKind::RiskMaxConsLossDays => 2,
    }
}

/// Free-function extraction roots for Aeneas. Charon (pinned revision) cannot
/// start a translation from an inherent method, so each method that extraction
/// targets is reached through a one-line wrapper here. Compiled only with the
/// `aeneas` feature, which production builds never enable.
#[cfg(feature = "aeneas")]
pub mod aeneas_roots {
    use super::{CsilInstr, StackEffectRule};

    /// [`CsilInstr::stack_effect_rule`].
    pub fn stack_effect_rule(instr: &CsilInstr) -> StackEffectRule {
        instr.stack_effect_rule()
    }

    /// [`super::CsilProgram::history_depth_with_ta_inputs`].
    pub fn history_depth_with_ta_inputs(
        program: &super::CsilProgram,
        values: &std::collections::HashMap<
            pinewasm_hir::TaInputTarget,
            i64,
            std::collections::hash_map::RandomState,
        >,
    ) -> u32 {
        program.history_depth_with_ta_inputs(values)
    }
}

/// Aeneas extraction roots for the CSIL validator kernels (see the module
/// docs). Built only with `--features aeneas`.
#[cfg(feature = "aeneas")]
pub mod aeneas_roots_csil_validate;

/// Aeneas root for the proof-literal conversion.
#[cfg(feature = "aeneas")]
pub mod aeneas_roots_proof_literal;
/// Owned TA input-rule selection, compiled only for extraction builds.
#[cfg(feature = "aeneas")]
pub mod aeneas_roots_ta_rule;

/// Aeneas extraction roots for the plot-table kernel. Built
/// only with `--features aeneas`.
#[cfg(feature = "aeneas")]
#[allow(dead_code)] // Extraction roots for Charon/Aeneas; never called from Rust.
mod aeneas_roots_plot_table;

/// Aeneas extraction roots for MIR-to-CSIL stackification (and the
/// stackification state they take). Built only with `--features aeneas`.
#[cfg(feature = "aeneas")]
pub use lower::{aeneas_roots_csil_stackify, LowerCtx};

#[cfg(test)]
mod lane_schema_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod order_operand_count_tests {
    use super::*;

    #[test]
    fn order_operand_count_matches_hir() {
        let mut kinds = 0;
        for tag in 0..=u8::MAX {
            let Some(kind) = OrderKind::from_u8(tag) else {
                continue;
            };
            kinds += 1;
            assert_eq!(order_operand_count(kind), kind.operand_count(), "{kind:?}");
            assert_eq!(
                CsilInstr::Order { kind }.stack_effect_rule(),
                StackEffectRule::Payload {
                    pops: usize::from(kind.operand_count()),
                    pushes: 0,
                },
                "{kind:?}"
            );
        }
        assert_eq!(kinds, 13);
    }
}

#[cfg(test)]
mod f64_bits_tests {
    use super::*;

    /// The plain `f64` payload shape, for wire and `Debug` comparison.
    #[derive(Debug, Serialize, Deserialize)]
    enum Reference {
        PushF64(f64),
    }

    fn samples() -> Vec<f64> {
        let mut values = vec![
            0.0,
            -0.0,
            1.0,
            -1.5,
            2.0,
            0.1,
            1.0 / 3.0,
            1e300,
            -1e-300,
            f64::MIN_POSITIVE,
            f64::MIN_POSITIVE / 4.0,
            f64::MAX,
            f64::MIN,
            f64::EPSILON,
            9_007_199_254_740_993.0,
            i64::MAX as f64,
            f64::NAN,
            -f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ];
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..2_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            values.push(f64::from_bits(state));
        }
        values
    }

    #[test]
    fn layout_is_zero_cost() {
        assert_eq!(std::mem::size_of::<F64Bits>(), std::mem::size_of::<f64>());
        assert_eq!(std::mem::align_of::<F64Bits>(), std::mem::align_of::<f64>());
        assert_eq!(
            std::mem::size_of::<Option<F64Bits>>(),
            std::mem::size_of::<Option<f64>>()
        );
    }

    #[test]
    fn serde_debug_display_and_equality_match_f64() {
        for value in samples() {
            let wrapped = CsilInstr::PushF64(F64Bits::new(value));
            let reference = Reference::PushF64(value);
            assert_eq!(F64Bits::new(value).to_bits(), value.to_bits());
            assert_eq!(f64::from(F64Bits::from(value)).to_bits(), value.to_bits());
            let json = serde_json::to_string(&wrapped).unwrap();
            assert_eq!(json, serde_json::to_string(&reference).unwrap());
            assert_eq!(format!("{wrapped:?}"), format!("{reference:?}"));
            assert_eq!(format!("{}", F64Bits::new(value)), format!("{value}"));
            // Decoding agrees bit-for-bit with the plain `f64` payload (non-finite
            // values encode as `null`, which neither form accepts).
            let decoded: Result<CsilInstr, _> = serde_json::from_str(&json);
            let expected: Result<Reference, _> = serde_json::from_str(&json);
            match (decoded, expected) {
                (Ok(CsilInstr::PushF64(bits)), Ok(Reference::PushF64(float))) => {
                    assert_eq!(bits.to_bits(), float.to_bits());
                }
                (Err(_), Err(_)) => assert!(!value.is_finite()),
                other => panic!("decode mismatch for {value:?}: {other:?}"),
            }
            for other in [0.0, -0.0, value, f64::NAN, 1.0] {
                assert_eq!(
                    F64Bits::new(value) == F64Bits::new(other),
                    value == other,
                    "{value:?} == {other:?}"
                );
            }
        }
        // Integer and exponent spellings on the wire still decode.
        for (json, value) in [
            (r#"{"PushF64":2}"#, 2.0),
            (r#"{"PushF64":-7}"#, -7.0),
            (r#"{"PushF64":1e3}"#, 1000.0),
            (
                r#"{"PushF64":18446744073709551615}"#,
                18_446_744_073_709_551_615.0,
            ),
        ] {
            let decoded: CsilInstr = serde_json::from_str(json).unwrap();
            assert_eq!(decoded, CsilInstr::PushF64(F64Bits::new(value)));
        }
    }
}

#[cfg(test)]
mod allocation_loop_tests;
